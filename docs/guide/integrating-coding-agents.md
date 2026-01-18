---
title: "Integrate a Coding Agent (Claude Code example)"
description: "Developer guide for adding a new coding agent executor to Vibe Kanban, using Claude Code as the reference implementation"
---

This guide explains how Vibe Kanban integrates with coding agent CLIs (like Claude Code), and how you can add support for a new agent end-to-end. You will build an *executor* that:

- Launches the agent CLI in a Git worktree
- Streams stdout/stderr into Vibe Kanban’s log pipeline
- Supports follow-ups by resuming a session (when the agent supports it)
- Optionally supports tool approvals and MCP configuration

## How the integration works

At a high level:

1. The frontend selects an **executor profile** (agent + optional variant) and sends a prompt.
2. The backend resolves that to a concrete agent configuration (from built-in defaults and user overrides).
3. The `crates/executors` crate spawns the agent CLI process and streams logs into a `MsgStore`.
4. Each executor normalises raw logs into Vibe Kanban’s `NormalizedEntry` format for rich UI rendering.
5. If the agent emits a session identifier, Vibe Kanban stores it and uses it for follow-ups.

### Runtime entry points

In code, “run an agent” is expressed as an action that spawns a child process:

- Initial run: `CodingAgentInitialRequest` in `crates/executors/src/actions/coding_agent_initial.rs`
- Follow-up: `CodingAgentFollowUpRequest` in `crates/executors/src/actions/coding_agent_follow_up.rs`
- Review mode (optional): `ReviewRequest` in `crates/executors/src/actions/review.rs`

Each request resolves an `ExecutorProfileId` (agent + optional variant) into a concrete `CodingAgent` via `ExecutorConfigs::get_cached()` in `crates/executors/src/profile.rs`, then calls the agent’s `StandardCodingAgentExecutor` implementation.

<Info>
If you are only setting up Claude Code as a user (not adding a new integration), see [Claude Code](/agents/claude-code) and [Agent Profiles & Configuration](/configuration-customisation/agent-configurations).
</Info>

## Where to make changes

These are the files you will touch most often when integrating a new agent:

- `crates/executors/src/executors/mod.rs`: Registers the agent in `CodingAgent`, declares capabilities, and defines the executor trait.
- `crates/executors/src/executors/<agent>.rs`: Implements `StandardCodingAgentExecutor` for your agent.
- `crates/executors/src/profile.rs`: Loads built-in profiles from `crates/executors/default_profiles.json` and merges user overrides.
- `crates/executors/default_profiles.json`: Adds the default profile and variants surfaced in Settings → Agents.
- `crates/executors/src/logs/*`: Shared utilities for building `NormalizedEntry` action logs.
- `crates/server/src/bin/generate_types.rs`: Exports Rust types for TS generation.
- `frontend/src/components/agents/AgentIcon.tsx`: Adds display name/icon for the new agent.

## Integration checklist (works for any agent)

<Steps>
<Step title="Define the agent configuration type">
  Create a new executor module under `crates/executors/src/executors/`.

  Your config type should typically:
  - Derive `Serialize`, `Deserialize`, `TS`, and `JsonSchema`
  - Include `append_prompt: AppendPrompt` (for per-profile prompt suffixes)
  - Include `cmd: CmdOverrides` (for `base_command_override`, `additional_params`, and `env`)

  <Tip>
  Follow the existing patterns in `crates/executors/src/executors/claude.rs` and `crates/executors/src/executors/codex.rs` to keep the Settings UI and JSON editor consistent.
  </Tip>
</Step>

<Step title="Implement process spawning">
  Implement `StandardCodingAgentExecutor` for your agent in `crates/executors/src/executors/mod.rs`.

  You must implement:
  - `spawn(...)` for a new conversation
  - `spawn_follow_up(...)` to resume or fork a session (if supported)
  - `normalize_logs(...)` to convert raw logs into `NormalizedEntry` patches
  - `default_mcp_config_path(...)` so Vibe Kanban can read/write the agent’s MCP config

  Use `CommandBuilder` + `CmdOverrides` so users can override the base command without patching Vibe Kanban.
</Step>

<Step title="Decide what a “session” means">
  For follow-ups, Vibe Kanban needs a stable `session_id` string.

  - If the agent supports resuming, extract a session identifier from stdout and store it via `msg_store.push_session_id(...)`.
  - If the agent does not support resuming, return `ExecutorError::FollowUpNotSupported(...)` from `spawn_follow_up(...)`.

  <Note>
  Capabilities are surfaced to the UI via `CodingAgent::capabilities()` in `crates/executors/src/executors/mod.rs` (for example, `SessionFork`).
  </Note>
</Step>

<Step title="Normalise logs for the UI">
  Vibe Kanban renders “actions” (commands, file edits, searches) from normalised logs.

  Your executor should:
  - Read raw output from stdout/stderr
  - Parse the agent’s native format (plain text, JSON, JSON-RPC, etc.)
  - Emit `ConversationPatch` updates with `NormalizedEntry` records

  <Tip>
  If your agent only emits plain text, start with `crates/executors/src/logs/plain_text_processor.rs` and incrementally add richer parsing as needed.
  </Tip>
</Step>

<Step title="Add MCP config support (optional)">
  If your agent supports MCP servers, implement `default_mcp_config_path(...)` and ensure `CodingAgent::get_mcp_config()` maps to the correct field path and file format.

  - JSON vs TOML is handled by `crates/executors/src/mcp_config.rs`
  - Server schema differences can be adapted in `CodingAgent::preconfigured_mcp()`
</Step>

<Step title="Add tool approvals (optional)">
  If the agent supports interactive tool permissions, connect it to Vibe Kanban’s approval service:

  - Use `StandardCodingAgentExecutor::use_approvals(...)` to receive an `ExecutorApprovalService`
  - When the agent requests permission for a tool, call `request_tool_approval(...)`
  - Translate the approval decision back into the agent’s native permission/allow/deny response

  <Note>
  The Claude Code integration is the reference for a control-protocol-based approval flow.
  </Note>
</Step>

<Step title="Register the new agent in the workspace defaults">
  Add a default profile and any useful variants in `crates/executors/default_profiles.json`.

  This is what shows up as built-in options in Settings → Agents.
</Step>

<Step title="Regenerate shared TypeScript types">
  Vibe Kanban generates `shared/types.ts` from Rust types.

  - Export any new types from `crates/server/src/bin/generate_types.rs`
  - Run `pnpm run generate-types`

  <Warning>
  Do not edit `shared/types.ts` directly.
  </Warning>
</Step>

<Step title="Update the frontend for naming and icons">
  Most UI wiring is data-driven from `BaseCodingAgent`, but you may need to update:
  - `frontend/src/components/agents/AgentIcon.tsx` (display name/icon)
  - Any onboarding or settings defaults that assume a particular agent
</Step>

<Step title="Validate end-to-end">
  Run checks locally and verify in the app:

  ```bash
  pnpm run backend:check
  pnpm run check
  ```

  Then start a task attempt with your new agent and confirm:
  - Logs render with the expected action types
  - A session ID is captured (if follow-ups are supported)
  - MCP servers can be added and persist to the agent’s config file (if supported)
</Step>
</Steps>

## Profiles: defaults and user overrides

Vibe Kanban treats “agent configuration” as **profiles**:

- Built-in defaults live in `crates/executors/default_profiles.json`.
- User overrides are stored in `profiles.json` (Vibe Kanban shows you the exact path in Settings).
- The merge logic lives in `ExecutorConfigs::load()` and `ExecutorConfigs::merge_with_defaults(...)` in `crates/executors/src/profile.rs`.

If you add a new agent and forget to add at least a `DEFAULT` configuration in `crates/executors/default_profiles.json`, it will not appear as an available built-in profile.

## Claude Code integration: the reference implementation

Claude Code is implemented in `crates/executors/src/executors/claude.rs` and demonstrates the “full” integration: command spawning, session follow-ups, rich log parsing, MCP config, and tool approvals.

### How Vibe Kanban launches Claude Code

By default, Vibe Kanban runs Claude Code via `npx`:

- Direct: `npx -y @anthropic-ai/claude-code@2.1.7`
- Router mode: `npx -y @musistudio/claude-code-router@1.0.66 code`

You can override this in a profile using `base_command_override` (via `CmdOverrides`), which is applied inside `ClaudeCode::build_command_builder(...)`.

### How prompts are sent

Vibe Kanban uses Claude Code’s stream JSON mode:

- `--output-format=stream-json`
- `--input-format=stream-json`

It spawns the process, sets up a control-protocol peer (`ProtocolPeer`), then sends a user message over stdin (`ProtocolPeer::send_user_message(...)`).

### How tool approvals work

Claude Code’s “can use tool?” events are handled over the stdio control protocol (`crates/executors/src/executors/claude/protocol.rs`).

When approvals are enabled in the profile:

- Vibe Kanban requests approval via `ExecutorApprovalService::request_tool_approval(...)`
- The decision is translated back into a `PermissionResult` and sent to Claude Code

### How follow-ups work

For follow-ups, Vibe Kanban calls `spawn_follow_up(...)` and passes:

- `--fork-session`
- `--resume <session_id>`

The `session_id` is extracted from Claude Code’s stream JSON output and stored in the message store.

### How availability is detected

Vibe Kanban treats Claude Code as “logged in” if `~/.claude.json` exists and has a readable modified timestamp (`ClaudeCode::get_availability_info()`).

### How MCP config is managed

For Claude Code, Vibe Kanban reads and writes MCP servers into `~/.claude.json` (see `ClaudeCode::default_mcp_config_path()` and `CodingAgent::get_mcp_config()`).

<Note>
Claude Code uses `mcpServers` as the root object for MCP server configuration. Vibe Kanban writes to that path for Claude Code and several other agents, while some agents (like Codex) require a different path and schema.
</Note>

## Common pitfalls when adding a new agent

- **No stable session ID**: you cannot support follow-ups without a durable session identifier.
- **Non-streaming output**: if the agent buffers output heavily, the UI will feel “stuck” until exit.
- **Incompatible MCP schema**: you may need to add an adapter in `crates/executors/src/mcp_config.rs`.
- **Missing executable**: if the base command cannot be resolved, you will get `ExecutableNotFound`; prefer `npx -y ...` or document installation clearly.
