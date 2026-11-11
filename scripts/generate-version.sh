#!/bin/bash
# Generate version.json from git information
# Usage: ./scripts/generate-version.sh [version]
# 不传参时用 git describe --tags，保留完整输出：正好在 tag 上得到 v0.9.2，
# 离开 tag 得到 v0.9.2-2-g9d289a8（v0.9.2 之后还有 2 个 commit）。
# 后缀是有意保留的 —— 只取 base tag 会让一个含未发布 commit 的构建自称 v0.9.2，
# 这正是「本地版本号和仓库 tag 对不上」的根源。
# 发版流程必须显式传版本号（如 ./scripts/generate-version.sh v0.9.2）：那一刻新 tag
# 还没创建，describe 只会给出「上一个 tag + 1」，不传参会把旧版本号写进去。

set -e

# Get version from argument or git tag
if [ -n "$1" ]; then
  VERSION="$1"
else
  # Full describe output; no suffix when HEAD is exactly on a tag.
  VERSION=$(git describe --tags 2>/dev/null || echo "")
  if [ -z "$VERSION" ]; then
    # 拿不到 git 或 tag（镜像构建上下文、无 tag 的浅克隆）时沿用签入值，
    # 而不是写 unknown —— 一个真实的旧版本号比 unknown 有用。
    VERSION=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      frontend-svelte/public/version.json 2>/dev/null | head -1 || echo "")
  fi
  if [ -z "$VERSION" ]; then
    VERSION="unknown"
  fi
fi

# Get commit hash
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")

# Get build time
BUILD_TIME=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# Get current branch
BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")

# Output version.json
cat > frontend-svelte/public/version.json << EOF
{
  "version": "${VERSION}",
  "commit": "${COMMIT}",
  "branch": "${BRANCH}",
  "buildTime": "${BUILD_TIME}"
}
EOF

echo "Generated version.json: ${VERSION} (${COMMIT})"
