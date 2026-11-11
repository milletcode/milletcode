# Quick Start (macOS)

This guide takes a macOS machine from source code to a running MilletCode installed as a boot-time system service. Expect 10–20 minutes depending on compile speed.

## Prerequisites

- macOS 12 or later
- Rust 1.75 or later:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- Node.js 18 or later (with npm)
- Git
- Claude Code CLI installed and signed in:

  ```bash
  npm install -g @anthropic-ai/claude-code
  claude   # follow the prompts to sign in
  ```

## 1. Get the source

```bash
git clone https://github.com/milletcode/milletcode.git
cd milletcode
```

## 2. Build and install

```bash
make install
```

This single command does everything:

1. Generates version info, builds the frontend (npm) and the backend (release mode, with the frontend embedded);
2. Installs the binary to `~/.local/bin/milletcode` (make sure it is on your PATH);
3. Registers a macOS LaunchAgent (`~/Library/LaunchAgents/com.milletcode.milletcode.plist`) that starts the service at login.

On first start, a default configuration is generated at `~/.milletcode/config.toml`.

## 3. Configure login

The auth mode (`auth.mode`) is required — the service refuses to start when it is empty. Edit `~/.milletcode/config.toml`:

```toml
[auth]
mode = "plain"

[auth.plain]
user = "admin"
password = "change-me-to-a-strong-password"
# Optional: static API token (sk- prefix) for scripts / CI calling the REST API
# token = "sk-xxxxxxxx"
```

Restart the service:

```bash
make service-restart
```

## 4. Open the workbench

Visit <http://127.0.0.1:3030> and sign in with the credentials above (the login page includes a captcha).

- The default bind address is `0.0.0.0:3030`, so other devices on your LAN can reach it.
- Local-only? Reinstall the service with an explicit bind address:

  ```bash
  make service-install HOST=127.0.0.1
  ```

## 5. Everyday service commands

| Command | Effect |
|---|---|
| `make service-status` | Show service status |
| `make service-stop` | Stop the service |
| `make service-start` | Start the service |
| `make service-restart` | Restart the service |
| `make update` | Rebuild after pulling and replace the binary |
| `make service-uninstall` | Stop and uninstall the service |

## 6. Create your first session

1. Add a project directory to the discovery paths ("Add project" in the UI, or edit `[project.discovery_paths]` in `config.toml` — see [Remote Directories](remote-directories.md) for the details);
2. On the project page choose Claude Code and create a session;
3. Type a task and send — watch the streaming output, tool-call cards and approval prompts.

## Development mode

For day-to-day development use `make dev`: backend on `127.0.0.1:3030` plus a hot-reloading frontend on `0.0.0.0:5180`. During development, open <http://127.0.0.1:5180>.

## Security notes (alpha features)

Terminal direct-connect and port forwarding are alpha features — they hand operational control of the target environment to workbench users: the terminal exposes the target's shell, and port forwarding maps remote services onto the visitor's machine. Use them only on trusted networks. Never expose the workbench port directly to the public internet; put TLS termination and access control in front if you need remote access.

## Next steps

- [Remote Directories](remote-directories.md) — bring remote-host projects into the workbench
- [Multi-Environment Claude](multi-agent-claude.md) — set up multiple Claude instances per account / gateway
