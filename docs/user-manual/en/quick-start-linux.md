# Quick Start (Linux)

This guide installs MilletCode on a Linux server with a single command and registers it as a systemd service. Expect 1–2 minutes.

## Prerequisites

- Claude Code CLI installed and signed in (requires Node.js):

  ```bash
  npm install -g @anthropic-ai/claude-code
  claude   # follow the prompts to sign in
  ```

## 1. One-line install

```bash
curl -sfL https://milletcode.com/install.sh | sh -
```

The installer takes care of everything:

1. Downloads and installs the prebuilt binary to `/usr/local/bin/milletcode` (sudo required);
2. Creates the workspace `~/Codes/milletcode-workspace` and generates `~/.milletcode/config.toml` with a **random login and password** (an existing config is never touched);
3. Registers a systemd service (`/etc/systemd/system/milletcode.service`, service name `milletcode`) enabled at boot, and starts it right away.

**The last line of the installer output is your randomly generated login credentials** (e.g. `Login credentials: user-xxxx / xxxx`) — save it.

Options: pin a version with `MILLETCODE_VERSION=v0.11.1 curl -sfL … | sh -`; add `MILLETCODE_HOST=127.0.0.1` to bind locally only.

## 2. Open the workbench

Visit `http://<server-address>:9527` (script-installed instances listen on 9527) and sign in with the credentials printed at the end of the install (the login page includes a captcha).

- The default bind address is `0.0.0.0:9527`.
- To change the login, edit the `[auth.plain]` section of `~/.milletcode/config.toml` (an `sk-` prefixed static API token is optional, for scripts / CI calling the REST API) and restart the service.
- **Server deployment advice**: do not expose port 9527 directly to the public internet. Either bind locally (install with `MILLETCODE_HOST=127.0.0.1`) behind an Nginx/Caddy reverse proxy with TLS, or restrict source IPs with a firewall:

  ```bash
  sudo ufw allow from 192.168.1.0/24 to any port 9527
  ```

## 3. Everyday service commands

| Action | Command |
|---|---|
| Status | `sudo systemctl status milletcode` |
| Stop | `sudo systemctl stop milletcode` |
| Start | `sudo systemctl start milletcode` |
| Restart | `sudo systemctl restart milletcode` |
| Logs | `sudo journalctl -u milletcode -f` |
| Upgrade | re-run the install one-liner |
| Uninstall | stop and disable the service, then remove `/etc/systemd/system/milletcode.service` and `/usr/local/bin/milletcode` |

## 4. Create your first session

1. Add a project directory to the discovery paths ("Add project" in the UI, or edit `[project.discovery_paths]` in `config.toml` — see [Remote Directories](remote-directories.md) for the details);
2. On the project page choose Claude Code and create a session;
3. Type a task and send — watch the streaming output, tool-call cards and approval prompts.

## Development mode

For day-to-day development use `make dev`: backend on `127.0.0.1:3030` plus a hot-reloading frontend on `0.0.0.0:5180`. During development, open `http://127.0.0.1:5180` (this never clashes with a script-installed instance on 9527). See [CONTRIBUTING.md](https://github.com/milletcode/milletcode/blob/main/CONTRIBUTING.md) in the repository root to get involved.

## Security notes (alpha features)

Terminal direct-connect and port forwarding are alpha features — they hand operational control of the target environment to workbench users: the terminal exposes the target's shell, and port forwarding maps remote services onto the visitor's machine. Use them only on trusted networks. Never expose the workbench port directly to the public internet; put TLS termination and access control in front if you need remote access.

## Next steps

- [Remote Directories](remote-directories.md) — bring remote-host projects into the workbench
- [Multi-Environment Claude](multi-agent-claude.md) — set up multiple Claude instances per account / gateway
