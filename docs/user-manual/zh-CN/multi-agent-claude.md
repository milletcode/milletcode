# 多环境 Claude 配置

很多团队同时有多套 Claude 环境：官方账号、公司中转网关、不同版本的 CLI……MilletCode 用「智能体实例」来承载它们——每个实例是一份独立配置（命令路径、API key、启动参数、发布渠道），在创建会话时自由选择。

## 实例配置在哪

编辑 `~/.milletcode/config.toml` 的 `[agent.instances.<key>]`。默认自带这些实例：

| 键 | 默认状态 | 渠道 |
|---|---|---|
| `claude` | 启用 | stable |
| `smartcode` | 停用 | alpha |
| `codex` / `opencode` / `codewhale` | 停用 | beta / stable / stable |

只有 `claude` 默认启用；其余按需打开（`enabled = true`）或修改。

## 字段说明

```toml
[agent.instances.<key>]
name = "显示名称"            # 会话创建界面展示的名字
enabled = true               # 是否在界面出现
path = "/path/to/claude"     # 本机 CLI 绝对路径；留空则用 PATH 里的 claude
remote_path = "/opt/claude"  # 远程主机上的 CLI 路径；留空则回退 path
api_key = "sk-ant-..."       # 留空则使用 CLI 自身登录态
extra_args = ["--flag"]      # 追加到 CLI 启动命令的参数
auto_approve = false         # true = 不弹审批，智能体自主执行
release_channel = "stable"   # stable / beta / alpha，仅影响界面角标
allow_config_overwrite = true # 升级时是否允许覆盖该实例的默认配置
```

重点字段：

- **`api_key`**：填写后启动会话时会以 `ANTHROPIC_API_KEY` 环境变量注入该实例，适合走中转网关或多账号隔离；不填则沿用 CLI 本身的登录状态。
- **`path` / `remote_path`**：本地与远程可以是不同的二进制（比如本机用官方版、服务器用定制版）。远程主机还能按发现路径再覆盖一次（见下）。
- **`auto_approve`**：默认 false。开启后该实例所有操作不再请求人工审批，请只对完全信任的环境开启。

## 示例一：官方账号 + 公司网关双实例

```toml
[agent.instances.claude]
name = "Claude Code"
enabled = true

[agent.instances.claude-relay]
name = "Claude（公司网关）"
enabled = true
api_key = "sk-company-relay-key"
extra_args = ["--settings", "/etc/milletcode/claude-relay.json"]
```

创建会话时下拉框里就会出现两个选项，各用各的凭据，互不影响。

## 示例二：不同版本 CLI 并存

```toml
[agent.instances.claude]
name = "Claude Code（正式版）"
enabled = true
path = "/Users/me/.local/bin/claude"

[agent.instances.claude-nightly]
name = "Claude Code（尝鲜版）"
enabled = true
path = "/Users/me/.local/bin/claude-nightly"
release_channel = "beta"
```

## 与远程目录联动

远程主机上 CLI 的路径，可以按主机覆盖（优先级最高）：

```toml
[project.discovery_paths.prod.paths]
claude = "/home/deploy/.local/bin/claude"        # prod 主机上的 claude 实例路径
claude-relay = "/opt/claude-relay/bin/claude"    # 同一台主机上的另一个实例
```

完整流程见[远程目录](remote-directories.md)。

## 默认实例

新建会话时默认选中的实例由 `[agent]` 决定：

```toml
[agent]
default_agent = "claude"
approval_timeout_secs = 600   # 审批等待秒数，超时自动拒绝
```

## 下一步

- [远程目录](remote-directories.md)：把远程主机项目接入工作台
- 仓库根目录 [README.zh-CN.md](../../../README.zh-CN.md)：整体架构与开发指南
