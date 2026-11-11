use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use crate::AppState;
use crate::projects::discovery::build_ssh_args;
use crate::projects::templates;
use crate::projects::types::ProjectPath;

#[derive(Serialize)]
struct GitStatus {
    dirty: bool,
    branch: String,
}

#[derive(Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub app_type: String,
    pub keyword: String,
    pub description: Option<String>,
    pub backend_lang: Option<String>,
    pub frontend_lang: Option<String>,
    pub style: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,  // backward compatibility
}

#[derive(Deserialize)]
pub struct CloneProjectRequest {
    pub repo_url: String,
    pub keyword: String,
    #[serde(default = "default_true")]
    pub clone_tags: bool,
    pub branch: Option<String>,
    pub depth: Option<u32>,
    /// HTTPS clone credentials — consumed via env-fed credential helper,
    /// never persisted, never placed in argv.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Credential helper fed by env vars. The `!` prefix makes git run it via
/// `sh -c "<snippet> get"`, so the secrets stay in the environment (invisible
/// to `ps`) and only the key=value lines git expects land on stdout.
pub(crate) const CRED_HELPER: &str = "!f() { test -n \"$GIT_CLONE_USER\" && printf 'username=%s\\n' \"$GIT_CLONE_USER\"; test -n \"$GIT_CLONE_PASS\" && printf 'password=%s\\n' \"$GIT_CLONE_PASS\"; }; f";

/// HTTPS credentials are only meaningful for http(s) URLs, and must never be
/// able to inject extra lines into the credential protocol (key=value per line).
pub(crate) fn validate_clone_creds(
    repo_url: &str,
    username: &Option<String>,
    password: &Option<String>,
) -> Result<(), String> {
    let any = username.is_some() || password.is_some();
    if !any {
        return Ok(());
    }
    let lower = repo_url.to_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(
            "HTTPS credentials require an http(s):// repository URL".to_string(),
        );
    }
    for v in [username.as_deref(), password.as_deref()].into_iter().flatten() {
        if v.contains('\n') || v.contains('\r') || v.contains('\0') {
            return Err("credentials must not contain newline or null characters".to_string());
        }
    }
    Ok(())
}

pub async fn list_projects(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let projects = state.project_manager.list();
    (StatusCode::OK, Json(projects))
}

#[derive(Serialize)]
pub struct DiscoveryPathInfo {
    pub name: String,
    pub path: String,
    pub hostname: String,
    pub remote_mode: String,
    pub ssh_config_path: String,
    pub is_mobile_home: bool,
}

/// Return all discovery paths with their resolved full paths
pub async fn list_discovery_paths(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let resolved = state.config_manager.resolve_project_discovery_paths();
    let paths: Vec<DiscoveryPathInfo> = resolved
        .into_iter()
        .map(|(keyword, (path, hostname, remote_mode, ssh_config_path, is_mobile_home))| DiscoveryPathInfo {
            name: keyword,
            path: path.to_string_lossy().to_string(),
            hostname,
            remote_mode,
            ssh_config_path,
            is_mobile_home,
        })
        .collect();
    (StatusCode::OK, Json(paths))
}

pub async fn get_project(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    match state.project_manager.get_by_id(&id) {
        Some(project) => (StatusCode::OK, Json(project)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "project not found" }))).into_response(),
    }
}

pub async fn refresh_projects(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.project_manager.refresh();
    let projects = state.project_manager.list();
    (StatusCode::OK, Json(projects))
}

pub async fn list_project_sessions(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "project not found" }))).into_response(),
    };
    // Use redb-based listing to include historical sessions, not just in-memory active ones
    let sessions = state.session_manager.list_all_sessions_by_project_id(&project.id);
    (StatusCode::OK, Json(sessions)).into_response()
}

pub async fn git_status(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "project not found" }))).into_response(),
    };

    let fs_path = project.path.filesystem_path();
    let branch = Command::new("git")
        .args(["-C", &fs_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let dirty = Command::new("git")
        .args(["-C", &fs_path, "status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    (StatusCode::OK, Json(GitStatus { dirty, branch })).into_response()
}

pub async fn create_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateProjectRequest>,
) -> impl IntoResponse {
    let config = state.config_manager.config();
    let mobile_roots = config.project.mobile_discovery_roots();

    // Resolve discovery paths (expand ~ to home dir for local, keep ~ for remote)
    let resolved_paths = state.config_manager.resolve_project_discovery_paths();

    // Find the base path and remote info for this keyword
    let (base_path, hostname, remote_mode, ssh_config_path) = match resolved_paths.iter().find(|(keyword, _)| **keyword == req.keyword) {
        Some((_, (path, hostname, remote_mode, ssh_config_path, _))) => {
            (path.clone(), hostname.clone(), remote_mode.clone(), ssh_config_path.clone())
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid keyword" })),
            )
                .into_response()
        }
    };

    let is_remote = remote_mode == "ssh";

    // Validate mobile app types must be in mobile_discovery_roots
    let is_mobile = matches!(req.app_type.as_str(), "harmonyos" | "ios" | "android");
    if is_mobile && !mobile_roots.contains(&req.keyword) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("{} projects must be created in {:?} directory", req.app_type, mobile_roots)
            })),
        )
            .into_response();
    }

    // Validate project name (devops uses same rules as web)
    let name_regex = if req.app_type == "web" || req.app_type == "devops" {
        Regex::new(r"^[a-z][a-z0-9-]*$").unwrap()
    } else {
        Regex::new(r"^[A-Z][a-zA-Z0-9]*$").unwrap()
    };

    if !name_regex.is_match(&req.name) {
        let hint = if req.app_type == "web" || req.app_type == "devops" {
            "Web/DevOps project names must be lowercase with hyphens, starting with a letter"
        } else {
            "Mobile project names must be PascalCase (uppercase first letter)"
        };
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": hint })),
        )
            .into_response();
    }

    // Build languages list from new fields (backward compatibility)
    let languages = if !req.languages.is_empty() {
        req.languages.clone()
    } else {
        let mut langs = Vec::new();
        if let Some(ref backend) = req.backend_lang {
            langs.push(backend.clone());
        }
        if let Some(ref frontend) = req.frontend_lang {
            langs.push(frontend.clone());
        }
        langs
    };

    // Validate at least one language
    if languages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "at least one language required" })),
        )
            .into_response();
    }

    // Allocate ports
    let (frontend_port, backend_port) = allocate_ports(&state);

    // Generate file contents
    let makefile_content =
        templates::generate_makefile(&req.name, &req.app_type, &languages, frontend_port, backend_port);
    let readme_content = templates::generate_readme(
        &req.name,
        &req.app_type,
        &languages,
        req.description.as_deref(),
    );
    let project_json = templates::generate_project_json(
        &req.name,
        &req.app_type,
        req.description.as_deref(),
        req.backend_lang.as_deref(),
        req.frontend_lang.as_deref(),
        req.style.as_deref(),
        frontend_port,
        backend_port,
    );

    let response = if is_remote {
        // Remote project creation via SSH
        create_remote_project(
            &base_path,
            &req.name,
            &hostname,
            &ssh_config_path,
            &makefile_content,
            &readme_content,
            &project_json,
        )
    } else {
        // Local project creation
        create_local_project(
            &base_path,
            &req.name,
            &makefile_content,
            &readme_content,
            &project_json,
        )
    };

    // Refresh projects if creation was successful, and inject project id into response
    if response.status() == StatusCode::CREATED {
        state.project_manager.refresh();
        let project_id = format!("{}/{}", req.keyword, req.name);
        // Re-build the response to include the id field
        return (StatusCode::CREATED, Json(serde_json::json!({
            "status": "created",
            "id": project_id,
        }))).into_response();
    }

    response
}

/// Create a project on the local filesystem
fn create_local_project(
    base_path: &std::path::Path,
    name: &str,
    makefile_content: &str,
    readme_content: &str,
    project_json: &str,
) -> axum::response::Response {
    let project_path = base_path.join(name);
    tracing::info!("create_local_project: project_path={:?}", project_path);

    if project_path.exists() {
        if project_path.join("README.md").exists() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("project already exists: {}", project_path.display()) })),
            )
                .into_response();
        }
        tracing::info!("Directory exists but is not a project, will initialize: {:?}", project_path);
    } else {
        if let Err(e) = std::fs::create_dir_all(&project_path) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to create directory: {}", e) })),
            )
                .into_response();
        }
    }

    if let Err(e) = std::fs::write(project_path.join("Makefile"), makefile_content) {
        let _ = std::fs::remove_dir_all(&project_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to write Makefile: {}", e) })),
        )
            .into_response();
    }

    if let Err(e) = std::fs::write(project_path.join("README.md"), readme_content) {
        let _ = std::fs::remove_dir_all(&project_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to write README.md: {}", e) })),
        )
            .into_response();
    }

    let milletcode_dir = project_path.join(".milletcode");
    if let Err(e) = std::fs::create_dir_all(&milletcode_dir) {
        let _ = std::fs::remove_dir_all(&project_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to create .milletcode directory: {}", e) })),
        )
            .into_response();
    }

    if let Err(e) = std::fs::write(milletcode_dir.join("project.json"), project_json) {
        let _ = std::fs::remove_dir_all(&project_path);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to write project.json: {}", e) })),
        )
            .into_response();
    }

    (StatusCode::CREATED, Json(serde_json::json!({ "status": "created" }))).into_response()
}

/// Create a project on a remote host via SSH
fn create_remote_project(
    base_path: &std::path::Path,
    name: &str,
    hostname: &str,
    ssh_config_path: &str,
    makefile_content: &str,
    readme_content: &str,
    project_json: &str,
) -> axum::response::Response {
    let base_str = base_path.to_string_lossy();
    // Expand ~ to $HOME for SSH commands
    let expanded_base = if base_str.starts_with("~/") {
        format!("$HOME{}", &base_str[1..])
    } else if base_str == "~" {
        "$HOME".to_string()
    } else {
        base_str.to_string()
    };

    let remote_project_path = format!("{}/{}", expanded_base, name);
    tracing::info!("create_remote_project: host={}, remote_path={}", hostname, remote_project_path);

    // Build SSH args with optional config file
    let expanded_config = if ssh_config_path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            format!("{}/{}", home.to_string_lossy(), &ssh_config_path[2..])
        } else {
            ssh_config_path.to_string()
        }
    } else {
        ssh_config_path.to_string()
    };
    let mut ssh_args = vec!["-o".to_string(), "StrictHostKeyChecking=accept-new".to_string()];
    if std::path::Path::new(&expanded_config).exists() {
        ssh_args.extend(["-F".to_string(), expanded_config]);
    }
    ssh_args.push(hostname.to_string());

    // Check if project already exists on remote
    let check_cmd = format!("[ -f {}/README.md ] && echo 'exists' || echo 'not_exists'", remote_project_path);
    let check_output = std::process::Command::new("ssh")
        .args(&ssh_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .arg(&check_cmd)
        .output();

    match check_output {
        Ok(output) if output.status.success() => {
            let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if result == "exists" {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("project already exists: {}", remote_project_path) })),
                )
                    .into_response();
            }
        }
        Ok(output) => {
            tracing::warn!("SSH check command failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => {
            tracing::warn!("Failed to execute SSH check: {}", e);
        }
    }

    // Create directory and write files via SSH
    let create_cmd = format!(
        r#"mkdir -p {path}/.milletcode && \
cat > {path}/Makefile << 'MILLET_EOF'
{makefile}
MILLET_EOF
cat > {path}/README.md << 'MILLET_EOF'
{readme}
MILLET_EOF
cat > {path}/.milletcode/project.json << 'MILLET_EOF'
{project_json}
MILLET_EOF
echo "created""#,
        path = remote_project_path,
        makefile = makefile_content,
        readme = readme_content,
        project_json = project_json,
    );

    let output = std::process::Command::new("ssh")
        .args(&ssh_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .arg(&create_cmd)
        .output();

    match output {
        Ok(output) if output.status.success() => {
            tracing::info!("Remote project created successfully: {}", remote_project_path);
            (StatusCode::CREATED, Json(serde_json::json!({ "status": "created" }))).into_response()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!("Failed to create remote project: {}", stderr);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to create remote project: {}", stderr) })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to execute SSH command: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to execute SSH command: {}", e) })),
            )
                .into_response()
        }
    }
}

pub async fn archive_project(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let config = state.config_manager.config();
    if !config.project.allow_archive && state.env != "debug" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "archiving projects is disabled" })),
        )
            .into_response();
    }

    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            )
                .into_response()
        }
    };

    // Create .ARCHIVED.md marker file
    let archived_marker = std::path::Path::new(&project.path.filesystem_path()).join(".ARCHIVED.md");
    if let Err(e) = std::fs::write(&archived_marker, "# Archived\n\nThis project has been archived.\n") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to create archive marker: {}", e) })),
        )
            .into_response();
    }

    // Refresh projects to pick up the archived status
    state.project_manager.refresh();

    match state.project_manager.get_by_id(&id) {
        Some(project) => (StatusCode::OK, Json(project)).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "project archived but not found after refresh" })),
        )
            .into_response(),
    }
}

pub async fn unarchive_project(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let config = state.config_manager.config();
    if !config.project.allow_archive && state.env != "debug" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "archiving projects is disabled" })),
        )
            .into_response();
    }

    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            )
                .into_response()
        }
    };

    // Remove .ARCHIVED.md marker file
    let archived_marker = std::path::Path::new(&project.path.filesystem_path()).join(".ARCHIVED.md");
    if archived_marker.exists() {
        if let Err(e) = std::fs::remove_file(&archived_marker) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to remove archive marker: {}", e) })),
            )
                .into_response();
        }
    }

    // Refresh projects to pick up the unarchived status
    state.project_manager.refresh();

    match state.project_manager.get_by_id(&id) {
        Some(project) => (StatusCode::OK, Json(project)).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "project unarchived but not found after refresh" })),
        )
            .into_response(),
    }
}

pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let config = state.config_manager.config();
    if !config.project.allow_delete && state.env != "debug" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "deleting projects is disabled" })),
        )
            .into_response();
    }

    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            )
                .into_response()
        }
    };

    // Delete the project directory. `path.0` is the "keyword:path" index key,
    // never a usable filesystem path — unpack it first.
    let fs_path = project.path.filesystem_path();

    if project.remote_mode == "ssh" && !project.hostname.is_empty() {
        let target = match remote_delete_path(&fs_path) {
            Ok(t) => t,
            Err(reason) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("refusing to delete: {reason}") })),
                )
                    .into_response()
            }
        };

        let ssh_config_path = state
            .config_manager
            .resolve_project_discovery_paths()
            .into_iter()
            .find(|(kw, _)| *kw == project.keyword)
            .map(|(_, (_, _, _, cfg, _))| cfg)
            .unwrap_or_default();

        let cmd = format!("rm -rf {target}");
        match Command::new("ssh")
            .args(build_ssh_args(&project.hostname, &ssh_config_path).iter().map(|s| s.as_str()))
            .arg(&cmd)
            .output()
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("failed to delete remote project: {}", String::from_utf8_lossy(&o.stderr)) })),
                )
                    .into_response()
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("failed to execute ssh: {}", e) })),
                )
                    .into_response()
            }
        }
    } else if fs_path.is_empty() || fs_path == "/" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "refusing to delete: invalid project path" })),
        )
            .into_response();
    } else if let Err(e) = std::fs::remove_dir_all(&fs_path) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to delete project directory: {}", e) })),
        )
            .into_response();
    }

    // Refresh projects
    state.project_manager.refresh();

    (StatusCode::OK, Json(serde_json::json!({ "message": "project deleted" }))).into_response()
}

pub async fn clone_project(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CloneProjectRequest>,
) -> impl IntoResponse {
    let config = state.config_manager.config();
    if !config.project.allow_create && state.env != "debug" {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "creating projects is disabled" })),
        )
            .into_response();
    }

    // Resolve discovery paths (expand ~ to home dir)
    let resolved_paths = state.config_manager.resolve_project_discovery_paths();

    // Find the base path for this keyword
    let (base_path, hostname, remote_mode, ssh_config_path) =
        match resolved_paths.iter().find(|(keyword, _)| **keyword == req.keyword) {
            Some((_, (path, hostname, remote_mode, ssh_config_path, _))) => {
                (path.clone(), hostname.clone(), remote_mode.clone(), ssh_config_path.clone())
            }
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "invalid keyword" })),
                )
                    .into_response()
            }
        };

    // Extract project name from repo URL
    let repo_name = req
        .repo_url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_end_matches(".git");

    if repo_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid repo_url: cannot extract project name" })),
        )
            .into_response();
    }

    if let Err(msg) = validate_clone_creds(&req.repo_url, &req.username, &req.password) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        )
            .into_response();
    }

    let project_path = std::path::Path::new(&base_path).join(repo_name);
    let is_remote = remote_mode == "ssh";

    if is_remote {
        // Remote clone: run git on the remote host. The script travels over
        // ssh stdin (`ssh host sh -s`) so credentials never appear in any
        // command line — local ps shows only "sh -s", remote too.
        if let Err((status, msg)) =
            clone_remote_project(&req, repo_name, &hostname, &ssh_config_path, &base_path)
        {
            return (status, Json(serde_json::json!({ "error": msg }))).into_response();
        }
    } else {
        if project_path.exists() && project_path.join("README.md").exists() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("project already exists: {}", project_path.display()) })),
            )
                .into_response();
        }

        // Build git clone command
        let mut cmd = Command::new("git");
        cmd.arg("clone");

        // Never let git fall back to an interactive credential prompt — without a
        // tty it would hang the request until timeout.
        cmd.env("GIT_TERMINAL_PROMPT", "0");

        if !req.clone_tags {
            cmd.arg("--no-tags");
        }

        if let Some(branch) = &req.branch {
            cmd.arg("--branch").arg(branch);
        }

        if let Some(depth) = req.depth {
            cmd.arg("--depth").arg(depth.to_string());
        }

        if let (Some(user), Some(pass)) = (&req.username, &req.password) {
            // Empty value resets any inherited global credential.helper (e.g.
            // osxkeychain) so the credentials can never be written to disk.
            cmd.arg("-c").arg("credential.helper=");
            cmd.arg("-c").arg(format!("credential.helper={CRED_HELPER}"));
            cmd.env("GIT_CLONE_USER", user).env("GIT_CLONE_PASS", pass);
        }

        cmd.arg(&req.repo_url).arg(&project_path);

        // Execute git clone
        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("failed to execute git: {}", e) })),
                )
                    .into_response()
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("git clone failed: {}", stderr) })),
            )
                .into_response();
        }
    }

    // Refresh projects
    state.project_manager.refresh();

    // Return the new project — discovery keys are "keyword:path", so the
    // lookup must use the same form the scanner inserted, not a bare path.
    // Remote keys keep the raw (tilde) base form used by the scanner.
    let project_key = ProjectPath::from_keyword_path(&req.keyword, &project_path);
    match state.project_manager.get(&project_key) {
        Some(project) => (StatusCode::CREATED, Json(project)).into_response(),
        // The clone itself succeeded; a lookup miss (e.g. indexer lag or a
        // repo shape the scanner skips) must not turn into a false failure.
        None => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "status": "cloned",
                "id": format!("{}/{}", req.keyword, repo_name),
            })),
        )
            .into_response(),
    }
}

/// Single-quote a value for POSIX shell — the result expands to the literal
/// input regardless of $, backticks, quotes or spaces inside.
pub(crate) fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', r"'\''"))
}

/// Escape a value for insertion inside a double-quoted shell string: `$`,
/// backticks, `\` and `"` become literal, while intact `$HOME` expansion is
/// achieved by only ever passing the part AFTER the `~/` prefix through here.
fn double_quote_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Shell word for `rm -rf` on the remote host. Tilde bases stay expandable
/// ("$HOME/..."), everything dynamic is double-quote escaped. Refuses paths
/// that would resolve to the home directory itself.
pub(crate) fn remote_delete_path(fs_path: &str) -> Result<String, String> {
    if fs_path.is_empty() || fs_path == "/" {
        return Err("invalid project path".to_string());
    }
    if let Some(rest) = fs_path.strip_prefix("~/") {
        if rest.is_empty() {
            return Err("path resolves to home directory".to_string());
        }
        Ok(format!("\"$HOME/{}\"", double_quote_escape(rest)))
    } else if fs_path == "~" {
        Err("path resolves to home directory".to_string())
    } else {
        Ok(format!("\"{}\"", double_quote_escape(fs_path)))
    }
}

/// Destination path on the remote host as a double-quoted shell word with a
/// live `$HOME` expansion for tilde bases.
fn remote_dest(base_str: &str, repo_name: &str) -> String {
    if let Some(rest) = base_str.strip_prefix("~/") {
        format!("\"$HOME/{}/{}\"", double_quote_escape(rest), double_quote_escape(repo_name))
    } else if base_str == "~" {
        format!("\"$HOME/{}\"", double_quote_escape(repo_name))
    } else {
        format!("\"{}/{}\"", double_quote_escape(base_str), double_quote_escape(repo_name))
    }
}

/// Build the shell script executed by `ssh host sh -s`. Pure so tests can
/// assert quoting and injection safety without a remote host.
pub(crate) fn build_remote_clone_script(
    repo_url: &str,
    dest: &str,
    clone_tags: bool,
    branch: Option<&str>,
    depth: Option<u32>,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    let mut script = String::from("export GIT_TERMINAL_PROMPT=0\n");
    let git_cmd = if let (Some(user), Some(pass)) = (username, password) {
        script.push_str(&format!("export GIT_CLONE_USER={}\n", shell_quote(user)));
        script.push_str(&format!("export GIT_CLONE_PASS={}\n", shell_quote(pass)));
        script.push_str(&format!("HELPER={}\n", shell_quote(CRED_HELPER)));
        "git -c credential.helper= -c \"credential.helper=$HELPER\"".to_string()
    } else {
        "git".to_string()
    };

    let mut opts = String::new();
    if !clone_tags {
        opts.push_str(" --no-tags");
    }
    if let Some(branch) = branch {
        opts.push_str(&format!(" --branch {}", shell_quote(branch)));
    }
    if let Some(depth) = depth {
        opts.push_str(&format!(" --depth {depth}"));
    }

    // The export lines above live in `script` — the git line must be APPENDED
    // to it, not returned separately, or the env/helper never reach git.
    script.push_str(&format!(
        "{git_cmd} clone{opts} {} {}\n",
        shell_quote(repo_url),
        dest
    ));
    script
}

/// Clone on a remote host over ssh. Errors map to (status, message).
fn clone_remote_project(
    req: &CloneProjectRequest,
    repo_name: &str,
    hostname: &str,
    ssh_config_path: &str,
    base_path: &std::path::Path,
) -> Result<(), (StatusCode, String)> {
    let base_str = base_path.to_string_lossy();
    let dest = remote_dest(&base_str, repo_name);

    // Skip silently on ssh problems — the clone below reports the real error.
    let check_cmd = format!("[ -f {dest}/README.md ] && echo EXISTS; true");
    if let Ok(output) = Command::new("ssh")
        .args(build_ssh_args(hostname, ssh_config_path).iter().map(|s| s.as_str()))
        .arg(&check_cmd)
        .output()
    {
        if String::from_utf8_lossy(&output.stdout).trim() == "EXISTS" {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("project already exists: {}/{}", base_str, repo_name),
            ));
        }
    }

    let script = build_remote_clone_script(
        &req.repo_url,
        &dest,
        req.clone_tags,
        req.branch.as_deref(),
        req.depth,
        req.username.as_deref(),
        req.password.as_deref(),
    );
    // Observability without secrets: shape of the script only.
    tracing::info!(
        "remote clone: dest={} script_len={} first_line={:?} has_user_env={} has_helper={}",
        dest,
        script.len(),
        script.lines().next().unwrap_or(""),
        script.contains("GIT_CLONE_USER="),
        script.contains("HELPER="),
    );

    let mut child = Command::new("ssh")
        .args(build_ssh_args(hostname, ssh_config_path).iter().map(|s| s.as_str()))
        .arg("sh -s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to execute ssh: {e}")))?;

    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(script.as_bytes())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to send clone script: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to run ssh clone: {e}")))?;

    if !output.status.success() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("git clone failed: {}", String::from_utf8_lossy(&output.stderr)),
        ));
    }
    Ok(())
}

/// Allocate frontend and backend ports by finding unused ports
fn allocate_ports(state: &Arc<AppState>) -> (u16, u16) {
    let projects = state.project_manager.list();

    // Find max frontend port (starting from 5173)
    let max_fe = projects
        .iter()
        .filter_map(|p| p.frontend_port)
        .max()
        .unwrap_or(5172);
    let frontend_port = max_fe + 1;

    // Find max backend port (starting from 3000)
    let max_be = projects
        .iter()
        .filter_map(|p| p.backend_port)
        .max()
        .unwrap_or(2999);
    let backend_port = max_be + 1;

    (frontend_port, backend_port)
}

/// Env-store segment validation: hierarchical redb keys break on `/` or empty.
fn valid_env_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains('/')
}

/// List environment names that hold at least one variable
pub async fn list_project_envs(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.env_store.list_envs(&keyword, &name) {
        Ok(envs) => (StatusCode::OK, Json(serde_json::json!({ "envs": envs }))).into_response(),
        Err(e) => {
            tracing::error!("Failed to list envs: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to list envs: {e}") })),
            )
                .into_response()
        }
    }
}

/// List key-value pairs of one environment
pub async fn list_project_env_vars(
    State(state): State<Arc<AppState>>,
    Path((keyword, name, env)): Path<(String, String, String)>,
) -> impl IntoResponse {
    if !valid_env_segment(&env) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid env name" })),
        )
            .into_response();
    }
    match state.env_store.list_vars(&keyword, &name, &env) {
        Ok(vars) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "env": env,
                "vars": vars.into_iter().map(|(key, value)| serde_json::json!({ "key": key, "value": value })).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to list env vars: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to list env vars: {e}") })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct SetEnvVarRequest {
    pub key: String,
    pub value: String,
}

/// Upsert one variable in an environment
pub async fn set_project_env_var(
    State(state): State<Arc<AppState>>,
    Path((keyword, name, env)): Path<(String, String, String)>,
    Json(req): Json<SetEnvVarRequest>,
) -> impl IntoResponse {
    if !valid_env_segment(&env) || !valid_env_segment(&req.key) || req.value.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "env/key must be non-empty without '/', value non-empty" })),
        )
            .into_response();
    }
    match state.env_store.set_var(&keyword, &name, &env, &req.key, &req.value) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => {
            tracing::error!("Failed to set env var: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to save env var: {e}") })),
            )
                .into_response()
        }
    }
}

/// Delete one variable; key may not contain `/` (env var names don't)
pub async fn delete_project_env_var(
    State(state): State<Arc<AppState>>,
    Path((keyword, name, env, key)): Path<(String, String, String, String)>,
) -> impl IntoResponse {
    if !valid_env_segment(&env) || !valid_env_segment(&key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid env or key" })),
        )
            .into_response();
    }
    match state.env_store.delete_var(&keyword, &name, &env, &key) {
        Ok(true) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "env var not found" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to delete env var: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to delete env var: {e}") })),
            )
                .into_response()
        }
    }
}

/// 需求接收 (project-level git issue intake) config. GET returns the URL plus
/// a masked token and whether a token is stored — the raw token never leaves
/// the server.
pub async fn get_project_issue_config(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let cfg = match state.issue_store.get(&keyword, &name) {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(e) => {
            tracing::error!("Failed to get issue config: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to get issue config: {e}") })),
            )
                .into_response();
        }
    };
    let has_token = !cfg.token.is_empty();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "url": cfg.url,
            "token": crate::config::mask_token(&cfg.token),
            "has_token": has_token,
        })),
    )
        .into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct SetIssueConfigRequest {
    #[serde(default)]
    pub url: String,
    /// Empty = keep the previously stored token (the UI never echoes it back).
    #[serde(default)]
    pub token: String,
}

/// Upsert the project-level issue config. An empty token keeps the stored one;
    /// when url and token both end up empty the entry is removed (system-level
    /// [git.issue] takes over again).
pub async fn set_project_issue_config(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
    Json(req): Json<SetIssueConfigRequest>,
) -> impl IntoResponse {
    let existing = match state.issue_store.get(&keyword, &name) {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(e) => {
            tracing::error!("Failed to read issue config: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to read issue config: {e}") })),
            )
                .into_response();
        }
    };
    let merged = crate::config::GitIssueConfig {
        url: req.url.trim().to_string(),
        token: if req.token.is_empty() {
            existing.token
        } else {
            req.token.trim().to_string()
        },
    };
    let result = if merged.url.is_empty() && merged.token.is_empty() {
        state.issue_store.clear(&keyword, &name).map(|_| ())
    } else {
        state.issue_store.set(&keyword, &name, &merged)
    };
    match result {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "url": merged.url,
                "token": crate::config::mask_token(&merged.token),
                "has_token": !merged.token.is_empty(),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to save issue config: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to save issue config: {e}") })),
            )
                .into_response()
        }
    }
}

/// Remove the project-level config — the system-level [git.issue] takes over.
pub async fn delete_project_issue_config(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.issue_store.clear(&keyword, &name) {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response(),
        Err(e) => {
            tracing::error!("Failed to clear issue config: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to clear issue config: {e}") })),
            )
                .into_response()
        }
    }
}

/// 项目连接页面 URL（会话工具栏「连接」按钮用）。存 settings KV：project.link.<keyword>/<name>。
pub async fn get_project_link(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let key = format!("project.link.{keyword}/{name}");
    let url = state
        .settings_store
        .get(&key)
        .ok()
        .flatten()
        .unwrap_or_default();
    (StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct SetProjectLinkRequest {
    #[serde(default)]
    pub url: String,
}

/// Upsert the project link URL. Empty url removes the entry.
pub async fn set_project_link(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
    Json(req): Json<SetProjectLinkRequest>,
) -> impl IntoResponse {
    let url = req.url.trim().to_string();
    if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "url must be http(s)" })),
        )
            .into_response();
    }
    let key = format!("project.link.{keyword}/{name}");
    let result = if url.is_empty() {
        state.settings_store.delete(&key).map(|_| ())
    } else {
        state.settings_store.set(&key, &url)
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "url": url }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to save link: {e}") })),
        )
            .into_response(),
    }
}

/// Get terminal commands for a project
pub async fn get_project_terminal_commands(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            )
                .into_response()
        }
    };

    match state.project_store.get_terminal_commands(&project.path.0) {
        Ok(commands) => (StatusCode::OK, Json(commands)).into_response(),
        Err(e) => {
            tracing::error!("Failed to get terminal commands: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to load terminal commands: {}", e) })),
            )
                .into_response()
        }
    }
}

/// Set terminal commands for a project
pub async fn set_project_terminal_commands(
    State(state): State<Arc<AppState>>,
    Path((keyword, name)): Path<(String, String)>,
    Json(commands): Json<Vec<crate::db::ProjectTerminalCommand>>,
) -> impl IntoResponse {
    let id = format!("{}/{}", keyword, name);
    let project = match state.project_manager.get_by_id(&id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "project not found" })),
            )
                .into_response()
        }
    };

    if let Err(e) = state.project_store.set_terminal_commands(&project.path.0, &commands) {
        tracing::error!("Failed to set terminal commands: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("failed to save terminal commands: {}", e) })),
        )
            .into_response();
    }

    (StatusCode::OK, Json(serde_json::json!({ "message": "ok" }))).into_response()
}

#[cfg(test)]
mod clone_creds_tests {
    use super::{CRED_HELPER, validate_clone_creds};
    use std::process::Command;

    #[test]
    fn validate_accepts_anonymous_when_no_creds() {
        assert!(validate_clone_creds("git@github.com:org/repo.git", &None, &None).is_ok());
    }

    #[test]
    fn validate_accepts_https_with_creds() {
        assert!(validate_clone_creds(
            "https://git.example.com/org/repo.git",
            &Some("u".into()),
            &Some("p".into())
        )
        .is_ok());
    }

    #[test]
    fn validate_accepts_case_insensitive_scheme() {
        assert!(validate_clone_creds(
            "HTTPS://git.example.com/org/repo.git",
            &Some("u".into()),
            &Some("p".into())
        )
        .is_ok());
    }

    #[test]
    fn validate_rejects_creds_on_ssh_url() {
        let err = validate_clone_creds(
            "git@github.com:org/repo.git",
            &Some("u".into()),
            &Some("p".into()),
        )
        .unwrap_err();
        assert!(err.contains("http(s)"), "unexpected: {}", err);
    }

    #[test]
    fn validate_rejects_newline_injection() {
        assert!(validate_clone_creds(
            "https://git.example.com/org/repo.git",
            &Some("u\nurl=git://evil".into()),
            &Some("p".into())
        )
        .is_err());
        assert!(validate_clone_creds(
            "https://git.example.com/org/repo.git",
            &Some("u".into()),
            &Some("p\r\n".into())
        )
        .is_err());
        assert!(validate_clone_creds(
            "https://git.example.com/org/repo.git",
            &Some("u\0".into()),
            &None
        )
        .is_err());
    }

    /// Contract test: git must accept our helper and emit exactly the
    /// username/password lines — pins the helper against git's credential
    /// protocol across environments.
    #[test]
    fn credential_helper_protocol_contract() {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new("git")
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg(format!("credential.helper={CRED_HELPER}"))
            .arg("credential")
            .arg("fill")
            .env("GIT_CLONE_USER", "alice")
            .env("GIT_CLONE_PASS", "s3cret")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git credential fill should spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(b"url=https://git.example.com/org/repo.git\n\n")
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait git credential fill");
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("username=alice\n") && stdout.contains("password=s3cret\n"),
            "stdout was: {:?}", stdout
        );
    }

    /// With env unset the helper must emit nothing (token-only / anonymous-safe).
    #[test]
    fn credential_helper_emits_nothing_without_env() {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new("git")
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg(format!("credential.helper={CRED_HELPER}"))
            .arg("credential")
            .arg("fill")
            .env_remove("GIT_CLONE_USER")
            .env_remove("GIT_CLONE_PASS")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git credential fill should spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(b"url=https://git.example.com/org/repo.git\n\n")
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait git credential fill");
        // git may fail (no credentials available) — either way our helper
        // must not have produced output lines.
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!stdout.contains("username="), "unexpected stdout: {:?}", stdout);
    }
}

#[cfg(test)]
mod remote_clone_script_tests {
    use super::{build_remote_clone_script, remote_dest, shell_quote};
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// shell_quote must roundtrip arbitrary hostile values through sh.
    #[test]
    fn shell_quote_roundtrips_hostile_values() {
        for v in [
            "bob",
            "pass with spaces",
            "it's got a quote",
            "dollar_$HOME",
            "back`echo pwned`tick",
            "semi;colon",
            "double\"quote",
            "back\\slash",
            "$(touch /tmp/should-not-exist)",
            "new\nline removed earlier but quote-safe anyway",
        ] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(v)))
                .output()
                .expect("sh should run");
            assert_eq!(String::from_utf8_lossy(&out.stdout), v, "roundtrip failed for {:?}", v);
        }
    }

    /// The generated script must be syntactically valid shell.
    #[test]
    fn generated_script_is_valid_sh() {
        let script = build_remote_clone_script(
            "https://git.example.com/org/repo.git",
            "\"$HOME/Codes/huawei/repo\"",
            false,
            Some("main"),
            Some(1),
            Some("bob"),
            Some("s'ecret"),
        );
        let out = Command::new("sh").arg("-n").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
        let mut child = out.expect("sh -n should spawn");
        child.stdin.as_mut().expect("piped").write_all(script.as_bytes()).expect("write");
        let done = child.wait_with_output().expect("wait");
        assert!(done.status.success(), "sh -n rejected script: {}", String::from_utf8_lossy(&done.stderr));
    }

    /// Regressions: the exported env (terminal prompt off + credential env)
    /// must actually be IN the returned script. A refactor once returned the
    /// git line alone, so $HELPER stayed unset and git fell back to a terminal
    /// prompt ("Device not configured") on non-tty ssh sessions.
    #[test]
    fn credentialed_script_carries_env_exports() {
        let script = build_remote_clone_script(
            "https://git.example.com/org/repo.git",
            "\"$HOME/Codes/huawei/repo\"",
            true,
            None,
            None,
            Some("bob"),
            Some("s'ecret"),
        );

        assert!(
            script.starts_with("export GIT_TERMINAL_PROMPT=0\n"),
            "script must start with GIT_TERMINAL_PROMPT=0 export, got: {script:?}"
        );
        assert!(script.contains("export GIT_CLONE_USER='bob'\n"), "missing user export: {script:?}");
        assert!(script.contains("export GIT_CLONE_PASS="), "missing pass export: {script:?}");
        assert!(script.contains("HELPER="), "missing helper assignment: {script:?}");
        // No-credential script must NOT carry the helper plumbing.
        let plain = build_remote_clone_script("https://git.example.com/org/repo.git", "\"$HOME/r\"", true, None, None, None, None);
        assert!(!plain.contains("GIT_CLONE_USER"), "plain script must not reference user env: {plain:?}");
        assert!(plain.contains("git clone "), "plain script must use bare git: {plain:?}");
    }

    /// Command substitution in the repo URL must end up quoted — the script
    /// clones a literal (failing) name instead of executing the payload.
    #[test]
    fn script_neutralizes_url_injection() {
        let marker = "/tmp/millet-clone-injection-probe";
        let _ = std::fs::remove_file(marker);

        let script = build_remote_clone_script(
            &format!("https://git.example.com/org/a$(touch {marker}).git"),
            "\"$HOME/probe-dest\"",
            true,
            None,
            None,
            None,
            None,
        );
        let mut child = Command::new("sh").arg("-s").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("sh -s");
        child.stdin.as_mut().expect("piped").write_all(script.as_bytes()).expect("write");
        let _ = child.wait_with_output().expect("wait");

        assert!(
            !std::path::Path::new(marker).exists(),
            "URL command substitution was executed — injection!"
        );
        let _ = std::fs::remove_file(marker);
    }

    /// Full no-credential script run: clone a local seed repo through sh -s.
    #[test]
    fn script_clones_end_to_end_without_creds() {
        let dir = std::env::temp_dir().join(format!("millet-clone-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let seed = dir.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        run(Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&seed));
        std::fs::write(seed.join("README.md"), "seed").unwrap();
        run(Command::new("git").current_dir(&seed).args(["add", "."]));
        run(Command::new("git").current_dir(&seed).args(["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]));

        let home = dir.join("home");
        std::fs::create_dir_all(home.join("Codes")).unwrap();
        let dest = format!("\"$HOME/Codes/{}\"", "cloned");
        let script = build_remote_clone_script(seed.to_str().unwrap(), &dest, true, None, None, None, None);

        let mut child = Command::new("sh")
            .arg("-s")
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh -s");
        child.stdin.as_mut().expect("piped").write_all(script.as_bytes()).expect("write");
        let done = child.wait_with_output().expect("wait");
        assert!(done.status.success(), "clone failed: {}", String::from_utf8_lossy(&done.stderr));
        assert!(home.join("Codes/cloned/README.md").exists(), "cloned content missing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tilde bases must produce a live $HOME expansion; hostile repo names
    /// must stay literal inside the double-quoted destination.
    #[test]
    fn remote_dest_quoting() {
        assert_eq!(remote_dest("~/Codes/huawei", "repo"), "\"$HOME/Codes/huawei/repo\"");
        assert_eq!(
            remote_dest("~/Codes/huawei", "a$(touch x)"),
            "\"$HOME/Codes/huawei/a\\$(touch x)\""
        );
        assert_eq!(remote_dest("/srv/git", "repo"), "\"/srv/git/repo\"");
        assert_eq!(remote_dest("~", "repo"), "\"$HOME/repo\"");
    }

    /// Delete targets keep $HOME expandable (single-quoting it would make
    /// `rm -rf` a silent no-op), refuse degenerate paths, and stay literal
    /// for hostile content.
    #[test]
    fn remote_delete_path_quoting() {
        use super::remote_delete_path;
        assert_eq!(
            remote_delete_path("~/Codes/huawei/repo").unwrap(),
            "\"$HOME/Codes/huawei/repo\""
        );
        // Executed by sh, the word must expand to a real absolute path —
        // the regression this pins: '$HOME/...' single-quoted is a no-op for rm.
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", remote_delete_path("~/Codes/huawei/repo").unwrap()))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with('/'));

        assert_eq!(remote_delete_path("/srv/git/repo").unwrap(), "\"/srv/git/repo\"");
        assert!(remote_delete_path("").is_err());
        assert!(remote_delete_path("/").is_err());
        assert!(remote_delete_path("~").is_err());
        assert!(remote_delete_path("~/").is_err());
        assert_eq!(
            remote_delete_path("~/x/$(touch y)").unwrap(),
            "\"$HOME/x/\\$(touch y)\""
        );
    }

    fn run(cmd: &mut Command) {
        let out = cmd.output().expect("command should run");
        assert!(out.status.success(), "command failed: {}", String::from_utf8_lossy(&out.stderr));
    }
}
