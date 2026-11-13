# MilletCode User Manual (English)

MilletCode is a self-hosted multi-agent orchestration platform that brings mainstream coding agents like Claude Code into a browser workbench. You create sessions, give the agent a task, and it reads and writes code, runs commands, and delivers results inside your project directory (local or on a remote host). File management, terminals, port forwarding and skill distribution all live in the same workbench.

## What's in this manual

1. [Quick Start (macOS)](quick-start-mac.md) — one-line install, sign in, manage the service
2. [Quick Start (Linux)](quick-start-linux.md) — same, systemd edition
3. [Remote Directories](remote-directories.md) — bring projects on remote hosts into the workbench over SSH
4. [Multi-Environment Claude](multi-agent-claude.md) — configure multiple Claude instances for different accounts / gateways / CLI paths

## Core concepts

- **Agent**: the coding agent that powers a session, such as Claude Code. Each agent is an "instance" in the configuration with its own binary path, API key and launch arguments.
- **Discovery path**: a project directory the workbench scans. It can be a local directory or an SSH-configured remote directory.
- **Session**: one continuous collaboration with an agent inside a project, with full conversation history and file changes.

## Alpha feature security boundaries

Terminal direct-connect and port forwarding are currently alpha features:

- Terminal direct-connect exposes the target environment's shell to the browser session;
- Port forwarding maps remote service ports onto the visitor's local 127.0.0.1.

Both effectively hand operational control of the target environment to workbench users. Enable them only on trusted networks and environments. Never expose the workbench port directly to the public internet — put TLS termination and access control (a reverse proxy) in front if you need remote access.

## Feedback

- Open an issue: [milletcode/milletcode](https://github.com/milletcode/milletcode/issues)
