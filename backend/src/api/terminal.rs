//! Terminal REST + WebSocket API endpoints.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    Json,
};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::terminal::{TerminalOutput, TerminalSessionInfo, SeqAction, seq_action};
use crate::AppState;

fn err_json(status: axum::http::StatusCode, msg: impl std::fmt::Display) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

/// Request body for creating a terminal session
#[derive(Debug, Deserialize)]
pub struct CreateTerminalRequest {
    pub project_id: String,
    /// Initial columns (default 80)
    pub cols: Option<u16>,
    /// Initial rows (default 24)
    pub rows: Option<u16>,
}

/// Request body for injecting text into a terminal
#[derive(Debug, Deserialize)]
pub struct InputTerminalRequest {
    pub token: String,
    pub text: String,
    /// Append `\r` to execute the text
    pub enter: Option<bool>,
}

/// Request body for turning natural language into a command
#[derive(Debug, Deserialize)]
pub struct CommandTerminalRequest {
    pub token: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct ResizeTerminalRequest {
    pub cols: u16,
    pub rows: u16,
}

/// Create a terminal session for a project. SSH projects open a remote shell
/// on the project host — a failed SSH connection fails the request (502);
/// there is deliberately no local fallback.
pub async fn create_terminal(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTerminalRequest>,
) -> impl IntoResponse {
    let cols = req.cols.unwrap_or(80);
    let rows = req.rows.unwrap_or(24);

    let Some(project) = state.project_manager.get_by_id(&req.project_id) else {
        return err_json(axum::http::StatusCode::NOT_FOUND, format!("project {} not found", req.project_id));
    };

    if project.remote_mode == "ssh" {
        // SSH connect with retries can take seconds — keep it OUTSIDE the
        // manager lock so resize/list on other terminals isn't blocked.
        let ctx = state.session_manager.build_runtime_context(Some(&project.path), "");
        let conn = match crate::runtime::ssh::connect_ssh_with_retry(&ctx, 3).await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!(project = %project.id, hostname = %project.hostname, error = %e, "terminal SSH connect failed");
                return err_json(
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("SSH 连接 {} 失败: {}", project.hostname, e),
                );
            }
        };

        let mut manager = state.terminal_manager.lock().await;
        match manager
            .create_remote(conn, &project.hostname, &project.path.filesystem_path(), cols, rows)
            .await
        {
            Ok((terminal_id, token)) => (
                axum::http::StatusCode::CREATED,
                Json(serde_json::json!({
                    "terminal_id": terminal_id,
                    "token": token,
                    "mode": "ssh",
                    "hostname": project.hostname,
                })),
            )
                .into_response(),
            Err(e) => {
                tracing::error!(project = %project.id, hostname = %project.hostname, error = %e, "terminal SSH shell setup failed");
                err_json(
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("SSH 远端 shell 建立失败: {}", e),
                )
            }
        }
    } else {
        let cwd = project.path.filesystem_path();
        let mut manager = state.terminal_manager.lock().await;
        match manager.create_local(&cwd, cols, rows) {
            Ok((terminal_id, token)) => (
                axum::http::StatusCode::CREATED,
                Json(serde_json::json!({
                    "terminal_id": terminal_id,
                    "token": token,
                    "mode": "local",
                    "hostname": serde_json::Value::Null,
                })),
            )
                .into_response(),
            Err(e) => {
                tracing::warn!(project = %project.id, cwd = %cwd, error = %e, "terminal cwd invalid");
                err_json(axum::http::StatusCode::BAD_REQUEST, format!("无效的项目目录: {}", e))
            }
        }
    }
}

/// Inject text into a running terminal (used for LLM-generated commands).
/// REST rather than WS so injection still works during a WS reconnect gap.
pub async fn input_terminal(
    State(state): State<Arc<AppState>>,
    Path(terminal_id): Path<String>,
    Json(req): Json<InputTerminalRequest>,
) -> impl IntoResponse {
    let manager = state.terminal_manager.lock().await;
    if !manager.verify_token(&terminal_id, &req.token) {
        return err_json(axum::http::StatusCode::FORBIDDEN, "invalid terminal token");
    }
    match manager.send_text(&terminal_id, &req.text, req.enter.unwrap_or(false)) {
        Ok(()) => axum::http::StatusCode::OK.into_response(),
        Err(e) => err_json(axum::http::StatusCode::NOT_FOUND, e),
    }
}

/// Natural language → single shell command via the active GPT API.
pub async fn command_terminal(
    State(state): State<Arc<AppState>>,
    Path(terminal_id): Path<String>,
    Json(req): Json<CommandTerminalRequest>,
) -> impl IntoResponse {
    let hint = {
        let manager = state.terminal_manager.lock().await;
        if !manager.verify_token(&terminal_id, &req.token) {
            return err_json(axum::http::StatusCode::FORBIDDEN, "invalid terminal token");
        }
        match manager.session_info(&terminal_id) {
            Some(info) => match &info.mode {
                crate::terminal::TerminalMode::Local => {
                    format!("local terminal, cwd {}", info.cwd)
                }
                crate::terminal::TerminalMode::Ssh { hostname } => {
                    format!("ssh terminal on host {}, directory {}", hostname, info.cwd)
                }
            },
            None => return err_json(axum::http::StatusCode::NOT_FOUND, "terminal session not found"),
        }
    };

    match crate::api::config::nl_to_command(&state, &hint, &req.text).await {
        Ok(raw) => match extract_command(&raw) {
            Ok(command) => (axum::http::StatusCode::OK, Json(serde_json::json!({ "command": command })))
                .into_response(),
            Err(e) => err_json(axum::http::StatusCode::BAD_GATEWAY, e),
        },
        Err(e) => err_json(axum::http::StatusCode::BAD_GATEWAY, e),
    }
}

/// Strip an LLM answer down to one executable line: remove code fences,
/// stray backticks, and newlines. Empty result is an error.
pub(crate) fn extract_command(raw: &str) -> Result<String, String> {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("```") {
        // Drop the language tag on the fence's first line.
        s = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest).trim();
        if let Some(inner) = s.strip_suffix("```") {
            s = inner.trim();
        }
    }
    let joined = s.replace('`', "").split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        Err("model returned an empty command".to_string())
    } else {
        Ok(joined)
    }
}

/// Resize a terminal session
pub async fn resize_terminal(
    State(state): State<Arc<AppState>>,
    Path(terminal_id): Path<String>,
    Json(req): Json<ResizeTerminalRequest>,
) -> impl IntoResponse {
    let manager = state.terminal_manager.lock().await;
    match manager.resize_session(&terminal_id, req.cols, req.rows) {
        Ok(()) => axum::http::StatusCode::OK.into_response(),
        Err(e) => err_json(axum::http::StatusCode::NOT_FOUND, e),
    }
}

/// Close a terminal session
pub async fn close_terminal(
    State(state): State<Arc<AppState>>,
    Path(terminal_id): Path<String>,
) -> impl IntoResponse {
    let mut manager = state.terminal_manager.lock().await;
    match manager.close_session(&terminal_id) {
        Ok(()) => axum::http::StatusCode::OK.into_response(),
        Err(e) => err_json(axum::http::StatusCode::NOT_FOUND, e),
    }
}

/// List all active terminal sessions
pub async fn list_terminals(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let manager = state.terminal_manager.lock().await;
    Json(serde_json::json!({
        "terminals": manager.list_sessions() as Vec<TerminalSessionInfo>,
    }))
}

/// WS endpoint. Auth via subprotocol: the client passes the terminal token
/// as a WebSocket subprotocol, keeping it out of URLs and access logs.
pub async fn terminal_ws_handler(
    State(state): State<Arc<AppState>>,
    ws: WebSocketUpgrade,
    Path(terminal_id): Path<String>,
) -> impl IntoResponse {
    let mut matched: Option<String> = None;
    {
        let manager = state.terminal_manager.lock().await;
        for proto in ws.requested_protocols() {
            if let Ok(p) = proto.to_str() {
                if manager.verify_token(&terminal_id, p) {
                    matched = Some(p.to_string());
                    break;
                }
            }
        }
    }
    let Some(token) = matched else {
        tracing::warn!(terminal_id = %terminal_id, "terminal WS rejected: no valid subprotocol token");
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "invalid terminal token",
        )
            .into_response();
    };

    let mut ws = ws;
    if let Ok(hv) = axum::http::HeaderValue::from_str(&token) {
        ws.set_selected_protocol(hv);
    }

    tracing::debug!(terminal_id = %terminal_id, "terminal WS connecting");
    ws.on_upgrade(move |socket| async move {
        handle_terminal_ws(socket, terminal_id, state).await;
    })
}

/// After a lag/gap, take a fresh snapshot: clear the screen, repaint, and
/// swap in the new broadcast receiver.
async fn resync_from_snapshot(
    ws_tx: &mut SplitSink<WebSocket, Message>,
    state: &Arc<AppState>,
    terminal_id: &str,
) -> Option<(u64, broadcast::Receiver<TerminalOutput>)> {
    let attach = {
        let manager = state.terminal_manager.lock().await;
        manager.attach(terminal_id)?
    };
    let redraw = [b"\x1b[2J\x1b[3J\x1b[H".as_slice(), attach.snapshot.as_slice()].concat();
    ws_tx.send(Message::Binary(redraw.into())).await.ok()?;
    Some((attach.last_seq, attach.output_rx))
}

/// WS main loop: snapshot first, then live events under the seq discipline
/// (forward == last+1, skip <= last, resync on gaps).
async fn handle_terminal_ws(socket: WebSocket, terminal_id: String, state: Arc<AppState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let attach = {
        let manager = state.terminal_manager.lock().await;
        match manager.attach(&terminal_id) {
            Some(a) => a,
            None => {
                tracing::warn!(terminal_id = %terminal_id, "terminal WS attach failed: session gone");
                return;
            }
        }
    };
    let mut last_seq = attach.last_seq;
    let mut output_rx = attach.output_rx;
    let mut exit_rx = attach.exit_rx;

    // Snapshot precedes any live event (attach is atomic w.r.t. producers).
    if ws_tx.send(Message::Binary(attach.snapshot.into())).await.is_err() {
        return;
    }

    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        tokio::select! {
            output = output_rx.recv() => match output {
                Ok(ev) => match seq_action(last_seq, ev.seq) {
                    SeqAction::Forward => {
                        last_seq = ev.seq;
                        if ws_tx.send(Message::Binary(ev.data.into())).await.is_err() {
                            break;
                        }
                    }
                    SeqAction::Skip => {}
                    SeqAction::Resync => {
                        match resync_from_snapshot(&mut ws_tx, &state, &terminal_id).await {
                            Some((seq, rx)) => {
                                last_seq = seq;
                                output_rx = rx;
                            }
                            None => break,
                        }
                    }
                },
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(terminal_id = %terminal_id, lagged = n, "terminal WS lagged, resyncing");
                    match resync_from_snapshot(&mut ws_tx, &state, &terminal_id).await {
                        Some((seq, rx)) => {
                            last_seq = seq;
                            output_rx = rx;
                        }
                        None => break,
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            changed = exit_rx.changed() => {
                if changed.is_err() {
                    break; // session dropped
                }
                // Shell exited — drain in-flight events (the exit banner) in
                // order, then close.
                loop {
                    match output_rx.try_recv() {
                        Ok(ev) if ev.seq == last_seq + 1 => {
                            last_seq = ev.seq;
                            if ws_tx.send(Message::Binary(ev.data.into())).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                break;
            }
            msg = ws_rx.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    let manager = state.terminal_manager.lock().await;
                    if manager.write_input(&terminal_id, &data).is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    let manager = state.terminal_manager.lock().await;
                    if manager.write_input(&terminal_id, text.as_bytes()).is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    if ws_tx.send(Message::Pong(data)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                _ => {}
            },
            _ = heartbeat.tick() => {
                if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }

    let _ = ws_tx.close().await;
    tracing::debug!(terminal_id = %terminal_id, "terminal WS closed");
}

#[cfg(test)]
mod tests {
    use super::extract_command;

    #[test]
    fn extract_command_plain() {
        assert_eq!(extract_command("  docker ps -a \n").unwrap(), "docker ps -a");
    }

    #[test]
    fn extract_command_fenced() {
        assert_eq!(extract_command("```bash\nkubectl get pods\n```").unwrap(), "kubectl get pods");
        assert_eq!(extract_command("```\ngit status\n```").unwrap(), "git status");
    }

    #[test]
    fn extract_command_stray_backticks_and_multiline() {
        assert_eq!(extract_command("`ls -la`").unwrap(), "ls -la");
        // multi-line answers collapse to one line
        assert_eq!(extract_command("cd /tmp\nls -la").unwrap(), "cd /tmp ls -la");
    }

    #[test]
    fn extract_command_empty_is_error() {
        assert!(extract_command("").is_err());
        assert!(extract_command("   \n ").is_err());
        assert!(extract_command("```\n```").is_err());
    }
}
