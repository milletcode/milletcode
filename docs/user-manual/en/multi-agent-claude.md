# Multi-Environment Claude

Many teams juggle several Claude environments at once: a personal official account, a company relay gateway, different CLI versions... Milletcode models each of these as an **agent instance** — an independent configuration (binary path, API key, launch arguments, release channel) that you pick when creating a session.

## Where instances live

Edit `[agent.instances.<key>]` in `~/.milletcode/config.toml`. These ship by default:

| Key | Default state | Channel |
|---|---|---|
| `claude` | enabled | stable |
| `smartcode` | disabled | alpha |
| `codex` / `opencode` / `codewhale` | disabled | beta / stable / stable |

Only `claude` is enabled by default; turn the others on (`enabled = true`) or edit them as needed.

## Field reference

```toml
[agent.instances.<key>]
name = "Display name"         # shown in the session-creation UI
enabled = true                # whether it appears in the UI
path = "/path/to/claude"      # absolute path to the CLI; empty = claude from PATH
remote_path = "/opt/claude"   # CLI path on remote hosts; empty = fall back to path
api_key = "sk-ant-..."        # empty = use the CLI's own login
extra_args = ["--flag"]       # extra arguments appended to the CLI launch
auto_approve = false          # true = no approval prompts, agent acts autonomously
release_channel = "stable"    # stable / beta / alpha — UI badge only
allow_config_overwrite = true # whether upgrades may overwrite this instance's defaults
```

Key fields:

- **`api_key`**: when set, it is injected as the `ANTHROPIC_API_KEY` environment variable for that instance — ideal for relay gateways or per-account isolation. Leave empty to keep the CLI's own login.
- **`path` / `remote_path`**: local and remote hosts can use different binaries (e.g. official build locally, a customized build on servers). Remote hosts can override per discovery path (below).
- **`auto_approve`**: false by default. When true, that instance never asks for human approval — enable only for fully trusted environments.

## Example 1: official account + company gateway

```toml
[agent.instances.claude]
name = "Claude Code"
enabled = true

[agent.instances.claude-relay]
name = "Claude (company gateway)"
enabled = true
api_key = "sk-company-relay-key"
extra_args = ["--settings", "/etc/milletcode/claude-relay.json"]
```

Both options appear in the session-creation dropdown, each with its own credentials.

## Example 2: multiple CLI versions side by side

```toml
[agent.instances.claude]
name = "Claude Code (stable)"
enabled = true
path = "/Users/me/.local/bin/claude"

[agent.instances.claude-nightly]
name = "Claude Code (nightly)"
enabled = true
path = "/Users/me/.local/bin/claude-nightly"
release_channel = "beta"
```

## Remote host integration

The CLI path on a remote host can be overridden per host (highest priority):

```toml
[project.discovery_paths.prod.paths]
claude = "/home/deploy/.local/bin/claude"        # claude instance path on prod
claude-relay = "/opt/claude-relay/bin/claude"    # another instance on the same host
```

See [Remote Directories](remote-directories.md) for the full walkthrough.

## Default instance

The instance pre-selected for new sessions is controlled by `[agent]`:

```toml
[agent]
default_agent = "claude"
approval_timeout_secs = 600   # approval wait; auto-denies after timeout
```

## Next steps

- [Remote Directories](remote-directories.md) — attach remote-host projects
- [README.md](../../../README.md) at the repo root — architecture and developer guide
