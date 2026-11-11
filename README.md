# MilletCode — 多 Agent 调度平台

[简体中文](README.md) | English ([README-EN.md](README-EN.md))

一个自托管的 Web 工作台，把 Claude Code、DeepSeek、SmartCode 等 AI 编程智能体统一进浏览器，本地与远程项目通吃。终端用户指南见[使用手册](docs/user-manual/)（简体中文 / English）。

## 技术栈

| 层 | 技术 |
|---|---|
| **后端** | Rust + Axum + redb + tokio |
| **前端** | Svelte 5 + TypeScript + Vite + Tailwind CSS |
| **状态** | Svelte Stores（客户端）+ WebSocket（实时） |
| **数据库** | redb（嵌入式键值存储） |

## 功能特性

### 项目管理
- **自动发现**：扫描配置的目录寻找项目（含 `README.md` 的目录）
- **标签过滤**：按发现根路径对项目分组
- **手动刷新**：通过「扫描项目」按钮触发重新扫描
- **会话列表**：查看每个项目下的全部会话

### 会话管理
- **复合键路由**：会话以 `org/project/agent/name` 标识，不再使用 UUID
- **会话命名**：可编辑的会话名（默认 "default"），以徽标形式显示在输入框上
- **已有会话检测**：创建前检查同一键位是否已有会话
- **覆盖 / 恢复流程**：会话已存在时，可选择覆盖或恢复
- **实时输出**：基于 WebSocket 的智能体终端输出流式推送
- **审批处理**：智能体审批请求以横幅展示（选择操作、附加评论）
- **会话状态**：可视化指示（Running、WaitingApproval、Stopped、Error、Starting）
- **会话持久化**：会话经 redb 持久化，后端重启不丢失

### 智能体支持
- **Claude Code** — 基于 PTY 的会话，流式输出（默认启用）
- **DeepSeek** — HTTP API 适配器
- **SmartCode** — 流式适配器（alpha）
- 可选实例（`codewhale`、`opencode`、`codex`）默认关闭；在 `[agent.instances]` 下启用并配置

### 远程执行（SSH）
- **远程项目**：把任意 SSH 可达主机的目录挂为发现路径，智能体直接在远程主机上运行
- **连接池**：SSH 连接跨会话复用
- **标准 SSH 配置**：主机别名、端口、密钥、跳板机均来自 `~/.ssh/config`
- **SFTP 文件管理**：在 UI 中浏览、读取、下载远程项目文件

### 配置
- **TOML 配置**：全局设置位于 `~/.milletcode/config.toml`
- **多实例智能体**：每个实例可配置二进制路径、API key、启动参数与发布通道
- **认证模式**：`plain`（用户名密码 + 验证码登录）或 `oauth2_proxy`（部署在 OAuth2 Proxy 之后）
- **GPT API 配置**：主副 API key 与模型选择，均可在 UI 中操作

## 快速开始

### 前置要求

- Rust 1.75+
- Node.js 18+
- npm

### 安装

```bash
make install
```

### 运行开发服务器

```bash
make dev
```

后端跑在 `:3030`，前端跑在 `:5180`。

### 构建

```bash
# 后端
cd backend && cargo build --release

# 前端
cd frontend-svelte && npm run build
```

## 配置

### `~/.milletcode/config.toml`

首次启动时自动生成。`auth.mode` 为必填——未设置前服务拒绝启动。最小配置示例：

```toml
[auth]
mode = "plain"

[auth.plain]
user = "admin"
password = "change-me"

[project.discovery_paths.default]
path = "~/Codes"          # 要扫描的本地项目根目录

# 可选：通过 SSH 接入的远程主机（hostname 为 ~/.ssh/config 里的别名）
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
# path = "/path/to/claude"        # 覆盖 CLI 二进制路径
# api_key = "sk-ant-..."          # 以 ANTHROPIC_API_KEY 注入
# extra_args = []
# auto_approve = false
# release_channel = "stable"
```

字段完整说明见使用手册：[多环境 Claude 配置](docs/user-manual/zh-CN/multi-agent-claude.md) 与 [远程目录](docs/user-manual/zh-CN/remote-directories.md)。

## API

后端在 `/api` 下暴露 REST API，外加一条用于会话实时流的 WebSocket。会话以复合键寻址：`/api/sessions/{org}/{project}/{agent_name}/{session_name}`，子资源包括 `/message`、`/approve`、`/stop`、`/restore`、`/config`、`/history`、`/files` 等。

常用入口：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/health` | 健康检查 |
| GET | `/api/agents` | 列出已配置的智能体实例 |
| GET | `/api/projects` | 列出发现的项目 |
| GET | `/api/sessions` | 列出全部会话 |
| POST | `/api/sessions/{org}/{project}/{agent}/{name}/message` | 向会话发送消息 |
| POST | `/api/sessions/{org}/{project}/{agent}/{name}/approve` | 处理审批请求 |

完整的 REST/WebSocket 参考见 [docs/developer-guide.md](docs/developer-guide.md)。

## 赞助支持

开源不易。如果 MilletCode 帮到了你，欢迎请我们喝杯奶茶。

| 微信赞助 | 支付宝赞助 |
|---|---|
| ![微信赞助二维码](docs/images/sponsor/wechat.png) | ![支付宝赞助二维码](docs/images/sponsor/alipay.png) |

其他方式（Bitcoin / Ethereum 及最新二维码）见官网：[milletcode.com](https://milletcode.com)。

## 许可证

Copyright (c) 2026 杭州玄米科技有限公司 (Hangzhou MilletCode Technology Co., Ltd.)

本项目基于 **GNU Affero General Public License v3.0 only**（[AGPL-3.0-only](./LICENSE)）授权。

简而言之：你可以自由地使用、学习、修改和再分发本软件，**但如果你将修改后的版本作为网络服务运行（例如托管平台），你必须向该服务的用户提供相应的源代码。** 不允许对本项目进行闭源的托管衍生。

**MilletCode®** 与 MilletCode 标识是杭州玄米科技有限公司的注册商标。许可证覆盖代码，不覆盖品牌——名称与标识的可用范围见 [TRADEMARKS.md](TRADEMARKS.md)（中英双语）。
