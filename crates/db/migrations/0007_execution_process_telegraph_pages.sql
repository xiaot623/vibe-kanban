CREATE TABLE execution_process_telegraph_pages (
    execution_process_id TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (execution_process_id) REFERENCES execution_processes(id) ON DELETE CASCADE
);

CREATE INDEX idx_execution_process_telegraph_pages_execution_process_id
    ON execution_process_telegraph_pages(execution_process_id);
