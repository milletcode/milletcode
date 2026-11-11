mod agents;
mod api;
mod auth;
mod config;
mod db;
mod events;
mod logging;
mod projects;
mod runtime;
mod sanitize;
mod static_files;
mod stream_json;
mod terminal;
mod tunnel_frame;
mod ws;

use std::time::Duration;

use clap::Parser;
use config::ConfigManager;
use events::EventBus;
use agents::{AgentRegistry, SessionManager};
use agents::ClaudeAdapter;
use agents::DeepSeekAdapter;
use agents::SmartcodeAdapter;
use projects::manager::ProjectManager;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(name = "milletcode")]
#[command(about = "MilletCode - AI coding agent platform")]
struct Args {
    /// Path to the sessions database file
    /// Defaults to ~/.milletcode/sessions.redb if not specified
    #[arg(long, env = "MILLET_SESSIONS_DB", default_value = "~/.milletcode/sessions.redb")]
    sessions_db: String,

    /// Path to the config database file
    /// Defaults to ~/.milletcode/config.redb if not specified
    #[arg(long, env = "MILLET_CONFIG_DB", default_value = "~/.milletcode/config.redb")]
    config_db: String,

    /// Path to the decision-history (audit) database file
    /// Defaults to ~/.milletcode/approvals.redb if not specified
    #[arg(long, env = "MILLET_APPROVALS_DB", default_value = "~/.milletcode/approvals.redb")]
    approvals_db: String,

    /// Path to the skill install-record database file
    /// Defaults to ~/.milletcode/skills.redb if not specified
    #[arg(long, env = "MILLET_SKILLS_DB", default_value = "~/.milletcode/skills.redb")]
    skills_db: String,

    /// Host address to bind the server to (e.g. 127.0.0.1, 0.0.0.0)
    #[arg(long, env = "MILLET_HOST", default_value = "0.0.0.0")]
    host: String,
}

pub struct AppState {
    pub config_manager: std::sync::Arc<ConfigManager>,
    pub event_bus: std::sync::Arc<EventBus>,
    pub session_manager: std::sync::Arc<SessionManager>,
    pub registry: std::sync::Arc<AgentRegistry>,
    pub project_manager: std::sync::Arc<ProjectManager>,
    pub settings_store: db::SettingsStore,
    pub commands_store: db::CommandsStore,
    pub project_store: db::ProjectStore,
    pub env_store: db::EnvStore,
    pub issue_store: db::IssueStore,
    pub approval_store: db::ApprovalStore,
    /// 第三方技能安装留痕（skills.redb）
    pub skills_store: db::SkillsStore,
    pub env: String,
    /// WS connection registry: ProjectKey -> connection count. Used to enforce max_total_sessions.
    pub ws_registry: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<crate::ws::ProjectKey, usize>>>,
    /// Total WS subscription count across all connections.
    pub ws_subscription_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Terminal session manager for web terminal feature
    pub terminal_manager: std::sync::Arc<tokio::sync::Mutex<crate::terminal::TerminalManager>>,
    /// One-time tickets authorizing session WebSocket upgrades
    pub ws_tickets: std::sync::Arc<crate::auth::WsTickets>,
    /// plain 模式的内存会话表（login 签发 / 中间件校验 / logout 注销）
    pub plain_sessions: std::sync::Arc<crate::auth::PlainSessions>,
    /// 登录验证码（内存 TTL，单次使用）
    pub captcha_store: std::sync::Arc<crate::auth::CaptchaStore>,
    /// 登录 IP 限速（滑动窗口失败计数）
    pub login_throttle: std::sync::Arc<crate::auth::LoginThrottle>,
    /// Per-host throttle cache for remote (ssh) `ps` output in the debug panel
    pub remote_ps_cache: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<String, crate::api::debug::RemotePsCache>>>,
    /// Port forwarding: workspace ports bridged to console-host listeners
    pub port_forwards: std::sync::Arc<tokio::sync::Mutex<crate::runtime::port_forward::PortForwardManager>>,
    /// millet-tunnel: user-side tunnels bridged over WS to workspace ports
    pub tunnels: std::sync::Arc<tokio::sync::Mutex<crate::runtime::tunnel::TunnelManager>>,
    /// /p/ 反代的 per-session SSH 连接缓存（避免每请求重建 SSH 连接）
    pub proxy_ssh: std::sync::Arc<tokio::sync::Mutex<api::proxy::ProxySshCache>>,
    /// 技能包分发任务表（最近 20 条）
    pub skill_tasks: std::sync::Arc<crate::runtime::skills::SkillTaskRegistry>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Load config first so we know where/how to log
    let config_manager = std::sync::Arc::new(ConfigManager::new()?);

    // Initialize logging based on config — guard must live for process lifetime
    let _logging_guard = logging::init(&config_manager.config().logs);

    tracing::info!("Starting milletcode...");
    tracing::info!("Config loaded from ~/.milletcode/config.toml");

    // Validate auth configuration — exit if missing
    if let Err(e) = config_manager.config().auth.validate() {
        tracing::error!(
            "Authentication is required but not configured: {}. \
             Please set [auth] in ~/.milletcode/config.toml",
            e
        );
        eprintln!(
            "ERROR: Authentication is required but not configured: {}. \
             Please set [auth] in ~/.milletcode/config.toml",
            e
        );
        std::process::exit(1);
    }

    // Config DB（settings_store）先于 adapter 打开：SmartcodeAdapter 依赖它读取
    // 系统设置的 GPT API 配置做启动注入
    let config_db_path = shellexpand::tilde(&args.config_db)
        .into_owned();
    let config_db_path = std::path::PathBuf::from(config_db_path);
    tracing::info!("Config DB: {}", config_db_path.display());
    let (settings_store, commands_store) = db::open_config_db(&config_db_path).expect("Failed to open config database");

    let event_bus = std::sync::Arc::new(EventBus::new(256));
    let registry = std::sync::Arc::new(AgentRegistry::new());

    let claude_adapter = std::sync::Arc::new(ClaudeAdapter::new(event_bus.clone(), config_manager.clone()));
    let deepseek_adapter = std::sync::Arc::new(DeepSeekAdapter::new(event_bus.clone()));
    let smartcode_adapter = std::sync::Arc::new(SmartcodeAdapter::new(event_bus.clone(), config_manager.clone(), settings_store.clone()));
    registry.register(claude_adapter);
    registry.register(deepseek_adapter);
    registry.register(smartcode_adapter);

    let project_manager = std::sync::Arc::new(ProjectManager::new(config_manager.clone()));
    // Internal skills-installer project (refresh() re-registers it after every scan)
    project_manager.register_internal_skills_project();
    let scan_interval = Duration::from_secs(
        config_manager.config().project.scan_interval_secs,
    );
    let scanner_cancel = CancellationToken::new();

    // Run initial scan in background to avoid blocking startup on slow SSH connections
    let pm_for_scan = project_manager.clone();
    tokio::spawn(async move {
        pm_for_scan.refresh();
    });

    if config_manager.config().project.scanner_enabled {
        ProjectManager::spawn_scanner(
            project_manager.clone(),
            scan_interval,
            scanner_cancel.clone(),
        );
    }

    // Initialize separate redb databases: sessions, approvals, and config
    // Expand tilde (~) in paths to home directory
    let sessions_db_path = shellexpand::tilde(&args.sessions_db)
        .into_owned();
    let sessions_db_path = std::path::PathBuf::from(sessions_db_path);

    tracing::info!("Sessions DB: {}", sessions_db_path.display());

    let store = db::open_session_db(&sessions_db_path).expect("Failed to open session database");
    let project_db_path = config_db_path.with_file_name("projects.redb");
    let project_store = db::open_project_db(&project_db_path).expect("Failed to open project database");
    let envs_db_path = config_db_path.with_file_name("envs.redb");
    let (env_store, issue_store) = db::open_env_db(&envs_db_path).expect("Failed to open env database");
    let approvals_db_path = shellexpand::tilde(&args.approvals_db).into_owned();
    let approvals_db_path = std::path::PathBuf::from(approvals_db_path);
    tracing::info!("Approvals DB: {}", approvals_db_path.display());
    let approval_store = db::open_approval_db(&approvals_db_path).expect("Failed to open approvals database");
    let skills_db_path = shellexpand::tilde(&args.skills_db).into_owned();
    let skills_db_path = std::path::PathBuf::from(skills_db_path);
    tracing::info!("Skills DB: {}", skills_db_path.display());
    let skills_store = db::open_skills_db(&skills_db_path).expect("Failed to open skills database");
    match skills_store.finalize_interrupted_runs() {
        Ok(n) if n > 0 => tracing::info!("Finalized {} interrupted skill install runs", n),
        Ok(_) => {}
        Err(e) => tracing::warn!("Skill install run sweep failed: {}", e),
    }

    // === Startup checks and initialization ===
    // This section runs various checks to ensure the system is properly initialized.
    // Add new checks here as needed.

    // Check 1: Initialize default commands if empty (backfill new defaults by
    // title), gated by updates.auto_config_update
    tracing::info!("Checking default commands...");
    if config_manager.config().updates.auto_config_update {
        if let Err(e) = commands_store.init_defaults_if_empty() {
            tracing::warn!("Failed to initialize default commands: {}", e);
        } else {
            match commands_store.list() {
                Ok(cmds) => tracing::info!("Default commands check passed, {} commands loaded", cmds.len()),
                Err(e) => tracing::warn!("Failed to list commands after init: {}", e),
            }
        }
    } else {
        tracing::info!("Auto config update disabled (updates.auto_config_update=false), skipping defaults backfill");
    }

    // TODO: Add more startup checks here
    // Check 2: ...
    // Check 3: ...

    // === End of startup checks ===

    // Load agent overrides from config.redb (user changes to name/enabled)
    if let Ok(agent_overrides) = settings_store.list("agents.") {
        for (key, value) in agent_overrides {
            if let Some(agent) = serde_json::from_str::<crate::config::AgentConfig>(&value).ok() {
                let agent_id = key.strip_prefix("agents.").unwrap_or(&key);
                let _ = config_manager.update_agent(agent_id, agent);
            }
        }
    }

    let session_manager = std::sync::Arc::new(SessionManager::new(
        registry.clone(),
        event_bus.clone(),
        config_manager.clone(),
        project_manager.clone(),
        store,
        approval_store.clone(),
        issue_store.clone(),
    ));

    // Restore in-memory session state from database (metadata only, no process respawn)
    session_manager.restore_sessions();

    // Backend-driven idle notification — server-side detection keeps working
    // when the browser tab is closed or frozen.
    session_manager.spawn_idle_notifier();

    let env = std::env::var("MILLET_ENV").unwrap_or_else(|_| "production".to_string());
    tracing::info!("Running environment: {}", env);

    let listen = {
        let web = &config_manager.config().web;
        let addr = web.listen_addr(&args.host);
        if !web.listen_uri_configured() {
            tracing::info!(
                "[web] listen_uri not set; binding via legacy --host + default port: {} (set [web] listen_uri to override)",
                addr
            );
        }
        addr
    };
    let state = std::sync::Arc::new(AppState {
        config_manager,
        event_bus,
        session_manager,
        registry,
        project_manager,
        settings_store,
        commands_store,
        project_store,
        env_store,
        issue_store,
        approval_store,
        skills_store,
        env,
        ws_registry: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        ws_subscription_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        terminal_manager: std::sync::Arc::new(tokio::sync::Mutex::new(crate::terminal::TerminalManager::new())),
        ws_tickets: std::sync::Arc::new(crate::auth::WsTickets::new()),
        plain_sessions: std::sync::Arc::new(crate::auth::PlainSessions::new()),
        captcha_store: std::sync::Arc::new(crate::auth::CaptchaStore::new()),
        login_throttle: std::sync::Arc::new(crate::auth::LoginThrottle::new()),
        remote_ps_cache: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        port_forwards: std::sync::Arc::new(tokio::sync::Mutex::new(crate::runtime::port_forward::PortForwardManager::new())),
        tunnels: std::sync::Arc::new(tokio::sync::Mutex::new(crate::runtime::tunnel::TunnelManager::new())),
        proxy_ssh: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        skill_tasks: std::sync::Arc::new(crate::runtime::skills::SkillTaskRegistry::default()),
    });

    // Initialize Claude settings on startup (local + all remote SSH hosts).
    // force=false + existing file → only permissions.ask rules are patched in;
    // model/env stay as the user configured them.
    tracing::info!("Syncing Claude settings.json...");
    if let Err(e) = api::config::write_claude_settings(&state, None, false).await {
        tracing::warn!("Local Claude settings initialization failed: {}", e);
    }

    // Sync to all remote SSH hosts (container restarts wipe ~/.claude).
    // Discovery paths with sync_settings = false are skipped here: the host keeps its
    // own ~/.claude/settings.json. Explicit writes (model switch, GPT API save) still
    // go through — only this best-effort startup pass is suppressed.
    let startup_config = state.config_manager.config();
    let (remote_keywords, skipped): (Vec<String>, Vec<String>) = state
        .project_manager
        .remote_keywords()
        .into_iter()
        .partition(|kw| {
            startup_config
                .project
                .get_discovery_path(kw)
                .map(|dp| dp.sync_settings)
                .unwrap_or(true)
        });
    if !skipped.is_empty() {
        tracing::info!(
            "Skipping startup settings.json sync for {} host(s) with sync_settings = false: {:?}",
            skipped.len(),
            skipped
        );
    }
    if !remote_keywords.is_empty() {
        tracing::info!("Syncing Claude settings.json to {} remote host(s)...", remote_keywords.len());
        let state_clone = state.clone();
        let keywords = remote_keywords.clone();
        tokio::spawn(async move {
            for keyword in &keywords {
                match api::config::write_remote_claude_settings(&state_clone, None, keyword, false).await {
                    Ok(msg) => tracing::info!("Remote settings sync OK [{}]: {}", keyword, msg),
                    Err(e) => tracing::warn!("Remote settings sync failed [{}]: {}", keyword, e),
                }
            }
        });
    }

    let router = api::create_router(state);

    // listen_uri may be a hostname (not just IP:port), so bind via ToSocketAddrs.
    let listener = tokio::net::TcpListener::bind(&listen).await?;

    tracing::info!("Server listening on http://{}", listen);
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}
