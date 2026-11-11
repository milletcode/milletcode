.PHONY: dev backend frontend build install uninstall clean kill-ports restart update \
        check init-config init \
        service-install service-uninstall service-start service-stop service-status

# Default bind host (can be overridden: make service-install HOST=127.0.0.1)
HOST ?= 0.0.0.0

# cargo 实际产物路径。不能写死 target/release/：CARGO_TARGET_DIR 指向外部卷时
# repo 根下根本没有该文件，cp 会静默失败装进旧二进制。cargo metadata 给出
# 真实 target_directory（懒展开，只在用到时求值）。
RELEASE_BIN = $(shell cd backend && cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')/release/agent-console

# Kill processes occupying backend/frontend ports
kill-ports:
	@for port in 3030 5180; do \
		pids=$$(lsof -ti tcp:$$port 2>/dev/null || true); \
		if [ -n "$$pids" ]; then \
			echo "Killing processes on port $$port: $$pids"; \
			echo $$pids | xargs kill -9 2>/dev/null || true; \
		fi; \
	done
	@sleep 1

# Default: start both backend and frontend.
# 默认非 debug 模式（debug 面板关闭、走正式校验）；需要调试面板时
# 前置 export MILLET_ENV=debug。
# MILETCODE_STATIC_CACHE_CONTROL=0 → 固定文件名的静态资源（version.json、
# index.html、logos/*）发 no-cache。本机跑的就是刚构建出来的产物，缓存一秒
# 都只会让 Sidebar 显示旧版本号。镜像里由 Dockerfile 的 ARG 设成 900（15 分钟）。
dev: kill-ports
	@echo "Starting backend (127.0.0.1:3030) and frontend (0.0.0.0:5180)..."
	@trap 'kill 0' EXIT; \
	( i=0; \
	  until curl -sf -o /dev/null http://127.0.0.1:3030/api/health || [ $$i -ge 60 ]; do \
		i=$$((i+1)); sleep 1; \
	  done; \
	  if curl -sf -o /dev/null http://127.0.0.1:3030/api/health; then \
		LOGIN_USER=$$(awk '/^\[auth\.plain\]/{f=1;next} /^\[/{f=0} f && /^user *=/{sub(/^user *= *"/,""); sub(/".*/,""); print; exit}' "$$HOME/.milletcode/config.toml"); \
		LOGIN_PASS=$$(awk '/^\[auth\.plain\]/{f=1;next} /^\[/{f=0} f && /^password *=/{sub(/^password *= *"/,""); sub(/".*/,""); print; exit}' "$$HOME/.milletcode/config.toml"); \
		if [ -n "$$LOGIN_USER" ] && [ -n "$$LOGIN_PASS" ]; then \
			echo ""; \
			echo "========================================"; \
			echo "  ✓ 服务已就绪"; \
			echo "    登录账号: $$LOGIN_USER"; \
			echo "    登录密码: $$LOGIN_PASS"; \
			echo "========================================"; \
		fi; \
	  fi ) & \
	(cd backend && MILETCODE_STATIC_CACHE_CONTROL=0 cargo run) & \
	(cd frontend-svelte && npm run dev) & \
	wait

# Start backend only (binds to 127.0.0.1)
backend:
	@echo "Starting backend server on 127.0.0.1:3030..."
	cd backend && MILETCODE_STATIC_CACHE_CONTROL=0 cargo run

# Start frontend only (binds to 0.0.0.0)
frontend:
	@echo "Starting frontend dev server on 0.0.0.0:5180..."
	cd frontend-svelte && npm run dev

# Build dependencies (backend + frontend)
build:
	@echo "Generating version info..."
	@./scripts/generate-version.sh
	@echo "Building frontend..."
	cd frontend-svelte && npm install && npm run build
	@echo "Building backend (with embedded frontend)..."
	cd backend && cargo build --release

# Install binary + register system service (auto-start on boot)
install: build service-install
	@echo ""
	@echo "========================================"
	@echo "  ✓ Installation complete"
	@echo "  Service is running. Manage with:"
	@echo "    make service-status"
	@echo "    make service-stop"
	@echo "    make service-start"
	@echo "    make service-restart"
	@echo "    make service-uninstall"
	@echo "========================================"

# Update binary only (no service registration)
update: build
	@UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		INSTALL_DIR="$$HOME/.local/bin"; \
		mkdir -p "$$INSTALL_DIR"; \
		cp "$(RELEASE_BIN)" "$$INSTALL_DIR/milletcode" || exit 1; \
		chmod +x "$$INSTALL_DIR/milletcode"; \
		echo "✓ Updated binary to $$INSTALL_DIR/milletcode"; \
	elif [ "$$UNAME" = "Linux" ]; then \
		sudo cp "$(RELEASE_BIN)" /usr/local/bin/milletcode || exit 1; \
		sudo chmod +x /usr/local/bin/milletcode; \
		echo "✓ Updated binary to /usr/local/bin/milletcode"; \
	fi

# Uninstall binary + system service
uninstall: service-uninstall
	@UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		rm -f "$$HOME/.local/bin/milletcode"; \
		echo "✓ Removed ~/.local/bin/milletcode"; \
	elif [ "$$UNAME" = "Linux" ]; then \
		sudo rm -f /usr/local/bin/milletcode; \
		echo "✓ Removed /usr/local/bin/milletcode"; \
	fi

# Clean build artifacts
clean:
	cd backend && cargo clean
	cd frontend-svelte && rm -rf dist node_modules/.vite

# Restart services (kill ports + start dev)
restart: kill-ports dev

# ============================================================
# 本地初始化
# ============================================================

# 检查环境依赖：必需工具缺失则列出并返回非零；可选工具只提示。
check:
	@MISSING_REQ=""; MISSING_OPT=""; \
	DESC_git="版本控制与版本号生成"; \
	DESC_cargo="Rust 后端构建（https://rustup.rs 安装）"; \
	DESC_rustc="Rust 编译器（随 cargo 一起安装）"; \
	DESC_node="前端构建（https://nodejs.org）"; \
	DESC_npm="前端依赖安装与构建（随 node 一起安装）"; \
	DESC_ssh="远程节点连接"; \
	DESC_lsof="make kill-ports 端口清理"; \
	DESC_openssl="init-config 生成随机登录密码"; \
	echo "环境依赖检查"; echo ""; \
	echo "必需:"; \
	for t in git cargo rustc node npm; do \
		if command -v $$t >/dev/null 2>&1; then \
			echo "  ✓ $$t — $$($$t --version 2>/dev/null | head -1)"; \
		else \
			eval desc=\$\$DESC_$$t; \
			echo "  ✗ $$t — 缺少（$$desc）"; \
			MISSING_REQ="$$MISSING_REQ $$t"; \
		fi; \
	done; \
	echo ""; \
	echo "可选:"; \
	for t in ssh lsof openssl; do \
		if command -v $$t >/dev/null 2>&1; then \
			echo "  ✓ $$t — 可用"; \
		else \
			eval desc=\$\$DESC_$$t; \
			echo "  ✗ $$t — 缺少（$$desc），对应功能将不可用"; \
			MISSING_OPT="$$MISSING_OPT $$t"; \
		fi; \
	done; \
	echo ""; \
	if [ -n "$$MISSING_REQ" ]; then \
		echo "✗ 缺少必需依赖:$$MISSING_REQ"; \
		echo "  请先安装上述工具后重试"; \
		exit 1; \
	fi; \
	echo "✓ 必需依赖齐全"; \
	if [ -n "$$MISSING_OPT" ]; then \
		echo "△ 缺少可选依赖:$$MISSING_OPT"; \
	fi

# 初始化本地最小化环境：写入 ~/.milletcode/config.toml（已存在则跳过），
# 创建工作目录 ~/Codes/milletcode 与 ~/Codes/smartcode。模板在
# scripts/init-config-template.toml（字段名由 config.rs 测试锁定）。
init-config:
	@if [ -f "$$HOME/.milletcode/config.toml" ]; then \
		echo "✓ $$HOME/.milletcode/config.toml 已存在，跳过初始化"; \
	else \
		mkdir -p "$$HOME/.milletcode" "$$HOME/Codes/milletcode" "$$HOME/Codes/smartcode"; \
		rand() { openssl rand -hex "$$1" 2>/dev/null || head -c $$(( $$1 * 2 )) /dev/urandom | od -An -tx1 | tr -d ' \n'; }; \
		USER_NAME="user-$$(rand 4)"; \
		PW="$$(rand 8)"; \
		sed -e "s/__USER__/$$USER_NAME/" -e "s/__PASSWORD__/$$PW/" \
			scripts/init-config-template.toml > "$$HOME/.milletcode/config.toml"; \
		echo "✓ 已创建 $$HOME/.milletcode/config.toml"; \
		echo "✓ 已创建工作目录 $$HOME/Codes/milletcode 与 $$HOME/Codes/smartcode"; \
	fi; \
	LOGIN_USER=$$(awk '/^\[auth\.plain\]/{f=1;next} /^\[/{f=0} f && /^user *=/{sub(/^user *= *"/,""); sub(/".*/,""); print; exit}' "$$HOME/.milletcode/config.toml"); \
	LOGIN_PASS=$$(awk '/^\[auth\.plain\]/{f=1;next} /^\[/{f=0} f && /^password *=/{sub(/^password *= *"/,""); sub(/".*/,""); print; exit}' "$$HOME/.milletcode/config.toml"); \
	echo ""; \
	echo "========================================"; \
	if [ -n "$$LOGIN_USER" ] && [ -n "$$LOGIN_PASS" ]; then \
		echo "  登录账号: $$LOGIN_USER"; \
		echo "  登录密码: $$LOGIN_PASS"; \
		echo "  改账号密码: 编辑 $$HOME/.milletcode/config.toml 的 [auth.plain] 段后重启服务"; \
	else \
		echo "  登录方式: 未使用账号密码（见 $$HOME/.milletcode/config.toml 的 [auth] 段）"; \
	fi; \
	echo "========================================"; \
	echo ""; \
	echo "提示: 服务启动后，请记得在网页「设置」中配置自己的 GPT API 密钥。"

# 一键初始化：输入 yes 确认后依次执行 check → init-config → install → service-stop → dev。
# init-config 必须在 install 之前：install 启动系统服务的瞬间后端会把默认
# config.toml 物化落盘，先写好用户配置才不会被跳过/覆盖。
# service-stop 前必须等服务健康：全新环境上服务刚启动就在初始化 redb 数据库
# （先写无 magic 的结构头、再补 magic），立刻 SIGTERM 会留下打不开的半成品库。
init:
	@echo "========================================"
	@echo "  MilletCode 一键初始化"
	@echo "========================================"
	@echo "这是一个简单的初始化脚本，将依次执行："
	@echo "  1. make check        检查依赖工具是否齐全（缺少必需依赖会中止）"
	@echo "  2. make init-config  初始化最小化配置 ~/.milletcode/config.toml（随机账号密码）"
	@echo "                       与工作目录 ~/Codes/milletcode、~/Codes/smartcode"
	@echo "  3. make install      构建后端+前端并安装，注册系统服务（开机自启；Linux 需要 sudo）"
	@echo "  4. make service-stop 停止系统服务（避免与开发模式端口冲突）"
	@echo "  5. make dev          启动开发服务（后端 127.0.0.1:3030 / 前端 0.0.0.0:5180）"
	@echo ""
	@printf '确认继续请输入 yes（其他输入取消）: '; \
	read -r ANSWER; \
	if [ "$$ANSWER" != "yes" ]; then \
		echo "已取消，未做任何改动。"; \
		exit 1; \
	fi; \
	$(MAKE) check && $(MAKE) init-config && $(MAKE) install \
	&& { echo "等待系统服务完成首次初始化..."; \
		i=0; \
		until curl -sf -o /dev/null http://127.0.0.1:3030/api/health || [ $$i -ge 30 ]; do \
			i=$$((i+1)); sleep 1; \
		done; } \
	&& $(MAKE) service-stop && $(MAKE) dev

# ============================================================
# Service management
#   macOS:  launchctl, service = com.milletcode.milletcode
#   Linux:  systemd,   service = milletcode
# ============================================================

# Install binary + write service file + enable + start
#
# PATH 注入服务环境：launchd/systemd 都不会加载用户的 shell 配置（~/.zshrc
# 是交互式 shell 才读的），而 agent 会话和平台终端继承服务进程的 env——
# 不注入的话会话里 PATH 全丢。做法：安装时用交互式 shell（zsh -ic / bash -ic）
# 真实加载一次用户的 rc 文件，把得到的完整 PATH 烧进服务定义。以后 PATH 有变
# （如 nvm 升级版本）重跑 make service-install 即同步。取不到时回退通用列表。
service-install: build
	@SVC_PATH=$$( \
		if command -v zsh >/dev/null 2>&1 && [ -f "$$HOME/.zshrc" ]; then \
			zsh -ic 'print -r -- "MILLET_PATH_MARKER:$$PATH"' 2>/dev/null \
				| sed -n 's/^MILLET_PATH_MARKER://p' | tail -1; \
		elif command -v bash >/dev/null 2>&1 && [ -f "$$HOME/.bashrc" ]; then \
			bash -ic 'echo "MILLET_PATH_MARKER:$$PATH"' 2>/dev/null \
				| sed -n 's/^MILLET_PATH_MARKER://p' | tail -1; \
		fi; ); \
	if [ -z "$$SVC_PATH" ]; then \
		SVC_PATH="$$HOME/.local/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"; \
	fi; \
	UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		INSTALL_DIR="$$HOME/.local/bin"; \
		mkdir -p "$$INSTALL_DIR"; \
		cp "$(RELEASE_BIN)" "$$INSTALL_DIR/milletcode" || exit 1; \
		chmod +x "$$INSTALL_DIR/milletcode"; \
		echo "✓ Installed binary to $$INSTALL_DIR/milletcode"; \
		\
		PLIST="$$HOME/Library/LaunchAgents/com.milletcode.milletcode.plist"; \
		mkdir -p "$$HOME/Library/LaunchAgents"; \
		LOG_DIR="$$HOME/.milletcode/logs"; \
		mkdir -p "$$LOG_DIR"; \
		echo "Writing $$PLIST... (PATH captured from user shell)"; \
		printf '%s\n' \
			'<?xml version="1.0" encoding="UTF-8"?>' \
			'<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
			'<plist version="1.0">' \
			'<dict>' \
			'    <key>Label</key>' \
			'    <string>com.milletcode.milletcode</string>' \
			'    <key>ProgramArguments</key>' \
			'    <array>' \
			"        <string>$$INSTALL_DIR/milletcode</string>" \
			"        <string>--host</string>" \
			"        <string>$(HOST)</string>" \
			'    </array>' \
			'    <key>WorkingDirectory</key>' \
			"    <string>$$HOME</string>" \
			'    <key>RunAtLoad</key>' \
			'    <true/>' \
			'    <key>KeepAlive</key>' \
			'    <true/>' \
			'    <key>StandardOutPath</key>' \
			"    <string>$$LOG_DIR/milletcode.log</string>" \
			'    <key>StandardErrorPath</key>' \
			"    <string>$$LOG_DIR/milletcode.log</string>" \
			'    <key>EnvironmentVariables</key>' \
			'    <dict>' \
			'        <key>PATH</key>' \
			"        <string>$$SVC_PATH</string>" \
			'        <key>MILETCODE_STATIC_CACHE_CONTROL</key>' \
			'        <string>0</string>' \
			'    </dict>' \
			'</dict>' \
			'</plist>' > "$$PLIST"; \
		echo "✓ Wrote $$PLIST"; \
		\
		launchctl unload "$$PLIST" 2>/dev/null || true; \
		launchctl load "$$PLIST"; \
		echo "✓ Service com.milletcode.milletcode loaded and started"; \
		echo "  Logs: $$LOG_DIR/milletcode.log"; \
	elif [ "$$UNAME" = "Linux" ]; then \
		sudo cp "$(RELEASE_BIN)" /usr/local/bin/milletcode || exit 1; \
		sudo chmod +x /usr/local/bin/milletcode; \
		echo "✓ Installed binary to /usr/local/bin/milletcode"; \
		\
		SERVICE_FILE="/etc/systemd/system/milletcode.service"; \
		echo "Writing $$SERVICE_FILE..."; \
		printf '%s\n' \
			'[Unit]' \
			'Description=MilletCode Agent Console' \
			'After=network-online.target' \
			'Wants=network-online.target' \
			'' \
			'[Service]' \
			'Type=simple' \
			'ExecStart=/usr/local/bin/milletcode --host $(HOST)' \
			'Restart=on-failure' \
			'RestartSec=5' \
			'Environment=MILETCODE_STATIC_CACHE_CONTROL=0' \
			"    Environment=PATH=$$SVC_PATH" \
			'' \
			'[Install]' \
			'WantedBy=multi-user.target' | sudo tee "$$SERVICE_FILE" > /dev/null; \
		echo "✓ Wrote $$SERVICE_FILE"; \
		\
		sudo systemctl daemon-reload; \
		sudo systemctl enable milletcode; \
		sudo systemctl restart milletcode; \
		echo "✓ Service milletcode enabled and started"; \
		echo "  Logs: sudo journalctl -u milletcode -f"; \
	else \
		echo "Unsupported platform: $$UNAME"; \
		exit 1; \
	fi

# Stop + unload/remove service (keeps binary installed)
service-uninstall:
	@UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		PLIST="$$HOME/Library/LaunchAgents/com.milletcode.milletcode.plist"; \
		if [ -f "$$PLIST" ]; then \
			launchctl unload "$$PLIST" 2>/dev/null || true; \
			rm -f "$$PLIST"; \
			echo "✓ Service com.milletcode.milletcode unloaded and removed"; \
		else \
			echo "Service not installed"; \
		fi; \
	elif [ "$$UNAME" = "Linux" ]; then \
		if systemctl is-active --quiet milletcode 2>/dev/null; then \
			sudo systemctl stop milletcode; \
		fi; \
		if systemctl is-enabled --quiet milletcode 2>/dev/null; then \
			sudo systemctl disable milletcode; \
		fi; \
		if [ -f /etc/systemd/system/milletcode.service ]; then \
			sudo rm -f /etc/systemd/system/milletcode.service; \
			sudo systemctl daemon-reload; \
			echo "✓ Service milletcode stopped, disabled, and removed"; \
		else \
			echo "Service not installed"; \
		fi; \
	fi

service-start:
	@UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		PLIST="$$HOME/Library/LaunchAgents/com.milletcode.milletcode.plist"; \
		if [ -f "$$PLIST" ]; then \
			launchctl load "$$PLIST"; \
			echo "✓ Service com.milletcode.milletcode started"; \
		else \
			echo "Service not installed. Run: make service-install"; \
			exit 1; \
		fi; \
	elif [ "$$UNAME" = "Linux" ]; then \
		sudo systemctl start milletcode; \
		echo "✓ Service milletcode started"; \
	fi

service-stop:
	@UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		PLIST="$$HOME/Library/LaunchAgents/com.milletcode.milletcode.plist"; \
		if [ -f "$$PLIST" ]; then \
			launchctl unload "$$PLIST"; \
			echo "✓ Service com.milletcode.milletcode stopped"; \
		else \
			echo "Service not installed"; \
		fi; \
	elif [ "$$UNAME" = "Linux" ]; then \
		sudo systemctl stop milletcode; \
		echo "✓ Service milletcode stopped"; \
	fi

service-status:
	@UNAME=$$(uname -s); \
	if [ "$$UNAME" = "Darwin" ]; then \
		if launchctl list | grep -q com.milletcode.milletcode; then \
			echo "● com.milletcode.milletcode is loaded"; \
			launchctl list | grep com.milletcode.milletcode; \
		else \
			echo "○ com.milletcode.milletcode is not loaded"; \
		fi; \
	elif [ "$$UNAME" = "Linux" ]; then \
		sudo systemctl status milletcode --no-pager || true; \
	fi

service-restart: service-stop
	@sleep 2
	@$(MAKE) service-start
