# MilletCode — Multi-Agent Orchestration Platform

English | [简体中文](README.md)

A self-hosted, web-based workbench for AI coding agents (Claude Code, DeepSeek, SmartCode, and more) across local and remote projects. See the [user manual](docs/user-manual/) for end-user guides (English / 简体中文).

## Tech Stack

| Layer | Technology |
|-------|------------|
| **Backend** | Rust + Axum + redb + tokio |
| **Frontend** | Svelte 5 + TypeScript + Vite + Tailwind CSS |
| **State** | Svelte Stores (client) + WebSocket (real-time) |
| **Database** | redb (embedded key-value store) |

## Features

### Project Management
- **Auto-discovery**: Scans configured directories for projects (containing `README.md`)
- **Tag filtering**: Group projects by discovery root path
- **Manual refresh**: Trigger re-scan via the "Scan Projects" button
- **Session listing**: View all sessions per project

### Session Management
- **Composite key routing**: Sessions identified by `org/project/agent/name` instead of UUIDs
- **Session naming**: Editable session name (defaults to "default"), shown as badge on input
- **Existing session detection**: Before creating, checks for existing session at the same key
- **Overwrite / Resume flow**: When session exists, offers options to overwrite or resume
- **Real-time output**: WebSocket-based streaming of agent terminal output
- **Approval handling**: UI banner for agent approval prompts (select action, add comment)
- **Session status**: Visual indicators (Running, WaitingApproval, Stopped, Error, Starting)
- **Session persistence**: Sessions survive backend restarts via redb

### Agent Support
- **Claude Code** — PTY-based session with streaming output (enabled by default)
- **DeepSeek** — HTTP API adapter
- **SmartCode** — streaming adapter (alpha)
- Optional instances (`codewhale`, `opencode`, `codex`) ship disabled by default; enable and configure them under `[agent.instances]`

### Remote Execution (SSH)
- **Remote projects**: Attach any SSH-reachable host's directories as discovery paths; agents run directly on the remote host
- **Connection pooling**: Reuse SSH connections across sessions
- **Standard SSH config**: Host aliases, ports, keys and jump hosts all come from `~/.ssh/config`
- **SFTP file management**: Browse, read and download remote project files from the UI

### Configuration
- **TOML-based config**: `~/.milletcode/config.toml` for global settings
- **Multi-instance agents**: Per-instance binary path, API key, launch arguments and release channel
- **Auth modes**: `plain` (username/password login with captcha) or `oauth2_proxy` (behind an OAuth2 Proxy)
- **GPT API config**: Primary and secondary API keys with model selection via UI

## Getting Started

### Prerequisites

- Rust 1.75+
- Node.js 18+
- npm

### Install

```bash
make install
```

### Run Dev Server

```bash
make dev
```

This starts the backend on `:3030` and frontend on `:5180`.

### Build

```bash
# Backend
cd backend && cargo build --release

# Frontend
cd frontend-svelte && npm run build
```

## Configuration

### `~/.milletcode/config.toml`

The file is generated on first start. `auth.mode` is required — the service refuses to start until it is set. A minimal setup:

```toml
[auth]
mode = "plain"

[auth.plain]
user = "admin"
password = "change-me"

[project.discovery_paths.default]
path = "~/Codes"          # local project root to scan

# Optional: a remote host over SSH (alias from ~/.ssh/config)
# [project.discovery_paths.prod]
# path = "~/projects"
# hostname = "prod"
# remote_mode = "ssh"
#
# [project.discovery_paths.prod.paths]
# claude = "/home/deploy/.local/bin/claude"

[agent]
default_agent = "claude"
approval_timeout_secs = 600

[agent.instances.claude]
name = "Claude Code"
enabled = true
# path = "/path/to/claude"        # override the CLI binary
# api_key = "sk-ant-..."          # injected as ANTHROPIC_API_KEY
# extra_args = []
# auto_approve = false
# release_channel = "stable"
```

Full field reference: [multi-agent configuration](docs/user-manual/zh-CN/multi-agent-claude.md) and [remote directories](docs/user-manual/zh-CN/remote-directories.md) in the user manual.

## API

The backend exposes a REST API under `/api` plus a WebSocket for real-time session streaming. Sessions are addressed by composite key `/api/sessions/{org}/{project}/{agent_name}/{session_name}` with sub-resources such as `/message`, `/approve`, `/stop`, `/restore`, `/config`, `/history` and `/files`.

A few entry points:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/health` | Health check |
| GET | `/api/agents` | List configured agent instances |
| GET | `/api/projects` | List discovered projects |
| GET | `/api/sessions` | List all sessions |
| POST | `/api/sessions/{org}/{project}/{agent}/{name}/message` | Send a message to a session |
| POST | `/api/sessions/{org}/{project}/{agent}/{name}/approve` | Resolve an approval prompt |

The complete REST/WebSocket reference lives in [docs/developer-guide.md](docs/developer-guide.md).

## Sponsor

Open source runs on coffee — if MilletCode helps you, consider buying us one.

| Bitcoin | Ethereum |
|---------|----------|
| ![Bitcoin QR code](docs/images/sponsor/bitcoin.png) | ![Ethereum QR code](docs/images/sponsor/ether.png) |

Other methods (WeChat / Alipay and the latest codes) are on our website: [milletcode.com](https://milletcode.com).

## License

Copyright (c) 2026 杭州玄米科技有限公司 (Hangzhou MilletCode Technology Co., Ltd.)

This project is licensed under the **GNU Affero General Public License v3.0 only** ([AGPL-3.0-only](./LICENSE)).

In short: you are free to use, study, modify and redistribute this software, **but if you run a modified version as a network service (e.g., a hosted platform), you must offer the corresponding source code to the users of that service.** Closed-source hosted derivatives of this project are not permitted.

**MilletCode®** and the MilletCode logo are registered trademarks of Hangzhou MilletCode Technology Co., Ltd. (杭州玄米科技有限公司). The license covers the code, not the brand — see [TRADEMARKS.md](TRADEMARKS.md) (bilingual: English / 简体中文) for what uses of the name and logo are permitted.
