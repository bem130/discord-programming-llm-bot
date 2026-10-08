CREATE TABLE channel_history_sync (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    settings_channel_id TEXT NOT NULL,
    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (guild_id, channel_id)
);
