use axum::{Json, extract::{Path, Query, State}};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::AppState;
use crate::auth::is_debug_mode;
use crate::db::SessionRecord;

/// Helper to check debug mode from headers
fn is_debug_mode_from_headers(state: &AppState, headers: &axum::http::HeaderMap) -> bool {
    is_debug_mode(state, headers)
}

/// Get the current log file path
fn get_current_log_path() -> Option<PathBuf> {
    let log_dir = dirs::home_dir()?.join(".milletcode").join("logs");

    // Find the most recent agent-console.YYYY-MM-DD file
    let mut entries: Vec<_> = fs::read_dir(&log_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("agent-console.")
        })
        .filter_map(|e| {
            let metadata = e.metadata().ok()?;
            let modified = metadata.modified().ok()?;
            Some((e.path(), modified))
        })
        .collect();

    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.first().map(|(p, _)| p.clone())
}

/// Get current log file info
pub async fn get_log_info(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    if !is_debug_mode_from_headers(&state, &headers) {
        return Json(serde_json::json!({ "error": "debug mode only" }));
    }

    match get_current_log_path() {
        Some(path) => {
            let metadata = fs::metadata(&path).ok();
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            Json(serde_json::json!({
                "path": path.to_string_lossy(),
                "size": size,
                "modified": modified,
                "exists": true,
            }))
        }
        None => {
            Json(serde_json::json!({
                "exists": false,
                "error": "No log file found",
            }))
        }
    }
}

/// Read log file content (tail)
#[derive(Deserialize)]
pub struct LogTailQuery {
    pub lines: Option<usize>,
}

pub async fn get_log_tail(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Query(query): Query<LogTailQuery>,
) -> Json<serde_json::Value> {
    if !is_debug_mode_from_headers(&state, &headers) {
        return Json(serde_json::json!({ "error": "debug mode only" }));
    }

    let lines = query.lines.unwrap_or(100);

    let log_path = match get_current_log_path() {
        Some(p) => p,
        None => {
            return Json(serde_json::json!({
                "error": "No log file found",
                "lines": [],
            }));
        }
    };

    match fs::read_to_string(&log_path) {
        Ok(content) => {
            let all_lines: Vec<&str> = content.lines().collect();
            let total = all_lines.len();
            let start = total.saturating_sub(lines);
            let tail_lines = &all_lines[start..];

            Json(serde_json::json!({
                "path": log_path.to_string_lossy(),
                "total_lines": total,
                "lines": tail_lines,
                "offset": start,
            }))
        }
        Err(e) => {
            Json(serde_json::json!({
                "error": format!("Failed to read log: {}", e),
                "lines": [],
            }))
        }
    }
}

#[derive(Serialize)]
pub struct ClaudeSession {
    pub pid: String,
    pub session_uuid: String,
    /// "local" or the remote hostname the process was found on
    pub host: String,
    /// Discovery-path keyword (org) the session belongs to, "?" if unattributed
    pub org: String,
    /// Project name (may contain '/'), "?" if unattributed
    pub project: String,
    pub agent: String,
    /// Session env name (last segment of the composite session key)
    pub session: String,
    pub command: String,
}

/// Server-side throttle cache for one remote host's `ps` output.
#[derive(Clone)]
pub struct RemotePsCache {
    pub fetched_at: Instant,
    pub raw: String,
    pub error: Option<String>,
}

/// How long a remote host's ps output is served from cache before refreshing.
const REMOTE_PS_TTL: Duration = Duration::from_secs(10);
/// Per-host SSH round trip budget; on timeout the last good cache is kept.
const REMOTE_PS_TIMEOUT: Duration = Duration::from_secs(5);

/// Extract session UUID from --session-id or --resume argument
fn extract_session_uuid(command: &str) -> Option<String> {
    // Try --session-id first, then --resume
    for flag in &["--session-id", "--resume"] {
        if let Some(pos) = command.find(flag) {
            let rest = &command[pos + flag.len()..];
            let rest = rest.trim_start();
            let uuid = rest.split_whitespace().next()?;
            if !uuid.is_empty() {
                return Some(uuid.to_string());
            }
        }
    }
    None
}

/// Look up session by composite key or claude session UUID and return
/// (working_dir, claude_session_id). Returns None if not found.
fn get_session_info(state: &AppState, key: &str) -> Option<(String, String)> {
    let record = state
        .session_manager
        .get_session_by_key(key)
        .ok()
        .flatten()
        .or_else(|| {
            // Callers may pass the claude session UUID instead of the
            // composite key — reverse lookup by claude_session_id.
            state
                .session_manager
                .list_all_sessions()
                .into_iter()
                .find(|r| r.claude_session_id.as_deref() == Some(key))
        })?;
    // project_id is in "keyword/name" format — look up to get the actual filesystem path
    let working_dir = record.project_id.as_ref()
        .and_then(|pid| state.project_manager.get_by_id(pid))
        .map(|p| p.path.filesystem_path())?;
    let claude_session_id = record.claude_session_id.unwrap_or_else(|| key.to_string());
    Some((working_dir, claude_session_id))
}

struct RawProcess {
    pid: String,
    command: String,
}

/// Parse one `ps auwx` line into a milletcode-managed claude process.
/// Returns None for non-claude lines, claude invocations without
/// stream-json, or malformed output.
fn parse_ps_line(line: &str) -> Option<RawProcess> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    // ps auwx format: USER PID %CPU %MEM VSZ RSS TT STAT STARTED TIME COMMAND
    let _user = parts.next()?;
    let pid = parts.next()?.to_string();
    let _cpu = parts.next()?;
    let _mem = parts.next()?;
    // skip VSZ, RSS, TT, STAT, STARTED, TIME (6 fields)
    for _ in 0..6 {
        parts.next()?;
    }
    let command = parts.collect::<Vec<_>>().join(" ");
    if !command.contains("stream-json") {
        return None;
    }
    Some(RawProcess { pid, command })
}

/// Attribution fields derived from a stored session record.
#[derive(Clone)]
struct Attribution {
    org: String,
    project: String,
    agent: String,
    session: String,
}

fn attribution_of(record: &SessionRecord) -> Attribution {
    let org = record.keyword.clone();
    // project_id is "keyword/name" and the name may contain '/' — strip only
    // the keyword prefix instead of parsing the composite session key.
    let project = record
        .project_id
        .as_ref()
        .and_then(|pid| pid.strip_prefix(&format!("{}/", org)))
        .unwrap_or("?")
        .to_string();
    let session = record.session_id.rsplit('/').next().unwrap_or("?").to_string();
    Attribution {
        org,
        project,
        agent: record.agent_name.clone(),
        session,
    }
}

/// Index claude_session_id (UUID) -> attribution for every stored session.
/// Processes found by ps are attributed by matching the UUID in their
/// --session-id/--resume argument against this index.
fn build_uuid_index(state: &AppState) -> HashMap<String, Attribution> {
    let mut index = HashMap::new();
    for record in state.session_manager.list_all_sessions() {
        if let Some(uuid) = &record.claude_session_id {
            index.insert(uuid.clone(), attribution_of(&record));
        }
    }
    index
}

fn entry_from_line(
    line: &str,
    host: &str,
    index: &HashMap<String, Attribution>,
) -> Option<ClaudeSession> {
    let raw = parse_ps_line(line)?;
    let session_uuid = extract_session_uuid(&raw.command)?;
    let attr = index
        .get(&session_uuid)
        .cloned()
        .unwrap_or(Attribution {
            org: "?".to_string(),
            project: "?".to_string(),
            agent: String::new(),
            session: String::new(),
        });
    Some(ClaudeSession {
        pid: raw.pid,
        session_uuid,
        host: host.to_string(),
        org: attr.org,
        project: attr.project,
        agent: attr.agent,
        session: attr.session,
        command: raw.command,
    })
}

struct RemoteHost {
    hostname: String,
    ssh_config_path: String,
}

/// Distinct remote (remote_mode=ssh) hosts across all discovery paths.
/// Multiple keywords may share one host — dedupe by hostname.
fn remote_hosts(state: &AppState) -> Vec<RemoteHost> {
    let mut hosts: Vec<RemoteHost> = Vec::new();
    for (_, (_, hostname, remote_mode, ssh_config, _)) in
        state.config_manager.resolve_project_discovery_paths()
    {
        if remote_mode != "ssh" || hostname.is_empty() {
            continue;
        }
        if hosts.iter().any(|h| h.hostname == hostname) {
            continue;
        }
        hosts.push(RemoteHost {
            hostname,
            ssh_config_path: ssh_config,
        });
    }
    hosts
}

/// Shared ps filter chain; `extra_exclude` is inserted before the final
/// claude grep (local uses it to hide the console's own process).
fn ps_filter_chain(extra_exclude: Option<&str>) -> String {
    let mut cmd =
        "ps auwx | grep -v grep | grep -v '\\.claude' | grep -v node_modules".to_string();
    if let Some(extra) = extra_exclude {
        cmd.push_str(&format!(" | grep -v '{}'", extra));
    }
    cmd.push_str(" | grep claude || true");
    cmd
}

fn run_local_ps() -> String {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(ps_filter_chain(Some("target/debug/agent-console")))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Run the same filter chain on a remote host via ssh (shell-out pattern
/// shared with project discovery).
fn run_remote_ps(hostname: &str, ssh_config_path: &str) -> std::io::Result<String> {
    let mut args = crate::projects::discovery::build_ssh_args(hostname, ssh_config_path);
    args.push(ps_filter_chain(None));
    let output = std::process::Command::new("ssh").args(&args).output()?;
    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "ssh {}: {}",
                hostname,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub async fn ps(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    if !is_debug_mode_from_headers(&state, &headers) {
        return Json(serde_json::json!({ "error": "debug mode only" }));
    }

    let uuid_index = build_uuid_index(&state);
    let mut entries: Vec<ClaudeSession> = Vec::new();
    let mut raw_sections: Vec<String> = Vec::new();

    // Local processes: fresh scan on every call.
    let local_raw = run_local_ps();
    let mut local_lines: Vec<&str> = Vec::new();
    for line in local_raw.lines() {
        if let Some(entry) = entry_from_line(line, "local", &uuid_index) {
            local_lines.push(line);
            entries.push(entry);
        }
    }
    raw_sections.push(format!("--- local ---\n{}", local_lines.join("\n")));

    // Remote processes: throttled per host behind a server-side cache so the
    // 5s panel polling doesn't translate into 5s SSH round trips.
    let hosts = remote_hosts(&state);

    let mut to_refresh: Vec<RemoteHost> = Vec::new();
    {
        let cache = state.remote_ps_cache.lock();
        for host in &hosts {
            let fresh = cache
                .get(&host.hostname)
                .is_some_and(|c| c.fetched_at.elapsed() < REMOTE_PS_TTL);
            if !fresh {
                to_refresh.push(RemoteHost {
                    hostname: host.hostname.clone(),
                    ssh_config_path: host.ssh_config_path.clone(),
                });
            }
        }
    }

    if !to_refresh.is_empty() {
        let refreshed = futures::future::join_all(to_refresh.into_iter().map(|host| {
            let state = state.clone();
            async move {
                let hostname = host.hostname.clone();
                let ssh_config = host.ssh_config_path.clone();
                let result = tokio::time::timeout(
                    REMOTE_PS_TIMEOUT,
                    tokio::task::spawn_blocking(move || run_remote_ps(&hostname, &ssh_config)),
                )
                .await;
                let (raw, error) = match result {
                    Ok(Ok(Ok(raw))) => (raw, None),
                    Ok(Ok(Err(e))) => (String::new(), Some(e.to_string())),
                    Ok(Err(e)) => (String::new(), Some(format!("spawn failed: {}", e))),
                    Err(_) => (
                        String::new(),
                        Some(format!("timed out after {}s", REMOTE_PS_TIMEOUT.as_secs())),
                    ),
                };
                let raw = if error.is_some() {
                    // Keep the last good output visible alongside the error marker
                    state
                        .remote_ps_cache
                        .lock()
                        .get(&host.hostname)
                        .map(|c| c.raw.clone())
                        .unwrap_or_default()
                } else {
                    raw
                };
                (host.hostname, RemotePsCache { fetched_at: Instant::now(), raw, error })
            }
        }))
        .await;
        state.remote_ps_cache.lock().extend(refreshed);
    }

    let snapshot: Vec<(String, RemotePsCache)> = state
        .remote_ps_cache
        .lock()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    for (hostname, cached) in snapshot {
        let mut host_lines: Vec<&str> = Vec::new();
        for line in cached.raw.lines() {
            if let Some(entry) = entry_from_line(line, &hostname, &uuid_index) {
                host_lines.push(line);
                entries.push(entry);
            }
        }
        let mut section = format!("--- {} ---\n{}", hostname, host_lines.join("\n"));
        if let Some(err) = &cached.error {
            section.push_str(&format!("\n[error] {}", err));
        }
        raw_sections.push(section);
    }

    Json(serde_json::json!({
        "entries": entries,
        "raw": raw_sections.join("\n\n"),
    }))
}

#[derive(Serialize)]
pub struct LogEntry {
    pub project: String,
    pub session_uuid: String,
    pub path: String,
    pub size: u64,
    pub modified: u64,
}

#[derive(Deserialize)]
pub struct LogQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub async fn list_logs(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    if !is_debug_mode_from_headers(&state, &headers) {
        return Json(serde_json::json!({ "error": "debug mode only" }));
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let projects_dir = home.join(".claude").join("projects");

    let mut logs: Vec<LogEntry> = Vec::new();

    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let project_slug = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                // Decode project path from slug: -Users-foo-bar -> /Users/foo/bar
                let project = if project_slug.starts_with('-') {
                    project_slug[1..].replace('-', "/")
                } else {
                    project_slug.replace('-', "/")
                };

                // Find all .jsonl files in this project directory
                if let Ok(files) = fs::read_dir(&path) {
                    for file_entry in files.flatten() {
                        let file_path = file_entry.path();
                        if file_path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                            let session_uuid = file_path.file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("")
                                .to_string();

                            let metadata = fs::metadata(&file_path).ok();
                            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                            let modified = metadata
                                .as_ref()
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);

                            logs.push(LogEntry {
                                project: project.clone(),
                                session_uuid,
                                path: file_path.to_string_lossy().to_string(),
                                size,
                                modified,
                            });
                        }
                    }
                }
            }
        }
    }

    // Sort by modified time, newest first
    logs.sort_by(|a, b| b.modified.cmp(&a.modified));

    Json(serde_json::json!({ "logs": logs }))
}

#[derive(Deserialize)]
pub struct SessionLogQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub async fn read_session_log(
    State(state): State<Arc<AppState>>,
    Path(session_uuid): Path<String>,
    Query(query): Query<SessionLogQuery>,
) -> Json<serde_json::Value> {
    if state.env != "debug" {
        return Json(serde_json::json!({ "error": "debug mode only" }));
    }

    let limit = query.limit.unwrap_or(50);

    // Look up session in in-memory store to get working_dir and claude_session_id
    let (working_dir, claude_session_id) = match get_session_info(&state, &session_uuid) {
        Some(info) => info,
        None => {
            return Json(serde_json::json!({
                "error": format!("session {} not found in store — process may have been destroyed. Please refresh to get latest results.", session_uuid),
                "messages": [],
                "has_more": false,
            }));
        }
    };

    // Construct JSONL path: ~/.claude/projects/<slug>/<claude_session_id>.jsonl
    // slug = working_dir with '/' replaced by '-'
    let slug = working_dir.replace('/', "-");
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let path_buf = home.join(".claude").join("projects").join(&slug).join(format!("{}.jsonl", claude_session_id));

    if !path_buf.exists() {
        return Json(serde_json::json!({
            "error": format!("log file not found at {}", path_buf.display()),
            "messages": [],
            "has_more": false,
        }));
    }

    // Read raw JSONL lines directly (don't go through read_session_history
    // which re-derives the path and gets the slug decoding wrong)
    match fs::read_to_string(&path_buf) {
        Ok(content) => {
            let mut entries: Vec<serde_json::Value> = Vec::new();

            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) => entries.push(v),
                    Err(_) => continue,
                }
            }

            let total = entries.len();
            // Read last N entries (tail behavior)
            let start = total.saturating_sub(limit);
            let end = total;
            let has_more = start > 0;

            let page: Vec<serde_json::Value> = if start < total {
                entries[start..end].to_vec()
            } else {
                Vec::new()
            };

            Json(serde_json::json!({
                "messages": page,
                "has_more": has_more,
                "total": total,
                "session_uuid": session_uuid,
                "project": working_dir,
                "path": path_buf.to_string_lossy(),
            }))
        }
        Err(e) => {
            Json(serde_json::json!({
                "error": format!("failed to read log: {}", e),
                "messages": [],
                "has_more": false,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps_line(command: &str) -> String {
        // USER PID %CPU %MEM VSZ RSS TT STAT STARTED TIME COMMAND
        format!("user 12345 0.1 0.2 123456 6789 ?? S 10:00AM 0:01 {}", command)
    }

    fn record(
        uuid: Option<&str>,
        project_id: Option<&str>,
        keyword: &str,
        agent_name: &str,
        session_id: &str,
    ) -> SessionRecord {
        SessionRecord {
            session_id: session_id.to_string(),
            claude_session_id: uuid.map(|s| s.to_string()),
            project_id: project_id.map(|s| s.to_string()),
            keyword: keyword.to_string(),
            agent_id: "claude".to_string(),
            agent_name: agent_name.to_string(),
            status: "running".to_string(),
            prompt: String::new(),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            create_time: 0,
            update_time: 0,
            pid: Some(1),
            pid_started_at: None,
            idle_notify_enabled: false,
            model: None,
            last_error: None,
        }
    }

    #[test]
    fn parse_ps_line_extracts_pid_and_command() {
        let line = ps_line("/usr/local/bin/claude --session-id abc --input-format stream-json");
        let raw = parse_ps_line(&line).unwrap();
        assert_eq!(raw.pid, "12345");
        assert!(raw.command.contains("--session-id abc"));
    }

    #[test]
    fn parse_ps_line_rejects_non_stream_json() {
        assert!(parse_ps_line(&ps_line("claude --session-id abc")).is_none());
    }

    #[test]
    fn parse_ps_line_rejects_malformed() {
        assert!(parse_ps_line("").is_none());
        assert!(parse_ps_line("only a few words").is_none());
    }

    #[test]
    fn extract_session_uuid_prefers_session_id_then_resume() {
        assert_eq!(
            extract_session_uuid("claude --session-id 111-222 --input-format stream-json")
                .as_deref(),
            Some("111-222")
        );
        assert_eq!(
            extract_session_uuid("claude --resume 333-444").as_deref(),
            Some("333-444")
        );
        assert_eq!(extract_session_uuid("claude --input-format stream-json"), None);
    }

    #[test]
    fn attribution_handles_project_name_with_slash() {
        let r = record(
            Some("uuid-1"),
            Some("WebApps/group/sub"),
            "WebApps",
            "claude",
            "WebApps/group/sub/claude/env1",
        );
        let a = attribution_of(&r);
        assert_eq!(a.org, "WebApps");
        assert_eq!(a.project, "group/sub");
        assert_eq!(a.agent, "claude");
        assert_eq!(a.session, "env1");
    }

    #[test]
    fn attribution_falls_back_to_unknown() {
        let r = record(None, None, "WebApps", "claude", "WebApps/demo/claude/env1");
        let a = attribution_of(&r);
        assert_eq!(a.org, "WebApps");
        assert_eq!(a.project, "?");
        assert_eq!(a.session, "env1");
    }

    #[test]
    fn entry_from_line_uses_index_and_marks_unknown() {
        let mut index = HashMap::new();
        index.insert(
            "uuid-1".to_string(),
            attribution_of(&record(
                Some("uuid-1"),
                Some("WebApps/demo"),
                "WebApps",
                "claude",
                "WebApps/demo/claude/env1",
            )),
        );

        let known = ps_line("claude --session-id uuid-1 --input-format stream-json");
        let e = entry_from_line(&known, "remote1", &index).unwrap();
        assert_eq!(e.host, "remote1");
        assert_eq!(e.org, "WebApps");
        assert_eq!(e.project, "demo");
        assert_eq!(e.session, "env1");

        let orphan = ps_line("claude --session-id uuid-9 --input-format stream-json");
        let e = entry_from_line(&orphan, "local", &index).unwrap();
        assert_eq!(e.org, "?");
        assert_eq!(e.project, "?");
    }

    #[test]
    fn remote_ps_cache_ttl_boundary() {
        let fresh = RemotePsCache {
            fetched_at: Instant::now(),
            raw: String::new(),
            error: None,
        };
        assert!(fresh.fetched_at.elapsed() < REMOTE_PS_TTL);

        let stale = RemotePsCache {
            fetched_at: Instant::now() - REMOTE_PS_TTL - Duration::from_secs(1),
            raw: String::new(),
            error: None,
        };
        assert!(stale.fetched_at.elapsed() >= REMOTE_PS_TTL);
    }

    #[test]
    fn ps_filter_chain_includes_extra_exclude_only_for_local() {
        let local = ps_filter_chain(Some("target/debug/agent-console"));
        assert!(local.contains("grep -v 'target/debug/agent-console'"));
        let remote = ps_filter_chain(None);
        assert!(!remote.contains("agent-console"));
        assert!(remote.ends_with("grep claude || true"));
    }
}
