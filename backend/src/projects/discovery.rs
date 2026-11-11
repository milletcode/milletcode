use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::projects::types::{Project, ProjectPath};

/// Scan immediate subdirectories of each base path.
/// A directory is considered a project if it contains a README.md.
/// Supports both local paths and remote paths (format: "hostname:path")
pub fn scan_projects(base_paths: &HashMap<String, (PathBuf, String, String, String, bool)>) -> Vec<Project> {
    let mut projects = Vec::new();

    for (keyword, (base, hostname, remote_mode, ssh_config_path, is_mobile_home)) in base_paths {
        // Scan local or remote directory
        let scanned = if remote_mode == "ssh" {
            scan_remote_directory(base, keyword, hostname, remote_mode, ssh_config_path, *is_mobile_home)
        } else {
            scan_local_directory(base, keyword, *is_mobile_home)
        };

        projects.extend(scanned);
    }

    projects
}

/// Scan local directory for projects
fn scan_local_directory(base: &Path, keyword: &str, is_mobile_home: bool) -> Vec<Project> {
    let mut projects = Vec::new();

    let Ok(entries) = std::fs::read_dir(base) else {
        tracing::warn!("Failed to read directory: {:?}", base);
        return projects;
    };

    let is_community = keyword.ends_with("github");

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let readme = path.join("README.md");
        // A .git dir alone marks a project too — freshly cloned repos often
        // have no README yet, and skipping them made clones undiscoverable.
        let is_git_repo = path.join(".git").exists();
        if !readme.exists() && !is_git_repo {
            continue;
        }

        let description = std::fs::read_to_string(&readme)
            .ok()
            .and_then(|content| {
                content
                    .lines()
                    .nth(2)
                    .map(|line| line.trim().trim_start_matches('#').trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();

        let last_modified = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                chrono::DateTime::<chrono::Utc>::from(t)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown".to_string());

        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let (frontend_port, backend_port) = parse_ports(&path, is_community);

        let archived = path.join(".ARCHIVED.md").exists();

        projects.push(Project {
            id: format!("{}/{}", keyword, name),
            path: ProjectPath::from_keyword_path(keyword, &path),
            name,
            hostname: String::new(),
            remote_mode: String::new(),
            is_mobile_home,
            description,
            last_modified,
            keyword: keyword.to_string(),
            frontend_port,
            backend_port,
            archived,
        });
    }

    projects
}

/// Expand `~/` prefix to `$HOME` for SSH commands (SSH doesn't expand ~ inside quotes).
fn expand_tilde_for_ssh(path: &str) -> String {
    if path.starts_with("~/") {
        format!("$HOME{}", &path[1..])
    } else if path == "~" {
        "$HOME".to_string()
    } else {
        path.to_string()
    }
}

/// Expand `~/` prefix to actual home directory for file paths
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

/// Build SSH command arguments with optional config file
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

/// Scan remote directory via SSH
fn scan_remote_directory(base: &Path, keyword: &str, hostname: &str, remote_mode: &str, ssh_config_path: &str, is_mobile_home: bool) -> Vec<Project> {
    let mut projects = Vec::new();

    let base_str = base.to_string_lossy();
    // SSH doesn't expand `~` inside double quotes, so replace it with $HOME.
    let expanded_base = expand_tilde_for_ssh(&base_str);
    let cmd = format!(
        "ls -1 {} 2>/dev/null | while read dir; do if [ -d \"{}/$dir\" ] && {{ [ -f \"{}/$dir/README.md\" ] || [ -e \"{}/$dir/.git\" ]; }}; then echo \"$dir\"; fi; done",
        expanded_base, expanded_base, expanded_base, expanded_base
    );

    let ssh_args = build_ssh_args(hostname, ssh_config_path);
    let output = std::process::Command::new("ssh")
        .args(&ssh_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .arg(&cmd)
        .output();

    let Ok(output) = output else {
        tracing::warn!("Failed to SSH to {}: command execution failed", hostname);
        return projects;
    };

    if !output.status.success() {
        tracing::warn!("SSH command failed for {}: {}", hostname, String::from_utf8_lossy(&output.stderr));
        return projects;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        let dir_name = line.trim();
        if dir_name.is_empty() {
            continue;
        }

        let remote_path = format!("{}/{}", base_str, dir_name);
        let expanded_remote = expand_tilde_for_ssh(&remote_path);

        // Get README description
        let description = get_remote_readme_description(hostname, &expanded_remote, ssh_config_path);

        // Get last modified time
        let last_modified = get_remote_last_modified(hostname, &expanded_remote, ssh_config_path);

        // Check if archived
        let archived = check_remote_archived(hostname, &expanded_remote, ssh_config_path);

        projects.push(Project {
            id: format!("{}/{}", keyword, dir_name),
            path: ProjectPath::from_keyword_path(keyword, Path::new(&remote_path)),
            name: dir_name.to_string(),
            hostname: hostname.to_string(),
            remote_mode: remote_mode.to_string(),
            is_mobile_home,
            description,
            last_modified,
            keyword: keyword.to_string(),
            frontend_port: None, // TODO: Parse ports from remote Makefile
            backend_port: None,
            archived,
        });
    }

    projects
}

/// Get README description from remote host
fn get_remote_readme_description(hostname: &str, path: &str, ssh_config_path: &str) -> String {
    let cmd = format!("head -n 3 {}/README.md 2>/dev/null | tail -n 1 | sed 's/^[# ]*//'", path);
    let mut ssh_args = build_ssh_args(hostname, ssh_config_path);
    ssh_args.push(cmd);
    let output = std::process::Command::new("ssh")
        .args(&ssh_args)
        .output();

    output.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

/// Get last modified time from remote host
fn get_remote_last_modified(hostname: &str, path: &str, ssh_config_path: &str) -> String {
    let cmd = format!("stat -f '%Sm' -t '%Y-%m-%d' {} 2>/dev/null || stat -c '%y' {} 2>/dev/null | cut -d' ' -f1", path, path);
    let mut ssh_args = build_ssh_args(hostname, ssh_config_path);
    ssh_args.push(cmd);
    let output = std::process::Command::new("ssh")
        .args(&ssh_args)
        .output();

    output.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Check if project is archived on remote host
fn check_remote_archived(hostname: &str, path: &str, ssh_config_path: &str) -> bool {
    let cmd = format!("[ -f {}/.ARCHIVED.md ] && echo 'yes' || echo 'no'", path);
    let mut ssh_args = build_ssh_args(hostname, ssh_config_path);
    ssh_args.push(cmd);
    let output = std::process::Command::new("ssh")
        .args(&ssh_args)
        .output();

    output.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "yes")
        .unwrap_or(false)
}

/// Parse frontend and backend ports from a project.
///
/// For internal projects (is_community = false):
///   Only check Makefile for `frontend` and `backend` targets.
///
/// For community projects (is_community = true):
///   1. Makefile variable assignments (FRONTEND_PORT, BACKEND_PORT, etc.)
///   2. Makefile target commands (--port NNNN, PORT=NNNN)
///   3. vite.config.ts/js as frontend fallback
fn parse_ports(project_path: &Path, is_community: bool) -> (Option<u16>, Option<u16>) {
    if is_community {
        parse_ports_community(project_path)
    } else {
        parse_ports_internal(project_path)
    }
}

/// Find the Makefile in a project directory (case-insensitive, prefer "Makefile" over "makefile").
fn find_makefile(project_path: &Path) -> Option<PathBuf> {
    let upper = project_path.join("Makefile");
    if upper.exists() {
        return Some(upper);
    }
    let lower = project_path.join("makefile");
    if lower.exists() {
        return Some(lower);
    }
    None
}

/// Parse ports for internal projects - from Makefile variables and targets
fn parse_ports_internal(project_path: &Path) -> (Option<u16>, Option<u16>) {
    let mut frontend_port: Option<u16> = None;
    let mut backend_port: Option<u16> = None;

    if let Some(makefile_path) = find_makefile(project_path) {
        if let Ok(content) = std::fs::read_to_string(&makefile_path) {
            // First, check for variable definitions (like BACKEND_PORT ?= 3000)
            let fe_patterns = [
                r"(?i)FRONTEND_PORT\s*\??=\s*(\d+)",
                r"(?i)DEV_FRONTEND[_A-Z]*PORT\s*\??=\s*(\d+)",
                r"(?i)VITE_PORT\s*\??=\s*(\d+)",
            ];
            let be_patterns = [
                r"(?i)BACKEND_PORT\s*\??=\s*(\d+)",
                r"(?i)SERVER_PORT\s*\??=\s*(\d+)",
                r"(?i)API_PORT\s*\??=\s*(\d+)",
            ];

            for pat in &fe_patterns {
                if let Ok(re) = Regex::new(pat) {
                    if let Some(caps) = re.captures(&content) {
                        if let Some(m) = caps.get(1) {
                            frontend_port = m.as_str().parse().ok();
                            break;
                        }
                    }
                }
            }

            for pat in &be_patterns {
                if let Ok(re) = Regex::new(pat) {
                    if let Some(caps) = re.captures(&content) {
                        if let Some(m) = caps.get(1) {
                            backend_port = m.as_str().parse().ok();
                            break;
                        }
                    }
                }
            }

            // If not found in variables, check targets
            if frontend_port.is_none() {
                if let Some(block) = extract_makefile_target(&content, "frontend") {
                    frontend_port = parse_port_from_target_block(&block);
                }
            }
            if backend_port.is_none() {
                if let Some(block) = extract_makefile_target(&content, "backend") {
                    backend_port = parse_port_from_target_block(&block);
                }
            }
        }
    }

    (frontend_port, backend_port)
}

/// Parse ports for community projects - flexible detection
fn parse_ports_community(project_path: &Path) -> (Option<u16>, Option<u16>) {
    let mut frontend_port: Option<u16> = None;
    let mut backend_port: Option<u16> = None;

    // --- Parse Makefile (case-insensitive, prefer "Makefile") ---
    if let Some(makefile_path) = find_makefile(project_path) {
        if let Ok(content) = std::fs::read_to_string(&makefile_path) {
            let fe_patterns = [
                r"(?i)FRONTEND_PORT\s*\??=\s*(\d+)",
                r"(?i)DEV_FRONTEND[_A-Z]*PORT\s*\??=\s*(\d+)",
                r"(?i)VITE_PORT\s*\??=\s*(\d+)",
            ];
            let be_patterns = [
                r"(?i)BACKEND_PORT\s*\??=\s*(\d+)",
                r"(?i)SERVER_PORT\s*\??=\s*(\d+)",
                r"(?i)API_PORT\s*\??=\s*(\d+)",
            ];
            let generic_port_re = Regex::new(r"(?i)^PORT\s*\??=\s*(\d+)").ok();

            for pat in &fe_patterns {
                if let Ok(re) = Regex::new(pat) {
                    if let Some(caps) = re.captures(&content) {
                        if let Some(m) = caps.get(1) {
                            frontend_port = m.as_str().parse().ok();
                            break;
                        }
                    }
                }
            }

            for pat in &be_patterns {
                if let Ok(re) = Regex::new(pat) {
                    if let Some(caps) = re.captures(&content) {
                        if let Some(m) = caps.get(1) {
                            backend_port = m.as_str().parse().ok();
                            break;
                        }
                    }
                }
            }

            // Generic PORT — treat as backend if no backend found yet
            if backend_port.is_none() {
                if let Some(re) = &generic_port_re {
                    if let Some(caps) = re.captures(&content) {
                        if let Some(m) = caps.get(1) {
                            backend_port = m.as_str().parse().ok();
                        }
                    }
                }
            }

            // --- Parse frontend / backend targets (community projects may use either naming) ---
            if frontend_port.is_none() {
                for target in &["frontend", "dev-frontend"] {
                    if let Some(block) = extract_makefile_target(&content, target) {
                        frontend_port = parse_port_from_target_block(&block);
                        if frontend_port.is_some() {
                            break;
                        }
                    }
                }
            }
            if backend_port.is_none() {
                for target in &["backend", "dev-backend"] {
                    if let Some(block) = extract_makefile_target(&content, target) {
                        backend_port = parse_port_from_target_block(&block);
                        if backend_port.is_some() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // --- Parse vite.config.ts/js as frontend fallback ---
    if frontend_port.is_none() {
        // Check both root and frontend/ subdirectory
        let vite_search_paths = [
            project_path.to_path_buf(),
            project_path.join("frontend"),
        ];
        for base in &vite_search_paths {
            for name in &["vite.config.ts", "vite.config.js", "vite.config.mts"] {
                let vite_path = base.join(name);
                if let Ok(content) = std::fs::read_to_string(&vite_path) {
                    if let Ok(re) = Regex::new(r"(?m)port:\s*(\d+)") {
                        if let Some(caps) = re.captures(&content) {
                            if let Some(m) = caps.get(1) {
                                frontend_port = m.as_str().parse().ok();
                                break;
                            }
                        }
                    }
                }
            }
            if frontend_port.is_some() {
                break;
            }
        }
    }

    (frontend_port, backend_port)
}

/// Extract the command block for a specific Makefile target.
/// Returns the tab-indented recipe lines for `target_name:`.
fn extract_makefile_target<'a>(content: &'a str, target_name: &str) -> Option<String> {
    let mut lines = content.lines().peekable();
    let mut in_target = false;
    let mut block_lines = Vec::new();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();

        // Skip comment lines
        if trimmed.starts_with('#') {
            continue;
        }

        if !in_target {
            // Target definitions must start at column 0 (not indented)
            if line.starts_with('\t') || line.starts_with(' ') {
                continue;
            }

            // Match target header: "dev-frontend:" or "dev-frontend : something"
            if let Some(before_colon) = trimmed.split(':').next() {
                let targets: Vec<&str> = before_colon.split_whitespace().collect();
                if targets.contains(&target_name) {
                    in_target = true;
                }
            }
        } else {
            // Inside target block: collect tab-indented recipe lines
            if line.starts_with('\t') {
                block_lines.push(line);
            } else if trimmed.is_empty() {
                // Blank lines can appear within recipes, keep going
                if lines.peek().map_or(false, |l| l.starts_with('\t')) {
                    continue;
                }
                break;
            } else {
                break;
            }
        }
    }

    if block_lines.is_empty() {
        None
    } else {
        Some(block_lines.join("\n"))
    }
}

/// Parse port number from a Makefile target block.
/// Matches: --port NNNN, -p NNNN, PORT=NNNN, HOST:PORT, :PORT
fn parse_port_from_target_block(block: &str) -> Option<u16> {
    let patterns = [
        r"--port\s+(\d+)",
        r"-p\s+(\d+)",
        r"(?i)PORT[:?]?=\s*(\d+)",
        r"\d+\.\d+\.\d+\.\d+:(\d+)",   // HOST:PORT e.g. 127.0.0.1:3030
        r":(\d{4,5})",                    // :PORT (4-5 digits to avoid false positives)
    ];
    for pat in &patterns {
        if let Ok(re) = Regex::new(pat) {
            if let Some(caps) = re.captures(block) {
                if let Some(m) = caps.get(1) {
                    return m.as_str().parse().ok();
                }
            }
        }
    }
    None
}
