use axum::{Json, extract::Path, extract::Query, extract::State, response::IntoResponse, http::StatusCode};
use serde::Deserialize;
use std::sync::Arc;

use crate::AppState;
use crate::agents::types::{ApprovalResponse, SessionId};

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub project_id: Option<String>,
    /// Agent instance key (e.g. "claude", "codex")
    pub agent_id: String,
    pub prompt: String,
    /// User-defined session name (e.g. "default", "auth-fix"). Defaults to "default".
    pub session_name: Option<String>,
    /// If true, stop and delete existing session with the same key before creating.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct IdleNotifyRequest {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct ApproveRequest {
    pub selected: String,
    pub comment: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnswerQuestionRequest {
    pub request_id: String,
    pub answers: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// If provided, return a single session by composite key instead of listing all.
    pub key: Option<String>,
}

pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    // If ?key= is provided, return single session by composite key
    if let Some(key) = query.key {
        if let Ok(Some(record)) = state.session_manager.get_session_by_key(&key) {
            return (StatusCode::OK, Json(record)).into_response();
        }
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "session not found" }))).into_response();
    }

    Json(state.session_manager.list_all_sessions()).into_response()
}

/// GET /api/sessions/idle — sessions whose agent process is alive but has had
/// no activity beyond the configured threshold. Composite ids only; the
/// global switch lives in `[project.sessions] idle_reminder_enabled`.
pub async fn list_idle_sessions(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let cfg = state.config_manager.config();
    let enabled = cfg.project.sessions.idle_reminder_enabled;
    let idles = state.session_manager.idle_sessions();
    Json(serde_json::json!({ "enabled": enabled, "idles": idles }))
}

pub async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    let agent_id = req.agent_id.clone();
    // Look up project by id (format: "keyword/name") to get the correct ProjectPath
    let project_path = req.project_id.as_ref()
        .and_then(|pid| state.project_manager.get_by_id(pid))
        .map(|p| p.path);

    // Build session name: user-provided or default to "default"
    let session_name = req.session_name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string());

    // Build composite key
    let config = state.config_manager.config();
    let agent_config = config.agent.find_enabled(&agent_id);
    let agent_name = agent_config.map(|(_, c)| c.name.clone()).unwrap_or_else(|| "unknown".to_string());
    // Get keyword from project
    let keyword = req.project_id.as_ref()
        .and_then(|pid| state.project_manager.get_by_id(pid))
        .map(|p| p.keyword.clone())
        .unwrap_or_else(|| "default".to_string());
    let session_key = crate::db::SessionStore::make_key(
        &keyword,
        req.project_id.as_deref(),
        &agent_id,
        &session_name,
    );

    // Check if session already exists at this key
    if let Ok(Some(existing)) = state.session_manager.get_session_by_key(&session_key) {
        if !req.overwrite {
            // Session exists — return conflict so frontend can ask user
            return (StatusCode::CONFLICT, Json(serde_json::json!({
                "exists": true,
                "session_key": session_key,
                "existing_session_id": existing.session_id,
                "status": existing.status,
                "prompt": existing.prompt,
            }))).into_response();
        }

        // Overwrite: stop existing session (do NOT delete — keep for local audit/restore)
        // The new create_session below will overwrite the same composite key in redb.
        let _ = state.session_manager.stop_session(&SessionId(existing.session_id.clone()), false).await;
    }

    let result = state.session_manager.create_session(project_path, agent_id.clone(), &req.prompt, &session_name).await;

    match result {
        Ok((session_id, key)) => {
            (StatusCode::CREATED, Json(serde_json::json!({
                "session_id": session_id.0,
                "session_key": key,
                "agent_name": agent_name
            }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// Get session by composite key — GET /api/sessions/{org}/{project}/{agent_name}/{session_name}
pub async fn get_session(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
) -> impl IntoResponse {
    // Build composite key from path parameters
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key.clone());

    // Deep links rely on this response for project resolution; the in-memory
    // info variant lacks project_id, so inject "keyword/name" from the key.
    let mut body = if let Some(info) = state.session_manager.get_session(&session_id) {
        serde_json::to_value(&info).ok()
    } else {
        state
            .session_manager
            .get_session_by_key(&composite_key)
            .ok()
            .flatten()
            .and_then(|record| serde_json::to_value(&record).ok())
    };

    match body {
        Some(mut value) => {
            if value.get("project_id").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                value["project_id"] = serde_json::json!(format!("{}/{}", org, project));
            }
            // The in-memory SessionInfo variant lacks persistence-only fields —
            // always overlay the bell flag from the store so the frontend sees
            // the truth regardless of which variant answered.
            if let Ok(Some(record)) = state.session_manager.get_session_by_key(&composite_key) {
                value["idle_notify_enabled"] = serde_json::json!(record.idle_notify_enabled);
                value["last_error"] = serde_json::json!(record.last_error);
            }
            (StatusCode::OK, Json(value)).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "session not found" }))).into_response(),
    }
}

/// Toggle the idle-notification bell — POST .../idle-notify {enabled: bool}
/// Persists to redb; the backend scanner reads it every tick.
pub async fn set_idle_notify(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Json(req): Json<IdleNotifyRequest>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    match state.session_manager.set_idle_notify_enabled(&composite_key, req.enabled) {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "idle_notify_enabled": req.enabled })),
        )
            .into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "session not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct StopQuery {
    pub force: Option<bool>,
}

/// Hot-switch runtime config — POST .../config {model?, base_url?, api_key?, provider?}
/// smartcode console only; success = stdin accepted, confirmed later by the
/// config_changed WS event.
#[derive(Deserialize)]
pub struct SetConfigRequest {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub provider: Option<String>,
}

pub async fn set_session_config(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Json(req): Json<SetConfigRequest>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key);
    match state
        .session_manager
        .set_session_config(
            &session_id,
            req.model.as_deref(),
            req.base_url.as_deref(),
            req.api_key.as_deref(),
            req.provider.as_deref(),
        )
        .await
    {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "sent" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn stop_session(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Query(query): Query<StopQuery>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key);
    let force = query.force.unwrap_or(false);
    match state.session_manager.stop_session(&session_id, force).await {
        Ok(claude_session_id) => {
            let mut response = serde_json::json!({ "status": "stopped" });
            if let Some(uuid) = claude_session_id {
                response["claude_session_id"] = serde_json::json!(uuid);
            }
            (StatusCode::ACCEPTED, Json(response)).into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// Restore a stopped/errored session — POST /api/sessions/{org}/{project}/{agent_name}/{session_name}/restore
/// Restarts the agent process for an existing session.
pub async fn restore_session(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key.clone());
    match state.session_manager.restore_session(&session_id).await {
        Ok(()) => {
            // Get updated session info to return current status
            if let Some(info) = state.session_manager.get_session(&session_id) {
                (StatusCode::OK, Json(serde_json::json!({
                    "session_id": composite_key,
                    "status": info.status,
                }))).into_response()
            } else {
                (StatusCode::OK, Json(serde_json::json!({
                    "session_id": composite_key,
                    "status": "Starting",
                }))).into_response()
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn send_message(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Json(req): Json<SendMessageRequest>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key);
    match state.session_manager.send_message(&session_id, &req.content).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "sent" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn handle_approval(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Json(req): Json<ApproveRequest>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key);
    let response = ApprovalResponse { selected: req.selected, comment: req.comment };
    match state.session_manager.handle_approval(&session_id, req.request_id.as_deref(), response).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "approved" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn answer_question(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Json(req): Json<AnswerQuestionRequest>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key);
    match state.session_manager.answer_question(&session_id, &req.request_id, &req.answers).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "answered" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let session_id = SessionId(composite_key.clone());
    let _ = state.session_manager.stop_session(&session_id, false).await;

    if state.session_manager.delete_session(&composite_key) {
        (StatusCode::OK, Json(serde_json::json!({ "status": "deleted" }))).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "session not found" }))).into_response()
    }
}

/// Full delete — stops session, deletes redb record, and removes local agent session data
/// (e.g. ~/.claude/sessions/{uuid}/ for Claude Code).
pub async fn full_delete_session(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    match state.session_manager.full_delete_session(&composite_key).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "deleted" }))).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    /// Number of messages from the end to skip (default 0)
    pub offset: Option<usize>,
    /// Max messages to return (default 50)
    pub limit: Option<usize>,
}

/// Read conversation history from Claude's JSONL file — GET /api/sessions/{org}/{project}/{agent_name}/{session_name}/history
/// id is the composite key; we look up claude_session_id from redb for the JSONL path.
pub async fn get_session_history(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Query(query): Query<HistoryQuery>,
) -> impl IntoResponse {
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(50);

    // Build composite key from path parameters
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);

    // Look up session record by composite key to get project_id.
    // project_id is in "keyword/name" format — resolve it to a real ProjectPath
    // via project_manager.get_by_id() so read_session_history gets the filesystem path.
    let project_path = state.session_manager.resolve_project_path(&composite_key);

    match state.session_manager.read_session_history(&composite_key, project_path.as_ref(), offset, limit).await {
        Ok(history) => (StatusCode::OK, Json(history)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}
