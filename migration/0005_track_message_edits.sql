ALTER TABLE messages ADD COLUMN edited_at TEXT;

CREATE TABLE message_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    previous_content TEXT NOT NULL,
    revised_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX message_revisions_message_idx
    ON message_revisions (message_id, id DESC);
