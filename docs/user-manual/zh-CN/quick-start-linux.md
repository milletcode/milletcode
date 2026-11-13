# 快速开始（Linux）

本章带你在一台 Linux 服务器上用一条命令安装 Milletcode，并注册为 systemd 服务。全程约 1–2 分钟。

## 前置条件

- 已安装并登录的 Claude Code CLI（需要 Node.js）：

  ```bash
  npm install -g @anthropic-ai/claude-code
  claude   # 按提示完成登录
  ```

## 1. 一条命令安装

```bash
curl -sfL https://milletcode.com/install.sh | sh -
```

脚本会自动完成：

1. 下载并安装预编译二进制到 `/usr/local/bin/milletcode`（需要 sudo）；
2. 创建工作区 `~/Codes/milletcode-workspace`，生成 `~/.milletcode/config.toml`——登录账号与密码均为随机生成（已有配置不会被覆盖）；
3. 注册 systemd 服务（`/etc/systemd/system/milletcode.service`，服务名 `milletcode`），设为开机自启并立即启动。

**安装输出的最后一行是随机生成的登录凭据**（形如 `Login credentials: user-xxxx / xxxx`），请妥善保存。

可选项：钉住版本 `MILLETCODE_VERSION=v0.11.1 curl -sfL … | sh -`；只绑定本机时加 `MILLETCODE_HOST=127.0.0.1`。

## 2. 访问工作台

浏览器打开 `http://<服务器地址>:9527`（脚本安装的实例监听 9527），用安装输出最后打印的账号密码登录（登录页带图形验证码）。

- 默认监听 `0.0.0.0:9527`；
- 修改账号密码：编辑 `~/.milletcode/config.toml` 的 `[auth.plain]` 段（可选配 `sk-` 前缀静态 API token，供脚本 / CI 调用 REST API），保存后重启服务；
- **服务器部署建议**：不要把 9527 直接暴露到公网。要么只绑定本机（`MILLETCODE_HOST=127.0.0.1` 安装）再套 Nginx/Caddy 反向代理加 TLS，要么用防火墙限制来源 IP：

  ```bash
  sudo ufw allow from 192.168.1.0/24 to any port 9527
  ```

## 3. 常用服务命令

| 操作 | 命令 |
|---|---|
| 查看状态 | `sudo systemctl status milletcode` |
| 停止服务 | `sudo systemctl stop milletcode` |
| 启动服务 | `sudo systemctl start milletcode` |
| 重启服务 | `sudo systemctl restart milletcode` |
| 查看日志 | `sudo journalctl -u milletcode -f` |
| 升级 | 重新执行安装命令 |
| 卸载 | 停止并禁用服务后删除 `/etc/systemd/system/milletcode.service` 与 `/usr/local/bin/milletcode` |

## 4. 创建第一个会话

1. 把一个项目目录加进发现路径（页面上「添加项目」，或直接编辑 `config.toml` 的 `[project.discovery_paths]`，见[远程目录](remote-directories.md)中的配置说明）；
2. 在项目页选择 Claude Code，创建会话；
3. 输入任务描述发送，观察智能体流式输出、工具调用卡片与审批提示。

## 开发模式

日常二次开发用 `make dev`：后端 `127.0.0.1:3030` + 前端热更新 `0.0.0.0:5180`，开发时请访问 `http://127.0.0.1:5180`（与脚本安装的 9527 实例互不冲突）。参与贡献请阅读仓库根目录的 [CONTRIBUTING.md](https://github.com/milletcode/milletcode/blob/main/CONTRIBUTING.md)。

## 安全须知（alpha 特性）

「终端直连」与「端口转发」为 alpha 特性，等同于把目标环境的操作权交给工作台使用者：终端直连暴露目标环境的 shell，端口转发把远程服务映射到访问者本机。请只在受信任的网络与环境中使用，不要把工作台端口直接暴露到公网；确需远程访问，请自行配置 TLS 反向代理与访问控制。

## 下一步

- [远程目录](remote-directories.md)：把远程主机上的项目接入工作台
- [多环境 Claude 配置](multi-agent-claude.md)：为不同账号 / 网关配置多个 Claude 实例
