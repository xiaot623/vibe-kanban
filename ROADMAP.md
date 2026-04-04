# Fork Roadmap

This document preserves the fork-specific roadmap and status checklist that previously lived in the root README.

> Status note: currently most directly tested for local environment usage on macOS.

## Stage One: feature for local environment `Local Version`

- [x] using agent cli in system path rather than npx
- [x] add start port configure by using "--port" flag
- [x] add proxy configure from web config && passthrough the running proxy to the agent cli
- [x] add trae/goland editor support
- [x] add native desktop app (using tauri)
- [x] add auto-switch color theme
- [x] refactor the native notice(using script before)
- [x] refactor the executor profile loading for better use
- [x] add integration telegram (better experience)
- [x] add powermode manage feature
- [x] add local network visting with auth
- [x] create a global context bus for sharing between different agent (P0)
- [x] chat mode support(daily task kanban) (P1)
- [x] skills manager (P0)
- [x] better experience of telegram bot(interactive mode refactor)
- [x] perf for long task log
- [x] migrate to acp protocol for opencode
- [ ] mobile app support with auto-detection
- [x] add support for more user-friendly pages to modify or submit a task (MDXEditor)
- [x] running log pages view by using telegraph
- [ ] ...

## Remove

- [x] beta_new_workspace(Just follow the state of your vibe-coding project, pure implementation)
- [x] remote collaboration feature(local first, run the executor remote will be implemented in the several weeks)

## Stage Two: feature for remote environment `Remote Version`

- [ ] FIFO queue to separate executor and "state log" (P2)
- [ ] command line && TUI running mode (P1)
- [ ] ssh connect tunnel
- [ ] master && slave config

## Stage Three: feature for cloud native environment `Distributed Version`

- [ ] todo
