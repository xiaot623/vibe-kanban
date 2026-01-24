-- Remove remote/shared functionality columns and tables

-- SQLite doesn't support DROP COLUMN directly, so we need to recreate tables

-- Step 1: Recreate tasks table without shared_task_id
PRAGMA foreign_keys = OFF;

-- Create new tasks table without shared_task_id
CREATE TABLE tasks_new (
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

-- Copy data from old tasks table (excluding shared_task_id)
INSERT INTO tasks_new (id, project_id, title, description, status, parent_workspace_id, created_at, updated_at)
SELECT id, project_id, title, description, status, parent_workspace_id, created_at, updated_at
FROM tasks;

-- Drop old tasks table and rename new one
DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

-- Recreate indexes for tasks
CREATE INDEX IF NOT EXISTS idx_tasks_project_id ON tasks(project_id);
CREATE INDEX IF NOT EXISTS idx_tasks_parent_workspace_id ON tasks(parent_workspace_id);

-- Step 2: Recreate projects table without remote_project_id
CREATE TABLE projects_new (
    id                        BLOB PRIMARY KEY,
    name                      TEXT NOT NULL,
    default_agent_working_dir TEXT,
    created_at                TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at                TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);

-- Copy data from old projects table (excluding remote_project_id)
INSERT INTO projects_new (id, name, default_agent_working_dir, created_at, updated_at)
SELECT id, name, default_agent_working_dir, created_at, updated_at
FROM projects;

-- Drop old projects table and rename new one
DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

-- Step 3: Drop shared_tasks related tables
DROP TABLE IF EXISTS shared_activity_cursors;
DROP TABLE IF EXISTS shared_tasks;

PRAGMA foreign_keys = ON;
