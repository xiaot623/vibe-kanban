# ⚠️: IMPORTANT
**It's a fork of the original repository. Including unplanned features of the original repository.**
**Go to the origin repository for more information: https://github.com/BloopAI/vibe-kanban**

> currently only test for **local environment on macOS**

## To Install
1. Double-click the .dmg to mount it
2. Drag Vibe Kanban.app to /Applications
3. Before opening, run this in Terminal to clear the quarantine flag
    ```bash
    xattr -cr /Applications/Vibe\ Kanban.app
    ```
4. Now you can open the app normally

## Desktop CLI command (`kanban`)
- On first desktop launch, the app bootstraps `~/.kanban/bin` and installs:
  - `kanban` (default command)
  - `vibe-kanban` (compat alias)
- The launcher also adds a managed PATH block to `~/.zshrc` and `~/.bashrc`.
- If PATH changed, open a new terminal session before running commands.

```bash
kanban --server
kanban --server --port 8080
```

## Stage One: feature for local environment `Local Version`
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
- [x] create a global context bus for sharing between different agent (P0)
- [ ] chat mode and auto router to different agent (P1)
- [x] skills manager (P0)
- [ ] configure sync (by using git repo) (P1)
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

## Stage Two: feature for remote environment `Remote Version`
- [ ] todo

## Stage Three: feature for cloud native environment `Distributed Version`
- [ ] todo
