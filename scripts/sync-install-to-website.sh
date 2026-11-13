#!/bin/sh
#
# 把本仓库的 scripts/install.sh 同步到官网仓库（website-ui）
#
# 用法：install.sh 更新后执行一次，然后到 website-ui 提交并部署，
#       milletcode.com/install.sh 即为新版本。
#   sh scripts/sync-install-to-website.sh
#
# 可配置项（环境变量）：
#   WEBSITE_UI_DIR  官网仓库路径（默认：~/Codes/smartcode/website-ui）
#
set -eu

SRC_REPO=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SRC="${SRC_REPO}/scripts/install.sh"
DEST_DIR="${WEBSITE_UI_DIR:-$HOME/Codes/smartcode/website-ui}"
DEST="${DEST_DIR}/client-scripts/install.sh"

[ -f "$SRC" ] || { printf '[ERROR] 找不到源文件：%s\n' "$SRC" >&2; exit 1; }
sh -n "$SRC" || { printf '[ERROR] install.sh 语法不过关，先修再同步\n' >&2; exit 1; }
[ -d "${DEST_DIR}/.git" ] || { printf '[ERROR] 官网仓库不存在：%s（可用 WEBSITE_UI_DIR 指定）\n' "$DEST_DIR" >&2; exit 1; }

mkdir -p "${DEST_DIR}/client-scripts"
if [ -f "$DEST" ] && cmp -s "$SRC" "$DEST"; then
    printf '[INFO] 官网侧已是最新，无需同步\n'
    exit 0
fi
cp "$SRC" "$DEST"
chmod 0755 "$DEST"
printf '[INFO] 已同步：%s\n' "$DEST"
printf '下一步：到 %s 提交并部署官网（milletcode.com/install.sh 即生效）\n' "$DEST_DIR"
