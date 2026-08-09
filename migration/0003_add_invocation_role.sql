CREATE TABLE guild_settings (
    guild_id TEXT PRIMARY KEY,
    invocation_role_id TEXT NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
