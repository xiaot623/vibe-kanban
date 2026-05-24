CREATE TABLE IF NOT EXISTS telegram_task_topics (
    task_id           BLOB PRIMARY KEY,
    message_thread_id INTEGER,
    topic_name        TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_telegram_task_topics_message_thread_id
    ON telegram_task_topics(message_thread_id);
