mod commands;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use serenity::{
    all::{
        Attachment, Channel, ChannelId, ChannelType, Context, CreateAllowedMentions, CreateMessage,
        Emoji, EventHandler, GatewayIntents, GuildId, Interaction, Message, MessageId,
        MessageUpdateEvent, ReactionType, Ready, RoleId,
    },
    async_trait,
    gateway::{ActivityData, ShardMessenger},
};
use tokio::{
    sync::{Mutex, Notify, oneshot, watch},
    task::JoinHandle,
};

use crate::{
    ai::SharedLanguageModel,
    context::{ResponseMode, RuntimeBot, RuntimeContext, RuntimeLocation, RuntimeUser},
    db::Database,
    delivery::split_message,
    model::{IncomingAttachment, IncomingMessage, RecentChannelActivity},
    runner::SharedCodeRunner,
    tutor::{BotAction, ToolStatusSink, decide_action, generate_reviewed_answer},
};

const RECENT_CONTEXT_MESSAGE_LIMIT: u32 = 5;
const MAX_CONVERSATION_BATCH_MESSAGES: usize = 80;
const RECENT_ACTIVE_CHANNEL_LIMIT: u32 = 12;
const MAX_TEXT_ATTACHMENT_BYTES: u32 = 512 * 1024;
const MAX_MULTIMODAL_ATTACHMENT_BYTES: u32 = 20 * 1024 * 1024;
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
const CONVERSATION_BATCH_DELAY: Duration = Duration::from_millis(1_200);
const GENERATION_FAILURE_MESSAGE: &str = "回答生成またはレビューが一時的に完了しませんでした。少し待ってから、もう一度呼び出してください。";

#[derive(Clone)]
struct PendingConversationMessage {
    message: Message,
    channel_name: Option<String>,
    settings_channel_id: ChannelId,
    mode: crate::model::ChannelMode,
    mentioned: bool,
    direct: bool,
}

struct ConversationBatch {
    channel_id: u64,
    generation: u64,
    messages: Vec<PendingConversationMessage>,
    updates: watch::Receiver<u64>,
}

struct ConversationTicket {
    channel_id: u64,
    generation: u64,
    updates: watch::Receiver<u64>,
}

struct PreparedConversationResponse {
    source: Message,
    settings_channel_id: ChannelId,
    action: BotAction,
    custom_emojis: Vec<Emoji>,
    answer: Option<String>,
}

struct ChannelConversationState {
    generation: u64,
    active_generation: Option<u64>,
    pending: Vec<PendingConversationMessage>,
    updates: watch::Sender<u64>,
    availability: Arc<Notify>,
}

impl ChannelConversationState {
    fn new() -> Self {
        let (updates, _) = watch::channel(0);
        Self {
            generation: 0,
            active_generation: None,
            pending: vec![],
            updates,
            availability: Arc::new(Notify::new()),
        }
    }

    fn advance_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.updates.send_replace(self.generation);
        self.generation
    }

    fn try_claim(&mut self, generation: u64) -> bool {
        if self.generation == generation && self.active_generation.is_none() {
            self.active_generation = Some(generation);
            true
        } else {
            false
        }
    }

    fn release(&mut self, generation: u64) {
        if self.active_generation == Some(generation) {
            self.active_generation = None;
            self.availability.notify_waiters();
        }
    }
}

#[derive(Default)]
struct ConversationCoordinator {
    channels: Mutex<HashMap<u64, ChannelConversationState>>,
}

impl ConversationCoordinator {
    async fn enqueue(&self, pending: PendingConversationMessage) -> ConversationTicket {
        let channel_id = pending.message.channel_id.get();
        let mut channels = self.channels.lock().await;
        let state = channels
            .entry(channel_id)
            .or_insert_with(ChannelConversationState::new);
        state.pending.push(pending);
        let dropped = retain_latest(&mut state.pending, MAX_CONVERSATION_BATCH_MESSAGES);
        if dropped > 0 {
            tracing::warn!(
                channel_id,
                dropped,
                "conversation batch exceeded its context limit; discarding oldest pending messages"
            );
        }
        let generation = state.advance_generation();
        ConversationTicket {
            channel_id,
            generation,
            updates: state.updates.subscribe(),
        }
    }

    async fn claim_after_delay(&self, ticket: ConversationTicket) -> Option<ConversationBatch> {
        tokio::time::sleep(CONVERSATION_BATCH_DELAY).await;
        loop {
            let availability = {
                let mut channels = self.channels.lock().await;
                let state = channels.get_mut(&ticket.channel_id)?;
                if state.generation != ticket.generation {
                    return None;
                }
                if state.try_claim(ticket.generation) {
                    return Some(ConversationBatch {
                        channel_id: ticket.channel_id,
                        generation: ticket.generation,
                        messages: state.pending.clone(),
                        updates: ticket.updates,
                    });
                }
                Arc::clone(&state.availability).notified_owned()
            };
            availability.await;
        }
    }

    async fn is_current(&self, batch: &ConversationBatch) -> bool {
        self.channels
            .lock()
            .await
            .get(&batch.channel_id)
            .is_some_and(|state| state.generation == batch.generation)
    }

    async fn finish(&self, batch: &ConversationBatch, covered: bool) {
        let mut channels = self.channels.lock().await;
        let Some(state) = channels.get_mut(&batch.channel_id) else {
            return;
        };
        state.release(batch.generation);
        if covered {
            let covered_ids: std::collections::HashSet<_> = batch
                .messages
                .iter()
                .map(|pending| pending.message.id.get())
                .collect();
            state
                .pending
                .retain(|pending| !covered_ids.contains(&pending.message.id.get()));
        }
        if state.pending.is_empty() && state.active_generation.is_none() {
            channels.remove(&batch.channel_id);
        }
    }
}

struct ToolStatusSinkGuard {
    shard: ShardMessenger,
    coordinator: Arc<ToolActivityCoordinator>,
    active: AtomicBool,
}

impl ToolStatusSinkGuard {
    fn new(shard: ShardMessenger, coordinator: Arc<ToolActivityCoordinator>) -> Self {
        Self {
            shard,
            coordinator,
            active: AtomicBool::new(false),
        }
    }
}

#[derive(Default)]
struct ToolActivityCoordinator {
    state: StdMutex<ToolActivityState>,
}

#[derive(Default)]
struct ToolActivityState {
    active_responses: usize,
}

impl ToolActivityState {
    fn start(&mut self) {
        self.active_responses += 1;
    }

    fn finish(&mut self) -> bool {
        debug_assert!(self.active_responses > 0);
        self.active_responses = self.active_responses.saturating_sub(1);
        self.active_responses == 0
    }
}

impl ToolActivityCoordinator {
    fn start(&self, shard: &ShardMessenger, tool_name: &str) {
        let mut state = self.state.lock().expect("tool activity lock poisoned");
        state.start();
        shard.set_activity(Some(ActivityData::custom(format!(
            "ツール実行中: {tool_name}"
        ))));
    }

    fn update(&self, shard: &ShardMessenger, tool_name: &str) {
        let _state = self.state.lock().expect("tool activity lock poisoned");
        shard.set_activity(Some(ActivityData::custom(format!(
            "ツール実行中: {tool_name}"
        ))));
    }

    fn finish(&self, shard: &ShardMessenger) {
        let mut state = self.state.lock().expect("tool activity lock poisoned");
        if state.finish() {
            shard.set_activity(None);
        }
    }
}

impl ToolStatusSink for ToolStatusSinkGuard {
    fn tool_started(&self, tool_name: &str) {
        if !self.active.swap(true, Ordering::AcqRel) {
            self.coordinator.start(&self.shard, tool_name);
        } else {
            self.coordinator.update(&self.shard, tool_name);
        }
    }

    fn idle(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.coordinator.finish(&self.shard);
        }
    }
}

impl Drop for ToolStatusSinkGuard {
    fn drop(&mut self) {
        self.idle();
    }
}

pub struct DiscordBot {
    guild_id: GuildId,
    database: Database,
    model: SharedLanguageModel,
    decision_models: Vec<SharedLanguageModel>,
    intervention_threshold: f64,
    bot_user_id: AtomicU64,
    bot_owner_id: AtomicU64,
    bot_name: RwLock<String>,
    settings_channel_cache: RwLock<HashMap<u64, u64>>,
    conversations: ConversationCoordinator,
    tool_activity: Arc<ToolActivityCoordinator>,
    runner: Option<SharedCodeRunner>,
}

impl DiscordBot {
    pub fn new(
        guild_id: GuildId,
        database: Database,
        model: SharedLanguageModel,
        decision_models: Vec<SharedLanguageModel>,
        intervention_threshold: f64,
        runner: Option<SharedCodeRunner>,
    ) -> Self {
        Self {
            guild_id,
            database,
            model,
            decision_models,
            intervention_threshold,
            bot_user_id: AtomicU64::new(0),
            bot_owner_id: AtomicU64::new(0),
            bot_name: RwLock::new(String::new()),
            settings_channel_cache: RwLock::new(HashMap::new()),
            conversations: ConversationCoordinator::default(),
            tool_activity: Arc::new(ToolActivityCoordinator::default()),
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

    async fn message_update(
        &self,
        _context: Context,
        _old_if_available: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        if event.guild_id.is_some() && event.guild_id != Some(self.guild_id) {
            return;
        }
        let attachments = match event.attachments.as_deref() {
            Some(attachments) => Some(extract_attachments(attachments).await),
            None => None,
        };
        let edited_at = event
            .edited_timestamp
            .map(|timestamp| timestamp.to_string());
        if let Err(error) = self
            .database
            .update_message(
                event.id.get(),
                event.content.as_deref(),
                attachments.as_deref(),
                edited_at.as_deref(),
            )
            .await
        {
            tracing::error!(
                ?error,
                message_id = event.id.get(),
                "failed to sync message edit"
            );
        }
    }

    async fn message_delete(
        &self,
        _context: Context,
        _channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        if guild_id.is_some() && guild_id != Some(self.guild_id) {
            return;
        }
        if let Err(error) = self.database.delete_message(deleted_message_id.get()).await {
            tracing::error!(
                ?error,
                message_id = deleted_message_id.get(),
                "failed to sync message deletion"
            );
        }
    }

    async fn message_delete_bulk(
        &self,
        _context: Context,
        _channel_id: ChannelId,
        deleted_message_ids: Vec<MessageId>,
        guild_id: Option<GuildId>,
    ) {
        if guild_id.is_some() && guild_id != Some(self.guild_id) {
            return;
        }
        let ids: Vec<_> = deleted_message_ids.iter().map(|id| id.get()).collect();
        if let Err(error) = self.database.delete_messages(&ids).await {
            tracing::error!(?error, "failed to sync bulk message deletion");
        }
    }
}

impl DiscordBot {
    async fn handle_message(&self, context: &Context, message: &Message) -> Result<()> {
        let settings_channel_id = self
            .cached_configuration_channel_id(context, message.channel_id)
            .await?;
        let mode = self
            .database
            .channel_mode(self.guild_id.get(), settings_channel_id.get())
            .await?;
        if !mode.stores_messages() {
            return Ok(());
        }

        let (referenced_message_id, referenced_message) =
            if let Some(ref_msg) = &message.referenced_message {
                (
                    Some(ref_msg.id.get()),
                    Some(crate::model::ReferencedMessage {
                        message_id: ref_msg.id.get(),
                        channel_id: ref_msg.channel_id.get(),
                        author_id: ref_msg.author.id.get(),
                        author_name: ref_msg
                            .author
                            .global_name
                            .clone()
                            .unwrap_or_else(|| ref_msg.author.name.clone()),
                        content: ref_msg.content.clone(),
                        is_bot: ref_msg.author.bot,
                        created_at: ref_msg.timestamp.to_string(),
                    }),
                )
            } else if let Some(msg_ref) = &message.message_reference {
                if let Some(msg_id) = msg_ref.message_id {
                    let chan_id = msg_ref.channel_id.get();
                    let ref_msg =
                        resolve_referenced_message(context, &self.database, chan_id, msg_id.get())
                            .await;
                    (Some(msg_id.get()), Some(ref_msg))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

        let channel_name = match message.channel_id.to_channel(&context.http).await {
            Ok(Channel::Guild(channel)) => Some(channel.name),
            _ => None,
        };
        let attachments = extract_attachments(&message.attachments).await;
        self.database
            .record_message(&IncomingMessage {
                discord_message_id: message.id.get(),
                guild_id: self.guild_id.get(),
                channel_id: message.channel_id.get(),
                channel_name: channel_name.clone(),
                settings_channel_id: settings_channel_id.get(),
                author_id: message.author.id.get(),
                author_name: message.author.name.clone(),
                content: message.content.clone(),
                is_bot: false,
                created_at: message.timestamp.to_string(),
                referenced_message_id,
                referenced_message: referenced_message.clone(),
                attachments,
            })
            .await?;

        if mode == crate::model::ChannelMode::ReadOnly {
            return Ok(());
        }
        let bot_user_id = self.bot_user_id.load(Ordering::Relaxed);
        let user_mentioned = bot_user_id != 0
            && message
                .mentions
                .iter()
                .any(|user| user.id.get() == bot_user_id);
        let invocation_role_id = self
            .database
            .invocation_role_id(self.guild_id.get())
            .await?;
        let role_mentioned =
            is_invocation_role_mentioned(invocation_role_id, message.mention_roles.as_slice());
        let mentioned = user_mentioned || role_mentioned;
        let direct = bot_user_id != 0
            && message
                .referenced_message
                .as_ref()
                .is_some_and(|referenced| referenced.author.id.get() == bot_user_id);
        let ticket = self
            .conversations
            .enqueue(PendingConversationMessage {
                message: message.clone(),
                channel_name,
                settings_channel_id,
                mode,
                mentioned,
                direct,
            })
            .await;
        let Some(batch) = self.conversations.claim_after_delay(ticket).await else {
            return Ok(());
        };
        let mut updates = batch.updates.clone();
        let prepared = tokio::select! {
            result = self.prepare_conversation_response(context, &batch) => result,
            changed = updates.changed() => {
                if changed.is_ok() {
                    tracing::debug!(
                        channel_id = batch.channel_id,
                        generation = batch.generation,
                        "cancelling conversation response because newer messages arrived"
                    );
                }
                self.conversations.finish(&batch, false).await;
                return Ok(());
            }
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.conversations.finish(&batch, true).await;
                return Err(error);
            }
        };
        if !self.conversations.is_current(&batch).await {
            self.conversations.finish(&batch, false).await;
            return Ok(());
        }
        for requested in prepared.action.reactions.iter().take(3) {
            let Some(reaction) = resolve_reaction(requested, &prepared.custom_emojis) else {
                tracing::warn!(reaction = %requested, "AI selected an unavailable reaction");
                continue;
            };
            if let Err(error) = prepared.source.react(&context.http, reaction).await {
                tracing::warn!(?error, "failed to add AI-selected reaction");
            }
        }
        let result = if let Some(answer) = prepared.answer {
            self.send_answer(
                context,
                &prepared.source,
                prepared.settings_channel_id,
                &answer,
            )
            .await
        } else {
            Ok(())
        };
        self.conversations.finish(&batch, true).await;
        result
    }

    async fn prepare_conversation_response(
        &self,
        context: &Context,
        batch: &ConversationBatch,
    ) -> Result<PreparedConversationResponse> {
        let latest = batch
            .messages
            .last()
            .context("conversation batch is empty")?;
        let source = latest.message.clone();
        let mode = latest.mode;
        let channel_name = latest.channel_name.clone();
        let settings_channel_id = latest.settings_channel_id;
        let context_limit = u32::try_from(batch.messages.len())
            .unwrap_or(u32::MAX)
            .clamp(
                RECENT_CONTEXT_MESSAGE_LIMIT,
                MAX_CONVERSATION_BATCH_MESSAGES as u32,
            );
        let mut current_channel_context = self
            .database
            .recent_channel_context(self.guild_id.get(), source.channel_id.get(), context_limit)
            .await?;

        for ctx_msg in &mut current_channel_context {
            let linked_urls = extract_discord_message_urls(&ctx_msg.content);
            for (linked_chan_id, linked_msg_id) in linked_urls {
                if ctx_msg.message_id != linked_msg_id
                    && !ctx_msg
                        .linked_messages
                        .iter()
                        .any(|m| m.message_id == linked_msg_id)
                {
                    let resolved = resolve_referenced_message(
                        context,
                        &self.database,
                        linked_chan_id,
                        linked_msg_id,
                    )
                    .await;
                    ctx_msg.linked_messages.push(resolved);
                }
            }
        }
        let recent_active_channels = self
            .database
            .recent_active_channels(
                self.guild_id.get(),
                source.channel_id.get(),
                RECENT_ACTIVE_CHANNEL_LIMIT,
            )
            .await?;
        let bot_user_id = self.bot_user_id.load(Ordering::Relaxed);
        let mentioned = batch.messages.iter().any(|pending| pending.mentioned);
        let direct = batch.messages.iter().any(|pending| pending.direct);
        let explicit_request = mentioned
            || batch
                .messages
                .iter()
                .any(|pending| pending.mode == crate::model::ChannelMode::Auto && pending.direct);
        let response_mode = if direct {
            ResponseMode::Direct
        } else if mentioned {
            ResponseMode::Invoked
        } else {
            ResponseMode::Passive
        };
        let runtime_context = self
            .runtime_context(
                context,
                &source,
                response_mode,
                bot_user_id,
                channel_name,
                recent_active_channels,
            )
            .await;
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
            &self.decision_models,
            mode,
            explicit_request,
            self.intervention_threshold,
            &current_channel_context,
            &runtime_context,
            &emoji_names,
        )
        .await?;
        if !action.should_reply {
            return Ok(PreparedConversationResponse {
                source,
                settings_channel_id,
                action,
                custom_emojis,
                answer: None,
            });
        }

        let _typing = TypingGuard::start(context, source.channel_id);
        let memories = self.database.active_memories(self.guild_id.get()).await?;
        let status_sink =
            ToolStatusSinkGuard::new(context.shard.clone(), Arc::clone(&self.tool_activity));
        let answer = generate_reviewed_answer(
            self.model.as_ref(),
            &current_channel_context,
            &runtime_context,
            &memories,
            &self.database,
            source.id.get(),
            self.runner.as_deref(),
            Some(&status_sink),
        )
        .await
        .unwrap_or_else(|error| {
            tracing::error!(?error, "answer generation or review exhausted all retries");
            GENERATION_FAILURE_MESSAGE.to_owned()
        });
        Ok(PreparedConversationResponse {
            source,
            settings_channel_id,
            action,
            custom_emojis,
            answer: Some(answer),
        })
    }

    async fn runtime_context(
        &self,
        context: &Context,
        message: &Message,
        mode: ResponseMode,
        bot_user_id: u64,
        channel_name: Option<String>,
        recent_active_channels: Vec<RecentChannelActivity>,
    ) -> RuntimeContext {
        let server_name = self
            .guild_id
            .to_partial_guild(&context.http)
            .await
            .ok()
            .map(|guild| guild.name);
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
            recent_active_channels,
            now: crate::context::now_iso_string(),
        }
    }

    async fn cached_configuration_channel_id(
        &self,
        context: &Context,
        channel_id: ChannelId,
    ) -> Result<ChannelId> {
        if let Some(settings_channel_id) = self
            .settings_channel_cache
            .read()
            .ok()
            .and_then(|cache| cache.get(&channel_id.get()).copied())
        {
            return Ok(ChannelId::new(settings_channel_id));
        }

        let settings_channel_id = configuration_channel_id(context, channel_id).await?;
        if let Ok(mut cache) = self.settings_channel_cache.write() {
            cache.insert(channel_id.get(), settings_channel_id.get());
        }
        Ok(settings_channel_id)
    }

    async fn send_answer(
        &self,
        context: &Context,
        source_message: &Message,
        settings_channel_id: ChannelId,
        answer: &str,
    ) -> Result<()> {
        let chunks = split_message(answer);
        if chunks.is_empty() {
            anyhow::bail!("reviewed answer was empty");
        }

        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut builder = CreateMessage::new().content(chunk).allowed_mentions(
                CreateAllowedMentions::new()
                    .all_users(true)
                    .replied_user(true),
            );
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
                    channel_name: None,
                    settings_channel_id: settings_channel_id.get(),
                    author_id: sent.author.id.get(),
                    author_name: sent.author.name,
                    content: sent.content,
                    is_bot: true,
                    created_at: sent.timestamp.to_string(),
                    referenced_message_id: (index == 0).then_some(source_message.id.get()),
                    referenced_message: (index == 0).then(|| crate::model::ReferencedMessage {
                        message_id: source_message.id.get(),
                        channel_id: source_message.channel_id.get(),
                        author_id: source_message.author.id.get(),
                        author_name: source_message
                            .author
                            .global_name
                            .clone()
                            .unwrap_or_else(|| source_message.author.name.clone()),
                        content: source_message.content.clone(),
                        is_bot: source_message.author.bot,
                        created_at: source_message.timestamp.to_string(),
                    }),
                    attachments: vec![],
                })
                .await?;
        }
        Ok(())
    }
}

fn retain_latest<T>(items: &mut Vec<T>, limit: usize) -> usize {
    let dropped = items.len().saturating_sub(limit);
    if dropped > 0 {
        items.drain(..dropped);
    }
    dropped
}

struct TypingGuard {
    stop: Option<oneshot::Sender<()>>,
    _task: JoinHandle<()>,
}

impl TypingGuard {
    fn start(context: &Context, channel_id: ChannelId) -> Self {
        let http = context.http.clone();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(TYPING_REFRESH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = interval.tick() => {
                        if let Err(error) = channel_id.broadcast_typing(&http).await {
                            tracing::debug!(?error, "failed to refresh Discord typing indicator");
                        }
                    }
                }
            }
        });
        Self {
            stop: Some(stop),
            _task: task,
        }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub(super) async fn configuration_channel_id(
    context: &Context,
    channel_id: ChannelId,
) -> Result<ChannelId> {
    let channel = channel_id
        .to_channel(&context.http)
        .await
        .context("failed to resolve message channel")?;
    let Channel::Guild(channel) = channel else {
        return Ok(channel_id);
    };
    Ok(inherited_settings_channel_id(
        channel.id,
        channel.parent_id,
        channel.kind,
    ))
}

fn inherited_settings_channel_id(
    channel_id: ChannelId,
    parent_id: Option<ChannelId>,
    channel_type: ChannelType,
) -> ChannelId {
    if matches!(
        channel_type,
        ChannelType::NewsThread | ChannelType::PublicThread | ChannelType::PrivateThread
    ) {
        parent_id.unwrap_or(channel_id)
    } else {
        channel_id
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

fn is_invocation_role_mentioned(
    configured_role_id: Option<u64>,
    mentioned_roles: &[RoleId],
) -> bool {
    configured_role_id.is_some_and(|configured_role_id| {
        mentioned_roles
            .iter()
            .any(|role_id| role_id.get() == configured_role_id)
    })
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

fn extract_discord_message_urls(content: &str) -> Vec<(u64, u64)> {
    let mut results = Vec::new();
    let re = match regex::Regex::new(
        r"https://(?:ptb\.|canary\.)?discord\.com/channels/(?:@me|\d+)/(\d+)/(\d+)",
    ) {
        Ok(re) => re,
        Err(_) => return results,
    };
    for captures in re.captures_iter(content) {
        if let (Some(channel_mat), Some(message_mat)) = (captures.get(1), captures.get(2))
            && let (Ok(channel_id), Ok(message_id)) = (
                channel_mat.as_str().parse::<u64>(),
                message_mat.as_str().parse::<u64>(),
            )
            && !results.contains(&(channel_id, message_id))
        {
            results.push((channel_id, message_id));
        }
    }
    results.truncate(5);
    results
}

async fn resolve_referenced_message(
    context: &Context,
    database: &Database,
    channel_id: u64,
    message_id: u64,
) -> crate::model::ReferencedMessage {
    if let Ok(Some(cached)) = database.get_message_by_discord_id(message_id).await {
        return cached;
    }

    match context
        .http
        .get_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
    {
        Ok(disc_msg) => crate::model::ReferencedMessage {
            message_id: disc_msg.id.get(),
            channel_id: disc_msg.channel_id.get(),
            author_id: disc_msg.author.id.get(),
            author_name: disc_msg.author.global_name.unwrap_or(disc_msg.author.name),
            content: disc_msg.content,
            is_bot: disc_msg.author.bot,
            created_at: disc_msg.timestamp.to_string(),
        },
        Err(_) => crate::model::ReferencedMessage {
            message_id,
            channel_id,
            author_id: 0,
            author_name: "Unknown".to_owned(),
            content: "[メッセージを取得できませんでした]".to_owned(),
            is_bot: false,
            created_at: String::new(),
        },
    }
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

    #[test]
    fn detects_only_the_configured_invocation_role() {
        let mentioned_roles = [RoleId::new(10), RoleId::new(20)];
        assert!(is_invocation_role_mentioned(Some(20), &mentioned_roles));
        assert!(!is_invocation_role_mentioned(Some(30), &mentioned_roles));
        assert!(!is_invocation_role_mentioned(None, &mentioned_roles));
    }

    #[test]
    fn threads_inherit_their_parent_channel_setting() {
        let thread_id = ChannelId::new(10);
        let parent_id = ChannelId::new(20);
        assert_eq!(
            inherited_settings_channel_id(thread_id, Some(parent_id), ChannelType::PublicThread),
            parent_id
        );
        assert_eq!(
            inherited_settings_channel_id(thread_id, Some(parent_id), ChannelType::PrivateThread),
            parent_id
        );
        assert_eq!(
            inherited_settings_channel_id(thread_id, Some(parent_id), ChannelType::Text),
            thread_id
        );
    }

    #[test]
    fn extracts_discord_message_urls() {
        let content = "これを見て: https://discord.com/channels/12345/67890/999888777 あとこれも https://ptb.discord.com/channels/@me/111/222";
        let extracted = extract_discord_message_urls(content);
        assert_eq!(extracted, vec![(67890, 999888777), (111, 222)]);
    }

    #[test]
    fn tool_activity_counter_tracks_every_active_response() {
        let mut state = ToolActivityState::default();
        state.start();
        state.start();

        assert!(!state.finish());
        assert!(state.finish());
    }

    #[test]
    fn conversation_state_only_allows_the_latest_generation_to_run() {
        let mut state = ChannelConversationState::new();
        let first = state.advance_generation();
        assert!(state.try_claim(first));

        let second = state.advance_generation();
        assert!(!state.try_claim(first));
        assert!(!state.try_claim(second));

        state.release(first);
        assert!(state.try_claim(second));
    }

    #[test]
    fn conversation_batch_retains_only_the_latest_messages() {
        let mut messages = vec![1, 2, 3, 4];
        assert_eq!(retain_latest(&mut messages, 3), 1);
        assert_eq!(messages, vec![2, 3, 4]);
    }
}
