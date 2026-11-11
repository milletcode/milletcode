# MilletCode Developer Guide

Multi-agent session management platform for AI coding agents (Claude Code, DeepSeek/Codewhale, SmartCode).

## Table of Contents

- [Overview](#overview)
- [Quick Start](#quick-start)
- [Configuration](#configuration)
- [Authentication](#authentication)
- [Projects](#projects)
- [Sessions](#sessions)
  - [Session Lifecycle](#session-lifecycle)
  - [Composite Keys](#composite-keys)
  - [Creating a Session](#creating-a-session)
  - [Sending Messages](#sending-messages)
  - [Approval Workflow](#approval-workflow)
  - [Close, Restore, Delete](#close-restore-delete)
- [Session Detail View](#session-detail-view)
- [Extras](#extras)
- [REST API Reference](#rest-api-reference)
- [WebSocket Protocol](#websocket-protocol)
- [Data Storage](#data-storage)
- [Logging](#logging)

## Overview

MilletCode is a Rust + Svelte web application that manages multiple AI coding agent sessions simultaneously. It provides:

- **Unified interface** — manage Claude Code, Codewhale (DeepSeek-TUI), SmartCode and other agents from one dashboard
- **Real-time output** — WebSocket streams agent output, approval requests, and status changes live
- **Session persistence** — sessions survive server restarts via embedded redb databases
- **Project discovery** — automatically discovers projects by scanning configured directories (local or remote via SSH)
- **Approval workflow** — handle agent permission requests from the UI
- **Composite session keys** — human-readable identifiers like `org/project/agent-name/default`
- **Built-in terminal, file browser, port forwards and SSH tunnels** — per-session and per-project tooling
- **Skills management** — install official skills or register third-party ones

### Architecture

```
┌─────────────────────────────────────────────┐
│  Svelte 5 SPA (SvelteKit + Vite)            │
│  stores + wsPool/wsBridge ↔ REST API        │
└──────────────────┬──────────────────────────┘
                   │ HTTP + WebSocket
┌──────────────────▼──────────────────────────┐
│  Rust Axum Server (port 3030)               │
│  ├── AgentAdapters (claude / codewhale /    │
│  │   smartcode)                             │
│  ├── SessionManager (in-memory + redb)      │
│  ├── EventBus (broadcast channel)           │
│  ├── ProjectManager (local + SSH discovery) │
│  ├── Terminals / PortForwards / Tunnels     │
│  └── Auth (login+session / Bearer token /   │
│      oauth2_proxy)                          │
└─────────────────────────────────────────────┘
```

The frontend lives in `frontend-svelte/` (Svelte 5 + SvelteKit + Vite, no React/Zustand). The backend embeds the built frontend via rust-embed.

## Quick Start

### Prerequisites

- **Rust** (stable toolchain, installed via [rustup](https://rustup.rs))
- **Node.js 20+** and npm (for the frontend)
- **Claude CLI** (`npm install -g @anthropic-ai/claude-code`) for Claude Code sessions
- **API keys** configured for your agents

### One-shot Setup

```bash
make check        # verify required tools are installed
make init         # guided init: deps check → make install (build + register system service)
make init-config  # only write a default ~/.milletcode/config.toml (random login password)
```

### Manual Build and Run

```bash
# Backend (terminal 1)
cd backend
cargo run                 # listens on 0.0.0.0:3030

# Frontend (terminal 2)
cd frontend-svelte
npm install
npm run dev               # Vite dev server on 0.0.0.0:5180, proxies API to :3030
```

Or both at once: `make dev` (kills processes on ports 3030/5180 first).

### Production Build

```bash
make build   # frontend dist + backend release build (frontend embedded via rust-embed)
make update  # build + install binary to ~/.local/bin/milletcode
make service-install   # register launchd (macOS) / systemd (Linux) service, auto-start on boot
```

### First-Time Setup

1. Create `~/.milletcode/config.toml` (see [Configuration](#configuration))
2. Open the web UI and log in (see [Authentication](#authentication))
3. Projects are discovered from the paths in your config

## Configuration

Configuration is stored in `~/.milletcode/config.toml` and read once at startup. Unknown/stale keys are silently ignored; restart the service to apply changes.

```toml
protect_mode = true

[agent]
default_agent = "claude"
approval_timeout_secs = 300

# One block per agent instance. The instance key MUST equal the adapter id.
[agent.instances.claude]
name = "Claude Code"
extra_args = []
auto_approve = true
enabled = true
allow_config_overwrite = true

[agent.instances.codewhale]
name = "CodeWhale"
auto_approve = true
enabled = true

[agent.instances.smartcode]
name = "SmartCode"
auto_approve = true
enabled = true

[web]
listen_uri = "0.0.0.0:3030"

[web.upload]
storage_type = "local"
local_dir = "/tmp/milletcode-uploads"
max_size_mb = 10

[ssh]
config_path = ""    # custom ssh config path, empty = ~/.ssh/config
passphrase = ""

[auth]
# plain | oauth2_proxy（local / oauth2 为预留值，选中会启动失败）
mode = "plain"

[auth.plain]
user = "admin"
password = "..."
token = ""           # sk- prefixed static API token, empty = disabled
token_secret = ""    # HMAC seed for time-slot tokens, empty = disabled

[logs]
session_log_dir = ""
output = "both"      # stdout | file | both
level = "info"
max_files = 30
session_log_enabled = true

[project]
allow_create = false
allow_archive = false
allow_delete = false
scan_interval_secs = 300
scanner_enabled = false

[project.sessions]
max_subscriptions_per_project = "auto"
max_total_sessions = "auto"

# One group per scan root. hostname + remote_mode = "ssh" makes it a remote group.
[project.discovery_paths.milletcode]
path = "~/Codes/milletcode"
hostname = ""                  # empty = local
remote_mode = ""               # "ssh" for remote groups
is_mobile_home = false
sync_settings = true
sync_models = true

[project.discovery_paths.milletcode.paths]
# Per-group agent binary overrides (highest priority), keyed by adapter id:
# claude = "/usr/local/bin/claude"

[permissions]
protected_commands = ["kubectl delete*", "rm -rf*"]
approval_history_count = 1000

[updates]
auto_config_update = true

[changelog]
password = "..."     # password gate for /changelog (independent of login)

[git.issue]
url = "https://git.example.com"
token = "..."
```

Built-in adapters (the `agent.instances.*` key must match the adapter id):

| Adapter id | Implementation | CLI binary |
|------------|----------------|------------|
| `claude` | `adapters/claude.rs` | Claude Code CLI |
| `codewhale` | `adapters/deepseek.rs` | DeepSeek-TUI |
| `smartcode` | `adapters/smartcode.rs` | SmartCode console |

### Agent Binary Resolution

For a session in discovery group `<keyword>` with agent `<id>`, the binary path is resolved in this order:

1. `project.discovery_paths.<keyword>.paths[<id>]` — per-group override (highest priority)
2. `agent.instances.<id>.remote_path` — remote groups only (falls back to `path`)
3. `agent.instances.<id>.path`
4. Empty → bare adapter binary name, resolved from `PATH`

## Authentication

Three mutually exclusive modes via `[auth] mode`:

- **`plain`** (default) — credentials from `[auth.plain]`. Browsers use the self-contained login page (`/login`, with captcha) which mints an in-memory session cookie; scripts use Bearer tokens (`token` = static `sk-` token, `token_secret` = time-slot HMAC tokens). **Basic auth is not accepted in plain mode.** Everything outside the public paths (`/login`, `/api/captcha`, `/api/logout`, `/changelog`, `/api/changelog`) requires a session — including `/`, `/assets/*` and `/version.json`.
- **`oauth2_proxy`** — an upstream oauth2-proxy injects `X-Auth-Request-User`; direct backend access falls back to `[auth.oauth2_proxy]` basic auth.
- `local` / `oauth2` — reserved, selecting them fails at startup.

`protect_mode = true` rewrites error output to avoid leaking internal details in responses.

## Projects

### Discovery

Projects are auto-discovered by scanning immediate subdirectories of each `project.discovery_paths` entry. A directory is recognized as a project if it contains a **`README.md` or a `.git` entry** (freshly cloned repos often have no README yet). The first non-empty line of the README (stripped of leading `#`) becomes the project description.

Remote groups (`remote_mode = "ssh"`) run the same scan on the remote host over SSH.

### Dashboard

The **Projects** page shows all discovered projects as cards (name, description, session count, last modified) plus a manual rescan. Depending on `[project]` flags, cards may also offer create / archive / delete / clone actions.

### Project Detail

Per-project page with: new session form, session list, git status, environment variable management (`/envs`), issue tracker config, quick links, terminal quick commands.

## Sessions

### Session Lifecycle

Sessions progress through these states (snake_case over the wire):

| State | Description |
|-------|-------------|
| `starting` | Agent process is being spawned |
| `running` | Agent is active and processing |
| `waiting_approval` | Agent needs user permission before an action |
| `stopped` | Manually stopped by the user |
| `complete` | Agent finished successfully (exit code 0) |
| `error` | Agent failed (non-zero exit code) |
| `restarting` | Agent is being restarted (e.g., after config change) |

### Composite Keys

Sessions are identified by human-readable composite keys instead of raw UUIDs:

```
{org}/{project}/{agent-name}/{session-name}
```

Example: `smartcode/agent-demo/claude-code/default`

- **org/project** — derived from the project directory path
- **agent-name** — agent display name, spaces replaced with dashes, lowercased
- **session-name** — user-provided name, sanitized. Defaults to `default`

The UUID is still generated internally for the agent process, but the composite key is what appears in URLs and REST paths.

### Creating a Session

1. Navigate to a project detail page or the **Sessions** list
2. Select an **Agent** from the dropdown (release stage badges such as beta/alpha may apply)
3. Optionally edit the **Session Name** (defaults to `default`)
4. Enter an initial **Prompt** and press **Enter** or click **Start**

If a session already exists at the same composite key, a warning banner offers **Overwrite / Resume / Dismiss**.

### Sending Messages

Type in the input field at the bottom of the session detail view and press **Enter**. Messages go over the WebSocket; user messages render right-aligned, agent responses stream in left-aligned.

### Approval Workflow

When an agent attempts a protected action, it pauses and emits an approval request. The UI surfaces it as a global banner plus an inline panel in the session view, with **Approve / Deny** buttons (decisions are relayed to the agent's stdin). Protected command patterns from `[permissions] protected_commands` always require explicit approval; `auto_approve = true` skips prompts entirely. Full approval history is kept (default 1000 entries, `approval_history_count`).

Some agents also raise **AskUserQuestion**-style prompts (see `answer_question` below) — these render as option cards instead of approve/deny.

### Close, Restore, Delete

| Session Status | Available Actions |
|---------------|-------------------|
| Running / WaitingApproval / Starting | **Close** (stop the agent process) |
| Stopped / Error | **Restore** (restart), **Delete** (full cleanup) |

- **Close** — kills the agent process and removes its lock file; session data and the redb record are preserved. Status becomes `stopped`.
- **Restore** — spawns a new agent process, resuming from conversation history (Claude uses `--continue`).
- **Delete (full)** — stops the session, removes the redb record, in-memory state, and the agent's local session data (e.g. `~/.claude/projects/<path>/<uuid>.jsonl` and the `<uuid>/` directory).

The Sessions list supports multi-select with bulk delete.

## Session Detail View

Accessed at `/sessions/{org}/{project}/{agent}/{name}`. Header shows project tag, agent + status, composite key badge, truncated session UUID with copy, and status-aware action icons. Main area is the conversation view (initial prompt, streamed agent output, collapsible thinking blocks, tool-use indicators, follow-up messages) with auto-scroll, plus tabs/panels for the file browser, approval history and an embedded debug console.

## Extras

- **Terminal** — browser terminals per project/session (`/api/terminal/*` + `/api/terminal/{id}/ws`)
- **Port forwards & tunnels** — scan, create, test and proxy local/remote ports; `/p/{key}/{port}/...` serves signed reverse-proxy links
- **Skills** — install/uninstall official skills, register third-party skills, per-node install status
- **Debug** — process list, log browsing and tailing (`/api/debug/*`)
- **Changelog** — public page at `/changelog`, gated by its own `[changelog] password`

## REST API Reference

Base URL: `http://host:3030`. All routes require auth unless listed as public (see [Authentication](#authentication)).

### Sessions

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/sessions` | List all sessions |
| GET | `/api/sessions/idle` | List idle sessions |
| GET/DELETE | `/api/sessions/{org}/{project}/{agent_name}/{session_name}` | Get / delete session |
| POST | `.../{key}/stop` | Stop session |
| POST | `.../{key}/restore` | Restore session |
| POST | `.../{key}/message` | Send message `{"content": "..."}` |
| POST | `.../{key}/approve` | Approval decision `{"selected": "approve|deny", "comment": "..."}` |
| POST | `.../{key}/answer` | Answer an AskUserQuestion prompt |
| POST | `.../{key}/config` | Per-session config (e.g. model hot-switch) |
| GET | `.../{key}/history` | Session conversation history |
| GET | `.../{key}/files`, `/file`, `/file/download` | File browser |
| POST | `.../{key}/idle-notify` | Idle notification settings |
| DELETE | `.../{key}/full` | Full delete (stop + redb + local data) |

(`{key}` = `{org}/{project}/{agent_name}/{session_name}`)

### Projects

| Method | Path | Description |
|--------|------|-------------|
| GET/POST | `/api/projects` | List / create projects |
| GET | `/api/projects/discovery-paths` | List configured discovery groups |
| POST | `/api/projects/refresh` | Trigger re-scan |
| POST | `/api/projects/clone` | Clone a repository |
| GET/DELETE | `/api/projects/{keyword}/{name}` | Get / delete project |
| POST | `/api/projects/{keyword}/{name}/archive` `/unarchive` | Archive state |
| GET | `/api/projects/{keyword}/{name}/sessions` | Sessions of a project |
| GET | `/api/projects/{keyword}/{name}/git-status` | Git status |
| GET/PUT/DELETE | `.../envs`, `.../envs/{env}`, `.../envs/{env}/{key}` | Env var management |
| GET/PUT/DELETE | `.../issue-config` | Issue tracker config |
| GET/PUT | `.../link` | Quick links |
| GET/POST | `.../terminal/commands` | Terminal quick commands |

### Agents / Settings / Skills

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/agents` | List configured agents |
| PUT | `/api/agents/{id}` | Update agent (e.g. model / release channel) |
| GET | `/api/settings` | Current settings |
| GET | `/api/settings/auth` | Auth settings |
| GET | `/api/settings/ssh-hosts` | SSH hosts from config |
| GET | `/api/settings/capacity`, `/online` | Capacity / online stats |
| GET/PUT | `/api/skills/*` | Official & third-party skills management |
| GET | `/api/pending`, `/api/approvals` | Pending approvals / decision history |
| GET | `/api/health` | Health check |
| POST | `/api/ws-ticket` | Mint a one-time WebSocket ticket |

### Uploads / Terminals / Forwards / Tunnels / Debug

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/uploads` | File upload (`?project_key=` forwards to remote hosts via SSH) |
| GET | `/api/uploads/preview` | Preview an uploaded file |
| GET/POST/DELETE | `/api/terminal/*` | Terminal CRUD + input/resize/command |
| GET/POST/DELETE | `/api/forwards/*` | Port forward CRUD + scan + test + proxy-link |
| GET/POST/DELETE | `/api/tunnels/*` | SSH tunnels |
| GET | `/api/debug/*` | Process list, logs, session logs, tail |
| GET | `/api/config/{key}` | Config router (merged endpoints) |

## WebSocket Protocol

Session WS: `ws://host:3030/ws/{org}/{project}/{agent_id}`. Terminal WS: `/api/terminal/{id}/ws`.

Browsers cannot set headers on WebSocket upgrades, so handshake auth uses one of: a real `Upgrade: websocket` handshake on the terminal/tunnel endpoints (per-endpoint credential check), or a one-time ticket minted via `POST /api/ws-ticket`.

### Client → Server

All messages are JSON with a `type` field (snake_case):

| Type | Fields | Description |
|------|--------|-------------|
| `subscribe_session` | `session_id` | Subscribe; replays the output buffer on first subscribe |
| `unsubscribe_session` | `session_id` | Unsubscribe |
| `send_message` | `session_id`, `content` | Send a message to the agent |
| `approve` | `session_id`, `decision`, `comment?`, `request_id?` | Approve or deny a pending action |
| `answer_question` | `session_id`, `request_id`, `answers` (map question → chosen option) | Answer an AskUserQuestion prompt |
| `stop_session` | `session_id` | Stop a running session |
| `interrupt_session` | `session_id` | Interrupt current turn without stopping the session |
| `restore_session` | `session_id` | Restore a closed session |
| `delete_session` | `session_id` | Full delete |

### Server → Client

| Type | Fields | Description |
|------|--------|-------------|
| `output` | `session_id`, `content`, `ts`, `raw?` | Agent output; `raw` preserves the original JSONL line (model, usage, sessionId, ...) |
| `approval_request` | `session_id`, ... | Agent needs user approval |
| `approval_resolved` | `session_id` | Pending approval was resolved |
| `user_question` | `session_id`, ... | AskUserQuestion prompt for the user |
| `session_status` | `session_id`, `status` | Session state changed |
| `session_restarted` | `session_id` | Session restarted (output cleared) |
| `session_complete` | `session_id`, `exit_code` | Session finished |
| `global_notification` | ... | Cross-session notification |
| `config_changed` | ... | Config hot-reload notice |
| `error` | `session_id`, `message` | Error occurred |

### Subscription Behavior

- Multiple clients can subscribe to the same session simultaneously
- The first subscribe per session replays the full output buffer; subsequent subscribes only receive new events
- On disconnect the frontend auto-reconnects and re-subscribes to its active sessions

## Data Storage

All data lives under `~/.milletcode/`:

| File/Directory | Purpose |
|----------------|---------|
| `config.toml` | Main configuration |
| `sessions.redb` | Session metadata and output (embedded KV database) |
| `approvals.redb` | Approval request/decision history |
| `projects.redb` | Project metadata and overrides |
| `config.redb` | Runtime settings overrides |
| `envs.redb` | Project environment variables |
| `skills.redb` | Installed skills registry |
| `logs/` | Daily rotated log files |
| `runtime/`, `bin/`, `ssh-config`, `root.pem` | Runtime state, managed binaries, SSH assets |

Uploads go to `[web.upload] local_dir` (default `/tmp/milletcode-uploads`), organized by category (`pics/docs/excel/log/other`); for remote-group sessions the file is forwarded to the remote host over SSH.

### Session Database Schema

Each session record contains: `session_id` (UUID), `session_key` (composite key), `project_id` (absolute path), `agent_id` / `agent_type` / `agent_name`, `status`, `prompt`, `session_output` (accumulated buffer), `created_at`, `create_time` / `update_time` (ms).

### Claude CLI Local Data

Claude Code stores session data at `~/.claude/projects/<path-with-dashes>/`: `<uuid>.jsonl` (conversation history) and `<uuid>/` (tool results, subagent outputs). Full delete removes both.

## Logging

Logs are written to `~/.milletcode/logs/` (or `[logs] session_log_dir`) with daily rotation, named `agent-console.YYYY-MM-DD`.

| Level | When to Use |
|-------|-------------|
| `error` | Only errors and panics |
| `warn` | Warnings and errors |
| `info` | Session lifecycle events, user messages (recommended default) |
| `debug` | Detailed agent events and stream data |
| `trace` | Every individual stream-json line |

When `session_log_enabled` is true, agent interactions (output, approvals, user messages) are logged; `max_files` controls retention. Panics are always captured with full backtrace at ERROR level. Error output may be rewritten by `protect_mode` to avoid leaking internals.
