use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{header, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use crate::AppState;

#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub filename: String,
    pub path: String,
    pub category: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteInfo>,
}

#[derive(Debug, Serialize, Clone)]
pub struct RemoteInfo {
    pub hostname: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    pub path: String,
}

/// Categorize file by extension
fn categorize_file(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        // Images
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "ico" => "pics",
        // Documents
        "pdf" | "doc" | "docx" | "txt" | "md" | "rtf" | "odt" | "pages" => "docs",
        // Spreadsheets
        "xls" | "xlsx" | "csv" | "tsv" | "ods" | "numbers" => "excel",
        // Logs
        "log" | "logs" => "log",
        // Everything else
        _ => "other",
    }
}

/// POST /api/uploads
/// Optional query param: project_key=keyword/project/agent
/// When project_key belongs to a remote SSH project, file is forwarded via SSH.
pub async fn upload_file(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<Json<UploadResponse>, (StatusCode, String)> {
    let config = &state.config_manager.config().web.upload;

    // Currently only local storage is supported
    if config.storage_type != "local" {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            format!("Storage type '{}' not yet supported", config.storage_type),
        ));
    }

    // Ensure root upload directory exists
    if let Err(e) = tokio::fs::create_dir_all(&config.local_dir).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create upload directory: {}", e),
        ));
    }

    // Extract keyword from project_key (format: "keyword/project/agent")
    let keyword = query.project_key.as_ref().map(|k| k.split('/').next().unwrap_or(k).to_string());

    tracing::info!(
        project_key = ?query.project_key,
        keyword = ?keyword,
        "Upload request received"
    );

    // Resolve remote config if this is a remote project
    let remote_config = keyword.as_ref().and_then(|kw| {
        let config = state.config_manager.config();
        let discovery = config.project.get_discovery_path(kw)?;
        tracing::info!(
            keyword = %kw,
            remote_mode = %discovery.remote_mode,
            hostname = %discovery.hostname,
            "Resolved discovery path"
        );
        if discovery.remote_mode != "ssh" || discovery.hostname.is_empty() {
            return None;
        }
        let ssh_config_path = discovery.ssh
            .as_ref()
            .map(|s| s.config_path.clone())
            .unwrap_or_else(|| config.ssh.config_path.clone());
        Some((discovery.hostname.clone(), ssh_config_path))
    });

    // Process uploaded file(s) — only first file is accepted per request
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (StatusCode::BAD_REQUEST, format!("Failed to read field: {}", e))
    })? {
        let filename = field.file_name().unwrap_or("unnamed").to_string();
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();

        let data = field.bytes().await.map_err(|e| {
            (StatusCode::BAD_REQUEST, format!("Failed to read file: {}", e))
        })?;

        let size = data.len() as u64;
        let category = categorize_file(&filename);

        // Build storage path: {base_dir}/{category}/{timestamp}_{filename}
        let category_dir = format!("{}/{}", config.local_dir, category);
        if let Err(e) = tokio::fs::create_dir_all(&category_dir).await {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create category directory: {}", e),
            ));
        }

        let timestamp = chrono::Utc::now().timestamp();
        let safe_filename: String = filename
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
            .collect();
        let stored_filename = format!("{}_{}", timestamp, safe_filename);
        let file_path = format!("{}/{}", category_dir, stored_filename);

        // Write local copy (always — used for preview and as SCP source)
        if let Err(e) = tokio::fs::write(&file_path, &data).await {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write file: {}", e),
            ));
        }

        // If remote project, forward file to remote host via SSH pipe
        let remote_info = if let Some((hostname, ssh_config_path)) = remote_config.as_ref() {
            // Remote path uses the same configured upload directory
            let remote_dir = format!("{}/{}", config.local_dir, category);
            let remote_path = format!("{}/{}", remote_dir, stored_filename);

            match forward_to_remote(&file_path, hostname, &remote_dir, &stored_filename, ssh_config_path).await {
                Ok(_) => {
                    tracing::info!(
                        filename = %filename,
                        remote_host = %hostname,
                        remote_path = %remote_path,
                        "File forwarded to remote host"
                    );
                    Some(RemoteInfo {
                        hostname: hostname.clone(),
                        path: remote_path,
                    })
                }
                Err(e) => {
                    // Clean up local copy on failure
                    let _ = tokio::fs::remove_file(&file_path).await;
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to forward to remote host: {}", e),
                    ));
                }
            }
        } else {
            None
        };

        tracing::info!(
            filename = %filename,
            path = %file_path,
            category = %category,
            size = size,
            content_type = %content_type,
            remote = remote_info.is_some(),
            "File uploaded"
        );

        return Ok(Json(UploadResponse {
            filename,
            path: file_path,
            category: category.to_string(),
            size,
            remote: remote_info,
        }));
    }

    Err((StatusCode::BAD_REQUEST, "No file provided".to_string()))
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub project_key: Option<String>,
}

/// Expand `~/` prefix to actual home directory
pub(crate) fn expand_tilde(path: &str) -> String {
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

/// Build SSH args with StrictHostKeyChecking=accept-new
pub(crate) fn build_ssh_args(hostname: &str, ssh_config_path: &str) -> Vec<String> {
    let expanded_config = expand_tilde(ssh_config_path);
    let config_exists = std::path::Path::new(&expanded_config).exists();

    let mut args = vec!["-o".to_string(), "StrictHostKeyChecking=accept-new".to_string()];
    if config_exists {
        args.extend(["-F".to_string(), expanded_config]);
    }
    args.push(hostname.to_string());
    args
}

/// Forward a local file to a remote host via SSH.
/// Creates the remote directory if needed, then pipes file content over SSH.
async fn forward_to_remote(
    local_path: &str,
    hostname: &str,
    remote_dir: &str,
    filename: &str,
    ssh_config_path: &str,
) -> Result<(), String> {
    let ssh_args = build_ssh_args(hostname, ssh_config_path);

    // Expand ~ in remote_dir for mkdir (SSH doesn't expand ~ inside quotes)
    let expanded_dir = if remote_dir.starts_with("~/") {
        format!("$HOME{}", &remote_dir[1..])
    } else if remote_dir == "~" {
        "$HOME".to_string()
    } else {
        remote_dir.to_string()
    };

    // Create remote directory
    let mkdir_cmd = format!("mkdir -p {}", expanded_dir);
    let mkdir_output = tokio::process::Command::new("ssh")
        .args(ssh_args.iter().map(|s| s.as_str()))
        .arg(&mkdir_cmd)
        .output()
        .await
        .map_err(|e| format!("Failed to run ssh mkdir: {}", e))?;

    if !mkdir_output.status.success() {
        return Err(format!(
            "mkdir failed: {}",
            String::from_utf8_lossy(&mkdir_output.stderr)
        ));
    }

    // Pipe file content to remote via cat
    let expanded_remote_path = if remote_dir.starts_with("~/") {
        format!("$HOME{}/{}", &remote_dir[1..], filename)
    } else {
        format!("{}/{}", remote_dir, filename)
    };
    let cat_cmd = format!("cat > {}", expanded_remote_path);

    let file_data = tokio::fs::read(local_path)
        .await
        .map_err(|e| format!("Failed to read local file: {}", e))?;

    let mut child = tokio::process::Command::new("ssh")
        .args(ssh_args.iter().map(|s| s.as_str()))
        .arg(&cat_cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ssh: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(&file_data).await.map_err(|e| {
            let _ = child.kill();
            format!("Failed to write to ssh stdin: {}", e)
        })?;
        // Drop stdin to signal EOF
        drop(stdin);
    }

    let output = child.wait_with_output().await.map_err(|e| {
        format!("Failed to wait for ssh: {}", e)
    })?;

    if !output.status.success() {
        return Err(format!(
            "ssh cat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

/// Create upload route with body size limit from config
pub fn upload_route(state: Arc<AppState>) -> axum::Router<Arc<AppState>> {
    let max_bytes = state.config_manager.config().web.upload.max_size_mb * 1024 * 1024;
    axum::Router::new()
        .route("/api/uploads", axum::routing::post(upload_file))
        .route("/api/uploads/preview", axum::routing::get(preview_file))
        .layer(DefaultBodyLimit::max(max_bytes))
}

/// Guess MIME type from file extension
fn guess_mime_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        // Images
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        // Documents
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" | "tsv" => "text/csv; charset=utf-8",
        "log" | "logs" => "text/plain; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" => "application/json",
        "xml" => "application/xml",
        // Office (binary — typically download, not preview)
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    }
}

/// GET /api/uploads/preview?path=...
///
/// Serves the uploaded file for preview. Path must be inside the configured upload directory
/// to prevent path traversal attacks.
pub async fn preview_file(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PreviewQuery>,
) -> Result<(StatusCode, [(header::HeaderName, String); 2], Body), (StatusCode, String)> {
    let config = &state.config_manager.config().web.upload;

    // Resolve the requested path and the upload root to absolute paths
    let requested = PathBuf::from(&query.path);
    let requested_abs = if requested.is_absolute() {
        requested.clone()
    } else {
        PathBuf::from(&config.local_dir).join(requested)
    };

    let upload_root = PathBuf::from(&config.local_dir);

    // Canonicalize both paths to resolve symlinks and ".."
    let upload_root_canonical = tokio::fs::canonicalize(&upload_root)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Upload dir error: {}", e)))?;

    let requested_canonical = match tokio::fs::canonicalize(&requested_abs).await {
        Ok(p) => p,
        Err(_) => {
            return Err((StatusCode::NOT_FOUND, "File not found".to_string()));
        }
    };

    // Security: ensure the resolved path is inside the upload root
    if !requested_canonical.starts_with(&upload_root_canonical) {
        tracing::warn!(
            requested = %query.path,
            resolved = %requested_canonical.display(),
            "Path traversal attempt blocked"
        );
        return Err((StatusCode::FORBIDDEN, "Access denied".to_string()));
    }

    // Read file content
    let data = tokio::fs::read(&requested_canonical)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Read error: {}", e)))?;

    let mime = guess_mime_type(&requested_canonical.to_string_lossy());
    let filename = requested_canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{}\"", filename),
            ),
        ],
        Body::from(data),
    ))
}
