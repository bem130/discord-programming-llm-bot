mod commands;

use std::sync::{
    RwLock,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result};
use serenity::{
    all::{
        Attachment, Channel, Context, CreateAllowedMentions, CreateMessage, Emoji, EventHandler,
        GatewayIntents, GuildId, Interaction, Message, ReactionType, Ready,
    },
    async_trait,
};
use tokio::sync::Mutex;

use crate::{
    ai::SharedLanguageModel,
    context::{ResponseMode, RuntimeBot, RuntimeContext, RuntimeLocation, RuntimeUser},
    db::Database,
    delivery::split_message,
    model::{IncomingAttachment, IncomingMessage},
    runner::SharedCodeRunner,
    tutor::{decide_action, generate_reviewed_answer},
};

const CONTEXT_MESSAGE_LIMIT: u32 = 40;
const MAX_TEXT_ATTACHMENT_BYTES: u32 = 512 * 1024;
const MAX_MULTIMODAL_ATTACHMENT_BYTES: u32 = 20 * 1024 * 1024;

pub struct DiscordBot {
    guild_id: GuildId,
    database: Database,
    model: SharedLanguageModel,
    bot_user_id: AtomicU64,
    bot_owner_id: AtomicU64,
    bot_name: RwLock<String>,
    response_lock: Mutex<()>,
    runner: Option<SharedCodeRunner>,
}

impl DiscordBot {
    pub fn new(
        guild_id: GuildId,
        database: Database,
        model: SharedLanguageModel,
        runner: Option<SharedCodeRunner>,
    ) -> Self {
        Self {
            guild_id,
            database,
            model,
            bot_user_id: AtomicU64::new(0),
            bot_owner_id: AtomicU64::new(0),
            bot_name: RwLock::new(String::new()),
            response_lock: Mutex::new(()),
            runner,
        }
    }
}

pub const fn required_intents() -> GatewayIntents {
    GatewayIntents::GUILDS
        .union(GatewayIntents::GUILD_MESSAGES)
        .union(GatewayIntents::MESSAGE_CONTENT)
}

#[async_trait]
impl EventHandler for DiscordBot {
    async fn ready(&self, context: Context, ready: Ready) {
        self.bot_user_id
            .store(ready.user.id.get(), Ordering::Relaxed);
        if let Ok(mut bot_name) = self.bot_name.write() {
            ready.user.name.clone_into(&mut bot_name);
        }
        match context.http.get_current_application_info().await {
            Ok(application) => {
                let owner_id = application
                    .owner
                    .map(|owner| owner.id.get())
                    .or_else(|| application.team.map(|team| team.owner_user_id.get()))
                    .unwrap_or(0);
                self.bot_owner_id.store(owner_id, Ordering::Relaxed);
            }
            Err(error) => tracing::warn!(?error, "failed to load bot owner information"),
        }
        match self
            .guild_id
            .set_commands(&context.http, vec![commands::ai_command()])
            .await
        {
            Ok(commands) => tracing::info!(
                guild_id = self.guild_id.get(),
                command_count = commands.len(),
                user = %ready.user.name,
                "Discord bot is ready"
            ),
            Err(error) => tracing::error!(?error, "failed to register guild commands"),
        }
    }

    async fn interaction_create(&self, context: Context, interaction: Interaction) {
        let Interaction::Command(command) = interaction else {
            return;
        };
        if command.data.name != "ai" {
            return;
        }

        if let Err(error) =
            commands::handle_ai_command(&context, &command, self.guild_id, &self.database).await
        {
            tracing::error!(?error, "slash command failed");
            commands::respond_with_generic_error(&context, &command).await;
        }
    }

    async fn message(&self, context: Context, message: Message) {
        if message.author.bot || message.guild_id != Some(self.guild_id) {
            return;
        }

        if let Err(error) = self.handle_message(&context, &message).await {
            tracing::error!(
                ?error,
                message_id = message.id.get(),
                channel_id = message.channel_id.get(),
                "failed to process Discord message"
            );
        }
    }
}

impl DiscordBot {
    async fn handle_message(&self, context: &Context, message: &Message) -> Result<()> {
        let mode = self
            .database
            .channel_mode(self.guild_id.get(), message.channel_id.get())
            .await?;
        if !mode.stores_messages() {
            return Ok(());
        }

        let attachments = extract_attachments(&message.attachments).await;
        self.database
            .record_message(&IncomingMessage {
                discord_message_id: message.id.get(),
                guild_id: self.guild_id.get(),
                channel_id: message.channel_id.get(),
                author_id: message.author.id.get(),
                author_name: message.author.name.clone(),
                content: message.content.clone(),
                is_bot: false,
                created_at: message.timestamp.to_string(),
                attachments,
            })
            .await?;

        let _response_guard = self.response_lock.lock().await;
        let guild_context = self
            .database
            .recent_context(self.guild_id.get(), CONTEXT_MESSAGE_LIMIT)
            .await?;
        let bot_user_id = self.bot_user_id.load(Ordering::Relaxed);
        let mentioned = bot_user_id != 0
            && message
                .mentions
                .iter()
                .any(|user| user.id.get() == bot_user_id);
        let direct = bot_user_id != 0
            && message
                .referenced_message
                .as_ref()
                .is_some_and(|referenced| referenced.author.id.get() == bot_user_id);
        let forced_reply = mentioned || (mode == crate::model::ChannelMode::Auto && direct);
        let custom_emojis = match self.guild_id.emojis(&context.http).await {
            Ok(emojis) => emojis,
            Err(error) => {
                tracing::warn!(?error, "failed to list guild emojis");
                vec![]
            }
        };
        let emoji_names: Vec<_> = custom_emojis
            .iter()
            .filter(|emoji| emoji.available)
            .map(|emoji| emoji.name.clone())
            .collect();
        let action = decide_action(
            self.model.as_ref(),
            mode,
            forced_reply,
            &guild_context,
            &emoji_names,
        )
        .await?;
        for requested in action.reactions.iter().take(3) {
            let Some(reaction) = resolve_reaction(requested, &custom_emojis) else {
                tracing::warn!(reaction = %requested, "AI selected an unavailable reaction");
                continue;
            };
            if let Err(error) = message.react(&context.http, reaction).await {
                tracing::warn!(?error, "failed to add AI-selected reaction");
            }
        }
        if !action.should_reply {
            return Ok(());
        }

        let _ = message.channel_id.broadcast_typing(&context.http).await;
        let response_mode = if direct {
            ResponseMode::Direct
        } else if mentioned {
            ResponseMode::Invoked
        } else {
            ResponseMode::Passive
        };
        let runtime_context = self
            .runtime_context(context, message, response_mode, bot_user_id)
            .await;
        let answer = generate_reviewed_answer(
            self.model.as_ref(),
            &guild_context,
            &runtime_context,
            self.runner.as_deref(),
        )
        .await?;
        self.send_answer(context, message, &answer).await
    }

    async fn runtime_context(
        &self,
        context: &Context,
        message: &Message,
        mode: ResponseMode,
        bot_user_id: u64,
    ) -> RuntimeContext {
        let server_name = self
            .guild_id
            .to_partial_guild(&context.http)
            .await
            .ok()
            .map(|guild| guild.name);
        let channel_name = match message.channel_id.to_channel(&context.http).await {
            Ok(Channel::Guild(channel)) => Some(channel.name),
            _ => None,
        };
        let bot_name = self
            .bot_name
            .read()
            .map(|name| name.clone())
            .unwrap_or_else(|_| "AI Bot".to_owned());
        let owner_id = self.bot_owner_id.load(Ordering::Relaxed);

        RuntimeContext {
            mode,
            bot: RuntimeBot {
                id: bot_user_id,
                name: bot_name,
                owner_user_id: (owner_id != 0).then_some(owner_id),
            },
            server: RuntimeLocation {
                id: self.guild_id.get(),
                name: server_name,
            },
            channel: RuntimeLocation {
                id: message.channel_id.get(),
                name: channel_name,
            },
            current_user: RuntimeUser {
                id: message.author.id.get(),
                display_name: message
                    .author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| message.author.name.clone()),
            },
        }
    }

    async fn send_answer(
        &self,
        context: &Context,
        source_message: &Message,
        answer: &str,
    ) -> Result<()> {
        let chunks = split_message(answer);
        if chunks.is_empty() {
            anyhow::bail!("reviewed answer was empty");
        }

        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut builder = CreateMessage::new()
                .content(chunk)
                .allowed_mentions(CreateAllowedMentions::new().replied_user(false));
            if index == 0 {
                builder = builder.reference_message(source_message);
            }
            let sent = source_message
                .channel_id
                .send_message(&context.http, builder)
                .await
                .context("failed to send reviewed answer")?;
            self.database
                .record_message(&IncomingMessage {
                    discord_message_id: sent.id.get(),
                    guild_id: self.guild_id.get(),
                    channel_id: sent.channel_id.get(),
                    author_id: sent.author.id.get(),
                    author_name: sent.author.name,
                    content: sent.content,
                    is_bot: true,
                    created_at: sent.timestamp.to_string(),
                    attachments: vec![],
                })
                .await?;
        }
        Ok(())
    }
}

async fn extract_attachments(attachments: &[Attachment]) -> Vec<IncomingAttachment> {
    let mut extracted = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let is_text = is_text_attachment(attachment);
        let is_multimodal = is_multimodal_attachment(attachment);
        let should_download = (is_text && attachment.size <= MAX_TEXT_ATTACHMENT_BYTES)
            || (is_multimodal && attachment.size <= MAX_MULTIMODAL_ATTACHMENT_BYTES);
        let bytes = if should_download {
            match attachment.download().await {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        filename = %attachment.filename,
                        "failed to download readable attachment"
                    );
                    None
                }
            }
        } else {
            None
        };
        let extracted_text = if is_text {
            bytes
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
        } else {
            None
        };
        extracted.push(IncomingAttachment {
            filename: attachment.filename.clone(),
            content_type: attachment.content_type.clone(),
            size_bytes: u64::from(attachment.size),
            extracted_text,
            binary_data: if is_multimodal { bytes } else { None },
        });
    }
    extracted
}

fn is_multimodal_attachment(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|content_type| {
            matches!(
                content_type,
                "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "application/pdf"
            )
        })
}

fn resolve_reaction(requested: &str, custom_emojis: &[Emoji]) -> Option<ReactionType> {
    let requested = requested.trim();
    if let Some(shortcode) = requested
        .strip_prefix(':')
        .and_then(|value| value.strip_suffix(':'))
    {
        if let Some(custom) = custom_emojis
            .iter()
            .find(|emoji| emoji.available && emoji.name == shortcode)
        {
            return Some(ReactionType::from(custom.clone()));
        }
        return emojis::get_by_shortcode(shortcode)
            .map(|emoji| ReactionType::Unicode(emoji.as_str().to_owned()));
    }
    (!requested.is_empty() && requested.chars().count() <= 16)
        .then(|| ReactionType::Unicode(requested.to_owned()))
}

fn is_text_attachment(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|content_type| content_type.starts_with("text/"))
        || attachment
            .filename
            .rsplit_once('.')
            .is_some_and(|(_, extension)| {
                matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "c" | "cpp"
                        | "cs"
                        | "css"
                        | "go"
                        | "h"
                        | "hpp"
                        | "html"
                        | "java"
                        | "js"
                        | "json"
                        | "kt"
                        | "md"
                        | "php"
                        | "py"
                        | "rb"
                        | "rs"
                        | "sh"
                        | "sql"
                        | "toml"
                        | "ts"
                        | "tsx"
                        | "txt"
                        | "xml"
                        | "yaml"
                        | "yml"
                )
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_standard_emoji_shortcode_to_unicode() {
        let Some(ReactionType::Unicode(value)) = resolve_reaction(":rocket:", &[]) else {
            panic!("expected a standard Unicode reaction");
        };
        assert_eq!(value, "🚀");
    }

    #[test]
    fn rejects_unknown_shortcode() {
        assert!(resolve_reaction(":not_a_real_emoji:", &[]).is_none());
    }
}
