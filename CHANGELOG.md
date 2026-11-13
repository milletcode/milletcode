# Changelog

Milletcode® multi-agent scheduling platform — every step forward, written down here. The Chinese edition lives in [CHANGELOG.zh-CN.md](CHANGELOG.zh-CN.md).

## v0.11.1

- **New Codex coding agent (beta)**: one more teammate to pick from when you create a session. You watch it work in real time; when it is unsure it asks you and offers choices; operations that need your sign-off still pop up an approval; after an interruption it picks up right where it left off, and you can switch models mid-run (beta, feedback welcome).
- **One-line install**: no development environment to prepare — copy one command from the website, paste it into a terminal, and Milletcode is installed and running. Future upgrades use the very same command.
- **Refreshed project homepage**: why you need this workbench, what it can do for you, and how to get started quickly — the introduction was rewritten from scratch, so first-time visitors can find their bearings fast.

## v0.11.0

- **Now open source**: the Milletcode source code is publicly available — explore it, give feedback, and build with us.
- **Security hardened across the board**: we ran a complete health check and fixed every important security issue — your workbench, files, and data are safer.
- **User manual published**: a multilingual manual (Chinese / English) that covers installation, remote environments, and multi-agent setup in one place.
- **Terminal direct-connect (alpha)**: open a terminal on the target environment right inside a session — no more switching back and forth (alpha, still being polished).
- **Port forwarding (alpha)**: map a service port from a remote environment into your local browser with one click — debugging a remote service now feels like debugging a local one (alpha, still being polished).

## v0.10.0

- **A new way to sign in**: account-and-password login, with a captcha on the login page that automatically blocks malicious attempts — your workbench is safer.
- **Smoother session history**: opening a session takes you straight to the latest message; scroll up to load earlier records on demand — even very long conversations stay smooth.
- **Interrupt anytime**: while a task is running, the send button turns into "Stop" — stop or revise whenever you like.
- **Idle alerts**: when an agent becomes idle it sends you a message, so a finished task or a request for new instructions never goes unnoticed.
- **Switch models mid-session**: the SmartCode agent supports switching models while running — change your approach without restarting the session.
- **Redesigned approvals**: operations that need your decision are presented clearly; everything else no longer interrupts you so often.
- **Quick-connect from a session**: jump straight to the target environment from the session page — one less round trip.
- **More flexible repository access**: clone private repositories with a username and password, including straight into remote directories.
- **Agent channels at a glance**: the session-creation picker was redesigned, so you can instantly tell stable from experimental (beta / alpha).
- **Crash reasons made visible**: when an agent exits unexpectedly it tells you why — no more guessing; normal stops are no longer misreported as crashes.
- **Cleaner stops**: fixed sessions that "looked stopped but were still quietly running", so resources are no longer wasted.
- **One-click file download**: download a single file straight from the file list — no commands needed.
- **Safer restarts**: a terminal restart now asks first, with a "don't ask again" option, so an accidental click no longer interrupts your work.
- **Configs are respected**: an existing configuration stays exactly as it is after an upgrade — never silently overwritten.
- **More reliable deployments**: upgrades and restarts no longer lose data, and file ownership is more dependable.
- **Skills module (beta)**: install or uninstall official skill packs with one click; third-party packs install after you review them.
- **One-click skill distribution (beta)**: install one skill onto multiple devices and multiple coding agents at once, with live progress and automatic verification when the install finishes.
- **Steadier, clearer skill installs (beta)**: before installing, it says exactly what will be installed and where; install status keeps only the key information, so success or failure is clear at a glance.
- **New official skill "Project Init" (beta)**: initialize your project with the officially recommended tech stack in one go.
- **Better-behaved install records (beta)**: third-party skill install records can be deleted one by one; records that are still installing are protected from accidental deletion.
- **Detail fixes**: fixed the command-generation panel going dead, and links in messages failing to open a new tab on mobile browsers, among other issues.

## Earlier versions

- **Rebrand**: Milletcode® is now positioned as a "multi-agent scheduling platform" — we turn your ideas into software.
- Core features officially launched after months of development and testing.
