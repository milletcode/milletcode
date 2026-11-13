use std::collections::HashMap;
use std::path::PathBuf;

use dirs;
use serde::{Deserialize, Serialize};


// ---------------------------------------------------------------------------
// SSH global configuration
// ---------------------------------------------------------------------------

/// Global SSH configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshGlobalConfig {
    /// Default SSH config file path
    #[serde(default = "default_ssh_config_path")]
    pub config_path: String,
    /// Default SSH passphrase for encrypted keys
    #[serde(default)]
    pub passphrase: Option<String>,
}

impl Default for SshGlobalConfig {
    fn default() -> Self {
        Self {
            config_path: default_ssh_config_path(),
            passphrase: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Agent configuration
// ---------------------------------------------------------------------------

/// Per-agent instance configuration (no longer carries hostname/ssh fields)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Display name shown in UI
    pub name: String,
    /// Additional CLI args passed when launching
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// API key for auth
    #[serde(default)]
    pub api_key: Option<String>,
    /// Auto-approve all agent actions (no user prompts)
    #[serde(default)]
    pub auto_approve: bool,
    /// Whether this agent is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Whether MilletCode is allowed to overwrite this agent's config files
    #[serde(default = "default_true")]
    pub allow_config_overwrite: bool,
    /// Path to agent binary (optional, can be overridden by discovery_paths)
    #[serde(default)]
    pub path: Option<String>,
    /// Remote path to agent binary (optional). When remote_mode is set and this
    /// is None, `path` is used as the default (still connected via remote_mode).
    #[serde(default)]
    pub remote_path: Option<String>,
    /// 发布阶段标识（stable/beta/alpha），UI 据此显示角标，stable 不显示
    #[serde(default = "default_release_channel")]
    pub release_channel: String,
}

/// Agent settings — top-level `[agent]` section
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Default agent instance key to use for new sessions (e.g. "claude", "codex")
    #[serde(default = "default_agent_key")]
    pub default_agent: Option<String>,
    /// How long (seconds) to wait for approval before auto-deny
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_secs: u64,
    /// Agent instances keyed by name (e.g. "claude", "codex")
    #[serde(default = "default_agent_instances")]
    pub instances: HashMap<String, AgentConfig>,
}

fn default_agent_key() -> Option<String> {
    Some("claude".to_string())
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            default_agent: default_agent_key(),
            approval_timeout_secs: default_approval_timeout(),
            instances: default_agent_instances(),
        }
    }
}

impl AgentSettings {
    /// Find enabled agent by instance key (e.g. "claude")
    pub fn find_enabled(&self, agent_id: &str) -> Option<(&String, &AgentConfig)> {
        self.instances
            .get_key_value(agent_id)
            .filter(|(_, c)| c.enabled)
    }

    /// Alias for find_enabled — looks up by instance key
    pub fn find_by_id(&self, agent_id: &str) -> Option<(&String, &AgentConfig)> {
        self.find_enabled(agent_id)
    }
}

// ---------------------------------------------------------------------------
// Web configuration
// ---------------------------------------------------------------------------

/// Web service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    /// Bind address for the HTTP listener, e.g. "127.0.0.1:3030" or "0.0.0.0:3030".
    /// A bare port ("3030") binds on 0.0.0.0. When unset, --host/MILLET_HOST
    /// with the default port 3030 applies. There is deliberately NO public-URL
    /// setting: production domains are dynamic (StatefulSet), so outbound
    /// links (e.g. idle notifications) use the browsing session's own origin.
    #[serde(default)]
    pub listen_uri: Option<String>,
    /// File upload configuration
    #[serde(default)]
    pub upload: UploadConfig,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen_uri: None,
            upload: UploadConfig::default(),
        }
    }
}

impl WebConfig {
    /// True when listen_uri is set to a non-blank value.
    pub fn listen_uri_configured(&self) -> bool {
        self.listen_uri
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
    }

    /// Resolve the bind address for the HTTP listener. listen_uri wins; a bare
    /// port binds on 0.0.0.0; otherwise --host with the default port 3030.
    pub fn listen_addr(&self, legacy_host: &str) -> String {
        if let Some(uri) = self.listen_uri.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(port) = uri.parse::<u16>() {
                return format!("0.0.0.0:{}", port);
            }
            return uri.to_string();
        }
        format!("{}:3030", legacy_host)
    }

    /// Port for fallback URL composition (e.g. idle notifications when no WS
    /// Origin has been learned yet). Parses listen_uri, else the default 3030.
    /// Only a last-resort guess — the browsing origin is authoritative.
    pub fn advertised_port(&self) -> u16 {
        if let Some(uri) = self.listen_uri.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(port) = uri.parse::<u16>() {
                return port;
            }
            if let Some((_, port)) = uri.rsplit_once(':') {
                if let Ok(port) = port.parse::<u16>() {
                    return port;
                }
            }
        }
        3030
    }
}

// ---------------------------------------------------------------------------
// Project configuration (merged ProjectConfig + ProjectSettings)
// ---------------------------------------------------------------------------

/// SSH override for a discovery path
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshOverride {
    /// SSH config file path for this discovery path
    #[serde(default = "default_ssh_config_path")]
    pub config_path: String,
}

/// Discovery path configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryPath {
    /// Path to scan (e.g. "~/Codes/milletcode")
    pub path: String,
    /// Hostname for remote paths (empty for local)
    #[serde(default)]
    pub hostname: String,
    /// Remote mode: "" = local, "ssh" = SSH remote (future: "s3"/"grpc"/"nfs")
    #[serde(default)]
    pub remote_mode: String,
    /// Whether this path is a mobile project root (HarmonyOS/iOS/Android)
    #[serde(default)]
    pub is_mobile_home: bool,
    /// Whether to sync ~/.claude/settings.json to this remote host during the
    /// startup sync. Default: true. Setting false only skips that best-effort
    /// startup pass — an explicit model switch or GPT API save still writes.
    #[serde(default = "default_true")]
    pub sync_settings: bool,
    /// Whether sessions on this path may switch the model. Default: true. Setting
    /// false hides the model selector in the UI and rejects explicit switch_model
    /// requests — for hosts whose Claude settings are managed outside milletcode.
    #[serde(default = "default_true")]
    pub sync_models: bool,
    /// SSH override for this discovery path
    #[serde(default)]
    pub ssh: Option<SshOverride>,
    /// Agent binary paths keyed by agent instance name
    /// e.g. { "claude": "/home/user/.local/bin/claude" }
    #[serde(default)]
    pub paths: HashMap<String, String>,
}

/// Session-related settings (under `[project.sessions]`)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSettings {
    /// Max subscriptions per project + agent-type.
    /// "auto" = unlimited, or a number like "10".
    #[serde(default = "default_max_subscriptions")]
    pub max_subscriptions_per_project: String,
    /// Max total sessions across all projects.
    /// "auto" = calculated from CPU cores, or a number like "64".
    #[serde(default = "default_max_total_sessions")]
    pub max_total_sessions: String,
    /// 闲置提醒全局开关 — 关闭时 /api/sessions/idle 恒为空清单。
    #[serde(default = "default_idle_reminder_enabled")]
    pub idle_reminder_enabled: bool,
    /// 闲置判定阈值（秒）：进程存活且连续该时长无任何活动（用户消息或 agent 事件）即视为闲置。
    /// 用户级可调（前端设置页），合法范围 30..=900。
    #[serde(default = "default_idle_seconds")]
    pub idle_seconds: u64,
}

impl SessionSettings {
    /// Calculate max total sessions based on CPU cores.
    pub fn max_total_sessions(&self) -> usize {
        if self.max_total_sessions == "auto" {
            let cpu_count = num_cpus::get();
            if cpu_count <= 16 {
                (cpu_count * 2).min(32)
            } else if cpu_count <= 32 {
                (cpu_count as f64 * 1.5) as usize
            } else {
                cpu_count.min(64)
            }
        } else {
            self.max_total_sessions.parse().unwrap_or(64)
        }
    }

    /// Parse max_subscriptions_per_project.
    /// Returns None if "auto" (unlimited), or Some(number).
    pub fn max_subscriptions_per_project(&self) -> Option<usize> {
        if self.max_subscriptions_per_project == "auto" {
            None
        } else {
            self.max_subscriptions_per_project.parse().ok()
        }
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            max_subscriptions_per_project: default_max_subscriptions(),
            max_total_sessions: default_max_total_sessions(),
            idle_reminder_enabled: default_idle_reminder_enabled(),
            idle_seconds: default_idle_seconds(),
        }
    }
}

pub(crate) fn default_idle_reminder_enabled() -> bool {
    true
}

/// Default idle threshold: 120 seconds. Bounds live in the API layer (30..=900).
pub(crate) fn default_idle_seconds() -> u64 {
    120
}

/// Project configuration — top-level `[project]` section
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Allow creating new projects (always true in debug mode)
    #[serde(default)]
    pub allow_create: bool,
    /// Allow archiving projects (adds .ARCHIVED.md marker; always true in debug mode)
    #[serde(default)]
    pub allow_archive: bool,
    /// Allow deleting projects (removes directory; always true in debug mode)
    #[serde(default)]
    pub allow_delete: bool,
    /// How often to rescan for projects (seconds)
    #[serde(default = "default_project_scan_interval")]
    pub scan_interval_secs: u64,
    /// Whether to enable automatic project scanning
    #[serde(default)]
    pub scanner_enabled: bool,
    /// Session-related settings
    #[serde(default)]
    pub sessions: SessionSettings,
    /// Project discovery paths configuration
    /// Key: keyword (e.g. "milletcode", "vip")
    #[serde(default = "default_project_discovery_paths")]
    pub discovery_paths: HashMap<String, DiscoveryPath>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            allow_create: false,
            allow_archive: false,
            allow_delete: false,
            scan_interval_secs: default_project_scan_interval(),
            scanner_enabled: false,
            sessions: SessionSettings::default(),
            discovery_paths: default_project_discovery_paths(),
        }
    }
}

impl ProjectConfig {
    /// Check if a keyword is a mobile discovery root
    pub fn is_mobile_root(&self, keyword: &str) -> bool {
        self.discovery_paths
            .get(keyword)
            .map(|dp| dp.is_mobile_home)
            .unwrap_or(false)
    }

    /// Get list of mobile discovery root keywords
    pub fn mobile_discovery_roots(&self) -> Vec<String> {
        self.discovery_paths
            .iter()
            .filter(|(_, dp)| dp.is_mobile_home)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Get the DiscoveryPath configuration for a keyword
    pub fn get_discovery_path(&self, keyword: &str) -> Option<&DiscoveryPath> {
        self.discovery_paths.get(keyword)
    }
}

// ---------------------------------------------------------------------------
// File upload configuration
// ---------------------------------------------------------------------------

/// File upload configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadConfig {
    /// Storage type: "local" (current), "oss", "s3" (future)
    #[serde(default = "default_storage_type")]
    pub storage_type: String,
    /// Local storage root directory, default: /tmp/milletcode-uploads
    #[serde(default = "default_upload_dir")]
    pub local_dir: String,
    /// Max upload file size in MB, default: 10
    #[serde(default = "default_max_size_mb")]
    pub max_size_mb: usize,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            storage_type: default_storage_type(),
            local_dir: default_upload_dir(),
            max_size_mb: default_max_size_mb(),
        }
    }
}

// ---------------------------------------------------------------------------
// Log configuration
// ---------------------------------------------------------------------------

/// Log output destination
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogOutput {
    Stdout,
    File,
    Both,
}

impl Default for LogOutput {
    fn default() -> Self {
        LogOutput::Both
    }
}

/// Log verbosity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Debug
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Error => write!(f, "error"),
            LogLevel::Warn => write!(f, "warn"),
            LogLevel::Info => write!(f, "info"),
            LogLevel::Debug => write!(f, "debug"),
            LogLevel::Trace => write!(f, "trace"),
        }
    }
}

/// Log configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default)]
    pub session_log_dir: Option<String>,
    #[serde(default)]
    pub output: LogOutput,
    #[serde(default)]
    pub level: LogLevel,
    #[serde(default = "default_log_max_files")]
    pub max_files: usize,
    #[serde(default = "default_session_log_enabled")]
    pub session_log_enabled: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            session_log_dir: None,
            output: LogOutput::default(),
            level: LogLevel::default(),
            max_files: default_log_max_files(),
            session_log_enabled: default_session_log_enabled(),
        }
    }
}

// ---------------------------------------------------------------------------
// Auth configuration
// ---------------------------------------------------------------------------

/// `[auth.plain]` — 明文配置认证的凭据。凭据只来自配置文件，不落库。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlainAuthConfig {
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
    /// 静态 API token（Bearer，sk- 前缀），留给客户侧长期使用。
    /// 校验时只认 sk- 开头的 Bearer；留空 = 静态 token 关闭。
    #[serde(default)]
    pub token: String,
    /// 时间槽 token 的 HMAC 种子（内部测试用，绝不发给客户）。
    /// Bearer 非 sk- 前缀即走 `slot:HMAC(secret,slot)` 校验；留空 = 时间槽关闭。
    #[serde(default)]
    pub token_secret: String,
}

/// `[auth.oauth2_proxy]` — oauth2_proxy 模式配置。上游由 OAuth2 Proxy 注入
/// `X-Auth-Request-User`；这里的 basic_auth 凭据是**后端自身的保护层**：
/// 绕过 proxy 直连后端的请求会收到 `WWW-Authenticate: Basic` 挑战。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuth2ProxyConfig {
    /// Basic 保护层用户名（也是后端给 oauth2-proxy 回注 Basic 头时用的凭据）
    #[serde(default)]
    pub basic_auth_user: Option<String>,
    #[serde(default)]
    pub basic_auth_password: Option<String>,
    /// OAuth2 Proxy 的对外域名（/oauth2/* 反代目标）
    #[serde(default)]
    pub proxy_domain: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub redirect_url: Option<String>,
    #[serde(default)]
    pub allowed_orgs: Vec<String>,
    #[serde(default)]
    pub allowed_admins: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// 自定义根证书路径（proxy 自签证书时）
    #[serde(default)]
    pub ssl_pem: Option<String>,
    /// Parent cookie domain (e.g. "example.com") used by /api/logout to also
    /// clear SSO cookies scoped above the host. Empty/absent = host-only clear.
    #[serde(default)]
    pub cookie_domain: Option<String>,
}

impl OAuth2ProxyConfig {
    /// 是否配置了任一 oauth2_proxy 字段——旧配置（平铺）之外的嵌套自动识别用。
    fn is_configured(&self) -> bool {
        self.basic_auth_user.is_some()
            || self.basic_auth_password.is_some()
            || self.proxy_domain.is_some()
            || self.base_url.is_some()
            || self.redirect_url.is_some()
            || self.ssl_pem.is_some()
            || self.cookie_domain.is_some()
            || !self.allowed_orgs.is_empty()
            || !self.allowed_admins.is_empty()
            || !self.allowed_users.is_empty()
    }
}

/// `[auth.local]` — 预留：数据库版认证（含 MFA）。占位以保证四种 mode 的
/// 配置层级一致；选中该 mode 当前的 validate 会直接拒绝。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalAuthConfig {}

/// `[auth.oauth2]` — 预留：直连 OIDC。占位同上。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OAuth2DirectConfig {}

/// Authentication configuration。四种 mode 各占一个子表，层级一致：
/// `[auth.plain]` / `[auth.local]` / `[auth.oauth2_proxy]` / `[auth.oauth2]`。
/// mode 必填，不做旧平铺格式（oauth_proxy_domain 等平级字段）的兼容读取。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    /// plain | local | oauth2_proxy | oauth2 —— 互斥，同时只激活一种。
    /// local 与 oauth2 目前为预留，选中即启动失败。留空启动即报错。
    #[serde(default)]
    pub mode: String,
    /// 登录限速采信 X-Forwarded-For 的可信代理网段（CIDR 或裸 IP）。
    /// 仅当 TCP 对端命中网段且带 x-millet-edge 标记头时，XFF 末位才作为
    /// 限速 key；空列表 = 默认 loopback + RFC1918 内网段。x-millet-edge
    /// 头本身可被任意客户端伪造，信任判定必须基于不可伪造的 TCP 对端地址。
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// plain 模式凭据（[auth.plain]）
    #[serde(default)]
    pub plain: PlainAuthConfig,
    /// local 模式配置（[auth.local]，预留占位）
    #[serde(default)]
    pub local: LocalAuthConfig,
    /// oauth2_proxy 模式配置（[auth.oauth2_proxy]）
    #[serde(default)]
    pub oauth2_proxy: OAuth2ProxyConfig,
    /// oauth2 直连模式配置（[auth.oauth2]，预留占位）
    #[serde(default)]
    pub oauth2: OAuth2DirectConfig,
}

impl AuthConfig {
    /// 运行时实际生效的模式。mode 必填，无旧格式自动识别。
    pub fn effective_mode(&self) -> &str {
        &self.mode
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for entry in &self.trusted_proxies {
            if crate::auth::IpNet::parse(entry).is_none() {
                anyhow::bail!("auth.trusted_proxies entry is not a valid CIDR/IP: {entry}");
            }
        }
        match self.effective_mode() {
            "plain" => {
                if self.plain.user.is_empty() {
                    anyhow::bail!("auth.plain.user is required when mode = \"plain\"");
                }
                if self.plain.password.is_empty() {
                    anyhow::bail!("auth.plain.password is required when mode = \"plain\"");
                }
            }
            "local" => {
                anyhow::bail!("auth.mode = \"local\"（数据库版认证，含 MFA）为预留模式，尚未实现，请使用 \"plain\"");
            }
            "oauth2" => {
                anyhow::bail!("auth.mode = \"oauth2\"（直连 OIDC）为预留模式，尚未实现，请使用 \"oauth2_proxy\"");
            }
            "oauth2_proxy" => {
                let p = &self.oauth2_proxy;
                if p.proxy_domain.is_none() || p.proxy_domain.as_deref() == Some("") {
                    anyhow::bail!("auth.oauth2_proxy.proxy_domain is required when mode = \"oauth2_proxy\"");
                }
                if p.basic_auth_user.is_none() || p.basic_auth_user.as_deref() == Some("") {
                    anyhow::bail!("auth.oauth2_proxy.basic_auth_user is required when mode = \"oauth2_proxy\"");
                }
                if self.basic_auth_password_resolved().is_none() {
                    anyhow::bail!("auth.oauth2_proxy.basic_auth_password or BASIC_AUTH_PASSWORD environment variable is required when mode = \"oauth2_proxy\"");
                }
                if p.allowed_admins.is_empty() && p.allowed_users.is_empty() {
                    anyhow::bail!("at least one of auth.oauth2_proxy.allowed_admins or auth.oauth2_proxy.allowed_users must be configured when mode = \"oauth2_proxy\"");
                }
            }
            "" => {
                anyhow::bail!("auth.mode is required (plain | local | oauth2_proxy | oauth2)");
            }
            other => {
                anyhow::bail!("unsupported auth.mode: \"{}\" (allowed: plain | local | oauth2_proxy | oauth2)", other);
            }
        }
        Ok(())
    }

    pub fn basic_auth_password_resolved(&self) -> Option<String> {
        std::env::var("BASIC_AUTH_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.oauth2_proxy
                    .basic_auth_password
                    .clone()
                    .filter(|s| !s.is_empty())
            })
    }

    pub fn oauth_client_id(&self) -> Option<String> {
        std::env::var("OAUTH_CLIENT_ID").ok().filter(|s| !s.is_empty())
    }

    pub fn oauth_client_secret(&self) -> Option<String> {
        std::env::var("OAUTH_CLIENT_SECRET").ok().filter(|s| !s.is_empty())
    }

    pub fn oauth_cookie_secret(&self) -> Option<String> {
        std::env::var("OAUTH_COOKIE_SECRET").ok().filter(|s| !s.is_empty())
    }

    pub fn is_admin(&self, username: &str) -> bool {
        self.oauth2_proxy.allowed_admins.iter().any(|u| u == username)
    }

    pub fn is_user(&self, username: &str) -> bool {
        self.oauth2_proxy.allowed_users.iter().any(|u| u == username)
    }

    pub fn is_allowed(&self, username: &str) -> bool {
        self.is_admin(username) || self.is_user(username)
    }
}

// ---------------------------------------------------------------------------
// Updates configuration
// ---------------------------------------------------------------------------

/// `[updates]` section — automatic config updates applied at startup
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatesConfig {
    /// Backfill newly introduced defaults (e.g. default custom commands) on
    /// restart. Default: true. Turning it off keeps the stored config as-is.
    #[serde(default = "default_true")]
    pub auto_config_update: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            auto_config_update: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Git configuration
// ---------------------------------------------------------------------------

/// `[git.issue]` section — Gitea credentials consumed by the /git issue skill.
/// Injected as env vars when the console spawns an agent, so the skill reads
/// console config as its first-priority credential source.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIssueConfig {
    /// Gitea base URL, with or without /api/v1
    #[serde(default)]
    pub url: String,
    /// Access token with repo scope
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitConfig {
    #[serde(default)]
    pub issue: GitIssueConfig,
}

/// `[skills]` section — third-party install record (skills.redb) settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// 留痕统计方式："total" 只显示安装总数量；"detail" 额外显示节点数量
    #[serde(default)]
    pub stats_mode: String,
    /// skills.redb 保留的留痕条数。0 = 永久保留。
    #[serde(default = "default_skills_history_count")]
    pub history_count: u32,
    /// 官方技能包下载基础地址（包地址 = base/{name}.7z）。
    /// 无默认值：未配置时官方技能下载前直接报错。
    #[serde(default)]
    pub official_base_url: Option<String>,
    /// 官方技能包（.7z）解压密码。无默认值：未配置时官方技能下载前直接报错。
    #[serde(default)]
    pub official_password: Option<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            stats_mode: "total".to_string(),
            history_count: default_skills_history_count(),
            official_base_url: None,
            official_password: None,
        }
    }
}

/// `[debug]` section — debug 面板（仅 debug 环境）外观自定义。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugConfig {
    /// Debug 悬浮球自定义图片的本地文件路径（png/jpg/webp/svg，支持 ~）。
    /// 未配置时前端使用内置开源图标。
    #[serde(default)]
    pub icon_path: Option<String>,
}

pub(crate) fn default_skills_history_count() -> u32 {
    100
}

impl SkillsConfig {
    pub fn is_detail_mode(&self) -> bool {
        self.stats_mode.eq_ignore_ascii_case("detail")
    }
}

/// `[changelog]` section — 对外更新日志（CHANGELOG.md）的查看密码门。
/// 只挡「看」，不参与任何登录/认证体系；未配置时使用默认密码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangelogConfig {
    #[serde(default = "default_changelog_password")]
    pub password: String,
}

impl Default for ChangelogConfig {
    fn default() -> Self {
        Self {
            password: default_changelog_password(),
        }
    }
}

pub(crate) fn default_changelog_password() -> String {
    "MilletcodeIsAwesome!".to_string()
}

/// Token shown to the frontend: first 4 + last 4 chars when long enough,
/// otherwise fully masked. Never expose the raw token over the API.
pub fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}*****{}", head, tail)
    } else if chars.is_empty() {
        String::new()
    } else {
        "****".to_string()
    }
}

/// Env vars injected at spawn time so /git issue reads console config first.
/// Project-level values win per-field over the system-level `[git.issue]`;
/// empty fields inject nothing (a pre-existing env var from the parent shell
/// stays untouched).
pub fn resolve_issue_env(
    project: Option<&GitIssueConfig>,
    system: &GitIssueConfig,
) -> Vec<(String, String)> {
    let (url, token) = match project {
        Some(p) => (
            if p.url.is_empty() { &system.url } else { &p.url },
            if p.token.is_empty() { &system.token } else { &p.token },
        ),
        None => (&system.url, &system.token),
    };
    let mut env = Vec::new();
    if !url.is_empty() {
        env.push(("MILLET_AGENT_GITEA_HOST".to_string(), url.clone()));
    }
    if !token.is_empty() {
        env.push(("MILLET_AGENT_GITEA_TOKEN".to_string(), token.clone()));
    }
    env
}

// ---------------------------------------------------------------------------
// Top-level Config
// ---------------------------------------------------------------------------

/// Top-level configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ssh: SshGlobalConfig,
    #[serde(default)]
    pub agent: AgentSettings,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub logs: LogConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub project: ProjectConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub updates: UpdatesConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub debug: DebugConfig,
    #[serde(default)]
    pub changelog: ChangelogConfig,
    /// 保护模式（默认开）：开启时系统配置接口不输出 ssh / auth / discovery_paths
    /// 等敏感段，前端也隐藏对应板块。
    #[serde(default = "default_true")]
    pub protect_mode: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ssh: SshGlobalConfig::default(),
            agent: AgentSettings::default(),
            web: WebConfig::default(),
            logs: LogConfig::default(),
            auth: AuthConfig::default(),
            project: ProjectConfig::default(),
            permissions: PermissionsConfig::default(),
            updates: UpdatesConfig::default(),
            git: GitConfig::default(),
            skills: SkillsConfig::default(),
            debug: DebugConfig::default(),
            changelog: ChangelogConfig::default(),
            protect_mode: true,
        }
    }
}

/// `[permissions]` section — the 保护指令 (protected commands) list. Everything
/// runs without approval except Bash commands hitting an entry here: plain
/// entries match the program basename exactly, entries containing `*` or `?`
/// use shell wildcard syntax against the whole command (so `kubectl delete*`
/// is a prefix rule), and `*` alone routes every command to approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default = "default_protected_commands")]
    pub protected_commands: Vec<String>,
    /// How many decision records to keep in approvals.redb. 0 = forever.
    #[serde(default = "default_approval_history_count")]
    pub approval_history_count: u32,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            protected_commands: default_protected_commands(),
            approval_history_count: default_approval_history_count(),
        }
    }
}

pub(crate) fn default_protected_commands() -> Vec<String> {
    vec![
        "rm".to_string(),
        // 全拦（含读操作）：kube 前缀同时覆盖 kubectl 与自定义 kube 包装指令
        "kubectl*".to_string(),
        "kube*".to_string(),
    ]
}

pub(crate) fn default_approval_history_count() -> u32 {
    1000
}

// ---------------------------------------------------------------------------
// Default value functions
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_enabled() -> bool {
    true
}

fn default_release_channel() -> String {
    "stable".to_string()
}

fn default_ssh_config_path() -> String {
    String::new()
}

/// Resolve SSH config path: if empty, auto-detect from
/// ~/.milletcode/ssh-config → ~/.ssh/config (first existing file wins).
/// Returns the expanded absolute path.
pub fn resolve_ssh_config_path(config_path: &str) -> String {
    let home = dirs::home_dir();

    // If explicitly set, expand ~ and return
    if !config_path.is_empty() {
        return shellexpand::tilde(config_path).to_string();
    }

    // Auto-detect: try ~/.milletcode/ssh-config first, then ~/.ssh/config
    let candidates = [
        home.as_ref().map(|h| h.join(".milletcode").join("ssh-config")),
        home.as_ref().map(|h| h.join(".ssh").join("config")),
    ];

    for candidate in candidates.iter().flatten() {
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }

    // Final fallback: ~/.ssh/config (even if it doesn't exist yet)
    home.map(|h| h.join(".ssh").join("config").to_string_lossy().to_string())
        .unwrap_or_else(|| "~/.ssh/config".to_string())
}

fn default_approval_timeout() -> u64 {
    300
}

fn default_log_max_files() -> usize {
    30
}

fn default_session_log_enabled() -> bool {
    true
}

fn default_max_subscriptions() -> String {
    "auto".to_string()
}

fn default_max_total_sessions() -> String {
    "auto".to_string()
}

fn default_project_scan_interval() -> u64 {
    300
}

fn default_storage_type() -> String {
    "local".to_string()
}

fn default_upload_dir() -> String {
    "/tmp/milletcode-uploads".to_string()
}

fn default_max_size_mb() -> usize {
    10
}

fn default_project_discovery_paths() -> HashMap<String, DiscoveryPath> {
    let mut paths = HashMap::new();
    paths.insert("default".to_string(), DiscoveryPath {
        path: "~/Codes".to_string(),
        hostname: String::new(),
        remote_mode: String::new(),
        is_mobile_home: false,
        sync_settings: true,
        sync_models: true,
        ssh: None,
        paths: HashMap::new(),
    });
    paths
}

fn default_agent_instances() -> HashMap<String, AgentConfig> {
    let mut instances = HashMap::new();
    instances.insert("claude".to_string(), AgentConfig {
        name: "Claude Code".to_string(),
        extra_args: vec![],
        api_key: None,
        auto_approve: false,
        enabled: true,
        allow_config_overwrite: false,
        path: None,
        remote_path: None,
        release_channel: "stable".to_string(),
    });
    instances.insert("codewhale".to_string(), AgentConfig {
        name: "CodeWhale".to_string(),
        extra_args: vec![],
        api_key: None,
        auto_approve: false,
        enabled: false,
        allow_config_overwrite: true,
        path: None,
        remote_path: None,
        release_channel: "stable".to_string(),
    });
    instances.insert("opencode".to_string(), AgentConfig {
        name: "OpenCode".to_string(),
        extra_args: vec![],
        api_key: None,
        auto_approve: false,
        enabled: false,
        allow_config_overwrite: true,
        path: None,
        remote_path: None,
        release_channel: "stable".to_string(),
    });
    instances.insert("codex".to_string(), AgentConfig {
        name: "Codex".to_string(),
        extra_args: vec![],
        api_key: None,
        auto_approve: false,
        enabled: false,
        allow_config_overwrite: true,
        path: None,
        remote_path: None,
        release_channel: "beta".to_string(),
    });
    instances.insert("smartcode".to_string(), AgentConfig {
        name: "SmartCode".to_string(),
        extra_args: vec![],
        api_key: None,
        auto_approve: false,
        enabled: false,
        allow_config_overwrite: true,
        path: None,
        remote_path: None,
        release_channel: "alpha".to_string(),
    });
    instances
}

// ---------------------------------------------------------------------------
// ConfigManager
// ---------------------------------------------------------------------------

/// Config manager handles loading/saving configuration.
pub struct ConfigManager {
    config_path: PathBuf,
    config: parking_lot::Mutex<Config>,
}

impl ConfigManager {
    pub fn new() -> anyhow::Result<Self> {
        let config_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".milletcode");
        std::fs::create_dir_all(&config_dir)?;

        let config_path = config_dir.join("config.toml");

        let config = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path)?;
            toml::from_str(&content).map_err(|e| anyhow::anyhow!("Failed to parse config: {}", e))?
        } else {
            let default = Config::default();
            let toml_str = toml::to_string_pretty(&default)?;
            std::fs::write(&config_path, toml_str)?;
            default
        };

        Ok(Self {
            config_path,
            config: parking_lot::Mutex::new(config),
        })
    }

    /// Returns a clone of the current config
    pub fn config(&self) -> Config {
        self.config.lock().clone()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let toml_str = toml::to_string_pretty(&*self.config.lock())?;
        std::fs::write(&self.config_path, toml_str)?;
        Ok(())
    }

    pub fn add_agent(&self, id: &str, agent: AgentConfig) -> anyhow::Result<()> {
        self.config.lock().agent.instances.insert(id.to_string(), agent);
        self.save()
    }

    pub fn update_agent(&self, id: &str, agent: AgentConfig) -> anyhow::Result<()> {
        let mut config = self.config.lock();
        if config.agent.instances.contains_key(id) {
            config.agent.instances.insert(id.to_string(), agent);
            drop(config);
            return self.save();
        }
        Err(anyhow::anyhow!("Agent not found: {}", id))
    }

    pub fn remove_agent(&self, id: &str) -> anyhow::Result<()> {
        let mut config = self.config.lock();
        if config.agent.instances.remove(id).is_none() {
            return Err(anyhow::anyhow!("Agent not found: {}", id));
        }
        drop(config);
        self.save()
    }

    pub fn get_agent(&self, id: &str) -> Option<AgentConfig> {
        self.config.lock().agent.instances.get(id).cloned()
    }

    pub fn get_enabled_agents(&self) -> Vec<(String, AgentConfig)> {
        self.config
            .lock()
            .agent
            .instances
            .iter()
            .filter(|(_, a)| a.enabled)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Expand ~ in paths to home directory
    /// Returns HashMap<keyword, (path, hostname, remote_mode, ssh_config_path, is_mobile_home)>
    /// Note: For remote paths (remote_mode == "ssh"), ~ is NOT expanded locally
    /// to allow the remote host to resolve it correctly.
    pub fn resolve_project_discovery_paths(&self) -> HashMap<String, (PathBuf, String, String, String, bool)> {
        let config = self.config.lock();
        let global_ssh_config = resolve_ssh_config_path(&config.ssh.config_path);

        let home = match dirs::home_dir() {
            Some(h) => h,
            None => {
                tracing::warn!("Cannot determine home directory, using paths as-is");
                return config
                    .project
                    .discovery_paths
                    .iter()
                    .map(|(k, v)| {
                        let ssh_config = v.ssh.as_ref()
                            .map(|s| resolve_ssh_config_path(&s.config_path))
                            .unwrap_or_else(|| global_ssh_config.clone());
                        (k.clone(), (PathBuf::from(&v.path), v.hostname.clone(), v.remote_mode.clone(), ssh_config, v.is_mobile_home))
                    })
                    .collect();
            }
        };

        config
            .project
            .discovery_paths
            .iter()
            .map(|(keyword, dp)| {
                let path = dp.path.trim();
                let is_remote = dp.remote_mode == "ssh";

                // For remote paths, don't expand ~ locally - let the remote host resolve it
                let expanded_path = if is_remote {
                    PathBuf::from(path)
                } else {
                    // Local path: expand ~ to local home directory
                    if path == "~" {
                        home.clone()
                    } else if let Some(rest) = path.strip_prefix("~/") {
                        home.join(rest)
                    } else {
                        PathBuf::from(path)
                    }
                };

                let ssh_config = dp.ssh.as_ref()
                    .map(|s| resolve_ssh_config_path(&s.config_path))
                    .unwrap_or_else(|| global_ssh_config.clone());
                (keyword.clone(), (expanded_path, dp.hostname.clone(), dp.remote_mode.clone(), ssh_config, dp.is_mobile_home))
            })
            .collect()
    }

    /// Set auto_approve for all agents and persist to disk.
    pub fn set_auto_approve_for_all(&self, value: bool) -> anyhow::Result<()> {
        let mut config = self.config.lock();
        for (_, agent) in &mut config.agent.instances {
            agent.auto_approve = value;
        }
        drop(config);
        self.save()
    }

    /// Replace the 保护指令 list and persist to disk.
    pub fn set_protected_commands(&self, commands: Vec<String>) -> anyhow::Result<()> {
        self.config.lock().permissions.protected_commands = commands;
        self.save()
    }

    /// Set the decision-history retention (number of records, 0 = forever) and persist.
    pub fn set_approval_history_count(&self, count: u32) -> anyhow::Result<()> {
        self.config.lock().permissions.approval_history_count = count;
        self.save()
    }

    /// Set the skill install-record retention (skills.redb, 0 = forever) and persist.
    pub fn set_skills_history_count(&self, count: u32) -> anyhow::Result<()> {
        self.config.lock().skills.history_count = count;
        self.save()
    }

    /// Set whether startup auto-config updates (defaults backfill) run, and persist.
    pub fn set_auto_config_update(&self, value: bool) -> anyhow::Result<()> {
        self.config.lock().updates.auto_config_update = value;
        self.save()
    }

    /// Set the idle-reminder global switch and threshold (seconds), and persist.
    pub fn set_idle_reminder(&self, enabled: bool, idle_seconds: u64) -> anyhow::Result<()> {
        {
            let mut cfg = self.config.lock();
            cfg.project.sessions.idle_reminder_enabled = enabled;
            cfg.project.sessions.idle_seconds = idle_seconds;
        }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_trusted_proxies_defaults_and_validation() {
        // 未配置 = 空列表（运行时回退默认网段集合）
        let a: AuthConfig = toml::from_str("").unwrap();
        assert!(a.trusted_proxies.is_empty());

        // 合法 CIDR / 裸 IP 通过，非法条目启动即拒
        let ok: AuthConfig = toml::from_str(
            "mode = 'plain'\ntrusted_proxies = ['10.0.0.0/8', '127.0.0.1']\n[plain]\nuser = 'u'\npassword = 'p'\n\n",
        )
        .unwrap();
        assert!(ok.validate().is_ok());
        let bad: AuthConfig = toml::from_str(
            "mode = 'plain'\ntrusted_proxies = ['10.0.0.0/99']\n[plain]\nuser = 'u'\npassword = 'p'\n\n",
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn session_settings_idle_defaults_backfill() {
        // Existing config.toml files have neither key — serde defaults apply.
        let s: SessionSettings = toml::from_str("").unwrap();
        assert!(s.idle_reminder_enabled, "idle reminder must default to ON");
        assert_eq!(s.idle_seconds, 120, "default idle threshold is 120 seconds");

        // Explicit values round-trip.
        let s2: SessionSettings =
            toml::from_str("idle_reminder_enabled = false\nidle_seconds = 300").unwrap();
        assert!(!s2.idle_reminder_enabled);
        assert_eq!(s2.idle_seconds, 300);
    }

    #[test]
    fn session_limits_auto_and_explicit() {
        // 原集成测试（tests/ws_integration_test.rs，引用了不存在的 lib target，
        // 从未编译通过）迁移而来：覆盖并发上限的两种取值形态。
        let auto: SessionSettings = toml::from_str("max_total_sessions = 'auto'").unwrap();
        let n = auto.max_total_sessions();
        assert!(n > 0 && n <= 64, "auto 按核数推导，上限 64，实际 {n}");

        let explicit: SessionSettings = toml::from_str("max_total_sessions = '32'").unwrap();
        assert_eq!(explicit.max_total_sessions(), 32);

        let subs_auto: SessionSettings =
            toml::from_str("max_subscriptions_per_project = 'auto'").unwrap();
        assert_eq!(subs_auto.max_subscriptions_per_project(), None, "auto 即不限");

        let subs_n: SessionSettings =
            toml::from_str("max_subscriptions_per_project = '10'").unwrap();
        assert_eq!(subs_n.max_subscriptions_per_project(), Some(10));
    }

    #[test]
    fn listen_uri_wins_for_bind_address() {
        // listen_uri is the sole authority for the bind address.
        let w: WebConfig = toml::from_str("listen_uri = '127.0.0.1:9999'").unwrap();
        assert!(w.listen_uri_configured());
        assert_eq!(w.listen_addr("0.0.0.0"), "127.0.0.1:9999");

        // Bare port binds on 0.0.0.0.
        let w2: WebConfig = toml::from_str("listen_uri = '8080'").unwrap();
        assert_eq!(w2.listen_addr("127.0.0.1"), "0.0.0.0:8080");
    }

    #[test]
    fn listen_addr_falls_back_to_legacy_host_and_default_port() {
        // No listen_uri: --host + default port 3030.
        let w = WebConfig::default();
        assert!(!w.listen_uri_configured());
        assert_eq!(w.listen_addr("127.0.0.1"), "127.0.0.1:3030");

        // Blank listen_uri counts as unset.
        let w2: WebConfig = toml::from_str("listen_uri = '  '").unwrap();
        assert!(!w2.listen_uri_configured());
        assert_eq!(w2.listen_addr("0.0.0.0"), "0.0.0.0:3030");

        // Stale base_url keys from older configs are simply ignored.
        let w3: WebConfig =
            toml::from_str("base_url = 'http://192.168.1.5:8080'").unwrap();
        assert_eq!(w3.listen_addr("127.0.0.1"), "127.0.0.1:3030");
    }

    #[test]
    fn advertised_port_parses_listen_uri_variants() {
        // Bare port form.
        let w: WebConfig = toml::from_str("listen_uri = '8080'").unwrap();
        assert_eq!(w.advertised_port(), 8080);

        // host:port form.
        let w2: WebConfig = toml::from_str("listen_uri = '0.0.0.0:9090'").unwrap();
        assert_eq!(w2.advertised_port(), 9090);

        // Unset / malformed falls back to the default 3030.
        assert_eq!(WebConfig::default().advertised_port(), 3030);
        let w3: WebConfig = toml::from_str("listen_uri = 'no-port-here'").unwrap();
        assert_eq!(w3.advertised_port(), 3030);
    }

    #[test]
    fn git_issue_parses_from_toml() {
        let cfg: Config = toml::from_str(
            r#"
[git.issue]
url = "https://git.example.com/api/v1"
token = "0123456789abcdef"
"#,
        )
        .unwrap();
        assert_eq!(cfg.git.issue.url, "https://git.example.com/api/v1");
        assert_eq!(cfg.git.issue.token, "0123456789abcdef");
    }

    #[test]
    fn agent_release_channel_defaults_to_stable_and_round_trips() {
        // 缺字段（旧 config.toml / 旧 redb 回放）→ stable
        let a: AgentConfig = toml::from_str(r#"name = "X""#).unwrap();
        assert_eq!(a.release_channel, "stable");

        // 显式值保留，且能序列化回 toml/JSON 不丢
        let b: AgentConfig =
            toml::from_str(r#"name = "X"
release_channel = "alpha""#)
                .unwrap();
        assert_eq!(b.release_channel, "alpha");
        let json = serde_json::to_string(&b).unwrap();
        let c: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c.release_channel, "alpha");

        // 产品默认实例：claude=stable、codex=beta、smartcode=alpha
        let instances = default_agent_instances();
        assert_eq!(instances["claude"].release_channel, "stable");
        assert_eq!(instances["codex"].release_channel, "beta");
        assert_eq!(instances["smartcode"].release_channel, "alpha");
    }

    #[test]
    fn git_issue_defaults_empty() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.git.issue.url.is_empty());
        assert!(cfg.git.issue.token.is_empty());
    }

    #[test]
    fn mask_token_keeps_head_and_tail() {
        assert_eq!(mask_token("0123456789abcdef"), "0123*****cdef");
    }

    #[test]
    fn mask_token_short_tokens_fully_masked() {
        // <= 8 chars must not reveal anything (first4+last4 would leak the whole token)
        assert_eq!(mask_token("12345678"), "****");
        assert_eq!(mask_token("short"), "****");
        assert_eq!(mask_token("1"), "****");
        assert_eq!(mask_token(""), "");
    }

    #[test]
    fn resolve_issue_env_project_overrides_system_per_field() {
        let system = GitIssueConfig {
            url: "https://sys.example.com".into(),
            token: "sys-token-long-enough".into(),
        };
        let project = GitIssueConfig {
            url: "https://proj.example.com".into(),
            token: String::new(),
        };
        let env = resolve_issue_env(Some(&project), &system);
        assert_eq!(
            env,
            vec![
                ("MILLET_AGENT_GITEA_HOST".to_string(), "https://proj.example.com".to_string()),
                ("MILLET_AGENT_GITEA_TOKEN".to_string(), "sys-token-long-enough".to_string()),
            ]
        );
    }

    #[test]
    fn resolve_issue_env_empty_system_injects_nothing() {
        let env = resolve_issue_env(None, &GitIssueConfig::default());
        assert!(env.is_empty());
    }

    #[test]
    fn protect_mode_defaults_true_and_parses_false() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.protect_mode);
        let cfg: Config = toml::from_str("protect_mode = false").unwrap();
        assert!(!cfg.protect_mode);
    }

    fn auth(mode: &str) -> AuthConfig {
        let mut a = AuthConfig::default();
        a.mode = mode.to_string();
        a
    }

    #[test]
    fn plain_mode_requires_user_and_password() {
        let mut a = auth("plain");
        assert!(a.validate().is_err(), "empty [auth.plain] must fail");
        a.plain.user = "alice".into();
        assert!(a.validate().is_err(), "missing password must fail");
        a.plain.password = "secret".into();
        assert!(a.validate().is_ok());
        assert_eq!(a.effective_mode(), "plain");
        // token 可选
        a.plain.token = "tok".into();
        assert!(a.validate().is_ok());
    }

    #[test]
    fn reserved_modes_are_rejected() {
        assert!(auth("local").validate().is_err());
        assert!(auth("oauth2").validate().is_err());
    }

    #[test]
    fn unknown_and_empty_modes_are_rejected() {
        assert!(auth("").validate().is_err());
        assert!(auth("basic").validate().is_err());
    }

    #[test]
    fn nested_config_with_mode_loads() {
        let toml = r#"
mode = "oauth2_proxy"
[oauth2_proxy]
proxy_domain = "git.example.com"
basic_auth_user = "gateway"
basic_auth_password = "pw"
allowed_admins = ["admin"]
"#;
        let a: AuthConfig = toml::from_str(toml).unwrap();
        assert_eq!(a.effective_mode(), "oauth2_proxy");
        assert!(a.validate().is_ok());
    }

    #[test]
    fn oauth2_proxy_mode_requires_full_oauth_settings() {
        let mut a = auth("oauth2_proxy");
        a.oauth2_proxy.proxy_domain = Some("git.example.com".into());
        a.oauth2_proxy.basic_auth_user = Some("gateway".into());
        a.oauth2_proxy.basic_auth_password = Some("pw".into());
        assert!(a.validate().is_err(), "empty allowlist must fail");
        a.oauth2_proxy.allowed_admins = vec!["admin".into()];
        assert!(a.validate().is_ok());
    }

    #[test]
    fn serialize_writes_nested_shape_only() {
        let mut a = auth("oauth2_proxy");
        a.oauth2_proxy.proxy_domain = Some("git.example.com".into());
        a.oauth2_proxy.basic_auth_user = Some("gateway".into());
        a.oauth2_proxy.basic_auth_password = Some("pw".into());
        a.oauth2_proxy.allowed_admins = vec!["admin".into()];
        let out = toml::to_string(&a).unwrap();
        assert!(out.contains("[oauth2_proxy]"), "nested table must be written: {out}");
        assert!(out.contains("basic_auth_user = \"gateway\""), "nested values must be written: {out}");
        // roundtrip：写出的配置再读回来，值不丢
        let rt: AuthConfig = toml::from_str(&out).unwrap();
        assert_eq!(rt.oauth2_proxy.basic_auth_user.as_deref(), Some("gateway"));
        assert!(rt.validate().is_ok());
    }
}

#[cfg(test)]
mod changelog_config_tests {
    use super::*;

    #[test]
    fn empty_toml_defaults_to_builtin_password() {
        let c: ChangelogConfig = toml::from_str("").unwrap();
        assert_eq!(c.password, "MilletcodeIsAwesome!");
    }

    #[test]
    fn configured_password_round_trips() {
        let toml = r#"
[changelog]
password = "s3cret!"
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.changelog.password, "s3cret!");
        let out = toml::to_string(&c.changelog).unwrap();
        assert!(out.contains("password = \"s3cret!\""));
        let rt: ChangelogConfig = toml::from_str(&out).unwrap();
        assert_eq!(rt.password, "s3cret!");
    }

    #[test]
    fn full_config_without_changelog_section_gets_default() {
        let c = Config::default();
        assert_eq!(c.changelog.password, "MilletcodeIsAwesome!");
    }
}

/// `make init-config` 写出的模板（scripts/init-config-template.toml）必须能被
/// Config 完整解析——字段名在这里锁住，模板改动若破坏解析会直接挂测试。
#[cfg(test)]
mod init_config_template_tests {
    use super::*;

    const TEMPLATE: &str = include_str!("../../scripts/init-config-template.toml");

    #[test]
    fn template_parses_as_full_config() {
        assert!(TEMPLATE.contains("__USER__"), "模板必须保留 __USER__ 占位符");
        assert!(TEMPLATE.contains("__PASSWORD__"), "模板必须保留 __PASSWORD__ 占位符");
        assert!(
            !TEMPLATE.contains(dirs::home_dir().unwrap().to_str().unwrap()),
            "模板不得包含绝对 home 路径（ssh 等路径用 ~，运行时展开）"
        );
        let toml = TEMPLATE
            .replace("__USER__", "user-abcd1234")
            .replace("__PASSWORD__", "test-password");
        let c: Config = toml::from_str(&toml).unwrap();
        assert!(matches!(c.auth.effective_mode(), "plain"));
        assert_eq!(c.auth.plain.user, "user-abcd1234");
        assert_eq!(c.auth.plain.password, "test-password");
        assert!(c.auth.validate().is_ok());
        assert_eq!(c.ssh.config_path, "~/.ssh/config");
        assert!(c.project.allow_create && c.project.allow_archive && c.project.allow_delete);
        let paths = &c.project.discovery_paths;
        assert_eq!(paths.len(), 2);
        assert_eq!(paths["milletcode"].path, "~/Codes/milletcode");
        assert_eq!(paths["smartcode"].path, "~/Codes/smartcode");
        for p in paths.values() {
            assert!(p.hostname.is_empty() && p.remote_mode.is_empty());
        }
    }
}
