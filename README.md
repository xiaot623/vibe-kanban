# ⚠️: IMPORTANT
**It's a fork of the original repository. Including unplanned features of the original repository.**
**Go to the origin repository for more information: https://github.com/BloopAI/vibe-kanban**

> currently only test for **local environment on macOS**

## RoadMap
- [x] using agent cli in system path rather than npx
- [x] add start port configure by using "--port" flag
- [x] add proxy configure from web config && passthrough the running proxy to the agent cli
- [x] add trae/goland editor support
- [x] add native desktop app (using tauri)
- [x] add auto-switch color theme
- [x] refactor the native notice(using script before) 
- [x] refactor the executor profile loading for better use
- [x] add integration telegram
- [x] add powermode manage feature
- [x] add local network visting with auth 
- [ ] better experience of telegram bot(interactive mode refactor)
- [ ] perf for long task log
- [ ] acp protocol wrapper to link with any agent
- [ ] mobile app support with auto-detection
- [ ] add "dangerously_skip_permissions" and "plan" setting when create a task
- [ ] add web terminal support && open workspace in terminal(such as iterm2)
- [ ] add support for more user-friendly pages to modify or submit a task
- [ ] ...

## remove
- [x] beta_new_workspace(Just follow the state of your vibe-coding project, pure implementation)
- [x] remote collaboration feature(local first, run the executor remote will be implemented in the several weeks)
