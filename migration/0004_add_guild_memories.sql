CREATE TABLE guild_memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('feedback', 'project', 'reference')),
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    content TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('candidate', 'active', 'deleted')),
    created_by TEXT NOT NULL,
    source_channel_id TEXT,
    source_message_id TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    reviewed_at TEXT,
    review_after TEXT
);

CREATE INDEX guild_memories_index_idx
    ON guild_memories (guild_id, status, updated_at DESC);

CREATE INDEX guild_memories_review_idx
    ON guild_memories (guild_id, status, review_after);
