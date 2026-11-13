# MilletCode 使用手册（简体中文）

MilletCode 是一个自托管的多Agent调度平台：把 Claude Code 等主流编程智能体装进浏览器工作台。你在网页里创建会话、给智能体下达任务，它就能在你的项目目录（本机或远程主机）里读写代码、执行命令、交付结果；文件管理、终端、端口转发、技能分发，也都在同一个工作台完成。

## 本手册包含

1. [快速开始（macOS）](quick-start-mac.md) —— 一条命令安装、登录访问、服务管理
2. [快速开始（Linux）](quick-start-linux.md) —— 同上，systemd 版
3. [远程目录](remote-directories.md) —— 把远程主机上的项目通过 SSH 接入工作台
4. [多环境 Claude 配置](multi-agent-claude.md) —— 为不同账号 / API 网关 / CLI 路径配置多个 Claude 实例

## 核心概念

- **智能体（agent）**：驱动会话的编程智能体，如 Claude Code。每个智能体在配置里是一个「实例」，可以定义自己的命令路径、API key 与启动参数。
- **发现路径（discovery path）**：工作台扫描的项目目录。可以是本机目录，也可以是配置了 SSH 的远程目录。
- **会话（session）**：在一个项目里与智能体的一次连续协作，包含完整的对话历史与文件变更。

## Alpha 特性安全边界

「终端直连」与「端口转发」目前为 alpha 特性：

- 终端直连会把目标环境的 shell 暴露给浏览器会话；
- 端口转发会把远程服务的端口映射到访问者的本机 127.0.0.1。

这两类能力等同于把目标环境的操作权交给了工作台的使用者。请只在受信任的网络与环境中启用，不要把工作台端口直接暴露到公网；确需远程访问，请自行配置 TLS 反向代理与访问控制。

## 反馈

- 提交 Issue：[milletcode/milletcode](https://github.com/milletcode/milletcode/issues)
