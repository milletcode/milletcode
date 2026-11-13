use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;
use parking_lot;

use crate::agents::{AgentAdapter, AgentEvent, RuntimeContext, SessionHandle, SessionHandleState};
use crate::agents::types::{ApprovalResponse, SessionId, SessionStatus};
use crate::config::{AgentConfig, ConfigManager};
use crate::logging::is_session_log_enabled;
use crate::runtime::{LocalRuntime, Process, Runtime, SftpClient, SshRuntime};

pub struct ClaudeAdapter {
    event_bus: std::sync::Arc<crate::events::EventBus>,
    config_manager: std::sync::Arc<ConfigManager>,
    sessions: parking_lot::Mutex<std::collections::HashMap<SessionId, ClaudeSessionState>>,
}

impl ClaudeAdapter {
    pub fn new(event_bus: std::sync::Arc<crate::events::EventBus>, config_manager: std::sync::Arc<ConfigManager>) -> Self {
        Self {
            event_bus,
            config_manager,
            sessions: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Build command and args for Claude CLI
    fn build_command(
        config: &AgentConfig,
        ctx: &RuntimeContext,
        claude_session_id: Option<&str>,
        is_resume: bool,
        name: Option<&str>,
    ) -> (String, Vec<String>) {
        let binary = if ctx.binary_path.is_empty() {
            "claude".to_string()
        } else {
            ctx.binary_path.clone()
        };

        crate::runtime::build_claude_args(
            &binary,
            claude_session_id,
            is_resume,
            config.auto_approve,
            &config.extra_args,
            name,
        )
    }

    /// Build environment variables for Claude process
    fn build_env(config: &AgentConfig, ctx: &RuntimeContext) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(ref api_key) = config.api_key {
            if !api_key.is_empty() {
                env.push(("ANTHROPIC_API_KEY".to_string(), api_key.clone()));
            }
        }
        env.extend(ctx.gitea_env.iter().cloned());
        env
    }

    /// Spawn a Claude process using the Runtime abstraction
    async fn spawn_claude_process(
        config: &AgentConfig,
        ctx: &RuntimeContext,
        claude_session_id: Option<&str>,
        is_resume: bool,
        name: Option<&str>,
        working_dir: Option<&str>,
    ) -> Result<(Box<dyn Process>, Box<dyn tokio::io::AsyncRead + Send + Unpin>, Box<dyn tokio::io::AsyncWrite + Send + Unpin>, u32, std::sync::Arc<parking_lot::Mutex<String>>)> {
        let (cmd, args) = Self::build_command(config, ctx, claude_session_id, is_resume, name);
        let env = Self::build_env(config, ctx);
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let mut process: Box<dyn Process> = if ctx.remote_mode == "ssh" {
            // SSH remote execution. NEVER fall back to a local spawn for a
            // remote_mode=ssh project: the agent binary may only exist on the
            // remote host (the agent container itself has no node/claude).
            // Report connection/execution failures as SSH errors, not as a
            // missing local claude install.
            let ssh_runtime = SshRuntime::connect(ctx).await.map_err(|e| {
                anyhow::anyhow!(
                    "SSH 连接失败：无法通过 SSH 连接到远程主机（remote_mode=ssh, host: {}）: {}",
                    ctx.hostname,
                    e
                )
            })?;
            ssh_runtime
                .spawn(
                    &cmd,
                    &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    working_dir,
                    env_refs,
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "SSH 远程执行失败（remote_mode=ssh, host: {}, 命令: {}）: {}",
                        ctx.hostname,
                        cmd,
                        e
                    )
                })?
        } else {
            // Local execution — only reached for genuinely local projects.
            // Here "Is 'claude' installed?" is the right diagnostic.
            let runtime = LocalRuntime::new();
            runtime
                .spawn(
                    &cmd,
                    &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    working_dir,
                    env_refs,
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to spawn claude process: {}. Is 'claude' installed?",
                        e
                    )
                })?
        };

        let pid = process.id();
        let stdout = process.take_stdout().ok_or_else(|| anyhow::anyhow!("stdout not available"))?;
        let stdin = process.take_stdin().ok_or_else(|| anyhow::anyhow!("stdin not available"))?;

        // The CLI reports startup/validation failures on stderr; without this
        // drain the pipe fills and the reason for an early exit is lost.
        // The tail (last ~8KB) is kept so spawn_wait_task can surface the
        // reason in the conversation when the process crashes.
        let stderr_tail = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
        if let Some(stderr) = process.take_stderr() {
            let tail = stderr_tail.clone();
            let session_for_err = name.unwrap_or("claude").to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(session = %session_for_err, "Claude stderr: {}", line);
                    let mut buf = tail.lock();
                    buf.push_str(&line);
                    buf.push('\n');
                    let len = buf.len();
                    if len > 8 * 1024 {
                        let cut = len - 8 * 1024;
                        let split = buf[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
                        buf.drain(..split);
                    }
                }
            });
        }

        Ok((process, stdout, stdin, pid, stderr_tail))
    }

    /// Spawn the stdout reader task that parses Claude's stream-json output
    fn spawn_reader_task(
        stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        session_id: SessionId,
        tx: mpsc::Sender<AgentEvent>,
        event_bus: std::sync::Arc<crate::events::EventBus>,
        cancel: CancellationToken,
        stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        pending_control: std::sync::Arc<parking_lot::Mutex<Option<PendingControl>>>,
        config_manager: std::sync::Arc<ConfigManager>,
        log_suffix: &str,
    ) {
        let session_for_reader = session_id.clone();
        let tx_for_reader = tx.clone();
        let event_bus_for_reader = event_bus.clone();
        let cancel_for_reader = cancel.clone();
        let stdin_for_reader = stdin.clone();
        let pending_for_reader = pending_control.clone();
        let config_manager_for_reader = config_manager.clone();
        let suffix = log_suffix.to_string();

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            // Track permission-denied tool messages between lines. When Claude's
            // built-in permission system denies a tool (tool_result with is_error
            // containing "permissions"/"haven't granted"), we collect the details.
            // When the following assistant message ends with stop_reason:"end_turn",
            // we fire an ApprovalRequest so the frontend shows interactive
            // approve/deny UI. On approve, handle_approval writes "y\n" to stdin,
            // which Claude interprets as approval to retry.
            let mut pending_permission_msgs: Vec<String> = Vec::new();
            // AskUserQuestion's tool_use block arrives on an assistant line BEFORE
            // its control_request; remember the id so the UserQuestion event can
            // carry it — the control_request itself has no tool_use_id.
            let mut last_ask_tool_use_id: Option<String> = None;
            while let Ok(Some(line)) = lines.next_line().await {
                if is_session_log_enabled() {
                    tracing::info!(session = %session_for_reader, line_preview = line.chars().take(80).collect::<String>(), "Claude stdout line {}", suffix);
                }
                if cancel_for_reader.is_cancelled() {
                    break;
                }

                // In-band control protocol and permission-denied detection.
                let mut handled = false;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    let evt = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match evt {
                        "control_request" => {
                            // Must never reach parse_claude_event: the `_` fallback
                            // there would leak the raw line to the user as Output.
                            Self::handle_control_request(
                                &v,
                                &session_for_reader,
                                &tx_for_reader,
                                &event_bus_for_reader,
                                &stdin_for_reader,
                                &pending_for_reader,
                                &config_manager_for_reader,
                                last_ask_tool_use_id.clone(),
                            )
                            .await;
                            handled = true;
                        }
                        "control_response" => {
                            // Reply to OUR outbound control_request (interrupt).
                            // Swallow — same leak hazard as control_request.
                            handled = true;
                        }
                        "result" => {
                            // Authoritative end-of-turn marker from the CLI (success or
                            // error_*). Carried as an empty Output whose raw.type lets the
                            // frontend bridge flag turn completion — the thread-based
                            // "last assistant stop_reason" heuristic alone misses cases
                            // like interrupted turns that end on a user message.
                            let event = AgentEvent::Output {
                                session_id: session_for_reader.clone(),
                                content: String::new(),
                                ts: AgentEvent::now(),
                                raw: Some(v.clone()),
                            };
                            let _ = tx_for_reader.send(event.clone()).await;
                            event_bus_for_reader.emit(event);
                            handled = true;
                        }
                        "user" => {
                            if let Some(parts) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                                for p in parts {
                                    if p.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                                        let is_error = p.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                                        let content = p.get("content").and_then(|c| c.as_str()).unwrap_or("");
                                        if is_error && (content.contains("permissions") || content.contains("haven't granted"))
                                        {
                                            pending_permission_msgs.push(content.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        "assistant" => {
                            if let Some(parts) = v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                                for p in parts {
                                    if p.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                                        && p.get("name").and_then(|n| n.as_str()) == Some("AskUserQuestion")
                                    {
                                        if let Some(id) = p.get("id").and_then(|i| i.as_str()) {
                                            last_ask_tool_use_id = Some(id.to_string());
                                        }
                                    }
                                }
                            }
                            let stop_reason = v.get("message")
                                .and_then(|m| m.get("stop_reason"))
                                .and_then(|s| s.as_str());
                            if stop_reason == Some("end_turn") && !pending_permission_msgs.is_empty() {
                                let prompt = if pending_permission_msgs.len() == 1 {
                                    pending_permission_msgs[0].clone()
                                } else {
                                    format!("{} permission requests pending approval", pending_permission_msgs.len())
                                };
                                let approval_event = AgentEvent::ApprovalRequest {
                                    session_id: session_for_reader.clone(),
                                    prompt,
                                    options: vec![
                                        "Approve".to_string(),
                                        "Deny".to_string(),
                                        "Provide feedback...".to_string(),
                                    ],
                                    request_id: None,
                                    tool_name: None,
                                    ts: AgentEvent::now(),
                                };
                                let _ = tx_for_reader.send(approval_event.clone()).await;
                                event_bus_for_reader.emit(approval_event);
                                pending_permission_msgs.clear();
                            }
                        }
                        _ => {}
                    }
                }

                if !handled {
                    for event in parse_claude_event(&line, &session_for_reader) {
                        if is_session_log_enabled() {
                            if let AgentEvent::Output { ref content, .. } = event {
                                tracing::info!(session = %session_for_reader, content_len = content.len(), "Output event {}", suffix);
                            }
                        }
                        let _ = tx_for_reader.send(event.clone()).await;
                        event_bus_for_reader.emit(event);
                    }
                }
            }
            if is_session_log_enabled() {
                tracing::info!(session = %session_for_reader, "Claude stdout reader finished {}", suffix);
            }
            event_bus_for_reader.emit(AgentEvent::SessionComplete {
                session_id: session_for_reader.clone(),
                exit_code: 0,
                ts: AgentEvent::now(),
            });
        });
    }

    /// Handle an in-band control_request from the CLI (tool permission prompt or
    /// AskUserQuestion). Allow-listed Bash commands are answered allow() in place
    /// without surfacing to the UI; everything else is forwarded to the frontend
    /// and parked in the pending slot until the user decides — there is no timeout.
    async fn handle_control_request(
        v: &serde_json::Value,
        session_id: &SessionId,
        tx: &mpsc::Sender<AgentEvent>,
        event_bus: &std::sync::Arc<crate::events::EventBus>,
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        pending: &std::sync::Arc<parking_lot::Mutex<Option<PendingControl>>>,
        config_manager: &std::sync::Arc<ConfigManager>,
        ask_tool_use_id: Option<String>,
    ) {
        let request_id = v.get("request_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
        if request_id.is_empty() {
            tracing::warn!(session = %session_id, "control_request without request_id — ignored");
            return;
        }
        let request = v.get("request");
        let subtype = request
            .and_then(|r| r.get("subtype"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        if subtype != "can_use_tool" {
            tracing::debug!(session = %session_id, subtype = %subtype, "Ignoring non can_use_tool control_request");
            return;
        }
        let tool_name = request
            .and_then(|r| r.get("tool_name"))
            .and_then(|t| t.as_str())
            .unwrap_or("unknown")
            .to_string();
        let input = request
            .and_then(|r| r.get("input"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Approval policy: default-allow everything (bypass-equivalent scope).
        // Only protected Bash commands and AskUserQuestion reach a human.
        let protected = config_manager.config().permissions.protected_commands;
        if !approval_required(&tool_name, &input, &protected) {
            let response = build_control_response(&request_id, "allow", Some(input.clone()), None);
            let mut guard = stdin.lock().await;
            if let Err(e) = guard.write_all(format!("{}\n", response).as_bytes()).await {
                tracing::error!(session = %session_id, "Failed to write auto-allow control_response: {}", e);
                return;
            }
            if let Err(e) = guard.flush().await {
                tracing::error!(session = %session_id, "Failed to flush auto-allow control_response: {}", e);
                return;
            }
            tracing::info!(session = %session_id, tool = %tool_name, request_id = %request_id, "Auto-allowed by approval policy");
            return;
        }

        *pending.lock() = Some(PendingControl {
            request_id: request_id.clone(),
            tool_name: tool_name.clone(),
            input: input.clone(),
        });

        if tool_name == "AskUserQuestion" {
            let questions: Vec<crate::agents::types::UserQuestionItem> = input
                .get("questions")
                .and_then(|q| serde_json::from_value(q.clone()).ok())
                .unwrap_or_default();
            let event = AgentEvent::UserQuestion {
                session_id: session_id.clone(),
                tool_use_id: ask_tool_use_id.unwrap_or_default(),
                request_id,
                questions,
                ts: AgentEvent::now(),
            };
            let _ = tx.send(event.clone()).await;
            event_bus.emit(event);
        } else {
            let input_str = serde_json::to_string(&input).unwrap_or_default();
            let prompt = format!(
                "Tool '{}' requested: {}",
                tool_name,
                input_str.chars().take(500).collect::<String>()
            );
            let event = AgentEvent::ApprovalRequest {
                session_id: session_id.clone(),
                prompt,
                options: vec![
                    "Approve".to_string(),
                    "Deny".to_string(),
                    "Provide feedback...".to_string(),
                ],
                request_id: Some(request_id),
                tool_name: Some(tool_name),
                ts: AgentEvent::now(),
            };
            let _ = tx.send(event.clone()).await;
            event_bus.emit(event);
        }
    }

    /// Spawn the wait task that monitors process exit
    fn spawn_wait_task(
        process: std::sync::Arc<TokioMutex<Option<Box<dyn Process>>>>,
        event_bus: std::sync::Arc<crate::events::EventBus>,
        session_id: SessionId,
        log_suffix: &str,
        stderr_tail: std::sync::Arc<parking_lot::Mutex<String>>,
        tx: mpsc::Sender<AgentEvent>,
        suppress_crash: std::sync::Arc<parking_lot::Mutex<bool>>,
    ) {
        let process_for_wait = process.clone();
        let suffix = log_suffix.to_string();
        tokio::spawn(async move {
            if let Some(mut p) = process_for_wait.lock().await.take() {
                let status = p.wait().await;
                tracing::info!(?status, "Claude Code process exited {}", suffix);

                // Only emit Stopped/Error for abnormal exits.
                // Normal exit (Ok(0)) is handled by SessionComplete from the reader task,
                // which sets status to Complete instead of Stopped.
                // This prevents the "agent process not running" dialog from showing
                // when Claude exits normally after completing a response.
                if let Err(_) = &status {
                    event_bus.emit(AgentEvent::SessionStatus {
                        session_id: session_id.clone(),
                        status: SessionStatus::Error,
                        ts: AgentEvent::now(),
                    });
                }
                // For Ok(0) and Ok(non-zero), don't emit status here.
                // The reader task will emit SessionComplete which sets Complete/Error status.

                // 崩溃（非 0 退出）：把失败原因写进会话，否则远端启动失败
                // （如包装脚本报错）只会无声消失。事件走通道（session 任务：
                // 内存+落库）和 bus（WS 前端）双投递，与 reader 的约定一致。
                // Output 只在 stderr 有内容时发（写入对话流，live 可见）；
                // Error 事件必发——它驱动 record.status=error + last_error
                // 落库，刷新页面后失败原因仍可见，也避免死进程卡在 running。
                // 平台自己杀的（stop/restore 回收/resume 热切换）不算崩溃，
                // suppress 标记由杀进程方在动手前置位。
                let crashed = !matches!(&status, Ok(0));
                if crashed {
                    if *suppress_crash.lock() {
                        tracing::info!(session = %session_id, ?status, "Process exit suppressed: platform-initiated stop/reclaim/resume");
                    } else {
                    let code = match &status {
                        Ok(c) => *c,
                        Err(_) => -1,
                    };
                    let reason = crash_reason(code);
                    // tail 走用户可见通道（对话流 + last_error 落库），出口处保密
                    // 改写；stderr_tail 原始内容仍留给服务端日志排查。
                    let tail = crate::sanitize::sanitize_public_error(stderr_tail.lock().trim());
                    let message = if tail.is_empty() {
                        reason
                    } else {
                        format!("{reason}。stderr 输出：\n{tail}")
                    };
                    if !tail.is_empty() {
                        let event = AgentEvent::Output {
                            session_id: session_id.clone(),
                            content: message.clone(),
                            ts: AgentEvent::now(),
                            raw: None,
                        };
                        let _ = tx.send(event.clone()).await;
                        event_bus.emit(event);
                    }
                    let error_event = AgentEvent::Error {
                        session_id: session_id.clone(),
                        message,
                        ts: AgentEvent::now(),
                    };
                    let _ = tx.send(error_event.clone()).await;
                    event_bus.emit(error_event);
                    }
                }
            }
        });
    }
}

/// 非 0 退出码的用户可读原因。-1 = 信号终止无退出码（local wait 对无 code
/// 进程的约定值）；130/137/143 是 shell 包装下常见的信号退出码。
pub(crate) fn crash_reason(code: i32) -> String {
    match code {
        -1 => format!("进程被信号终止（exit code: {code}，可能被系统强制结束，如内存不足）"),
        130 => format!("进程收到中断信号 SIGINT（exit code: {code}）"),
        137 => format!("进程被强制杀死 SIGKILL（exit code: {code}，常见于系统内存不足）"),
        143 => format!("进程被终止信号 SIGTERM 结束（exit code: {code}）"),
        c => format!("进程异常退出（exit code: {c}）"),
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn adapter_id(&self) -> &str {
        "claude"
    }

    async fn start_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        session_key: &str,
        prompt: &str,
        working_dir: Option<String>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, claude_session_id = %claude_session_id, session_key = %session_key, working_dir = ?working_dir, "Starting Claude Code session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        // Legacy cleanup: prune HTTP hooks from earlier releases out of the project's
        // .claude/settings.json — they point at /api/hooks/* endpoints that no longer
        // exist now that approvals ride the in-band control protocol. For remote
        // sessions (remote_mode == "ssh") this cleans the remote host via SFTP.
        if let Some(wd) = working_dir.as_deref() {
            if let Err(e) = Self::prune_project_hook_settings(ctx, wd, &session_id.0).await {
                tracing::warn!(session = %session_id, error = %e, "Failed to prune legacy hook settings");
            }
        }

        let wd = working_dir.as_deref();
        let (process, stdout, stdin, pid, stderr_tail) = Self::spawn_claude_process(
            config,
            ctx,
            Some(claude_session_id),
            false,
            Some(session_key),
            wd,
        ).await?;

        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending_control = std::sync::Arc::new(parking_lot::Mutex::new(None));

        // Write the initial prompt to stdin as stream-json
        {
            let mut stdin_handle = stdin_arc.lock().await;
            let input = serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": prompt}]
                }
            });
            stdin_handle.write_all(format!("{}\n", input).as_bytes()).await?;
            stdin_handle.flush().await?;
        }

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();

        Self::spawn_reader_task(stdout, session_id.clone(), tx.clone(), self.event_bus.clone(), cancel.clone(), stdin_arc.clone(), pending_control.clone(), self.config_manager.clone(), "");

        let process_arc = std::sync::Arc::new(TokioMutex::new(Some(process)));
        let suppress = std::sync::Arc::new(parking_lot::Mutex::new(false));
        Self::spawn_wait_task(process_arc.clone(), self.event_bus.clone(), session_id.clone(), "", stderr_tail.clone(), tx.clone(), suppress.clone());

        let state = ClaudeSessionState {
            tx,
            rx: parking_lot::Mutex::new(Some(rx)),
            stdin: stdin_arc,
            cancel,
            pid,
            process: process_arc,
            pending_control,
            suppress_crash: suppress,
        };
        self.sessions.lock().insert(session_id.clone(), state);

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        let handle_state = SessionHandleState::Subprocess { pid };
        Ok(SessionHandle::new(session_id, "claude".to_string(), handle_state))
    }

    async fn send_message(&self, session: &SessionHandle, message: &str) -> Result<()> {
        let stdin = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            state.stdin.clone()
        };

        let mut stdin_guard = stdin.lock().await;
        let input = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": message}]
            }
        });
        stdin_guard.write_all(format!("{}\n", input).as_bytes()).await?;
        stdin_guard.flush().await?;
        Ok(())
    }

    async fn handle_approval(
        &self,
        session: &SessionHandle,
        request_id: Option<&str>,
        response: ApprovalResponse,
    ) -> Result<()> {
        let (stdin, pending_control) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (state.stdin.clone(), state.pending_control.clone())
        };

        let entry = pending_control.lock().take();
        let Some(entry) = entry else {
            anyhow::bail!("No pending control request for session {}", session.id);
        };
        let current_id = entry.request_id.clone();
        if let Some(rid) = request_id {
            if rid != current_id {
                *pending_control.lock() = Some(entry);
                anyhow::bail!(
                    "Stale approval decision: request {} no longer pending (current {})",
                    rid,
                    current_id
                );
            }
        }

        let approve = matches!(
            response.selected.to_lowercase().as_str(),
            "approve" | "allow" | "yes" | "y"
        );

        let message = if approve {
            build_control_response(&entry.request_id, "allow", Some(entry.input.clone()), None)
        } else {
            let comment = response
                .comment
                .clone()
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| "User denied this tool use".to_string());
            build_control_response(&entry.request_id, "deny", None, Some(&comment))
        };

        let mut stdin_guard = stdin.lock().await;
        stdin_guard.write_all(format!("{}\n", message).as_bytes()).await?;
        stdin_guard.flush().await?;

        tracing::info!(
            session = %session.id,
            request_id = %entry.request_id,
            tool = %entry.tool_name,
            approved = approve,
            "control_response sent"
        );
        Ok(())
    }

    async fn answer_question(
        &self,
        session: &SessionHandle,
        request_id: &str,
        answers: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let (stdin, pending_control) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (state.stdin.clone(), state.pending_control.clone())
        };

        let entry = pending_control.lock().take();
        let Some(entry) = entry else {
            anyhow::bail!("No pending control request for session {}", session.id);
        };
        let current_id = entry.request_id.clone();
        if request_id != current_id {
            *pending_control.lock() = Some(entry);
            anyhow::bail!(
                "Stale question answer: request {} no longer pending (current {})",
                request_id,
                current_id
            );
        }

        // Full mapping: every question text (as sent by the CLI) -> chosen label,
        // attached to the original input as updatedInput. Deliberately maps ALL
        // questions — the old hook path only mapped questions.first().
        let mut updated_input = entry.input.clone();
        if let Some(obj) = updated_input.as_object_mut() {
            obj.insert("answers".to_string(), serde_json::to_value(answers)?);
        }

        let message = build_control_response(request_id, "allow", Some(updated_input), None);

        let mut stdin_guard = stdin.lock().await;
        stdin_guard.write_all(format!("{}\n", message).as_bytes()).await?;
        stdin_guard.flush().await?;

        tracing::info!(
            session = %session.id,
            request_id = %request_id,
            answers = answers.len(),
            "AskUserQuestion answered via control_response"
        );
        Ok(())
    }

    async fn interrupt(&self, session: &SessionHandle) -> Result<()> {
        let stdin = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            state.stdin.clone()
        };

        let request_id = uuid::Uuid::new_v4().to_string();
        let message = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        });

        let mut stdin_guard = stdin.lock().await;
        stdin_guard.write_all(format!("{}\n", message).as_bytes()).await?;
        stdin_guard.flush().await?;

        tracing::info!(
            session = %session.id,
            request_id = %request_id,
            "interrupt control_request sent"
        );
        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, force: bool) -> Result<()> {
        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session.id.clone(),
            status: SessionStatus::Stopped,
            ts: AgentEvent::now(),
        });

        let process_opt = {
            let mut sessions = self.sessions.lock();
            // Only remove if the entry matches — restore may have replaced it
            sessions.remove(&session.id).map(|state| {
                *state.suppress_crash.lock() = true;
                state.cancel.cancel();
                state.process
            })
        };

        if let Some(process) = process_opt {
            let session_id = session.id.0.clone();
            // Spawn the kill sequence so the caller isn't blocked. Must await the
            // lock, not try_lock: the mutex is briefly held by in-flight stdin
            // writes / reader tasks, and a failed try_lock silently skipped the
            // kill — state was already removed from the map, so the process kept
            // running with nobody holding its handle (the leaked-children bug).
            tokio::spawn(async move {
                let mut p = process.lock().await;
                if let Some(mut proc) = p.take() {
                    // Use Process::kill() which maps to SIGKILL for local processes
                    // and channel.close() for remote SSH processes
                    if let Err(e) = proc.kill().await {
                        tracing::warn!(session = %session_id, error = %e, "Failed to kill session process");
                    }
                }
                // Only remove lock file if the session hasn't been restored
                Self::remove_claude_session_lock(&session_id);
            });
        }

        Ok(())
    }

    fn has_process(&self, session_id: &SessionId) -> bool {
        let sessions = self.sessions.lock();
        if let Some(state) = sessions.get(session_id) {
            // Check if we still have a process handle (not yet killed/exited)
            state.process.try_lock().map(|p| p.is_some()).unwrap_or(false)
        } else {
            false
        }
    }

    fn suppress_crash_report(&self, session_id: &SessionId) {
        if let Some(state) = self.sessions.lock().get(session_id) {
            *state.suppress_crash.lock() = true;
        }
    }

    fn output_stream(&self, session: &SessionHandle) -> Box<dyn Stream<Item = AgentEvent> + Send + Unpin> {
        let sessions = self.sessions.lock();
        if let Some(state) = sessions.get(&session.id) {
            if let Some(rx) = state.rx.lock().take() {
                return Box::new(tokio_stream::wrappers::ReceiverStream::new(rx));
            }
        }
        Box::new(futures::stream::empty())
    }

    async fn restore_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        session_key: &str,
        working_dir: Option<String>,
        _model: Option<&str>,
    ) -> Result<SessionHandle> {
        ClaudeAdapter::restore_session(self, config, ctx, session_id, claude_session_id, session_key, working_dir).await
    }

    async fn resume_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        session_key: &str,
        working_dir: Option<String>,
        _model: Option<&str>,
    ) -> Result<SessionHandle> {
        ClaudeAdapter::resume_session(self, config, ctx, session_id, claude_session_id, session_key, working_dir).await
    }

    async fn cleanup_session(&self, session_id: &str, working_dir: Option<&str>) {
        if let Some(wd) = working_dir {
            Self::remove_claude_project_files(session_id, wd);
        }
    }
}

impl ClaudeAdapter {
    /// Remove Claude CLI's session lock file so the same session_id can be
    /// reused. Claude CLI stores a lock file in ~/.claude/sessions/<uuid>/ — if
    /// not removed after SIGKILL, the CLI refuses to start with that session_id.
    pub(crate) fn remove_claude_session_lock(session_id: &str) {
        let session_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".claude")
            .join("sessions")
            .join(session_id);

        if !session_dir.exists() {
            return;
        }

        let lock_file = session_dir.join("lock");
        if lock_file.exists() {
            let _ = std::fs::remove_file(&lock_file);
            tracing::debug!(session = %session_id, lock = ?lock_file, "Removed Claude session lock");
        }
    }

    /// Prune legacy HTTP hooks out of a project's .claude/settings.json. Earlier
    /// releases wrote PermissionRequest/PreToolUse entries pointing at /api/hooks/*
    /// endpoints; those endpoints are gone (approvals ride the in-band control
    /// protocol now), so leftover entries would make the CLI call dead URLs on every
    /// permission event. This pass only removes — it never adds. Once every
    /// workspace has been opened once, it can be deleted entirely.
    async fn prune_project_hook_settings(
        ctx: &RuntimeContext,
        working_dir: &str,
        session_id: &str,
    ) -> Result<()> {
        if ctx.remote_mode == "ssh" {
            Self::prune_project_hook_settings_remote(ctx, working_dir, session_id).await
        } else {
            Self::prune_project_hook_settings_local(working_dir, session_id);
            Ok(())
        }
    }

    /// Remove our hook entries from a settings value in place. Returns true if
    /// anything changed. Hooks the user configured themselves are untouched.
    fn prune_settings_value(settings: &mut serde_json::Value) -> bool {
        let mut changed = false;
        if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            for (_event, entries) in hooks.iter_mut() {
                if let Some(arr) = entries.as_array_mut() {
                    changed |= Self::prune_our_hooks(arr);
                }
            }
            hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
        }
        if settings
            .get("hooks")
            .and_then(|h| h.as_object())
            .is_some_and(|m| m.is_empty())
        {
            if let Some(obj) = settings.as_object_mut() {
                obj.remove("hooks");
            }
            changed = true;
        }
        changed
    }

    /// Our hook endpoints all live under this path, whatever the base URL was.
    const HOOK_PATH: &'static str = "/api/hooks/";

    /// Drop every hook entry that points at one of our endpoints, leaving hooks the
    /// user configured themselves untouched. Entries whose hook list becomes empty are
    /// removed outright. Returns true if anything was removed.
    fn prune_our_hooks(entries: &mut Vec<serde_json::Value>) -> bool {
        let mut removed = false;
        for entry in entries.iter_mut() {
            if let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                let before = hooks.len();
                hooks.retain(|hook| {
                    hook.get("url")
                        .and_then(|u| u.as_str())
                        .is_none_or(|url| !url.contains(Self::HOOK_PATH))
                });
                removed |= hooks.len() != before;
            }
        }
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_none_or(|hooks| !hooks.is_empty())
        });
        removed
    }

    /// Prune legacy HTTP hooks from the local project's .claude/settings.json.
    fn prune_project_hook_settings_local(working_dir: &str, session_id: &str) {
        let settings_file = std::path::Path::new(working_dir)
            .join(".claude")
            .join("settings.json");
        let Ok(content) = std::fs::read_to_string(&settings_file) else {
            return;
        };
        let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&content) else {
            return;
        };
        if Self::prune_settings_value(&mut settings) {
            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                let _ = std::fs::write(&settings_file, json);
                tracing::info!(
                    file = ?settings_file,
                    session = %session_id,
                    "Pruned legacy HTTP hooks from .claude/settings.json"
                );
            }
        }
    }

    /// Prune legacy HTTP hooks from the remote project's .claude/settings.json via SFTP.
    async fn prune_project_hook_settings_remote(
        ctx: &RuntimeContext,
        working_dir: &str,
        session_id: &str,
    ) -> Result<()> {
        let ssh_runtime = SshRuntime::connect(ctx).await?;
        let handle = ssh_runtime.shared_handle();
        let mut sftp = SftpClient::new(&handle).await?;

        // Remote paths are typically "~/..." — SFTP resolves relative paths against
        // the remote user's home directory.
        let dir = if working_dir == "~" {
            ".".to_string()
        } else if let Some(rest) = working_dir.strip_prefix("~/") {
            rest.trim_start_matches('/').to_string()
        } else {
            working_dir.to_string()
        };
        let settings_path = format!("{}/.claude/settings.json", dir);

        let content = match sftp.read_to_string(&settings_path).await {
            Ok(content) => content,
            Err(_) => return Ok(()), // no settings file — nothing to prune
        };
        let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Ok(());
        };

        if Self::prune_settings_value(&mut settings) {
            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                sftp.write(&settings_path, &json).await?;
                tracing::info!(
                    remote_path = %settings_path,
                    session = %session_id,
                    "Pruned legacy HTTP hooks from remote .claude/settings.json"
                );
            }
        }

        Ok(())
    }

    /// Delete Claude session files:
    /// 1. The JSONL file at ~/.claude/projects/<path>/<uuid>.jsonl
    /// 2. The subdirectory at ~/.claude/projects/<path>/<uuid>/ (tool-results, subagents, etc.)
    /// The project path is derived from the working_dir of the session.
    async fn cleanup_session(&self, session_id: &str, working_dir: Option<&str>) {
        if let Some(wd) = working_dir {
            Self::remove_claude_project_files(session_id, wd);
        }
    }

    /// Build the project path that Claude CLI uses: ~/.claude/projects/<path-with-dashes>/
    /// e.g. /Users/dev/Codes/my-app ->
    ///      ~/.claude/projects/-Users-dev-Codes-my-app/
    fn claude_project_dir(working_dir: &str) -> std::path::PathBuf {
        let slug = working_dir.replace('/', "-");
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".claude")
            .join("projects")
            .join(slug)
    }

    fn remove_claude_project_files(session_id: &str, working_dir: &str) {
        let project_dir = Self::claude_project_dir(working_dir);
        let jsonl_file = project_dir.join(format!("{}.jsonl", session_id));
        let session_dir = project_dir.join(session_id);

        // Remove the .jsonl conversation log
        if jsonl_file.exists() {
            let _ = std::fs::remove_file(&jsonl_file);
            tracing::info!(session = %session_id, file = ?jsonl_file, "Deleted Claude session JSONL");
        }

        // Remove the session subdirectory (tool-results, subagents, etc.)
        if session_dir.exists() {
            let _ = std::fs::remove_dir_all(&session_dir);
            tracing::info!(session = %session_id, dir = ?session_dir, "Deleted Claude session subdirectory");
        }
    }

    /// Resume a running session after config change — kill old process and restart
    /// with `--resume <claude_session_id>` to preserve conversation context.
    pub async fn resume_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        session_key: &str,
        working_dir: Option<String>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, claude_session_id = %claude_session_id, session_key = %session_key, working_dir = ?working_dir, "Resuming Claude Code session with new config");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Restarting,
            ts: AgentEvent::now(),
        });

        // Kill existing process if present
        let existing_state = self.sessions.lock().remove(&session_id);
        if let Some(state) = existing_state {
            *state.suppress_crash.lock() = true;
            state.cancel.cancel();
            let mut process_guard = state.process.lock().await;
            if let Some(mut p) = process_guard.take() {
                let _ = p.kill().await;
            }
        }

        let wd = working_dir.as_deref();
        let (process, stdout, stdin, pid, stderr_tail) = Self::spawn_claude_process(
            config,
            ctx,
            Some(claude_session_id),
            true, // is_resume
            Some(session_key),
            wd,
        ).await?;

        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending_control = std::sync::Arc::new(parking_lot::Mutex::new(None));

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();

        Self::spawn_reader_task(stdout, session_id.clone(), tx.clone(), self.event_bus.clone(), cancel.clone(), stdin_arc.clone(), pending_control.clone(), self.config_manager.clone(), "(resumed)");

        let process_arc = std::sync::Arc::new(TokioMutex::new(Some(process)));
        let suppress = std::sync::Arc::new(parking_lot::Mutex::new(false));
        Self::spawn_wait_task(process_arc.clone(), self.event_bus.clone(), session_id.clone(), "(resumed)", stderr_tail.clone(), tx.clone(), suppress.clone());

        let state = ClaudeSessionState {
            tx,
            rx: parking_lot::Mutex::new(Some(rx)),
            stdin: stdin_arc,
            cancel,
            pid,
            process: process_arc,
            pending_control,
            suppress_crash: suppress,
        };
        self.sessions.lock().insert(session_id.clone(), state);

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        let handle_state = SessionHandleState::Subprocess { pid };
        Ok(SessionHandle::new(session_id, "claude".to_string(), handle_state))
    }

    /// Restore a previously closed session by spawning Claude with --resume <claude_session_id>.
    /// Claude resumes from history.jsonl using the claude_session_id context.
    pub async fn restore_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        _session_key: &str,
        working_dir: Option<String>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, claude_session_id = %claude_session_id, working_dir = ?working_dir, "Restoring Claude Code session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let wd = working_dir.as_deref();
        let (process, stdout, stdin, pid, stderr_tail) = Self::spawn_claude_process(
            config,
            ctx,
            Some(claude_session_id),
            true, // is_resume
            None,
            wd,
        ).await?;

        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending_control = std::sync::Arc::new(parking_lot::Mutex::new(None));

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();

        Self::spawn_reader_task(stdout, session_id.clone(), tx.clone(), self.event_bus.clone(), cancel.clone(), stdin_arc.clone(), pending_control.clone(), self.config_manager.clone(), "(restored)");

        let process_arc = std::sync::Arc::new(TokioMutex::new(Some(process)));
        let suppress = std::sync::Arc::new(parking_lot::Mutex::new(false));
        Self::spawn_wait_task(process_arc.clone(), self.event_bus.clone(), session_id.clone(), "(restored)", stderr_tail.clone(), tx.clone(), suppress.clone());

        let state = ClaudeSessionState {
            tx,
            rx: parking_lot::Mutex::new(Some(rx)),
            stdin: stdin_arc,
            cancel,
            pid,
            process: process_arc,
            pending_control,
            suppress_crash: suppress,
        };
        self.sessions.lock().insert(session_id.clone(), state);

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        let handle_state = SessionHandleState::Subprocess { pid };
        Ok(SessionHandle::new(session_id, "claude".to_string(), handle_state))
    }
}

fn parse_claude_event(line: &str, session_id: &SessionId) -> Vec<AgentEvent> {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return vec![AgentEvent::Output {
                session_id: session_id.clone(),
                content: format!("{}\n", line),
                ts: AgentEvent::now(),
                raw: None,
            }];
        }
    };

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");

    match event_type {
        "system" => vec![],
        "assistant" => {
            let parts_opt = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            let mut events: Vec<AgentEvent> = Vec::new();

            // NOTE: live UserQuestion events are emitted exclusively from the
            // control_request interceptor — not here. This branch only formats the
            // [ask_user_question] block below so history cards still render.

            let content = parts_opt
                .map(|parts| {
                    parts.iter()
                        .filter_map(|p| {
                            let ct = p.get("type").and_then(|t| t.as_str())?;
                            match ct {
                                "text" => p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()),
                                "thinking" => p.get("thinking").and_then(|t| t.as_str()).map(|s| format!("[thinking]{}[/thinking]", s)),
                                "tool_use" => {
                                    let id = p.get("id").and_then(|i| i.as_str()).unwrap_or("");
                                    let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                                    if name == "AskUserQuestion" {
                                        // Render as special block for history display;
                                        // id 嵌入规则与 history.rs 的打平格式一致
                                        let input_json = p.get("input").map(|v| serde_json::to_string(v).unwrap_or_default()).unwrap_or_default();
                                        return Some(if id.is_empty() {
                                            format!("[ask_user_question]{}[/ask_user_question]", input_json)
                                        } else {
                                            format!("[ask_user_question id=\"{}\"]{}[/ask_user_question]", id, input_json)
                                        });
                                    }
                                    let input = p.get("input").map(|v| serde_json::to_string(v).unwrap_or_default()).unwrap_or_default();
                                    let display = format!("[tool_use]{} {}[/tool_use]", name, input);
                                    Some(display)
                                }
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| line.to_string());

            events.push(AgentEvent::Output {
                session_id: session_id.clone(),
                content: format!("{}\n", content),
                ts: AgentEvent::now(),
                raw: Some(value.clone()),
            });
            events
        }
        "result" => vec![],
        "user" => {
            // Only emit output for tool_result content, not for user text input.
            // User text is echoed by --replay-user-messages and the frontend already
            // appends it manually when the user sends a message.
            let content = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|parts| {
                    let text = parts.iter()
                        .filter_map(|p| {
                            let ct = p.get("type").and_then(|t| t.as_str())?;
                            match ct {
                                "tool_result" => {
                                    let is_error = p.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                                    let content = p.get("content").and_then(|c| c.as_str())?;
                                    // Skip the SDK's auto-error response to AskUserQuestion
                                    if is_error && content == "Answer questions?" {
                                        return None;
                                    }
                                    let truncated = if content.len() > 2000 {
                                        // Byte 2000 may land mid-character (CJK is 3 bytes
                                        // in UTF-8) — back off to the nearest char boundary.
                                        let mut end = 2000;
                                        while !content.is_char_boundary(end) {
                                            end -= 1;
                                        }
                                        format!("{}...\n(truncated, {} chars total)", &content[..end], content.chars().count())
                                    } else {
                                        content.to_string()
                                    };
                                    // Include tool_use_id in tag so frontend can differentiate "选项" answers
                                    let tool_use_id = p.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
                                    if tool_use_id.is_empty() {
                                        Some(format!("[tool_result]{}[/tool_result]", truncated))
                                    } else {
                                        Some(format!("[tool_result tool_use_id=\"{}\"]{}[/tool_result]", tool_use_id, truncated))
                                    }
                                },
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.is_empty() {
                        None
                    } else {
                        Some(format!("{}\n", text))
                    }
                })
                .flatten();
            match content {
                Some(text) => vec![AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: text,
                    ts: AgentEvent::now(),
                    raw: Some(value.clone()),
                }],
                None => vec![],
            }
        }
        "control_request" | "control_response" => {
            // Normally intercepted in the reader pre-pass; this guard keeps the `_`
            // fallback from leaking a raw protocol line to the user as Output.
            vec![]
        }
        _ => vec![AgentEvent::Output {
            session_id: session_id.clone(),
            content: format!("{}\n", line),
            ts: AgentEvent::now(),
            raw: Some(value.clone()),
        }],
    }
}

/// A control_request awaiting a human decision (tool approval or AskUserQuestion).
/// One per session at a time — the CLI blocks until the control_response arrives.
#[derive(Debug, Clone)]
struct PendingControl {
    request_id: String,
    tool_name: String,
    input: serde_json::Value,
}

/// Build a control_response line for the in-band control protocol.
/// NOTE: `updatedInput` is camelCase — the one place the control protocol differs
/// from the hook world's snake_case `updated_input`; writing snake_case silently
/// fails at the CLI.
fn build_control_response(
    request_id: &str,
    behavior: &str,
    updated_input: Option<serde_json::Value>,
    deny_message: Option<&str>,
) -> serde_json::Value {
    let mut inner = serde_json::Map::new();
    inner.insert(
        "behavior".to_string(),
        serde_json::Value::String(behavior.to_string()),
    );
    if behavior == "allow" {
        inner.insert(
            "updatedInput".to_string(),
            updated_input.unwrap_or(serde_json::Value::Object(Default::default())),
        );
    }
    if let Some(msg) = deny_message {
        inner.insert("message".to_string(), serde_json::Value::String(msg.to_string()));
    }
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": serde_json::Value::Object(inner),
        }
    })
}

/// Approval policy: auto-allow everything, matching the scope of
/// --dangerously-skip-permissions. Exactly two kinds of requests still reach
/// a human: AskUserQuestion (it must be answered — an allow response would
/// hand Claude no answers and it would self-continue), and Bash commands hit
/// by the user-configured 保护指令 list (see `bash_requires_approval`).
pub(crate) fn approval_required(tool_name: &str, input: &serde_json::Value, protected: &[String]) -> bool {
    match tool_name {
        "AskUserQuestion" => true,
        "Bash" => input
            .get("command")
            .and_then(|c| c.as_str())
            .map(|cmd| bash_requires_approval(cmd, protected))
            .unwrap_or(false),
        _ => false,
    }
}

/// Split a command into segments on shell connectors (`&&`, `||`, `;`, `|`, `&`).
fn split_shell_segments(command: &str) -> Vec<String> {
    let masked = command.replace(">&", "\u{2}"); // fd duplication is not a connector
    // drop whole fd-duplication sequences (2>&1, >&1, >&2) so they don't pollute tokens
    let mut stripped = String::new();
    let mut chars = masked.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() && chars.peek() == Some(&'\u{2}') {
            chars.next();
            while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                chars.next();
            }
        } else if ch == '\u{2}' {
            while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                chars.next();
            }
        } else {
            stripped.push(ch);
        }
    }
    let mut out = String::new();
    let mut chars = stripped.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '&' | ';' | '|' => {
                if (ch == '&' || ch == '|') && chars.peek() == Some(&ch) {
                    chars.next();
                }
                out.push('\u{1}');
            }
            _ => out.push(ch),
        }
    }
    out.split('\u{1}')
        .map(|seg| seg.trim().to_string())
        .filter(|seg| !seg.is_empty())
        .collect()
}

/// A Bash command asks when ANY of its chain segments hits the 保护指令 list.
/// Approval granularity is one control_request per Bash invocation, so there
/// is no way to approve only the offending segment.
pub(crate) fn bash_requires_approval(command: &str, protected: &[String]) -> bool {
    split_shell_segments(command)
        .iter()
        .any(|seg| segment_requires_approval(seg, protected))
}

/// One pipeline segment (no connectors left). Leading NAME=VALUE assignments
/// are skipped so `FOO=1 rm -rf /` is still caught. Plain entries match the
/// program basename exactly, so `/bin/rm` hits `rm`. Entries containing `*`
/// or `?` use shell wildcard syntax against the whole segment (or the
/// basename), so `*delete*` also catches `kubectl delete pod a` and `kube*`
/// catches `/usr/local/bin/kubectl ...`; `*` alone protects every command.
fn segment_requires_approval(segment: &str, protected: &[String]) -> bool {
    if protected.is_empty() {
        return false;
    }
    let segment = segment.trim();
    if segment.is_empty() {
        return false;
    }
    let mut tokens = segment.split_whitespace().peekable();
    if tokens.peek() == Some(&"export") {
        return false;
    }
    let mut program = "";
    for tok in tokens {
        let is_assignment = tok
            .split_once('=')
            .map(|(name, _)| !name.is_empty())
            .unwrap_or(false);
        if is_assignment {
            continue;
        }
        program = tok;
        break;
    }
    let program = program.rsplit('/').next().unwrap_or(program);
    protected.iter().any(|p| {
        let p = p.trim();
        if p.contains('*') || p.contains('?') {
            wildcard_match(p, segment) || wildcard_match(p, program)
        } else {
            p == program
        }
    })
}

/// Shell-style wildcard match: `*` = any run (incl. empty), `?` = exactly one
/// char. Case-sensitive, no character classes. Greedy backtracking, worst case
/// O(len(pattern) × len(text)) — for the short strings seen here it is far
/// cheaper than compiling/running a regex per command.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

struct ClaudeSessionState {
    tx: mpsc::Sender<AgentEvent>,
    rx: parking_lot::Mutex<Option<mpsc::Receiver<AgentEvent>>>,
    stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
    cancel: CancellationToken,
    pid: u32,
    process: std::sync::Arc<TokioMutex<Option<Box<dyn Process>>>>,
    pending_control: std::sync::Arc<parking_lot::Mutex<Option<PendingControl>>>,
    /// 平台主动杀进程前置位，wait 任务看到后不再上报崩溃（自杀 ≠ 崩溃）
    suppress_crash: std::sync::Arc<parking_lot::Mutex<bool>>,
}

#[cfg(test)]
mod tests {
    use super::{
        approval_required, build_control_response, crash_reason, parse_claude_event,
        ClaudeAdapter,
    };
    use crate::agents::types::{AgentEvent, SessionId};
    use serde_json::json;

    #[test]
    fn crash_reason_classifies_signal_exits() {
        assert!(crash_reason(-1).contains("信号终止"));
        assert!(crash_reason(137).contains("SIGKILL"));
        assert!(crash_reason(143).contains("SIGTERM"));
        assert!(crash_reason(130).contains("SIGINT"));
        assert_eq!(crash_reason(1), "进程异常退出（exit code: 1）");
    }

    fn commands(entries: &[serde_json::Value]) -> Vec<String> {
        entries
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .filter_map(|h| h["command"].as_str())
            .map(str::to_string)
            .collect()
    }

    // Legacy HTTP hooks point at endpoints that no longer exist; the prune pass
    // must remove them while leaving the user's own command hooks alone.
    #[test]
    fn prune_removes_our_http_hooks_but_keeps_user_hooks() {
        let mut settings = json!({
            "model": "gpt-5",
            "hooks": {
                "PermissionRequest": [
                    { "matcher": ".*", "hooks": [
                        { "type": "http", "url": "http://127.0.0.1:3030/api/hooks/permission/org/proj/claude/old", "timeout": 300 }] },
                    { "matcher": ".*", "hooks": [{ "type": "command", "command": "echo mine" }] }
                ],
                "PreToolUse": [
                    { "matcher": "AskUserQuestion", "hooks": [
                        { "type": "http", "url": "/api/hooks/pre-tool/org/proj/claude/old", "timeout": 300 }] }
                ]
            }
        });

        assert!(ClaudeAdapter::prune_settings_value(&mut settings));

        assert_eq!(settings["model"], "gpt-5");
        let perm = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(commands(perm), vec!["echo mine"]);
        // the event key whose only hooks were ours disappears entirely
        assert!(settings["hooks"].get("PreToolUse").is_none());
    }

    // One hook entry can hold several hooks; ours has to go without taking the user's with it.
    #[test]
    fn prune_splits_mixed_hook_arrays() {
        let mut settings = json!({
            "hooks": {
                "PermissionRequest": [
                    { "matcher": ".*", "hooks": [
                        { "type": "http", "url": "http://127.0.0.1:3030/api/hooks/permission/org/proj/claude/old", "timeout": 300 },
                        { "type": "command", "command": "echo mine" }] }
                ]
            }
        });

        assert!(ClaudeAdapter::prune_settings_value(&mut settings));

        let entries = settings["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(commands(entries), vec!["echo mine"]);
    }

    // A settings file without any of our hooks is left byte-identical in meaning
    // (and the prune pass must not report a change, so no rewrite happens).
    #[test]
    fn prune_is_noop_without_our_hooks() {
        let mut settings = json!({
            "model": "deepseek-v4-flash",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }
                ]
            }
        });

        assert!(!ClaudeAdapter::prune_settings_value(&mut settings));
        assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "guard.sh");
    }

    // --- In-band control protocol (control_request / control_response) ---

    // Highest-risk guess point of the whole design: the wire bytes. `updatedInput`
    // must be camelCase (the control protocol's only divergence from the hook
    // world's snake_case), nested at response.response.updatedInput.
    #[test]
    fn control_response_allow_wire_format() {
        let input = json!({"questions": [{"question": "Which DB?", "header": "DB"}]});
        let msg = build_control_response("req-1", "allow", Some(input), None);

        assert_eq!(msg["type"], "control_response");
        assert_eq!(msg["response"]["subtype"], "success");
        assert_eq!(msg["response"]["request_id"], "req-1");
        assert_eq!(msg["response"]["response"]["behavior"], "allow");
        // camelCase, present; snake_case must NOT appear anywhere
        assert!(msg["response"]["response"].get("updatedInput").is_some());
        assert!(msg.to_string().find("updated_input").is_none());
        // original input rides through untouched
        assert_eq!(msg["response"]["response"]["updatedInput"]["questions"][0]["question"], "Which DB?");
    }

    #[test]
    fn control_response_deny_carries_message() {
        let msg = build_control_response("req-2", "deny", None, Some("not allowed"));
        assert_eq!(msg["response"]["response"]["behavior"], "deny");
        assert_eq!(msg["response"]["response"]["message"], "not allowed");
        assert!(msg["response"]["response"].get("updatedInput").is_none());
    }

    // AskUserQuestion answers: full mapping keyed by the FULL question text,
    // merged into the original input — not just questions.first().
    #[test]
    fn ask_user_question_answer_updated_input_shape() {
        let input = json!({
            "questions": [
                {"question": "Which DB?", "header": "DB", "options": [{"label": "MySQL", "description": ""}]},
                {"question": "Which cache?", "header": "Cache", "options": [{"label": "Redis", "description": ""}]}
            ]
        });
        let mut answers = std::collections::HashMap::new();
        answers.insert("Which DB?".to_string(), "MySQL".to_string());
        answers.insert("Which cache?".to_string(), "Redis".to_string());

        let mut updated = input.clone();
        updated.as_object_mut().unwrap()
            .insert("answers".to_string(), serde_json::to_value(&answers).unwrap());
        let msg = build_control_response("req-3", "allow", Some(updated), None);

        let out_answers = &msg["response"]["response"]["updatedInput"]["answers"];
        assert_eq!(out_answers["Which DB?"], "MySQL");
        assert_eq!(out_answers["Which cache?"], "Redis");
        // questions preserved alongside answers
        assert_eq!(msg["response"]["response"]["updatedInput"]["questions"].as_array().unwrap().len(), 2);
    }

    // Approval policy: everything auto-allowed except Bash commands hitting the
    // 保护指令 list — plain entries by program basename, `*`/`?` entries as
    // shell wildcards over the whole segment.
    #[test]
    fn approval_protected_basenames_and_wildcards_ask() {
        let default_list = crate::config::default_protected_commands();
        let asks_with = |cmd: &str, list: &[&str]| {
            approval_required(
                "Bash",
                &json!({"command": cmd}),
                &list.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        };
        let asks = |cmd: &str| asks_with(cmd, &["rm", "kubectl"]);
        let allows = |cmd: &str| !asks(cmd);

        // the shipped default: rm + kube/kubectl 全拦（含读操作，k8s 一律转人工）
        let asks_default = |cmd: &str| {
            approval_required("Bash", &json!({"command": cmd}), &default_list)
        };
        assert!(asks_default("rm -rf build"));
        assert!(asks_default("kubectl delete pod a"));
        assert!(asks_default("kubectl apply -f deploy.yaml"));
        assert!(asks_default("kubectl get pods"));
        assert!(asks_default("kubectl logs x"));
        assert!(asks_default("kubectl"));
        assert!(asks_default("kube delete pod a"));
        assert!(asks_default("/usr/local/bin/kubectl get pods"));
        assert!(!asks_default("git push --force origin master"));
        assert!(!asks_default("git reset --hard"));
        // a prefix entry needs the trailing `*` — a plain entry is basename-exact
        assert!(!asks_with("kubectl delete pod a", &["kubectl delete"]));
        assert!(asks_with("kubectl delete pod a", &["kubectl delete*"]));

        // everyday commands ride through
        assert!(allows("ls -la"));
        assert!(allows("curl example.com"));
        assert!(allows("git push --force origin master"));
        assert!(allows("hvigorw assembleHap 2>&1 | tail -3"));
        assert!(allows("k9s"));

        // basename matching catches absolute paths and env-prefixed forms
        assert!(asks("rm -rf build"));
        assert!(asks("/bin/rm -rf /"));
        assert!(asks("FOO=1 rm -rf /"));
        assert!(asks("kubectl delete pod a"));
        assert!(asks("/usr/local/bin/kubectl get pods"));

        // a chain asks as a whole when any segment matches
        assert!(asks("ls && rm -rf /"));
        assert!(asks("echo hi; kubectl delete pod a"));
        // mentions inside arguments are not program basenames
        assert!(allows("echo rm"));
        assert!(allows("grep rm file.txt"));

        // empty list protects nothing
        assert!(!asks_with("rm -rf /", &[]));
        // `*` protects everything
        assert!(asks_with("ls", &["*"]));
        assert!(asks_with("rm -rf /", &["*"]));
        // user-added entries work like the built-in ones
        assert!(asks_with("git reset --hard", &["git"]));
        assert!(asks_with("/usr/bin/git reset --hard", &["git"]));
        assert!(!asks_with("cargo test", &["git"]));

        // wildcard entries: `*`/`?` match the whole segment or the basename
        assert!(asks_with("kubectl delete pod a", &["*delete*"]));
        assert!(asks_with("helm delete demo", &["*delete*"]));
        assert!(!asks_with("kubectl get pods", &["*delete*"]));
        assert!(asks_with("kubectl get pods", &["kube*"]));
        // basename fallback lets a prefix pattern catch absolute paths too
        assert!(asks_with("/usr/local/bin/kubectl get pods", &["kube*"]));
        assert!(asks_with("rm -rf build", &["rm*"]));
        assert!(asks_with("FOO=1 rm -rf /", &["rm*"]));
        // `?` = exactly one char
        assert!(asks_with("wc -l", &["??"]));
        assert!(!asks_with("wc -l", &["?"]));
        // wildcards see arguments, so this broader entry does hit `echo rm`
        assert!(asks_with("echo rm", &["*rm*"]));

        assert!(approval_required("AskUserQuestion", &json!({}), &default_list));
        assert!(!asks_with("", &["rm", "kubectl"]));  // nothing to protect
        // every other tool auto-allowed (bypass-equivalent scope)
        assert!(!approval_required("Write", &json!({"file_path": "/tmp/x"}), &default_list));
        assert!(!approval_required("WebFetch", &json!({"url": "https://x"}), &default_list));
        assert!(!approval_required("Bash", &json!({}), &default_list));
    }

    // The reader pre-pass must swallow control_request lines; parse_claude_event is
    // the safety net so the `_` fallback can't leak protocol lines as user Output.
    #[test]
    fn parse_claude_event_never_emits_control_request() {
        let line = json!({
            "type": "control_request",
            "request_id": "27ec245d",
            "request": {"subtype": "can_use_tool", "tool_name": "AskUserQuestion",
                        "input": {"questions": [{"question": "q", "header": "h", "options": [], "multiSelect": false}]}}
        })
        .to_string();
        assert!(parse_claude_event(&line, &SessionId("s".into())).is_empty());
    }

    // Regression: tool_result truncation sliced at byte 2000, which panics when
    // the boundary lands inside a multi-byte UTF-8 char (CJK is 3 bytes).
    #[test]
    fn parse_claude_event_truncates_cjk_tool_result_without_panic() {
        let cjk: String = "汉".repeat(1500); // 4500 bytes, all 3-byte chars
        let line = json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": cjk}]}
        })
        .to_string();
        let events = parse_claude_event(&line, &SessionId("s".into()));
        assert!(!events.is_empty());
        let text = match &events[0] {
            AgentEvent::Output { content, .. } => content.clone(),
            other => panic!("expected Output, got {:?}", other),
        };
        assert!(text.contains("(truncated, 1500 chars total)"));
        // The kept prefix must be whole characters — no replacement garbage.
        let body = text.split("tool_use_id=\"t1\"]").nth(1).unwrap_or("");
        let kept = &body[..body.find("...").unwrap()];
        assert!(!kept.is_empty());
        assert!(kept.chars().all(|c| c == '汉'));
    }
}
