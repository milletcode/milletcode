#!/bin/sh
#
# milletcode installer (downloads a prebuilt binary from GitHub Releases)
#
# Usage:
#   curl -sfL https://milletcode.com/install.sh | sh -
#   MILLETCODE_VERSION=v0.11.1 sh install.sh      # pin a version
#   sh install.sh --help
#
# Configuration (environment variables):
#   MILLETCODE_VERSION        Version to install (vX.Y.Z or latest; default latest = newest stable release)
#   MILLETCODE_DOWNLOAD_BASE  Download base URL (default: GitHub Releases; point it at a mirror to switch source)
#   MILLETCODE_API_BASE       Version-resolution API base (not touched when a version is pinned)
#   MILLETCODE_BIN_DIR        Install directory (default: /usr/local/bin)
#   MILLETCODE_SKIP_CHECKSUM  Set to true to skip checksum verification (not recommended)
#   MILLETCODE_SKIP_SERVICE   Set to true to skip service installation (binary + config only)
#   MILLETCODE_HOST           Bind address for the service (default: 0.0.0.0; use 127.0.0.1 for local-only)
#   MILLETCODE_PORT           Listen port for the service (default: 9527; dev instances use 3030)
#   MILLETCODE_WORKSPACE      Project workspace directory (default: ~/Codes/milletcode-workspace)
#
set -eu

MILLET_REPO="milletcode/milletcode"
MILLET_DOCS="https://github.com/${MILLET_REPO}#readme"
SERVICE_NAME="milletcode"
PLIST_LABEL="com.milletcode.milletcode"
# Mirror swap points: downloads go through DOWNLOAD_BASE, version resolution through API_BASE.
MILLETCODE_DOWNLOAD_BASE="${MILLETCODE_DOWNLOAD_BASE:-https://github.com/${MILLET_REPO}}"
MILLETCODE_API_BASE="${MILLETCODE_API_BASE:-https://api.github.com/repos/${MILLET_REPO}}"
# Production listens on 9527 so it can run side by side with a dev instance (3030).
LISTEN_HOST="${MILLETCODE_HOST:-0.0.0.0}"
PORT="${MILLETCODE_PORT:-9527}"

# --- Output helpers (colors auto-disabled when stderr is not a tty, e.g. curl | sh) ---

if [ -t 2 ]; then
    info()  { printf '\033[1;32m[INFO]\033[0m %s\n' "$*" >&2; }
    warn()  { printf '\033[1;33m[WARN]\033[0m %s\n' "$*" >&2; }
    fatal() { printf '\033[1;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }
else
    info()  { printf '[INFO] %s\n' "$*" >&2; }
    warn()  { printf '[WARN] %s\n' "$*" >&2; }
    fatal() { printf '[ERROR] %s\n' "$*" >&2; exit 1; }
fi

usage() {
    cat <<EOF
milletcode installer: downloads a prebuilt binary from GitHub Releases,
sets up the workspace and config, and installs it as a system service.

Usage:
  sh install.sh [--help]
  MILLETCODE_VERSION=v0.11.1 sh install.sh
  MILLETCODE_VERSION=latest sh install.sh
  MILLETCODE_DOWNLOAD_BASE=https://mirror.example/https://github.com/milletcode/milletcode sh install.sh
  MILLETCODE_BIN_DIR=/custom/bin sh install.sh

Environment variables:
  MILLETCODE_VERSION        Target version (vX.Y.Z or latest; default latest = newest stable release)
  MILLETCODE_DOWNLOAD_BASE  Download base URL, default https://github.com/milletcode/milletcode.
                            Replace it wholesale to use a mirror; the asset path
                            /releases/download/... stays the same.
  MILLETCODE_API_BASE       Version-resolution API base, default the GitHub API.
                            Not accessed when the version is pinned.
  MILLETCODE_BIN_DIR        Install directory (default /usr/local/bin)
  MILLETCODE_SKIP_CHECKSUM  Set to true to skip checksum verification (not recommended)
  MILLETCODE_SKIP_SERVICE   Set to true to skip service installation (binary + config only)
  MILLETCODE_HOST           Bind address for the service (default 0.0.0.0; use 127.0.0.1 for local-only)
  MILLETCODE_PORT           Listen port for the service (default 9527; dev instances use 3030)
  MILLETCODE_WORKSPACE      Project workspace directory (default ~/Codes/milletcode-workspace)
EOF
}

case "${1:-}" in
    -h|--help) usage; exit 0 ;;
    "") ;;
    *) fatal "unknown argument: $1 (see --help)" ;;
esac

# --- Platform and architecture -----------------------------------------------

setup_verify_os_arch() {
    OS=$(uname -s)
    ARCH=$(uname -m)
    case "$OS" in
        Darwin) OS=darwin ;;
        Linux)  OS=linux ;;
        *) fatal "unsupported operating system: $OS (supported: darwin / linux)" ;;
    esac
    case "$ARCH" in
        x86_64|amd64)   ARCH=amd64 ;;
        aarch64|arm64)  ARCH=arm64 ;;
        *) fatal "unsupported CPU architecture: $ARCH (supported: amd64 / arm64)" ;;
    esac
    # Release assets use aarch64 in the darwin package names, arm64 elsewhere.
    if [ "$OS" = darwin ] && [ "$ARCH" = arm64 ]; then
        ARCH=aarch64
    fi
    info "Detected platform: ${OS}-${ARCH}"
}

# --- Version resolution --------------------------------------------------------

resolve_version() {
    VERSION="${MILLETCODE_VERSION:-latest}"
    case "$VERSION" in
        latest) ;;  # explicit latest behaves like unset: resolve the newest stable release
        v*) info "Target version: ${VERSION} (from MILLETCODE_VERSION)"; return ;;
        *)  VERSION="v${VERSION}"  # tolerate a bare version number without the v prefix
            info "Target version: ${VERSION} (from MILLETCODE_VERSION)"; return ;;
    esac
    info "Resolving the latest stable version..."
    VERSION=$(download_stdout "${MILLETCODE_API_BASE}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -1)
    # Fallback when the API is rate-limited or unreachable: the releases/latest page
    # 302-redirects to /tag/<version> on the download host, with no API rate limit.
    if [ -z "$VERSION" ] && has_curl; then
        redir=$(curl -sf -o /dev/null -w '%{redirect_url}' "${MILLETCODE_DOWNLOAD_BASE}/releases/latest" 2>/dev/null || true)
        [ -n "$redir" ] && VERSION=$(printf '%s\n' "$redir" | sed -n 's|.*/tag/\([^/?#]*\).*|\1|p')
        [ -n "$VERSION" ] && info "API unavailable, resolved version via releases/latest redirect"
    fi
    [ -n "$VERSION" ] || fatal "could not resolve the latest version (API unreachable or rate-limited). Pin it with MILLETCODE_VERSION=vX.Y.Z, or set MILLETCODE_DOWNLOAD_BASE to a mirror."
    info "Latest stable version: ${VERSION}"
}

# --- Download (curl preferred, wget fallback, up to 5 attempts) ------------------

has_curl() { command -v curl >/dev/null 2>&1; }
has_wget() { command -v wget >/dev/null 2>&1; }

# --retry-all-errors needs curl >= 7.71 (treats mid-transfer resets as retryable);
# passing it to an older curl errors out, so probe for support first.
CURL_RETRY_ALL=""
if has_curl && curl --help all 2>/dev/null | grep -q -- --retry-all-errors; then
    CURL_RETRY_ALL="--retry-all-errors"
fi

download_file() {
    # $1 = URL, $2 = output path
    has_curl || has_wget || fatal "either curl or wget is required, neither found"
    i=1
    while :; do
        if has_curl; then
            # shellcheck disable=SC2086  # optional flag, relies on word splitting
            curl -sfL --retry 2 $CURL_RETRY_ALL "$1" -o "$2" && return 0
        else
            wget -q "$1" -O "$2" && return 0
        fi
        [ "$i" -ge 5 ] && fatal "download failed after $i attempts: $1"
        warn "download failed, attempt $((i + 1)): $1"
        i=$((i + 1))
        sleep 3
    done
}

download_stdout() {
    has_curl && { curl -sfL --retry 2 "$1"; return; }
    has_wget && { wget -q -O - "$1"; return; }
    fatal "either curl or wget is required, neither found"
}

http_ok() {
    if has_curl; then
        curl -sf -o /dev/null "$1"
    elif has_wget; then
        wget -q -O /dev/null "$1"
    else
        return 1
    fi
}

# --- Checksum -------------------------------------------------------------------

verify_checksum() {
    # $1 = working directory (contains the tar.gz and checksums.txt), $2 = package name
    if [ "${MILLETCODE_SKIP_CHECKSUM:-false}" = "true" ]; then
        warn "skipping checksum verification as requested"
        return
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && grep " $2\$" checksums.txt | sha256sum -c -) || fatal "checksum mismatch: $2 (download may be incomplete or tampered)"
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$1" && grep " $2\$" checksums.txt | shasum -a 256 -c -) || fatal "checksum mismatch: $2 (download may be incomplete or tampered)"
    else
        warn "sha256sum / shasum not found, skipping checksum verification"
        return
    fi
    info "Checksum verification passed"
}

# --- Small helpers ----------------------------------------------------------------

gen_rand_hex() {
    # $1 = number of bytes, prints 2x that many hex chars
    if command -v openssl >/dev/null 2>&1 && openssl rand -hex "$1" 2>/dev/null; then
        return 0
    fi
    head -c $(( $1 * 2 )) /dev/urandom | od -An -tx1 | tr -d ' \n'
}

xml_escape() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

# systemd treats % in unit values as a specifier prefix; double it to get a literal.
unit_escape() { printf '%s' "$1" | sed -e 's/%/%%/g'; }

# --- Run user ---------------------------------------------------------------------
# The service must run as the human who installed milletcode (agents inherit its
# env and write files as this user). When the script runs elevated (sudo), work
# with the invoking user instead of root.

resolve_run_user() {
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
        RUN_USER=$SUDO_USER
        if [ "$OS" = darwin ]; then
            RUN_HOME=$(dscl . -read "/Users/${RUN_USER}" NFSHomeDirectory 2>/dev/null | awk '{print $2}')
        else
            RUN_HOME=$(getent passwd "${RUN_USER}" 2>/dev/null | cut -d: -f6)
            [ -n "$RUN_HOME" ] || RUN_HOME=$(awk -F: -v u="$RUN_USER" '$1 == u {print $6}' /etc/passwd)
        fi
    else
        RUN_USER=$(id -un)
        RUN_HOME=$HOME
    fi
    [ -n "${RUN_HOME:-}" ] || fatal "could not resolve the home directory of user ${RUN_USER}"
    info "Service will run as user: ${RUN_USER}"
}

chown_run_user_if_elevated() {
    # $@ = paths created this run that must belong to the run user
    if [ "$(id -u)" -eq 0 ] && [ "$RUN_USER" != "root" ]; then
        RUN_GROUP=$(id -gn "$RUN_USER")
        chown -R "${RUN_USER}:${RUN_GROUP}" "$@"
    fi
}

# --- Workspace and config -----------------------------------------------------------

# Config must exist BEFORE the service starts: on first start the backend
# materializes a default config.toml if none is present, and ours (random
# credentials, workspace path) would never be picked up.
setup_workspace_and_config() {
    WORKSPACE="${MILLETCODE_WORKSPACE:-${RUN_HOME}/Codes/milletcode-workspace}"
    case "$WORKSPACE" in
        '~'/*) WORKSPACE="${RUN_HOME}/${WORKSPACE#\~/}" ;;  # tolerate a quoted ~/...
        /*) ;;
        *)   WORKSPACE="${RUN_HOME}/${WORKSPACE}" ;;
    esac
    WORKSPACE_CREATED=no
    if [ ! -d "$WORKSPACE" ]; then
        mkdir -p "$WORKSPACE" || fatal "could not create workspace directory: ${WORKSPACE}"
        WORKSPACE_CREATED=yes
    fi
    info "Workspace: ${WORKSPACE}"

    MILLET_DIR="${RUN_HOME}/.milletcode"
    mkdir -p "$MILLET_DIR" || fatal "could not create config directory: ${MILLET_DIR}"

    CONFIG_CREATED=no
    if [ ! -f "${MILLET_DIR}/config.toml" ]; then
        CRED_USER="user-$(gen_rand_hex 4)"
        CRED_PASS=$(gen_rand_hex 8)
        [ -n "$CRED_PASS" ] || fatal "could not generate random credentials (no openssl, /dev/urandom unreadable)"

        # Tilde-ify the workspace path inside the config; the backend expands ~
        # against the runtime user's home, keeping the file portable.
        WS_CFG=$WORKSPACE
        case "$WS_CFG" in
            "$RUN_HOME"/*) WS_CFG="~${WS_CFG#"$RUN_HOME"}" ;;
        esac

        # Field names are locked by backend config.rs tests; keep in sync with
        # scripts/init-config-template.toml in the repo.
        cat > "${MILLET_DIR}/config.toml" <<EOF
# Generated by the milletcode installer. Login credentials are random;
# edit [auth.plain] and restart the service to change them.

[auth]
mode = "plain"

[auth.plain]
user = "${CRED_USER}"
password = "${CRED_PASS}"

[ssh]
config_path = "~/.ssh/config"

[web]
listen_uri = "${LISTEN_HOST}:${PORT}"

[project]
allow_create = true
allow_archive = true
allow_delete = true

[project.discovery_paths.workspace]
path = "${WS_CFG}"
hostname = ""
remote_mode = ""
is_mobile_home = false
sync_settings = true
sync_models = true
EOF
        chmod 0600 "${MILLET_DIR}/config.toml"
        CONFIG_CREATED=yes
        info "Created config: ${MILLET_DIR}/config.toml (random login credentials)"
    else
        info "Config already exists, keeping it: ${MILLET_DIR}/config.toml"
    fi

    if [ "$WORKSPACE_CREATED" = yes ] || [ "$CONFIG_CREATED" = yes ]; then
        chown_run_user_if_elevated "$WORKSPACE" "$MILLET_DIR"
    fi
}

# --- Service ---------------------------------------------------------------------

# launchd/systemd never load the user's interactive shell rc files, while agent
# sessions and terminals inherit the service env - without this the spawned
# shells lose PATH. Capture the real PATH once from the user's shell and bake it
# into the service definition. Fallback to a generic list (always includes
# BIN_DIR so the installed binary itself stays reachable).
capture_service_path() {
    p=""
    if command -v zsh >/dev/null 2>&1 && [ -f "${RUN_HOME}/.zshrc" ]; then
        p=$(zsh -ic 'print -r -- "MILLET_PATH_MARKER:$PATH"' 2>/dev/null \
            | sed -n 's/^MILLET_PATH_MARKER://p' | tail -1)
    elif command -v bash >/dev/null 2>&1 && [ -f "${RUN_HOME}/.bashrc" ]; then
        p=$(bash -ic 'echo "MILLET_PATH_MARKER:$PATH"' 2>/dev/null \
            | sed -n 's/^MILLET_PATH_MARKER://p' | tail -1)
    fi
    [ -n "$p" ] || p="${BIN_DIR}:${RUN_HOME}/.local/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    printf '%s' "$p"
}

install_service() {
    SERVICE_INSTALLED=false
    if [ "${MILLETCODE_SKIP_SERVICE:-false}" = "true" ]; then
        warn "skipping service installation as requested"
        return
    fi
    if [ "$OS" = darwin ]; then
        install_service_darwin
    else
        install_service_linux
    fi
}

install_service_darwin() {
    PLIST_DIR="${RUN_HOME}/Library/LaunchAgents"
    PLIST="${PLIST_DIR}/${PLIST_LABEL}.plist"
    LOG_DIR="${RUN_HOME}/.milletcode/logs"
    mkdir -p "$PLIST_DIR" "$LOG_DIR" || fatal "could not create ${PLIST_DIR} / ${LOG_DIR}"

    SVC_PATH=$(capture_service_path)
    BIN_PATH=$(xml_escape "${BIN_DIR}/milletcode")
    HOME_ESC=$(xml_escape "$RUN_HOME")
    LOG_ESC=$(xml_escape "$LOG_DIR")
    PATH_ESC=$(xml_escape "$SVC_PATH")

    printf '%s\n' \
        '<?xml version="1.0" encoding="UTF-8"?>' \
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
        '<plist version="1.0">' \
        '<dict>' \
        "    <key>Label</key>" \
        "    <string>${PLIST_LABEL}</string>" \
        '    <key>ProgramArguments</key>' \
        '    <array>' \
        "        <string>${BIN_PATH}</string>" \
        '        <string>--host</string>' \
        "        <string>${LISTEN_HOST}</string>" \
        '    </array>' \
        '    <key>WorkingDirectory</key>' \
        "    <string>${HOME_ESC}</string>" \
        '    <key>RunAtLoad</key>' \
        '    <true/>' \
        '    <key>KeepAlive</key>' \
        '    <true/>' \
        '    <key>StandardOutPath</key>' \
        "    <string>${LOG_ESC}/milletcode.log</string>" \
        '    <key>StandardErrorPath</key>' \
        "    <string>${LOG_ESC}/milletcode.log</string>" \
        '    <key>EnvironmentVariables</key>' \
        '    <dict>' \
        '        <key>HOME</key>' \
        "        <string>${HOME_ESC}</string>" \
        '        <key>PATH</key>' \
        "        <string>${PATH_ESC}</string>" \
        '        <key>MILETCODE_STATIC_CACHE_CONTROL</key>' \
        '        <string>0</string>' \
        '    </dict>' \
        '</dict>' \
        '</plist>' > "$PLIST"
    chown_run_user_if_elevated "$PLIST"

    # Load into the run user's GUI session (matters when the script itself runs via sudo).
    # shellcheck disable=SC2086  # AS_USER is intentionally word-split
    AS_USER=""
    if [ "$(id -u)" -eq 0 ] && [ "$RUN_USER" != "root" ]; then
        AS_USER="sudo -u ${RUN_USER}"
    fi
    $AS_USER launchctl unload "$PLIST" 2>/dev/null || true
    $AS_USER launchctl load "$PLIST" || fatal "launchctl load failed for ${PLIST}"
    SERVICE_INSTALLED=true
    info "Service ${PLIST_LABEL} installed and started (logs: ${LOG_DIR}/milletcode.log)"
}

install_service_linux() {
    if ! command -v systemctl >/dev/null 2>&1; then
        warn "systemd (systemctl) not found; skipping service installation. Start the server manually: ${BIN_DIR}/milletcode"
        return
    fi
    if [ "$(id -u)" -eq 0 ]; then
        SYSCTL="systemctl"
        SUDOC=""
    else
        command -v sudo >/dev/null 2>&1 || fatal "sudo is required to install a systemd service (run as root, or install sudo)"
        SYSCTL="sudo systemctl"
        SUDOC="sudo"
    fi

    UNIT="/etc/systemd/system/${SERVICE_NAME}.service"
    SVC_PATH=$(capture_service_path)

    printf '%s\n' \
        '[Unit]' \
        'Description=MilletCode Agent Console' \
        'After=network-online.target' \
        'Wants=network-online.target' \
        '' \
        '[Service]' \
        'Type=simple' \
        "User=$(unit_escape "$RUN_USER")" \
        "WorkingDirectory=$(unit_escape "$RUN_HOME")" \
        "ExecStart=$(unit_escape "${BIN_DIR}/milletcode") --host ${LISTEN_HOST}" \
        'Restart=on-failure' \
        'RestartSec=5' \
        'Environment=MILETCODE_STATIC_CACHE_CONTROL=0' \
        "Environment=HOME=$(unit_escape "$RUN_HOME")" \
        "Environment=PATH=$(unit_escape "$SVC_PATH")" \
        '' \
        '[Install]' \
        'WantedBy=multi-user.target' | $SUDOC tee "$UNIT" > /dev/null

    # shellcheck disable=SC2086  # SYSCTL is intentionally word-split
    $SYSCTL daemon-reload
    $SYSCTL enable "$SERVICE_NAME" >/dev/null
    $SYSCTL restart "$SERVICE_NAME" || fatal "systemctl restart ${SERVICE_NAME} failed (check: journalctl -u ${SERVICE_NAME} -n 50)"
    SERVICE_INSTALLED=true
    info "Service ${SERVICE_NAME} installed, enabled and started (logs: journalctl -u ${SERVICE_NAME} -f)"
}

wait_for_health() {
    [ "$SERVICE_INSTALLED" = "true" ] || return 0
    info "Waiting for the service to become healthy..."
    i=1
    while [ "$i" -le 30 ]; do
        if http_ok "http://127.0.0.1:${PORT}/api/health"; then
            info "Service is healthy"
            return 0
        fi
        i=$((i + 1))
        sleep 1
    done
    warn "service did not respond on http://127.0.0.1:${PORT} within 30s"
    if [ "$OS" = darwin ]; then
        warn "check the log: ${RUN_HOME}/.milletcode/logs/milletcode.log"
    else
        warn "check the log: journalctl -u ${SERVICE_NAME} -n 50"
    fi
    return 0
}

# --- Install ----------------------------------------------------------------------

do_install() {
    BIN_DIR="${MILLETCODE_BIN_DIR:-/usr/local/bin}"

    # Try a direct install first (create the directory if missing; custom directories
    # often live in the user's home and need no root). Fall back to sudo; never sudo as root.
    if mkdir -p "$BIN_DIR" 2>/dev/null \
        && touch "${BIN_DIR}/.mc-write-test" 2>/dev/null \
        && rm -f "${BIN_DIR}/.mc-write-test" 2>/dev/null; then
        SUDO=""
    elif [ "$(id -u)" -eq 0 ]; then
        fatal "directory not writable even as root: ${BIN_DIR} (set MILLETCODE_BIN_DIR to another path)"
    else
        SUDO=sudo
        $SUDO sh -c "mkdir -p '${BIN_DIR}' && touch '${BIN_DIR}/.mc-write-test' && rm -f '${BIN_DIR}/.mc-write-test'" 2>/dev/null \
            || fatal "directory not writable: ${BIN_DIR} (set MILLETCODE_BIN_DIR to another path)"
    fi

    if command -v pgrep >/dev/null 2>&1 && pgrep -x milletcode >/dev/null 2>&1; then
        warn "a running milletcode process was detected; it will be restarted by the service"
    fi

    $SUDO install -m 0755 "${TMP_DIR}/milletcode" "${BIN_DIR}/milletcode"
    chown_run_user_if_elevated "${BIN_DIR}/milletcode"
    info "Installed: ${BIN_DIR}/milletcode"
}

print_next_steps() {
    cat >&2 <<EOF

[INFO] Installation complete: milletcode ${VERSION} (${OS}-${ARCH})
[INFO] Workspace: ${WORKSPACE}
[INFO] Config: ${RUN_HOME}/.milletcode/config.toml
EOF
    if [ "$SERVICE_INSTALLED" = "true" ]; then
        cat >&2 <<EOF
[INFO] Service is running (service name: ${SERVICE_NAME}, port ${PORT})
[INFO] Open http://127.0.0.1:${PORT} and sign in. Manage the service:
EOF
        if [ "$OS" = darwin ]; then
            printf '         launchctl load|unload %s/Library/LaunchAgents/%s.plist\n' "$RUN_HOME" "$PLIST_LABEL" >&2
            printf '         Logs: %s/.milletcode/logs/milletcode.log\n' "$RUN_HOME" >&2
        else
            printf '         sudo systemctl start|stop|status %s\n' "$SERVICE_NAME" >&2
            printf '         Logs: journalctl -u %s -f\n' "$SERVICE_NAME" >&2
        fi
    else
        cat >&2 <<EOF
[INFO] Service not installed. Start the server manually:
       ${BIN_DIR}/milletcode
EOF
    fi
    printf 'Upgrade: re-run this installer (same one-liner).\nDocs: %s\n' "$MILLET_DOCS" >&2
}

# --- Main flow ----------------------------------------------------------------------

main() {
    setup_verify_os_arch
    resolve_version

    PKG="milletcode-${VERSION}-${OS}-${ARCH}.tar.gz"
    ASSET_BASE="${MILLETCODE_DOWNLOAD_BASE}/releases/download/${VERSION}"

    TMP_DIR=$(mktemp -d) || fatal "could not create a temporary directory"
    trap 'rm -rf "${TMP_DIR}"' EXIT INT TERM

    info "Downloading ${PKG} ..."
    download_file "${ASSET_BASE}/${PKG}" "${TMP_DIR}/${PKG}"
    download_file "${ASSET_BASE}/checksums.txt" "${TMP_DIR}/checksums.txt"

    verify_checksum "${TMP_DIR}" "${PKG}"

    info "Extracting..."
    tar xzf "${TMP_DIR}/${PKG}" -C "${TMP_DIR}"

    resolve_run_user
    do_install
    setup_workspace_and_config
    install_service
    wait_for_health
    print_next_steps

    # Credentials go on the very last line so they are easy to find in scrollback.
    if [ "$CONFIG_CREATED" = "yes" ]; then
        printf '\n[INFO] Login credentials: %s / %s\n' "$CRED_USER" "$CRED_PASS" >&2
    fi
}

main
