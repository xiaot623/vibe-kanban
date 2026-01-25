#!/bin/bash

set -e

SOURCE_DB="${SOURCE_DB:-$HOME/Library/Application Support/ai.bloop.vibe-kanban/db.sqlite}"
TARGET_DB="${TARGET_DB:-$HOME/.kanban/db.sqlite}"

if [ ! -f "$SOURCE_DB" ]; then
    echo "Error: Source database not found: $SOURCE_DB"
    exit 1
fi

echo "Migrating database from $SOURCE_DB to $TARGET_DB..."

sqlite3 "$TARGET_DB" << 'EOF'
ATTACH '~/Library/Application Support/ai.bloop.vibe-kanban/db.sqlite' AS source_db;

PRAGMA foreign_keys = OFF;

BEGIN TRANSACTION;

DELETE FROM main.task_images;
DELETE FROM main.images;
DELETE FROM main.merges;
DELETE FROM main.execution_process_repo_states;
DELETE FROM main.workspace_repos;
DELETE FROM main.project_repos;
DELETE FROM main.coding_agent_turns;
DELETE FROM main.execution_process_logs;
DELETE FROM main.execution_processes;
DELETE FROM main.sessions;
DELETE FROM main.workspaces;
DELETE FROM main.tasks;
DELETE FROM main.projects;
DELETE FROM main.repos;
DELETE FROM main.tags;
DELETE FROM main.scratch;

INSERT INTO main.projects (id, name, default_agent_working_dir, created_at, updated_at)
SELECT id, name, default_agent_working_dir, created_at, updated_at FROM source_db.projects;

INSERT INTO main.repos (id, path, name, display_name, setup_script, cleanup_script, copy_files, parallel_setup_script, dev_server_script, created_at, updated_at)
SELECT id, path, name, display_name, setup_script, cleanup_script, copy_files, parallel_setup_script, dev_server_script, created_at, updated_at FROM source_db.repos;

INSERT INTO main.project_repos (id, project_id, repo_id)
SELECT id, project_id, repo_id FROM source_db.project_repos;

INSERT INTO main.tasks (id, project_id, title, description, status, parent_workspace_id, created_at, updated_at)
SELECT id, project_id, title, description, status, parent_workspace_id, created_at, updated_at FROM source_db.tasks;

INSERT INTO main.workspaces (id, task_id, container_ref, branch, agent_working_dir, setup_completed_at, archived, pinned, name, created_at, updated_at)
SELECT id, task_id, container_ref, branch, agent_working_dir, setup_completed_at, archived, pinned, name, created_at, updated_at FROM source_db.workspaces;

INSERT INTO main.sessions (id, workspace_id, executor, created_at, updated_at)
SELECT id, workspace_id, executor, created_at, updated_at FROM source_db.sessions;

INSERT INTO main.execution_processes (id, session_id, run_reason, executor_action, status, exit_code, dropped, started_at, completed_at, created_at, updated_at)
SELECT id, session_id, run_reason, executor_action, status, exit_code, dropped, started_at, completed_at, created_at, updated_at FROM source_db.execution_processes;

INSERT INTO main.execution_process_logs (execution_id, logs, byte_size, inserted_at)
SELECT execution_id, logs, byte_size, inserted_at FROM source_db.execution_process_logs;

INSERT INTO main.coding_agent_turns (id, execution_process_id, agent_session_id, prompt, summary, seen, created_at, updated_at)
SELECT id, execution_process_id, agent_session_id, prompt, summary, seen, created_at, updated_at FROM source_db.coding_agent_turns;

INSERT INTO main.workspace_repos (id, workspace_id, repo_id, target_branch, created_at, updated_at)
SELECT id, workspace_id, repo_id, target_branch, created_at, updated_at FROM source_db.workspace_repos;

INSERT INTO main.execution_process_repo_states (id, execution_process_id, repo_id, before_head_commit, after_head_commit, merge_commit, created_at, updated_at)
SELECT id, execution_process_id, repo_id, before_head_commit, after_head_commit, merge_commit, created_at, updated_at FROM source_db.execution_process_repo_states;

INSERT INTO main.images (id, file_path, original_name, mime_type, size_bytes, hash, created_at, updated_at)
SELECT id, file_path, original_name, mime_type, size_bytes, hash, created_at, updated_at FROM source_db.images;

INSERT INTO main.task_images (id, task_id, image_id, created_at)
SELECT id, task_id, image_id, created_at FROM source_db.task_images;

INSERT INTO main.tags (id, tag_name, content, created_at, updated_at)
SELECT id, tag_name, content, created_at, updated_at FROM source_db.tags;

INSERT INTO main.scratch (id, scratch_type, payload, created_at, updated_at)
SELECT id, scratch_type, payload, created_at, updated_at FROM source_db.scratch;

INSERT INTO main.merges (id, workspace_id, repo_id, merge_type, merge_commit, pr_number, pr_url, pr_status, pr_merged_at, pr_merge_commit_sha, target_branch_name, created_at)
SELECT id, workspace_id, repo_id, merge_type, merge_commit, pr_number, pr_url, pr_status, pr_merged_at, pr_merge_commit_sha, target_branch_name, created_at FROM source_db.merges;

COMMIT;
EOF

echo "Migration completed successfully!"
