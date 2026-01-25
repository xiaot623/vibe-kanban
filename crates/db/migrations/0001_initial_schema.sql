-- Initial Schema for Vibe Kanban (Consolidated)

-- 1. Projects
CREATE TABLE projects (
    id                        BLOB PRIMARY KEY,
    name                      TEXT NOT NULL,
    default_agent_working_dir TEXT,
    created_at                TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at                TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);

-- 2. Repositories (Global Registry)
CREATE TABLE repos (
    id                      BLOB PRIMARY KEY,
    path                    TEXT NOT NULL UNIQUE,
    name                    TEXT NOT NULL,
    display_name            TEXT NOT NULL,
    setup_script            TEXT,
    cleanup_script          TEXT,
    copy_files              TEXT,
    parallel_setup_script   INTEGER NOT NULL DEFAULT 0,
    dev_server_script       TEXT,
    created_at              TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at              TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);

-- 3. Project Repositories Junction
CREATE TABLE project_repos (
    id                      BLOB PRIMARY KEY,
    project_id              BLOB NOT NULL,
    repo_id                 BLOB NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
    UNIQUE (project_id, repo_id)
);
CREATE INDEX idx_project_repos_project_id ON project_repos(project_id);
CREATE INDEX idx_project_repos_repo_id ON project_repos(repo_id);

-- 4. Workspaces (formerly Task Attempts)
CREATE TABLE workspaces (
    id                 BLOB PRIMARY KEY,
    task_id            BLOB NOT NULL,
    container_ref      TEXT,
    branch             TEXT NOT NULL DEFAULT 'main',
    agent_working_dir  TEXT,
    setup_completed_at DATETIME,
    archived           INTEGER NOT NULL DEFAULT 0,
    pinned             INTEGER NOT NULL DEFAULT 0,
    name               TEXT,
    created_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at         TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
);
CREATE INDEX idx_workspaces_task_id_created_at ON workspaces (task_id, created_at DESC);
CREATE INDEX idx_workspaces_created_at ON workspaces (created_at DESC);

-- 5. Tasks
CREATE TABLE tasks (
    id                    BLOB PRIMARY KEY,
    project_id            BLOB NOT NULL,
    title                 TEXT NOT NULL,
    description           TEXT,
    status                TEXT NOT NULL DEFAULT 'todo'
                          CHECK (status IN ('todo','inprogress','done','cancelled','inreview')),
    parent_workspace_id   BLOB,
    created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    FOREIGN KEY (parent_workspace_id) REFERENCES workspaces(id) ON DELETE SET NULL
);
CREATE INDEX idx_tasks_project_created_at ON tasks (project_id, created_at DESC);
CREATE INDEX idx_tasks_parent_workspace_id ON tasks(parent_workspace_id);

-- 6. Sessions (Executor Sessions)
CREATE TABLE sessions (
    id              BLOB PRIMARY KEY,
    workspace_id    BLOB NOT NULL,
    executor        TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);
CREATE INDEX idx_sessions_workspace_id ON sessions(workspace_id);

-- 7. Execution Processes
CREATE TABLE execution_processes (
    id              BLOB PRIMARY KEY,
    session_id      BLOB NOT NULL,
    run_reason      TEXT NOT NULL DEFAULT 'setupscript'
                       CHECK (run_reason IN ('setupscript','codingagent','devserver','cleanupscript')),
    executor_action TEXT NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL DEFAULT 'running'
                       CHECK (status IN ('running','completed','failed','killed')),
    exit_code       INTEGER,
    dropped         INTEGER NOT NULL DEFAULT 0,
    started_at      TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    completed_at    TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
CREATE INDEX idx_execution_processes_session_id ON execution_processes(session_id);
CREATE INDEX idx_execution_processes_status ON execution_processes(status);
CREATE INDEX idx_execution_processes_run_reason ON execution_processes(run_reason);
CREATE INDEX idx_execution_processes_session_status_run_reason ON execution_processes (session_id, status, run_reason);
CREATE INDEX idx_execution_processes_session_run_reason_created ON execution_processes (session_id, run_reason, created_at DESC);

-- 8. Execution Process Logs
CREATE TABLE execution_process_logs (
    execution_id      BLOB NOT NULL,
    logs              TEXT NOT NULL,      -- JSONL format
    byte_size         INTEGER NOT NULL,
    inserted_at       TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (execution_id) REFERENCES execution_processes(id) ON DELETE CASCADE
);
CREATE INDEX idx_execution_process_logs_execution_id_inserted_at ON execution_process_logs (execution_id, inserted_at);

-- 9. Coding Agent Turns (formerly Executor Sessions)
CREATE TABLE coding_agent_turns (
    id                    BLOB PRIMARY KEY,
    execution_process_id  BLOB NOT NULL,
    agent_session_id      TEXT,
    prompt                TEXT,
    summary               TEXT,
    seen                  INTEGER NOT NULL DEFAULT 0,
    created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (execution_process_id) REFERENCES execution_processes(id) ON DELETE CASCADE
);
CREATE INDEX idx_coding_agent_turns_execution_process_id ON coding_agent_turns(execution_process_id);
CREATE INDEX idx_coding_agent_turns_agent_session_id ON coding_agent_turns(agent_session_id);

-- 10. Workspace Repositories (formerly Attempt Repos)
CREATE TABLE workspace_repos (
    id            BLOB PRIMARY KEY,
    workspace_id  BLOB NOT NULL,
    repo_id       BLOB NOT NULL,
    target_branch TEXT NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
    FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
    UNIQUE (workspace_id, repo_id)
);
CREATE INDEX idx_workspace_repos_workspace_id ON workspace_repos(workspace_id);
CREATE INDEX idx_workspace_repos_repo_id ON workspace_repos(repo_id);

-- 11. Execution Process Repo States
CREATE TABLE execution_process_repo_states (
    id                   BLOB PRIMARY KEY,
    execution_process_id BLOB NOT NULL,
    repo_id              BLOB NOT NULL,
    before_head_commit   TEXT,
    after_head_commit    TEXT,
    merge_commit         TEXT,
    created_at           TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at           TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (execution_process_id) REFERENCES execution_processes(id) ON DELETE CASCADE,
    FOREIGN KEY (repo_id) REFERENCES repos(id) ON DELETE CASCADE,
    UNIQUE (execution_process_id, repo_id)
);
CREATE INDEX idx_eprs_process_id ON execution_process_repo_states(execution_process_id);
CREATE INDEX idx_eprs_repo_id ON execution_process_repo_states(repo_id);

-- 12. Merges
CREATE TABLE merges (
    id              BLOB PRIMARY KEY,
    workspace_id    BLOB NOT NULL,
    repo_id         BLOB NOT NULL,
    merge_type      TEXT NOT NULL CHECK (merge_type IN ('direct', 'pr')),
    
    -- Direct merge fields
    merge_commit    TEXT,
    
    -- PR merge fields
    pr_number       INTEGER,
    pr_url          TEXT,
    pr_status       TEXT CHECK (pr_status IN ('open', 'merged', 'closed')),
    pr_merged_at    TEXT,
    pr_merge_commit_sha TEXT,
    
    target_branch_name TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),

    CHECK (
        (merge_type = 'direct' AND merge_commit IS NOT NULL 
         AND pr_number IS NULL AND pr_url IS NULL) 
        OR 
        (merge_type = 'pr' AND pr_number IS NOT NULL AND pr_url IS NOT NULL 
         AND pr_status IS NOT NULL AND merge_commit IS NULL)
    ),
    
    FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
    FOREIGN KEY (repo_id) REFERENCES repos(id)
);
CREATE INDEX idx_merges_workspace_id ON merges(workspace_id);
CREATE INDEX idx_merges_repo_id ON merges(repo_id);
CREATE INDEX idx_merges_open_pr ON merges(workspace_id, pr_status) 
WHERE merge_type = 'pr' AND pr_status = 'open';

-- 13. Images
CREATE TABLE images (
    id                    BLOB PRIMARY KEY,
    file_path             TEXT NOT NULL,
    original_name         TEXT NOT NULL,
    mime_type             TEXT,
    size_bytes            INTEGER,
    hash                  TEXT NOT NULL UNIQUE,
    created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);
CREATE INDEX idx_images_hash ON images(hash);

-- 14. Task Images
CREATE TABLE task_images (
    id                    BLOB PRIMARY KEY,
    task_id               BLOB NOT NULL,
    image_id              BLOB NOT NULL,
    created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE,
    FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE,
    UNIQUE(task_id, image_id)
);
CREATE INDEX idx_task_images_task_id ON task_images(task_id);
CREATE INDEX idx_task_images_image_id ON task_images(image_id);

-- 15. Tags
CREATE TABLE tags (
    id            BLOB PRIMARY KEY,
    tag_name      TEXT NOT NULL CHECK(INSTR(tag_name, ' ') = 0),
    content       TEXT NOT NULL CHECK(content != ''),
    created_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);

-- 16. Scratch
CREATE TABLE scratch (
    id           BLOB NOT NULL,
    scratch_type TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at   TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    PRIMARY KEY (id, scratch_type)
);
CREATE INDEX idx_scratch_created_at ON scratch(created_at);
