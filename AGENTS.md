# Repository Guidelines

## Project Structure & Module Organization
- `crates/` contains the Rust workspace. Key crates: `server` (HTTP API/routes), `services` (business logic), `db` (models + migrations), `executors` (agent runners), `desktop` (Tauri app).
- `frontend/` is the main React + TypeScript web UI; shared UI logic lives under `frontend/src`.
- `frontend/mobile-ui/` is the mobile-focused frontend bundle.
- `shared/` stores cross-runtime TypeScript types and JSON schemas.
- `assets/` includes default configs, sounds, and packaging resources; `docs/` contains product documentation.

## Build, Test, and Development Commands
- `pnpm install --frozen-lockfile` — install JS dependencies.
- `pnpm dev` — run backend and frontend dev servers together.
- `pnpm run frontend:dev` / `pnpm run backend:dev` — run one side only.
- `pnpm run check` — run TypeScript and Rust compile checks.
- `pnpm run lint` — run ESLint + Clippy (`-D warnings`).
- `pnpm run format` — format frontend and backend code.
- `cargo test --workspace` — run Rust tests (including integration tests).
- `pnpm run desktop:dev` / `pnpm run desktop:build` — build and run the Tauri desktop app.

## Coding Style & Naming Conventions
- Frontend formatting is enforced by Prettier (`2` spaces, single quotes, semicolons, 80-char width).
- ESLint rules require:
  - PascalCase for most `.tsx` component files.
  - `use*.ts` camelCase for hooks.
  - kebab-case for files in `frontend/src/components/ui/`.
- Prefer strict TypeScript patterns (avoid `any`, keep exhaustive `switch` logic).
- Rust code must pass `cargo fmt --all` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

## Testing Guidelines
- Primary automated tests are Rust integration tests (see `crates/services/tests/`, e.g. `git_workflow.rs`).
- Add new backend behavior tests in the relevant crate’s `tests/` directory using feature-focused file names.
- For frontend changes, run `pnpm run frontend:check` and `pnpm run frontend:lint`; include manual UI verification notes for flows you changed.

## Commit & Pull Request Guidelines
- Follow conventional commit style seen in history: `feat: ...`, `fix: ...`, `refactor: ...`, `perf: ...`, `chore: ...`.
- Keep commit scopes small and functional; avoid mixing refactors with behavior changes.
- PRs should include: purpose, key files touched, verification commands run, and linked issue/task.
- Include screenshots or short recordings for UI changes (desktop or mobile) and note any config/migration impacts.
