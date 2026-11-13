# Contributing to MilletCode

Thanks for your interest in improving MilletCode! This guide covers how to set up a
development environment, run the project locally, and submit changes.

For architecture and API details, see [docs/developer-guide.md](docs/developer-guide.md).
For end-user installation, see the [README](README.md).

## Ways to contribute

- **Bug reports** — open an issue with version, platform, expected vs. actual behavior, and relevant logs.
- **Feature ideas** — open an issue describing the use case before investing in an implementation.
- **Code** — bug fixes and features (see the workflow below).
- **Docs & translations** — the user manual is maintained in Chinese and English under `docs/user-manual/`.

## Prerequisites

- **Rust** (stable toolchain, installed via [rustup](https://rustup.rs))
- **Node.js 20+** with npm
- **Git**
- Optional: an agent CLI (e.g. `npm install -g @anthropic-ai/claude-code`) if you want to
  exercise real agent sessions

## Local development (make)

All common workflows are driven by Makefile targets:

```bash
git clone https://github.com/milletcode/milletcode.git
cd milletcode

make check         # verify required tools are installed
make init-config   # write a minimal ~/.milletcode/config.toml (random login; skipped if it exists)
make dev           # start backend + frontend dev servers
```

`make dev` starts:

| Process | Address | Notes |
|---|---|---|
| Backend (Rust, debug) | `http://127.0.0.1:3030` | kills anything on ports 3030/5180 first |
| Frontend (Vite, hot reload) | `http://127.0.0.1:5180` | proxies API calls to the backend |

Open <http://127.0.0.1:5180> and sign in with the credentials in `~/.milletcode/config.toml`
(`[auth.plain]` — `make init-config` no longer prints them to the terminal).

Useful notes:

- The config file is read once at startup; restart `make dev` after editing it.
- The backend embeds the frontend via rust-embed. In debug builds the dist directory is
  read from disk at runtime, so API tests rely on a stub `frontend-svelte/dist/index.html`
  (CI prepares one automatically).
- **Port model**: development runs on 3030; instances installed via the one-line installer
  listen on 9527 (`[web] listen_uri` in their config). Both can run side by side on the
  same machine — handy when testing a dev build against a production install.

## Tests and checks

Run the same checks CI runs:

```bash
cargo test --workspace --locked --no-fail-fast   # backend (Rust) tests
cd frontend-svelte && npm run check              # svelte-check (types)
npm run build                                    # type-check + production build
```

## Production build

```bash
make build             # frontend dist + backend release build (frontend embedded)
make update            # build + replace the installed binary
make service-install   # register launchd (macOS) / systemd (Linux) service
```

Service management targets: `make service-start | service-stop | service-restart |
service-status | service-uninstall`.

## Project layout

```
backend/          Rust workspace (axum server, agent adapters, redb storage)
frontend-svelte/  Svelte 5 + SvelteKit SPA
scripts/          installer, version stamping, config templates
docs/             developer guide + bilingual user manual
deploy/           Kubernetes manifests
```

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/) style, matching the
existing history:

```
feat: add codex agent adapter
fix: stop reporting normal exits as crashes
docs: rewrite quick-start around the installer
```

Types in use: `feat`, `fix`, `docs`, `test`, `chore`, `refactor`, `perf`, `ci`.

## Pull requests

1. Fork / branch from `main`; keep branches focused on one topic.
2. Make sure the checks above pass locally.
3. Open a PR describing **why** the change is needed; link related issues.
4. For user-facing changes, update the affected docs (README and the bilingual user
   manual where applicable).
5. Keep diffs minimal — no drive-by refactors or formatting sweeps in unrelated files.

## Reporting security issues

Please do not include secrets, tokens, or working exploits in public issues. Include
enough context (version, config shape with secrets redacted) for maintainers to
reproduce the problem.

## License

By contributing, you agree that your contributions will be licensed under the
[AGPL-3.0-only](LICENSE) license used by this project.
