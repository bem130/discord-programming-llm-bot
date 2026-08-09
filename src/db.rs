use std::{str::FromStr, time::Duration};

use anyhow::{Context, Result};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

use crate::model::{
    ChannelMode, ChannelSetting, ContextAttachment, ContextMessage, IncomingMessage,
};

const MAX_CONTEXT_MESSAGES: u32 = 80;

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

    pub async fn record_message(&self, message: &IncomingMessage) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        let result = sqlx::query(
            r#"
            INSERT INTO messages (
                discord_message_id, guild_id, channel_id, author_id,
                author_name, content, is_bot, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (discord_message_id) DO NOTHING
            "#,
        )
        .bind(message.discord_message_id.to_string())
        .bind(message.guild_id.to_string())
        .bind(message.channel_id.to_string())
        .bind(message.author_id.to_string())
        .bind(&message.author_name)
        .bind(&message.content)
        .bind(message.is_bot)
        .bind(&message.created_at)
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

    pub async fn recent_context(&self, guild_id: u64, limit: u32) -> Result<Vec<ContextMessage>> {
        let limit = limit.clamp(1, MAX_CONTEXT_MESSAGES);
        let rows = sqlx::query(
            r#"
            SELECT id, channel_id, author_id, author_name, content, is_bot, created_at
            FROM messages
            WHERE guild_id = ?
            ORDER BY id DESC
            LIMIT ?
            "#,
        )
        .bind(guild_id.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load guild context")?;

        let mut messages = Vec::with_capacity(rows.len());
        for row in rows.into_iter().rev() {
            let message_id: i64 = row.try_get("id")?;
            let attachment_rows = sqlx::query(
                "SELECT filename, content_type, extracted_text, binary_data FROM message_attachments WHERE message_id = ? ORDER BY id",
            )
            .bind(message_id)
            .fetch_all(&self.pool)
            .await?;

            messages.push(ContextMessage {
                channel_id: parse_id(row.try_get("channel_id")?)?,
                author_id: parse_id(row.try_get("author_id")?)?,
                author_name: row.try_get("author_name")?,
                content: row.try_get("content")?,
                is_bot: row.try_get("is_bot")?,
                created_at: row.try_get("created_at")?,
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
    async fn context_is_shared_across_channels_and_chronological() {
        let database = test_database().await;
        for (message_id, channel_id, content) in [(10, 100, "first"), (11, 200, "second")] {
            database
                .record_message(&IncomingMessage {
                    discord_message_id: message_id,
                    guild_id: 1,
                    channel_id,
                    author_id: 99,
                    author_name: "learner".into(),
                    content: content.into(),
                    is_bot: false,
                    created_at: "2026-08-09T00:00:00Z".into(),
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

        let context = database.recent_context(1, 10).await.unwrap();
        assert_eq!(context.len(), 2);
        assert_eq!(context[0].channel_id, 100);
        assert_eq!(context[0].attachments[0].filename, "main.rs");
        assert_eq!(context[1].channel_id, 200);
    }
}
