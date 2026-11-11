//! /p/ 反代 HTTP 入口 + 签名链接签发。
//!
//! 路由 `/p/{org}/{project}/{agent}/{session}/{port}[/{rest}]`：session_key
//! 固定 4 段。认证走两道：auth 中间件先验 `?_pwt=` 签名（浏览器新标签页
//! 带不上 Basic 头），签名不过的落回 Basic；本 handler 只管转发。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{OriginalUri, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::api::forwards::resolve_project;
use crate::runtime::port_forward::{connect_forward_target, ForwardStream, ForwardTarget};
use crate::runtime::web_proxy;
use crate::AppState;

fn err_json(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (
        status,
        Json(json!({ "error": msg.to_string() })),
    )
        .into_response()
}

/// 取一条到 workspace 端口的上游连接。Ssh 模式复用 per-session 缓存的
/// 连接（connect_forward_target 内部自愈重连并写回）。
async fn acquire_upstream(
    state: &Arc<AppState>,
    session_key: &str,
    port: u16,
    target: &ForwardTarget,
) -> anyhow::Result<ForwardStream> {
    match target {
        ForwardTarget::Local => connect_forward_target(target, port, &mut None).await,
        ForwardTarget::Ssh(_) => {
            let cached = state.proxy_ssh.lock().await.get(session_key).cloned();
            let mut ssh = cached;
            let stream = connect_forward_target(target, port, &mut ssh).await?;
            if let Some(h) = ssh {
                state
                    .proxy_ssh
                    .lock()
                    .await
                    .insert(session_key.to_string(), h);
            }
            Ok(stream)
        }
    }
}

/// `/p/{org}/{project}/{agent}/{session}/{port}[/...]` — 三条路由（精确 /
/// 尾斜杠 / 通配）共用本 handler；尾斜杠是签发链接的标准形态（上游相对
/// 路径资源据此解析）。不使用 Path 提取器：三条路由的捕获段数不同，
/// 统一从 OriginalUri 解析（auth 中间件用的是同一套解析）。
pub async fn proxy_entry(
    State(state): State<Arc<AppState>>,
    OriginalUri(original): OriginalUri,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    req: Request,
) -> Response {
    let Some((session_key, port)) = web_proxy::parse_proxy_path(original.path()) else {
        return err_json(StatusCode::NOT_FOUND, "invalid proxy path");
    };
    let (proj, ctx) = match resolve_project(&state, &session_key).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let target = if proj.remote_mode == "ssh" {
        ForwardTarget::Ssh(ctx)
    } else {
        ForwardTarget::Local
    };

    let prefix = format!("/p/{session_key}/{port}");
    let rest_path = {
        let stripped = original.path().strip_prefix(&prefix).unwrap_or("");
        let mut s = stripped.to_string();
        if !s.is_empty() && !s.starts_with('/') {
            s.insert(0, '/');
        }
        if let Some(q) = original.query() {
            s.push('?');
            s.push_str(q);
        }
        s
    };

    let stream = match acquire_upstream(&state, &session_key, port, &target).await {
        Ok(s) => s,
        Err(e) => {
            return err_json(
                StatusCode::BAD_GATEWAY,
                format!("upstream {session_key}:{port} unreachable: {e:#}"),
            )
        }
    };
    web_proxy::handle_proxy(stream, rest_path, port, ws, req).await
}

#[derive(Debug, Deserialize)]
pub struct ProxyLinkRequest {
    pub session_key: String,
    pub remote_port: u16,
}

/// `POST /api/forwards/proxy-link` — 签发带 `_pwt` 签名的 /p/ 链接（24h
/// 有效，绑定该 session+port）。
pub async fn proxy_link(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ProxyLinkRequest>,
) -> Response {
    if let Err(resp) = resolve_project(&state, &req.session_key).await.map(|_| ()) {
        return resp;
    }
    let secret = web_proxy::proxy_secret(&state.settings_store);
    let exp = chrono::Utc::now().timestamp() + web_proxy::PROXY_TOKEN_TTL_SECS;
    let token = web_proxy::sign_proxy_token(&secret, &req.session_key, req.remote_port, exp);
    // 尾斜杠形态：上游相对路径资源基于当前 URL 解析，少了它就会指到
    // 控制台根路径
    let url = format!(
        "/p/{}/{}/?_pwt={}",
        req.session_key, req.remote_port, token
    );
    (StatusCode::OK, Json(json!({ "url": url }))).into_response()
}

/// AppState.proxy_ssh 的类型别名（main.rs 构造用）
pub type ProxySshCache = HashMap<String, crate::runtime::ssh::SharedSshHandle>;
