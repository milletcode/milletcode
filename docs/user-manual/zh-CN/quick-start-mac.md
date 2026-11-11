# 快速开始（macOS）

本章带你在一台 macOS 上从源码跑起 MilletCode，并安装为开机自启的系统服务。全程约 10–20 分钟（取决于编译速度）。

## 前置条件

- macOS 12 或更高版本
- Rust 1.75 或更高：

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- Node.js 18 或更高（含 npm）
- Git
- 已安装并登录的 Claude Code CLI：

  ```bash
  npm install -g @anthropic-ai/claude-code
  claude   # 按提示完成登录
  ```

## 1. 获取源码

```bash
git clone https://github.com/milletcode/milletcode.git
cd milletcode
```

## 2. 构建并安装

```bash
make install
```

这一条命令会完成全部工作：

1. 生成版本信息、构建前端（npm）并构建后端（release 模式，前端产物内嵌其中）；
2. 安装命令行到 `~/.local/bin/milletcode`（请确认它在 PATH 里）；
3. 注册 macOS LaunchAgent（`~/Library/LaunchAgents/com.milletcode.milletcode.plist`），开机自动启动。

首次启动时会在 `~/.milletcode/config.toml` 生成默认配置。

## 3. 配置登录

登录方式（`auth.mode`）为必填项，留空时服务无法启动。编辑 `~/.milletcode/config.toml`：

```toml
[auth]
mode = "plain"

[auth.plain]
user = "admin"
password = "请改成你自己的强密码"
# 可选：静态 API token（sk- 前缀），供脚本 / CI 调用 REST API 时使用
# token = "sk-xxxxxxxx"
```

保存后重启服务：

```bash
make service-restart
```

## 4. 访问工作台

浏览器打开 <http://127.0.0.1:3030>，用配置的账号密码登录（登录页带图形验证码）。

- 默认监听 `0.0.0.0:3030`，局域网内其他设备可访问。
- 只想本机访问？重装服务并指定绑定地址：

  ```bash
  make service-install HOST=127.0.0.1
  ```

## 5. 常用服务命令

| 命令 | 作用 |
|---|---|
| `make service-status` | 查看服务状态 |
| `make service-stop` | 停止服务 |
| `make service-start` | 启动服务 |
| `make service-restart` | 重启服务 |
| `make update` | 更新代码后重新构建并替换二进制 |
| `make service-uninstall` | 停止并卸载服务 |

## 6. 创建第一个会话

1. 把一个项目目录加进发现路径（页面上「添加项目」，或直接编辑 `config.toml` 的 `[project.discovery_paths]`，见[远程目录](remote-directories.md)中的配置说明）；
2. 在项目页选择 Claude Code，创建会话；
3. 输入任务描述发送，观察智能体流式输出、工具调用卡片与审批提示。

## 开发模式

日常二次开发用 `make dev`：后端 `127.0.0.1:3030` + 前端热更新 `0.0.0.0:5180`，开发时请访问 <http://127.0.0.1:5180>。

## 安全须知（alpha 特性）

「终端直连」与「端口转发」为 alpha 特性，等同于把目标环境的操作权交给工作台使用者：终端直连暴露目标环境的 shell，端口转发把远程服务映射到访问者本机。请只在受信任的网络与环境中使用，不要把工作台端口直接暴露到公网；确需远程访问，请自行配置 TLS 反向代理与访问控制。

## 下一步

- [远程目录](remote-directories.md)：把远程主机上的项目接入工作台
- [多环境 Claude 配置](multi-agent-claude.md)：为不同账号 / 网关配置多个 Claude 实例
