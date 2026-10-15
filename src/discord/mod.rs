mod commands;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use serenity::builder::GetMessages;
use serenity::{
    all::{
        Attachment, Channel, ChannelId, ChannelType, Context, CreateAllowedMentions,
        CreateAttachment, CreateMessage, Emoji, EventHandler, GatewayIntents, GuildId, Interaction,
        Message, MessageId, MessageType, MessageUpdateEvent, ReactionType, Ready, RoleId,
    },
    async_trait,
    gateway::{ActivityData, ShardMessenger},
    http::Http,
};
use tokio::{
    sync::{Mutex, Notify, oneshot, watch},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    ai::SharedLanguageModel,
    context::{ResponseMode, RuntimeBot, RuntimeContext, RuntimeLocation, RuntimeUser},
    db::Database,
    delivery::split_message,
    model::{IncomingAttachment, IncomingMessage, RecentChannelActivity},
    runner::SharedCodeRunner,
    tutor::{
        BotAction, GeneratedAnswer, MessageHistoryReader, ToolStatusSink, decide_action,
        generate_reviewed_answer_with_history,
    },
};

const RECENT_CONTEXT_MESSAGE_LIMIT: u32 = 5;
const MAX_CONVERSATION_BATCH_MESSAGES: usize = 80;
const RECENT_ACTIVE_CHANNEL_LIMIT: u32 = 12;
const MAX_TEXT_ATTACHMENT_BYTES: u32 = 512 * 1024;
const MAX_MULTIMODAL_ATTACHMENT_BYTES: u32 = 20 * 1024 * 1024;
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);
const CONVERSATION_BATCH_DELAY: Duration = Duration::from_millis(1_200);
const CONVERSATION_BATCH_MAX_WAIT: Duration = Duration::from_secs(4);
const MAX_GENERATION_RESTARTS: u8 = 1;
const DISCORD_SEND_ATTEMPTS: usize = 3;
const DISCORD_SEND_RETRY_DELAY: Duration = Duration::from_millis(500);
const GENERATION_FAILURE_MESSAGE: &str = "回答生成またはレビューが一時的に完了しませんでした。少し待ってから、もう一度呼び出してください。";

#[derive(Clone)]
struct PendingConversationMessage {
    message: Message,
    enqueued_at: Instant,
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
    may_restart: bool,
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
    answer: Option<GeneratedAnswer>,
}

#[derive(Clone, Copy)]
struct HistoryTarget {
    channel_id: ChannelId,
    settings_channel_id: ChannelId,
}

struct ChannelConversationState {
    generation: u64,
    active_generation: Option<u64>,
    pending: Vec<PendingConversationMessage>,
    batch_started_at: Instant,
    generation_restarts: u8,
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
            batch_started_at: Instant::now(),
            generation_restarts: 0,
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

    fn may_restart(&self) -> bool {
        self.generation_restarts < MAX_GENERATION_RESTARTS
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
        if state.pending.is_empty() {
            state.batch_started_at = pending.enqueued_at;
        }
        state.pending.push(pending);
        let dropped = retain_latest_prioritizing(
            &mut state.pending,
            MAX_CONVERSATION_BATCH_MESSAGES,
            |pending| {
                pending.mentioned
                    || (pending.mode == crate::model::ChannelMode::Auto && pending.direct)
            },
        );
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
        let ready_at = {
            let channels = self.channels.lock().await;
            let state = channels.get(&ticket.channel_id)?;
            if state.generation != ticket.generation {
                return None;
            }
            let latest = state.pending.last()?.enqueued_at;
            conversation_batch_ready_at(state.batch_started_at, latest)
        };
        tokio::time::sleep_until(ready_at).await;
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
                        may_restart: state.may_restart(),
                    });
                }
                Arc::clone(&state.availability).notified_owned()
            };
            availability.await;
        }
    }

    async fn can_commit(&self, batch: &ConversationBatch) -> bool {
        self.channels
            .lock()
            .await
            .get(&batch.channel_id)
            .is_some_and(|state| {
                state.active_generation == Some(batch.generation)
                    && (!batch.may_restart || state.generation == batch.generation)
            })
    }

    async fn restart(&self, batch: &ConversationBatch) {
        let mut channels = self.channels.lock().await;
        let Some(state) = channels.get_mut(&batch.channel_id) else {
            return;
        };
        if state.active_generation == Some(batch.generation) {
            state.generation_restarts = state.generation_restarts.saturating_add(1);
        }
        state.release(batch.generation);
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
            state.generation_restarts = 0;
            if let Some(first) = state.pending.first() {
                state.batch_started_at = first.enqueued_at;
            }
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
    history_sync_started: Arc<AtomicBool>,
    history_sync_notify: Arc<Notify>,
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
            history_sync_started: Arc::new(AtomicBool::new(false)),
            history_sync_notify: Arc::new(Notify::new()),
        }
    }

    fn start_history_sync(&self, context: &Context) {
        if self
            .history_sync_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.history_sync_notify.notify_one();
            return;
        }

        let database = self.database.clone();
        let guild_id = self.guild_id;
        let http = context.http.clone();
        let notify = Arc::clone(&self.history_sync_notify);
        tokio::spawn(async move {
            loop {
                if let Err(error) = sync_allowed_history(&http, &database, guild_id).await {
                    tracing::error!(?error, "failed to synchronize Discord message history");
                }
                notify.notified().await;
            }
        });
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
        self.start_history_sync(&context);
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
        } else {
            self.start_history_sync(&context);
        }
    }

    async fn message(&self, context: Context, message: Message) {
        if message.guild_id != Some(self.guild_id) {
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

        let reference = message
            .message_reference
            .as_ref()
            .and_then(|reference| {
                reference
                    .message_id
                    .map(|id| (reference.channel_id.get(), id.get()))
            })
            .or_else(|| {
                message
                    .referenced_message
                    .as_ref()
                    .map(|message| (message.channel_id.get(), message.id.get()))
            });
        let referenced_message_id = reference.map(|(_, id)| id);
        let referenced_message = match reference {
            Some((channel, id)) => {
                Some(self.resolve_referenced_message(context, channel, id).await)
            }
            None => None,
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
                is_bot: message.author.bot,
                created_at: message.timestamp.to_string(),
                referenced_message_id,
                referenced_message: referenced_message.clone(),
                attachments,
            })
            .await?;

        if message.author.bot
            || mode == crate::model::ChannelMode::ReadOnly
            || !can_reply_to(message.kind)
        {
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
                enqueued_at: Instant::now(),
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
        let prepared = if batch.may_restart {
            let mut updates = batch.updates.clone();
            tokio::select! {
                result = self.prepare_conversation_response(context, &batch) => result,
                changed = updates.changed() => {
                    if changed.is_ok() {
                        tracing::debug!(
                            channel_id = batch.channel_id,
                            generation = batch.generation,
                            "restarting conversation response once to include newer messages"
                        );
                    }
                    self.conversations.restart(&batch).await;
                    return Ok(());
                }
            }
        } else {
            self.prepare_conversation_response(context, &batch).await
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.conversations.finish(&batch, true).await;
                return Err(error);
            }
        };
        if !self.conversations.can_commit(&batch).await {
            if batch.may_restart {
                self.conversations.restart(&batch).await;
            } else {
                self.conversations.finish(&batch, false).await;
            }
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
        self.conversations.finish(&batch, result.is_ok()).await;
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
            for (linked_guild_id, linked_chan_id, linked_msg_id) in linked_urls {
                if ctx_msg.message_id != linked_msg_id
                    && !ctx_msg
                        .linked_messages
                        .iter()
                        .any(|m| m.message_id == linked_msg_id)
                {
                    let resolved = if linked_guild_id == self.guild_id.get() {
                        self.resolve_referenced_message(context, linked_chan_id, linked_msg_id)
                            .await
                    } else {
                        unavailable_referenced_message(linked_chan_id, linked_msg_id)
                    };
                    ctx_msg.linked_messages.push(resolved);
                }
            }
        }
        // Include the full parent message, including attachments, rather than only its text summary.
        let mut referenced_context = Vec::new();
        for message in current_channel_context.iter().rev().take(5) {
            if let Some(reference) = &message.reply_to
                && !current_channel_context
                    .iter()
                    .any(|message| message.message_id == reference.message_id)
                && !referenced_context
                    .iter()
                    .any(|message: &crate::model::ContextMessage| {
                        message.message_id == reference.message_id
                    })
                && let Some(parent) = self
                    .database
                    .message_context_by_id(self.guild_id.get(), reference.message_id)
                    .await?
            {
                referenced_context.push(parent);
            }
        }
        referenced_context.reverse();
        referenced_context.extend(current_channel_context);
        let current_channel_context = referenced_context;
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
        let answer = generate_reviewed_answer_with_history(
            self.model.as_ref(),
            &current_channel_context,
            &runtime_context,
            &memories,
            &self.database,
            source.id.get(),
            self.runner.as_deref(),
            Some(&status_sink),
            Some(&DiscordHistoryReader {
                database: &self.database,
                http: &context.http,
                guild_id: self.guild_id,
            }),
        )
        .await
        .unwrap_or_else(|error| {
            tracing::error!(?error, "answer generation or review exhausted all retries");
            GeneratedAnswer {
                text: GENERATION_FAILURE_MESSAGE.to_owned(),
                artifacts: vec![],
            }
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
        answer: &GeneratedAnswer,
    ) -> Result<()> {
        let chunks = split_message(&answer.text);
        if chunks.is_empty() {
            anyhow::bail!("reviewed answer was empty");
        }

        for (index, chunk) in chunks.into_iter().enumerate() {
            let (sent, reply) = send_answer_chunk(
                &context.http,
                source_message,
                &chunk,
                index,
                &answer.artifacts,
            )
            .await?;
            if let Err(error) = self
                .database
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
                    referenced_message_id: reply.then_some(source_message.id.get()),
                    referenced_message: reply.then(|| crate::model::ReferencedMessage {
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
                    attachments: if index == 0 {
                        answer
                            .artifacts
                            .iter()
                            .map(|artifact| IncomingAttachment {
                                filename: artifact.filename.clone(),
                                content_type: Some("image/png".into()),
                                size_bytes: artifact.bytes.len() as u64,
                                extracted_text: None,
                                binary_data: Some(artifact.bytes.clone()),
                            })
                            .collect()
                    } else {
                        vec![]
                    },
                })
                .await
            {
                tracing::error!(
                    ?error,
                    message_id = sent.id.get(),
                    "Discord answer was sent but could not be recorded"
                );
            }
        }
        Ok(())
    }

    async fn resolve_referenced_message(
        &self,
        context: &Context,
        channel_id: u64,
        message_id: u64,
    ) -> crate::model::ReferencedMessage {
        match (DiscordHistoryReader {
            database: &self.database,
            http: &context.http,
            guild_id: self.guild_id,
        })
        .message(channel_id, message_id)
        .await
        {
            Ok(message) => crate::model::ReferencedMessage {
                message_id: message.message_id,
                channel_id: message.channel_id,
                author_id: message.author_id,
                author_name: message.author_name,
                content: message.content,
                is_bot: message.is_bot,
                created_at: message.created_at,
            },
            Err(error) => {
                tracing::debug!(%error, channel_id, message_id, "referenced message unavailable");
                unavailable_referenced_message(channel_id, message_id)
            }
        }
    }
}

struct DiscordHistoryReader<'a> {
    database: &'a Database,
    http: &'a Http,
    guild_id: GuildId,
}

#[async_trait]
impl MessageHistoryReader for DiscordHistoryReader<'_> {
    async fn message(
        &self,
        channel_id: u64,
        message_id: u64,
    ) -> Result<crate::model::ContextMessage> {
        let channel_id = ChannelId::new(channel_id);
        let Channel::Guild(channel) = channel_id.to_channel(self.http).await? else {
            anyhow::bail!("message channel is not a guild channel");
        };
        anyhow::ensure!(
            channel.guild_id == self.guild_id,
            "message channel belongs to another server"
        );
        let settings_channel_id =
            inherited_settings_channel_id(channel.id, channel.parent_id, channel.kind);
        anyhow::ensure!(
            self.database
                .channel_mode(self.guild_id.get(), settings_channel_id.get())
                .await?
                .stores_messages(),
            "message channel does not allow AI access"
        );
        let message = channel_id
            .message(self.http, MessageId::new(message_id))
            .await?;
        // REST message objects can omit guild_id; the channel above establishes the guild.
        anyhow::ensure!(
            message.id.get() == message_id
                && message.channel_id == channel_id
                && message.guild_id.is_none_or(|guild| guild == self.guild_id),
            "message belongs to another channel or server"
        );
        let attachments = extract_attachments(&message.attachments).await;
        self.database
            .record_message(&IncomingMessage {
                discord_message_id: message.id.get(),
                guild_id: self.guild_id.get(),
                channel_id: channel_id.get(),
                channel_name: Some(channel.name),
                settings_channel_id: settings_channel_id.get(),
                author_id: message.author.id.get(),
                author_name: message
                    .author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| message.author.name.clone()),
                content: message.content.clone(),
                is_bot: message.author.bot,
                created_at: message.timestamp.to_string(),
                referenced_message_id: message
                    .message_reference
                    .as_ref()
                    .and_then(|reference| reference.message_id.map(|id| id.get())),
                referenced_message: None,
                attachments: attachments.clone(),
            })
            .await?;
        // Refresh old backfill rows too: their image/PDF bytes were deliberately omitted.
        self.database
            .update_message(
                message.id.get(),
                Some(&message.content),
                Some(&attachments),
                message
                    .edited_timestamp
                    .map(|timestamp| timestamp.to_string())
                    .as_deref(),
            )
            .await?;
        let mut result = self
            .database
            .message_context_by_id(self.guild_id.get(), message_id)
            .await?
            .context("message is unavailable or its channel no longer allows AI access")?;
        if result.reply_to.is_none()
            && let Some(reference) = &message.message_reference
            && let Some(parent_id) = reference.message_id
        {
            result.reply_to = Some(unavailable_referenced_message(
                reference.channel_id.get(),
                parent_id.get(),
            ));
        }
        Ok(result)
    }
}

fn retain_latest_prioritizing<T>(
    items: &mut Vec<T>,
    limit: usize,
    is_priority: impl Fn(&T) -> bool,
) -> usize {
    let dropped = items.len().saturating_sub(limit);
    for _ in 0..dropped {
        let index = items
            .iter()
            .position(|item| !is_priority(item))
            .unwrap_or(0);
        items.remove(index);
    }
    dropped
}

fn conversation_batch_ready_at(first: Instant, latest: Instant) -> Instant {
    std::cmp::min(
        latest + CONVERSATION_BATCH_DELAY,
        first + CONVERSATION_BATCH_MAX_WAIT,
    )
}

fn answer_allowed_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new().replied_user(true)
}

async fn send_answer_chunk(
    http: &Http,
    source_message: &Message,
    chunk: &str,
    chunk_index: usize,
    artifacts: &[crate::runner::GeneratedArtifact],
) -> Result<(Message, bool)> {
    let first = chunk_index == 0;
    let mut sent = None;
    let mut last_error = None;
    let mut reply = first && can_reply_to(source_message.kind);
    for attempt in 1..=DISCORD_SEND_ATTEMPTS {
        let mut builder = CreateMessage::new()
            .content(chunk)
            .allowed_mentions(answer_allowed_mentions());
        if reply {
            builder = builder.reference_message(source_message);
        }
        if first {
            for artifact in artifacts {
                builder = builder.add_file(CreateAttachment::bytes(
                    artifact.bytes.clone(),
                    artifact.filename.clone(),
                ));
            }
        }
        match source_message.channel_id.send_message(http, builder).await {
            Ok(message) => {
                sent = Some(message);
                break;
            }
            Err(error) => {
                let drop_reference = reply && rejected_reply_reference(&error);
                tracing::warn!(
                    ?error,
                    attempt,
                    max_attempts = DISCORD_SEND_ATTEMPTS,
                    chunk_index = chunk_index,
                    "failed to send reviewed answer chunk"
                );
                last_error = Some(error);
                if drop_reference {
                    reply = false;
                } else if !retryable_send_error(last_error.as_ref().expect("send error")) {
                    break;
                }
                if attempt < DISCORD_SEND_ATTEMPTS {
                    tokio::time::sleep(DISCORD_SEND_RETRY_DELAY).await;
                }
            }
        }
    }
    let sent = sent.ok_or_else(|| {
        anyhow::anyhow!(
            "failed to send reviewed answer after retries: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "unknown Discord error".to_owned())
        )
    })?;
    Ok((sent, reply))
}

fn can_reply_to(kind: MessageType) -> bool {
    matches!(
        kind,
        MessageType::Regular
            | MessageType::InlineReply
            | MessageType::ChatInputCommand
            | MessageType::ContextMenuCommand
    )
}

fn rejected_reply_reference(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response))
        if response.error.code == 10008 || (response.error.code == 50035 && response.error.errors.iter().any(|error|
            error.path == "message_reference" && error.code == "REPLIES_CANNOT_REPLY_TO_SYSTEM_MESSAGE")))
}

fn retryable_send_error(error: &serenity::Error) -> bool {
    match error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
            response.status_code.is_server_error() || response.status_code.as_u16() == 429
        }
        serenity::Error::Http(serenity::http::HttpError::Request(_)) | serenity::Error::Io(_) => {
            true
        }
        _ => false,
    }
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
    extract_attachments_with_binary(attachments, true).await
}

async fn extract_attachments_with_binary(
    attachments: &[Attachment],
    include_binary: bool,
) -> Vec<IncomingAttachment> {
    let mut extracted = Vec::with_capacity(attachments.len());
    let mut remaining_bytes = MAX_MULTIMODAL_ATTACHMENT_BYTES as usize;
    for attachment in attachments {
        let is_text = is_text_attachment(attachment);
        let is_multimodal = is_multimodal_attachment(attachment);
        let should_download = (is_text && attachment.size <= MAX_TEXT_ATTACHMENT_BYTES)
            || (include_binary
                && is_multimodal
                && attachment.size <= MAX_MULTIMODAL_ATTACHMENT_BYTES);
        let limit = if is_multimodal && include_binary {
            MAX_MULTIMODAL_ATTACHMENT_BYTES as usize
        } else {
            MAX_TEXT_ATTACHMENT_BYTES as usize
        }
        .min(remaining_bytes);
        let bytes = if should_download && attachment.size as usize <= limit {
            match download_attachment(attachment, limit).await {
                Ok(bytes) => {
                    remaining_bytes -= bytes.len();
                    Some(bytes)
                }
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
        let extracted_text = if is_text && attachment.size <= MAX_TEXT_ATTACHMENT_BYTES {
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
            binary_data: if include_binary && is_multimodal {
                bytes
            } else {
                None
            },
        });
    }
    extracted
}

async fn download_attachment(attachment: &Attachment, limit: usize) -> Result<Vec<u8>> {
    let url = reqwest::Url::parse(&attachment.url)?;
    anyhow::ensure!(
        url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("cdn.discordapp.com" | "media.discordapp.net")
            )
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none(),
        "unsupported attachment URL"
    );
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("attachment HTTP client")
    });
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("attachment download failed"))?;
    read_attachment_response(response, limit).await
}

async fn read_attachment_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>> {
    anyhow::ensure!(
        response.status().is_success(),
        "attachment HTTP request was rejected"
    );
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limit as u64),
        "attachment exceeds byte limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("attachment body could not be read"))?
    {
        anyhow::ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "attachment exceeds byte limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn sync_allowed_history(http: &Http, database: &Database, guild_id: GuildId) -> Result<()> {
    let settings = database.channel_settings(guild_id.get()).await?;
    let allowed_settings: HashSet<_> = settings
        .iter()
        .filter(|setting| setting.mode.stores_messages())
        .map(|setting| setting.channel_id)
        .collect();
    if allowed_settings.is_empty() {
        return Ok(());
    }

    let mut targets = HashMap::new();
    let mut thread_parents = Vec::new();

    match guild_id.channels(http).await {
        Ok(channels) => {
            for channel in channels.values() {
                let settings_channel_id =
                    inherited_settings_channel_id(channel.id, channel.parent_id, channel.kind);
                if allowed_settings.contains(&settings_channel_id.get())
                    && is_history_channel(channel.kind)
                {
                    targets.insert(
                        channel.id.get(),
                        HistoryTarget {
                            channel_id: channel.id,
                            settings_channel_id,
                        },
                    );
                }
                if allowed_settings.contains(&channel.id.get())
                    && is_history_thread_parent(channel.kind)
                {
                    thread_parents.push(channel.id);
                }
            }
        }
        Err(error) => {
            tracing::warn!(?error, "failed to list guild channels for history sync");
            for channel_id in &allowed_settings {
                targets.insert(
                    *channel_id,
                    HistoryTarget {
                        channel_id: ChannelId::new(*channel_id),
                        settings_channel_id: ChannelId::new(*channel_id),
                    },
                );
            }
        }
    }

    for parent_id in thread_parents {
        if let Err(error) = add_archived_threads(http, parent_id, &mut targets).await {
            tracing::debug!(
                ?error,
                parent_id = parent_id.get(),
                "failed to list archived threads"
            );
        }
    }

    match guild_id.get_active_threads(http).await {
        Ok(active_threads) => {
            for thread in active_threads.threads {
                let Some(parent_id) = thread.parent_id else {
                    continue;
                };
                if allowed_settings.contains(&parent_id.get()) {
                    targets.insert(
                        thread.id.get(),
                        HistoryTarget {
                            channel_id: thread.id,
                            settings_channel_id: parent_id,
                        },
                    );
                }
            }
        }
        Err(error) => {
            tracing::debug!(?error, "failed to list active threads for history sync");
        }
    }

    for target in targets.into_values() {
        match sync_channel_history(http, database, guild_id, target).await {
            Ok(count) if count > 0 => tracing::info!(
                channel_id = target.channel_id.get(),
                settings_channel_id = target.settings_channel_id.get(),
                count,
                "synchronized Discord message history"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                ?error,
                channel_id = target.channel_id.get(),
                settings_channel_id = target.settings_channel_id.get(),
                "failed to synchronize one Discord channel history"
            ),
        }
    }
    Ok(())
}

async fn add_archived_threads(
    http: &Http,
    parent_id: ChannelId,
    targets: &mut HashMap<u64, HistoryTarget>,
) -> Result<()> {
    let public_error = add_archived_thread_pages(http, parent_id, targets, false)
        .await
        .err();
    let private_error = add_archived_thread_pages(http, parent_id, targets, true)
        .await
        .err();
    if let Some(error) = public_error.or(private_error) {
        return Err(error);
    }
    Ok(())
}

async fn add_archived_thread_pages(
    http: &Http,
    parent_id: ChannelId,
    targets: &mut HashMap<u64, HistoryTarget>,
    private: bool,
) -> Result<()> {
    let mut before: Option<String> = None;
    loop {
        let threads = if private {
            parent_id
                .get_joined_archived_private_threads(
                    http,
                    before.as_deref().map(str::parse).transpose()?,
                    Some(100),
                )
                .await?
        } else {
            let mut params = vec![("limit", "100".to_owned())];
            if let Some(before) = &before {
                params.push(("before", before.clone()));
            }
            // Serenity 0.12's convenience method accepts a snowflake, but Discord requires
            // an ISO8601 archive timestamp for public threads.
            http.fire::<serenity::all::ThreadsData>(
                serenity::http::Request::new(
                    serenity::http::Route::ChannelArchivedPublicThreads {
                        channel_id: parent_id,
                    },
                    serenity::http::LightMethod::Get,
                )
                .params(Some(params)),
            )
            .await?
        };
        let oldest = if private {
            threads
                .threads
                .iter()
                .map(|thread| thread.id.get())
                .min()
                .map(|id| id.to_string())
        } else {
            threads
                .threads
                .iter()
                .filter_map(|thread| {
                    thread
                        .thread_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.archive_timestamp)
                })
                .min()
                .map(|timestamp| timestamp.to_string())
        };
        for thread in threads.threads {
            targets.insert(
                thread.id.get(),
                HistoryTarget {
                    channel_id: thread.id,
                    settings_channel_id: parent_id,
                },
            );
        }
        if !threads.has_more {
            break;
        }
        anyhow::ensure!(
            oldest.is_some() && oldest != before,
            "archived thread pagination did not advance"
        );
        before = oldest;
    }
    Ok(())
}

async fn sync_channel_history(
    http: &Http,
    database: &Database,
    guild_id: GuildId,
    target: HistoryTarget,
) -> Result<usize> {
    let Channel::Guild(channel) = target.channel_id.to_channel(http).await? else {
        anyhow::bail!("history target is not a guild channel");
    };
    anyhow::ensure!(
        channel.guild_id == guild_id
            && inherited_settings_channel_id(channel.id, channel.parent_id, channel.kind)
                == target.settings_channel_id,
        "history target does not match its guild or settings channel"
    );
    anyhow::ensure!(
        database
            .channel_mode(guild_id.get(), target.settings_channel_id.get())
            .await?
            .stores_messages(),
        "history target no longer allows AI access"
    );
    let channel_name = Some(channel.name);
    let completed = database
        .history_sync_completed(guild_id.get(), target.channel_id.get())
        .await?;
    let (count, last_synced) = if completed {
        match database
            .history_sync_cursor(guild_id.get(), target.channel_id.get())
            .await?
        {
            Some(after) => {
                sync_messages_after(
                    http,
                    database,
                    guild_id,
                    target,
                    channel_name.as_deref(),
                    after,
                )
                .await?
            }
            None => {
                sync_messages_before(http, database, guild_id, target, channel_name.as_deref())
                    .await?
            }
        }
    } else {
        sync_messages_before(http, database, guild_id, target, channel_name.as_deref()).await?
    };
    database
        .mark_history_sync_completed(
            guild_id.get(),
            target.channel_id.get(),
            target.settings_channel_id.get(),
            last_synced,
        )
        .await?;
    Ok(count)
}

async fn sync_messages_before(
    http: &Http,
    database: &Database,
    guild_id: GuildId,
    target: HistoryTarget,
    channel_name: Option<&str>,
) -> Result<(usize, Option<u64>)> {
    let mut before = None;
    let mut recorded = 0;
    let mut newest = None;
    loop {
        let mut builder = GetMessages::new().limit(100);
        if let Some(before_id) = before {
            builder = builder.before(MessageId::new(before_id));
        }
        let mut messages = target.channel_id.messages(http, builder).await?;
        if messages.is_empty() {
            break;
        }
        messages.sort_unstable_by_key(|message| message.id.get());
        newest = newest.or_else(|| messages.last().map(|message| message.id.get()));
        let next_before = messages[0].id.get();
        for message in &messages {
            record_historical_message(database, guild_id, target, channel_name, message).await?;
            recorded += 1;
        }
        if Some(next_before) == before || messages.len() < 100 {
            break;
        }
        before = Some(next_before);
    }
    Ok((recorded, newest))
}

async fn sync_messages_after(
    http: &Http,
    database: &Database,
    guild_id: GuildId,
    target: HistoryTarget,
    channel_name: Option<&str>,
    after: u64,
) -> Result<(usize, Option<u64>)> {
    let mut after = after;
    let mut recorded = 0;
    loop {
        let mut messages = target
            .channel_id
            .messages(
                http,
                GetMessages::new().after(MessageId::new(after)).limit(100),
            )
            .await?;
        if messages.is_empty() {
            break;
        }
        messages.sort_unstable_by_key(|message| message.id.get());
        let mut progressed = false;
        for message in &messages {
            if message.id.get() <= after {
                continue;
            }
            record_historical_message(database, guild_id, target, channel_name, message).await?;
            after = message.id.get();
            recorded += 1;
            progressed = true;
        }
        if !progressed || messages.len() < 100 {
            break;
        }
    }
    Ok((recorded, Some(after)))
}

async fn record_historical_message(
    database: &Database,
    guild_id: GuildId,
    target: HistoryTarget,
    channel_name: Option<&str>,
    message: &Message,
) -> Result<()> {
    anyhow::ensure!(
        message.channel_id == target.channel_id
            && message.guild_id.is_none_or(|guild| guild == guild_id),
        "historical message belongs to another channel or guild"
    );
    anyhow::ensure!(
        database
            .channel_mode(guild_id.get(), target.settings_channel_id.get())
            .await?
            .stores_messages(),
        "history target no longer allows AI access"
    );
    let referenced_message_id = message
        .message_reference
        .as_ref()
        .and_then(|reference| reference.message_id.map(|id| id.get()))
        .or_else(|| {
            message
                .referenced_message
                .as_ref()
                .map(|message| message.id.get())
        });
    let attachments = extract_attachments_with_binary(&message.attachments, false).await;
    database
        .record_message(&IncomingMessage {
            discord_message_id: message.id.get(),
            guild_id: guild_id.get(),
            channel_id: target.channel_id.get(),
            channel_name: channel_name.map(str::to_owned),
            settings_channel_id: target.settings_channel_id.get(),
            author_id: message.author.id.get(),
            author_name: message.author.name.clone(),
            content: message.content.clone(),
            is_bot: message.author.bot,
            created_at: message.timestamp.to_string(),
            referenced_message_id,
            referenced_message: None,
            attachments,
        })
        .await
}

fn is_history_channel(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::Text
            | ChannelType::News
            | ChannelType::PublicThread
            | ChannelType::PrivateThread
            | ChannelType::NewsThread
    )
}

fn is_history_thread_parent(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::Text | ChannelType::News | ChannelType::Forum
    )
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

fn extract_discord_message_urls(content: &str) -> Vec<(u64, u64, u64)> {
    let mut results = Vec::new();
    let re = match regex::Regex::new(
        r"https://(?:ptb\.|canary\.)?discord\.com/channels/(\d+)/(\d+)/(\d+)",
    ) {
        Ok(re) => re,
        Err(_) => return results,
    };
    for captures in re.captures_iter(content) {
        if let (Some(guild_mat), Some(channel_mat), Some(message_mat)) =
            (captures.get(1), captures.get(2), captures.get(3))
            && let (Ok(guild_id), Ok(channel_id), Ok(message_id)) = (
                guild_mat.as_str().parse::<u64>(),
                channel_mat.as_str().parse::<u64>(),
                message_mat.as_str().parse::<u64>(),
            )
            && !results.contains(&(guild_id, channel_id, message_id))
        {
            results.push((guild_id, channel_id, message_id));
        }
    }
    results.truncate(5);
    results
}

fn unavailable_referenced_message(
    channel_id: u64,
    message_id: u64,
) -> crate::model::ReferencedMessage {
    crate::model::ReferencedMessage {
        message_id,
        channel_id,
        author_id: 0,
        author_name: "Unknown".to_owned(),
        content: "[メッセージを取得できないか、AIアクセスが許可されていません]".to_owned(),
        is_bot: false,
        created_at: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_http(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (
        Http,
        std::thread::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, response) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "mock request timed out"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(index) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut buffer = [0; 4096];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let body = if length == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
                };
                requests.push((path, body));
                let body = response.to_string();
                write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        let http = serenity::http::HttpBuilder::new("dummy-test-token")
            .client(
                reqwest::Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
            )
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build();
        (http, server)
    }

    fn test_message() -> Message {
        let mut message = Message::default();
        message.id = MessageId::new(10);
        message.channel_id = ChannelId::new(2);
        message.author.id = serenity::all::UserId::new(3);
        message.content = "history message".into();
        message
    }

    #[tokio::test]
    async fn history_sync_does_not_skip_a_gap_when_newer_messages_are_already_cached() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        database
            .set_channel_mode(1, 2, crate::model::ChannelMode::ReadOnly, 3)
            .await
            .unwrap();
        let mut cached = test_message();
        cached.id = MessageId::new(300);
        let target = HistoryTarget {
            channel_id: ChannelId::new(2),
            settings_channel_id: ChannelId::new(2),
        };
        record_historical_message(&database, GuildId::new(1), target, None, &cached)
            .await
            .unwrap();
        database
            .mark_history_sync_completed(1, 2, 2, Some(100))
            .await
            .unwrap();
        let mut missing = test_message();
        missing.id = MessageId::new(200);
        let (http, server) = mock_http(vec![
            (
                200,
                serde_json::json!({"id":"2","guild_id":"1","type":0,"name":"history"}),
            ),
            (200, serde_json::to_value(vec![cached, missing]).unwrap()),
        ]);
        assert_eq!(
            sync_channel_history(&http, &database, GuildId::new(1), target)
                .await
                .unwrap(),
            2
        );
        assert!(
            database
                .message_context_by_id(1, 200)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(database.history_sync_cursor(1, 2).await.unwrap(), Some(300));
        let requests = server.join().unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{}", requests[1].0)).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "after").unwrap().1,
            "100"
        );
    }

    #[tokio::test]
    async fn attachment_body_limits_check_actual_download_size() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0; 4096];
            stream.read(&mut buffer).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n0\r\n\r\n").unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        assert!(
            read_attachment_response(response, 5)
                .await
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn system_message_reply_rejection_retries_without_reference() {
        let source = test_message();
        let (http, server) = mock_http(vec![
            (
                400,
                serde_json::json!({"code":50035,"message":"Invalid Form Body","errors":{"message_reference":{"_errors":[{"code":"REPLIES_CANNOT_REPLY_TO_SYSTEM_MESSAGE","message":"Cannot reply to a system message"}]}}}),
            ),
            (200, serde_json::to_value(&source).unwrap()),
        ]);
        let (_, reply) = send_answer_chunk(&http, &source, "answer", 0, &[])
            .await
            .unwrap();
        assert!(!reply);
        let requests = server.join().unwrap();
        assert_eq!(requests[0].1["message_reference"]["message_id"], "10");
        assert!(requests[1].1.get("message_reference").is_none());
        assert_eq!(requests[1].1["content"], "answer");
    }

    #[tokio::test]
    async fn known_system_messages_are_sent_without_a_reply_and_bad_requests_are_not_retried() {
        let mut source = test_message();
        source.kind = MessageType::ThreadStarterMessage;
        let (http, server) = mock_http(vec![(200, serde_json::to_value(&source).unwrap())]);
        assert!(
            !send_answer_chunk(&http, &source, "answer", 0, &[])
                .await
                .unwrap()
                .1
        );
        assert!(
            server.join().unwrap()[0]
                .1
                .get("message_reference")
                .is_none()
        );
        let (http, server) = mock_http(vec![(
            403,
            serde_json::json!({"code":50013,"message":"Missing Permissions"}),
        )]);
        assert!(
            send_answer_chunk(&http, &source, "answer", 0, &[])
                .await
                .is_err()
        );
        assert_eq!(server.join().unwrap().len(), 1);
        for kind in [
            MessageType::Regular,
            MessageType::InlineReply,
            MessageType::ChatInputCommand,
            MessageType::ContextMenuCommand,
        ] {
            assert!(can_reply_to(kind));
        }
        for kind in [
            MessageType::PinsAdd,
            MessageType::MemberJoin,
            MessageType::ThreadCreated,
            MessageType::Unknown(255),
        ] {
            assert!(!can_reply_to(kind));
        }
    }

    #[tokio::test]
    async fn public_archived_threads_use_archive_time_and_not_creation_id_for_paging() {
        let thread = |id: u64, timestamp: &str| {
            serde_json::json!({
                "id": id.to_string(), "guild_id":"1", "parent_id":"2", "type":11, "name":"archived",
                "thread_metadata":{"archived":true,"auto_archive_duration":60,"archive_timestamp":timestamp,"locked":false}
            })
        };
        let (http, server) = mock_http(vec![
            (
                200,
                serde_json::json!({"threads":[thread(300, "2026-10-01T00:00:00Z"), thread(100, "2026-10-02T00:00:00Z")],"members":[],"has_more":true}),
            ),
            (
                200,
                serde_json::json!({"threads":[thread(200, "2026-09-01T00:00:00Z")],"members":[],"has_more":false}),
            ),
        ]);
        let mut targets = HashMap::new();
        add_archived_thread_pages(&http, ChannelId::new(2), &mut targets, false)
            .await
            .unwrap();
        assert_eq!(targets.len(), 3);
        let requests = server.join().unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{}", requests[1].0)).unwrap();
        let before = url
            .query_pairs()
            .find(|(key, _)| key == "before")
            .unwrap()
            .1
            .into_owned();
        assert!(before.starts_with("2026-10-01"));
    }

    #[tokio::test]
    async fn live_history_checks_guild_and_mode_and_accepts_rest_messages_without_guild_id() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        database
            .set_channel_mode(1, 2, crate::model::ChannelMode::ReadOnly, 3)
            .await
            .unwrap();
        let channel = serde_json::json!({"id":"2","guild_id":"1","type":0,"name":"history"});
        let mut message = test_message();
        message.guild_id = None;
        let (http, server) = mock_http(vec![
            (200, channel.clone()),
            (200, serde_json::to_value(&message).unwrap()),
        ]);
        let reader = DiscordHistoryReader {
            database: &database,
            http: &http,
            guild_id: GuildId::new(1),
        };
        assert_eq!(
            reader.message(2, 10).await.unwrap().content,
            "history message"
        );
        assert_eq!(server.join().unwrap().len(), 2);
        database
            .set_channel_mode(1, 2, crate::model::ChannelMode::Off, 3)
            .await
            .unwrap();
        let (http, server) = mock_http(vec![(200, channel)]);
        let reader = DiscordHistoryReader {
            database: &database,
            http: &http,
            guild_id: GuildId::new(1),
        };
        assert!(reader.message(2, 10).await.is_err());
        assert_eq!(server.join().unwrap().len(), 1);
        let (http, server) = mock_http(vec![(
            200,
            serde_json::json!({"id":"2","guild_id":"9","type":0,"name":"foreign"}),
        )]);
        let reader = DiscordHistoryReader {
            database: &database,
            http: &http,
            guild_id: GuildId::new(1),
        };
        assert!(reader.message(2, 10).await.is_err());
        assert_eq!(server.join().unwrap().len(), 1);
    }

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
    fn answers_only_notify_the_replied_user() {
        let allowed = serde_json::to_value(answer_allowed_mentions()).unwrap();
        assert_eq!(allowed["parse"], serde_json::json!([]));
        assert_eq!(allowed["users"], serde_json::json!([]));
        assert_eq!(allowed["roles"], serde_json::json!([]));
        assert_eq!(allowed["replied_user"], true);
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
        let content = "これを見て: https://discord.com/channels/12345/67890/999888777 あとこれはDMなので対象外 https://ptb.discord.com/channels/@me/111/222";
        let extracted = extract_discord_message_urls(content);
        assert_eq!(extracted, vec![(12345, 67890, 999888777)]);
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

        state.generation_restarts = MAX_GENERATION_RESTARTS;
        assert!(!state.may_restart());
    }

    #[test]
    fn conversation_batch_retains_only_the_latest_messages() {
        let mut messages = vec![1, 2, 3, 4];
        assert_eq!(retain_latest_prioritizing(&mut messages, 3, |_| false), 1);
        assert_eq!(messages, vec![2, 3, 4]);
    }

    #[test]
    fn conversation_batch_retains_priority_messages_when_bounded() {
        let mut messages = vec![(1, true), (2, false), (3, false), (4, false)];
        assert_eq!(
            retain_latest_prioritizing(&mut messages, 3, |(_, priority)| *priority),
            1
        );
        assert_eq!(messages, vec![(1, true), (3, false), (4, false)]);
    }

    #[test]
    fn conversation_batch_hard_deadline_does_not_slide_with_new_messages() {
        let first = Instant::now();
        let latest = first + CONVERSATION_BATCH_MAX_WAIT;
        assert_eq!(
            conversation_batch_ready_at(first, latest),
            first + CONVERSATION_BATCH_MAX_WAIT
        );
    }
}
