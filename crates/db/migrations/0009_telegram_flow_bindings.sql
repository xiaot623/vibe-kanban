CREATE TABLE IF NOT EXISTS telegram_flow_bindings (
    flow_token                  TEXT PRIMARY KEY,
    task_id                     BLOB NOT NULL,
    workspace_id                BLOB NOT NULL,
    session_id                  BLOB NOT NULL UNIQUE,
    latest_execution_process_id BLOB,
    executor_label              TEXT NOT NULL,
    expires_at                  TEXT NOT NULL,
    created_at                  TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at                  TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (latest_execution_process_id) REFERENCES execution_processes(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_telegram_flow_bindings_session_id
    ON telegram_flow_bindings(session_id);

CREATE INDEX IF NOT EXISTS idx_telegram_flow_bindings_task_id
    ON telegram_flow_bindings(task_id);

CREATE INDEX IF NOT EXISTS idx_telegram_flow_bindings_workspace_id
    ON telegram_flow_bindings(workspace_id);

CREATE INDEX IF NOT EXISTS idx_telegram_flow_bindings_expires_at
    ON telegram_flow_bindings(expires_at);
