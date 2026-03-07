# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Vibe Kanban — a full-stack AI agent task management platform. React/TypeScript frontend, Rust/Axum backend, SQLite database, Tauri desktop app. Manages tasks that AI coding agents (Claude Code, Codex, etc.) execute in isolated workspaces.

## Build & Development Commands

```bash
pnpm install                    # Install dependencies
pnpm run dev                    # Run frontend + backend concurrently (ports auto-assigned)
pnpm run dev:qa                 # Dev mode optimised for QA testing (prefer this when testing changes)
pnpm run frontend:dev           # Frontend only
pnpm run backend:dev            # Backend only (cargo watch)
```

### Checks & Linting

```bash
pnpm run check                  # Both frontend (tsc --noEmit) and backend (cargo check)
pnpm run frontend:check         # TypeScript type-checking only
pnpm run backend:check          # Rust cargo check only
pnpm run lint                   # Both frontend (ESLint) and backend (cargo clippy)
pnpm run frontend:lint          # ESLint with zero warnings
pnpm run backend:lint           # cargo clippy --workspace -- -D warnings
pnpm run format                 # Format both (Prettier + cargo fmt)
```

### Testing

```bash
cargo test --workspace          # Run all Rust tests
cargo test -p <crate>           # Run tests for a specific crate (e.g., cargo test -p db)
```

### Code Generation & Database

```bash
pnpm run generate-types         # Regenerate shared/types.ts from Rust types (ts-rs)
pnpm run generate-types:check   # Check types are up to date (CI)
pnpm run prepare-db             # Prepare SQLx offline query data
```

### Building

```bash
pnpm run frontend:build         # Build frontend (tsc + vite)
pnpm run desktop:build          # Build Tauri desktop app (builds frontend first)
pnpm run build:npx              # Build for npm/npx distribution
```

## Architecture

### Layered Rust Backend (`crates/`)

```
crates/
├── server/          # Axum HTTP server, API routes, MCP server, entry point
│   └── src/routes/  # Route handlers: tasks, projects, skills, approvals, sessions, etc.
├── db/              # SQLite via sqlx — models, migrations, task state machine
│   ├── src/models/  # Per-entity modules (task, workspace, project, repo, session, etc.)
│   └── migrations/  # Sequential SQL migrations
├── services/        # Business logic layer (container, git, workspace_manager)
├── executors/       # Agent executor runtime and protocol bindings
├── utils/           # Shared utilities
├── review/          # Code review/approval functionality
├── deployment/      # Deployment configuration
├── local-deployment/ # Local deployment specifics
└── desktop/         # Tauri desktop app shell
```

Request flow: **Routes** (crates/server/src/routes/) → **Services** (crates/services/) → **Database** (crates/db/)

### React Frontend (`frontend/`)

- **Components**: `src/components/` — organised by domain (agents, dialogs, layout, panels, tasks, ui)
- **Hooks**: `src/hooks/` — extensive custom hooks for API calls and state logic
- **Contexts**: `src/contexts/` — ProjectContext, SearchContext, etc.
- **Stores**: `src/stores/` — Zustand state management
- **Pages**: `src/pages/` — Projects, ProjectTasks, Settings

**Component patterns**:
- `views/` — stateless, receive data via props
- `containers/` — manage state, pass to views
- `ui-new/` — reusable primitives (PascalCase filenames)

### Shared Types

`shared/types.ts` is auto-generated from Rust types using ts-rs. Edit `crates/server/src/bin/generate_types.rs` to change types, then run `pnpm run generate-types`. Never edit `shared/types.ts` directly.

### Core Domain Model

Tasks progress through states: **todo → inprogress → inreview → done / cancelled**. Each task can have multiple **Workspaces** (execution attempts), which contain **Sessions** (user/agent sessions), which contain **Execution Processes** (individual process runs with JSONL logs).

### REST API

All endpoints under `/api/` — health, projects, tasks, task-attempts, execution-processes, skills, approvals, sessions, config, events, terminal.

## Coding Conventions

**Rust**: `rustfmt` enforced; snake_case modules, PascalCase types; group imports by crate; add `Debug`/`Serialize`/`Deserialize` derives where useful.

**TypeScript/React**: ESLint + Prettier (2 spaces, single quotes, 80 cols); PascalCase components, camelCase variables/functions.

### Frontend Design System

The new design uses CSS variables defined in `src/styles/new/index.css` and `tailwind.new.config.js`, scoped to `.new-design`. Use semantic color tokens (`text-high`, `text-normal`, `text-low`, `bg-primary`, `bg-secondary`, `bg-panel`, `brand`) instead of raw Tailwind colors. Custom spacing: `p-half` (6px), `p-base` (12px), `p-double` (24px). Font sizes are smaller than Tailwind defaults (`text-base` = 12px).

## Environment

- Node >= 18, pnpm >= 8 (packageManager: pnpm@10.13.1)
- Rust stable toolchain
- Dev ports managed by `scripts/setup-dev-environment.js`
- Key env vars: `FRONTEND_PORT`, `BACKEND_PORT`, `HOST`
- Use `.env` for local overrides
