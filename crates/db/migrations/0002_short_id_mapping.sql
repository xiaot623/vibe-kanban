CREATE TABLE IF NOT EXISTS short_id_mappings (
    short_id   TEXT PRIMARY KEY,
    task_id    BLOB NOT NULL,
    expires_at TEXT NOT NULL,
    UNIQUE(task_id)
);
