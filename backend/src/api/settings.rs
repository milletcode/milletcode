use axum::{Json, extract::State};
use serde::Serialize;
use std::sync::Arc;

use crate::AppState;
use crate::config::{resolve_ssh_config_path, mask_token};
use crate::runtime::parse_ssh_config;

pub async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // Return config as read-only JSON
    let config = state.config_manager.config();
    let protect_mode = config.protect_mode;
    let mut json = serde_json::to_value(&config).unwrap_or_default();
    if let Some(obj) = json.as_object_mut() {
        obj.insert("env".to_string(), serde_json::Value::String(state.env.clone()));
        obj.insert("protect_mode".to_string(), serde_json::Value::Bool(protect_mode));
    }
    // git.issue.token must never reach the frontend in the clear
    let raw_token = json
        .pointer("/git/issue/token")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());
    if let Some(raw) = raw_token {
        if let Some(target) = json.pointer_mut("/git/issue/token") {
            *target = serde_json::Value::String(mask_token(&raw));
        }
    }
    // 更新日志密码不回显：settings 页若原样展示再整体回存，会把密文当新密码
    if let Some(target) = json.pointer_mut("/changelog/password") {
        *target = serde_json::Value::String("********".to_string());
    }
    // 官方技能包解压密码同理不回显（official_base_url 属公开信息，保持可见）
    if let Some(target) = json.pointer_mut("/skills/official_password") {
        *target = serde_json::Value::String("********".to_string());
    }
    // 保护模式：ssh（含 passphrase）、auth（含 basic_auth_password 明文）、
    // discovery_paths（内网主机/路径）不输出，防止登录用户拉全量配置破解。
    // discovery_paths 保留 sync_models 布尔（会话页靠它决定是否显示模型选择菜单），
    // 其余暴露内网信息的字段全部剥掉。
    if protect_mode {
        if let Some(obj) = json.as_object_mut() {
            obj.remove("ssh");
            obj.remove("auth");
        }
        if let Some(project) = json.pointer_mut("/project").and_then(|p| p.as_object_mut()) {
            let flags: serde_json::Map<String, serde_json::Value> = config
                .project
                .discovery_paths
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::json!({ "sync_models": v.sync_models })))
                .collect();
            project.insert("discovery_paths".to_string(), serde_json::Value::Object(flags));
        }
    }
    Json(json)
}

#[derive(Serialize)]
pub struct AuthInfo {
    mode: String,
    /// oauth2_proxy 模式的代理域名（前端拼 /oauth2 跳转用）
    proxy_domain: Option<String>,
    basic_auth_user: Option<String>,
    password_configured: bool,
}

pub async fn get_auth_settings(
    State(state): State<Arc<AppState>>,
) -> Json<AuthInfo> {
    let config = state.config_manager.config();
    let auth = &config.auth;
    // 保护模式：此端点是公开的（登录页要用），只保留 mode（前端据此决定
    // 跳 /login 还是走 oauth2 代理），用户名、域名、白名单等一律不输出。
    if config.protect_mode {
        return Json(AuthInfo {
            mode: auth.effective_mode().to_string(),
            proxy_domain: None,
            basic_auth_user: None,
            password_configured: false,
        });
    }
    let password_configured = auth
        .basic_auth_password_resolved()
        .map(|p| !p.is_empty())
        .unwrap_or(false);
    Json(AuthInfo {
        mode: auth.effective_mode().to_string(),
        proxy_domain: auth.oauth2_proxy.proxy_domain.clone(),
        basic_auth_user: auth.oauth2_proxy.basic_auth_user.clone(),
        password_configured,
    })
}

#[derive(Serialize)]
pub struct CapacityInfo {
    pub max_total_sessions: usize,
}

pub async fn get_capacity(
    State(state): State<Arc<AppState>>,
) -> Json<CapacityInfo> {
    let max_sessions = state.config_manager.config().project.sessions.max_total_sessions();
    Json(CapacityInfo {
        max_total_sessions: max_sessions,
    })
}

#[derive(Serialize)]
pub struct OnlineStats {
    pub total_connections: usize,
    pub total_subscriptions: usize,
    pub process_count: usize,
    pub by_project: std::collections::HashMap<String, usize>,
}

pub async fn get_online_stats(
    State(state): State<Arc<AppState>>,
) -> Json<OnlineStats> {
    let registry = state.ws_registry.lock();
    let by_project: std::collections::HashMap<String, usize> = registry
        .iter()
        .map(|(k, v)| (k.0.clone(), *v))
        .collect();
    let total: usize = by_project.values().sum();
    let total_subscriptions = state.ws_subscription_count.load(std::sync::atomic::Ordering::Relaxed);

    // Count claude processes managed by milletcode
    let process_count = count_claude_processes();

    Json(OnlineStats {
        total_connections: total,
        total_subscriptions,
        process_count,
        by_project,
    })
}

fn count_claude_processes() -> usize {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg("ps auwx | grep -v grep | grep -v '\\.claude' | grep -v node_modules | grep -v 'target/debug/agent-console' | grep claude || true")
        .output();

    match output {
        Ok(out) => {
            let raw = String::from_utf8_lossy(&out.stdout);
            raw.lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !trimmed.is_empty() && trimmed.contains("stream-json")
                })
                .count()
        }
        Err(_) => 0,
    }
}

#[derive(Serialize)]
pub struct SshHostInfo {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
}

/// GET /api/settings/ssh-hosts — return parsed SSH config entries
pub async fn get_ssh_hosts(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<SshHostInfo>> {
    // 保护模式下不输出 SSH 配置内容
    if state.config_manager.config().protect_mode {
        return Json(Vec::new());
    }
    let ssh_config_path = resolve_ssh_config_path(&state.config_manager.config().ssh.config_path);
    let hosts = match parse_ssh_config(&ssh_config_path) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("Failed to parse SSH config: {}", e);
            return Json(Vec::new());
        }
    };

    let mut result: Vec<SshHostInfo> = hosts
        .into_iter()
        .map(|(alias, cfg)| SshHostInfo {
            alias,
            hostname: cfg.hostname,
            user: cfg.user,
            port: cfg.port,
            identity_files: cfg.identity_files,
        })
        .collect();

    // Sort by alias for consistent display
    result.sort_by(|a, b| a.alias.cmp(&b.alias));
    Json(result)
}
