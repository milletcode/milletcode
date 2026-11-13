# syntax=docker/dockerfile:1

# Stage 1: Build frontend
FROM node:22-alpine AS frontend-builder
WORKDIR /src/frontend-svelte

# 使用淘宝 npm 镜像源
RUN npm config set registry https://registry.npmmirror.com

COPY frontend-svelte/package.json frontend-svelte/package-lock.json* ./
# npm 下载缓存挂成跨构建持久卷：package.json 不变时 install 层直接命中缓存，
# 变了也只需重新下载有改动的包。
RUN --mount=type=cache,target=/root/.npm npm install
COPY frontend-svelte/ ./

# 构建期重写 version.json。仓库里签入的 frontend-svelte/public/version.json 是上一次
# 发版时的快照（由 scripts/generate-version.sh 生成后提交），而构建上下文里没有 .git，
# 那个脚本在镜像里跑不起来 —— 直接读签入值就等于每个镜像都印着同一个旧版本号
# （前端 Sidebar 和 /api/health 读的都是这份 version.json）。
# 所以 version 由 CI 通过 build arg 传进来：
#   GIT_VERSION ← 版本号本身：stable 线取 ${DRONE_TAG}（tag 即版本号），
#                 dev / master 线取 ${DRONE_COMMIT_BRANCH/\//-}-${DRONE_COMMIT_SHA:0:8}
#                 （push 事件没有 tag 可取，拼上提交前 8 位；分支名里的 `/`
#                  已由 CI 换成 `-`，所以前缀就是镜像 tag 本身）
#   GIT_COMMIT  ← drone 的 ${DRONE_COMMIT_SHA}，全量 sha 在这里截短到 7 位
#   GIT_BRANCH  ← 分支名；CI 不单独传（研发线的分支名已经并进 GIT_VERSION），
#                 留默认值 unknown
#   buildTime   ← 构建时刻
# 没传 build arg 时（例如本地裸 docker build）写 unknown，而不是继续留一个假版本号；
# GIT_VERSION 传了空串时同样回落成 unknown —— drone 对空变量的替换结果就是空串，
# 会把 ARG 的默认值顶掉，宁可显示 unknown 也不要一个空白的版本号。
# 本地开发不走这条路径：make build 先跑 scripts/generate-version.sh，那里有 .git 和 tag。
# 镜像里刻意不跑那个脚本（npm 的 prebuild 钩子已移除）：构建不依赖 .git，
# 也就不要求 CI 把全量 tag 克隆下来 —— 版本信息全部由 build arg 从外部带入。
ARG GIT_VERSION=unknown
ARG GIT_COMMIT=unknown
ARG GIT_BRANCH=unknown
RUN set -eu; \
    [ -n "$GIT_VERSION" ] || GIT_VERSION=unknown; \
    printf '{\n  "version": "%s",\n  "commit": "%s",\n  "branch": "%s",\n  "buildTime": "%s"\n}\n' \
        "$GIT_VERSION" "$(printf %s "$GIT_COMMIT" | cut -c1-7)" "$GIT_BRANCH" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        > public/version.json; \
    cat public/version.json

RUN npm run build

# Stage 2: Build backend (with embedded frontend)
FROM rust:1.95-alpine AS backend-builder

# 使用阿里云 Alpine 镜像源
RUN sed -i 's/dl-cdn.alpinelinux.org/mirrors.aliyun.com/g' /etc/apk/repositories

RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

# 配置 Cargo 使用中科大镜像源
RUN mkdir -p /usr/local/cargo && \
    echo '[source.crates-io]' > /usr/local/cargo/config.toml && \
    echo 'replace-with = "ustc"' >> /usr/local/cargo/config.toml && \
    echo '[source.ustc]' >> /usr/local/cargo/config.toml && \
    echo 'registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"' >> /usr/local/cargo/config.toml

WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY backend/ ./backend/
# 两份 CHANGELOG 都必须进编译：api/changelog.rs 用 include_str! 把中英文
# 更新日志正文嵌进二进制，缺任一文件 cargo 直接编译失败（changelog 功能
# 上线时踩过；2026-11 双语拆分又踩了 zh-CN 这份没 COPY 的同一坑）。
COPY CHANGELOG.md CHANGELOG.zh-CN.md ./

# 复制前端构建产物到 WORKDIR(/src) 下的 frontend-svelte/dist，供 rust-embed 嵌入
# 路径必须与上面 frontend 阶段的 WORKDIR 一致（vite 的 outDir 是项目根下的 dist）。
COPY --from=frontend-builder /src/frontend-svelte/dist ./frontend-svelte/dist

# cargo registry 与 target 用 BuildKit cache mount 跨构建持久化（机制同 millet-invoice）：
# runner 是固定 worker（daemon 常驻、BuildKit 默认开启），同一机器第二次构建起
# 依赖全部命中缓存，只重编真正有改动的 crate——前端 dist 变化时只重编 agent-console
# 本体，不再整棵依赖树从头来。两个注意点：
#   1) cache mount 不进镜像层，所以产物必须在同一个 RUN 里 cp 到普通目录（/out）
#      供最后的 COPY 取用；
#   2) sharing=locked 让并发的双分支构建对同一缓存串行化，防 cargo 并发写坏目录。
RUN --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=locked,target=/src/target \
    mkdir -p /out && cd backend && cargo build --release \
    && cp /src/target/release/agent-console /out/agent-console && ls -la /out

# Stage 3: Runtime (single binary only, SSH mode — no AI tools needed)
FROM alpine:3.21

# 使用阿里云 Alpine 镜像源（libcap 提供 setcap，见下面的文件能力说明）
RUN sed -i 's/dl-cdn.alpinelinux.org/mirrors.aliyun.com/g' /etc/apk/repositories && \
    apk add --no-cache ca-certificates git curl sudo openssh-client libcap

# 创建 coder 用户并配置 sudo 免密
RUN adduser -D -s /bin/sh coder && \
    echo "coder ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

WORKDIR /app
# 只复制二进制，前端已嵌入（产物在构建阶段已 cp 到 /out，见 backend-builder）
COPY --from=backend-builder /out/agent-console ./milletcode
EXPOSE 3030

# 创建工作目录并设置权限。/home/coder/.milletcode 必须在镜像里预建：
# operator 把 config.toml 以 subPath 挂到 /home/coder/.milletcode/config.toml，
# 目录不存在时 kubelet 会补建父目录（root 属主 750），uid 1000 之后在它底下
# 创建 runtime/skills 等工作区就 EACCES（技能包安装第一步 create_dir_all 即炸）。
# 镜像里目录已存在，kubelet 就不再插手，属主保持 coder。
RUN mkdir -p /codes/workspace /home/coder/.milletcode && \
    chown -R coder:coder /app /codes /home/coder/.milletcode

# 给二进制挂 cap_ipc_lock 文件能力。为什么要这么做：
#   后端的 russh 建 SSH 连接时会 mlock 加密缓冲区（russh-cryptovec
#   unix.rs 失败即 panic），而容器默认 RLIMIT_MEMLOCK 只有 64KB。
#   非_root_进程自己抬不了 limit，k8s 的 securityContext.capabilities.add
#   在非 root 容器上又只进 bounding set（k3s 1.32 / containerd 2.0 实测
#   Permitted/Effective 恒为空）——非 root 进程拿到 effective cap 的
#   正规通道只有文件能力这一条。
# 注意两件事：
#   1) setcap 必须在 chown 之后 —— chown 会把文件能力 xattr 清掉；
#   2) 部署侧的 pod spec 必须把 IPC_LOCK 加进 capabilities.add：
#      exec 时文件能力要经 bounding set 掩码，bounding 里没有这一位
#      的话 exec 直接 EPERM，容器起都起不来。（operator 的 agent 容器
#      已默认加上。）
RUN setcap cap_ipc_lock=+ep /app/milletcode && \
    getcap /app/milletcode

# /config 用于存放 redb 数据库文件（sessions.redb, config.redb, approvals.redb, skills.redb）
VOLUME ["/config"]
# /codes/workspace 用于存放代码项目
VOLUME ["/codes/workspace"]

# 名字固定、内容随版本变的资源（version.json、index.html、logos/*）的缓存时长，
# 单位秒，由 backend/src/static_files.rs 在**运行时**读（不参与编译）。这些文件
# 文件名不变、内容随版本变，给 immutable 会让浏览器一年不回源，Sidebar 就永远
# 显示第一次拿到的那个版本号（实际踩过）。默认 15 分钟：发版后最多十几分钟
# 客户端跟上。
#   0 → no-cache         不设/认不出 → 代码里的默认 900
# 内容哈希命名的 assets/* 恒为 immutable，与此开关无关。
# 改它不用重新编译：
#   docker run -e MILETCODE_STATIC_CACHE_CONTROL=60 ...
# 部署侧也可以按环境在 Deployment 的 env 里调（stable 想更快跟上版本就调小）。
ARG MILETCODE_STATIC_CACHE_CONTROL=900
ENV MILETCODE_STATIC_CACHE_CONTROL=${MILETCODE_STATIC_CACHE_CONTROL}

# 切换到 coder 用户
USER coder

# skills.redb 与其余 redb 一起放 /config 卷（可写、随 PVC 持久化），
# 不落在 ~/.milletcode —— 那里在部分环境是只读挂载点。
# exec 形式（JSON 数组）：进程直接作为 PID 1 收信号，SIGTERM 能直达二进制；
# shell 形式会多套一层 sh，信号被吞、容器停得慢（buildx 也会因此告警）。
CMD ["./milletcode", "--sessions-db", "/config/sessions.redb", "--config-db", "/config/config.redb", "--approvals-db", "/config/approvals.redb", "--skills-db", "/config/skills.redb"]
