# 远程目录

Milletcode 不只管理本机项目：任何你能 SSH 登录的主机，其上的目录都可以作为项目接入工作台。智能体直接在远程主机上读写文件、执行命令，你的浏览器只是一个窗口。

## 工作原理

- 后端通过 SSH（密钥登录）连接远程主机，用 SFTP 管理文件、执行命令启动智能体 CLI；
- `hostname` 对应你 SSH config 里的 `Host` 别名（或直接写 `user@host`），连接参数（端口、跳板机、私钥）全部复用 `~/.ssh/config`；
- 远程主机上需要预装对应的智能体 CLI（如 Claude Code）。

## 1. 准备远程主机

1. 在远程主机安装智能体 CLI 并完成登录（以 Claude Code 为例）：

   ```bash
   npm install -g @anthropic-ai/claude-code
   claude   # 完成登录
   which claude   # 记下绝对路径，例如 /home/deploy/.local/bin/claude
   ```

2. 在运行 Milletcode 的机器上配置免密登录：

   ```bash
   ssh-keygen -t ed25519            # 已有密钥可跳过
   ssh-copy-id deploy@server.example.com
   ssh deploy@server.example.com    # 确认不再询问密码
   ```

3. 建议在 `~/.ssh/config` 里起个别名：

   ```ssh-config
   Host prod
       HostName server.example.com
       User deploy
       Port 22
   ```

## 2. 配置发现路径

编辑 `~/.milletcode/config.toml`，在 `[project.discovery_paths]` 下加一段。键（下面的 `prod`）是这台主机的标识：

```toml
[project.discovery_paths.prod]
path = "~/projects"              # 远程主机上要扫描的项目根目录
hostname = "prod"                # SSH config 里的 Host 别名
remote_mode = "ssh"              # 远程模式

[project.discovery_paths.prod.paths]
claude = "/home/deploy/.local/bin/claude"   # 远程主机上 claude CLI 的绝对路径
```

保存后重启服务（`make service-restart`），工作台项目列表里就会出现这台主机上的项目。

## 3. 可选开关

| 字段 | 默认 | 说明 |
|---|---|---|
| `sync_settings` | `true` | 启动时是否把本机 `~/.claude/settings.json` 同步到远程主机（尽力而为；显式切换模型等操作仍会写入） |
| `sync_models` | `true` | 是否允许该主机上的会话切换模型。设为 `false` 会隐藏界面上的模型选择器——适合远程 Claude 配置由其他系统托管的场景 |
| `ssh.config_path` | `~/.ssh/config` | 这台主机专用的 SSH 配置文件路径 |
| `is_mobile_home` | `false` | 标记为移动端项目根目录（HarmonyOS/iOS/Android 工作区） |

## 4. 使用远程会话

- 在项目页选择远程主机上的项目，正常创建会话即可；智能体的所有文件操作与命令都在远程主机上执行；
- 会话页可直达该环境的终端（alpha）；
- 远程服务调试可用端口转发（alpha）把远程端口映射到本机浏览器。

## 安全边界

- 远程目录 = 智能体可以在那台主机上执行任意命令，请只接入你拥有且信任的主机；
- 终端直连与端口转发为 alpha 特性：终端会把目标环境 shell 暴露给浏览器会话，端口转发会把远程服务映射到访问者本机 127.0.0.1。仅在工作台与远程主机都处于受信任网络时使用；
- SSH 私钥永远留在运行 Milletcode 的机器上，不会下发到浏览器。

## 下一步

- [多环境 Claude 配置](multi-agent-claude.md)：给不同主机 / 账号配置不同的 Claude 实例
