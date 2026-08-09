CREATE TABLE guild_channels (
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    channel_name TEXT,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (guild_id, channel_id)
);

INSERT INTO guild_channels (guild_id, channel_id)
SELECT DISTINCT guild_id, channel_id
FROM messages;

CREATE INDEX messages_guild_channel_activity_idx
    ON messages (guild_id, channel_id, id DESC);
