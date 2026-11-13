#!/bin/sh
#
# milletcode 一键安装脚本（从 GitHub Release 安装预编译二进制）
#
# 用法：
#   curl -sfL https://milletcode.com/install.sh | sh -
#   MILLETCODE_VERSION=v0.11.1 sh install.sh      # 钉住版本
#   sh install.sh --help
#
# 可配置项（环境变量）：
#   MILLETCODE_VERSION       要安装的版本（默认：最新正式 Release）
#   MILLETCODE_BIN_DIR       安装目录（默认：/usr/local/bin）
#   MILLETCODE_SKIP_CHECKSUM 跳过校验和验证（true 时跳过，不建议）
#
set -eu

GITHUB_REPO="milletcode/milletcode"
GITHUB_BASE="https://github.com/${GITHUB_REPO}"
API_BASE="https://api.github.com/repos/${GITHUB_REPO}"

# --- 输出助手（非终端自动去色，兼容 curl | sh 场景） -------------------------

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
milletcode 安装脚本：从 GitHub Release 下载预编译二进制并安装到本机。

用法：
  sh install.sh [--help]
  MILLETCODE_VERSION=v0.11.1 sh install.sh
  MILLETCODE_BIN_DIR=/custom/bin sh install.sh

环境变量：
  MILLETCODE_VERSION        目标版本（如 v0.11.1；默认取最新正式 Release）
  MILLETCODE_BIN_DIR        安装目录（默认 /usr/local/bin）
  MILLETCODE_SKIP_CHECKSUM  设为 true 跳过校验和验证（不建议）
EOF
}

case "${1:-}" in
    -h|--help) usage; exit 0 ;;
    "") ;;
    *) fatal "未知参数：$1（--help 查看用法）" ;;
esac

# --- 平台与架构 --------------------------------------------------------------

setup_verify_os_arch() {
    OS=$(uname -s)
    ARCH=$(uname -m)
    case "$OS" in
        Darwin) OS=darwin ;;
        Linux)  OS=linux ;;
        *) fatal "不支持的操作系统：$OS（当前支持 darwin / linux）" ;;
    esac
    case "$ARCH" in
        x86_64|amd64)   ARCH=amd64 ;;
        aarch64|arm64)  ARCH=arm64 ;;
        *) fatal "不支持的 CPU 架构：$ARCH（当前支持 amd64 / arm64）" ;;
    esac
    info "检测到平台：${OS}-${ARCH}"
}

# --- 版本解析 ----------------------------------------------------------------

resolve_version() {
    if [ -n "${MILLETCODE_VERSION:-}" ]; then
        VERSION="${MILLETCODE_VERSION}"
        # 容错：用户传裸版本号时补 v 前缀
        case "$VERSION" in
            v*) ;;
            *) VERSION="v${VERSION}" ;;
        esac
        info "目标版本：${VERSION}（由 MILLETCODE_VERSION 指定）"
        return
    fi
    info "正在查询最新正式版本……"
    VERSION=$(download_stdout "${API_BASE}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -1)
    [ -n "$VERSION" ] || fatal "无法获取最新版本号（GitHub API 不可达或限流）。可用 MILLETCODE_VERSION=vX.Y.Z 显式指定。"
    info "最新正式版本：${VERSION}"
}

# --- 下载（curl 优先，wget 兜底，5 次重试） -----------------------------------

has_curl() { command -v curl >/dev/null 2>&1; }
has_wget() { command -v wget >/dev/null 2>&1; }

# curl >= 7.71 才有 --retry-all-errors（传输中途被重置也算可重试错误）；
# 老版本没有该选项时传了会直接报错，故探测后再启用
CURL_RETRY_ALL=""
if has_curl && curl --help all 2>/dev/null | grep -q -- --retry-all-errors; then
    CURL_RETRY_ALL="--retry-all-errors"
fi

download_file() {
    # $1 = URL, $2 = 输出路径
    has_curl || has_wget || fatal "需要 curl 或 wget，两者都未安装"
    i=1
    while :; do
        if has_curl; then
            # shellcheck disable=SC2086  # 可选标志位，依赖按空格分词
            curl -sfL --retry 2 $CURL_RETRY_ALL "$1" -o "$2" && return 0
        else
            wget -q "$1" -O "$2" && return 0
        fi
        [ "$i" -ge 5 ] && fatal "下载失败（已重试 $i 次）：$1"
        warn "下载失败，第 $((i + 1)) 次重试：$1"
        i=$((i + 1))
        sleep 3
    done
}

download_stdout() {
    has_curl && { curl -sfL --retry 2 "$1"; return; }
    has_wget && { wget -q -O - "$1"; return; }
    fatal "需要 curl 或 wget，两者都未安装"
}

# --- 校验和 ------------------------------------------------------------------

verify_checksum() {
    # $1 = 工作目录（内含 tar.gz 与 checksums.txt），$2 = 包名
    if [ "${MILLETCODE_SKIP_CHECKSUM:-false}" = "true" ]; then
        warn "已按环境变量要求跳过校验和验证"
        return
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$1" && grep " $2\$" checksums.txt | sha256sum -c -) || fatal "校验和不匹配：$2（下载可能不完整或被篡改）"
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$1" && grep " $2\$" checksums.txt | shasum -a 256 -c -) || fatal "校验和不匹配：$2（下载可能不完整或被篡改）"
    else
        warn "系统缺少 sha256sum / shasum，跳过校验和验证"
        return
    fi
    info "校验和验证通过"
}

# --- 安装 --------------------------------------------------------------------

do_install() {
    SUDO=sudo
    [ "$(id -u)" -eq 0 ] && SUDO=""

    BIN_DIR="${MILLETCODE_BIN_DIR:-/usr/local/bin}"
    if ! $SUDO sh -c "touch '${BIN_DIR}/.mc-write-test' && rm -f '${BIN_DIR}/.mc-write-test'" 2>/dev/null; then
        fatal "安装目录不可写：${BIN_DIR}（可用 MILLETCODE_BIN_DIR 换目录）"
    fi

    if command -v pgrep >/dev/null 2>&1 && pgrep -x milletcode >/dev/null 2>&1; then
        warn "检测到正在运行的 milletcode，新版本需重启进程后生效"
    fi

    $SUDO install -m 0755 "${TMP_DIR}/milletcode" "${BIN_DIR}/milletcode"
    info "已安装：${BIN_DIR}/milletcode"
}

print_next_steps() {
    cat >&2 <<EOF

[INFO] 安装完成：milletcode ${VERSION}（${OS}-${ARCH}）

下一步：
  1) 创建最小配置 ~/.milletcode/config.toml（首次使用）：

         [auth]
         mode = "plain"

         [auth.plain]
         user = "admin"
         password = "改成你的强密码"

         [project.discovery_paths.default]
         path = "~/Codes"          # 要扫描的项目根目录

  2) 启动：milletcode
     打开 http://127.0.0.1:3030 ，用上面配置的账号密码登录。

升级：重新运行本安装脚本即可（同一条命令）。
文档：https://github.com/${GITHUB_REPO}#readme
EOF
}

# --- 主流程 ------------------------------------------------------------------

main() {
    setup_verify_os_arch
    resolve_version

    PKG="milletcode-${VERSION}-${OS}-${ARCH}.tar.gz"
    ASSET_BASE="${GITHUB_BASE}/releases/download/${VERSION}"

    TMP_DIR=$(mktemp -d) || fatal "无法创建临时目录"
    trap 'rm -rf "${TMP_DIR}"' EXIT INT TERM

    info "下载 ${PKG} ……"
    download_file "${ASSET_BASE}/${PKG}" "${TMP_DIR}/${PKG}"
    download_file "${ASSET_BASE}/checksums.txt" "${TMP_DIR}/checksums.txt"

    verify_checksum "${TMP_DIR}" "${PKG}"

    info "解压中……"
    tar xzf "${TMP_DIR}/${PKG}" -C "${TMP_DIR}"

    do_install
    print_next_steps
}

main
