ALTER TABLE channel_history_sync ADD COLUMN last_synced_message_id TEXT;

-- Older completion flags used MAX(messages), which could include live messages
-- beyond a gap. Rebuild once to establish a trustworthy synchronization cursor.
UPDATE channel_history_sync SET completed = 0;
