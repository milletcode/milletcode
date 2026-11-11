use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct FilePathQuery {
    /// Target path: empty (project root), absolute, `~/...`, or relative to project root
    pub path: Option<String>,
}

/// List a directory in the session's project workspace
/// GET /api/sessions/{org}/{project}/{agent_name}/{session_name}/files
pub async fn list_files(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Query(query): Query<FilePathQuery>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    match state.session_manager.list_project_dir(&composite_key, query.path).await {
        Ok(listing) => (StatusCode::OK, Json(listing)).into_response(),
        Err(e) => error_response(&e),
    }
}

/// Read a file from the session's project workspace
/// GET /api/sessions/{org}/{project}/{agent_name}/{session_name}/file
pub async fn read_file(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Query(query): Query<FilePathQuery>,
) -> impl IntoResponse {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let path = query.path.unwrap_or_default();
    if path.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "path query parameter is required" })),
        )
            .into_response();
    }
    match state.session_manager.read_project_file(&composite_key, &path).await {
        Ok(content) => (StatusCode::OK, Json(content)).into_response(),
        Err(e) => error_response(&e),
    }
}

/// Download a file from the session's project workspace as an attachment
/// (raw bytes, no preview cap short of the download ceiling).
/// GET /api/sessions/{org}/{project}/{agent_name}/{session_name}/file/download
pub async fn download_file(
    State(state): State<Arc<AppState>>,
    Path((org, project, agent_name, session_name)): Path<(String, String, String, String)>,
    Query(query): Query<FilePathQuery>,
) -> axum::response::Response {
    let composite_key = format!("{}/{}/{}/{}", org, project, agent_name, session_name);
    let path = query.path.unwrap_or_default();
    if path.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "path query parameter is required" })),
        )
            .into_response();
    }
    match state.session_manager.download_project_file(&composite_key, &path).await {
        Ok(bytes) => {
            // RFC 6266: plain filename for common cases + RFC 5987 encoded
            // form for non-ASCII names.
            let raw_name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download");
            let name: String = raw_name
                .chars()
                .filter(|c| !c.is_control() && *c != '"' && *c != '\\' && *c != '/')
                .collect();
            let name = if name.is_empty() { "download".to_string() } else { name };
            let encoded: String = name
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            let disposition = format!("attachment; filename=\"{name}\"; filename*=UTF-8''{encoded}");
            (
                [
                    (axum::http::header::CONTENT_TYPE, "application/octet-stream".to_string()),
                    (axum::http::header::CONTENT_DISPOSITION, disposition),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => error_response(&e),
    }
}

fn error_response(e: &anyhow::Error) -> axum::response::Response {
    let msg = e.to_string();
    if msg.contains("escapes project directory") {
        // 访问越出项目工作区：策略拒绝而不是服务器错误
        (StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "path is outside the project directory" })))
            .into_response()
    } else if msg.contains("not found") || msg.contains("Cannot read directory") || msg.contains("Cannot open") || msg.contains("Cannot stat") {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": msg }))).into_response()
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
