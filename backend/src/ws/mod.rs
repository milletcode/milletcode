use axum::{
    extract::{
        ws::{WebSocket, Message, WebSocketUpgrade},
        Path, State,
    },
    http::HeaderMap,
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::Arc;
use tracing;

use crate::AppState;
use crate::agents::types::{ApprovalResponse, SessionId};

/// Path key: "org/project/agent-id" (e.g. "smartcode/git-kanban/claude")
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ProjectKey(pub String);

impl ProjectKey {
    pub fn new(org: &str, project: &str, agent_id: &str) -> Self {
        Self(format!("{}/{}/{}", org, project, agent_id))
    }
}

impl std::fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    SendMessage { session_id: String, content: String },
    Approve {
        session_id: String,
        decision: String,
        comment: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
    },
    StopSession { session_id: String },
    InterruptSession { session_id: String },
    RestoreSession { session_id: String },
    DeleteSession { session_id: String },
    SubscribeSession { session_id: String },
    UnsubscribeSession { session_id: String },
    AnswerQuestion {
        session_id: String,
        /// Legacy field, kept so old clients still deserialize; the control
        /// protocol correlates by request_id, not tool_use_id.
        #[serde(default)]
        tool_use_id: String,
        /// Legacy single-answer form; superseded by `answers`.
        #[serde(default)]
        answer: String,
        #[serde(default)]
        request_id: Option<String>,
        /// Full mapping: question text -> chosen option label
        #[serde(default)]
        answers: Option<std::collections::HashMap<String, String>>,
    },
}

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Output {
        session_id: String,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
    },
    ApprovalRequest {
        session_id: String,
        prompt: String,
        options: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_key: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
    },
    SessionStatus { session_id: String, status: String },
    SessionRestarted { session_id: String },
    SessionComplete { session_id: String, exit_code: i32 },
    Error { session_id: String, message: String },
    UserQuestion {
        session_id: String,
        tool_use_id: String,
        request_id: String,
        questions: Vec<crate::agents::types::UserQuestionItem>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_key: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
    },
    ApprovalResolved { session_id: String },
    GlobalNotification {
        level: String,
        message: String,
    },
    /// smartcode 热切换配置已生效（api_key 永不回传）；None = 该项未变化
    ConfigChanged {
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
}

/// New per-project WS handler: /ws/{org}/{project}/{agent_id}
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path((org, project, agent_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let project_key = ProjectKey::new(&org, &project, &agent_id);

    // Remember where the user browses from — the idle notifier composes
    // session URLs from this origin (production domains are dynamic).
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let origin = origin.trim_end_matches('/').to_string();
        state.session_manager.remember_ws_origin(&project_key.0, origin);
    }

    // Enforce max_total_sessions (unique project keys across the system)
    let max_total = state.config_manager.config().project.sessions.max_total_sessions();
    {
        let mut registry = state.ws_registry.lock();
        // Lazy cleanup: drop keys with no live connections
        registry.retain(|_, count| *count > 0);
        if !registry.contains_key(&project_key) && registry.len() >= max_total {
            tracing::warn!(
                key = %project_key,
                total = registry.len(),
                max = max_total,
                "WS: rejected, max total sessions reached"
            );
            return axum::http::StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        *registry.entry(project_key.clone()).or_insert(0) += 1;
    }

    tracing::info!(key = %project_key, "WS: new connection");

    let state_for_ws = state.clone();
    let state_for_cleanup = state.clone();
    let key_for_ws = project_key.clone();
    let key_for_cleanup = project_key.clone();
    ws.on_upgrade(move |socket| async move {
        handle_ws(socket, state_for_ws, key_for_ws).await;
        // Decrement ref count on close
        let mut registry = state_for_cleanup.ws_registry.lock();
        if let Some(count) = registry.get_mut(&key_for_cleanup) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                registry.remove(&key_for_cleanup);
            }
        }
    })
}

/// Check whether a composite key belongs to this connection's project+agent_id scope.
/// Pure string comparison — no DB lookups needed.
fn session_in_scope(composite_key: &str, project_key: &ProjectKey) -> bool {
    let k: Vec<&str> = composite_key.split('/').collect();
    let p: Vec<&str> = project_key.0.split('/').collect();
    // composite_key: org/project/agent-name/session-name
    // project_key: org/project/agent-type
    k.len() >= 3 && p.len() >= 3 && k[0] == p[0] && k[1] == p[1] && k[2] == p[2]
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, project_key: ProjectKey) {
    use std::sync::atomic::Ordering;
    let (mut tx, mut rx) = socket.split();
    let mut subscribed: HashSet<String> = HashSet::new();
    let mut event_rx = state.event_bus.subscribe();

    // Enforce max_subscriptions_per_project
    let max_per_project = state.config_manager.config().project.sessions.max_subscriptions_per_project();

    loop {
        tokio::select! {
            result = event_rx.recv() => {
                match result {
                    Ok(event) => {
                        match event {
                            crate::agents::AgentEvent::GlobalNotification { level, message, .. } => {
                                let server_msg = ServerMessage::GlobalNotification { level, message };
                                if let Ok(json) = serde_json::to_string(&server_msg) {
                                    let _ = tx.send(Message::Text(json.into())).await;
                                }
                            }
                            // Control-protocol events are broadcast to every
                            // connection regardless of subscription: the
                            // message-panel badge must track all sessions.
                            crate::agents::AgentEvent::ApprovalRequest { ref session_id, ref prompt, ref options, ref request_id, ref tool_name, .. } => {
                                let sid = session_id.0.clone();
                                let session_info = state.session_manager.get_session(&crate::agents::types::SessionId(sid.clone()));
                                let (sk, pn, an) = match &session_info {
                                    Some(info) => {
                                        let pname = info.project_path.as_ref()
                                            .and_then(|pid| state.project_manager.get(pid))
                                            .map(|p| p.name.clone());
                                        (Some(sid.clone()), pname, Some(info.agent_name.clone()))
                                    }
                                    None => (None, None, None),
                                };
                                let server_msg = ServerMessage::ApprovalRequest {
                                    session_id: sid.clone(), prompt: prompt.clone(), options: options.clone(),
                                    request_id: request_id.clone(), tool_name: tool_name.clone(),
                                    session_key: sk, project_name: pn, agent_name: an,
                                };
                                if let Ok(json) = serde_json::to_string(&server_msg) {
                                    if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                }
                            }
                            crate::agents::AgentEvent::UserQuestion { ref session_id, ref tool_use_id, ref request_id, ref questions, .. } => {
                                let sid = session_id.0.clone();
                                let session_info = state.session_manager.get_session(&crate::agents::types::SessionId(sid.clone()));
                                let (sk, pn, an) = match &session_info {
                                    Some(info) => {
                                        let pname = info.project_path.as_ref()
                                            .and_then(|pid| state.project_manager.get(pid))
                                            .map(|p| p.name.clone());
                                        (Some(sid.clone()), pname, Some(info.agent_name.clone()))
                                    }
                                    None => (None, None, None),
                                };
                                let server_msg = ServerMessage::UserQuestion {
                                    session_id: sid, tool_use_id: tool_use_id.clone(),
                                    request_id: request_id.clone(), questions: questions.clone(),
                                    session_key: sk, project_name: pn, agent_name: an,
                                };
                                if let Ok(json) = serde_json::to_string(&server_msg) {
                                    if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                }
                            }
                            crate::agents::AgentEvent::ApprovalResolved { ref session_id, .. } => {
                                let server_msg = ServerMessage::ApprovalResolved { session_id: session_id.0.clone() };
                                if let Ok(json) = serde_json::to_string(&server_msg) {
                                    if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                }
                            }
                            ref event => {
                                let sid = match event.session_id() {
                                    Some(s) => s.0.clone(),
                                    None => continue,
                                };
                                // Only forward if this session is in our project scope AND subscribed
                                if !session_in_scope(&sid, &project_key) {
                                    tracing::debug!(
                                        event_sid = %sid,
                                        key = %project_key,
                                        "WS: event dropped - session not in scope"
                                    );
                                    continue;
                                }
                                let is_subscribed = subscribed.contains(&sid);
                                if !is_subscribed {
                                    tracing::debug!(
                                        event_sid = %sid,
                                        subscribed_count = subscribed.len(),
                                        "WS: event dropped - session not subscribed"
                                    );
                                    continue;
                                }
                                match event {
                                    crate::agents::AgentEvent::Output { content, raw, .. } => {
                                        tracing::info!(session = %sid, content_len = content.len(), "WS: forwarding output to client");
                                        let server_msg = ServerMessage::Output { session_id: sid.clone(), text: content.clone(), raw: raw.clone() };
                                        if let Ok(json) = serde_json::to_string(&server_msg) {
                                            if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                        }
                                    }
                                    crate::agents::AgentEvent::SessionComplete { exit_code, .. } => {
                                        let server_msg = ServerMessage::SessionComplete { session_id: sid.clone(), exit_code: *exit_code };
                                        if let Ok(json) = serde_json::to_string(&server_msg) {
                                            if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                        }
                                    }
                                    crate::agents::AgentEvent::SessionStatus { status, .. } => {
                                        let status_str = serde_json::to_value(&status)
                                            .ok()
                                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                                            .unwrap_or_else(|| format!("{:?}", status));
                                        if status_str == "restarting" {
                                            let _ = tx.send(Message::Text(
                                                serde_json::to_string(&ServerMessage::SessionRestarted { session_id: sid.clone() }).unwrap_or_default().into()
                                            )).await;
                                        }
                                        let server_msg = ServerMessage::SessionStatus { session_id: sid.clone(), status: status_str };
                                        if let Ok(json) = serde_json::to_string(&server_msg) {
                                            if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                        }
                                    }
                                    crate::agents::AgentEvent::Error { message, .. } => {
                                        // 错误文案保密改写：适配器各自 emit 的 Error 都汇聚到这里，
                                        // 在转发出口统一抹掉 ssh/22 字样
                                        let server_msg = ServerMessage::Error { session_id: sid.clone(), message: crate::sanitize::sanitize_public_error(message) };
                                        if let Ok(json) = serde_json::to_string(&server_msg) {
                                            if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                        }
                                    }
                                    crate::agents::AgentEvent::ApprovalResolved { .. } => {
                                        let server_msg = ServerMessage::ApprovalResolved { session_id: sid.clone() };
                                        if let Ok(json) = serde_json::to_string(&server_msg) {
                                            if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                        }
                                    }
                                    crate::agents::AgentEvent::ConfigChanged { model, provider, base_url, .. } => {
                                        let server_msg = ServerMessage::ConfigChanged {
                                            session_id: sid.clone(),
                                            model: model.clone(),
                                            provider: provider.clone(),
                                            base_url: base_url.clone(),
                                        };
                                        if let Ok(json) = serde_json::to_string(&server_msg) {
                                            if tx.send(Message::Text(json.into())).await.is_err() { break; }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(key = %project_key, "WebSocket client lagged, dropped {} events", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
            msg = rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                            match client_msg {
                                ClientMessage::SendMessage { session_id, content } => {
                                    let sid = SessionId(session_id.clone());
                                    tracing::info!(
                                        session = %sid,
                                        content_preview = content.chars().take(120).collect::<String>(),
                                        "WS: user message received"
                                    );
                                    if let Err(e) = state.session_manager.send_message(&sid, &content).await {
                                        tracing::error!(session = %sid, "Failed to send message: {}", e);
                                    }
                                }
                                ClientMessage::Approve { session_id, decision, ref comment, request_id } => {
                                    let sid = SessionId(session_id.clone());
                                    tracing::info!(
                                        session = %sid,
                                        decision = %decision,
                                        request_id = ?request_id,
                                        comment = ?comment,
                                        "WS: approval decision received"
                                    );
                                    let response = ApprovalResponse { selected: decision, comment: comment.clone() };
                                    if let Err(e) = state.session_manager.handle_approval(&sid, request_id.as_deref(), response).await {
                                        tracing::error!(session = %sid, "Failed to handle approval: {}", e);
                                    }
                                }
                                ClientMessage::StopSession { session_id } => {
                                    let sid = SessionId(session_id.clone());
                                    tracing::info!(session = %sid, "WS: stop session requested");
                                    if let Err(e) = state.session_manager.stop_session(&sid, false).await {
                                        tracing::error!(session = %sid, "Failed to stop session: {}", e);
                                    }
                                }
                                ClientMessage::InterruptSession { session_id } => {
                                    let sid = SessionId(session_id.clone());
                                    tracing::info!(session = %sid, "WS: interrupt session requested");
                                    if let Err(e) = state.session_manager.interrupt_session(&sid).await {
                                        tracing::error!(session = %sid, "Failed to interrupt session: {}", e);
                                    }
                                }
                                ClientMessage::RestoreSession { session_id } => {
                                    let sid = SessionId(session_id.clone());
                                    tracing::info!(session = %sid, "WS: restore session requested");
                                    if let Err(e) = state.session_manager.restore_session(&sid).await {
                                        tracing::error!(session = %sid, "Failed to restore session: {}", e);
                                    }
                                }
                                ClientMessage::DeleteSession { session_id } => {
                                    let sid = SessionId(session_id.clone());
                                    tracing::info!(session = %sid, "WS: full delete requested");
                                    if let Err(e) = state.session_manager.full_delete_session(&sid.0).await {
                                        tracing::error!(session = %sid, "Failed to full delete session: {}", e);
                                    }
                                }
                                ClientMessage::SubscribeSession { session_id: composite_key } => {
                                    // Enforce per-project subscription limit
                                    if let Some(max) = max_per_project {
                                        if !subscribed.contains(&composite_key) && subscribed.len() >= max {
                                            tracing::warn!(
                                                key = %project_key,
                                                composite_key = %composite_key,
                                                max = max,
                                                "WS: subscribe rejected, max subscriptions reached"
                                            );
                                            continue;
                                        }
                                    }
                                    tracing::info!(
                                        key = %project_key,
                                        composite_key = %composite_key,
                                        "WS: subscribe"
                                    );
                                    if subscribed.insert(composite_key) {
                                        state.ws_subscription_count.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                ClientMessage::AnswerQuestion { session_id, request_id, answers, .. } => {
                                    let sid = SessionId(session_id.clone());
                                    match (request_id, answers) {
                                        (Some(request_id), Some(answers)) => {
                                            tracing::info!(
                                                session = %sid,
                                                request_id = %request_id,
                                                answers = answers.len(),
                                                "WS: question answer received"
                                            );
                                            if let Err(e) = state.session_manager.answer_question(&sid, &request_id, &answers).await {
                                                tracing::error!(session = %sid, "Failed to answer question: {}", e);
                                            }
                                        }
                                        _ => {
                                            tracing::warn!(
                                                session = %sid,
                                                "WS: answer_question missing request_id/answers — ignored (control protocol required)"
                                            );
                                        }
                                    }
                                }
                                ClientMessage::UnsubscribeSession { session_id: composite_key } => {
                                    tracing::info!(key = %project_key, composite_key = %composite_key, "WS: unsubscribe");
                                    if subscribed.remove(&composite_key) {
                                        state.ws_subscription_count.fetch_sub(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    tracing::debug!(key = %project_key, "WebSocket connection closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    // 原集成测试（tests/ws_integration_test.rs，引用了不存在的 lib target，
    // 从未编译通过）迁移而来：覆盖 ProjectKey 的构造/展示/判等。
    #[test]
    fn project_key_joins_with_slash() {
        let key = ProjectKey::new("smartcode", "git-kanban", "claude-code");
        assert_eq!(key.0, "smartcode/git-kanban/claude-code");
    }

    #[test]
    fn project_key_display() {
        let key = ProjectKey::new("org", "project", "agent");
        assert_eq!(format!("{}", key), "org/project/agent");
    }

    #[test]
    fn project_key_equality() {
        let key1 = ProjectKey::new("org", "project", "agent");
        let key2 = ProjectKey::new("org", "project", "agent");
        let key3 = ProjectKey::new("org", "project", "different-agent");
        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }
}
