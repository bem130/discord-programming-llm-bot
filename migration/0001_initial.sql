PRAGMA foreign_keys = ON;

CREATE TABLE channel_settings (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('off', 'read_only', 'mention', 'auto')),
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (guild_id, channel_id)
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    discord_message_id TEXT NOT NULL UNIQUE,
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    author_id TEXT NOT NULL,
    author_name TEXT NOT NULL,
    content TEXT NOT NULL,
    is_bot INTEGER NOT NULL CHECK (is_bot IN (0, 1)),
    created_at TEXT NOT NULL
);

CREATE INDEX messages_guild_context_idx
    ON messages (guild_id, id DESC);

CREATE TABLE message_attachments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    content_type TEXT,
    size_bytes INTEGER NOT NULL,
    extracted_text TEXT
);

CREATE INDEX message_attachments_message_idx
    ON message_attachments (message_id);

