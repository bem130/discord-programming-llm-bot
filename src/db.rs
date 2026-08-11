use std::{str::FromStr, time::Duration};

use anyhow::{Context, Result};
use regex::RegexBuilder;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow},
};

use crate::model::{
    ChannelMode, ChannelSetting, ContextAttachment, ContextMessage, ConversationSearchResult,
    GuildMemory, IncomingMessage, MemoryKind, MemoryStatus, RecentChannelActivity,
};

const MAX_CONTEXT_MESSAGES: u32 = 80;
const MAX_CONTEXT_PAGE_MESSAGES: u32 = MAX_CONTEXT_MESSAGES + 1;
const MAX_RECENT_ACTIVE_CHANNELS: u32 = 20;
const MAX_MEMORY_INDEX_ENTRIES: u32 = 100;
const MAX_SEARCH_PATTERN_CHARS: usize = 256;
const SEARCH_PAGE_SIZE: i64 = 500;
const MAX_SEARCH_RESULTS: u32 = 20;
const MAX_SEARCH_EXCERPT_CHARS: usize = 800;

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let is_in_memory = database_url.contains(":memory:");
        let options = SqliteConnectOptions::from_str(database_url)
            .with_context(|| format!("invalid SQLite URL: {database_url}"))?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .journal_mode(if is_in_memory {
                SqliteJournalMode::Memory
            } else {
                SqliteJournalMode::Wal
            });
        let pool = SqlitePoolOptions::new()
            .max_connections(if is_in_memory { 1 } else { 5 })
            .connect_with(options)
            .await
            .context("failed to connect to SQLite")?;

        sqlx::migrate!("./migration")
            .run(&pool)
            .await
            .context("failed to run SQLite migrations")?;

        Ok(Self { pool })
    }

    pub async fn channel_mode(&self, guild_id: u64, channel_id: u64) -> Result<ChannelMode> {
        let mode = sqlx::query_scalar::<_, String>(
            "SELECT mode FROM channel_settings WHERE guild_id = ? AND channel_id = ?",
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .context("failed to read channel setting")?;

        mode.map(|value| value.parse().context("invalid channel mode in database"))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub async fn set_channel_mode(
        &self,
        guild_id: u64,
        channel_id: u64,
        mode: ChannelMode,
        updated_by: u64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO channel_settings (guild_id, channel_id, mode, updated_by)
            VALUES (?, ?, ?, ?)
            ON CONFLICT (guild_id, channel_id) DO UPDATE SET
                mode = excluded.mode,
                updated_by = excluded.updated_by,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(mode.as_str())
        .bind(updated_by.to_string())
        .execute(&self.pool)
        .await
        .context("failed to update channel setting")?;
        Ok(())
    }

    pub async fn channel_settings(&self, guild_id: u64) -> Result<Vec<ChannelSetting>> {
        let rows = sqlx::query(
            r#"
            SELECT guild_id, channel_id, mode, updated_by, updated_at
            FROM channel_settings
            WHERE guild_id = ?
            ORDER BY channel_id
            "#,
        )
        .bind(guild_id.to_string())
        .fetch_all(&self.pool)
        .await
        .context("failed to list channel settings")?;

        rows.into_iter()
            .map(|row| {
                Ok(ChannelSetting {
                    guild_id: parse_id(row.try_get("guild_id")?)?,
                    channel_id: parse_id(row.try_get("channel_id")?)?,
                    mode: row
                        .try_get::<String, _>("mode")?
                        .parse()
                        .context("invalid channel mode in database")?,
                    updated_by: parse_id(row.try_get("updated_by")?)?,
                    updated_at: row.try_get("updated_at")?,
                })
            })
            .collect()
    }

    pub async fn invocation_role_id(&self, guild_id: u64) -> Result<Option<u64>> {
        sqlx::query_scalar::<_, String>(
            "SELECT invocation_role_id FROM guild_settings WHERE guild_id = ?",
        )
        .bind(guild_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .context("failed to read invocation role setting")?
        .map(parse_id)
        .transpose()
    }

    pub async fn set_invocation_role(
        &self,
        guild_id: u64,
        role_id: u64,
        updated_by: u64,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO guild_settings (guild_id, invocation_role_id, updated_by)
            VALUES (?, ?, ?)
            ON CONFLICT (guild_id) DO UPDATE SET
                invocation_role_id = excluded.invocation_role_id,
                updated_by = excluded.updated_by,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(guild_id.to_string())
        .bind(role_id.to_string())
        .bind(updated_by.to_string())
        .execute(&self.pool)
        .await
        .context("failed to update invocation role setting")?;
        Ok(())
    }

    pub async fn clear_invocation_role(&self, guild_id: u64) -> Result<()> {
        sqlx::query("DELETE FROM guild_settings WHERE guild_id = ?")
            .bind(guild_id.to_string())
            .execute(&self.pool)
            .await
            .context("failed to clear invocation role setting")?;
        Ok(())
    }

    pub async fn record_message(&self, message: &IncomingMessage) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            r#"
            INSERT INTO guild_channels (guild_id, channel_id, channel_name)
            VALUES (?, ?, ?)
            ON CONFLICT (guild_id, channel_id) DO UPDATE SET
                channel_name = COALESCE(excluded.channel_name, guild_channels.channel_name),
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(message.guild_id.to_string())
        .bind(message.channel_id.to_string())
        .bind(&message.channel_name)
        .execute(&mut *transaction)
        .await
        .context("failed to update channel catalog")?;
        let result = sqlx::query(
            r#"
            INSERT INTO messages (
                discord_message_id, guild_id, channel_id, settings_channel_id,
                author_id, author_name, content, is_bot, created_at, referenced_message_id
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (discord_message_id) DO NOTHING
            "#,
        )
        .bind(message.discord_message_id.to_string())
        .bind(message.guild_id.to_string())
        .bind(message.channel_id.to_string())
        .bind(message.settings_channel_id.to_string())
        .bind(message.author_id.to_string())
        .bind(&message.author_name)
        .bind(&message.content)
        .bind(message.is_bot)
        .bind(&message.created_at)
        .bind(message.referenced_message_id.map(|id| id.to_string()))
        .execute(&mut *transaction)
        .await
        .context("failed to store message")?;

        if result.rows_affected() == 1 {
            let message_id = result.last_insert_rowid();
            for attachment in &message.attachments {
                sqlx::query(
                    r#"
                    INSERT INTO message_attachments (
                        message_id, filename, content_type, size_bytes, extracted_text, binary_data
                    ) VALUES (?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(message_id)
                .bind(&attachment.filename)
                .bind(&attachment.content_type)
                .bind(i64::try_from(attachment.size_bytes).context("attachment is too large")?)
                .bind(&attachment.extracted_text)
                .bind(&attachment.binary_data)
                .execute(&mut *transaction)
                .await
                .context("failed to store attachment metadata")?;
            }
        }

        transaction.commit().await?;
        Ok(())
    }

    pub async fn update_message(
        &self,
        discord_message_id: u64,
        content: Option<&str>,
        attachments: Option<&[crate::model::IncomingAttachment]>,
        edited_at: Option<&str>,
    ) -> Result<bool> {
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query("SELECT id, content FROM messages WHERE discord_message_id = ?")
            .bind(discord_message_id.to_string())
            .fetch_optional(&mut *transaction)
            .await
            .context("failed to find edited message")?;
        let Some(row) = row else {
            return Ok(false);
        };
        let message_id: i64 = row.try_get("id")?;
        let previous_content: String = row.try_get("content")?;

        if let Some(content) = content
            && content != previous_content
        {
            sqlx::query(
                "INSERT INTO message_revisions (message_id, previous_content, revised_at) VALUES (?, ?, COALESCE(?, CURRENT_TIMESTAMP))",
            )
            .bind(message_id)
            .bind(&previous_content)
            .bind(edited_at)
            .execute(&mut *transaction)
            .await
            .context("failed to store message revision")?;
            sqlx::query("UPDATE messages SET content = ?, edited_at = ? WHERE id = ?")
                .bind(content)
                .bind(edited_at)
                .bind(message_id)
                .execute(&mut *transaction)
                .await
                .context("failed to update message content")?;
        }

        if let Some(attachments) = attachments {
            sqlx::query("DELETE FROM message_attachments WHERE message_id = ?")
                .bind(message_id)
                .execute(&mut *transaction)
                .await
                .context("failed to replace edited message attachments")?;
            for attachment in attachments {
                sqlx::query(
                    r#"
                    INSERT INTO message_attachments (
                        message_id, filename, content_type, size_bytes, extracted_text, binary_data
                    ) VALUES (?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(message_id)
                .bind(&attachment.filename)
                .bind(&attachment.content_type)
                .bind(i64::try_from(attachment.size_bytes).context("attachment is too large")?)
                .bind(&attachment.extracted_text)
                .bind(&attachment.binary_data)
                .execute(&mut *transaction)
                .await
                .context("failed to store edited attachment")?;
            }
        }

        transaction.commit().await?;
        Ok(true)
    }

    pub async fn delete_message(&self, discord_message_id: u64) -> Result<bool> {
        let result = sqlx::query("DELETE FROM messages WHERE discord_message_id = ?")
            .bind(discord_message_id.to_string())
            .execute(&self.pool)
            .await
            .context("failed to delete message")?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn delete_messages(&self, discord_message_ids: &[u64]) -> Result<u64> {
        let mut deleted = 0;
        let mut transaction = self.pool.begin().await?;
        for message_id in discord_message_ids {
            deleted += sqlx::query("DELETE FROM messages WHERE discord_message_id = ?")
                .bind(message_id.to_string())
                .execute(&mut *transaction)
                .await
                .context("failed to bulk-delete message")?
                .rows_affected();
        }
        transaction.commit().await?;
        Ok(deleted)
    }

    pub async fn get_message_by_discord_id(
        &self,
        discord_message_id: u64,
    ) -> Result<Option<crate::model::ReferencedMessage>> {
        let row = sqlx::query(
            r#"
            SELECT discord_message_id, channel_id, author_id, author_name, content, is_bot, created_at
            FROM messages
            WHERE discord_message_id = ?
            "#,
        )
        .bind(discord_message_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch message by discord id")?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(crate::model::ReferencedMessage {
            message_id: parse_id(row.try_get("discord_message_id")?)?,
            channel_id: parse_id(row.try_get("channel_id")?)?,
            author_id: parse_id(row.try_get("author_id")?)?,
            author_name: row.try_get("author_name")?,
            content: row.try_get("content")?,
            is_bot: row.try_get("is_bot")?,
            created_at: row.try_get("created_at")?,
        }))
    }

    pub async fn recent_channel_context(
        &self,
        guild_id: u64,
        channel_id: u64,
        limit: u32,
    ) -> Result<Vec<ContextMessage>> {
        let limit = limit.clamp(1, MAX_CONTEXT_MESSAGES);
        let rows = sqlx::query(
            r#"
            SELECT m.id, m.discord_message_id, m.channel_id, m.author_id, m.author_name,
                   m.content, m.is_bot, m.created_at, m.referenced_message_id,
                   ref.discord_message_id AS ref_discord_message_id,
                   ref.channel_id AS ref_channel_id,
                   ref.author_id AS ref_author_id,
                   ref.author_name AS ref_author_name,
                   ref.content AS ref_content,
                   ref.is_bot AS ref_is_bot,
                   ref.created_at AS ref_created_at
            FROM messages AS m
            INNER JOIN channel_settings AS c
                ON c.guild_id = m.guild_id
               AND c.channel_id = COALESCE(m.settings_channel_id, m.channel_id)
               AND c.mode != 'off'
            LEFT JOIN messages AS ref
                ON ref.discord_message_id = m.referenced_message_id
            WHERE m.guild_id = ? AND m.channel_id = ?
            ORDER BY m.id DESC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load current channel context")?;

        let mut messages = self.context_messages_from_rows(rows).await?;
        messages.reverse();
        Ok(messages)
    }

    async fn context_messages_from_rows(
        &self,
        rows: Vec<SqliteRow>,
    ) -> Result<Vec<ContextMessage>> {
        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            let message_id: i64 = row.try_get("id")?;
            let attachment_rows = sqlx::query(
                "SELECT filename, content_type, extracted_text, binary_data FROM message_attachments WHERE message_id = ? ORDER BY id",
            )
            .bind(message_id)
            .fetch_all(&self.pool)
            .await?;

            let reply_to = if let Some(ref_id_str) =
                row.try_get::<Option<String>, _>("ref_discord_message_id")?
            {
                Some(crate::model::ReferencedMessage {
                    message_id: parse_id(ref_id_str)?,
                    channel_id: parse_id(
                        row.try_get::<String, _>("ref_channel_id")
                            .unwrap_or_default(),
                    )
                    .unwrap_or(0),
                    author_id: parse_id(
                        row.try_get::<String, _>("ref_author_id")
                            .unwrap_or_default(),
                    )
                    .unwrap_or(0),
                    author_name: row.try_get("ref_author_name").unwrap_or_default(),
                    content: row.try_get("ref_content").unwrap_or_default(),
                    is_bot: row.try_get("ref_is_bot").unwrap_or(false),
                    created_at: row.try_get("ref_created_at").unwrap_or_default(),
                })
            } else {
                None
            };

            messages.push(ContextMessage {
                message_id: parse_id(row.try_get("discord_message_id")?)?,
                channel_id: parse_id(row.try_get("channel_id")?)?,
                author_id: parse_id(row.try_get("author_id")?)?,
                author_name: row.try_get("author_name")?,
                content: row.try_get("content")?,
                is_bot: row.try_get("is_bot")?,
                created_at: row.try_get("created_at")?,
                reply_to,
                linked_messages: vec![],
                attachments: attachment_rows
                    .into_iter()
                    .map(|attachment| ContextAttachment {
                        filename: attachment.get("filename"),
                        content_type: attachment.get("content_type"),
                        extracted_text: attachment.get("extracted_text"),
                        binary_data: attachment.get("binary_data"),
                    })
                    .collect(),
            });
        }

        Ok(messages)
    }

    pub async fn messages_newer_than_message(
        &self,
        guild_id: u64,
        channel_id: u64,
        after_discord_message_id: u64,
        limit: u32,
    ) -> Result<Vec<ContextMessage>> {
        let limit = limit.clamp(1, MAX_CONTEXT_PAGE_MESSAGES);
        let rows = sqlx::query(
            r#"
            SELECT m.id, m.discord_message_id, m.channel_id, m.author_id, m.author_name,
                   m.content, m.is_bot, m.created_at, m.referenced_message_id,
                   ref.discord_message_id AS ref_discord_message_id,
                   ref.channel_id AS ref_channel_id,
                   ref.author_id AS ref_author_id,
                   ref.author_name AS ref_author_name,
                   ref.content AS ref_content,
                   ref.is_bot AS ref_is_bot,
                   ref.created_at AS ref_created_at
            FROM messages AS m
            INNER JOIN channel_settings AS c
                ON c.guild_id = m.guild_id
               AND c.channel_id = COALESCE(m.settings_channel_id, m.channel_id)
               AND c.mode != 'off'
            LEFT JOIN messages AS ref
                ON ref.discord_message_id = m.referenced_message_id
            WHERE m.guild_id = ? AND m.channel_id = ?
              AND CAST(m.discord_message_id AS INTEGER) > ?
            ORDER BY CAST(m.discord_message_id AS INTEGER) ASC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(after_discord_message_id.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load messages newer than anchor message")?;

        self.context_messages_from_rows(rows).await
    }

    pub async fn messages_newer_than_datetime(
        &self,
        guild_id: u64,
        channel_id: u64,
        after_datetime: &str,
        after_message_id: Option<u64>,
        limit: u32,
    ) -> Result<Vec<ContextMessage>> {
        let limit = limit.clamp(1, MAX_CONTEXT_PAGE_MESSAGES);
        let after_message_id = after_message_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let rows = sqlx::query(
            r#"
            SELECT m.id, m.discord_message_id, m.channel_id, m.author_id, m.author_name,
                   m.content, m.is_bot, m.created_at, m.referenced_message_id,
                   ref.discord_message_id AS ref_discord_message_id,
                   ref.channel_id AS ref_channel_id,
                   ref.author_id AS ref_author_id,
                   ref.author_name AS ref_author_name,
                   ref.content AS ref_content,
                   ref.is_bot AS ref_is_bot,
                   ref.created_at AS ref_created_at
            FROM messages AS m
            INNER JOIN channel_settings AS c
                ON c.guild_id = m.guild_id
               AND c.channel_id = COALESCE(m.settings_channel_id, m.channel_id)
               AND c.mode != 'off'
            LEFT JOIN messages AS ref
                ON ref.discord_message_id = m.referenced_message_id
            WHERE m.guild_id = ? AND m.channel_id = ?
              AND (
                  strftime('%Y-%m-%dT%H:%M:%fZ', m.created_at) >
                      strftime('%Y-%m-%dT%H:%M:%fZ', ?)
                  OR (
                      strftime('%Y-%m-%dT%H:%M:%fZ', m.created_at) =
                          strftime('%Y-%m-%dT%H:%M:%fZ', ?)
                      AND ? <> ''
                      AND CAST(m.discord_message_id AS INTEGER) > CAST(? AS INTEGER)
                  )
              )
            ORDER BY strftime('%Y-%m-%dT%H:%M:%fZ', m.created_at) ASC,
                     CAST(m.discord_message_id AS INTEGER) ASC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(after_datetime)
        .bind(after_datetime)
        .bind(&after_message_id)
        .bind(&after_message_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load messages newer than datetime")?;

        self.context_messages_from_rows(rows).await
    }

    pub async fn recent_active_channels(
        &self,
        guild_id: u64,
        current_channel_id: u64,
        limit: u32,
    ) -> Result<Vec<RecentChannelActivity>> {
        let limit = limit.clamp(1, MAX_RECENT_ACTIVE_CHANNELS);
        let rows = sqlx::query(
            r#"
            SELECT m.channel_id, gc.channel_name, m.created_at AS last_message_at,
                   m.id AS last_message_row_id
            FROM messages AS m
            INNER JOIN channel_settings AS c
                ON c.guild_id = m.guild_id
               AND c.channel_id = COALESCE(m.settings_channel_id, m.channel_id)
               AND c.mode != 'off'
            LEFT JOIN guild_channels AS gc
                ON gc.guild_id = m.guild_id AND gc.channel_id = m.channel_id
            WHERE m.guild_id = ?
              AND m.id = (
                  SELECT MAX(latest.id)
                  FROM messages AS latest
                  WHERE latest.guild_id = m.guild_id
                    AND latest.channel_id = m.channel_id
              )
            ORDER BY last_message_row_id DESC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load recent active channels")?;

        rows.into_iter()
            .map(|row| {
                let channel_id = parse_id(row.try_get("channel_id")?)?;
                Ok(RecentChannelActivity {
                    channel_id,
                    channel_name: row.try_get("channel_name")?,
                    is_current_channel: channel_id == current_channel_id,
                    last_message_at: row.try_get("last_message_at")?,
                })
            })
            .collect()
    }

    /// Finds a channel ID by name within a guild (case-insensitive, returns first match).
    pub async fn find_channel_id_by_name(
        &self,
        guild_id: u64,
        channel_name: &str,
    ) -> Result<Option<u64>> {
        let row = sqlx::query_scalar::<_, String>(
            r#"
            SELECT channel_id FROM guild_channels
            WHERE guild_id = ? AND LOWER(channel_name) = LOWER(?)
            LIMIT 1
            "#,
        )
        .bind(guild_id.to_string())
        .bind(channel_name)
        .fetch_optional(&self.pool)
        .await
        .context("failed to find channel by name")?;
        row.map(parse_id).transpose()
    }

    pub async fn channel_exists(&self, guild_id: u64, channel_id: u64) -> Result<bool> {
        let row = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM guild_channels
                WHERE guild_id = ? AND channel_id = ?
                UNION
                SELECT 1 FROM messages
                WHERE guild_id = ? AND channel_id = ?
            )
            "#,
        )
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .bind(guild_id.to_string())
        .bind(channel_id.to_string())
        .fetch_one(&self.pool)
        .await
        .context("failed to check whether channel exists")?;
        Ok(row != 0)
    }

    /// Looks up a channel whose id starts with `channel_id`. This recovers
    /// model-generated snowflakes that lost their trailing digits. Returns
    /// `None` when nothing matches or when several channels share the prefix.
    pub async fn find_channel_id_by_prefix(
        &self,
        guild_id: u64,
        channel_id: u64,
    ) -> Result<Option<u64>> {
        let prefix = format!("{channel_id}%");
        let rows = sqlx::query_scalar::<_, String>(
            r#"
            SELECT channel_id FROM (
                SELECT channel_id FROM guild_channels
                WHERE guild_id = ? AND channel_id LIKE ?
                UNION
                SELECT channel_id FROM messages
                WHERE guild_id = ? AND channel_id LIKE ?
            )
            GROUP BY channel_id
            LIMIT 2
            "#,
        )
        .bind(guild_id.to_string())
        .bind(&prefix)
        .bind(guild_id.to_string())
        .bind(&prefix)
        .fetch_all(&self.pool)
        .await
        .context("failed to find channel by id prefix")?;

        if rows.len() == 1 {
            rows.into_iter().next().map(parse_id).transpose()
        } else {
            Ok(None)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_memory(
        &self,
        guild_id: u64,
        kind: MemoryKind,
        title: &str,
        summary: &str,
        content: &str,
        status: MemoryStatus,
        created_by: u64,
        source_channel_id: Option<u64>,
        source_message_id: Option<u64>,
    ) -> Result<i64> {
        let active = status == MemoryStatus::Active;
        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO guild_memories (
                guild_id, kind, title, summary, content, status, created_by,
                source_channel_id, source_message_id, reviewed_at, review_after
            ) VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?,
                CASE WHEN ? THEN CURRENT_TIMESTAMP END,
                CASE WHEN ? THEN datetime('now', '+30 days') END
            )
            RETURNING id
            "#,
        )
        .bind(guild_id.to_string())
        .bind(kind.as_str())
        .bind(title)
        .bind(summary)
        .bind(content)
        .bind(status.as_str())
        .bind(created_by.to_string())
        .bind(source_channel_id.map(|id| id.to_string()))
        .bind(source_message_id.map(|id| id.to_string()))
        .bind(active)
        .bind(active)
        .fetch_one(&self.pool)
        .await
        .context("failed to create guild memory")?;
        Ok(id)
    }

    pub async fn list_memories(&self, guild_id: u64) -> Result<Vec<GuildMemory>> {
        let rows = sqlx::query(
            r#"
            SELECT id, guild_id, kind, title, summary, content, status, created_by,
                   source_channel_id, source_message_id, created_at, updated_at,
                   reviewed_at, review_after,
                   CASE
                       WHEN status = 'active' AND
                            (review_after IS NULL OR review_after <= CURRENT_TIMESTAMP)
                       THEN 1 ELSE 0
                   END AS stale
            FROM guild_memories
            WHERE guild_id = ? AND status != 'deleted'
            ORDER BY CASE status WHEN 'candidate' THEN 0 ELSE 1 END, updated_at DESC, id DESC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(MAX_MEMORY_INDEX_ENTRIES)
        .fetch_all(&self.pool)
        .await
        .context("failed to list guild memories")?;

        rows.into_iter().map(memory_from_row).collect()
    }

    pub async fn active_memories(&self, guild_id: u64) -> Result<Vec<GuildMemory>> {
        let rows = sqlx::query(
            r#"
            SELECT id, guild_id, kind, title, summary, content, status, created_by,
                   source_channel_id, source_message_id, created_at, updated_at,
                   reviewed_at, review_after, 0 AS stale
            FROM guild_memories
            WHERE guild_id = ? AND status = 'active'
              AND review_after > CURRENT_TIMESTAMP
            ORDER BY updated_at DESC, id DESC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(MAX_MEMORY_INDEX_ENTRIES)
        .fetch_all(&self.pool)
        .await
        .context("failed to load active guild memories")?;

        rows.into_iter().map(memory_from_row).collect()
    }

    pub async fn review_memory(&self, guild_id: u64, memory_id: i64) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE guild_memories
            SET reviewed_at = CURRENT_TIMESTAMP,
                review_after = datetime('now', '+30 days'), updated_at = CURRENT_TIMESTAMP
            WHERE guild_id = ? AND id = ? AND status = 'active'
            "#,
        )
        .bind(guild_id.to_string())
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .context("failed to review guild memory")?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn remove_memory(&self, guild_id: u64, memory_id: i64) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE guild_memories
            SET status = 'deleted', updated_at = CURRENT_TIMESTAMP
            WHERE guild_id = ? AND id = ? AND status != 'deleted'
            "#,
        )
        .bind(guild_id.to_string())
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .context("failed to remove guild memory")?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn memories_by_ids(
        &self,
        guild_id: u64,
        memory_ids: &[i64],
    ) -> Result<Vec<GuildMemory>> {
        let active = self.active_memories(guild_id).await?;
        Ok(active
            .into_iter()
            .filter(|memory| memory_ids.contains(&memory.id))
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn search_messages(
        &self,
        guild_id: u64,
        current_channel_id: u64,
        pattern: &str,
        regex: bool,
        channel_id: Option<u64>,
        author_id: Option<u64>,
        include_bot_messages: bool,
        limit: u32,
    ) -> Result<Vec<ConversationSearchResult>> {
        anyhow::ensure!(!pattern.is_empty(), "search pattern must not be empty");
        anyhow::ensure!(
            pattern.chars().count() <= MAX_SEARCH_PATTERN_CHARS,
            "search pattern is too long"
        );
        let expression = if regex {
            pattern.to_owned()
        } else {
            regex::escape(pattern)
        };
        let matcher = RegexBuilder::new(&expression)
            .case_insensitive(true)
            .size_limit(1024 * 1024)
            .build()
            .context("invalid or too complex regular expression")?;
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS) as usize;
        let mut results = Vec::with_capacity(limit);
        let mut before_id = i64::MAX;

        while results.len() < limit {
            let rows = sqlx::query(
                r#"
                 SELECT m.id, m.discord_message_id, m.channel_id, gc.channel_name, m.author_id,
                        m.author_name, m.content, m.is_bot, m.created_at,
                       COALESCE(GROUP_CONCAT(COALESCE(a.extracted_text, ''), char(10)), '')
                           AS attachment_text
                FROM messages AS m
                INNER JOIN channel_settings AS c
                    ON c.guild_id = m.guild_id
                   AND c.channel_id = COALESCE(m.settings_channel_id, m.channel_id)
                   AND c.mode != 'off'
                 LEFT JOIN message_attachments AS a ON a.message_id = m.id
                 LEFT JOIN guild_channels AS gc
                    ON gc.guild_id = m.guild_id AND gc.channel_id = m.channel_id
                WHERE m.guild_id = ? AND m.id < ?
                  AND (? IS NULL OR m.channel_id = ?)
                  AND (? IS NULL OR m.author_id = ?)
                  AND (? OR m.is_bot = 0)
                GROUP BY m.id
                ORDER BY m.id DESC
                LIMIT ?
                "#,
            )
            .bind(guild_id.to_string())
            .bind(before_id)
            .bind(channel_id.map(|id| id.to_string()))
            .bind(channel_id.map(|id| id.to_string()))
            .bind(author_id.map(|id| id.to_string()))
            .bind(author_id.map(|id| id.to_string()))
            .bind(include_bot_messages)
            .bind(SEARCH_PAGE_SIZE)
            .fetch_all(&self.pool)
            .await
            .context("failed to search conversation history")?;
            if rows.is_empty() {
                break;
            }

            for row in &rows {
                before_id = row.try_get("id")?;
                let message_id = parse_id(row.try_get("discord_message_id")?)?;
                let channel_id = parse_id(row.try_get("channel_id")?)?;
                let content: String = row.try_get("content")?;
                let attachment_text: String = row.try_get("attachment_text")?;
                if !matcher.is_match(&content) && !matcher.is_match(&attachment_text) {
                    continue;
                }
                results.push(ConversationSearchResult {
                    message_id,
                    channel_id,
                    channel_name: row.try_get("channel_name")?,
                    is_current_channel: channel_id == current_channel_id,
                    message_url: discord_message_url(guild_id, channel_id, message_id),
                    author_id: parse_id(row.try_get("author_id")?)?,
                    author_name: row.try_get("author_name")?,
                    content: matching_excerpt(&content, &matcher),
                    attachment_text: matching_excerpt(&attachment_text, &matcher),
                    created_at: row.try_get("created_at")?,
                });
                if results.len() == limit {
                    break;
                }
            }
            if rows.len() < SEARCH_PAGE_SIZE as usize {
                break;
            }
        }

        Ok(results)
    }
}

fn discord_message_url(guild_id: u64, channel_id: u64, message_id: u64) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}")
}

fn matching_excerpt(text: &str, matcher: &regex::Regex) -> String {
    let Some(found) = matcher.find(text) else {
        return String::new();
    };
    let match_start = text[..found.start()].chars().count();
    let start = match_start.saturating_sub(MAX_SEARCH_EXCERPT_CHARS / 3);
    let mut excerpt: String = text
        .chars()
        .skip(start)
        .take(MAX_SEARCH_EXCERPT_CHARS)
        .collect();
    if start > 0 {
        excerpt.insert(0, '…');
    }
    if text.chars().count() > start + MAX_SEARCH_EXCERPT_CHARS {
        excerpt.push('…');
    }
    excerpt
}

fn memory_from_row(row: sqlx::sqlite::SqliteRow) -> Result<GuildMemory> {
    Ok(GuildMemory {
        id: row.try_get("id")?,
        guild_id: parse_id(row.try_get("guild_id")?)?,
        kind: row
            .try_get::<String, _>("kind")?
            .parse()
            .context("invalid memory kind in database")?,
        title: row.try_get("title")?,
        summary: row.try_get("summary")?,
        content: row.try_get("content")?,
        status: row
            .try_get::<String, _>("status")?
            .parse()
            .context("invalid memory status in database")?,
        created_by: parse_id(row.try_get("created_by")?)?,
        source_channel_id: row
            .try_get::<Option<String>, _>("source_channel_id")?
            .map(parse_id)
            .transpose()?,
        source_message_id: row
            .try_get::<Option<String>, _>("source_message_id")?
            .map(parse_id)
            .transpose()?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        reviewed_at: row.try_get("reviewed_at")?,
        review_after: row.try_get("review_after")?,
        stale: row.try_get::<bool, _>("stale")?,
    })
}

fn parse_id(value: String) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid Discord ID in database: {value}"))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::model::IncomingAttachment;

    async fn test_database() -> Database {
        Database::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn channel_mode_defaults_to_off_and_can_be_updated() {
        let database = test_database().await;

        assert_eq!(database.channel_mode(1, 2).await.unwrap(), ChannelMode::Off);
        database
            .set_channel_mode(1, 2, ChannelMode::Auto, 3)
            .await
            .unwrap();

        assert_eq!(
            database.channel_mode(1, 2).await.unwrap(),
            ChannelMode::Auto
        );
        assert_eq!(database.channel_settings(1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invocation_role_can_be_set_replaced_and_cleared() {
        let database = test_database().await;
        assert_eq!(database.invocation_role_id(1).await.unwrap(), None);

        database.set_invocation_role(1, 10, 99).await.unwrap();
        assert_eq!(database.invocation_role_id(1).await.unwrap(), Some(10));

        database.set_invocation_role(1, 20, 99).await.unwrap();
        assert_eq!(database.invocation_role_id(1).await.unwrap(), Some(20));

        database.clear_invocation_role(1).await.unwrap();
        assert_eq!(database.invocation_role_id(1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn newer_than_queries_return_chronological_order() {
        let database = test_database().await;
        database
            .set_channel_mode(1, 100, ChannelMode::ReadOnly, 99)
            .await
            .unwrap();
        for (message_id, created_at) in [
            (1001, "2026-08-09T00:00:00Z"),
            (1002, "2026-08-09T00:10:00Z"),
            (1003, "2026-08-09T00:20:00Z"),
            (1004, "2026-08-09T00:30:00Z"),
        ] {
            database
                .record_message(&IncomingMessage {
                    discord_message_id: message_id,
                    guild_id: 1,
                    channel_id: 100,
                    channel_name: Some("questions".into()),
                    settings_channel_id: 100,
                    author_id: 99,
                    author_name: "learner".into(),
                    content: format!("msg {message_id}"),
                    is_bot: false,
                    created_at: created_at.into(),
                    referenced_message_id: None,
                    referenced_message: None,
                    attachments: vec![],
                })
                .await
                .unwrap();
        }

        let by_datetime = database
            .messages_newer_than_datetime(1, 100, "2026-08-09T00:10:00Z", None, 10)
            .await
            .unwrap();
        assert_eq!(
            by_datetime.iter().map(|m| m.message_id).collect::<Vec<_>>(),
            vec![1003, 1004]
        );

        let by_message = database
            .messages_newer_than_message(1, 100, 1001, 10)
            .await
            .unwrap();
        assert_eq!(
            by_message.iter().map(|m| m.message_id).collect::<Vec<_>>(),
            vec![1002, 1003, 1004]
        );

        let limited = database
            .messages_newer_than_datetime(1, 100, "2026-08-09T00:00:00Z", None, 2)
            .await
            .unwrap();
        assert_eq!(
            limited.iter().map(|m| m.message_id).collect::<Vec<_>>(),
            vec![1002, 1003]
        );
    }

    #[tokio::test]
    async fn current_channel_context_is_isolated_and_chronological() {
        let database = test_database().await;
        database
            .set_channel_mode(1, 100, ChannelMode::ReadOnly, 99)
            .await
            .unwrap();
        database
            .set_channel_mode(1, 200, ChannelMode::Mention, 99)
            .await
            .unwrap();
        for (message_id, channel_id, channel_name, content, created_at) in [
            (10, 100, "questions", "first", "2026-08-09T00:00:00Z"),
            (11, 200, "projects", "other channel", "2026-08-09T00:01:00Z"),
            (12, 100, "questions", "third", "2026-08-09T00:02:00Z"),
        ] {
            database
                .record_message(&IncomingMessage {
                    discord_message_id: message_id,
                    guild_id: 1,
                    channel_id,
                    channel_name: Some(channel_name.into()),
                    settings_channel_id: channel_id,
                    author_id: 99,
                    author_name: "learner".into(),
                    content: content.into(),
                    is_bot: false,
                    created_at: created_at.into(),
                    referenced_message_id: None,
                    referenced_message: None,
                    attachments: if message_id == 10 {
                        vec![IncomingAttachment {
                            filename: "main.rs".into(),
                            content_type: Some("text/plain".into()),
                            size_bytes: 12,
                            extracted_text: Some("fn main() {}".into()),
                            binary_data: None,
                        }]
                    } else {
                        vec![]
                    },
                })
                .await
                .unwrap();
        }

        let context = database.recent_channel_context(1, 100, 10).await.unwrap();
        assert_eq!(context.len(), 2);
        assert_eq!(context[0].channel_id, 100);
        assert_eq!(context[0].attachments[0].filename, "main.rs");
        assert_eq!(context[1].content, "third");

        let active = database.recent_active_channels(1, 100, 10).await.unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].channel_id, 100);
        assert_eq!(active[0].channel_name.as_deref(), Some("questions"));
        assert!(active[0].is_current_channel);
        assert_eq!(active[0].last_message_at, "2026-08-09T00:02:00Z");
        assert_eq!(active[1].channel_id, 200);
        assert!(!active[1].is_current_channel);

        let search = database
            .search_messages(1, 100, "other channel", false, None, None, true, 10)
            .await
            .unwrap();
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].channel_name.as_deref(), Some("projects"));
        assert!(!search[0].is_current_channel);
        assert_eq!(search[0].created_at, "2026-08-09T00:01:00Z");
        assert_eq!(
            search[0].message_url,
            "https://discord.com/channels/1/200/11"
        );
    }

    #[tokio::test]
    async fn messages_newer_than_anchor_are_channel_isolated_and_chronological() {
        let database = test_database().await;
        database
            .set_channel_mode(1, 100, ChannelMode::ReadOnly, 99)
            .await
            .unwrap();
        for (message_id, channel_id, content, created_at) in [
            (10, 100, "first", "2026-08-09T00:00:00Z"),
            (11, 200, "other channel", "2026-08-09T00:01:00Z"),
            (12, 100, "second", "2026-08-09T00:02:00Z"),
            (13, 100, "third", "2026-08-09 00:03:00 +00:00"),
        ] {
            database
                .record_message(&IncomingMessage {
                    discord_message_id: message_id,
                    guild_id: 1,
                    channel_id,
                    channel_name: Some("questions".into()),
                    settings_channel_id: channel_id,
                    author_id: 99,
                    author_name: "learner".into(),
                    content: content.into(),
                    is_bot: false,
                    created_at: created_at.into(),
                    referenced_message_id: None,
                    referenced_message: None,
                    attachments: vec![],
                })
                .await
                .unwrap();
        }

        let after_message = database
            .messages_newer_than_message(1, 100, 10, 80)
            .await
            .unwrap();
        assert_eq!(
            after_message
                .iter()
                .map(|m| m.message_id)
                .collect::<Vec<_>>(),
            vec![12, 13]
        );
        assert!(
            database
                .messages_newer_than_message(1, 100, 13, 80)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            database
                .messages_newer_than_message(1, 200, 10, 80)
                .await
                .unwrap()
                .is_empty()
        );

        let after_datetime = database
            .messages_newer_than_datetime(1, 100, "2026-08-09T00:01:30Z", None, 80)
            .await
            .unwrap();
        assert_eq!(
            after_datetime
                .iter()
                .map(|m| m.message_id)
                .collect::<Vec<_>>(),
            vec![12, 13]
        );
        assert!(
            database
                .messages_newer_than_datetime(1, 100, "2026-08-09 00:03:00", None, 80)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn datetime_cursor_preserves_same_timestamp_messages_and_sorts_by_created_at() {
        let database = test_database().await;
        database
            .set_channel_mode(1, 100, ChannelMode::ReadOnly, 99)
            .await
            .unwrap();
        for (message_id, created_at) in [
            (2002, "2026-08-09T00:00:00.000Z"),
            (2001, "2026-08-08T23:59:00.000Z"),
            (2003, "2026-08-09T00:00:00.000Z"),
            (2004, "2026-08-09T00:00:00.000Z"),
            (2005, "2026-08-09T00:01:00.000Z"),
        ] {
            database
                .record_message(&IncomingMessage {
                    discord_message_id: message_id,
                    guild_id: 1,
                    channel_id: 100,
                    channel_name: Some("questions".into()),
                    settings_channel_id: 100,
                    author_id: 99,
                    author_name: "learner".into(),
                    content: format!("msg {message_id}"),
                    is_bot: false,
                    created_at: created_at.into(),
                    referenced_message_id: None,
                    referenced_message: None,
                    attachments: vec![],
                })
                .await
                .unwrap();
        }

        let first_page = database
            .messages_newer_than_datetime(1, 100, "2026-08-08T00:00:00Z", None, 2)
            .await
            .unwrap();
        assert_eq!(
            first_page
                .iter()
                .map(|message| message.message_id)
                .collect::<Vec<_>>(),
            vec![2001, 2002]
        );

        let second_page = database
            .messages_newer_than_datetime(1, 100, "2026-08-09T00:00:00.000Z", Some(2002), 2)
            .await
            .unwrap();
        assert_eq!(
            second_page
                .iter()
                .map(|message| message.message_id)
                .collect::<Vec<_>>(),
            vec![2003, 2004]
        );

        let third_page = database
            .messages_newer_than_datetime(1, 100, "2026-08-09T00:00:00.000Z", Some(2004), 2)
            .await
            .unwrap();
        assert_eq!(
            third_page
                .iter()
                .map(|message| message.message_id)
                .collect::<Vec<_>>(),
            vec![2005]
        );
    }

    #[tokio::test]
    async fn edited_messages_replace_searchable_content_and_deletes_are_purged() {
        let database = test_database().await;
        database
            .set_channel_mode(1, 100, ChannelMode::ReadOnly, 99)
            .await
            .unwrap();
        database
            .record_message(&IncomingMessage {
                discord_message_id: 10,
                guild_id: 1,
                channel_id: 100,
                channel_name: Some("questions".into()),
                settings_channel_id: 100,
                author_id: 99,
                author_name: "learner".into(),
                content: "old typo".into(),
                is_bot: false,
                created_at: "2026-08-09T00:00:00Z".into(),
                referenced_message_id: None,
                referenced_message: None,
                attachments: vec![],
            })
            .await
            .unwrap();

        assert!(
            database
                .update_message(
                    10,
                    Some("new Result<T, E>"),
                    None,
                    Some("2026-08-09T00:01:00Z")
                )
                .await
                .unwrap()
        );
        assert!(
            database
                .search_messages(1, 100, r"Result<.*>", true, None, None, true, 10)
                .await
                .unwrap()
                .first()
                .is_some_and(|result| {
                    result.message_id == 10
                        && result.channel_name.as_deref() == Some("questions")
                        && result.is_current_channel
                        && result.created_at == "2026-08-09T00:00:00Z"
                        && result.message_url == "https://discord.com/channels/1/100/10"
                })
        );
        assert!(
            database
                .search_messages(1, 100, "old typo", false, None, None, true, 10)
                .await
                .unwrap()
                .is_empty()
        );

        assert!(database.delete_message(10).await.unwrap());
        assert!(
            database
                .recent_channel_context(1, 100, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn active_memory_can_be_reviewed_and_removed() {
        let database = test_database().await;
        let id = database
            .create_memory(
                1,
                MemoryKind::Project,
                "error policy",
                "Use Result",
                "Failures are explicit in the return type.",
                MemoryStatus::Active,
                99,
                Some(100),
                Some(10),
            )
            .await
            .unwrap();
        let active = database.active_memories(1).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, id);
        assert!(database.review_memory(1, id).await.unwrap());
        assert!(database.remove_memory(1, id).await.unwrap());
        assert!(database.active_memories(1).await.unwrap().is_empty());
    }
}
