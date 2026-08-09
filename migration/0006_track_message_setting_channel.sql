ALTER TABLE messages ADD COLUMN settings_channel_id TEXT;

UPDATE messages
SET settings_channel_id = channel_id
WHERE settings_channel_id IS NULL;

CREATE INDEX messages_guild_setting_channel_idx
    ON messages (guild_id, settings_channel_id, id DESC);
