# Remote Directories

MilletCode is not limited to local projects: any host you can SSH into can have its directories attached to the workbench as projects. The agent reads and writes files and runs commands directly on the remote host — your browser is just a window.

## How it works

- The backend connects to the remote host over SSH (key-based), manages files over SFTP, and launches the agent CLI over the same channel;
- `hostname` maps to a `Host` alias in your SSH config (or a literal `user@host`). All connection parameters — port, jump hosts, private keys — are reused from `~/.ssh/config`;
- The remote host needs the agent CLI (e.g. Claude Code) pre-installed.

## 1. Prepare the remote host

1. Install the agent CLI on the remote host and sign in (Claude Code shown here):

   ```bash
   npm install -g @anthropic-ai/claude-code
   claude   # sign in
   which claude   # note the absolute path, e.g. /home/deploy/.local/bin/claude
   ```

2. From the machine running MilletCode, set up passwordless SSH:

   ```bash
   ssh-keygen -t ed25519            # skip if you already have a key
   ssh-copy-id deploy@server.example.com
   ssh deploy@server.example.com    # confirm no password prompt
   ```

3. Give the host an alias in `~/.ssh/config`:

   ```ssh-config
   Host prod
       HostName server.example.com
       User deploy
       Port 22
   ```

## 2. Configure the discovery path

Edit `~/.milletcode/config.toml` and add an entry under `[project.discovery_paths]`. The key (`prod` below) identifies this host:

```toml
[project.discovery_paths.prod]
path = "~/projects"              # project root to scan on the remote host
hostname = "prod"                # Host alias from your SSH config
remote_mode = "ssh"              # remote mode

[project.discovery_paths.prod.paths]
claude = "/home/deploy/.local/bin/claude"   # absolute path of claude on the remote host
```

Restart the service (`make service-restart`) and projects on that host appear in the workbench project list.

## 3. Optional switches

| Field | Default | Description |
|---|---|---|
| `sync_settings` | `true` | Best-effort sync of the local `~/.claude/settings.json` to the remote host at startup (explicit model switches still write) |
| `sync_models` | `true` | Whether sessions on this host may switch models. `false` hides the model selector — for hosts whose Claude settings are managed elsewhere |
| `ssh.config_path` | `~/.ssh/config` | Host-specific SSH config file path |
| `is_mobile_home` | `false` | Mark as a mobile project root (HarmonyOS/iOS/Android workspace) |

## 4. Using remote sessions

- Pick a project on the remote host and create a session as usual; all file operations and commands run on that host;
- The session page links straight to that environment's terminal (alpha);
- Use port forwarding (alpha) to map a remote service port onto your local browser for debugging.

## Security boundaries

- A remote directory means the agent can run arbitrary commands on that host — only attach hosts you own and trust;
- Terminal direct-connect and port forwarding are alpha features: the terminal exposes the target's shell to the browser session, and port forwarding maps remote services onto the visitor's local 127.0.0.1. Use them only when the workbench and the remote host sit on trusted networks;
- SSH private keys never leave the machine running MilletCode; they are never sent to the browser.

## Next steps

- [Multi-Environment Claude](multi-agent-claude.md) — point different hosts / accounts at different Claude instances
