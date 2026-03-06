# Database Storage Structure

This crate manages the persistence layer for the application using SQLite (via sqlx). The database schema supports the core entities: Projects, Repositories, Tasks, Workspaces (Attempts), and their execution history.

## Schema Overview

The database is normalized and consists of the following key tables:

### 1. Projects (`projects`)
Represents the top-level organizational unit.
- `id`: Unique identifier (BLOB).
- `name`: Project name.
- `default_agent_working_dir`: Default working directory for agents.

### 2. Repositories (`repos`)
A global registry of git repositories available to the system.
- `id`: Unique identifier (BLOB).
- `path`: Absolute filesystem path to the repository.
- `name`, `display_name`: Identifiers for display.
- `setup_script`, `cleanup_script`: Scripts for environment management.
- `dev_server_script`: Script to start a development server.

### 3. Project Repositories (`project_repos`)
Junction table linking Projects to Repositories.
- `project_id`, `repo_id`: Foreign keys.

### 4. Tasks (`tasks`)
Represents a unit of work (ticket/issue).
- `id`: Unique identifier.
- `project_id`: The project this task belongs to.
- `title`, `description`: Task details.
- `status`: Lifecycle state (`todo`, `inprogress`, `done`, `cancelled`, `inreview`).
- `parent_workspace_id`: Link to a workspace if this is a subtask.

### 5. Workspaces (`workspaces`)
Previously known as "Task Attempts". Represents a specific attempt to work on a task in a dedicated environment.
- `task_id`: The task being worked on.
- `branch`: The git branch associated with this workspace (default: `main`).
- `container_ref`: Reference to the container/environment.

### 6. Workspace Repositories (`workspace_repos`)
Junction table defining which repositories are active in a specific workspace and their target branches.

### 7. Sessions (`sessions`)
Represents a user or agent session within a workspace.
- `workspace_id`: The workspace context.
- `executor`: The agent or user executor (e.g., 'CLAUDE_CODE', 'user').

### 8. Execution Processes (`execution_processes`)
Records individual processes run within a session.
- `session_id`: Parent session.
- `run_reason`: Why it ran (`setupscript`, `codingagent`, `devserver`, `cleanupscript`).
- `status`: Process state (`running`, `completed`, `failed`, `killed`).
- `executor_action`: JSON payload of the action taken.

### 9. Execution Process Logs (`execution_process_logs`)
Stores logs for execution processes in JSONL format.
- `execution_id`: The process these logs belong to.
- `logs`: Log content.
- `msg_type`: Optional message type for fast-path reads (e.g. `stdout`, `stderr`, `json_patch`).

### 10. Coding Agent Turns (`coding_agent_turns`)
Specific to coding agents, tracking individual interaction turns.
- `execution_process_id`: The parent process.
- `prompt`, `summary`: The input and summary of the turn.

### 11. Execution Process Repo States (`execution_process_repo_states`)
Snapshots of repository states (commit hashes) before and after an execution process.
- `execution_process_id`: The process.
- `repo_id`: The repository.
- `before_head_commit`, `after_head_commit`: Git commit hashes.

### 12. Merges (`merges`)
Tracks merge operations (Direct or Pull Request) for a workspace.
- `workspace_id`, `repo_id`: Context.
- `merge_type`: `direct` or `pr`.
- `pr_*`: Fields specific to Pull Requests (number, URL, status).

### 13. Images (`images`) & 14. Task Images (`task_images`)
Storage for image assets and their association with tasks.

### 15. Tags (`tags`)
General purpose tags.

### 16. Scratch (`scratch`)
A key-value store for temporary or miscellaneous data.
- `scratch_type`: Category/Type of scratch data.
- `payload`: Data content.

## Relationships

- **Project** 1--* **Task**
- **Task** 1--* **Workspace**
- **Workspace** 1--* **Session**
- **Session** 1--* **Execution Process**
- **Execution Process** 1--* **Coding Agent Turn**
