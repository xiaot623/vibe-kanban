# Rust 后端到 Go + Electron 重写计划

## Summary

- 完全重写后端为 Go，不做 Rust 逐行翻译；前端 HTTP/WS/SSE 契约保持兼容。
- 最终形态：Go 后台服务支持 desktop 内嵌启动和 headless server；Electron 取代 Tauri；彻底移除移动端。
- 现有用户数据原地兼容：继续读写 ~/.kanban/db.sqlite、config.json、profiles.json、credentials.json、图片/assets 路径和临时 port file。(开发环境保持 dev_assets 兼容)

## Key Changes

- 新建 Go 后端：
    - cmd/vibe-kanban：headless server 入口，支持 --port、BACKEND_PORT、
      PORT、随机端口和 port file。
    - internal/httpapi：REST、WebSocket、SSE、multipart、静态前端托管、
      local network Basic auth。
    - internal/app：项目、任务、workspace/session/execution、配置、skills、
      images、scratch、cron、approvals。
    - internal/db：SQLite 连接、迁移、查询层；保持现有 schema，UUID 继续按
      SQLite BLOB 兼容读写。
    - internal/executors：Claude/Codex/Gemini/OpenCode/Pi 进程管理、日志归
      一化、approval、cancel/kill、PTY。
    - internal/git：继续 shell out 到 git CLI，不切到 go-git。
- Telegram 是核心功能，不降级：
    - 用 github.com/go-telegram/bot (https://github.com/go-telegram/bot)
      作为唯一 Telegram Bot API 库；该库支持 polling/webhook、message/call
      back handlers、middlewares、workers、file upload、Bot API 方法封装。
    - Go 版必须完整迁移当前 Telegram 配置、交互 bot、inline keyboard
      callbacks、pending approvals、task creation、pin project、flow
      binding、run feed watcher、session receipt、TTS audio jobs、
      Telegraph、Lark wiki 集成。
    - 保持 TelegramConfig JSON 字段兼容：enabled、bot_token、chat_id、
      send_session_receipt、telegraph_*、lark_wiki_*、default_executor、
      default_mode。
- Electron 桌面：
    - Electron main 启动 Go binary 子进程，等待 /api/health 后加载
      http://127.0.0.1:{port}。
    - preload 暴露最小桌面桥，替代当前 Tauri save_export_context_file。
    - 删除 frontend/mobile-ui/、Android discovery、Tauri desktop/mobile 配
      置和 Cargo 桌面 crate。
- 构建脚本：
    - pnpm dev 启动 Go backend + Vite frontend。
    - backend:dev/check/lint/test 改为 Go 命令。
    - desktop:dev/build 改为 Electron builder，打包 frontend dist + Go
      binary。
    - 移除 mobile 相关 npm scripts。

## Public Interfaces

- HTTP API 保持不破坏：保留 /api/* 路径、方法、query/body、状态码习惯和
  ApiResponse<T, E>：success, data, error_data, message。
- 保留 WebSocket/SSE 契约：projects/tasks stream、execution logs、workspace
  diff、scratch stream、/api/events 的消息格式与初始 snapshot 行为。
- 保留 multipart 图片接口、/api/images/{id}/file、/api/sounds/{sound}、静态
  frontend fallback。
- 保留 per-project OpenAI-compatible API：/v1/models、/v1/chat/
  completions，包括 streaming SSE。
- shared/types.ts 先作为冻结兼容契约；Go structs 必须用 JSON tags 对齐字段
  名、enum 值、nullable/optional 语义和时间字符串格式。

- Contract tests：用 Rust 基线 fixtures 对 Go 响应做 JSON 等价校验，覆盖
  REST、typed errors、WS/SSE、multipart、OpenAI streaming。
- DB compatibility tests：Go 打开旧 db.sqlite golden file，验证 UUID BLOB、
  datetime、JSON 字段和迁移行为。
- Telegram tests：
    - 用 go-telegram/bot 的 handler 层做 fake update 单元测试，覆盖 slash
      commands、callback_data 编解码、chat_id 限制、dialogue 状态、approval
      按钮、audio job cancel。
    - 用 mock Bot API HTTP server 做集成测试，验证 send/edit/delete
      message、inline keyboard、file upload、polling shutdown。
    - 用旧 Rust callback fixtures 验证 Go callback decoder 兼容 64-byte
      callback_data 格式。
- Git/executor tests：复刻现有 git_workflow、git_ops_safety、filesystem
  discovery 场景。
- Desktop smoke tests：Electron dev/build 后验证启动 Go 子进程、退出清理、
  导出 context、headless vibe-kanban server --port。

## Assumptions

- 目标平台只保留桌面和 headless server；移动端、Android discovery、Tauri
  mobile 不迁移。
- Telegram 集成是必需功能，不允许作为后续补齐项。
- 数据必须原地兼容，不要求用户导出/导入。
- 允许前端做少量 Electron bridge 适配，但不改变用户可见 UI 和 HTTP API 消费
  方式。
- 大分支重写期间 Rust 仅作为契约与行为参考。

---

# 功能点对齐清单

下文按领域列出当前 Rust 版本的完整功能点，Go 迁移需逐项覆盖。每项仅描述"做什么"，不涉及实现方案。

---

## 1. HTTP 路由层 (internal/httpapi)

### 1.1 前端托管
- [ ] `GET /` → 返回前端 index.html
- [ ] `GET /{*path}` → SPA fallback，非 API 路径返回 index.html
- [ ] 静态资源代理（Vite dev 或 dist 目录）

### 1.2 Health
- [ ] `GET /api/health` → 返回服务健康状态

### 1.3 配置 (config)
- [ ] `GET /api/info` → 返回 UserSystemInfo（含 config、环境信息、executor 能力列表、config_parse_error）
- [ ] `PUT /api/config` → 全量更新 config.json；校验 Telegram/Lark 配置规范化
- [ ] `GET /api/sounds/{sound}` → 返回音频文件（.wav/.mp3），支持 Cache-Control
- [ ] `GET /api/mcp-config?executor=X` → 读取指定 executor 的 MCP 配置文件
- [ ] `POST /api/mcp-config` → 写入 MCP 服务器配置
- [ ] `GET /api/profiles` → 返回所有 executor profiles
- [ ] `PUT /api/profiles` → 保存 executor profiles，同步规范化的 variant key
- [ ] `GET /api/editors/check-availability?editor_type=X` → 检查编辑器是否可用（cursor / windsurf / vscode）
- [ ] `GET /api/agents/check-availability?executor=X` → 检查 coding agent 是否安装/登录
- [ ] `GET /api/lark/check-cli` → 检查 lark-cli 是否安装

### 1.4 项目 (projects)
- [ ] `GET /api/projects` → 列出所有项目
- [ ] `GET /api/projects/stream` → WebSocket streaming：项目增删改时推送当前 snapshot
- [ ] `GET /api/projects/{project_id}` → 获取单个项目（middleware 自动 load）
- [ ] `POST /api/projects` → 创建项目 + 关联 repos；校验重复 git 路径/仓库名
- [ ] `PUT /api/projects/{project_id}` → 更新项目名称
- [ ] `DELETE /api/projects/{project_id}` → 删除项目及关联 repos
- [ ] `GET /api/projects/{project_id}/open-editor?editor_type=X&file_path=X` → 打开编辑器（Cursor/Windsurf/VSCode）
- [ ] `GET /api/projects/{project_id}/search?query=X&project_id=X&...` → 项目内文件搜索（基于 file_search cache）

### 1.5 任务 (tasks)
- [ ] `GET /api/tasks?project_id=X` → 列出项目下任务（含 attempt status）
- [ ] `GET /api/tasks/stream?project_id=X` → WebSocket streaming：任务增删改时推送
- [ ] `GET /api/tasks/{task_id}` → 获取单个任务
- [ ] `POST /api/tasks` → 创建任务；支持关联图片、optional expected_status CAS 校验
- [ ] `PUT /api/tasks/{task_id}?expected_status=X` → 更新任务（标题、描述、状态、关联图片）；CAS 防并发
- [ ] `DELETE /api/tasks/{task_id}` → 删除任务
- [ ] `GET /api/tasks/{task_id}/relationships` → 获取父子任务关系链

### 1.6 任务执行 (task_attempts)
- [ ] `POST /api/task-attempts/create-and-start` → 创建 workspace + 开始 coding agent 执行
- [ ] `POST /api/task-attempts` → 为已有任务创建 workspace 并开始执行
- [ ] `GET /api/task-attempts/status?task_id=X` → 获取任务执行状态（是否有正在运行的 attempt）
- [ ] `POST /api/task-attempts/run-agent-setup` → 针对指定 executor 运行 setup helper（如 Codex）
- [ ] `POST /api/task-attempts/{workspace_id}/merge` → 合并 workspace 更改到目标分支
- [ ] `POST /api/task-attempts/{workspace_id}/push?repo_id=X` → push 分支到 remote
- [ ] `POST /api/task-attempts/{workspace_id}/change-target-branch` → 切换 worktree 目标分支
- [ ] `POST /api/task-attempts/{workspace_id}/create-pr` → 创建 Pull Request（GitHub/Azure DevOps）
- [ ] `POST /api/task-attempts/{workspace_id}/attach-existing-pr` → 关联已存在的 PR
- [ ] `GET /api/task-attempts/{workspace_id}/pr-comments?repo_id=X` → 获取 PR review comments
- [ ] `POST /api/task-attempts/{workspace_id}/rebase` → rebase worktree
- [ ] `POST /api/task-attempts/{workspace_id}/abort-conflicts` → 解决冲突 abort
- [ ] `GET /api/task-attempts/{workspace_id}/branch-status?repo_id=X` → 分支状态（ahead/behind/uncommitted/conflicts/merges）
- [ ] `GET /api/task-attempts/{workspace_id}/workspace-summary?archived=bool` → workspace 摘要（files changed/lines/diff stats）
- [ ] `POST /api/task-attempts/{workspace_id}/rename-branch` → 重命名分支
- [ ] `GET /api/task-attempts/{workspace_id}/diff` → SSE streaming workspace diff
- [ ] `POST /api/task-attempts/{workspace_id}/run-script?type=X` → 运行 setup/cleanup/dev_server 脚本
- [ ] `POST /api/task-attempts/{workspace_id}/run-repo-setup` → 运行仓库 setup 脚本

### 1.7 会话 (sessions)
- [ ] `GET /api/sessions?workspace_id=X` → 列出 workspace 下所有 session
- [ ] `GET /api/sessions/{session_id}` → 获取单个 session
- [ ] `POST /api/sessions` → 创建 session（指定 executor）
- [ ] `POST /api/sessions/{session_id}/follow-up` → 创建 follow-up 执行进程
  - 支持 retry（替换旧 process）、force_when_dirty、perform_git_reset
- [ ] `POST /api/sessions/{session_id}/start-summary` → 生成 workspace 摘要？
- [ ] `POST /api/sessions/{session_id}/review` → 创建 code review 进程
  - 支持 review_repos 上下文（base_commit → HEAD）
  - 支持 use_all_workspace_commits 模式

### 1.8 执行进程 (execution_processes)
- [ ] `GET /api/execution-processes?session_id=X&show_soft_deleted=bool` → 列出 session 的执行进程
- [ ] `GET /api/execution-processes/{process_id}` → 获取单个进程
- [ ] `POST /api/execution-processes/{process_id}/cancel` → 取消/终止进程
- [ ] `POST /api/execution-processes/{process_id}/kill` → 强制 kill 进程
- [ ] `GET /api/execution-processes/{process_id}/stream?after_seq=X&before_seq=X&limit=X` → SSE streaming 规范化日志事件
  - 支持 cursor 分页（after_seq/before_seq/limit）
  - 历史回放 + 实时追加
  - 日志格式：NormalizedLogEvent（Upsert/Remove/Finished），含 seq、tool_status、action_type
- [ ] `GET /api/execution-processes/{process_id}/stream-ws` → WebSocket 版日志 streaming
- [ ] `GET /api/execution-processes/{process_id}/repo-states` → 获取进程关联的 repo git 状态

### 1.9 审批 (approvals)
- [ ] `POST /api/approvals/{id}/respond` → 响应审批请求（approved/denied/provided_input）

### 1.10 标签 (tags)
- [ ] `GET /api/tags?search=X` → 列出所有标签，支持模糊搜索
- [ ] `POST /api/tags` → 创建标签
- [ ] `PUT /api/tags/{tag_id}` → 更新标签
- [ ] `DELETE /api/tags/{tag_id}` → 删除标签

### 1.11 仓库 (repo)
- [ ] `POST /api/repos/register` → 注册本地 git 仓库（路径 + display_name）
- [ ] `POST /api/repos/init` → 在指定路径初始化新 git 仓库
- [ ] `GET /api/repos` → 列出所有已注册仓库
- [ ] `PUT /api/repos/{repo_id}` → 更新仓库元数据（display_name、setup_script、cleanup_script、copy_files 等）
- [ ] `DELETE /api/repos/{repo_id}` → 删除仓库注册
- [ ] `GET /api/repos/branches?path=X` → 获取仓库本地分支列表
- [ ] `GET /api/repos/remote-branches?path=X` → 获取仓库 remote 分支列表
- [ ] `GET /api/repos/current-branch?path=X` → 获取仓库当前分支名
- [ ] `GET /api/repos/git-host` → 检测当前目录的 git host provider（GitHub/Azure DevOps）
- [ ] `POST /api/repos/batch` → 批量操作仓库（预留接口）

### 1.12 文件系统 (filesystem)
- [ ] `GET /api/filesystem/list-directory?path=X` → 列出目录内容（含 .git 过滤、排序）
- [ ] `GET /api/filesystem/list-git-repos?path=X` → 递归扫描目录中的 git 仓库（depth limited）
- [ ] `GET /api/filesystem/list-common-git-repos` → 扫描常见开发目录（~、~/dev、~/projects 等）

### 1.13 图片 (images)
- [ ] `POST /api/images/upload` → multipart 上传图片（20MB 限制）；SHA256 去重
- [ ] `POST /api/images/task/{task_id}/upload` → 上传图片并关联到任务
- [ ] `GET /api/images/{id}/file` → 返回图片文件（含 mime type + 1年 Cache-Control）
- [ ] `DELETE /api/images/{id}` → 删除图片
- [ ] `GET /api/images/task/{task_id}` → 列出任务关联图片
- [ ] `GET /api/images/task/{task_id}/metadata?path=X` → 查询图片元信息（路径解析、proxy URL）

### 1.14 草稿 (scratch)
- [ ] `GET /api/scratch` → 列出所有 scratch 条目
- [ ] `GET /api/scratch/{type}/{id}` → 获取单个 scratch（type ∈ {DRAFT_TASK, DRAFT_FOLLOW_UP, DRAFT_WORKSPACE, PREVIEW_SETTINGS}）
- [ ] `PUT /api/scratch/{type}/{id}` → 创建或更新 scratch（upsert 语义）
  - DRAFT_FOLLOW_UP 类型：写前检查是否有 queued_message，有则拒绝
- [ ] `GET /api/scratch/stream` → WebSocket streaming scratch 变更
- [ ] `DELETE /api/scratch/{type}/{id}` → 删除 scratch

### 1.15 事件流 (events)
- [ ] `GET /api/events` → SSE 统一事件流：合并所有项目/任务/workspace 变更事件
  - 初始 snapshot + 后续增量 patches
  - KeepAlive 心跳

### 1.16 终端 (terminal)
- [ ] `GET /api/terminal?workspace_id=X&cols=80&rows=24` → WebSocket PTY 终端
  - 双向通信：stdin/stdout/stderr
  - resize 支持（cols/rows）
  - 连接生命周期管理（断开即 kill PTY）

### 1.17 容器 (containers)
- [ ] `GET /api/containers/info?ref=X` → 通过 container_ref 前缀匹配解析 workspace 信息

### 1.18 Skills
- [ ] `GET /api/skills` → 列出规范 skills 目录下的所有 SKILL.md（扫描子目录，解析 name/description）
- [ ] `DELETE /api/skills` → 删除指定规范 skill
- [ ] `GET /api/skills/links?executor=X` → 列出 agent 的 skill 链接状态（linked/not-linked/legacy）
- [ ] `POST /api/skills/links` → 为指定 executor 链接 skills（symlink）
- [ ] `DELETE /api/skills/links` → 取消链接指定 skill
- [ ] `POST /api/skills/import` → 从 git 仓库导入 skills
  - 支持 source URL、git_ref、subpath、skill_filter
  - 返回 imported/skipped/warnings

### 1.19 OAuth (在迁移中移除，包括前端)
- [ ] `POST /api/auth/logout` → 清除 credentials 和 profile

### 1.20 OpenAI 兼容 API (per-project)
- [ ] `GET /api/v1/models` → 返回可用模型列表
- [ ] `POST /api/v1/chat/completions` → 创建 chat completion
  - 自动创建 task → workspace → session → execution_process
  - 复用 ExecutionLogHub 进行日志写入
  - 支持 streaming SSE（`stream: true`）
  - 支持 queued_message 等待 + 队列防重
  - 支持 fork 模式（`fork_session_id` / `fork_process_id`）
  - 支持 approval 桥接：executor 产生的 approval 请求通过该接口对外暴露
  - 自动项目路由：根据 X-Vibe-Project-Id header 或路径前缀定位项目
- [ ] 每项目独立端口监听 + 项目级 API key 认证

### 1.21 MCP HTTP Service (在迁移中移除，包括前端，迁移后不包含任何 MCP 相关)
- [ ] 启动 MCP HTTP 服务（用于 Claude/Gemini 等 agent 的 MCP 配置端点）
- [ ] MCP task server：提供 tool discovery、task CRUD、execution trigger

### 1.22 前端
- [ ] `GET /` 和 `GET /{path}` → SPA 模式静态文件服务
- [ ] 开发模式下代理 Vite dev server

### 1.23 Middleware
- [ ] load_project_middleware → 通过 {project_id} 路径参数预加载 Project
- [ ] load_task_middleware → 通过 {task_id} 路径参数预加载 Task
- [ ] load_session_middleware → 通过 {session_id} 路径参数预加载 Session
- [ ] load_execution_process_middleware → 通过 {process_id} 路径参数预加载 ExecutionProcess
- [ ] load_tag_middleware → 通过 {tag_id} 路径参数预加载 Tag
- [ ] local_network_auth → 非 loopback 请求要求 Basic Auth（可配置）

---

## 2. 数据库层 (internal/db)

### 2.1 核心数据模型
- [ ] Project — id(UUID BLOB), name, default_agent_working_dir, timestamps
- [ ] ProjectRepo — 多对多关联 project ↔ repo
- [ ] Repo — id, path, name, display_name, setup_script, cleanup_script, copy_files, parallel_setup_script, dev_server_script, timestamps
- [ ] Task — id, project_id, title, description, status(todo/inprogress/inreview/done/cancelled), parent_workspace_id, source_cron_task_id, diff stats, timestamps
- [ ] Workspace — id, task_id, container_ref, branch, agent_working_dir, setup_completed_at, archived, pinned, name, timestamps
- [ ] WorkspaceRepo — workspace ↔ repo + target_branch
- [ ] Session — id, workspace_id, executor, timestamps
- [ ] ExecutionProcess — id, session_id, run_reason(setupscript/cleanupscript/codingagent/devserver), executor_action(JSON), status(running/completed/failed/killed), exit_code, dropped(soft delete), timestamps
- [ ] ExecutionProcessLogs — 进程日志持久化（normalized events）
- [ ] ExecutionProcessRepoState — 进程前后 HEAD commit 快照
- [ ] ExecutionProcessTelegraphPage — Telegraph 页面关联
- [ ] ExecutionProcessLarkWikiDoc — 飞书文档关联
- [ ] ExecutionProcessLarkWikiMonthNode — 飞书月度节点关联
- [ ] CodingAgentTurn — agent 对话轮次记录（create/continue/review 等 action type）
- [ ] Image — id, file_path, original_name, mime_type, size_bytes, hash(SHA256), timestamps
- [ ] TaskImage — 任务 ↔ 图片多对多关联
- [ ] Tag — id, tag_name, content, timestamps
- [ ] Scratch — id(复合主键 scratch_type + uuid), payload(JSON), timestamps
- [ ] TelegramFlowBinding — flow_token ↔ session/workspace/task 映射
- [ ] Merge — direct merge 或 PR merge 记录

### 2.2 数据库操作
- [ ] SQLite 连接池管理（sqlx 或等价方案）
- [ ] Schema 迁移（向前兼容：现有 db.sqlite 可直接打开）
- [ ] UUID 读写兼容：SQLite BLOB(16) 格式，Go 侧与 Rust uuid 格式互通
- [ ] 时间字段兼容：Rust chrono DateTime<Utc> ↔ Go time.Time，统一 RFC3339 字符串存储
- [ ] JSON 字段读写：executor_action、payload 等保持 JSON 文本列
- [ ] 事务支持

### 2.3 任务状态机
- [ ] TaskState 注册表：注册状态迁移 handler
- [ ] 状态迁移 dispatching：根据 from_status + to_status 查找 handler
- [ ] 内置迁移：todo↔inprogress↔inreview↔done，支持 cancelled 终态
- [ ] 迁移 hook：状态变更时触发 side effects（通知、workspace 操作等）

---

## 3. 执行引擎 (internal/executors)

### 3.1 支持的 Coding Agent
- [ ] Claude Code (claude) — `claude` CLI 进程管理
  - spawn / spawn_follow_up：支持 --session-id 恢复
  - 日志规范化（ansi 解析、tool_use 事件提取）
  - MCP 配置注入（.mcp.json）
  - 可用性检测：which claude + claude mcp list
- [ ] Gemini CLI (gemini) — `gemini` CLI 进程管理
  - spawn / spawn_follow_up：支持 --resume 会话
  - 日志规范化
  - MCP 配置注入（.gemini/settings.json）
  - 可用性检测
- [ ] Codex (codex) — OpenAI Codex CLI
  - spawn / spawn_follow_up：支持 --resume
  - Setup Helper：首次运行前自动执行 codex setup
  - 日志规范化（JSON-RPC 事件格式）
  - MCP 配置注入（.codex/config.toml）
  - 可用性检测
- [ ] OpenCode (opencode) — `opencode` CLI
  - spawn / spawn_follow_up：支持 session fork
  - 日志规范化
  - Plan mode 支持（--plan 参数）
  - MCP 配置注入（opencode.json）
  - 可用性检测
- [ ] Pi (pi) — pi-coding-agent
  - spawn / spawn_follow_up：session 管理
  - 日志规范化
  - MCP 配置注入
  - 可用性检测

### 3.2 进程管理
- [ ] 子进程 spawn：command_group AsyncGroupChild（进程组管理，可 kill 整棵树）
- [ ] 环境变量注入：ExecutionEnv（含 API keys、proxy、MCP port、project context）
- [ ] exit_signal 机制：executor 通知 container 已正常完成
- [ ] interrupt_sender 机制：container 通知 executor 优雅中断
- [ ] 进程状态跟踪：running → completed/failed/killed
- [ ] 启动时清理孤儿进程（cleanup_orphan_executions）

### 3.3 日志处理
- [ ] stdout/stderr 分流捕获
- [ ] 日志规范化 pipeline：原始输出 → NormalizedLogEvent（Upsert/Remove/Finished）
  - 每个 agent 独立 normalize_logs 实现
  - 事件类型：ActionType（shell_command, coding_agent 等）
  - Tool status：running/success/error
  - Tool result value type 推断
- [ ] 日志持久化：ExecutionProcessLogs + ExecutionLogHub（内存 + DB 双重存储）
- [ ] 日志 streaming：支持 cursor 分页回放 + 实时追加

### 3.4 审批系统
- [ ] executor 产生 approval 请求 → 通过 MsgStore 通知前端/Telegram
- [ ] PendingApprovalInfo：id、tool_name、tool_input、execution_process_id、structured_input 标记
- [ ] 响应路由：Web UI 和 Telegram 双向同步
- [ ] 超时处理

### 3.5 Profile & Variant
- [ ] ExecutorProfileId：profile + variant 组合
- [ ] 规范化的 variant key（去空格、lowercase）
- [ ] ExecutorConfigs：每个 executor 的配置集合
- [ ] canonical_variant_key 映射

---

## 4. Git 操作 (internal/git)

### 4.1 基础操作
- [ ] git CLI shell out（不切换到 go-git）
- [ ] 仓库注册/发现：扫描目录递归找 .git
- [ ] 分支列表：本地 + remote
- [ ] 当前分支查询
- [ ] Git host 检测：GitHub / Azure DevOps / Unknown

### 4.2 Worktree 管理
- [ ] `git worktree add` — 创建隔离 worktree
- [ ] `git worktree remove` — 清理 worktree
- [ ] `git worktree list` — 列出活跃 worktree
- [ ] before_head_commit 快照：记录进程开始时的 HEAD

### 4.3 分支操作
- [ ] 分支创建（基于 target branch）
- [ ] 分支切换（change target branch）
- [ ] 分支重命名
- [ ] 分支状态查询：ahead/behind、uncommitted changes、untracked files、conflict 状态
- [ ] rebase 支持（含 rebase_in_progress 检测）
- [ ] merge 支持（含 conflict 检测、abort）

### 4.4 Remote 操作 (在迁移中移除，包括前端)
- [ ] push 到 remote（含 force push 检测提示）
- [ ] remote 分支列表
- [ ] remote tracking 状态（ahead/behind remote）

### 4.5 PR/Merge 集成 (在迁移中移除，包括前端)
- [ ] GitHub CLI (`gh`) 集成
  - 创建 PR
  - 查询 PR 状态
  - 查询 PR review comments（general + review threads）
  - 关联已有 PR
  - CLI 可用性检测（brew/install/login 状态）
- [ ] Azure DevOps CLI (`az`) 集成
  - 创建 PR
  - 查询 PR 状态

### 4.6 Diff
- [ ] `git diff` 生成 workspace 变更
- [ ] Diff 内容大小限制（超限标记 contentOmitted + stats）
- [ ] SSE streaming 推送 diff

### 4.7 Merge 记录 (在迁移中移除，包括前端)
- [ ] DirectMerge：记录 merge commit + target branch
- [ ] PrMerge：记录 PR info（number/url/status/merged_at/merge_commit_sha）
- [ ] Merge 查询（关联到 workspace + repo）

---

## 5. 工作空间管理

### 5.1 Workspace 生命周期
- [ ] 创建 workspace（关联 task + repos）
- [ ] container_ref 生成（基于 branch 名 + short uuid）
- [ ] worktree 创建（每个 repo 一个 worktree）
- [ ] 仓库 setup 脚本执行（可并行）
- [ ] 仓库 cleanup 脚本执行
- [ ] 文件复制（copy_files）
- [ ] workspace 归档/取消归档
- [ ] workspace 置顶/取消置顶
- [ ] workspace 重命名
- [ ] workspace 摘要（files changed/lines added/lines deleted）

### 5.2 Session 管理
- [ ] session 创建（关联 workspace）
- [ ] follow-up 对话管理
- [ ] retry 支持：替换旧 process（soft-delete 旧 process，关联新 process）
- [ ] CodingAgentTurn 记录：跟踪每轮对话

### 5.3 容器操作
- [ ] 确保容器存在（container_ref 查找或创建）
- [ ] 容器信息查询（从 container_ref 前缀反查 project/task/attempt）

---

## 6. Telegram 集成 (internal/telegram)

### 6.1 Bot 核心
- [ ] Polling / Webhook 两种连接模式
- [ ] chat_id 限制：仅响应配置的 chat_id
- [ ] Slash Commands：
  - /start — 首页（含 pin 状态显示）
  - /help — 帮助信息
  - /tasks — 浏览项目任务
  - /new — 创建新任务（交互式流程）
  - /pending — 列出待审批项
  - /cancel — 取消当前操作
  - /pin — 置顶项目
  - /unpin — 取消置顶

### 6.2 交互式对话 (Interactive Bot)
- [ ] DialogueState 状态机：管理多步对话流程
  - Idle → 等待用户选择项目 → 填写标题/描述 → 确认创建
- [ ] Inline Keyboard Callbacks（callback_data 编解码）：
  - 首页：Tasks / New / Pending / Pin/Unpin
  - 项目列表：分页浏览 + 直接选择
  - 任务列表：按状态过滤、分页
  - 任务详情：查看描述、状态切换
  - 审批：Approve / Deny 按钮
  - 创建任务：选择项目 → 输入标题 → 输入描述
  - 置顶流程：选择项目 → 确认
- [ ] Card 消息模式：inline keyboard 控制在同一消息上更新
- [ ] 64-byte callback_data 格式兼容

### 6.3 任务创建
- [ ] 通过 Telegram 创建任务 → 复用 Go 后端 CreateTask API
- [ ] 支持 pin project：快速任务创建无需每次选择项目
- [ ] 支持配置 default executor + default mode（TelegramConfig 字段）

### 6.4 Flow Binding
- [ ] 为每个 session 创建 flow_token → TelegramFlowBinding
- [ ] 通过 flow_token 查询/恢复 session 上下文
- [ ] Run feed watcher：异步监听 execution 日志并推送到 Telegram
- [ ] Session receipt：执行完成后发送摘要到 Telegram

### 6.5 审批通知
- [ ] 执行过程中产生的 approval 请求实时推送到 Telegram
- [ ] 支持 Approve / Deny 操作（从 Telegram 按钮回调）
- [ ] 需要 structured_input 的审批：提示用户前往 Web UI
- [ ] 审批状态同步：Telegram ↔ Web UI

### 6.6 Telegraph 集成
- [ ] TelegraphMirrorConfig 从 TelegramConfig 读取
- [ ] 执行日志自动同步到 Telegraph 页面
  - Markdown 格式化（标题层级、代码块、表格）
  - 分页：每 120 条 entry 创建新 Telegraph page
  - 字符数限制：28,000 chars/page、256 chars/title
  - 每 2 秒 flush 间隔
- [ ] Telegraph API：createAccount、createPage、editPage
- [ ] 页面链接存储到 ExecutionProcessTelegraphPage

### 6.7 Lark Wiki 集成
- [ ] LarkWikiMirrorConfig 从 TelegramConfig 读取
- [ ] 通过 lark-cli 工具创建/更新飞书文档
- [ ] 按月组织结构：每月创建 month node，下挂执行记录
- [ ] 执行日志转换为飞书 DocxXML 格式
- [ ] 文档链接存储到 ExecutionProcessLarkWikiDoc/MonthNode
- [ ] lark-cli 可用性检测

### 6.8 TTS 音频任务 (在迁移中移除，包括前端)
- [ ] TtsConfig：provider (Replicate) + model + api_token
- [ ] 文字合成语音（调用 Replicate API）
- [ ] 音频文件下载到本地
- [ ] 通过 Telegram 发送音频文件
- [ ] Cancel 支持（CancellationToken）
- [ ] Audio job 管理：创建/取消/状态跟踪

### 6.9 通知服务
- [ ] Notifier trait：通用通知接口
- [ ] Sound 通知：根据平台调用 afplay(macOS) / paplay(Linux) / PowerShell(Windows)
- [ ] SSE 推送通知到 Web 客户端
- [ ] SoundFile 缓存管理

---

## 7. 配置管理

### 7.1 config.json
- [ ] Config v2 结构：包含所有配置项
  - General settings（theme、editor、daily_mode 等）
  - GeminiConfig（api_key）
  - ProxyConfig（http/https/no_proxy）
  - TelegramConfig（enabled、bot_token、chat_id、telegraph_*、lark_wiki_*、default_executor、default_mode）
  - McpServerConfig（enabled、port）
  - NotificationConfig（push_enabled、sound_enabled、sound_file）
  - TtsConfig（provider、model、replicate_*）
  - LocalNetworkConfig（access 开关、password）
  - ProjectOpenAiApiConfig（per-project enabled/port）
  - EditorConfig（default、custom）
  - AnalyticsConfig（enabled）
- [ ] 配置加载/保存：全量读写 config.json
- [ ] 配置版本迁移：v1 → v2
- [ ] 配置字段规范化：Telegram 空字符串 → null
- [ ] 配置变更自动保存

### 7.2 profiles.json
- [ ] ExecutorConfigs：每个 executor 的 profile 列表
- [ ] ExecutorConfig：profile_id + variant + 各 agent 的特定配置
- [ ] MCP server 配置独立文件（每个 agent 有自己的 config 路径）

### 7.3 credentials.json
- [ ] OAuth credentials 持久化
- [ ] 读写/清除操作

### 7.4 环境变量
- [ ] 开发模式通过 dev_assets 目录加载
- [ ] 生产模式通过 ~/.kanban/ 目录加载

### 7.5 路径约定
- [ ] ~/.kanban/ — 数据根目录
- [ ] ~/.kanban/db.sqlite — SQLite 数据库
- [ ] ~/.kanban/config.json — 主配置
- [ ] ~/.kanban/profiles.json — executor profiles
- [ ] ~/.kanban/credentials.json — OAuth 凭证
- [ ] ~/.kanban/images/ — 图片存储
- [ ] ~/.kanban/assets/ — 音效等静态资源
- [ ] ~/.kanban/cache/ — 缓存目录
- [ ] ~/.kanban/port — 端口文件

---

## 8. 文件与资源管理

### 8.1 图片管理
- [ ] multipart 上传（20MB 限制）
- [ ] SHA256 哈希去重（同内容不重复存储）
- [ ] 文件存储到 ~/.kanban/images/{uuid}.{ext}
- [ ] 图片 ↔ 任务关联
- [ ] 图片 metadata 查询（路径解析、代理 URL 生成）
- [ ] 图片服务：根据 ID 返回文件流 + 正确 MIME type + 缓存头

### 8.2 文件系统浏览
- [ ] 目录列表：含类型标记、隐藏文件过滤
- [ ] Git 仓库递归扫描：定时器 + 深度限制，避免卡死
- [ ] 常用目录扫描（~、~/dev、~/projects、~/Documents）

### 8.3 文件搜索
- [ ] FileSearchCache：预构建项目文件索引
- [ ] SearchQuery：query、project_id、max_results、match_type 过滤
- [ ] 基于 git 历史的 ranking score（最近频繁编辑的文件排名靠前）
- [ ] 缓存预热：最活跃项目优先加载

### 8.4 Skills 管理
- [ ] 规范 skills 目录扫描
- [ ] SKILL.md 解析：提取 name + description
- [ ] Agent skills 目录：每个 agent 独立的 skills 文件夹
- [ ] Symbolic link 管理（agent → canonical）
- [ ] 从 git 仓库导入 skills
- [ ] Legacy skills 兼容标记

---

## 9. 定时任务 (Cron)

### 9.1 配置
- [ ] CronTaskConfig：per-project 定时任务配置
- [ ] CronTask：enabled、cron 表达式、title、description、executor、mode
- [ ] 配置持久化（JSON 文件）

### 9.2 调度引擎
- [ ] Cron 表达式解析
- [ ] 定时触发：根据 cron schedule 创建 task + workspace → 自动执行
- [ ] 任务去重：同一 cron source 避免重复创建
- [ ] 启动时同步所有启用的 cron 项目

---

## 10. 事件系统

### 10.1 MsgStore
- [ ] 内存消息总线：支持多类型事件（项目变更、任务变更、workspace 变更等）
- [ ] LogMsg 类型：Notification、Project、Task、Workspace 等
- [ ] 历史存储 + 实时推送

### 10.2 WebSocket Streaming
- [ ] projects/stream：项目列表实时推送
- [ ] tasks/stream?project_id=X：任务列表实时推送
- [ ] scratch/stream：草稿变更实时推送

### 10.3 SSE Streaming
- [ ] /api/events：统一事件流
- [ ] execution logs streaming
- [ ] workspace diff streaming

---

## 11. 审批系统 (内部)

### 11.1 审批流程
- [ ] 审批请求创建（来自 executor）
- [ ] 审批状态：pending → approved / denied / provided_input / timed_out
- [ ] 审批通知：MsgStore → Web UI + Telegram
- [ ] structured_input 审批：需要额外输入 → 要求 Web UI 操作
- [ ] 审批超时：可配置

### 11.2 审批存储
- [ ] 内存存储（PendingApprovalInfo map）
- [ ] ID 生成

---

## 12. Analytics

### 12.1 PostHog 集成
- [ ] 事件追踪：session_start、project_created、task_created、image_uploaded 等
- [ ] 用户 ID 生成（基于 hostname hash 匿名化）
- [ ] 可配置开关（analytics.enabled）
- [ ] API key / endpoint 通过环境变量注入

---

## 13. 分享 (Share) (在迁移中移除，包括前端)

### 13.1 任务分享
- [ ] 将任务发布到远程分享服务
- [ ] 需要 GitHub token 认证
- [ ] 分享状态管理
- [ ] 错误处理：AlreadyShared、ProjectNotLinked、MissingGitHubToken 等

---

## 14. 服务器生命周期

### 14.1 启动
- [ ] 日志初始化（tracing/log 级别过滤）
- [ ] Sentry 错误追踪（可选）
- [ ] 数据库初始化 + 迁移
- [ ] 清理孤儿进程
- [ ] 回填 before_head_commits 和 repo_names
- [ ] 后台服务启动：
  - Telegram bot polling
  - MCP HTTP 服务
  - OpenAI-compat 服务
  - Cron 调度器
  - 文件搜索缓存预热
- [ ] 端口绑定：支持 0 端口（自动分配）
- [ ] Port file 写入
- [ ] 浏览器自动打开

### 14.2 关闭
- [ ] SIGINT / SIGTERM 信号处理
- [ ] 清理所有运行中的进程（kill_all_running_processes）

### 14.3 桌面集成
- [ ] Electron main 进程管理 Go binary 作为子进程
- [ ] 健康检查等待（/api/health polling）
- [ ] 退出时清理子进程
- [ ] 日志转发（Go stdout/stderr → Electron logger）

---

## 15. Desktop / Electron

### 15.1 Electron Main
- [ ] 窗口创建与管理
- [ ] Go backend 子进程生命周期管理
- [ ] 环境变量传递（dev/production path）
- [ ] Tray icon（可选，保留当前行为）

### 15.2 Preload Bridge
- [ ] save_export_context_file：替代当前 Tauri 命令
- [ ] 最小化 contextBridge 暴露

---

## 16. 移除项（不迁移）

- [ ] Tauri desktop/mobile crate → Electron 替代
- [ ] Android discovery (crates/desktop/src/android.rs)
- [ ] frontend/mobile-ui/ 目录
- [ ] Tauri mobile 配置
- [ ] Cargo build for desktop
- [ ] mobile 相关 npm scripts
- [ ] Rust crate: deployment (trait abstraction → Go interface)
- [ ] Rust crate: review
- [ ] cargo test / cargo fmt / cargo clippy → Go 对应工具
- [ ] 上述备注不迁移的部分
