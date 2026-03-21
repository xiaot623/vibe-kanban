ALTER TABLE execution_process_telegraph_pages
    RENAME TO execution_process_telegraph_pages_old;

CREATE TABLE execution_process_telegraph_pages (
    execution_process_id TEXT NOT NULL,
    page_index INTEGER NOT NULL,
    url TEXT NOT NULL,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (execution_process_id, page_index),
    FOREIGN KEY (execution_process_id) REFERENCES execution_processes(id) ON DELETE CASCADE
);

INSERT INTO execution_process_telegraph_pages (
    execution_process_id,
    page_index,
    url,
    path,
    title,
    created_at,
    updated_at
)
SELECT
    execution_process_id,
    0,
    url,
    path,
    title,
    created_at,
    updated_at
FROM execution_process_telegraph_pages_old;

DROP TABLE execution_process_telegraph_pages_old;

CREATE INDEX idx_execution_process_telegraph_pages_execution_process_id
    ON execution_process_telegraph_pages(execution_process_id);
