CREATE TABLE execution_process_lark_wiki_docs (
    execution_process_id BLOB PRIMARY KEY NOT NULL,
    doc_id TEXT NOT NULL,
    url TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (execution_process_id) REFERENCES execution_processes(id) ON DELETE CASCADE
);
