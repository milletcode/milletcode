//! Port forwarding REST endpoints.
//!
//! 端口转发：workspace（本地或 ssh 远端）监听的 TCP 端口桥接到控制台机器
//! 的 listener，浏览器经 `http://<console-host>:<local_port>` 访问。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::runtime::port_forward::{
    self, ForwardError, ForwardTarget, PortForwardManager, SSH_OP_TIMEOUT,
};
use crate::runtime::ssh::connect_ssh_with_retry;
use crate::AppState;

fn err_json(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (
        status,
        Json(json!({ "error": msg.to_string() })),
    )
        .into_response()
}

fn err_json_code(status: StatusCode, code: &str, msg: impl std::fmt::Display) -> Response {
    (
        status,
        Json(json!({ "error": msg.to_string(), "code": code })),
    )
        .into_response()
}

/// 会话复合键 → (Project, RuntimeContext)。转发的目标主机、模式、ssh 配置
/// 全部由会话所属项目决定。隧道 API（tunnels.rs）复用。
pub(crate) async fn resolve_project(
    state: &Arc<AppState>,
    session_key: &str,
) -> Result<(crate::projects::types::Project, crate::agents::RuntimeContext), Response> {
    let Some(record) = state
        .session_manager
        .get_session_by_key(session_key)
        .ok()
        .flatten()
    else {
        return Err(err_json(StatusCode::NOT_FOUND, format!("session {session_key} not found")));
    };
    let Some(project_id) = record.project_id.filter(|id| !id.is_empty()) else {
        return Err(err_json(
            StatusCode::NOT_FOUND,
            format!("session {session_key} has no project"),
        ));
    };
    let Some(project) = state.project_manager.get_by_id(&project_id) else {
        return Err(err_json(StatusCode::NOT_FOUND, format!("project {project_id} not found")));
    };
    let ctx = state.session_manager.build_runtime_context(Some(&project.path), "");
    Ok((project, ctx))
}

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub session_key: String,
}

/// 扫描 workspace 所有监听 TCP 端口（ssh 模式经 ssh shell-out，本地模式
/// lsof/ss/netstat 依次兜底）。
pub async fn scan(State(state): State<Arc<AppState>>, Json(req): Json<ScanRequest>) -> Response {
    let (project, ctx) = match resolve_project(&state, &req.session_key).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if project.remote_mode == "ssh" {
        let hostname = ctx.hostname.clone();
        let config_path = ctx.ssh_config_path.clone();
        let result = tokio::time::timeout(
            SSH_OP_TIMEOUT,
            tokio::task::spawn_blocking(move || port_forward::scan_remote_blocking(&hostname, &config_path)),
        )
        .await;
        let ports = match result {
            Ok(Ok(Ok(ports))) => ports,
            Ok(Ok(Err(e))) => {
                return err_json(
                    StatusCode::BAD_GATEWAY,
                    format!("扫描 {} 失败: {}", project.hostname, e),
                )
            }
            Ok(Err(e)) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("scan task failed: {e}")),
            Err(_) => {
                return err_json(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("扫描 {} 超时（{}s）", project.hostname, SSH_OP_TIMEOUT.as_secs()),
                )
            }
        };
        (
            StatusCode::OK,
            Json(json!({ "mode": "ssh", "hostname": project.hostname, "ports": ports })),
        )
            .into_response()
    } else {
        let ports = match tokio::task::spawn_blocking(port_forward::scan_local_blocking).await {
            Ok(Ok(ports)) => ports,
            Ok(Err(e)) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("本地扫描失败: {e}")),
            Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, format!("scan task failed: {e}")),
        };
        (
            StatusCode::OK,
            Json(json!({ "mode": "local", "hostname": "", "ports": ports })),
        )
            .into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct TestRequest {
    pub session_key: String,
    pub remote_port: u16,
    pub local_port: u16,
    pub bind_lan: bool,
}

/// 连通测试：本地端口可绑定 + 远端端口可连通，两者都通过才能转发。
/// 始终返回 200，结果用 ok/reason 表达（前端按 reason 做 i18n）。
pub async fn test_forward(State(state): State<Arc<AppState>>, Json(req): Json<TestRequest>) -> Response {
    let (project, ctx) = match resolve_project(&state, &req.session_key).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // 1) 本地端口占用检查：严格 bind（关 SO_REUSEADDR，见 bind_strict 注释），
    //    成功即释放（真正转发时重新 bind）
    let bind_addr = if req.bind_lan { "0.0.0.0" } else { "127.0.0.1" };
    if let Err(e) = port_forward::bind_strict(bind_addr, req.local_port).await {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "reason": "local_port_busy",
                "detail": e.to_string(),
            })),
        )
            .into_response();
    }

    // 2) 远端端口连通性：ssh 模式开 direct-tcpip channel 即关；本地直连
    let remote_ok = if project.remote_mode == "ssh" {
        let check = tokio::time::timeout(SSH_OP_TIMEOUT, async {
            let conn = connect_ssh_with_retry(&ctx, 3).await?;
            port_forward::open_direct(&conn, req.remote_port).await
        })
        .await;
        matches!(check, Ok(Ok(_)))
    } else {
        tokio::net::TcpStream::connect(("127.0.0.1", req.remote_port))
            .await
            .is_ok()
    };

    if remote_ok {
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
    } else {
        (
            StatusCode::OK,
            Json(json!({ "ok": false, "reason": "remote_not_listening" })),
        )
            .into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateForwardRequest {
    pub session_key: String,
    pub remote_port: u16,
    pub local_port: u16,
    pub bind_lan: bool,
}

pub async fn create_forward(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateForwardRequest>,
) -> Response {
    let (project, ctx) = match resolve_project(&state, &req.session_key).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let target = if project.remote_mode == "ssh" {
        ForwardTarget::Ssh(ctx)
    } else {
        ForwardTarget::Local
    };

    match PortForwardManager::add(
        &state.port_forwards,
        &req.session_key,
        &project.hostname,
        req.remote_port,
        req.local_port,
        req.bind_lan,
        target,
    )
    .await
    {
        Ok(entry) => (
            StatusCode::CREATED,
            Json(json!({ "forward": entry })),
        )
            .into_response(),
        Err(e @ ForwardError::Duplicate) => err_json_code(StatusCode::CONFLICT, "duplicate", e),
        Err(e @ ForwardError::LocalPortBusy(_)) => {
            err_json_code(StatusCode::CONFLICT, "local_port_busy", e)
        }
        Err(e @ ForwardError::RemoteUnreachable(_)) => {
            err_json_code(StatusCode::BAD_GATEWAY, "remote_unreachable", e)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub session_key: Option<String>,
}

pub async fn list_forwards(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    let forwards = state.port_forwards.lock().await.list(q.session_key.as_deref());
    (StatusCode::OK, Json(json!({ "forwards": forwards }))).into_response()
}

/// id 形如 `org/project/agent/name:port`，含 `/`，路由用通配段匹配。
pub async fn delete_forward(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if PortForwardManager::remove(&state.port_forwards, &id).await {
        (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
    } else {
        err_json(StatusCode::NOT_FOUND, format!("forward {id} not found"))
    }
}
