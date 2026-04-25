CREATE TABLE execution_process_lark_wiki_month_nodes (
    space_id TEXT NOT NULL,
    month TEXT NOT NULL,
    node_token TEXT NOT NULL,
    obj_token TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (space_id, month)
);
