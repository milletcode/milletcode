use std::sync::Arc;

use anyhow::{Result, anyhow};
use dashmap::DashMap;
use futures::StreamExt;
use tracing;

use super::{AgentAdapter, AgentRegistry, SessionHandle, PendingControlInfo, PendingKind};
use super::parse_jsonl_history;
use super::read_session_history as read_jsonl_history;
use super::{
    parse_codex_history, parse_smartcode_history, read_codex_session_history,
    read_smartcode_session_history,
};
use super::HistoryResponse;
use crate::config::ConfigManager;
use crate::runtime::{SftpClient, SshRuntime};
use crate::agents::types::*;
use crate::db::{SessionRecord, SessionStore, ApprovalStore, DecisionRecord, slugify, sanitize_segment};
use crate::events::EventBus;
use crate::logging::is_session_log_enabled;
use crate::projects::manager::ProjectManager;
use crate::projects::types::ProjectPath;

/// Session manager tracks all active sessions, routes messages, and manages approvals.
pub struct SessionManager {
    registry: Arc<AgentRegistry>,
    event_bus: Arc<EventBus>,
    // Arc-wrapped: spawn_event_task receives a clone, and DashMap::clone is a
    // deep copy — without Arc the task-local map would silently diverge from
    // the manager's (pending decisions invisible to /api/pending, statuses
    // never landing).
    sessions: Arc<DashMap<SessionId, SessionInfo>>,
    /// Central registry of in-band control requests waiting for a human
    /// decision (approval or AskUserQuestion), keyed by session.
    pending_controls: Arc<DashMap<SessionId, PendingControlInfo>>,
    /// Last activity (unix ms) per session — bumped on every agent stream
    /// event and every delivered user message. In-memory only; the idle
    /// scanner falls back to the record's persisted update_time (e.g. after
    /// a backend restart) until the first event re-seeds it.
    last_activity: Arc<DashMap<SessionId, u64>>,
    /// Latest WS Origin header per project key ("org/project/agent"). The idle
    /// notifier composes session URLs from it so links work from wherever the
    /// user actually browses — production domains are dynamic (StatefulSet),
    /// no static URL can be authoritative.
    ws_origins: Arc<DashMap<String, String>>,
    /// Sessions already auto-notified in the current idle episode
    /// (edge-trigger). A entry survives agent activity and the auto
    /// notification itself; it is cleared only by a genuine user message that
    /// is not the auto /millet-msg payload.
    idle_notified: Arc<DashMap<SessionId, ()>>,
    config: Arc<ConfigManager>,
    project_manager: Arc<ProjectManager>,
    store: SessionStore,
    /// Decision audit trail (approvals.redb) for 回溯.
    approval_store: ApprovalStore,
    /// Per-project 需求接收 configs (envs.redb) — spawn-time gitea credentials.
    issue_store: crate::db::IssueStore,
}

/// Persisted statuses that mean the agent process should be running. Only
/// these records take part in the idle scan; everything else is already ended.
/// "complete" counts too: smartcode turn-complete sessions keep the process
/// alive waiting for the next message — same "agent idle, ping the user"
/// situation as running. Dead-process records are filtered by the pid check.
fn is_idle_candidate_status(status: &str) -> bool {
    status == "running" || status == "waiting_approval" || status == "complete"
}

/// Idle = process alive and no activity (agent event or user message) for at
/// least the threshold. A zero timestamp means "never active" — never idle.
fn is_idle_threshold_met(last_activity_ms: u64, now_ms: u64, threshold_ms: u64) -> bool {
    last_activity_ms != 0 && now_ms.saturating_sub(last_activity_ms) >= threshold_ms
}

/// Whether a delivered message should become the session list's "latest
/// message" tail. The auto /millet-msg idle notification is system-generated
/// (the user never typed it) and must not clobber the tail; other skill
/// commands stay visible.
fn should_update_prompt_tail(message: &str) -> bool {
    !message.trim_start().starts_with("/millet-msg")
}

/// Verify the pid's command line mentions `needle`. Guards SIGKILL-by-recorded-pid
/// against pid reuse: `is_process_alive` trusts claude's pidfile, which can outlive
/// its process and attach to an unrelated recycled pid.
/// If `ps` itself can't run (minimal containers), returns true — reclaim proceeds
/// with the same trust level as before rather than silently disabling it.
fn local_process_cmdline_contains(pid: u32, needle: &str) -> bool {
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    {
        // Fail closed: an unusable ps probe must never read as a match — the
        // caller responds to a match by killing the pid.
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .to_lowercase()
            .contains(needle),
        _ => false,
    }
}

/// Capture a process's start time (`ps -o lstart=`). Recorded at spawn; at
/// reclaim the recorded and current stamps must be equal — a reused PID cannot
/// reproduce the spawn-time stamp, so a foreign process is never killed.
fn local_process_start_time(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stamp = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stamp.is_empty() { None } else { Some(stamp) }
}

/// Whether `pid` still belongs to the agent process this record spawned.
/// Start-time equality rules out PID reuse; legacy records without a captured
/// stamp keep the historical cmdline-only best effort. Needle follows the
/// record's adapter id (records predating multi-agent support default to
/// claude).
fn local_reclaim_target_matches(record: &SessionRecord, pid: u32) -> bool {
    let needle = if record.agent_id.is_empty() { "claude" } else { record.agent_id.as_str() };
    if !local_process_cmdline_contains(pid, needle) {
        return false;
    }
    match record.pid_started_at.as_deref() {
        Some(recorded) => local_process_start_time(pid).as_deref() == Some(recorded),
        None => true,
    }
}

/// SIGKILL a local pid and poll until it is gone. Returns true when the process
/// exited (or was never there) within `timeout`.
async fn kill_local_pid_and_wait(pid: u32, timeout: std::time::Duration) -> bool {
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    if nix::sys::signal::kill(pid, None).is_err() {
        return true;
    }
    // 进程组优先（本地 spawn 均为组长）：npm shim 类 CLI（codex 等）会派生
    // 孙进程，只杀直接 pid 会留孤儿。组不存在时回退直接杀。
    let group = nix::unistd::Pid::from_raw(-(pid.as_raw() as i32));
    if nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGKILL).is_err() {
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if nix::sys::signal::kill(pid, None).is_err() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    nix::sys::signal::kill(pid, None).is_err()
}

impl SessionManager {
    /// Keywords of discovery paths configured with remote_mode == "ssh".
    /// These sessions run on a remote host; their recorded `pid` is a channel-id
    /// placeholder, not a real OS pid, so the local pid-file liveness probe must
    /// not be applied to them.
    fn remote_keywords(&self) -> std::collections::HashSet<String> {
        self.config
            .resolve_project_discovery_paths()
            .into_iter()
            .filter(|(_, (_, _, remote_mode, _, _))| remote_mode == "ssh")
            .map(|(keyword, _)| keyword)
            .collect()
    }

    /// Check whether a Claude process is alive.
    /// For local processes: check via pid file + signal probe.
    /// For remote SSH processes: check if session is in memory with active status.
    /// Remote processes cannot be probed via pid since the pid is actually an SSH channel ID.
    fn is_process_alive(&self, is_remote: bool, pid: u32, session_id: &SessionId) -> bool {
        if is_remote {
            // For remote SSH sessions, check if session is in memory with active status
            // If the session is not in memory or has a terminal status, the process is not alive
            if let Some(info) = self.sessions.get(session_id) {
                matches!(
                    info.status,
                    SessionStatus::Running | SessionStatus::Starting | SessionStatus::WaitingApproval
                )
            } else {
                false
            }
        } else {
            // For local processes, check via pid file + signal probe
            let path = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".claude")
                .join("sessions")
                .join(format!("{}.json", pid));
            if path.exists() {
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
            } else {
                false
            }
        }
    }

    pub fn new(
        registry: Arc<AgentRegistry>,
        event_bus: Arc<EventBus>,
        config: Arc<ConfigManager>,
        project_manager: Arc<ProjectManager>,
        store: SessionStore,
        approval_store: ApprovalStore,
        issue_store: crate::db::IssueStore,
    ) -> Self {
        Self {
            registry,
            event_bus,
            sessions: Arc::new(DashMap::new()),
            pending_controls: Arc::new(DashMap::new()),
            last_activity: Arc::new(DashMap::new()),
            ws_origins: Arc::new(DashMap::new()),
            idle_notified: Arc::new(DashMap::new()),
            config,
            project_manager,
            store,
            approval_store,
            issue_store,
        }
    }

    /// Create a new agent session with a specific agent ID
    pub async fn create_session_with_agent_id(
        &self,
        project_path: Option<ProjectPath>,
        agent_id: String,
        prompt: &str,
        session_name: &str,
    ) -> Result<(SessionId, String)> {
        let config = self.config.config();
        let (_id, agent_config) = config
            .agent
            .find_by_id(&agent_id)
            .ok_or_else(|| anyhow!("No enabled agent found with id: {}", agent_id))?;
        let agent_config = agent_config.clone();

        self.create_session_inner(project_path, agent_id, agent_config, prompt, session_name).await
    }

    /// Create a new agent session with an explicit session name
    pub async fn create_session(
        &self,
        project_path: Option<ProjectPath>,
        agent_id: String,
        prompt: &str,
        session_name: &str,
    ) -> Result<(SessionId, String)> {
        let config = self.config.config();
        let (_id, agent_config) = config
            .agent
            .find_enabled(&agent_id)
            .ok_or_else(|| anyhow!("No enabled agent found with id: {}", agent_id))?;
        let agent_config = agent_config.clone();

        self.create_session_inner(project_path, agent_id, agent_config, prompt, session_name).await
    }

    /// Build a RuntimeContext from project path and agent instance id.
    /// Resolves hostname, binary_path, ssh_config_path, ssh_passphrase from
    /// Project/DiscoveryPath config.
    ///
    /// Binary path resolution priority:
    /// 1. project.discovery_paths[keyword].paths[agent_id] (highest - project-level override)
    /// 2. agent.instances[agent_id].remote_path (remote only, falls back to .path)
    /// 3. agent.instances[agent_id].path (default, used for both local and remote)
    /// 4. "" (fallback - build_command will use "claude")
    pub fn build_runtime_context(&self, project_path: Option<&ProjectPath>, agent_id: &str) -> crate::agents::RuntimeContext {
        let full_config = self.config.config();

        let (keyword, hostname, remote_mode, project_name) = project_path
            .and_then(|pp| self.project_manager.get(pp))
            .map(|p| (p.keyword.clone(), p.hostname.clone(), p.remote_mode.clone(), p.name.clone()))
            .unwrap_or_else(|| ("default".to_string(), String::new(), String::new(), String::new()));

        // Resolve binary_path with priority:
        // 1. discovery_paths[keyword].paths[agent_id] (project-group override)
        // 2. agent.instances[agent_id].remote_path (remote only, falls back to .path)
        // 3. agent.instances[agent_id].path (default, used for local and remote)
        // 4. "" (fallback - build_command will use "claude")
        let project_binary_path = full_config
            .project
            .discovery_paths
            .get(&keyword)
            .and_then(|dp| dp.paths.get(agent_id))
            .cloned();

        let is_remote = remote_mode == "ssh";
        let agent_binary_path = full_config
            .agent
            .instances
            .get(agent_id)
            .and_then(|ac| {
                if is_remote {
                    ac.remote_path.clone().or_else(|| ac.path.clone())
                } else {
                    ac.path.clone()
                }
            });

        let binary_path = project_binary_path
            .or(agent_binary_path)
            .unwrap_or_default();

        // Expand ~ in binary_path to the local home for LOCAL spawns only
        // (Command::new does not perform shell expansion). Remote sessions keep
        // `~/...`: the SSH command builder rewrites the leading ~ to $HOME and
        // the REMOTE shell expands it — remote hosts may have a different home
        // layout.
        let binary_path = if !is_remote && (binary_path.starts_with("~/") || binary_path == "~") {
            if let Some(home) = dirs::home_dir() {
                shellexpand::tilde(&binary_path).to_string()
            } else {
                binary_path
            }
        } else {
            binary_path
        };

        let ssh_config_path = full_config
            .project
            .discovery_paths
            .get(&keyword)
            .and_then(|dp| dp.ssh.as_ref())
            .map(|s| crate::config::resolve_ssh_config_path(&s.config_path))
            .unwrap_or_else(|| crate::config::resolve_ssh_config_path(&full_config.ssh.config_path));

        let ssh_passphrase = full_config.ssh.passphrase.clone();

        // /git issue credentials: project-level 需求接收 config wins over the
        // system-level [git.issue]; nothing injected when both are empty.
        let project_issue = if project_name.is_empty() {
            None
        } else {
            self.issue_store.get(&keyword, &project_name).unwrap_or(None)
        };
        let gitea_env = crate::config::resolve_issue_env(
            project_issue.as_ref(),
            &full_config.git.issue,
        );

        crate::agents::RuntimeContext {
            hostname,
            remote_mode,
            binary_path,
            ssh_config_path,
            ssh_passphrase,
            ssh_password: None, // TODO: support from config
            gitea_env,
        }
    }

    /// Resolve a project's ProjectPath from a stored session's project_id.
    /// Records store the project id as "keyword/name" (e.g. "WebApps/demo-web"),
    /// resolvable via get_by_id.
    fn project_path_from_record(&self, project_id: &str) -> Option<ProjectPath> {
        self.project_manager
            .get_by_id(project_id)
            .map(|p| p.path)
    }

    /// Public wrapper: resolve ProjectPath from a session composite key.
    /// Used by API endpoints to convert a session key into a filesystem path
    /// for JSONL history loading.
    pub fn resolve_project_path(&self, session_key: &str) -> Option<ProjectPath> {
        let record = self.get_session_by_key(session_key).ok().flatten()?;
        let project_id = record.project_id.as_ref()?;
        self.project_path_from_record(project_id)
    }

    /// Extract the filesystem path from a legacy session record's project_id.
    /// Old records stored "keyword:/abs/path" — this parses the path part directly
    /// without requiring the project to still exist in ProjectManager.
    /// Used only by full_delete_session for disk cleanup of old sessions.
    fn filesystem_path_from_legacy_record(project_id: &str) -> Option<String> {
        // Old format: "keyword:/abs/path" — split on first ':'
        if let Some(idx) = project_id.find(':') {
            let path = &project_id[idx + 1..];
            if !path.is_empty() && path.starts_with('/') {
                return Some(path.to_string());
            }
        }
        None
    }

    /// Internal implementation for creating a session
    async fn create_session_inner(
        &self,
        project_path: Option<ProjectPath>,
        agent_id: String,
        config: crate::config::AgentConfig,
        prompt: &str,
        session_name: &str,
    ) -> Result<(SessionId, String)> {
        let agent_name = config.name.clone();

        // A session must be bound to a project discovered under a discovery_path.
        // Reject unbound sessions: without a project the code falls back to a local
        // spawn, which is wrong when the agent binary only exists on a remote
        // (remote_mode = "ssh") host.
        let project = project_path
            .as_ref()
            .and_then(|pp| self.project_manager.get(pp))
            .ok_or_else(|| anyhow::anyhow!(
                "无法创建会话：请先选择或新建一个项目（当前未命中任何 discovery path 下的项目）"
            ))?;

        // Working dir must be the actual filesystem path (strip "keyword:" prefix) —
        // for remote (ssh) sessions this is the path that gets cd'd into on the remote
        // host, where the "keyword:/abs/path" form does not exist. Same convention as
        // restore_session/create_session_from_record below.
        let working_dir = Some(project.path.filesystem_path());

        // Sanitize session name
        let session_name = sanitize_segment(session_name);
        let session_name = if session_name.is_empty() { "default".to_string() } else { session_name };

        // keyword + project id ("keyword/name") come from the resolved project
        let keyword = project.keyword.clone();
        let project_id = project.id.clone();

        // Build RuntimeContext
        let runtime_ctx = self.build_runtime_context(project_path.as_ref(), &agent_id);

        // Composite key: "keyword/project/agent-id/session-name"
        let composite_key = SessionStore::make_key(
            &keyword,
            Some(project_id.as_str()),
            &agent_id,
            &session_name,
        );

        // SessionId wraps composite key
        let session_id = SessionId::new(composite_key.clone());

        // Generate UUID for Claude CLI
        let claude_session_id = uuid::Uuid::new_v4().to_string();

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter registered for: {}", agent_id))?;

        tracing::info!(
            session = %session_id,
            claude_session_id = %claude_session_id,
            agent = %agent_id,
            project = ?project_path,
            key = %composite_key,
            working_dir = ?working_dir,
            prompt_preview = prompt.chars().take(120).collect::<String>(),
            "Creating new session"
        );

        let handle = adapter.start_session(&config, &runtime_ctx, session_id.clone(), &claude_session_id, &composite_key, prompt, working_dir.clone()).await?;

        // 会话标识由 agent 侧生成的 adapter（如 millet 经 stderr 自报 JSONL 路径）
        // 在启动后上报句柄 —— 用它覆盖预生成 UUID，restore/history 才有正确凭据。
        let claude_session_id = adapter.session_ref(&session_id).unwrap_or(claude_session_id);

        let session_info = SessionInfo {
            id: session_id.clone(),
            claude_session_id: Some(claude_session_id.clone()),
            project_path: project_path.clone(),
            agent_id: agent_id.to_string(),
            agent_name: agent_name.clone(),
            status: SessionStatus::Running,
            created_at: chrono::Utc::now(),
            prompt: prompt.to_string(),
        };

        // Persist to redb immediately with composite key (metadata only, no output)
        // Extract pid from the SessionHandle returned by start_session
        let pid = match &handle.state {
            crate::agents::SessionHandleState::Subprocess { pid } => Some(*pid),
            _ => None,
        };
        let record = SessionRecord {
            session_id: composite_key.clone(),  // composite key as id
            claude_session_id: Some(claude_session_id),
            project_id: Some(project_id), // "keyword/name" — resolvable via project_manager.get_by_id
            keyword,
            agent_id: session_info.agent_id.clone(),
            agent_name: session_info.agent_name.clone(),
            status: "running".to_string(),
            prompt: prompt.to_string(),
            created_at: session_info.created_at,
            create_time: SessionRecord::now_millis(),
            update_time: SessionRecord::now_millis(),
            pid,
            pid_started_at: pid.and_then(local_process_start_time),
            idle_notify_enabled: false,
            model: None,
            last_error: None,
        };
        self.store.put(&composite_key, &record)?;

        self.sessions.insert(session_id.clone(), session_info);

        self.spawn_event_task(adapter, handle, session_id.clone());

        Ok((session_id, composite_key))
    }

    /// Spawn the canonical per-session event-processing task. All
    /// start/restore paths share this: it mirrors adapter events onto the
    /// event bus, persists status transitions, and maintains the central
    /// pending-controls registry backing the message panel and badge.
    fn spawn_event_task(
        &self,
        adapter: Arc<dyn AgentAdapter>,
        handle: SessionHandle,
        session_id: SessionId,
    ) {
        let sessions = self.sessions.clone();
        let pending = self.pending_controls.clone();
        let event_bus = self.event_bus.clone();
        let store = self.store.clone();
        let project_manager = self.project_manager.clone();
        let last_activity = self.last_activity.clone();
        let session_id_clone = session_id.clone();
        tokio::spawn(async move {
            let stream = adapter.output_stream(&handle);
            futures::pin_mut!(stream);
            last_activity.insert(session_id_clone.clone(), SessionRecord::now_millis());
            while let Some(event) = stream.next().await {
                last_activity.insert(session_id_clone.clone(), SessionRecord::now_millis());
                if let AgentEvent::Output { content, .. } = &event {
                    if is_session_log_enabled() {
                        tracing::info!(
                            session = %session_id_clone,
                            output_preview = content.chars().take(200).collect::<String>(),
                            "Agent output"
                        );
                    }
                }
                match &event {
                    AgentEvent::ApprovalRequest { prompt, options, request_id, tool_name, .. } => {
                        if is_session_log_enabled() {
                            tracing::info!(
                                session = %session_id_clone,
                                approval_prompt = prompt.chars().take(200).collect::<String>(),
                                options = ?options,
                                "Approval request"
                            );
                        }
                        let (project_name, agent_name) = sessions
                            .get(&session_id_clone)
                            .map(|info| (
                                info.project_path.as_ref()
                                    .and_then(|pid| project_manager.get(pid).map(|p| p.name.clone())),
                                Some(info.agent_name.clone()),
                            ))
                            .unwrap_or((None, None));
                        pending.insert(
                            session_id_clone.clone(),
                            PendingControlInfo {
                                kind: PendingKind::Approval {
                                    prompt: prompt.clone(),
                                    options: options.clone(),
                                    tool_name: tool_name.clone(),
                                    request_id: request_id.clone(),
                                },
                                project_name,
                                agent_name,
                                ts: AgentEvent::now(),
                            },
                        );
                        if let Some(mut info) = sessions.get_mut(&session_id_clone) {
                            info.status = SessionStatus::WaitingApproval;
                        }
                        event_bus.emit(AgentEvent::SessionStatus {
                            session_id: session_id_clone.clone(),
                            status: SessionStatus::WaitingApproval,
                            ts: AgentEvent::now(),
                        });
                    }
                    AgentEvent::UserQuestion { tool_use_id, request_id, questions, .. } => {
                        let (project_name, agent_name) = sessions
                            .get(&session_id_clone)
                            .map(|info| (
                                info.project_path.as_ref()
                                    .and_then(|pid| project_manager.get(pid).map(|p| p.name.clone())),
                                Some(info.agent_name.clone()),
                            ))
                            .unwrap_or((None, None));
                        pending.insert(
                            session_id_clone.clone(),
                            PendingControlInfo {
                                kind: PendingKind::Question {
                                    tool_use_id: tool_use_id.clone(),
                                    request_id: request_id.clone(),
                                    questions: questions.clone(),
                                },
                                project_name,
                                agent_name,
                                ts: AgentEvent::now(),
                            },
                        );
                        if let Some(mut info) = sessions.get_mut(&session_id_clone) {
                            info.status = SessionStatus::WaitingApproval;
                        }
                        event_bus.emit(AgentEvent::SessionStatus {
                            session_id: session_id_clone.clone(),
                            status: SessionStatus::WaitingApproval,
                            ts: AgentEvent::now(),
                        });
                    }
                    AgentEvent::SessionComplete { exit_code, .. } => {
                        let status = if *exit_code == 0 { SessionStatus::Complete } else { SessionStatus::Error };
                        tracing::debug!(session = %session_id_clone, exit_code = exit_code, status = ?status, "Session complete event");
                        if let Some(mut info) = sessions.get_mut(&session_id_clone) {
                            info.status = status.clone();
                        }
                        // Persist status change to database
                        if let Ok(Some(mut record)) = store.get(&session_id_clone.0) {
                            record.status = match status {
                                SessionStatus::Complete => "complete".to_string(),
                                SessionStatus::Error => "error".to_string(),
                                _ => "stopped".to_string(),
                            };
                            record.update_time = SessionRecord::now_millis();
                            let _ = store.put(&session_id_clone.0, &record);
                        }
                        event_bus.emit(AgentEvent::SessionStatus {
                            session_id: session_id_clone.clone(),
                            status,
                            ts: AgentEvent::now(),
                        });
                        pending.remove(&session_id_clone);
                    }
                    AgentEvent::SessionStatus { status, .. } => {
                        tracing::debug!(session = %session_id_clone, status = ?status, "Session status event from adapter");
                        if let Some(mut info) = sessions.get_mut(&session_id_clone) {
                            info.status = status.clone();
                        }
                        // 轮次型状态翻转（smartcode done→complete / 消息→running）
                        // 也要落库，否则重启后读到旧状态：complete 的会话不会被
                        // restore_sessions 捞回内存，用户发消息走正常的 resume 路径。
                        if let Ok(Some(mut record)) = store.get(&session_id_clone.0) {
                            record.status = match status {
                                SessionStatus::Complete => "complete".to_string(),
                                SessionStatus::Error => "error".to_string(),
                                SessionStatus::WaitingApproval => "waiting_approval".to_string(),
                                SessionStatus::Stopped => "stopped".to_string(),
                                SessionStatus::Starting | SessionStatus::Restarting => "running".to_string(),
                                SessionStatus::Running => "running".to_string(),
                            };
                            // 会话恢复非 error 状态（重新发消息/重启进程）即清掉
                            // 上一次的崩溃原因，避免前端横幅残留过期错误。
                            if !matches!(status, SessionStatus::Error) {
                                record.last_error = None;
                            }
                            record.update_time = SessionRecord::now_millis();
                            let _ = store.put(&session_id_clone.0, &record);
                        }
                        pending.remove(&session_id_clone);
                    }
                    AgentEvent::Error { message, .. } => {
                        tracing::debug!(session = %session_id_clone, error = %message, "Session error event");
                        if let Some(mut info) = sessions.get_mut(&session_id_clone) {
                            info.status = SessionStatus::Error;
                        }
                        // 失败原因随 status=error 落盘：对话历史读的是 agent 自己的
                        // JSONL，平台内部的错误提示不在里面，不落库刷新就丢了。
                        // 落库前过保密改写——record 会原样进 API 响应。
                        if let Ok(Some(mut record)) = store.get(&session_id_clone.0) {
                            record.status = "error".to_string();
                            record.last_error = Some(crate::sanitize::sanitize_public_error(message));
                            record.update_time = SessionRecord::now_millis();
                            let _ = store.put(&session_id_clone.0, &record);
                        }
                        pending.remove(&session_id_clone);
                    }
                    AgentEvent::ConfigChanged { model, .. } => {
                        // smartcode 热切换回执（api_key 不回传）。model 落盘，
                        // restore/resume 后据此补发 set_config，进程重启不丢切换。
                        if let Some(m) = model {
                            if let Ok(Some(mut record)) = store.get(&session_id_clone.0) {
                                record.model = Some(m.clone());
                                match store.put(&session_id_clone.0, &record) {
                                    Ok(()) => tracing::info!(session = %session_id_clone, model = %m, "model switch persisted"),
                                    Err(e) => tracing::warn!(session = %session_id_clone, error = %e, "persist model failed"),
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// Send a message to a session. If the session exists in redb but the
    /// Claude process is not running (detached or server restart), check the
    /// Claude session file at ~/.claude/sessions/<pid>.json. If the process
    /// is still alive, auto-restore to re-attach; if not, start a new session.
    pub async fn send_message(&self, session_id: &SessionId, message: &str) -> Result<()> {
        // Check if session is in memory AND has an active Claude process
        let need_restore = self.needs_restore(session_id);

        if need_restore {
            self.do_restore_or_create(session_id).await?;
        }

        // Forward the message; on stale process handle, restore/create and retry once
        let result = match self.try_send_message(session_id, message).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    session = %session_id,
                    error = %e,
                    "Message send failed, attempting restore and retry"
                );
                self.do_restore_or_create(session_id).await?;
                self.try_send_message(session_id, message).await
            }
        };

        // The list UI tails `prompt` — keep it pointing at the latest user
        // message instead of the initial one from session creation.
        if result.is_ok() {
            // A genuine user message (anything but the auto /millet-msg
            // payload) re-arms idle notification: the user is clearly back.
            // The auto notification itself must not clear the flag — that
            // would loop-notify every threshold period.
            if !message.trim_start().starts_with("/millet-msg") {
                self.idle_notified.remove(session_id);
            }
            self.last_activity.insert(session_id.clone(), SessionRecord::now_millis());
            if let Ok(Some(mut record)) = self.store.get(&session_id.0) {
                if should_update_prompt_tail(message) {
                    record.prompt = message.chars().take(200).collect::<String>();
                }
                record.update_time = SessionRecord::now_millis();
                if let Err(e) = self.store.put(&session_id.0, &record) {
                    tracing::warn!(session = %session_id, error = %e, "Failed to update session prompt tail");
                }
            }
        }

        result
    }

    fn needs_restore(&self, session_id: &SessionId) -> bool {
        if let Some(session) = self.sessions.get(session_id) {
            let adapter_id = session.agent_id.clone();
            if let Some(adapter) = self.registry.get(&adapter_id) {
                if adapter.has_process(session_id) {
                    tracing::debug!(session = %session_id, "needs_restore: false (process alive)");
                    return false;
                }
                tracing::debug!(session = %session_id, status = ?session.status, "needs_restore: true (no process in adapter)");
                return true;
            }
        }
        tracing::debug!(session = %session_id, "needs_restore: true (session not in memory)");
        true
    }

    async fn do_restore_or_create(&self, session_id: &SessionId) -> Result<()> {
        // session_id.0 is composite key, direct O(1) lookup
        let record = self
            .get_session_by_key(&session_id.0)?
            .ok_or_else(|| anyhow!("Session not found: {}", session_id))?;

        let is_remote = self.remote_keywords().contains(&record.keyword);
        let process_alive = record.pid.map(|pid| self.is_process_alive(is_remote, pid, session_id)).unwrap_or(false);

        if process_alive {
            tracing::info!(
                session = %session_id,
                pid = ?record.pid,
                "Process alive, auto-restoring session"
            );
            self.restore_session(session_id).await?;
        } else {
            tracing::info!(
                session = %session_id,
                pid = ?record.pid,
                "Process not alive, starting new session"
            );
            self.create_session_from_record(&record).await?;
        }
        Ok(())
    }

    async fn try_send_message(&self, session_id: &SessionId, message: &str) -> Result<()> {
        let agent_id = {
            let session = self
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("Session not found after restore: {}", session_id))?;
            session.agent_id.clone()
        }; // DashMap Ref dropped here

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter for agent: {}", agent_id))?;

        let handle = SessionHandle::new(
            session_id.clone(),
            agent_id,
            crate::agents::SessionHandleState::Subprocess { pid: 0 },
        );

        tracing::info!(
            session = %session_id,
            content_preview = message.chars().take(120).collect::<String>(),
            content_len = message.len(),
            "User message sent"
        );

        adapter.send_message(&handle, message).await
    }

    /// Handle an approval response
    pub async fn handle_approval(
        &self,
        session_id: &SessionId,
        request_id: Option<&str>,
        response: ApprovalResponse,
    ) -> Result<()> {
        let agent_id = {
            let session = self
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("Session not found: {}", session_id))?;
            session.agent_id.clone()
        }; // DashMap Ref dropped here

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter for agent: {}", agent_id))?;

        let handle = SessionHandle::new(
            session_id.clone(),
            agent_id,
            crate::agents::SessionHandleState::Subprocess { pid: 0 },
        );

        let denied = response.selected.to_lowercase().contains("deny");
        let comment = response.comment.clone();
        adapter.handle_approval(&handle, request_id, response).await?;
        let removed = self.pending_controls.remove(session_id);
        self.audit_decision(
            session_id,
            removed,
            request_id.unwrap_or(""),
            if denied { "denied" } else { "approved" },
            comment,
            None,
        );

        if let Some(mut info) = self.sessions.get_mut(session_id) {
            info.status = SessionStatus::Running;
        }

        self.event_bus.emit(AgentEvent::ApprovalResolved {
            session_id: session_id.clone(),
            ts: AgentEvent::now(),
        });

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(())
    }

    /// Answer an AskUserQuestion from the agent
    pub async fn answer_question(
        &self,
        session_id: &SessionId,
        request_id: &str,
        answers: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let agent_id = {
            let session = self
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("Session not found: {}", session_id))?
            ; // DashMap Ref dropped here
            session.agent_id.clone()
        };

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter for agent: {}", agent_id))?;

        let handle = SessionHandle::new(
            session_id.clone(),
            agent_id,
            crate::agents::SessionHandleState::Subprocess { pid: 0 },
        );

        adapter.answer_question(&handle, request_id, answers).await?;
        let removed = self.pending_controls.remove(session_id);
        let summary = serde_json::to_string(answers).unwrap_or_default();
        self.audit_decision(session_id, removed, request_id, "answered", None, Some(summary));

        self.event_bus.emit(AgentEvent::ApprovalResolved {
            session_id: session_id.clone(),
            ts: AgentEvent::now(),
        });

        Ok(())
    }

    /// Persist one human decision to the audit trail (approvals.redb), then
    /// enforce retention. Fail-soft: audit problems are logged, never propagated
    /// — a decision that already reached the agent must still complete.
    fn audit_decision(
        &self,
        session_id: &SessionId,
        removed: Option<(SessionId, PendingControlInfo)>,
        request_id_fallback: &str,
        decision: &str,
        comment: Option<String>,
        answers_summary: Option<String>,
    ) {
        let Some((_, info)) = removed else { return };
        let (kind, tool_name, summary, request_id) = match info.kind {
            PendingKind::Approval { prompt, tool_name, request_id, .. } => (
                "approval",
                tool_name,
                prompt,
                request_id.unwrap_or_else(|| request_id_fallback.to_string()),
            ),
            PendingKind::Question { request_id, .. } => (
                "question",
                Some("AskUserQuestion".to_string()),
                answers_summary.unwrap_or_default(),
                request_id,
            ),
        };
        let rec = DecisionRecord {
            request_id,
            session_id: session_id.to_string(),
            project_name: info.project_name,
            agent_name: info.agent_name,
            kind: kind.to_string(),
            tool_name,
            summary,
            decision: decision.to_string(),
            comment,
            created_at: info.ts,
            decided_at: SessionRecord::now_millis(),
        };
        if let Err(e) = self.approval_store.record(&rec) {
            tracing::warn!(session = %session_id, "Failed to persist decision to audit trail: {}", e);
            return;
        }
        // Opportunistic retention sweep: an O(1) length check, so this is
        // effectively free per decision.
        let count = self.config.config().permissions.approval_history_count;
        if count > 0 {
            if let Err(e) = self.approval_store.enforce_retention(count) {
                tracing::warn!("Approval history retention sweep failed: {}", e);
            }
        }
    }

    /// Abort the current turn without killing the session process
    pub async fn interrupt_session(&self, session_id: &SessionId) -> Result<()> {
        let agent_id = {
            let session = self
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("Session not found: {}", session_id))?
            ; // DashMap Ref dropped here
            session.agent_id.clone()
        };

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter for agent: {}", agent_id))?;

        let handle = SessionHandle::new(
            session_id.clone(),
            agent_id,
            crate::agents::SessionHandleState::Subprocess { pid: 0 },
        );

        adapter.interrupt(&handle).await
    }

    /// Hot-switch runtime config (smartcode `set_config`). Success = the line
    /// was written to stdin; the authoritative confirmation is the
    /// config_changed event that arrives on stdout afterwards.
    pub async fn set_session_config(
        &self,
        session_id: &SessionId,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
        provider: Option<&str>,
    ) -> Result<()> {
        let agent_id = {
            let session = self
                .sessions
                .get(session_id)
                .ok_or_else(|| anyhow!("Session not found: {}", session_id))?;
            session.agent_id.clone()
        };

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter for agent: {}", agent_id))?;

        let handle = SessionHandle::new(
            session_id.clone(),
            agent_id,
            crate::agents::SessionHandleState::Subprocess { pid: 0 },
        );

        adapter.set_config(&handle, model, base_url, api_key, provider).await
    }

    /// Stop a session, returns the claude_session_id (UUID) if available
    /// If force=true, send SIGKILL immediately instead of SIGTERM
    pub async fn stop_session(&self, session_id: &SessionId, force: bool) -> Result<Option<String>> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow!("Session not found: {}", session_id))?;

        let claude_session_id = session.claude_session_id.clone();
        let agent_id = session.agent_id.clone();

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow!("No adapter for agent: {}", agent_id))?;

        let handle = SessionHandle::new(
            session_id.clone(),
            agent_id,
            crate::agents::SessionHandleState::Subprocess { pid: 0 },
        );

        // Drop the DashMap Ref BEFORE the await to prevent holding read lock across await
        drop(session);

        adapter.stop_session(&handle, force).await?;
        self.pending_controls.remove(session_id);

        if let Some(mut info) = self.sessions.get_mut(session_id) {
            info.status = SessionStatus::Stopped;
        }

        // Persist status change to database
        if let Ok(Some(mut record)) = self.store.get(&session_id.0) {
            record.status = "stopped".to_string();
            record.update_time = SessionRecord::now_millis();
            let _ = self.store.put(&session_id.0, &record);
        }

        tracing::debug!(session = %session_id, claude_session_id = ?claude_session_id, force = force, "stop_session completed");
        Ok(claude_session_id)
    }

    /// List all active sessions (in-memory only, no output)
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|e| e.value().clone()).collect()
    }

    /// List all sessions from redb (including completed/historical)
    pub fn list_all_sessions(&self) -> Vec<SessionRecord> {
        let remote_keywords = self.remote_keywords();
        match self.store.list() {
            Ok(entries) => entries.into_iter().map(|(_key, mut r)| {
                // For running/waiting_approval sessions, check if the process is still alive
                let needs_check = r.status == "running" || r.status == "waiting_approval";
                if needs_check {
                    if let Some(pid) = r.pid {
                        let is_remote = remote_keywords.contains(&r.keyword);
                        let session_id = SessionId(_key.clone());
                        if !self.is_process_alive(is_remote, pid, &session_id) {
                            r.status = "stopped".to_string();
                        }
                    }
                }
                r
            }).collect(),
            Err(e) => {
                tracing::error!("Failed to list sessions from store: {}", e);
                Vec::new()
            }
        }
    }

    /// Sessions whose agent process is alive but has had no activity for at
    /// least the configured idle threshold (`[project.sessions]`
    /// idle_reminder_enabled / idle_seconds). Only the composite session id
    /// and the idle duration are exposed — no pid/process details.
    ///
    /// Computed on demand; the frontend's polling cadence is the scan cadence.
    pub fn idle_sessions(&self) -> Vec<IdleSessionInfo> {
        let cfg = self.config.config();
        if !cfg.project.sessions.idle_reminder_enabled {
            return Vec::new();
        }
        let threshold_ms = cfg.project.sessions.idle_seconds.saturating_mul(1_000);
        let now = SessionRecord::now_millis();
        let remote_keywords = self.remote_keywords();

        let mut idles: Vec<IdleSessionInfo> = Vec::new();
        let Ok(entries) = self.store.list() else {
            return idles;
        };
        for (key, record) in entries {
            if !is_idle_candidate_status(&record.status) {
                continue;
            }
            let Some(pid) = record.pid else { continue };
            let is_remote = remote_keywords.contains(&record.keyword);
            let session_id = SessionId(key.clone());
            if !self.is_process_alive(is_remote, pid, &session_id) {
                continue;
            }
            let last = self
                .last_activity
                .get(&session_id)
                .map(|e| *e)
                .unwrap_or(record.update_time);
            if is_idle_threshold_met(last, now, threshold_ms) {
                idles.push(IdleSessionInfo {
                    id: key,
                    idle_seconds: now.saturating_sub(last) / 1_000,
                });
            }
        }
        idles.sort_by(|a, b| b.idle_seconds.cmp(&a.idle_seconds).then_with(|| a.id.cmp(&b.id)));
        idles
    }

    /// Remember the latest WS Origin seen for a project key. Later connections
    /// simply overwrite — the most recent browsing origin is the most likely
    /// one the user is reachable at.
    pub fn remember_ws_origin(&self, project_key: &str, origin: String) {
        self.ws_origins.insert(project_key.to_string(), origin);
    }

    /// Persist the idle-notification bell flag for a session. Returns false
    /// when the key does not exist. Deliberately does NOT touch the
    /// idle_notified guard: only a genuine user message re-arms notification.
    pub fn set_idle_notify_enabled(&self, key: &str, enabled: bool) -> Result<bool> {
        let Some(mut record) = self.store.get(key)? else {
            return Ok(false);
        };
        record.idle_notify_enabled = enabled;
        self.store.put(key, &record)?;
        Ok(true)
    }

    /// Backend-driven idle notification: detects idle sessions server-side so
    /// it keeps working with the browser tab closed or frozen (the whole point
    /// vs. the previous frontend polling). Edge-triggered — one notification
    /// per idle episode, re-armed only by a genuine user message.
    pub fn spawn_idle_notifier(self: &Arc<Self>) {
        let mgr = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                mgr.idle_notify_tick().await;
            }
        });
    }

    async fn idle_notify_tick(&self) {
        let cfg = self.config.config();
        if !cfg.project.sessions.idle_reminder_enabled {
            return;
        }
        let threshold_ms = cfg.project.sessions.idle_seconds.saturating_mul(1_000);
        let now = SessionRecord::now_millis();
        let remote_keywords = self.remote_keywords();
        let Ok(entries) = self.store.list() else {
            return;
        };
        for (key, record) in entries {
            if !record.idle_notify_enabled {
                continue;
            }
            if !is_idle_candidate_status(&record.status) {
                continue;
            }
            let Some(pid) = record.pid else { continue };
            let session_id = SessionId(key.clone());
            if !self.is_process_alive(remote_keywords.contains(&record.keyword), pid, &session_id) {
                continue;
            }
            if self.idle_notified.contains_key(&session_id) {
                continue;
            }
            let last = self
                .last_activity
                .get(&session_id)
                .map(|e| *e)
                .unwrap_or(record.update_time);
            if !is_idle_threshold_met(last, now, threshold_ms) {
                continue;
            }

            let session_name = key.rsplit('/').next().unwrap_or(&key).to_string();
            let project_key = key
                .rsplit_once('/')
                .map(|(p, _)| p)
                .unwrap_or(key.as_str())
                .to_string();
            let origin = self
                .ws_origins
                .get(&project_key)
                .map(|e| e.value().clone())
                .unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.web.advertised_port()));
            let url = format!("{}/sessions/{}", origin, key);
            let content = format!(
                "/millet-msg 请通知我，当前会话： [{}]({}) 已闲置.",
                session_name, url
            );

            tracing::info!(session = %key, idle_seconds = now.saturating_sub(last) / 1_000, "Idle threshold met, sending backend notification");
            // Insert before the async send: the send's own events bump
            // last_activity but must not trigger a second notify.
            self.idle_notified.insert(session_id.clone(), ());
            if let Err(e) = self.send_message(&session_id, &content).await {
                tracing::warn!(session = %key, error = %e, "Idle notification send failed; will retry next tick");
                self.idle_notified.remove(&session_id);
            }
        }
    }

    /// List sessions for a specific project (in-memory only, no output)
    pub fn list_sessions_by_project(&self, project_path: &ProjectPath) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .filter(|e| e.value().project_path.as_ref() == Some(project_path))
            .map(|e| e.value().clone())
            .collect()
    }

    /// List all sessions (including historical) for a project by project_id ("keyword/name").
    /// Reads from redb so it returns sessions that aren't currently running.
    pub fn list_all_sessions_by_project_id(&self, project_id: &str) -> Vec<SessionRecord> {
        let remote_keywords = self.remote_keywords();
        match self.store.list() {
            Ok(entries) => entries
                .into_iter()
                .filter(|(_key, r)| r.project_id.as_deref() == Some(project_id))
                .map(|(_key, mut r)| {
                    let needs_check = r.status == "running" || r.status == "waiting_approval";
                    if needs_check {
                        if let Some(pid) = r.pid {
                            let is_remote = remote_keywords.contains(&r.keyword);
                            let session_id = SessionId(_key.clone());
                            if !self.is_process_alive(is_remote, pid, &session_id) {
                                r.status = "stopped".to_string();
                            }
                        }
                    }
                    r
                })
                .collect(),
            Err(e) => {
                tracing::error!("Failed to list sessions for project {}: {}", project_id, e);
                Vec::new()
            }
        }
    }

    /// Get a session by ID (in-memory, no output)
    pub fn get_session(&self, session_id: &SessionId) -> Option<SessionInfo> {
        self.sessions.get(session_id).map(|e| e.value().clone())
    }

    /// Get session from store by composite key (direct O(1) lookup)
    pub fn get_session_by_key(&self, key: &str) -> Result<Option<SessionRecord>> {
        match self.store.get(key) {
            Ok(Some(mut record)) => {
                // Verify process liveness, same as list_all_sessions
                let needs_check = record.status == "running" || record.status == "waiting_approval";
                if needs_check {
                    if let Some(pid) = record.pid {
                        let is_remote = self.remote_keywords().contains(&record.keyword);
                        let session_id = SessionId(key.to_string());
                        if !self.is_process_alive(is_remote, pid, &session_id) {
                            record.status = "stopped".to_string();
                        }
                    }
                }
                Ok(Some(record))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::error!("Failed to get session from store by key: {}", e);
                Ok(None)
            }
        }
    }

    /// Read conversation history from Claude's JSONL file with pagination.
    /// Returns messages newest-first (most recent at index 0).
    /// `session_id` is the composite key; we look up claude_session_id (UUID) from redb
    /// because Claude CLI stores JSONL files by UUID.
    pub async fn read_session_history(
        &self,
        session_id: &str,
        project_path: Option<&ProjectPath>,
        offset: usize,
        limit: usize,
    ) -> Result<HistoryResponse> {
        // Strip the "keyword:" prefix so the JSONL slug matches what Claude CLI
        // actually writes (~/.claude/projects/{abs_path_slug}/). Same convention
        // as create_session_inner / debug.rs get_session_info.
        let working_dir = project_path
            .map(|pp| pp.filesystem_path())
            .ok_or_else(|| anyhow!("Project path not found"))?;

        // Look up claude_session_id (UUID) from redb for JSONL path
        let record = self.get_session_by_key(session_id).ok().flatten();
        let uuid_for_history = record
            .as_ref()
            .and_then(|r| r.claude_session_id.clone())
            .unwrap_or_else(|| session_id.to_string());

        // Remote (SSH) projects store their JSONL on the remote host, keyed by a
        // slug of the remote ABSOLUTE path. Read it over SFTP — the local lookup
        // below would miss because working_dir is "~/..." and the file lives on
        // the remote machine (e.g. /Users/dev/.claude/projects/-Users-...).
        let is_remote = project_path
            .and_then(|pp| self.project_manager.get(pp))
            .map(|p| p.remote_mode == "ssh")
            .unwrap_or(false);

        // smartcode 的 claude_session_id 槽位存的是它自报的会话 JSONL 绝对路径
        // （stderr `session: <path>`），直接按路径读，不走 claude 的 slug 拼接。
        let agent_id = record.as_ref().map(|r| r.agent_id.clone()).unwrap_or_default();
        if agent_id == "smartcode" {
            let millet_path = record
                .as_ref()
                .and_then(|r| r.claude_session_id.clone())
                .unwrap_or_else(|| uuid_for_history.clone());
            // 「当前模型」以 config_changed 回执持久化的 record.model 为准（切换后
            // 即时生效；meta 行只是该 turn 当时的模型，切换后可能滞后）。上游纯对话
            // turn 不落 meta（smartcode#7），record 缺失时再回退 meta。
            let mut resp = if is_remote {
                self.read_remote_smartcode_history(project_path, &millet_path, offset, limit)
                    .await?
            } else {
                read_smartcode_session_history(&millet_path, offset, limit)?
            };
            if let Some(m) = record.as_ref().and_then(|r| r.model.clone()) {
                resp.model = Some(m);
            }
            return Ok(resp);
        }

        // codex 的 claude_session_id 槽位存的是 rollout JSONL 绝对路径
        // （thread/started 自报），直接按路径读。
        if agent_id == "codex" {
            let rollout_path = record
                .as_ref()
                .and_then(|r| r.claude_session_id.clone())
                .unwrap_or_default();
            let mut resp = if is_remote {
                self.read_remote_codex_history(project_path, &rollout_path, offset, limit)
                    .await?
            } else {
                read_codex_session_history(&rollout_path, offset, limit)?
            };
            // record.model（config_changed 持久化）优先于 rollout turn_context
            if let Some(m) = record.as_ref().and_then(|r| r.model.clone()) {
                resp.model = Some(m);
            }
            return Ok(resp);
        }

        if is_remote {
            let agent_id = record.as_ref().map(|r| r.agent_id.clone()).unwrap_or_default();
            return self
                .read_remote_jsonl_history(project_path, &agent_id, &uuid_for_history, offset, limit)
                .await;
        }

        read_jsonl_history(&uuid_for_history, &working_dir, offset, limit)
    }

    /// Read conversation history from a remote host over SFTP.
    ///
    /// Claude Code stores JSONL at ~/.claude/projects/{abs_path_slug}/{uuid}.jsonl
    /// where the slug is the remote absolute path with '/' -> '-'. SFTP resolves
    /// relative paths against the remote user's home directory, so we enumerate
    /// `.claude/projects` (home-relative) and search each slug directory for the
    /// session's {uuid}.jsonl — this avoids needing to know the remote home up front.
    async fn read_remote_jsonl_history(
        &self,
        project_path: Option<&ProjectPath>,
        agent_id: &str,
        session_uuid: &str,
        offset: usize,
        limit: usize,
    ) -> Result<HistoryResponse> {
        let ctx = self.build_runtime_context(project_path, agent_id);
        let ssh_runtime = SshRuntime::connect(&ctx).await?;
        let handle = ssh_runtime.shared_handle();
        let mut sftp = SftpClient::new(&handle).await?;

        let projects_root = ".claude/projects";
        let entries = match sftp.list_dir(projects_root).await {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, %session_uuid, "remote JSONL: {projects_root} unreadable on remote host");
                return Ok(HistoryResponse { messages: Vec::new(), has_more: false, model: None });
            }
        };

        for entry in &entries {
            let candidate = format!("{projects_root}/{entry}/{session_uuid}.jsonl");
            if sftp.exists(&candidate).await? {
                let content = sftp.read_to_string(&candidate).await?;
                tracing::info!(path = %candidate, %session_uuid, "read remote session history over SFTP");
                return parse_jsonl_history(&content, offset, limit);
            }
        }

        tracing::debug!(%session_uuid, "no remote JSONL under {projects_root}");
        Ok(HistoryResponse { messages: Vec::new(), has_more: false, model: None })
    }

    /// Read a smartcode session JSONL from a remote host over SFTP. The stored
    /// claude_session_id is the ABSOLUTE path smartcode reported on stderr
    /// ($HOME-expanded on the remote), so a direct read suffices.
    async fn read_remote_smartcode_history(
        &self,
        project_path: Option<&ProjectPath>,
        session_path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<HistoryResponse> {
        let ctx = self.build_runtime_context(project_path, "smartcode");
        let ssh_runtime = SshRuntime::connect(&ctx).await?;
        let handle = ssh_runtime.shared_handle();
        let mut sftp = SftpClient::new(&handle).await?;

        match sftp.read_to_string(session_path).await {
            Ok(content) => {
                tracing::info!(path = %session_path, "read remote millet session history over SFTP");
                parse_smartcode_history(&content, offset, limit)
            }
            Err(e) => {
                tracing::debug!(path = %session_path, error = %e, "remote millet JSONL unreadable");
                Ok(HistoryResponse { messages: Vec::new(), has_more: false, model: None })
            }
        }
    }

    /// Read a codex rollout JSONL from a remote host over SFTP. The stored
    /// claude_session_id is the ABSOLUTE rollout path codex reported via
    /// thread/started ($HOME-expanded on the remote), so a direct read suffices.
    async fn read_remote_codex_history(
        &self,
        project_path: Option<&ProjectPath>,
        session_path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<HistoryResponse> {
        let ctx = self.build_runtime_context(project_path, "codex");
        let ssh_runtime = SshRuntime::connect(&ctx).await?;
        let handle = ssh_runtime.shared_handle();
        let mut sftp = SftpClient::new(&handle).await?;

        match sftp.read_to_string(session_path).await {
            Ok(content) => {
                tracing::info!(path = %session_path, "read remote codex rollout history over SFTP");
                parse_codex_history(&content, offset, limit)
            }
            Err(e) => {
                tracing::debug!(path = %session_path, error = %e, "remote codex rollout unreadable");
                Ok(HistoryResponse { messages: Vec::new(), has_more: false, model: None })
            }
        }
    }

    /// Restore sessions from store on startup — loads metadata only (no output buffer)
    /// for sessions that were running/waiting. Does NOT re-spawn processes.
    pub fn restore_sessions(&self) {
        match self.store.list() {
            Ok(entries) => {
                for (key, record) in entries {
                    if record.status != "running" && record.status != "waiting_approval" {
                        continue;
                    }
                    // key is composite key, record.session_id is also composite key
                    // project_id may be "keyword/name" (new records) or a raw
                    // ProjectPath string (legacy records) — resolve both so the
                    // restored session is bound to the correct project.
                    let project_path = record
                        .project_id
                        .as_deref()
                        .and_then(|pid| self.project_path_from_record(pid));
                    let session_info = SessionInfo {
                        id: SessionId(key.clone()),
                        claude_session_id: record.claude_session_id.clone(),
                        project_path,
                        agent_id: record.agent_id,
                        agent_name: record.agent_name,
                        status: SessionStatus::Stopped,
                        created_at: record.created_at,
                        prompt: record.prompt,
                    };
                    self.sessions.insert(SessionId(key.clone()), session_info);

                    tracing::info!(session = %key, "Restored session from store");
                }
            }
            Err(e) => {
                tracing::error!("Failed to restore sessions from store: {}", e);
            }
        }
    }

    /// List all pending control requests (approvals + questions) across
    /// sessions, for the message-panel bootstrap endpoint.
    pub fn list_pending_controls(&self) -> Vec<(SessionId, PendingControlInfo)> {
        self.pending_controls
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    /// Delete a session from the store AND in-memory state.
    /// `composite_key` is the redb key (same as SessionId.0).
    pub fn delete_session(&self, composite_key: &str) -> bool {
        let sid = SessionId(composite_key.to_string());
        // Remove from in-memory maps
        self.sessions.remove(&sid);
        self.pending_controls.remove(&sid);
        // Direct O(1) delete by composite key
        match self.store.delete(composite_key) {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::error!("Failed to delete session from store: {}", e);
                false
            }
        }
    }

    /// Full delete: stop session, remove from store + memory, and clean up local agent data
    /// (e.g. delete ~/.claude/projects/<path>/<uuid>.jsonl for Claude Code sessions).
    /// `composite_key` is the redb key (same as SessionId.0).
    pub async fn full_delete_session(&self, composite_key: &str) -> Result<()> {
        let sid = SessionId(composite_key.to_string());

        // Try to stop the process if it's running in memory (ignore error if not found)
        let _ = self.stop_session(&sid, false).await;

        // Look up record from redb BEFORE deleting — need claude_session_id for cleanup
        let record = self.get_session_by_key(composite_key)?;

        // Delete from redb and in-memory state
        self.delete_session(composite_key);

        // Clean up local agent session data using claude_session_id (UUID) for disk paths
        if let Some(record) = record {
            if let Some(adapter) = self.registry.get(&record.agent_id) {
                // Try new format first (project_id = "keyword/name"), then fall back to
                // legacy format (project_id = "keyword:/abs/path") for disk cleanup.
                let working_dir = record.project_id.as_deref()
                    .and_then(|pid| {
                        self.project_path_from_record(pid)
                            .map(|pp| pp.filesystem_path())
                            .or_else(|| Self::filesystem_path_from_legacy_record(pid))
                    });
                // Use claude_session_id (UUID) for disk file cleanup, fallback to composite_key
                let cleanup_id = record.claude_session_id.as_deref().unwrap_or(composite_key);
                adapter.cleanup_session(cleanup_id, working_dir.as_deref()).await;
            }
        }

        Ok(())
    }

    /// Kill a still-running previous agent process before its replacement
    /// spawns. Without this, restoring over a live-but-unmanaged process
    /// (typical after a console restart) orphans the old one — it keeps
    /// running with the same claude session id and nobody holds its handle
    /// (the 61-duplicate `claude --resume` leak).
    /// Local: SIGKILL by recorded pid, guarded by a cmdline check so a reused
    /// pid with a stale claude pidfile can't get an innocent process killed.
    /// Remote: the recorded pid is a channel id, meaningless after restart —
    /// best-effort `pkill -f <uuid>` instead (every spawned claude carries the
    /// uuid on its command line via --session-id/--resume).
    async fn reclaim_stale_process(
        &self,
        adapter: &std::sync::Arc<dyn crate::agents::AgentAdapter>,
        record: &SessionRecord,
        project_path: &ProjectPath,
        claude_session_id: &str,
        session_id: &SessionId,
    ) {
        // 旧进程若是本实例管理的（adapter 里还有 state、wait 任务在跑），
        // 杀它会被误报成崩溃——动手前置位抑制标记。
        adapter.suppress_crash_report(session_id);
        let is_remote = self.remote_keywords().contains(&record.keyword);
        if is_remote {
            let ctx = self.build_runtime_context(Some(project_path), "");
            match SshRuntime::connect(&ctx).await {
                Ok(rt) => {
                    let uuid_esc = crate::runtime::ssh::shell_escape(claude_session_id);
                    let cmd = format!(
                        "pkill -f -- {uuid} 2>/dev/null; sleep 1; \
                         rm -f ~/.claude/sessions/{uuid}/lock 2>/dev/null; true",
                        uuid = uuid_esc
                    );
                    if let Err(e) =
                        crate::runtime::ssh::exec_one_shot(&rt.shared_handle(), &cmd).await
                    {
                        tracing::warn!(session = %session_id, error = %e, "Remote reclaim of stale agent process failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(session = %session_id, error = %e, "Remote reclaim skipped: SSH connect failed");
                }
            }
        } else if let Some(pid) = record.pid {
            if self.is_process_alive(false, pid, session_id)
                && local_reclaim_target_matches(record, pid)
            {
                tracing::info!(session = %session_id, pid, "Reclaiming stale agent process before restore");
                if kill_local_pid_and_wait(pid, std::time::Duration::from_secs(2)).await {
                    crate::agents::ClaudeAdapter::remove_claude_session_lock(claude_session_id);
                }
            }
        }
    }

    /// Restore a previously closed session by spawning Claude with --resume <claude_session_id>.
    /// Claude resumes conversation history from the JSONL file.
    /// （restore/resume 的模型注入已下沉到 adapter 启动参数：trait 传入 record.model，
    /// smartcode 在 spawn 时转成 --model，首轮请求即用正确模型。）

    pub async fn restore_session(&self, session_id: &SessionId) -> Result<()> {
        // Guard: if session is already running with a live process, skip.
        if let Some(info) = self.sessions.get(session_id) {
            let status_active = matches!(
                info.status,
                SessionStatus::Running | SessionStatus::Starting | SessionStatus::WaitingApproval
            );
            if status_active {
                let adapter = self.registry.get(&info.agent_id);
                let process_alive = adapter.map(|a| a.has_process(session_id)).unwrap_or(false);
                if process_alive {
                    tracing::info!(session = %session_id, status = ?info.status, "Session already active, skipping restore");
                    return Ok(());
                }
                tracing::info!(session = %session_id, status = ?info.status, "Session status is active but process is dead, forcing restore");
            }
        }

        // session_id.0 is composite key — direct O(1) lookup
        let record = self
            .get_session_by_key(&session_id.0)?
            .ok_or_else(|| anyhow::anyhow!("Session record not found: {}", session_id.0))?;

        // project_id may be "keyword/name" (new records) or the raw ProjectPath
        // string (legacy records). Resolve it to a real ProjectPath; if the project
        // no longer exists the session can't be restored — fail clearly instead of
        // falling back to a local spawn inside the agent container.
        let project_path = record.project_id.as_ref()
            .and_then(|pid| self.project_path_from_record(pid))
            .ok_or_else(|| anyhow::anyhow!(
                "无法恢复会话：找不到会话绑定的项目（{}）",
                record.project_id.as_deref().unwrap_or("")
            ))?;

        let working_dir = Some(project_path.filesystem_path());

        let config = self
            .config
            .config();
        let (agent_id, agent_config) = config
            .agent
            .find_enabled(&record.agent_id)
            .ok_or_else(|| anyhow::anyhow!("No enabled agent found with id: {}", record.agent_id))?;
        let agent_id = agent_id.clone();
        let config = agent_config.clone();

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow::anyhow!("No adapter registered for: {}", agent_id))?;

        // Use the stored claude_session_id (UUID) for resume — Claude CLI needs
        // the existing JSONL file to restore conversation history.
        let claude_session_id = record
            .claude_session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        self.reclaim_stale_process(&adapter, &record, &project_path, &claude_session_id, session_id)
            .await;

        let runtime_ctx = self.build_runtime_context(Some(&project_path), &agent_id);
        let handle = adapter.restore_session(&config, &runtime_ctx, session_id.clone(), &claude_session_id, &session_id.0, working_dir, record.model.as_deref()).await?;

        let pid = match &handle.state {
            crate::agents::SessionHandleState::Subprocess { pid } => Some(*pid),
            _ => None,
        };

        // Update redb
        let updated_record = SessionRecord {
            session_id: session_id.0.clone(),
            claude_session_id: Some(claude_session_id.clone()),
            project_id: record.project_id.clone(),
            keyword: record.keyword.clone(),
            agent_id: record.agent_id.clone(),
            agent_name: record.agent_name.clone(),
            status: "running".to_string(),
            prompt: record.prompt.clone(),
            created_at: record.created_at,
            create_time: SessionRecord::now_millis(),
            update_time: SessionRecord::now_millis(),
            pid,
            pid_started_at: pid.and_then(local_process_start_time),
            idle_notify_enabled: record.idle_notify_enabled,
            model: record.model.clone(),
            last_error: None,
        };
        let _ = self.store.put(&session_id.0, &updated_record);

        let session_info = SessionInfo {
            id: session_id.clone(),
            claude_session_id: Some(claude_session_id),
            project_path: Some(project_path),
            agent_id: record.agent_id,
            agent_name: record.agent_name,
            status: SessionStatus::Running,
            created_at: record.created_at,
            prompt: record.prompt,
        };
        self.sessions.insert(session_id.clone(), session_info);
        // The restored session replaces a dead process; anything still parked
        // in pending_controls belonged to that process and can never resolve.
        self.pending_controls.remove(session_id);

        // Set up event stream processing
        self.spawn_event_task(adapter, handle, session_id.clone());

        Ok(())
    }

    /// Create a new session from a redb record — starts a fresh Claude process
    /// with --session-id so it picks up existing conversation history.
    async fn create_session_from_record(&self, record: &SessionRecord) -> Result<()> {
        // project_id may be "keyword/name" (new records) or the raw ProjectPath
        // string (legacy records). Resolve it to a real ProjectPath; if the project
        // is gone, fail clearly instead of spawning in the local agent container.
        let project_path = record.project_id.as_ref()
            .and_then(|pid| self.project_path_from_record(pid))
            .ok_or_else(|| anyhow::anyhow!(
                "无法启动会话：找不到会话绑定的项目（{}）",
                record.project_id.as_deref().unwrap_or("")
            ))?;
        let working_dir = Some(project_path.filesystem_path());

        let config = self
            .config
            .config();
        let (agent_id, agent_config) = config
            .agent
            .find_enabled(&record.agent_id)
            .ok_or_else(|| anyhow::anyhow!("No enabled agent found with id: {}", record.agent_id))?;
        let agent_id = agent_id.clone();
        let config = agent_config.clone();

        let adapter = self
            .registry
            .get(&agent_id)
            .ok_or_else(|| anyhow::anyhow!("No adapter registered for: {}", agent_id))?;

        // session_id in record is now composite key
        let session_id = SessionId(record.session_id.clone());
        let claude_session_id = record.claude_session_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let runtime_ctx = self.build_runtime_context(Some(&project_path), &agent_id);
        // A recorded claude_session_id means the transcript already exists on
        // disk; spawning with --session-id collides with it (the CLI rejects
        // "Session ID already in use"). Resume picks up the history instead.
        let handle = if record.claude_session_id.is_some() {
            adapter.restore_session(&config, &runtime_ctx, session_id.clone(), &claude_session_id, &record.session_id, working_dir.clone(), record.model.as_deref()).await?
        } else {
            adapter.start_session(&config, &runtime_ctx, session_id.clone(), &claude_session_id, &record.session_id, &record.prompt, working_dir.clone()).await?
        };

        let pid = match &handle.state {
            crate::agents::SessionHandleState::Subprocess { pid } => Some(*pid),
            _ => None,
        };

        let session_info = SessionInfo {
            id: session_id.clone(),
            claude_session_id: Some(claude_session_id.clone()),
            project_path: Some(project_path),
            agent_id: record.agent_id.clone(),
            agent_name: record.agent_name.clone(),
            status: SessionStatus::Running,
            created_at: record.created_at,
            prompt: record.prompt.clone(),
        };
        self.sessions.insert(session_id.clone(), session_info);
        // Fresh process replaces whatever held the old prompt — same as restore.
        self.pending_controls.remove(&session_id);

        // Update redb with new pid
        let updated_record = SessionRecord {
            session_id: record.session_id.clone(),
            claude_session_id: Some(claude_session_id),
            project_id: record.project_id.clone(),
            keyword: record.keyword.clone(),
            agent_id: record.agent_id.clone(),
            agent_name: record.agent_name.clone(),
            status: "running".to_string(),
            prompt: record.prompt.clone(),
            created_at: record.created_at,
            create_time: SessionRecord::now_millis(),
            update_time: SessionRecord::now_millis(),
            pid,
            pid_started_at: pid.and_then(local_process_start_time),
            idle_notify_enabled: record.idle_notify_enabled,
            model: record.model.clone(),
            last_error: None,
        };
        let _ = self.store.put(&record.session_id, &updated_record);

        // Set up event stream processing
        self.spawn_event_task(adapter, handle, session_id.clone());

        Ok(())
    }

    /// Restart all running sessions for a given agent id after a config change.
    /// Kills old processes and resumes with `--resume <session-id>`.
    pub async fn restart_sessions_for_config(&self, agent_id: &str) -> Result<()> {
        let config = self.config.config();

        let adapter = self
            .registry
            .get(agent_id)
            .ok_or_else(|| anyhow!("No adapter registered for: {}", agent_id))?;

        let snapshot: Vec<_> = self.sessions
            .iter()
            .filter(|e| e.value().agent_id == agent_id)
            // Only live sessions get restarted — resuming Stopped ones would
            // spawn processes nobody asked for.
            .filter(|e| matches!(
                e.value().status,
                SessionStatus::Running | SessionStatus::Starting | SessionStatus::WaitingApproval
            ))
            .map(|e| {
                let info = e.value().clone();
                (e.key().clone(), info.project_path.clone(), info.agent_id.clone())
            })
            .collect();

        if snapshot.is_empty() {
            return Ok(());
        }

        tracing::info!(agent_id = %agent_id, count = snapshot.len(), "Restarting sessions for config change");

        for (session_id, project_path, agent_id) in snapshot {
            let (agent_id_ref, agent_config) = config
                .agent
                .find_enabled(&agent_id)
                .ok_or_else(|| anyhow!("No enabled agent found with id: {}", agent_id))?;
            let agent_id_ref = agent_id_ref.clone();
            let agent_config = agent_config.clone();

            let working_dir = project_path
                .as_ref()
                .map(|pp| pp.filesystem_path());

            // session_id.0 is already the composite key
            let composite_key = session_id.0.clone();
            // Continue from the stored conversation, not a fresh uuid — a random
            // uuid has no JSONL behind it and the CLI rejects the resume.
            let claude_session_id = self
                .store
                .get(&composite_key)
                .ok()
                .flatten()
                .and_then(|r| r.claude_session_id)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            // Kill the live process before replacing it — resume spawns a new
            // one and the old handle would be orphaned.
            let _ = self.stop_session(&session_id, false).await;

            let runtime_ctx = self.build_runtime_context(project_path.as_ref(), &agent_id_ref);
            let persisted_model = self.store.get(&composite_key).ok().flatten().and_then(|r| r.model);

            match adapter.resume_session(&agent_config, &runtime_ctx, session_id.clone(), &claude_session_id, &composite_key, working_dir, persisted_model.as_deref()).await {
                Ok(handle) => {
                    let pid = match &handle.state {
                        crate::agents::SessionHandleState::Subprocess { pid } => Some(*pid),
                        _ => None,
                    };
                    if let Some(mut info) = self.sessions.get_mut(&session_id) {
                        info.status = SessionStatus::Running;
                        info.claude_session_id = Some(claude_session_id.clone());
                    }
                    if let Ok(Some(mut record)) = self.store.get(&composite_key) {
                        record.status = "running".to_string();
                        record.claude_session_id = Some(claude_session_id.clone());
                        record.pid = pid;
                        record.pid_started_at = pid.and_then(local_process_start_time);
                        record.update_time = SessionRecord::now_millis();
                        let _ = self.store.put(&composite_key, &record);
                    }
                    self.spawn_event_task(adapter.clone(), handle, session_id.clone());
                }
                Err(e) => {
                    tracing::warn!(session = %session_id, "Failed to resume session: {}", e);
                    self.event_bus.emit(AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: format!("Resume failed: {}", e),
                        ts: AgentEvent::now(),
                    });
                }
            }
        }

        Ok(())
    }

    // === Project file browsing (查看文件) ===

    /// Resolve a session to its project record (path + remote mode).
    fn project_for_session(
        &self,
        session_id: &str,
    ) -> Result<(ProjectPath, crate::projects::types::Project)> {
        let pp = self
            .resolve_project_path(session_id)
            .ok_or_else(|| anyhow!("Session or project not found: {}", session_id))?;
        let project = self
            .project_manager
            .get(&pp)
            .ok_or_else(|| anyhow!("Project not found: {}", pp))?;
        Ok((pp, project))
    }

    /// List a directory in the session's project workspace.
    /// `path` may be empty (project root), absolute, `~/...`, or relative to the project root.
    pub async fn list_project_dir(&self, session_id: &str, path: Option<String>) -> Result<DirListing> {
        let (pp, project) = self.project_for_session(session_id)?;
        let working_dir = pp.filesystem_path();
        let request = path.unwrap_or_default();
        let is_remote = project.remote_mode == "ssh";

        let entries: Vec<DirEntryDto>;
        let resolved: String;
        if is_remote {
            let ctx = self.build_runtime_context(Some(&pp), "");
            let ssh_runtime = SshRuntime::connect(&ctx).await?;
            let handle = ssh_runtime.shared_handle();
            let mut sftp = SftpClient::new(&handle).await?;
            let home = sftp.home_dir().await.unwrap_or_default();
            resolved = expand_remote_user(&resolve_remote_path(&working_dir, &request)?, &home);
            let sftp_path = to_sftp_path(&resolved);
            entries = sftp.list_entries(&sftp_path).await?
                .into_iter()
                .map(|e| DirEntryDto { name: e.name, is_dir: e.is_dir, size: e.size })
                .collect();
        } else {
            let full = resolve_local_path(&working_dir, &request).await?;
            let mut reader = tokio::fs::read_dir(&full).await
                .map_err(|e| anyhow!("Cannot read directory {}: {}", full.display(), e))?;
            let mut out = Vec::new();
            while let Some(entry) = reader.next_entry().await? {
                let metadata = entry.metadata().await?;
                out.push(DirEntryDto {
                    name: entry.file_name().to_string_lossy().to_string(),
                    is_dir: metadata.is_dir(),
                    size: metadata.len(),
                });
            }
            entries = out;
            resolved = full.to_string_lossy().to_string();
        }

        let mut entries = entries;
        entries.retain(|e| e.name != "." && e.name != "..");
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

        Ok(DirListing { path: resolved, entries })
    }

    /// Read a file from the session's project workspace.
    /// Same path semantics as `list_project_dir`. Files larger than 1 MiB are
    /// truncated; binary files are detected and returned without content.
    pub async fn read_project_file(&self, session_id: &str, path: &str) -> Result<FileContentDto> {
        let (pp, project) = self.project_for_session(session_id)?;
        let working_dir = pp.filesystem_path();
        let is_remote = project.remote_mode == "ssh";

        const MAX_FILE_BYTES: usize = 1024 * 1024;
        const BINARY_SNIFF_LEN: usize = 8192;

        let (bytes, total_size, truncated, resolved) = if is_remote {
            let ctx = self.build_runtime_context(Some(&pp), "");
            let ssh_runtime = SshRuntime::connect(&ctx).await?;
            let handle = ssh_runtime.shared_handle();
            let mut sftp = SftpClient::new(&handle).await?;
            let home = sftp.home_dir().await.unwrap_or_default();
            let resolved = expand_remote_user(&resolve_remote_path(&working_dir, path)?, &home);
            let sftp_path = to_sftp_path(&resolved);
            let total_size = sftp.file_size(&sftp_path).await?;
            let (bytes, truncated) = sftp.read_capped(&sftp_path, MAX_FILE_BYTES).await?;
            (bytes, total_size, truncated, resolved)
        } else {
            let full = resolve_local_path(&working_dir, path).await?;
            let metadata = tokio::fs::metadata(&full).await
                .map_err(|e| anyhow!("Cannot stat {}: {}", full.display(), e))?;
            let total_size = metadata.len();
            let mut file = tokio::fs::File::open(&full).await
                .map_err(|e| anyhow!("Cannot open {}: {}", full.display(), e))?;
            use tokio::io::AsyncReadExt;
            let mut bytes = Vec::new();
            let mut truncated = false;
            let max = MAX_FILE_BYTES as u64;
            if metadata.len() > max {
                file.take(max).read_to_end(&mut bytes).await?;
                truncated = true;
            } else {
                file.read_to_end(&mut bytes).await?;
            }
            let resolved = full.to_string_lossy().to_string();
            (bytes, total_size, truncated, resolved)
        };

        let is_binary = bytes[..BINARY_SNIFF_LEN.min(bytes.len())].contains(&0);
        let content = if is_binary {
            None
        } else {
            Some(String::from_utf8_lossy(&bytes).to_string())
        };

        Ok(FileContentDto {
            path: resolved,
            is_binary,
            truncated,
            size: total_size,
            content,
        })
    }

    /// Read a whole file from the session's project workspace for download.
    /// Same path semantics as `read_project_file`, but returns raw bytes
    /// without the 1 MiB preview cap — a generous ceiling guards against
    /// accidental multi-GB reads.
    pub async fn download_project_file(&self, session_id: &str, path: &str) -> Result<Vec<u8>> {
        let (pp, project) = self.project_for_session(session_id)?;
        let working_dir = pp.filesystem_path();
        let is_remote = project.remote_mode == "ssh";

        const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

        if is_remote {
            let ctx = self.build_runtime_context(Some(&pp), "");
            let ssh_runtime = SshRuntime::connect(&ctx).await?;
            let handle = ssh_runtime.shared_handle();
            let mut sftp = SftpClient::new(&handle).await?;
            let home = sftp.home_dir().await.unwrap_or_default();
            let resolved = expand_remote_user(&resolve_remote_path(&working_dir, path)?, &home);
            let sftp_path = to_sftp_path(&resolved);
            let size = sftp.file_size(&sftp_path).await?;
            if size > MAX_DOWNLOAD_BYTES {
                anyhow::bail!("file too large for download: {} bytes", size);
            }
            sftp.read(&sftp_path).await
        } else {
            let full = resolve_local_path(&working_dir, path).await?;
            let metadata = tokio::fs::metadata(&full).await
                .map_err(|e| anyhow!("Cannot stat {}: {}", full.display(), e))?;
            if metadata.is_dir() {
                anyhow::bail!("Cannot download {}: it is a directory", full.display());
            }
            if metadata.len() > MAX_DOWNLOAD_BYTES {
                anyhow::bail!("file too large for download: {} bytes", metadata.len());
            }
            let mut file = tokio::fs::File::open(&full).await
                .map_err(|e| anyhow!("Cannot open {}: {}", full.display(), e))?;
            use tokio::io::AsyncReadExt;
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.read_to_end(&mut bytes).await?;
            Ok(bytes)
        }
    }
}

/// Display path of a directory listing.
#[derive(Debug, serde::Serialize)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<DirEntryDto>,
}

#[derive(Debug, serde::Serialize)]
pub struct DirEntryDto {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Content of a single file read.
#[derive(Debug, serde::Serialize)]
pub struct FileContentDto {
    pub path: String,
    pub is_binary: bool,
    pub truncated: bool,
    pub size: u64,
    pub content: Option<String>,
}

/// Resolve a user-requested path against a local project root.
/// Accepts empty (root), absolute, `~/...`, and relative paths.
async fn resolve_local_path(base: &str, request: &str) -> Result<std::path::PathBuf> {
    let request = request.trim();
    let base_path = std::path::PathBuf::from(shellexpand::tilde(base).to_string());
    let candidate: std::path::PathBuf = if request.is_empty() {
        base_path.clone()
    } else if request == "~" || request.starts_with("~/") {
        std::path::PathBuf::from(shellexpand::tilde(request).to_string())
    } else if request.starts_with('/') {
        std::path::PathBuf::from(request)
    } else {
        base_path.join(request)
    };
    // Canonicalize resolves `..`/symlinks, preventing traversal past the intent.
    let resolved = tokio::fs::canonicalize(&candidate).await
        .map_err(|e| anyhow!("Path not found {}: {}", candidate.display(), e))?;
    // 围栏：解析后的真实路径必须仍位于项目根内。绝对路径、`~` 展开、
    // `../` 穿越与指向根外的符号链接一律拒绝——文件接口只服务项目工作区。
    let root = tokio::fs::canonicalize(&base_path).await
        .map_err(|e| anyhow!("Project root not found {}: {}", base_path.display(), e))?;
    if resolved != root && !resolved.starts_with(&root) {
        return Err(anyhow!("Path escapes project directory: {}", resolved.display()));
    }
    Ok(resolved)
}

/// Resolve a user-requested path against a remote project root (POSIX, textual).
/// The remote working dir may be `~/...`; the result keeps that form and is
/// converted to SFTP home-relative form by [`to_sftp_path`].
/// 与本地版同款围栏：越出项目根的请求（绝对路径 / `~` / `../`）返回错误。
fn resolve_remote_path(working_dir: &str, request: &str) -> Result<String> {
    let request = request.trim();
    let root = normalize_posix(working_dir);
    let joined: String = if request.is_empty() || request == "~" {
        root.clone()
    } else if request.starts_with("~/") || request.starts_with('/') {
        request.to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), request)
    };
    let resolved = normalize_posix(&joined);
    if !path_within(&root, &resolved) {
        return Err(anyhow!("Path escapes project directory: {}", resolved));
    }
    Ok(resolved)
}

/// 文本级组件围栏：`p` 等于 `root` 或以 `root/` 开头才视为在根内。
fn path_within(root: &str, p: &str) -> bool {
    if p == root {
        return true;
    }
    p.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'))
}

/// Replace a leading `~` / `~/` with the remote home directory so the UI can
/// show real absolute paths. Falls back to the textual form when the home
/// directory is unknown.
fn expand_remote_user(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        return home.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return home.to_string();
        }
        return format!("{}/{}", home.trim_end_matches('/'), rest);
    }
    path.to_string()
}

/// Normalize a POSIX path textually: collapse `//`, resolve `.` and `..`.
fn normalize_posix(path: &str) -> String {
    let (prefix, rest) = if let Some(rest) = path.strip_prefix("~/") {
        ("~/", rest)
    } else if path.starts_with('/') {
        ("/", &path[1..])
    } else {
        ("", path)
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // Climb out only within the normalized portion.
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if prefix.is_empty() {
                    parts.push("..");
                }
                // With a prefix (~/ or /), climbing above the root stays at root.
            }
            p => parts.push(p),
        }
    }
    format!("{}{}", prefix, parts.join("/"))
}

/// Convert a normalized remote path to SFTP form (home-relative, no `~`).
fn to_sftp_path(path: &str) -> String {
    if path == "~" {
        ".".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() { ".".to_string() } else { rest.to_string() }
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod file_browse_tests {
    use super::*;

    #[test]
    fn normalize_posix_resolves_dots() {
        assert_eq!(normalize_posix("/a/b/../c"), "/a/c");
        assert_eq!(normalize_posix("a//b/./c"), "a/b/c");
        assert_eq!(normalize_posix("~/a/../b"), "~/b");
        assert_eq!(normalize_posix("/a/../../b"), "/b");
        assert_eq!(normalize_posix("a/../.."), "..");
        assert_eq!(normalize_posix("/.."), "/");
    }

    #[test]
    fn resolve_remote_path_joins_relative_to_working_dir() {
        assert_eq!(resolve_remote_path("~/projects/foo", "").unwrap(), "~/projects/foo");
        assert_eq!(
            resolve_remote_path("~/projects/foo", "src/main.rs").unwrap(),
            "~/projects/foo/src/main.rs"
        );
        assert_eq!(
            resolve_remote_path("/opt/ws", "sub/file.txt").unwrap(),
            "/opt/ws/sub/file.txt"
        );
        assert_eq!(
            resolve_remote_path("~/projects/foo", "~/projects/foo/lib.rs").unwrap(),
            "~/projects/foo/lib.rs"
        );
        // 根内绝对路径（与根同形）放行
        assert_eq!(resolve_remote_path("/opt/ws", "/opt/ws").unwrap(), "/opt/ws");
    }

    #[test]
    fn resolve_remote_path_rejects_escape_from_working_dir() {
        // 绝对路径直读远端任意文件（渗透测试 PoC：/etc/hosts）
        assert!(resolve_remote_path("~/projects/foo", "/etc/hosts").is_err());
        // `~` 展开跳出项目根（PoC：~/.milletcode/config.toml）；
        // 单独的 "~" 语义上是项目根本身，不是家目录
        assert!(resolve_remote_path("~/projects/foo", "~/.milletcode/config.toml").is_err());
        assert_eq!(resolve_remote_path("~/projects/foo", "~").unwrap(), "~/projects/foo");
        // 相对路径 `..` 穿越出根
        assert!(resolve_remote_path("~/projects/foo", "../bar").is_err());
        assert!(resolve_remote_path("/opt/ws", "../../etc").is_err());
        // 组件前缀相似不算根内（/opt/ws-evil 不是 /opt/ws 的子路径）
        assert!(resolve_remote_path("/opt/ws", "/opt/ws-evil/x").is_err());
    }

    #[test]
    fn path_within_checks_component_boundary() {
        assert!(path_within("/opt/ws", "/opt/ws"));
        assert!(path_within("/opt/ws", "/opt/ws/a/b"));
        assert!(path_within("~/p", "~/p/x"));
        assert!(!path_within("/opt/ws", "/opt/ws-evil"));
        assert!(!path_within("/opt/ws", "/etc/hosts"));
        assert!(!path_within("~/p", "~/q"));
    }

    #[tokio::test]
    async fn resolve_local_path_allows_only_project_root_subtree() {
        let tmp = std::env::temp_dir().join(format!("millet-jail-{}", std::process::id()));
        let root = tmp.join("root");
        let outside = tmp.join("outside");
        tokio::fs::create_dir_all(root.join("src")).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();
        tokio::fs::write(root.join("src/main.rs"), "fn main(){}").await.unwrap();
        tokio::fs::write(outside.join("secret.txt"), "x").await.unwrap();
        let base = root.to_string_lossy().to_string();

        // 根内相对 / 绝对路径 / 根本身：放行
        assert!(resolve_local_path(&base, "src/main.rs").await.is_ok());
        assert!(resolve_local_path(&base, &root.join("src").to_string_lossy()).await.is_ok());
        assert!(resolve_local_path(&base, "").await.is_ok());

        // 渗透测试 PoC 三件套：绝对路径、`~` 展开、`../` 穿越：拒绝
        assert!(resolve_local_path(&base, "/etc/hosts").await.is_err());
        assert!(resolve_local_path(&base, "~/.milletcode/config.toml").await.is_err());
        assert!(resolve_local_path(&base, "../outside/secret.txt").await.is_err());
        // 根外的兄弟目录即使真实存在也不放行
        assert!(
            resolve_local_path(&base, &outside.join("secret.txt").to_string_lossy())
                .await
                .is_err()
        );
        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn to_sftp_path_strips_tilde() {
        assert_eq!(to_sftp_path("~/projects/foo"), "projects/foo");
        assert_eq!(to_sftp_path("~"), ".");
        assert_eq!(to_sftp_path("~/"), ".");
        assert_eq!(to_sftp_path("/opt/ws"), "/opt/ws");
        assert_eq!(to_sftp_path("projects/foo"), "projects/foo");
    }

    #[test]
    fn expand_remote_user_replaces_leading_tilde() {
        assert_eq!(expand_remote_user("~/projects/foo", "/home/u"), "/home/u/projects/foo");
        assert_eq!(expand_remote_user("~", "/home/u"), "/home/u");
        assert_eq!(expand_remote_user("~/", "/home/u"), "/home/u");
        assert_eq!(expand_remote_user("~/projects/../bar", "/home/u"), "/home/u/projects/../bar");
        // non-tilde and unknown-home cases pass through untouched
        assert_eq!(expand_remote_user("/etc/hosts", "/home/u"), "/etc/hosts");
        assert_eq!(expand_remote_user("~/projects/foo", ""), "~/projects/foo");
        // home with trailing slash doesn't double it
        assert_eq!(expand_remote_user("~/x", "/home/u/"), "/home/u/x");
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;

    use std::process::{Command, Stdio};

    /// Spawn a sleep as a detached grandchild (sh exits immediately, sleep is
    /// reparented). A direct child would linger as a zombie after SIGKILL —
    /// un-reaped by the test process — which is not what production reclaim
    /// targets (orphaned processes are never our children).
    fn spawn_sleep() -> u32 {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg("/bin/sleep 30 >/dev/null 2>&1 & echo $!")
            .output()
            .expect("spawn sh");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .expect("parse sleep pid")
    }

    fn kill_pid(pid: u32) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[tokio::test]
    async fn kill_local_pid_and_wait_kills_spawned_process() {
        let pid = spawn_sleep();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok());
        let gone = kill_local_pid_and_wait(pid, std::time::Duration::from_secs(2)).await;
        assert!(gone, "killed process should be gone before timeout");
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err());
    }

    #[tokio::test]
    async fn kill_local_pid_and_wait_returns_true_for_dead_pid() {
        // PID 4 on macOS/Linux is not a userspace process; signal probe fails.
        assert!(kill_local_pid_and_wait(4, std::time::Duration::from_millis(100)).await);
    }

    #[test]
    fn local_process_cmdline_contains_matches_and_rejects() {
        let pid = spawn_sleep();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(local_process_cmdline_contains(pid, "sleep"));
        assert!(!local_process_cmdline_contains(pid, "definitely-not-claude"));
        kill_pid(pid);
    }

    #[test]
    fn local_process_cmdline_contains_dead_pid_is_false() {
        assert!(!local_process_cmdline_contains(4, "claude"));
    }

    /// Copy /bin/sleep under a name containing "claude" so the spawned process
    /// passes the reclaim cmdline probe. Backgrounded via sh so it is
    /// reparented (never our zombie child), matching production targets.
    fn spawn_claude_named_sleeper(dir: &std::path::Path) -> u32 {
        let bin = dir.join("claude-reclaim-fixture");
        std::fs::copy("/bin/sleep", &bin).expect("copy sleep binary");
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{} 30 >/dev/null 2>&1 & echo $!", bin.display()))
            .output()
            .expect("spawn fixture");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .expect("parse fixture pid")
    }

    fn reclaim_record(pid: Option<u32>, started_at: Option<String>) -> SessionRecord {
        SessionRecord {
            session_id: "kw/proj/claude/default".to_string(),
            claude_session_id: None,
            project_id: None,
            keyword: "kw".to_string(),
            agent_id: "claude".to_string(),
            agent_name: "claude".to_string(),
            status: "running".to_string(),
            prompt: String::new(),
            created_at: chrono::Utc::now(),
            create_time: 0,
            update_time: 0,
            pid,
            pid_started_at: started_at,
            idle_notify_enabled: false,
            model: None,
            last_error: None,
        }
    }

    #[test]
    fn local_process_start_time_none_for_dead_pid() {
        // 固定低 PID 并不保证已死：Linux 上 PID 4 是常驻内核线程（kworker）。
        // 可移植的死 PID = 自己 spawn 并已 wait 回收的子进程。
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(local_process_start_time(pid).is_none());
    }

    #[test]
    fn local_reclaim_target_matches_accepts_spawn_time_and_rejects_reuse_stamp() {
        let dir = std::env::temp_dir().join(format!("reclaim-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid = spawn_claude_named_sleeper(&dir);
        std::thread::sleep(std::time::Duration::from_millis(100));

        let stamp = local_process_start_time(pid).expect("live fixture has a start time");

        // Same stamp as recorded at "spawn" → our own process, reclaimable
        let record = reclaim_record(Some(pid), Some(stamp));
        assert!(local_reclaim_target_matches(&record, pid));

        // A reused PID carries a different start time → foreign, never kill
        let record = reclaim_record(Some(pid), Some("Wed Dec 31 23:59:59 2020".to_string()));
        assert!(!local_reclaim_target_matches(&record, pid));

        kill_pid(pid);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_reclaim_target_matches_legacy_record_keeps_cmdline_only() {
        let dir = std::env::temp_dir().join(format!("reclaim-fixture-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid = spawn_claude_named_sleeper(&dir);
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Legacy record (no captured stamp): historical best-effort behavior
        let record = reclaim_record(Some(pid), None);
        assert!(local_reclaim_target_matches(&record, pid));

        // cmdline probe still rejects foreign process names
        assert!(!local_process_cmdline_contains(pid, "definitely-not-running"));

        kill_pid(pid);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod idle_scan_tests {
    use super::*;

    #[test]
    fn idle_candidate_status_covers_only_live_statuses() {
        assert!(is_idle_candidate_status("running"));
        assert!(is_idle_candidate_status("waiting_approval"));
        // smartcode 轮次结束后进程仍驻留等下一条消息 —— 与 running 一样是
        // "agent 闲置等用户" 的候选；死进程由 pid 存活检查过滤。
        assert!(is_idle_candidate_status("complete"));
        assert!(!is_idle_candidate_status("stopped"));
        assert!(!is_idle_candidate_status("error"));
        assert!(!is_idle_candidate_status(""));
    }

    #[test]
    fn idle_threshold_met_is_inclusive_and_skips_never_active() {
        let now = 100_000_000u64;
        let threshold = 5 * 60_000;

        // exactly at the threshold counts as idle (>=)
        assert!(is_idle_threshold_met(now - threshold, now, threshold));
        // one millisecond short stays active
        assert!(!is_idle_threshold_met(now - threshold + 1, now, threshold));
        // far past is idle
        assert!(is_idle_threshold_met(now - threshold * 10, now, threshold));
        // zero timestamp = never active, never idle
        assert!(!is_idle_threshold_met(0, now, threshold));
        // clock skew (last > now) must not report idle
        assert!(!is_idle_threshold_met(now + 60_000, now, threshold));
    }

    #[test]
    fn prompt_tail_skips_hidden_idle_notify_only() {
        // 系统自动发的闲置通知不进列表页
        assert!(!should_update_prompt_tail(
            "/millet-msg 请通知我，当前会话： [default](https://example.com/sessions/x) 已闲置."
        ));
        // 前导空白也应识别
        assert!(!should_update_prompt_tail("  /millet-msg 请通知我"));
        // 普通消息、其他 skill 指令照常显示
        assert!(should_update_prompt_tail("帮我看看这个报错"));
        assert!(should_update_prompt_tail("/compact 压缩上下文"));
        assert!(should_update_prompt_tail(""));
    }
}
