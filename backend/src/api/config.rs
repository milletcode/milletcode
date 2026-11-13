use axum::{Json, Router, extract::Path, extract::State, response::IntoResponse, routing::{get, post, delete}, http::{StatusCode, header}};
use std::path::PathBuf;
use std::sync::Arc;

use crate::AppState;

/// Hard-coded template for ~/.claude/settings.json
/// Update this when the template structure changes.
const CLAUDE_SETTINGS_TEMPLATE: &str = r#"{
  "enabledPlugins": {
    "clangd-lsp@claude-plugins-official": true,
    "rust-analyzer-lsp@claude-plugins-official": true
  },
  "env": {
    "ANTHROPIC_AUTH_TOKEN": "",
    "ANTHROPIC_BASE_URL": "",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "",
    "ANTHROPIC_DEFAULT_SONNET_MODEL": "",
    "ANTHROPIC_MODEL": "",
    "ANTHROPIC_REASONING_MODEL": ""
  },
  "hasCompletedOnboarding": true,
  "model": "",
  "permissions": {
    "ask": []
  },
  "skipDangerousModePermissionPrompt": true,
  "theme": "dark"
}"#;

/// Convert the 保护指令 list into Claude Code `permissions.ask` rules so the
/// CLI itself routes these commands through --permission-prompt-tool (i.e. our
/// control protocol). The CLI's built-in safety analyzer is unreliable across
/// --resume processes — ask rules are the deterministic forcing function.
/// "rm" → Bash(rm:*), "kubectl delete*" → Bash(kubectl delete:*), "*" → Bash.
fn protected_ask_rules(protected: &[String]) -> Vec<String> {
    let mut rules: Vec<String> = protected
        .iter()
        .filter_map(|entry| {
            let trimmed = entry.trim();
            if trimmed == "*" {
                return Some("Bash".to_string());
            }
            let prefix = trimmed
                .split_once(['*', '?'])
                .map(|(p, _)| p)
                .unwrap_or(trimmed)
                .trim_end();
            (!prefix.is_empty()).then(|| format!("Bash({}:*)", prefix))
        })
        .collect();
    rules.sort();
    rules.dedup();
    rules
}

/// Stamp the current ask rules into a settings value, merging into an
/// existing permissions block so sibling rules (allow/deny) survive.
fn apply_protected_ask_rules(settings: &mut serde_json::Value, protected: &[String]) {
    let Some(obj) = settings.as_object_mut() else { return };
    let block = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if let Some(perms) = block.as_object_mut() {
        perms.insert(
            "ask".to_string(),
            serde_json::json!(protected_ask_rules(protected)),
        );
    }
}

/// Remove the legacy k8s-guard PreToolUse hook earlier releases stamped into
/// settings.json. Identified by the "Kubernetes 写操作" marker in the command;
/// hooks the user configured themselves are untouched. Command routing now
/// lives entirely in permissions.ask (保护指令面板 → --permission-prompt-tool).
fn prune_legacy_k8s_hook(settings: &mut serde_json::Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return false;
    };
    let mut changed = false;
    if let Some(pre) = hooks.get_mut("PreToolUse").and_then(|v| v.as_array_mut()) {
        for group in pre.iter_mut() {
            if let Some(arr) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                let before = arr.len();
                arr.retain(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map_or(true, |s| !s.contains("Kubernetes 写操作"))
                });
                changed |= arr.len() != before;
            }
        }
        pre.retain(|g| {
            g.get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|a| !a.is_empty())
        });
    }
    if hooks
        .get("PreToolUse")
        .and_then(|v| v.as_array())
        .is_some_and(|a| a.is_empty())
    {
        hooks.remove("PreToolUse");
        changed = true;
    }
    if hooks.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
        changed = true;
    }
    changed
}

/// Patch only the permissions.ask block of an existing settings.json, leaving
/// everything else (model, env, user tweaks) untouched, and prune the legacy
/// k8s-guard hook. Returns false when the
/// file is missing or unreadable, so the caller can fall back to a full write.
fn patch_ask_rules_in_place(path: &std::path::Path, protected: &[String]) -> std::io::Result<bool> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let mut settings: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(false), // unparsable → let the full write replace it
    };
    apply_protected_ask_rules(&mut settings, protected);
    prune_legacy_k8s_hook(&mut settings);
    std::fs::write(path, serde_json::to_string_pretty(&settings).unwrap_or(raw))?;
    Ok(true)
}

pub fn create_config_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/config", get(list_all_settings))
        .route("/api/config/protected_commands", get(get_protected_commands))
        .route("/api/config/protected_commands", post(set_protected_commands))
        .route("/api/config/auto_update", get(get_auto_update))
        .route("/api/config/auto_update", post(set_auto_update))
        .route("/api/config/idle_reminder", get(get_idle_reminder))
        .route("/api/config/idle_reminder", post(set_idle_reminder))
        .route("/api/config/gpt_api", get(get_gpt_api))
        .route("/api/config/gpt_api/refresh", post(refresh_claude_settings))
        .route("/api/config/gpt_api/active_role", get(get_active_role))
        .route("/api/config/gpt_api/active_role", post(set_active_role))
        .route("/api/config/gpt_api/current_model", get(get_current_model))
        .route("/api/config/gpt_api/switch_model", post(switch_model))
        .route("/api/config/gpt_api/{role}/models", get(list_models))
        .route("/api/config/debug-icon", get(get_debug_icon))
        .route("/api/config/{key}", get(get_setting))
        .route("/api/config/{key}", post(set_setting))
        .route("/api/config/{key}", delete(delete_setting))
}

/// GET /api/config/debug-icon — 输出 [debug] icon_path 配置的自定义图片。
/// 未配置或文件不可读返回 404，前端 <img> onerror 落回内置开源 SVG。
async fn get_debug_icon(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let icon_path = state.config_manager.config().debug.icon_path.clone();
    let Some(path) = icon_path.map(|p| shellexpand::tilde(&p).into_owned()).filter(|p| !p.trim().is_empty()) else {
        return (StatusCode::NOT_FOUND, "debug icon not configured").into_response();
    };
    let path = PathBuf::from(path);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let content_type = match path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("gif") => "image/gif",
                Some("webp") => "image/webp",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "debug icon file unreadable").into_response(),
    }
}

/// GET /api/config — list all settings under "settings." prefix
async fn list_all_settings(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.settings_store.list("settings.") {
        Ok(entries) => {
            let map: serde_json::Map<String, serde_json::Value> = entries
                .into_iter()
                .map(|(k, v)| {
                    let key = k.strip_prefix("settings.").unwrap_or(&k).to_string();
                    let val: serde_json::Value = serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v));
                    (key, val)
                })
                .collect();
            (StatusCode::OK, Json(serde_json::Value::Object(map))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// GET /api/config/protected_commands — the 保护指令 list (basename matches ask)
async fn get_protected_commands(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let perms = &state.config_manager.config().permissions;
    (StatusCode::OK, Json(serde_json::json!({
        "commands": perms.protected_commands,
        "history_count": perms.approval_history_count,
    }))).into_response()
}

/// POST /api/config/protected_commands — replace the 保护指令 list and/or the
/// decision-history retention (number of records, 0 = forever). settings.json
/// resync and session restart only happen when the command list itself changed.
async fn set_protected_commands(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let commands: Vec<String> = body
        .get("commands")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let history_count = body
        .get("history_count")
        .and_then(|d| d.as_u64())
        .map(|d| d.min(u32::MAX as u64) as u32);

    let commands_changed = state.config_manager.config().permissions.protected_commands != commands;

    if let Err(e) = state.config_manager.set_protected_commands(commands.clone()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    if let Some(count) = history_count {
        if let Err(e) = state.config_manager.set_approval_history_count(count) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    // Sync ~/.claude/settings.json ask rules (patch-only when the file
    // exists), then restart running claude sessions so both layers — CLI ask
    // rules and console-side approval_required — see the same policy
    if commands_changed {
        let sm = state.session_manager.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = write_claude_settings(&state_clone, None, false).await {
                tracing::warn!("Failed to sync local settings.json: {}", e);
            }
            for keyword in state_clone.project_manager.remote_keywords() {
                if let Err(e) = write_remote_claude_settings(&state_clone, None, &keyword, false).await {
                    tracing::warn!("Failed to sync settings.json to remote host {}: {}", keyword, e);
                }
            }
            if let Err(e) = sm.restart_sessions_for_config("claude").await {
                tracing::warn!("Config change restart failed: {}", e);
            }
        });
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "commands": commands,
            "history_count": state.config_manager.config().permissions.approval_history_count,
        })),
    )
        .into_response()
}

/// GET /api/config/auto_update — whether startup auto-config updates run
async fn get_auto_update(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({
        "enabled": state.config_manager.config().updates.auto_config_update,
    }))).into_response()
}

/// POST /api/config/auto_update — toggle startup auto-config updates (defaults
/// backfill). Takes effect on the next restart.
async fn set_auto_update(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "enabled (bool) is required" })),
        )
            .into_response();
    };
    match state.config_manager.set_auto_config_update(enabled) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "enabled": enabled }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// GET /api/config/idle_reminder — global idle-reminder switch and threshold (seconds)
async fn get_idle_reminder(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let sessions = &state.config_manager.config().project.sessions;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "enabled": sessions.idle_reminder_enabled,
            "idle_seconds": sessions.idle_seconds,
        })),
    )
        .into_response()
}

/// POST /api/config/idle_reminder — persist switch + threshold (seconds, 30..=900)
async fn set_idle_reminder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "enabled (bool) is required" })),
        )
            .into_response();
    };
    let Some(idle_seconds) = body.get("idle_seconds").and_then(|v| v.as_u64()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "idle_seconds (u64) is required" })),
        )
            .into_response();
    };
    if !(30..=900).contains(&idle_seconds) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "idle_seconds must be 30..=900" })),
        )
            .into_response();
    }
    match state.config_manager.set_idle_reminder(enabled, idle_seconds) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ok", "enabled": enabled, "idle_seconds": idle_seconds })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
            .into_response(),
    }
}

/// GET /api/config/gpt_api — get primary/secondary GPT API keys, masked
async fn get_gpt_api(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let primary = get_key(&state, "settings.gpt_api.primary");
    let secondary = get_key(&state, "settings.gpt_api.secondary");
    // active_role rides along so a caller that renders one role's models never has to
    // guess which one it is, and never sees the two disagree.
    (StatusCode::OK, Json(serde_json::json!({
        "primary": primary,
        "secondary": secondary,
        "active_role": active_role(&state),
    }))).into_response()
}

/// The role whose GPT API config is in effect, defaulting to primary when unset.
fn active_role(state: &AppState) -> String {
    state.settings_store
        .get("settings.gpt_api.active_role")
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<String>(&v).ok())
        .unwrap_or_else(|| "primary".to_string())
}

/// GET /api/config/{key} — get a single settings value
async fn get_setting(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let full_key = ensure_settings_prefix(&key);
    match state.settings_store.get(&full_key) {
        Ok(Some(value)) => {
            let val: serde_json::Value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            (StatusCode::OK, Json(val)).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "key not found" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// POST /api/config/{key} — set a settings value
async fn set_setting(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Json(mut value): Json<serde_json::Value>,
) -> impl IntoResponse {
    let full_key = ensure_settings_prefix(&key);

    // Defense-in-depth: preserve the real api_key if the payload has a masked/empty/missing one.
    // This handles three cases:
    // 1. api_key is empty string
    // 2. api_key is the masked sentinel "*********"
    // 3. api_key field is missing entirely (e.g., frontend sends undefined which gets omitted)
    if let Some(obj) = value.as_object_mut() {
        let needs_preserve = match obj.get("api_key").and_then(|v| v.as_str()) {
            Some(v) => v.is_empty() || v == "*********",
            None => true, // field missing entirely
        };

        if needs_preserve {
            if let Ok(Some(existing)) = state.settings_store.get(&full_key) {
                if let Some(existing_val) = serde_json::from_str::<serde_json::Value>(&existing).ok() {
                    if let Some(real_key) = existing_val.get("api_key").cloned() {
                        obj.insert("api_key".to_string(), real_key);
                    }
                }
            }
        }
    }

    let value_str = serde_json::to_string(&value).unwrap_or_default();
    match state.settings_store.set(&full_key, &value_str) {
        Ok(()) => {
            // If this key affects session behavior, restart affected sessions
            if full_key == "settings.auto_approve" {
                if let Some(val) = value.as_bool() {
                    let _ = state.config_manager.set_auto_approve_for_all(val);
                    let sm = state.session_manager.clone();
                    tokio::spawn(async move {
                        if let Err(e) = sm.restart_sessions_for_config("claude").await {
                            tracing::warn!("Config change restart failed: {}", e);
                        }
                    });
                }
            }

            // If GPT API config was saved, sync settings.json to local and remote hosts
            if full_key.starts_with("settings.gpt_api.") {
                let state_clone = state.clone();
                tokio::spawn(async move {
                    // Sync local settings.json
                    if let Err(e) = write_claude_settings(&state_clone, None, true).await {
                        tracing::warn!("Failed to sync local settings.json: {}", e);
                    }
                    if let Err(e) = sync_codex_auth(&state_clone).await {
                        tracing::warn!("Failed to sync codex auth.json: {}", e);
                    }

                    // Sync to all remote SSH hosts
                    let remote_keywords = state_clone.project_manager.remote_keywords();
                    for keyword in remote_keywords {
                        if let Err(e) = write_remote_claude_settings(&state_clone, None, &keyword, true).await {
                            tracing::warn!("Failed to sync settings.json to remote host {}: {}", keyword, e);
                        }
                    }
                });
            }

            (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "key": full_key }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// DELETE /api/config/{key} — delete a settings value
async fn delete_setting(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let full_key = ensure_settings_prefix(&key);
    match state.settings_store.delete(&full_key) {
        Ok(true) => (StatusCode::OK, Json(serde_json::json!({ "status": "deleted", "key": full_key }))).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "key not found" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

fn get_key(state: &AppState, key: &str) -> Option<serde_json::Value> {
    let value = state.settings_store.get(key).ok()??;
    let mut val: serde_json::Value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
    // Mask api_key if present
    if let Some(obj) = val.as_object_mut() {
        if obj.contains_key("api_key") {
            obj.insert("api_key".to_string(), serde_json::Value::String("*********".to_string()));
        }
    }
    Some(val)
}

fn ensure_settings_prefix(key: &str) -> String {
    if key.starts_with("settings.") {
        key.to_string()
    } else {
        format!("settings.{}", key)
    }
}

/// GET /api/config/gpt_api/active_role — get which role is active (primary or secondary)
async fn get_active_role(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.settings_store.get("settings.gpt_api.active_role") {
        Ok(Some(value)) => {
            let role: String = serde_json::from_str(&value).unwrap_or(value);
            (StatusCode::OK, Json(serde_json::json!({ "role": role }))).into_response()
        }
        Ok(None) => (StatusCode::OK, Json(serde_json::json!({ "role": "primary" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// POST /api/config/gpt_api/active_role — set which role is active
async fn set_active_role(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let role = body.get("role").and_then(|r| r.as_str()).unwrap_or("primary");
    if role != "primary" && role != "secondary" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "role must be primary or secondary" }))).into_response();
    }

    let value_str = serde_json::to_string(&role).unwrap_or_default();
    match state.settings_store.set("settings.gpt_api.active_role", &value_str) {
        Ok(()) => {
            // Trigger Claude settings update
            if let Err(e) = write_claude_settings(&state, None, true).await {
                tracing::warn!("Failed to update Claude settings: {}", e);
            }
            if let Err(e) = sync_codex_auth(&state).await {
                tracing::warn!("Failed to sync codex auth.json: {}", e);
            }
            (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "role": role }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// POST /api/config/gpt_api/refresh — manually trigger Claude settings update
async fn refresh_claude_settings(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let claude = write_claude_settings(&state, None, true).await;
    let codex = sync_codex_auth(&state).await;
    match (claude, codex) {
        (Ok(c), Ok(x)) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "message": format!("{c}；{x}") }))).into_response(),
        (Err(e), _) | (_, Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))).into_response(),
    }
}

/// 纯函数核心：把新 OPENAI_API_KEY 合并进 auth.json 内容。
/// 保留其他字段（如 ChatGPT 登录 tokens）；返回 (新内容, 是否有变化)。
fn merge_codex_auth(existing: Option<&str>, api_key: &str) -> Result<(String, bool), String> {
    let mut auth: serde_json::Value = match existing {
        Some(content) => serde_json::from_str(content)
            .map_err(|e| format!("~/.codex/auth.json 解析失败: {e}"))?,
        None => serde_json::json!({}),
    };
    if !auth.is_object() {
        auth = serde_json::json!({});
    }
    let changed = auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()) != Some(api_key);
    auth.as_object_mut()
        .unwrap()
        .insert("OPENAI_API_KEY".to_string(), serde_json::Value::String(api_key.to_string()));
    let content = serde_json::to_string_pretty(&auth).map_err(|e| e.to_string())?;
    Ok((content, changed))
}

/// 把默认模型写进 ~/.codex/config.toml 的 `model` 键（幂等，值相同不重写）。
/// key 与 provider 不配对会 401，model 与 key 不配对会 403——GPT API 角色的
/// model[0] 就是该 relay 的默认模型，同步它才能兜住首轮 turn/start（不带
/// model 参数时 codex 回落 config.toml）。
fn set_codex_config_model(content: &str, model: &str) -> Result<(String, bool), String> {
    let mut cfg: toml::Value =
        toml::from_str(content).map_err(|e| format!("~/.codex/config.toml 解析失败: {e}"))?;
    if cfg.get("model").and_then(|v| v.as_str()) == Some(model) {
        return Ok((content.to_string(), false));
    }
    if let toml::Value::Table(table) = &mut cfg {
        table.insert("model".to_string(), toml::Value::String(model.to_string()));
    }
    let out = toml::to_string(&cfg).map_err(|e| format!("序列化 config.toml 失败: {e}"))?;
    Ok((out, true))
}

/// 从 URL（可带 scheme/路径/端口）提取小写主机名。
fn hostname_of(url: &str) -> Option<String> {
    let no_scheme = match url.trim().split_once("://") {
        Some((_, rest)) => rest,
        None => url.trim(),
    };
    let host = no_scheme.split(['/', '?']).next()?.split(':').next()?;
    let host = host.trim().to_lowercase();
    if host.is_empty() { None } else { Some(host) }
}

/// 解析 ~/.codex/config.toml 的默认 provider 主机名（model_provider →
/// model_providers.<name>.base_url）。没有自定义 provider = codex 用官方
/// OpenAI 登录，auth.json 由宿主机自己管理，返回 None。
fn codex_provider_hostname() -> Option<String> {
    let path = dirs::home_dir()?.join(".codex").join("config.toml");
    let content = std::fs::read_to_string(path).ok()?;
    let cfg: toml::Value = toml::from_str(&content).ok()?;
    let name = cfg.get("model_provider")?.as_str()?;
    let base_url = cfg
        .get("model_providers")?
        .get(name)?
        .get("base_url")?
        .as_str()?;
    hostname_of(base_url)
}

/// 在 primary/secondary 里选出 host 与 codex provider 匹配的角色：
/// 两个都匹配时优先活动角色，都不匹配返回 None（不能把别家的 key
/// 写进 codex 的 auth.json——key 与 provider 不配对是必 401）。
fn pick_codex_role(
    codex_host: Option<&str>,
    active: &str,
    primary_host: Option<&str>,
    secondary_host: Option<&str>,
) -> Option<String> {
    let codex_host = codex_host?;
    let matches = |host: Option<&str>| host.is_some_and(|h| hostname_of(h).as_deref() == Some(codex_host));
    let (active_first, other) = if active == "secondary" {
        ("secondary", "primary")
    } else {
        ("primary", "secondary")
    };
    let host_of = |role: &str| if role == "primary" { primary_host } else { secondary_host };
    if matches(host_of(active_first)) {
        Some(active_first.to_string())
    } else if matches(host_of(other)) {
        Some(other.to_string())
    } else {
        None
    }
}

/// 把 GPT API key 同步进 ~/.codex/auth.json（codex 的凭证来源）。
/// 按 host 匹配选角色：codex 的 provider 指向哪个 relay，就同步那个
/// relay 对应角色的 key；GPT API 是平台统一的 key 管理入口——claude 落
/// settings.json（活动角色），codex 落 auth.json（host 匹配的角色），
/// 缺一边就会出现"刷新了设置但 codex 还在用旧 token"的 401。
pub async fn sync_codex_auth(state: &AppState) -> Result<String, String> {
    let instances = &state.config_manager.config().agent.instances;
    let allow_overwrite = instances.get("codex").map(|a| a.allow_config_overwrite).unwrap_or(true);
    if !allow_overwrite {
        tracing::info!("codex agent has allow_config_overwrite=false, skipping auth.json update");
        return Ok("codex 写保护已启用，忽略写入".to_string());
    }

    let role_config = |role: &str| -> Option<(String, Option<String>, Option<String>)> {
        let v = state.settings_store.get(&format!("settings.gpt_api.{role}")).ok()??;
        let cfg: serde_json::Value = serde_json::from_str(&v).ok()?;
        let host = cfg.get("host").and_then(|h| h.as_str()).map(str::to_string);
        let key = cfg.get("api_key").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let model = cfg
            .get("model")
            .and_then(|m| m.as_array())
            .and_then(|list| list.first())
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        Some((key, host, model))
    };

    let active = active_role(state);
    let primary = role_config("primary");
    let secondary = role_config("secondary");
    if primary.is_none() && secondary.is_none() {
        tracing::info!("GPT API not configured, skipping codex auth.json write");
        return Ok("GPT API 未配置，跳过写入".to_string());
    }

    let codex_host = codex_provider_hostname();
    let matched = pick_codex_role(
        codex_host.as_deref(),
        &active,
        primary.as_ref().and_then(|(_, h, _)| h.as_deref()),
        secondary.as_ref().and_then(|(_, h, _)| h.as_deref()),
    );
    let Some(role) = matched else {
        let provider = codex_host.unwrap_or_default();
        tracing::info!(
            provider = %provider,
            "no GPT API role host matches codex provider, skipping auth.json write"
        );
        return Ok(format!(
            "GPT API 角色 host 与 codex provider（{}）不匹配，跳过写入",
            if provider.is_empty() { "官方登录" } else { &provider }
        ));
    };

    let (api_key, _, role_model) = if role == "primary" { primary } else { secondary }.unwrap();
    if api_key.is_empty() && role_model.is_none() {
        tracing::info!("GPT API {} has neither api_key nor model, skipping codex sync", role);
        return Ok(format!("GPT API {} 未配置 api_key，跳过写入", role));
    }

    let codex_dir = dirs::home_dir()
        .ok_or_else(|| "Cannot find home directory".to_string())?
        .join(".codex");
    std::fs::create_dir_all(&codex_dir).map_err(|e| format!("create ~/.codex failed: {e}"))?;

    let mut messages: Vec<String> = Vec::new();

    if !api_key.is_empty() {
        let auth_path = codex_dir.join("auth.json");
        let existing = std::fs::read_to_string(&auth_path).ok();
        let (content, changed) = merge_codex_auth(existing.as_deref(), &api_key)?;
        if changed {
            std::fs::write(&auth_path, &content)
                .map_err(|e| format!("write auth.json failed: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &auth_path,
                    std::fs::Permissions::from_mode(0o600),
                );
            }
            tracing::info!(role = %role, "Synced GPT API key into ~/.codex/auth.json");
            messages.push(format!("已同步 GPT API（{role}）key 到 ~/.codex/auth.json"));
        }
    }

    // 默认模型与 key 同源同 relay：首轮 turn/start 不带 model 参数时 codex
    // 回落 config.toml，不同步就会出现"key 刷新了但默认模型无权限"的 403。
    if let Some(model) = role_model {
        let config_path = codex_dir.join("config.toml");
        let existing = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("read config.toml failed: {e}"))?;
        match set_codex_config_model(&existing, &model) {
            Ok((content, true)) => {
                std::fs::write(&config_path, &content)
                    .map_err(|e| format!("write config.toml failed: {e}"))?;
                tracing::info!(role = %role, model = %model, "Synced default model into ~/.codex/config.toml");
                messages.push(format!("已同步默认模型 {model} 到 ~/.codex/config.toml"));
            }
            Ok((_, false)) => {}
            Err(e) => {
                tracing::warn!("codex config.toml model sync failed: {}", e);
            }
        }
    }

    if messages.is_empty() {
        return Ok("codex auth.json 已是最新".to_string());
    }
    Ok(messages.join("；"))
}

/// Write ~/.claude/settings.json based on active GPT API config
/// Uses a hard-coded template (CLAUDE_SETTINGS_TEMPLATE) to ensure consistent structure.
/// If selected_model is provided, uses that model; otherwise falls back to model[0] from config.
/// If force_overwrite is false, only creates settings.json if it doesn't exist.
/// Returns Ok(message) on success, where message describes what happened.
/// Returns Err(message) on failure.
pub async fn write_claude_settings(state: &AppState, selected_model: Option<&str>, force_overwrite: bool) -> Result<String, String> {
    // Check if ClaudeCode agent allows config overwrite
    let instances = &state.config_manager.config().agent.instances;
    let claude_agent = instances.get("claude");
    let allow_overwrite = claude_agent.map(|a| a.allow_config_overwrite).unwrap_or(true);

    if !allow_overwrite {
        tracing::info!("ClaudeCode agent has allow_config_overwrite=false, skipping settings.json update");
        return Ok("写保护已启用，忽略写入".to_string());
    }

    // Determine settings.json path early (needed for existence check)
    let claude_dir = dirs::home_dir()
        .ok_or_else(|| "Cannot find home directory".to_string())?
        .join(".claude");
    let settings_path = claude_dir.join("settings.json");

    // When not force-overwriting an existing settings.json, converge only the
    // protection policy — a full regen would reset the model/env the user
    // selected (priority selection may pick a model their plan can't serve).
    if !force_overwrite && settings_path.exists() {
        let protected = state.config_manager.config().permissions.protected_commands.clone();
        match patch_ask_rules_in_place(&settings_path, &protected) {
            Ok(true) => {
                tracing::info!("Patched permissions.ask rules into existing settings.json");
                return Ok("已同步保护指令规则到现有 settings.json".to_string());
            }
            Ok(false) => {} // missing/unparsable — fall through to full write below
            Err(e) => return Err(format!("Failed to patch settings.json: {}", e)),
        }
    }

    // Get active role (default to primary)
    let active_role = active_role(state);

    // Get config for active role
    let config_key = format!("settings.gpt_api.{}", active_role);
    let config_value = match state.settings_store.get(&config_key) {
        Ok(Some(v)) => v,
        _ => {
            // GPT API not configured — silently skip (don't error out)
            tracing::info!("GPT API {} not configured, skipping settings.json write", active_role);
            return Ok(format!("GPT API {} 未配置，跳过写入", active_role));
        }
    };

    let config: serde_json::Value = serde_json::from_str(&config_value)
        .map_err(|e| e.to_string())?;

    let host = config.get("host").and_then(|h| h.as_str()).unwrap_or("");
    let api_key = config.get("api_key").and_then(|k| k.as_str()).unwrap_or("");
    let models = config.get("model").and_then(|m| m.as_array());

    // If config is incomplete, do NOT overwrite existing settings.json
    if host.is_empty() || api_key.is_empty() {
        let missing = if host.is_empty() && api_key.is_empty() {
            "host and api_key"
        } else if host.is_empty() {
            "host"
        } else {
            "api_key"
        };
        tracing::warn!("GPT API {} config incomplete ({}), skipping settings.json overwrite", active_role, missing);
        return Err(format!("GPT API {} config incomplete: {} not configured", active_role, missing));
    }

    // Model selection: prefer selected_model, then use priority-based selection
    let model = selected_model
        .map(|s| s.to_string())
        .or_else(|| select_model_by_priority(models))
        .unwrap_or_else(|| "gpt-4".to_string());

    // Build settings from hard-coded template
    let mut settings: serde_json::Value = serde_json::from_str(CLAUDE_SETTINGS_TEMPLATE)
        .map_err(|e| format!("Failed to parse template: {}", e))?;

    // Update env block
    let env = settings
        .as_object_mut()
        .ok_or_else(|| "settings is not an object".to_string())?
        .get_mut("env")
        .ok_or_else(|| "env not found in template".to_string())?
        .as_object_mut()
        .ok_or_else(|| "env is not an object".to_string())?;

    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), serde_json::Value::String(api_key.to_string()));
    env.insert("ANTHROPIC_BASE_URL".to_string(), serde_json::Value::String(host.to_string()));
    env.insert("ANTHROPIC_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_REASONING_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(), serde_json::Value::String(model.to_string()));

    // Update top-level model field
    settings
        .as_object_mut()
        .unwrap()
        .insert("model".to_string(), serde_json::Value::String(model.to_string()));

    // Force the CLI to route protected commands through the permission prompt
    apply_protected_ask_rules(
        &mut settings,
        &state.config_manager.config().permissions.protected_commands,
    );

    // Write to settings.json
    std::fs::create_dir_all(&claude_dir).map_err(|e| e.to_string())?;

    let content = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(&settings_path, content).map_err(|e| e.to_string())?;

    tracing::info!("Updated ~/.claude/settings.json with model={}, role={}", model, active_role);
    Ok(format!("已更新 ~/.claude/settings.json（模型: {}, 角色: {}）", model, active_role))
}

/// Write Claude settings to a remote host via SSH.
/// `force=false` (startup batch sync, ask-rule sync): skipped entirely for
/// hosts with sync_settings=false, and never touches a host that already has
/// a settings.json — that file is the host's own. Only a missing file is
/// generated (e.g. after a container restart wiped ~/.claude). Explicit user
/// actions (model switch, GPT API save) pass force=true for a full rewrite.
pub async fn write_remote_claude_settings(
    state: &AppState,
    selected_model: Option<&str>,
    project_key: &str,
    force: bool,
) -> Result<String, String> {
    // Check if ClaudeCode agent allows config overwrite
    let instances = &state.config_manager.config().agent.instances;
    let claude_agent = instances.get("claude");
    let allow_overwrite = claude_agent.map(|a| a.allow_config_overwrite).unwrap_or(true);

    if !allow_overwrite {
        tracing::info!("ClaudeCode agent has allow_config_overwrite=false, skipping remote settings.json update");
        return Ok("写保护已启用，忽略写入".to_string());
    }

    // Get config and discovery path config for this project key
    let config = state.config_manager.config();
    let discovery_path = config.project.get_discovery_path(project_key)
        .ok_or_else(|| format!("Project key '{}' not found in discovery paths", project_key))?;

    if discovery_path.remote_mode != "ssh" {
        return Err(format!("Project key '{}' is not a remote SSH project", project_key));
    }

    // Non-explicit syncs (startup pass, protected-commands rule sync) must
    // respect the per-host switch: sync_settings=false means the host keeps
    // its own settings.json untouched. Explicit user actions (model switch,
    // GPT API save) pass force=true and are unaffected.
    if !force && !discovery_path.sync_settings {
        tracing::info!(keyword = %project_key, "sync_settings=false, skipping remote settings sync");
        return Ok(format!("{} 已禁用 settings 同步（sync_settings=false），跳过", project_key));
    }

    let hostname = discovery_path.hostname.clone();
    if hostname.is_empty() {
        return Err(format!("Project key '{}' has empty hostname", project_key));
    }

    // Get SSH config path
    let ssh_config_path = discovery_path.ssh
        .as_ref()
        .map(|s| s.config_path.clone())
        .unwrap_or_else(|| config.ssh.config_path.clone());

    // Get active role (default to primary)
    let active_role = active_role(state);

    // Get config for active role
    let config_key = format!("settings.gpt_api.{}", active_role);
    let config_value = state.settings_store
        .get(&config_key)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("GPT API {} not configured", active_role))?;

    let config: serde_json::Value = serde_json::from_str(&config_value)
        .map_err(|e| e.to_string())?;

    let host = config.get("host").and_then(|h| h.as_str()).unwrap_or("");
    let api_key = config.get("api_key").and_then(|k| k.as_str()).unwrap_or("");
    let models = config.get("model").and_then(|m| m.as_array());

    if host.is_empty() || api_key.is_empty() {
        return Err(format!("GPT API {} config incomplete", active_role));
    }

    // Model selection: prefer selected_model, then use priority-based selection
    let model = selected_model
        .map(|s| s.to_string())
        .or_else(|| select_model_by_priority(models))
        .unwrap_or_else(|| "gpt-4".to_string());

    // Build settings from hard-coded template
    let mut settings: serde_json::Value = serde_json::from_str(CLAUDE_SETTINGS_TEMPLATE)
        .map_err(|e| format!("Failed to parse template: {}", e))?;

    // Update env block
    let env = settings
        .as_object_mut()
        .ok_or_else(|| "settings is not an object".to_string())?
        .get_mut("env")
        .ok_or_else(|| "env not found in template".to_string())?
        .as_object_mut()
        .ok_or_else(|| "env is not an object".to_string())?;

    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), serde_json::Value::String(api_key.to_string()));
    env.insert("ANTHROPIC_BASE_URL".to_string(), serde_json::Value::String(host.to_string()));
    env.insert("ANTHROPIC_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_REASONING_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), serde_json::Value::String(model.to_string()));
    env.insert("ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(), serde_json::Value::String(model.to_string()));

    // Update top-level model field
    settings
        .as_object_mut()
        .unwrap()
        .insert("model".to_string(), serde_json::Value::String(model.to_string()));

    // Force the CLI to route protected commands through the permission prompt
    let protected = state.config_manager.config().permissions.protected_commands.clone();
    apply_protected_ask_rules(&mut settings, &protected);

    let content = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;

    // Build SSH arguments with timeout
    let expanded_config = expand_tilde(&ssh_config_path);
    let config_exists = std::path::Path::new(&expanded_config).exists();

    let mut ssh_args = vec![
        "-o".to_string(), "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(), "ConnectTimeout=10".to_string(),
    ];
    if config_exists {
        ssh_args.extend(["-F".to_string(), expanded_config]);
    }
    ssh_args.push(hostname.to_string());

    // An existing remote settings.json is entirely under the host's control —
    // never touch it from a non-explicit pass (patching rules into a file the
    // host maintains itself invites split-brain policy). A non-zero exit means
    // the file is missing (or ssh is broken — the write attempt below will
    // then surface the real error).
    if !force {
        let check_output = tokio::process::Command::new("ssh")
            .args(&ssh_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .arg("test -f ~/.claude/settings.json")
            .output()
            .await;
        if matches!(check_output, Ok(output) if output.status.success()) {
            tracing::info!("Remote ~/.claude/settings.json already exists on {}, leaving it untouched", hostname);
            return Ok(format!("远程 {} 的 ~/.claude/settings.json 已存在，由主机自行管理，跳过", hostname));
        }
    }

    // Retry logic: 3 attempts
    let max_retries = 3;
    let mut last_error = String::new();

    for attempt in 1..=max_retries {
        // Create ~/.claude directory on remote host
        let mkdir_cmd = "mkdir -p ~/.claude";
        let mkdir_output = tokio::process::Command::new("ssh")
            .args(&ssh_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .arg(mkdir_cmd)
            .output()
            .await;

        if let Err(e) = mkdir_output {
            last_error = format!("Failed to create ~/.claude on remote host: {}", e);
            tracing::warn!(attempt, max_retries, error = %last_error, "SSH mkdir failed, retrying...");
            if attempt < max_retries {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
            continue;
        }

        // Write settings.json to remote host using cat with heredoc
        let write_cmd = format!("cat > ~/.claude/settings.json << 'SETTINGS_EOF'\n{}\nSETTINGS_EOF", content);

        let write_output = tokio::process::Command::new("ssh")
            .args(&ssh_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .arg(&write_cmd)
            .output()
            .await;

        match write_output {
            Ok(output) => {
                if output.status.success() {
                    tracing::info!("Updated remote ~/.claude/settings.json on {} with model={}, role={}", hostname, model, active_role);
                    return Ok(format!("已更新远程 {} 的 ~/.claude/settings.json（模型: {}, 角色: {}）", hostname, model, active_role));
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    last_error = format!("Failed to write settings on remote host: {}", stderr);
                    tracing::warn!(attempt, max_retries, error = %last_error, "SSH write failed, retrying...");
                    if attempt < max_retries {
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            }
            Err(e) => {
                last_error = format!("Failed to execute SSH command: {}", e);
                tracing::warn!(attempt, max_retries, error = %last_error, "SSH command failed, retrying...");
                if attempt < max_retries {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
        }
    }

    Err(format!("Failed after {} attempts: {}", max_retries, last_error))
}

/// Select a model from the available models list based on priority.
/// Family order: GLM -> Deepseek -> Qwen -> KIMI -> Claude -> Chatgpt -> Codex -> Gemini.
/// Within a family: `flashx` variant, else `flash` variant, else the first member.
pub(crate) fn select_model_by_priority(models: Option<&Vec<serde_json::Value>>) -> Option<String> {
    let arr = models?;
    if arr.is_empty() {
        return None;
    }

    // Family priority, highest first. glm deliberately outranks deepseek.
    let priority_keywords = [
        "glm", "deepseek", "qwen", "kimi", "claude", "chatgpt", "gpt", "codex", "gemini",
    ];

    // Within a family, tiers rank flashx > flash > everything else. List order is just
    // whatever the user last saved or fetched, so picking the best tier explicitly keeps
    // the cheap/fast default from depending on that ordering — e.g. deepseek-v4-pro
    // sitting ahead of deepseek-v4-flash must not win.
    //
    // The check is a bare substring on purpose: families name their fast tiers
    // "<family>-<id>-flash" / "<family>-<id>-flashx" (glm-5.3-flash, deepseek-v4-flash),
    // so one rule covers them all and a new family needs no special case. The tier is
    // matched longest-suffix-first so "flashx" is not swallowed by the "flash" check.
    for keyword in &priority_keywords {
        // Lower tier wins. Within the same tier the first name seen wins.
        let mut best: Option<(u8, &str)> = None;
        for model_val in arr {
            if let Some(name) = model_val.as_str() {
                let lower = name.to_lowercase();
                if !lower.contains(keyword) {
                    continue;
                }
                let tier = if lower.contains("flashx") {
                    0
                } else if lower.contains("flash") {
                    1
                } else {
                    2
                };
                if best.is_none_or(|(best_tier, _)| tier < best_tier) {
                    best = Some((tier, name));
                }
            }
        }
        if let Some((_, name)) = best {
            return Some(name.to_string());
        }
    }

    // No priority match — fall back to the first model in the list
    arr.first()
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}

/// Expand `~/` prefix to actual home directory
fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.to_string_lossy(), &path[2..]);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// GET /api/config/gpt_api/current_model — get current model from settings.json
async fn get_current_model(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let claude_dir = match dirs::home_dir() {
        Some(dir) => dir.join(".claude"),
        None => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": "Cannot find home directory"
            }))).into_response();
        }
    };
    let settings_path = claude_dir.join("settings.json");

    if !settings_path.exists() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "settings.json not found"
        }))).into_response();
    }

    match std::fs::read_to_string(&settings_path) {
        Ok(content) => {
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(settings) => {
                    let model = settings.get("model")
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    (StatusCode::OK, Json(serde_json::json!({
                        "model": model
                    }))).into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                    "error": format!("Failed to parse settings: {}", e)
                }))).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "error": format!("Failed to read settings: {}", e)
        }))).into_response(),
    }
}

/// POST /api/config/gpt_api/switch_model — update settings.json with selected model
async fn switch_model(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let model = body.get("model").and_then(|m| m.as_str()).unwrap_or("");
    if model.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "model is required"
        }))).into_response();
    }

    // Get project_key to determine if this is a remote project
    // project_key format: "keyword/project/agent" (e.g., "huawei-m4-01/test/claude")
    // We need to extract just the keyword (first part) to check if it's remote
    let project_key = body.get("project_key").and_then(|k| k.as_str());

    // Extract keyword from project_key (first segment before '/')
    let keyword = project_key.map(|k| k.split('/').next().unwrap_or(k));

    // Discovery paths may forbid model switching (e.g. the host manages its own
    // Claude settings). Reject before anything gets written.
    if let Some(kw) = keyword {
        let config = state.config_manager.config();
        if let Some(dp) = config.project.get_discovery_path(kw) {
            if !dp.sync_models {
                tracing::info!(keyword = %kw, "switch_model: rejected, sync_models = false");
                return (StatusCode::FORBIDDEN, Json(serde_json::json!({
                    "error": format!("该配置（{}）已禁用模型切换（sync_models = false）", kw)
                }))).into_response();
            }
        }
    }

    // Check if this is a remote project by checking if the keyword is in remote_keywords
    let is_remote = if let Some(kw) = keyword {
        state.project_manager.remote_keywords().iter().any(|k| k == kw)
    } else {
        false
    };

    tracing::info!("switch_model: project_key={:?}, keyword={:?}, is_remote={}", project_key, keyword, is_remote);

    if is_remote {
        // For remote projects, update settings on the remote host
        match write_remote_claude_settings(&state, Some(model), keyword.unwrap(), true).await {
            Ok(msg) => (StatusCode::OK, Json(serde_json::json!({
                "status": "ok",
                "message": msg,
                "model": model
            }))).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": e
            }))).into_response(),
        }
    } else {
        // For local projects, update local settings.json
        let claude = write_claude_settings(&state, Some(model), true).await;
        let codex = sync_codex_auth(&state).await;
        match (claude, codex) {
            (Ok(msg), Ok(x)) => (StatusCode::OK, Json(serde_json::json!({
                "status": "ok",
                "message": format!("{msg}；{x}"),
                "model": model
            }))).into_response(),
            (Err(e), _) | (_, Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": e
            }))).into_response(),
        }
    }
}

/// GET /api/config/gpt_api/{role}/models — fetch available models from the configured API
async fn list_models(
    State(state): State<Arc<AppState>>,
    Path(role): Path<String>,
) -> impl IntoResponse {
    if role != "primary" && role != "secondary" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "role must be primary or secondary" }))).into_response();
    }
    let key = format!("settings.gpt_api.{}", role);
    match state.settings_store.get(&key) {
        Ok(Some(value)) => {
            let val: serde_json::Value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
            let host = val.get("host").and_then(|h| h.as_str()).unwrap_or("").trim();
            let api_key = val.get("api_key").and_then(|k| k.as_str()).unwrap_or("").trim();
            tracing::info!("list_models: host={} api_key_len={}", host, api_key.len());
            if host.is_empty() {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "host is not configured" }))).into_response();
            }
            if api_key.is_empty() || api_key == "*********" {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "api_key is missing or masked — please re-enter the key and save" }))).into_response();
            }
            // Model discovery lives at a different base URL for Anthropic-protocol
            // gateways (e.g. https://api.deepseek.com/anthropic implements
            // /v1/messages but 404s on /v1/models). models_url overrides the base
            // used here only; it never affects the runtime base URL.
            let models_url = val.get("models_url").and_then(|m| m.as_str()).unwrap_or("").trim();
            let base = if models_url.is_empty() { host } else { models_url };
            let client = reqwest::Client::new();
            let url = format!("{}/v1/models", base.trim_end_matches('/'));
            tracing::info!("list_models: fetching from url={}", url);
            match client.get(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        match resp.json::<serde_json::Value>().await {
                            Ok(body) => (StatusCode::OK, Json(body)).into_response(),
                            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("failed to parse response: {}", e) }))).into_response(),
                        }
                    } else {
                        let body = resp.text().await.unwrap_or_default();
                        tracing::warn!("list_models: upstream returned {} url={} body={}", status, url, body);
                        let msg = if status == reqwest::StatusCode::NOT_FOUND {
                            format!(
                                "{} 未实现 /v1/models，无法自动发现模型。请在「模型查询地址」填入支持该接口的地址（留空则使用主机地址）。",
                                url
                            )
                        } else {
                            format!("upstream returned {} {}", status, body)
                        };
                        (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": msg }))).into_response()
                    }
                }
                Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": format!("upstream request failed: {}", e) }))).into_response(),
            }
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no config found" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// Turn natural language into one executable shell command using the active
/// GPT API config. `terminal_hint` tells the model where the terminal lives
/// (local path or ssh host+path) so it can emit host-appropriate syntax.
/// Returns the raw model text — the caller strips fences/backticks.
/// The model currently active for sessions, read from ~/.claude/settings.json.
fn current_claude_model() -> Option<String> {
    let path = dirs::home_dir()?.join(".claude").join("settings.json");
    let content = std::fs::read_to_string(path).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&content).ok()?;
    let model = settings.get("model")?.as_str()?.trim().to_string();
    if model.is_empty() { None } else { Some(model) }
}

pub(crate) async fn nl_to_command(
    state: &AppState,
    terminal_hint: &str,
    text: &str,
) -> Result<String, String> {
    let role = active_role(state);
    let key = format!("settings.gpt_api.{}", role);
    let config_value = state
        .settings_store
        .get(&key)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("GPT API {} 未配置", role))?;
    let config: serde_json::Value = serde_json::from_str(&config_value).map_err(|e| e.to_string())?;
    let host = config.get("host").and_then(|h| h.as_str()).unwrap_or("").trim();
    let api_key = config.get("api_key").and_then(|k| k.as_str()).unwrap_or("").trim();
    if host.is_empty() {
        return Err(format!("GPT API {} 未配置 host", role));
    }
    if api_key.is_empty() || api_key == "*********" {
        return Err(format!("GPT API {} 的 api_key 缺失或已打码，请重新输入并保存", role));
    }
    // Reuse the model the sessions actually run on (~/.claude/settings.json
    // model, same source the session dropdown follows); priority list is the
    // fallback when nothing has been selected yet.
    let model = current_claude_model()
        .or_else(|| select_model_by_priority(config.get("model").and_then(|m| m.as_array())))
        .ok_or_else(|| "未找到可用模型：settings.json 无 model 且 GPT API 配置为空".to_string())?;
    tracing::info!(role = %role, model = %model, "terminal nl_to_command using active model");

    // The gateway speaks the Anthropic protocol; base is always host —
    // models_url only affects model listing, never the runtime base.
    let url = format!("{}/v1/messages", host.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 300,
        "system": "You translate the user's request into exactly ONE shell command for their terminal. Reply with the command only: single line, no explanation, no code fences, no backticks, no newlines. If the request is conversational or not actionable, reply with the single word: echo",
        "messages": [
            { "role": "user", "content": format!("[{}]\n{}", terminal_hint, text) }
        ]
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("上游请求失败: {}", e))?;
    let status = resp.status();
    let resp_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse response: {}", e))?;
    if !status.is_success() {
        return Err(format!("上游返回 {} {}", status, resp_body));
    }
    // Thinking models put a {type:"thinking"} block first — pick the first
    // text block, not content[0].
    let text = resp_body
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
        })
        .unwrap_or("")
        .to_string();
    if text.trim().is_empty() {
        return Err(format!("上游响应缺少 content/0/text: {}", resp_body));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_protected_ask_rules, hostname_of, merge_codex_auth, pick_codex_role,
        protected_ask_rules, prune_legacy_k8s_hook, select_model_by_priority,
        set_codex_config_model,
    };

    #[test]
    fn codex_auth_merge_replaces_key_preserves_other_fields() {
        let existing = r#"{"OPENAI_API_KEY":"sk-old","tokens":{"id_token":"t1"}}"#;
        let (content, changed) = merge_codex_auth(Some(existing), "sk-new").unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["OPENAI_API_KEY"], "sk-new");
        // ChatGPT 登录 tokens 等其他字段必须保留
        assert_eq!(v["tokens"]["id_token"], "t1");
    }

    #[test]
    fn codex_auth_merge_fresh_and_noop() {
        // 文件不存在 → 全新对象
        let (content, changed) = merge_codex_auth(None, "sk-fresh").unwrap();
        assert!(changed);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&content).unwrap()["OPENAI_API_KEY"],
            "sk-fresh"
        );
        // key 未变 → changed=false（调用方据此跳过写盘）
        let (_, changed) = merge_codex_auth(Some(r#"{"OPENAI_API_KEY":"sk-fresh"}"#), "sk-fresh").unwrap();
        assert!(!changed);
    }

    #[test]
    fn codex_auth_merge_corrupt_or_non_object_resets() {
        // 解析失败必须报错而不是静默覆盖
        assert!(merge_codex_auth(Some("{not json"), "sk-x").is_err());
        // 顶层非对象（数组）→ 重置为对象再写入
        let (content, changed) = merge_codex_auth(Some("[1,2]"), "sk-x").unwrap();
        assert!(changed);
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["OPENAI_API_KEY"], "sk-x");
    }

    #[test]
    fn codex_role_picked_by_provider_host() {
        let primary = Some("https://gpt.milletcode.cn");
        let secondary = Some("https://open.bigmodel.cn/api/anthropic");
        // 活动=secondary（bigmodel），但 codex provider 指向 relay → 选 primary
        assert_eq!(
            pick_codex_role(Some("gpt.milletcode.cn"), "secondary", primary, secondary),
            Some("primary".to_string())
        );
        // 两个角色都匹配同一 host → 优先活动角色
        assert_eq!(
            pick_codex_role(Some("gpt.milletcode.cn"), "secondary", primary, Some("https://gpt.milletcode.cn/v1")),
            Some("secondary".to_string())
        );
        // 无一匹配 → None（绝不把别家 key 写进 auth.json）
        assert_eq!(pick_codex_role(Some("api.openai.com"), "secondary", primary, secondary), None);
        // codex 无自定义 provider（官方登录）→ 不同步
        assert_eq!(pick_codex_role(None, "primary", primary, secondary), None);
        // 角色未配置 host → 视为不匹配
        assert_eq!(pick_codex_role(Some("gpt.milletcode.cn"), "primary", None, secondary), None);
    }

    #[test]
    fn hostname_extraction_strips_scheme_path_port() {
        assert_eq!(hostname_of("https://gpt.milletcode.cn/v1").as_deref(), Some("gpt.milletcode.cn"));
        assert_eq!(hostname_of("http://Host.Example:8443/api").as_deref(), Some("host.example"));
        assert_eq!(hostname_of("gpt.milletcode.cn").as_deref(), Some("gpt.milletcode.cn"));
        assert_eq!(hostname_of("https://"), None);
        assert_eq!(hostname_of(""), None);
    }

    #[test]
    fn codex_config_model_updates_only_when_different() {
        let base = r#"model = "gpt-5.5"
model_provider = "MilletCode"

[model_providers.MilletCode]
base_url = "https://gpt.milletcode.cn/v1"
"#;
        // 模型不同 → 重写且保留其他键
        let (out, changed) = set_codex_config_model(base, "glm-5.3").unwrap();
        assert!(changed);
        let cfg: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(cfg["model"].as_str(), Some("glm-5.3"));
        assert_eq!(cfg["model_provider"].as_str(), Some("MilletCode"));
        assert_eq!(
            cfg["model_providers"]["MilletCode"]["base_url"].as_str(),
            Some("https://gpt.milletcode.cn/v1")
        );
        // 模型相同 → 不重写（保留原文，避免无谓的注释丢失）
        let (out2, changed2) = set_codex_config_model(&out, "glm-5.3").unwrap();
        assert!(!changed2);
        assert_eq!(out2, out);
        // 没有 model 键 → 新增
        let (out3, changed3) =
            set_codex_config_model("model_provider = \"x\"\n", "glm-5.3").unwrap();
        assert!(changed3);
        assert_eq!(toml::from_str::<toml::Value>(&out3).unwrap()["model"].as_str(), Some("glm-5.3"));
        // 解析失败 → 报错不覆盖
        assert!(set_codex_config_model("not [valid toml", "glm-5.3").is_err());
    }

    // The production 保护指令 list, verbatim from ~/.milletcode/config.toml.
    #[test]
    fn ask_rules_from_production_list() {
        assert_eq!(
            protected_ask_rules(&["kubectl*".to_string(), "kube*".to_string()]),
            vec![
                "Bash(kube:*)".to_string(),
                "Bash(kubectl:*)".to_string(),
            ]
        );
    }

    #[test]
    fn ask_rules_edge_entries() {
        // "*" alone means every Bash call
        assert_eq!(protected_ask_rules(&["*".to_string()]), vec!["Bash".to_string()]);
        // "?" wildcard truncates like "*", junk and blanks drop out
        assert_eq!(
            protected_ask_rules(&["rm *?".to_string(), "  ".to_string(), "*".to_string()]),
            vec!["Bash".to_string(), "Bash(rm:*)".to_string()]
        );
        assert!(protected_ask_rules(&[]).is_empty());
    }

    #[test]
    fn ask_rules_stamp_into_template() {
        let mut settings: serde_json::Value =
            serde_json::from_str(super::CLAUDE_SETTINGS_TEMPLATE).unwrap();
        // full writes must never (re)introduce hooks — routing is ask-rules only
        assert!(settings.get("hooks").is_none());
        apply_protected_ask_rules(&mut settings, &["rm".to_string()]);
        assert_eq!(
            settings["permissions"]["ask"],
            serde_json::json!(["Bash(rm:*)"])
        );
        // must survive a round trip through the file format the writer emits
        let back: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(back["permissions"]["ask"], serde_json::json!(["Bash(rm:*)"]));
    }

    // A settings.json that already carries other permission rules must keep
    // them — the stamp merges the ask key, it does not replace the block.
    #[test]
    fn ask_rules_merge_preserves_sibling_rules() {
        let mut settings: serde_json::Value = serde_json::json!({
            "model": "glm-5.3",
            "permissions": {
                "allow": ["Read"],
                "deny": ["WebFetch"]
            }
        });
        apply_protected_ask_rules(&mut settings, &["rm".to_string()]);
        assert_eq!(settings["permissions"]["allow"], serde_json::json!(["Read"]));
        assert_eq!(settings["permissions"]["deny"], serde_json::json!(["WebFetch"]));
        assert_eq!(settings["permissions"]["ask"], serde_json::json!(["Bash(rm:*)"]));
    }

    fn legacy_hook_settings() -> serde_json::Value {
        serde_json::json!({
            "model": "glm-5.3",
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo '禁止执行 Kubernetes 写操作。'",
                                "timeout": 5000
                            },
                            { "type": "command", "command": "guard.sh" }
                        ]
                    },
                    {
                        "matcher": "WebFetch",
                        "hooks": [{ "type": "command", "command": "audit.sh" }]
                    }
                ],
                "PostToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "log.sh" }] }
                ]
            }
        })
    }

    #[test]
    fn prune_removes_our_hook_keeps_user_hooks() {
        let mut settings = legacy_hook_settings();
        assert!(prune_legacy_k8s_hook(&mut settings));
        let pre = &settings["hooks"]["PreToolUse"];
        // our k8s hook is gone; the user's guard.sh stays in the same group
        assert_eq!(pre[0]["matcher"], "Bash");
        assert_eq!(pre[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], "guard.sh");
        // untouched groups and other events survive
        assert_eq!(pre[1]["hooks"][0]["command"], "audit.sh");
        assert_eq!(settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "log.sh");
        // idempotent
        assert!(!prune_legacy_k8s_hook(&mut settings));
    }

    #[test]
    fn prune_drops_hook_block_when_only_ours_remained() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            { "type": "command", "command": "echo 禁止执行 Kubernetes 写操作。", "timeout": 5000 }
                        ]
                    }
                ]
            }
        });
        assert!(prune_legacy_k8s_hook(&mut settings));
        assert!(settings.get("hooks").is_none());
        assert!(settings.as_object().unwrap().is_empty());
    }

    #[test]
    fn prune_noop_without_hooks() {
        let mut settings = serde_json::json!({ "model": "glm-5.3" });
        assert!(!prune_legacy_k8s_hook(&mut settings));
        assert_eq!(settings["model"], "glm-5.3");
    }

    fn sel(names: &[&str]) -> Option<String> {
        let v: Vec<serde_json::Value> = names
            .iter()
            .map(|s| serde_json::Value::String(s.to_string()))
            .collect();
        select_model_by_priority(Some(&v))
    }

    // Real primary config: pro sits ahead of flash in the saved list.
    #[test]
    fn flash_wins_over_pro_regardless_of_list_order() {
        assert_eq!(
            sel(&[
                "qwen3.7-plus",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
                "gemini-flash-lite-latest",
                "claude-opus-4-8",
                "deepseek-v4-flash-max",
                "deepseek-v4-pro-max",
                "qwen3.6-plus",
            ]),
            Some("deepseek-v4-flash".to_string())
        );
    }

    // Real secondary config: already flash-first.
    #[test]
    fn secondary_config_still_picks_flash() {
        assert_eq!(
            sel(&["deepseek-v4-flash", "deepseek-v4-pro", "qwen3.7-plus"]),
            Some("deepseek-v4-flash".to_string())
        );
    }

    // Cross-family priority must survive: qwen outranks gemini, so a gemini flash
    // must not jump the queue.
    #[test]
    fn family_priority_beats_another_familys_flash() {
        assert_eq!(
            sel(&["gemini-flash-lite-latest", "qwen3.6-plus"]),
            Some("qwen3.6-plus".to_string())
        );
    }

    // No flash in the winning family -> first member of that family.
    #[test]
    fn falls_back_within_family() {
        assert_eq!(
            sel(&["glm-4-air", "kimi-k2"]),
            Some("glm-4-air".to_string())
        );
    }

    // glm outranks deepseek even when deepseek is listed first.
    #[test]
    fn glm_outranks_deepseek() {
        assert_eq!(
            sel(&["deepseek-v4-flash", "glm-4.6", "qwen3.7-plus"]),
            Some("glm-4.6".to_string())
        );
    }

    // glm names its fast tier "glm-<id>-flash"; it must beat the plain glm-<id>
    // whichever order the saved list happens to be in.
    #[test]
    fn glm_flash_outranks_glm_base() {
        assert_eq!(
            sel(&["glm-4.6", "glm-4.6-flash"]),
            Some("glm-4.6-flash".to_string())
        );
        assert_eq!(
            sel(&["glm-4.6-flash", "glm-4.6"]),
            Some("glm-4.6-flash".to_string())
        );
    }

    // Family priority still applies first: a glm flash beats a deepseek flash
    // even when deepseek is listed ahead of it.
    #[test]
    fn glm_flash_outranks_deepseek_flash() {
        assert_eq!(
            sel(&["deepseek-v4-flash", "glm-4.6", "glm-4.6-flash"]),
            Some("glm-4.6-flash".to_string())
        );
    }

    // flashx is the top tier: it beats flash and the base model regardless of
    // where each sits in the saved list.
    #[test]
    fn flashx_outranks_flash_and_base() {
        assert_eq!(
            sel(&["glm-5.3-flash", "glm-5.3", "glm-5.3-flashx"]),
            Some("glm-5.3-flashx".to_string())
        );
        assert_eq!(
            sel(&["glm-5.3-flashx", "glm-5.3-flash", "glm-5.3"]),
            Some("glm-5.3-flashx".to_string())
        );
        assert_eq!(
            sel(&["deepseek-v4-flash-max", "deepseek-v4-flashx", "deepseek-v4-pro"]),
            Some("deepseek-v4-flashx".to_string())
        );
    }

    // flashx is a within-family tier, not a global override: glm's family
    // priority still wins over a deepseek flashx.
    #[test]
    fn family_priority_still_beats_a_lower_familys_flashx() {
        assert_eq!(
            sel(&["deepseek-v4-flashx", "glm-5.3-flash"]),
            Some("glm-5.3-flash".to_string())
        );
    }

    #[test]
    fn empty_and_no_match_fall_back() {
        assert_eq!(select_model_by_priority(Some(&vec![])), None);
        assert_eq!(
            sel(&["totally-unknown-model"]),
            Some("totally-unknown-model".to_string())
        );
    }
}
