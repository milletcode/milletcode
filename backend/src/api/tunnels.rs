//! millet-tunnel REST + WS endpoints.
//!
//! 隧道：用户机器上的 millet-tunnel CLI 经 WS 连入控制台，把用户本机
//! listener 的流量桥到 workspace 端口（与端口转发互补——后者的 listener
//! 在控制台主机上）。

use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::api::forwards::resolve_project;
use crate::runtime::port_forward::ForwardTarget;
use crate::runtime::tunnel::{TunnelError, TunnelManager, handle_tunnel_ws};
use crate::AppState;

fn err_json(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (
        status,
        Json(json!({ "error": msg.to_string() })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateTunnelRequest {
    pub session_key: String,
    pub remote_port: u16,
}

/// 创建隧道（生成 token，返回给前端拼 CLI 命令）。不做连通测试——隧道在
/// CLI 连入时才逐流建到 workspace 的连接，失败经 CLOSE 帧反馈。
pub async fn create_tunnel(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTunnelRequest>,
) -> Response {
    let (project, ctx) = match resolve_project(&state, &req.session_key).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let target = Arc::new(if project.remote_mode == "ssh" {
        ForwardTarget::Ssh(ctx)
    } else {
        ForwardTarget::Local
    });

    let mut mgr = state.tunnels.lock().await;
    match mgr.create(&req.session_key, req.remote_port, target) {
        Ok(created) => (
            StatusCode::CREATED,
            Json(json!({
                "tunnel": created.info,
                "token": created.token,
            })),
        )
            .into_response(),
        // 幂等：隧道已存在则返回原有 token（200），前端重开子行无需再存
        Err(TunnelError::Duplicate) => {
            let id = format!("{}:{}", req.session_key, req.remote_port);
            match mgr.get_with_token(&id) {
                Some((info, token)) => (StatusCode::OK, Json(json!({ "tunnel": info, "token": token }))).into_response(),
                None => err_json(StatusCode::INTERNAL_SERVER_ERROR, "tunnel vanished"),
            }
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub session_key: Option<String>,
}

/// 隧道列表（含 connected 状态；token 绝不返回）。
pub async fn list_tunnels(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    let tunnels = state.tunnels.lock().await.list(q.session_key.as_deref());
    (StatusCode::OK, Json(json!({ "tunnels": tunnels }))).into_response()
}

/// id 形如 `{session_key}:{remote_port}`，含 `/`，路由用通配段匹配。
pub async fn delete_tunnel(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if state.tunnels.lock().await.remove(&id) {
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
    } else {
        err_json(StatusCode::NOT_FOUND, format!("tunnel {id} not found"))
    }
}

#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    pub token: String,
}

/// CLI 连接入口。auth 中间件对 `/api/tunnels/connect` 豁免（WS upgrade 无法
/// 带 Basic auth），query 里的 token 即凭证。
pub async fn connect_tunnel(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ConnectQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let manager: Arc<tokio::sync::Mutex<TunnelManager>> = state.tunnels.clone();
    let token = q.token;
    ws.on_upgrade(move |socket| async move {
        handle_tunnel_ws(&manager, &token, socket).await;
    })
}
