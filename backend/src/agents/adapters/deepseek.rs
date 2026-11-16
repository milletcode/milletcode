//! dsh（@deepseek-ai/dsh，DeepSeek Harness）适配器：Subprocess 形态，驱动
//! `dsh --profile acp` 长驻进程（标准 ACP v1 over NDJSON JSON-RPC 2.0 stdio）。
//!
//! 协议要点（实抓样本见 backend/tests/fixtures/dsh-acp-samples.jsonl）：
//! - 客户端请求：initialize / session/new / session/prompt / session/cancel /
//!   session/set_config_option / session/resume / session/close
//! - 服务端通知：session/update（update.sessionUpdate ∈ agent_message_chunk /
//!   agent_thought_chunk / tool_call / tool_call_update / usage_update …）
//! - 服务端请求：session/request_permission（须逐条回包，否则 dsh 整轮挂起）
//!
//! 与 codex 的同构点：stdout 流量聚合为 claude 形状的 raw（前端零改动）；
//! claude_session_id 槽位存 dsh sessionId（restore/resume 同句柄）；审批沿用
//! 平台策略（默认放行，仅保护名单转人工）。
//! 关键差异：session/prompt 的回包在整轮结束后才到（codex turn/start 秒回），
//! 因此 prompt 走 fire-and-forget + reader 按 current_prompt_id 结算；
//! 模型热切换是即时指令 session/set_config_option（codex 只能逐轮覆盖）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::agents::types::{ApprovalResponse, SessionId, SessionStatus};
use crate::agents::{AgentAdapter, AgentEvent, RuntimeContext, SessionHandle, SessionHandleState};
use crate::config::AgentConfig;
use crate::runtime::{LocalRuntime, Process, Runtime, SshRuntime};

/// 普通 JSON-RPC 请求（initialize / session/new / set_config_option …）回包超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// session/prompt 结算上限：LLM 整轮可能跑很久，超时只用于兜底防挂死。
const PROMPT_SETTLE_TIMEOUT: Duration = Duration::from_secs(1800);

pub struct DeepSeekAdapter {
    event_bus: std::sync::Arc<crate::events::EventBus>,
    config_manager: std::sync::Arc<crate::config::ConfigManager>,
    sessions: parking_lot::Mutex<HashMap<SessionId, DshSessionState>>,
}

struct DshSessionState {
    stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
    /// 只被消费一次：spawn_event_task 通过 output_stream 取走
    rx: parking_lot::Mutex<Option<mpsc::Receiver<AgentEvent>>>,
    /// 非 reader 路径（send_message/set_config）投递状态事件的双通道
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    kill_notify: std::sync::Arc<tokio::sync::Notify>,
    dead_rx: TokioMutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    alive: std::sync::Arc<AtomicBool>,
    suppress_crash: std::sync::Arc<parking_lot::Mutex<bool>>,
    next_id: std::sync::Arc<AtomicU64>,
    pending: std::sync::Arc<
        parking_lot::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>,
    >,
    /// 进行中的 session/prompt 请求 id：reader 据此结算整轮（fire-and-forget）
    current_prompt_id: std::sync::Arc<parking_lot::Mutex<Option<u64>>>,
    /// 停靠中的人工审批（同一时刻最多一个）
    pending_approval: std::sync::Arc<parking_lot::Mutex<Option<PendingDshApproval>>>,
    /// dsh 侧 sessionId（session/new 自报；claude_session_id 槽位存它）
    acp_session_id: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    /// 模型目录：model_id / 显示名 → 选项值（JSON 字符串 `["<provider>","<model>"]`）
    model_options: std::sync::Arc<parking_lot::Mutex<Vec<(String, String)>>>,
}

/// 停靠中的人工审批：jsonrpc_id 原样回显给 dsh。
#[derive(Debug, Clone)]
struct PendingDshApproval {
    jsonrpc_id: serde_json::Value,
    summary: String,
}

impl DeepSeekAdapter {
    pub fn new(
        event_bus: std::sync::Arc<crate::events::EventBus>,
        config_manager: std::sync::Arc<crate::config::ConfigManager>,
    ) -> Self {
        Self {
            event_bus,
            config_manager,
            sessions: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// 构建 `dsh --profile acp` 命令；extra_args 透传。
    fn build_command(config: &AgentConfig, ctx: &RuntimeContext) -> (String, Vec<String>) {
        let binary = if ctx.binary_path.is_empty() {
            "dsh".to_string()
        } else {
            ctx.binary_path.clone()
        };
        let mut args = vec!["--profile".to_string(), "acp".to_string()];
        args.extend(config.extra_args.iter().cloned());
        (binary, args)
    }

    /// OPENAI_API_KEY 仅在实例配置了 api_key 时注入；网关路由与 baseURL 在
    /// ~/.dsh/profiles/acp/cordis.patch.yml（pi-ai provider）里定义。
    fn build_env(config: &AgentConfig, ctx: &RuntimeContext) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(key) = config.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            env.push(("OPENAI_API_KEY".to_string(), key.to_string()));
        }
        env.extend(ctx.gitea_env.iter().cloned());
        env
    }

    /// 从 configOptions（session/new / set_config_option 回包）提取模型目录：
    /// 每个内层选项记两条（value 对里的 model id、显示名）→ 选项值。
    fn parse_model_options(config_options: &serde_json::Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for opt in config_options.as_array().into_iter().flatten() {
            if opt.get("id").and_then(|i| i.as_str()) != Some("model") {
                continue;
            }
            for group in opt.get("options").and_then(|o| o.as_array()).into_iter().flatten() {
                for inner in group.get("options").and_then(|o| o.as_array()).into_iter().flatten() {
                    let Some(value) = inner.get("value").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let display = inner.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    // value 是 JSON 字符串 '["<provider>","<model>"]'，取 model id 作键
                    if let Some(model_id) = parse_pair_model(value) {
                        out.push((model_id, value.to_string()));
                    }
                    if !display.is_empty() {
                        out.push((display.to_string(), value.to_string()));
                    }
                }
            }
        }
        out
    }

    /// currentValue（JSON 字符串对）里的 model id。
    fn current_model(config_options: &serde_json::Value) -> Option<String> {
        let opt = config_options
            .as_array()?
            .iter()
            .find(|o| o.get("id").and_then(|i| i.as_str()) == Some("model"))?;
        parse_pair_model(opt.get("currentValue").and_then(|v| v.as_str())?)
    }

    /// Spawn dsh 进程（本地 / ssh 同构），返回 (process, stdout, stdin, pid, stderr tail)。
    #[allow(clippy::type_complexity)]
    async fn spawn_dsh_process(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        working_dir: Option<&str>,
        log_suffix: &str,
    ) -> Result<(
        Box<dyn Process>,
        Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
        u32,
        std::sync::Arc<parking_lot::Mutex<String>>,
    )> {
        let (cmd, args) = Self::build_command(config, ctx);
        let env = Self::build_env(config, ctx);
        let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let mut process: Box<dyn Process> = if ctx.remote_mode == "ssh" {
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
            let runtime = LocalRuntime::new();
            runtime
                .spawn(
                    &cmd,
                    &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    working_dir,
                    env_refs,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Failed to spawn dsh process: {}. Is 'dsh' installed?", e))?
        };

        let pid = process.id();
        let stdout = process.take_stdout().ok_or_else(|| anyhow::anyhow!("stdout not available"))?;
        let stdin = process.take_stdin().ok_or_else(|| anyhow::anyhow!("stdin not available"))?;

        let stderr_tail: std::sync::Arc<parking_lot::Mutex<String>> =
            std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
        if let Some(stderr) = process.take_stderr() {
            let tail = stderr_tail.clone();
            let suffix = log_suffix.to_string();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    {
                        let mut t = tail.lock();
                        if t.len() > 8_000 {
                            t.clear();
                        }
                        if !t.is_empty() {
                            t.push('\n');
                        }
                        t.push_str(&line.chars().take(500).collect::<String>());
                    }
                    tracing::debug!(
                        line_preview = line.chars().take(200).collect::<String>(),
                        "dsh stderr {}",
                        suffix
                    );
                }
            });
        }

        Ok((process, stdout, stdin, pid, stderr_tail))
    }

    async fn write_stdin(
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let mut guard = stdin.lock().await;
        guard.write_all(format!("{}\n", payload).as_bytes()).await?;
        guard.flush().await?;
        Ok(())
    }

    /// 发送 JSON-RPC 请求并等待回包（不适用于 session/prompt——它整轮才回）。
    async fn request(
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        pending: &std::sync::Arc<
            parking_lot::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>,
        >,
        next_id: &std::sync::Arc<AtomicU64>,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
        pending.lock().insert(id, reply_tx);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = Self::write_stdin(stdin, frame).await {
            pending.lock().remove(&id);
            return Err(anyhow::anyhow!("dsh {} 写入失败: {}", method, e));
        }
        let response = match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                pending.lock().remove(&id);
                anyhow::bail!("dsh {} 回包通道关闭——进程可能已退出", method);
            }
            Err(_) => {
                pending.lock().remove(&id);
                anyhow::bail!("等待 dsh {} 回包超时（{}s）", method, timeout.as_secs());
            }
        };
        if let Some(err) = response.get("error") {
            anyhow::bail!("dsh {} 返回错误: {}", method, err);
        }
        Ok(response.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// 发送 session/prompt：fire-and-forget。整轮结束后 reader 按
    /// current_prompt_id 结算（回包 result.stopReason / error）。
    async fn send_prompt(
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        next_id: &std::sync::Arc<AtomicU64>,
        current_prompt_id: &std::sync::Arc<parking_lot::Mutex<Option<u64>>>,
        acp_session_id: &str,
        text: &str,
    ) -> Result<()> {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        current_prompt_id.lock().replace(id);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": acp_session_id,
                "prompt": [{ "type": "text", "text": text }],
            },
        });
        Self::write_stdin(stdin, frame).await
    }

    /// stdout 读取任务：JSON-RPC 帧分发 + 事件聚合 + prompt 结算。
    fn spawn_reader_task(
        stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        session_id: SessionId,
        tx: mpsc::Sender<AgentEvent>,
        event_bus: std::sync::Arc<crate::events::EventBus>,
        cancel: CancellationToken,
        stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        pending: std::sync::Arc<
            parking_lot::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>,
        >,
        current_prompt_id: std::sync::Arc<parking_lot::Mutex<Option<u64>>>,
        pending_approval: std::sync::Arc<parking_lot::Mutex<Option<PendingDshApproval>>>,
        protected_commands: Vec<String>,
        log_suffix: &str,
    ) {
        let session_for_reader = session_id.clone();
        let suffix = log_suffix.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut aggregator = DshAggregator::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if cancel.is_cancelled() {
                    break;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    tracing::debug!(
                        line_preview = line.chars().take(120).collect::<String>(),
                        "dsh stdout 非 JSON 行 {}",
                        suffix
                    );
                    continue;
                };
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let id = v.get("id").cloned();

                if !method.is_empty() {
                    if let Some(id) = id {
                        // server → client 请求（审批等）：必须回包
                        Self::handle_server_request(
                            &v,
                            id,
                            &stdin,
                            &pending_approval,
                            &tx,
                            &event_bus,
                            &session_for_reader,
                            &protected_commands,
                        )
                        .await;
                    } else {
                        // 通知：session/update → 聚合
                        if method == "session/update" {
                            let update = v.pointer("/params/update").cloned().unwrap_or(serde_json::Value::Null);
                            for event in aggregator.feed(&update, &session_for_reader) {
                                let _ = tx.send(event.clone()).await;
                                event_bus.emit(event);
                            }
                        }
                    }
                } else if let Some(id) = id {
                    let id_num = id.as_u64();
                    // prompt 结算优先于 pending 分发（prompt 不注册 oneshot）
                    let is_prompt = id_num.is_some_and(|n| {
                        current_prompt_id.lock().is_some_and(|p| p == n)
                    });
                    if is_prompt {
                        current_prompt_id.lock().take();
                        // 整轮结束仍未作答的审批作废：回 reject 释放 dsh
                        let stale = pending_approval.lock().take();
                        if let Some(a) = stale {
                            tracing::info!(session = %session_for_reader, "dsh approval dropped: turn ended without answer");
                            let _ = Self::write_stdin(&stdin, serde_json::json!({
                                "jsonrpc": "2.0", "id": a.jsonrpc_id,
                                "result": { "outcome": { "outcome": "reject" } }
                            })).await;
                        }
                        let mut events = aggregator.flush_all(&session_for_reader);
                        if let Some(err) = v.get("error") {
                            let message = err.get("message").and_then(|m| m.as_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| err.to_string());
                            events.push(AgentEvent::Error {
                                session_id: session_for_reader.clone(),
                                message,
                                ts: AgentEvent::now(),
                            });
                        } else {
                            let mut raw = v.get("result").cloned().unwrap_or(serde_json::json!({}));
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "type".to_string(),
                                    serde_json::Value::String("result".to_string()),
                                );
                            }
                            events.push(AgentEvent::Output {
                                session_id: session_for_reader.clone(),
                                content: String::new(),
                                ts: AgentEvent::now(),
                                raw: Some(raw),
                            });
                            events.push(AgentEvent::SessionStatus {
                                session_id: session_for_reader.clone(),
                                status: SessionStatus::Complete,
                                ts: AgentEvent::now(),
                            });
                        }
                        for event in events {
                            let _ = tx.send(event.clone()).await;
                            event_bus.emit(event);
                        }
                    } else if let Some(n) = id_num {
                        if let Some(reply_tx) = pending.lock().remove(&n) {
                            let _ = reply_tx.send(v);
                        }
                    }
                }
            }
            event_bus.emit(AgentEvent::SessionComplete {
                session_id: session_for_reader.clone(),
                exit_code: 0,
                ts: AgentEvent::now(),
            });
        });
    }

    /// server → client 请求处理。每条必须回包（dsh 在等回包期间整轮挂起）。
    /// 审批策略：请求文本命中保护名单 → 停靠转人工；其余直接放行（平台
    /// bypass 基线，dsh 沙箱自身已是第一道防线）。
    async fn handle_server_request(
        v: &serde_json::Value,
        id: serde_json::Value,
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        pending_approval: &std::sync::Arc<parking_lot::Mutex<Option<PendingDshApproval>>>,
        tx: &mpsc::Sender<AgentEvent>,
        event_bus: &std::sync::Arc<crate::events::EventBus>,
        session_id: &SessionId,
        protected: &[String],
    ) {
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
        fn reply(id: &serde_json::Value, outcome: &str) -> serde_json::Value {
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": { "outcome": outcome } } })
        }

        match method {
            "session/request_permission" => {
                let summary = permission_summary(&params);
                if !Self::permission_requires_approval(&params, protected) {
                    tracing::info!(summary = %summary, "dsh permission auto-allowed (not in protected commands)");
                    let _ = Self::write_stdin(stdin, reply(&id, "allow")).await;
                    return;
                }
                let request_id = jsonrpc_id_string(&id);
                let entry = PendingDshApproval {
                    jsonrpc_id: id.clone(),
                    summary: summary.clone(),
                };
                if pending_approval.lock().replace(entry).is_some() {
                    tracing::warn!(summary = %summary, "dsh approval already pending; auto-rejecting new request");
                    let _ = Self::write_stdin(stdin, reply(&id, "reject")).await;
                    return;
                }
                let event = AgentEvent::ApprovalRequest {
                    session_id: session_id.clone(),
                    prompt: format!("dsh 请求执行操作：\n{}", summary),
                    options: vec![
                        "Approve".to_string(),
                        "Deny".to_string(),
                        "Provide feedback...".to_string(),
                    ],
                    request_id: Some(request_id),
                    tool_name: Some("Bash".to_string()),
                    ts: AgentEvent::now(),
                };
                let _ = tx.send(event.clone()).await;
                event_bus.emit(event);
            }
            // 未知的 server 请求：回取消语义的空结果，别让 dsh 挂死
            _ => {
                tracing::warn!(method = %method, "dsh server request not supported; replying empty result");
                let _ = Self::write_stdin(stdin, serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": {}
                })).await;
            }
        }
    }

    /// 权限请求是否停靠：请求里能提取到的命令文本命中保护名单即转人工。
    fn permission_requires_approval(params: &serde_json::Value, protected: &[String]) -> bool {
        for val in params.as_object().into_iter().flatten().map(|(_, v)| v) {
            let text = val.as_str().unwrap_or_default();
            if !text.is_empty()
                && super::claude::approval_required(
                    "Bash",
                    &serde_json::json!({ "command": text }),
                    protected,
                )
            {
                return true;
            }
        }
        // 请求里没有任何可判定的命令文本：保持放行（dsh 沙箱已拦过危险面）
        false
    }

    /// 进程退出监视任务：与 codex 同构（崩溃上报 + 平台杀进程抑制）。
    #[allow(clippy::too_many_arguments)]
    fn spawn_wait_task(
        kill_notify: std::sync::Arc<tokio::sync::Notify>,
        dead_tx: tokio::sync::oneshot::Sender<()>,
        alive: std::sync::Arc<AtomicBool>,
        suppress_crash: std::sync::Arc<parking_lot::Mutex<bool>>,
        stderr_tail: std::sync::Arc<parking_lot::Mutex<String>>,
        event_bus: std::sync::Arc<crate::events::EventBus>,
        session_id: SessionId,
        process: Box<dyn Process>,
    ) {
        tokio::spawn(async move {
            let mut p = process;
            tokio::select! {
                status = p.wait() => {
                    tracing::info!(?status, "dsh process exited");
                    let code = status.as_ref().ok().copied();
                    let crashed = code != Some(0);
                    if code.is_none() {
                        event_bus.emit(AgentEvent::SessionStatus {
                            session_id: session_id.clone(),
                            status: SessionStatus::Error,
                            ts: AgentEvent::now(),
                        });
                    }
                    if crashed {
                        if *suppress_crash.lock() {
                            tracing::info!(session = %session_id, code = ?code, "dsh exit suppressed: platform-initiated stop/resume");
                        } else {
                            let code = code.unwrap_or(-1);
                            let reason = super::claude::crash_reason(code);
                            let tail = crate::sanitize::sanitize_public_error(stderr_tail.lock().trim());
                            let message = if tail.is_empty() {
                                reason
                            } else {
                                format!("{reason}。stderr 输出：\n{tail}")
                            };
                            let error_event = AgentEvent::Error {
                                session_id: session_id.clone(),
                                message,
                                ts: AgentEvent::now(),
                            };
                            event_bus.emit(error_event);
                        }
                    }
                }
                _ = kill_notify.notified() => {
                    if let Err(e) = p.kill().await {
                        tracing::warn!(session = %session_id, error = %e, "dsh kill failed");
                    }
                    let _ = p.wait().await;
                    tracing::info!(session = %session_id, "dsh process killed by stop");
                }
            }
            alive.store(false, Ordering::Release);
            let _ = dead_tx.send(());
        });
    }

    /// 拉起进程并完成 initialize + session/new（或 session/resume）。
    /// prompt 参数：新会话带初始 prompt；恢复会话传 None（由调用方决定后续）。
    async fn establish_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: &SessionId,
        working_dir: Option<&str>,
        initial_prompt: Option<&str>,
        resume_acp_session_id: Option<&str>,
        resume_model: Option<&str>,
        log_suffix: &str,
    ) -> Result<SessionHandle> {
        let cwd = working_dir
            .map(str::to_string)
            .filter(|d| !d.is_empty())
            .or_else(|| dirs::home_dir().map(|h| h.to_string_lossy().to_string()))
            .ok_or_else(|| anyhow::anyhow!("dsh session/new 需要工作目录（working_dir 与 home 均缺失）"))?;

        let (process, stdout, stdin, pid, stderr_tail) =
            self.spawn_dsh_process(config, ctx, Some(&cwd), log_suffix).await?;
        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let next_id = std::sync::Arc::new(AtomicU64::new(1));
        let current_prompt_id = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let pending_approval = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let acp_session_id = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let model_options = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();

        let protected = self.config_manager.config().permissions.protected_commands.clone();
        Self::spawn_reader_task(
            stdout,
            session_id.clone(),
            tx.clone(),
            self.event_bus.clone(),
            cancel.clone(),
            stdin_arc.clone(),
            pending.clone(),
            current_prompt_id.clone(),
            pending_approval.clone(),
            protected,
            log_suffix,
        );

        let kill_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let (dead_tx, dead_rx) = tokio::sync::oneshot::channel();
        let alive = std::sync::Arc::new(AtomicBool::new(true));
        let suppress_crash = std::sync::Arc::new(parking_lot::Mutex::new(false));
        Self::spawn_wait_task(
            kill_notify.clone(),
            dead_tx,
            alive.clone(),
            suppress_crash.clone(),
            stderr_tail,
            self.event_bus.clone(),
            session_id.clone(),
            process,
        );

        Self::request(
            &stdin_arc,
            &pending,
            &next_id,
            "initialize",
            serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {} }),
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| anyhow::anyhow!("dsh initialize 失败: {}", e))?;

        // session/new（新会话）或 session/resume（恢复）；两者回包都带 configOptions
        let result = match resume_acp_session_id {
            Some(sid) if !sid.is_empty() => {
                Self::request(
                    &stdin_arc,
                    &pending,
                    &next_id,
                    "session/resume",
                    serde_json::json!({ "sessionId": sid }),
                    REQUEST_TIMEOUT,
                )
                .await
                .map_err(|e| anyhow::anyhow!("dsh session/resume 失败: {}", e))?
            }
            _ => {
                Self::request(
                    &stdin_arc,
                    &pending,
                    &next_id,
                    "session/new",
                    serde_json::json!({ "cwd": cwd, "mcpServers": [] }),
                    REQUEST_TIMEOUT,
                )
                .await
                .map_err(|e| anyhow::anyhow!("dsh session/new 失败: {}", e))?
            }
        };

        let new_sid = result.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if new_sid.is_empty() {
            anyhow::bail!("dsh session 建立失败：回包缺少 sessionId");
        }
        *acp_session_id.lock() = Some(new_sid.clone());
        let config_options = result.get("configOptions").cloned().unwrap_or(serde_json::Value::Null);
        *model_options.lock() = Self::parse_model_options(&config_options);
        tracing::info!(session = %session_id, acp_session = %new_sid, "dsh session established");

        self.sessions.lock().insert(
            session_id.clone(),
            DshSessionState {
                stdin: stdin_arc.clone(),
                rx: parking_lot::Mutex::new(Some(rx)),
                event_tx: tx,
                cancel,
                kill_notify,
                dead_rx: TokioMutex::new(Some(dead_rx)),
                alive,
                suppress_crash,
                next_id: next_id.clone(),
                pending,
                current_prompt_id: current_prompt_id.clone(),
                pending_approval,
                acp_session_id,
                model_options,
            },
        );

        // 恢复会话时若指定了模型，立即切换（resume 默认还原上次选择）
        if resume_acp_session_id.is_some() {
            if let Some(m) = resume_model.map(str::trim).filter(|m| !m.is_empty()) {
                let handle = SessionHandle::new(session_id.clone(), "dsh".to_string(), SessionHandleState::Subprocess { pid });
                if let Err(e) = self.apply_model(&handle, m).await {
                    tracing::warn!(session = %session_id, error = %e, "dsh resume model switch failed");
                }
            }
        }

        // 上报当前模型（UI 侧选择器对齐；record.model 由 config_changed 持久化）
        if let Some(model) = Self::current_model(&config_options) {
            let event = AgentEvent::ConfigChanged {
                session_id: session_id.clone(),
                model: Some(model),
                provider: None,
                base_url: None,
                ts: AgentEvent::now(),
            };
            self.emit_config_event(session_id, event).await;
        }

        // 新会话带初始 prompt：进程已就绪，直接下发
        if let Some(prompt) = initial_prompt.filter(|p| !p.trim().is_empty()) {
            Self::send_prompt(&stdin_arc, &next_id, &current_prompt_id, &new_sid, prompt).await?;
        }

        Ok(SessionHandle::new(
            session_id.clone(),
            "dsh".to_string(),
            SessionHandleState::Subprocess { pid },
        ))
    }

    /// ConfigChanged 双发：通道喂 session_manager 转发循环（record.model 持久化 + WS），
    /// bus 兜底直连订阅者；establish 阶段通道可能尚未被消费，try_send 满了就只走 bus。
    async fn emit_config_event(&self, session_id: &SessionId, event: AgentEvent) {
        let event_tx = self
            .sessions
            .lock()
            .get(session_id)
            .map(|s| s.event_tx.clone());
        if let Some(tx) = event_tx {
            let _ = tx.try_send(event.clone());
        }
        self.event_bus.emit(event);
    }

    /// set_config_option(configId=model) + ConfigChanged 回执。
    async fn apply_model(&self, session: &SessionHandle, model: &str) -> Result<()> {
        let (stdin, event_tx, pending, next_id, acp_session_id, model_options, event_bus) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (
                state.stdin.clone(),
                state.event_tx.clone(),
                state.pending.clone(),
                state.next_id.clone(),
                state_acp_sid(state),
                state.model_options.clone(),
                self.event_bus.clone(),
            )
        };
        if acp_session_id.is_empty() {
            anyhow::bail!("dsh session has no acp session id");
        }
        let model = model.trim();
        // 选项值是 JSON 字符串对 '["<provider>","<model>"]'：按 model id / 显示名查表
        let value = model_options
            .lock()
            .iter()
            .find(|(k, _)| k == model)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| serde_json::to_string(&serde_json::json!([null, model])).unwrap());
        Self::request(
            &stdin,
            &pending,
            &next_id,
            "session/set_config_option",
            serde_json::json!({
                "sessionId": acp_session_id,
                "configId": "model",
                "value": value,
            }),
            REQUEST_TIMEOUT,
        )
        .await?;
        let event = AgentEvent::ConfigChanged {
            session_id: session.id.clone(),
            model: Some(model.to_string()),
            provider: None,
            base_url: None,
            ts: AgentEvent::now(),
        };
        let _ = event_tx.send(event.clone()).await;
        event_bus.emit(event);
        Ok(())
    }
}

/// JSON-RPC 请求 id → 平台审批 request_id 字符串。
fn jsonrpc_id_string(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 解析 '["<provider>","<model>"]' JSON 字符串里的 model id。
fn parse_pair_model(value: &str) -> Option<String> {
    let pair: Vec<String> = serde_json::from_str(value).ok()?;
    let model = pair.into_iter().nth(1)?;
    if model.is_empty() {
        None
    } else {
        Some(model)
    }
}

/// 权限请求的人类可读摘要（选项名 + 元数据里能找到的文本）。
fn permission_summary(params: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    if let Some(options) = params.get("options").and_then(|o| o.as_array()) {
        let names: Vec<String> = options
            .iter()
            .filter_map(|o| o.get("name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        if !names.is_empty() {
            parts.push(names.join(" / "));
        }
    }
    if let Some(meta) = params.get("metadata") {
        let text = serde_json::to_string(meta).unwrap_or_default();
        if !text.is_empty() && text != "{}" {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        serde_json::to_string(params).unwrap_or_default()
    } else {
        parts.join("\n")
    }
}

/// 辅助：取 state 内 acp sessionId（临时锁守卫限定在语句内，避免借用外泄）。
fn state_acp_sid(state: &DshSessionState) -> String {
    state.acp_session_id.lock().clone().unwrap_or_default()
}

/// dsh session/update → 平台 AgentEvent 的聚合器（claude 形状 raw，与 codex 同构）。
struct DshAggregator {
    thinking: String,
    thinking_msg: Option<String>,
    text: String,
    text_msg: Option<String>,
    seq: u64,
}

impl DshAggregator {
    fn new() -> Self {
        Self {
            thinking: String::new(),
            thinking_msg: None,
            text: String::new(),
            text_msg: None,
            seq: 0,
        }
    }

    fn assistant_raw(&mut self, block: serde_json::Value) -> serde_json::Value {
        self.seq += 1;
        serde_json::json!({
            "type": "assistant",
            "uuid": format!("dsh-{}", self.seq),
            "message": { "content": [block] },
        })
    }

    /// 进一条 session/update.update，出零或多个 AgentEvent。未知 sessionUpdate
    /// 变体 → 空 Vec（吞掉，避免泄漏原始帧给用户）。
    fn feed(&mut self, update: &serde_json::Value, session_id: &SessionId) -> Vec<AgentEvent> {
        let kind = update.get("sessionUpdate").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            // 用户消息回显：平台发送侧已入账，跳过避免重复
            "user_message_chunk" => vec![],
            "agent_message_chunk" => {
                let delta = update.pointer("/content/text").and_then(|t| t.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return vec![];
                }
                let msg_id = update.get("messageId").and_then(|m| m.as_str()).unwrap_or("");
                let mut events = self.flush_thinking(session_id);
                if !msg_id.is_empty() && self.text_msg.as_deref() != Some(msg_id) {
                    events.extend(self.flush_text(session_id));
                    self.text_msg = Some(msg_id.to_string());
                }
                self.text.push_str(delta);
                events
            }
            "agent_thought_chunk" => {
                let delta = update.pointer("/content/text").and_then(|t| t.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return vec![];
                }
                let msg_id = update.get("messageId").and_then(|m| m.as_str()).unwrap_or("");
                let mut events = self.flush_text(session_id);
                if !msg_id.is_empty() && self.thinking_msg.as_deref() != Some(msg_id) {
                    events.extend(self.flush_thinking(session_id));
                    self.thinking_msg = Some(msg_id.to_string());
                }
                self.thinking.push_str(delta);
                events
            }
            "tool_call" => {
                let Some(tool_call_id) = update.get("toolCallId").and_then(|t| t.as_str()) else {
                    return vec![];
                };
                let title = update.get("title").and_then(|t| t.as_str()).unwrap_or("tool");
                let input = update.get("rawInput").cloned().unwrap_or(serde_json::Value::Null);
                let mut events = self.flush_all(session_id);
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                let raw = self.assistant_raw(serde_json::json!({
                    "type": "tool_use",
                    "id": tool_call_id,
                    "name": title,
                    "input": input,
                }));
                events.push(AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: format!("[tool_use]{} {}[/tool_use]", title, input_str),
                    ts: AgentEvent::now(),
                    raw: Some(raw),
                });
                events
            }
            "tool_call_update" => {
                let Some(tool_call_id) = update.get("toolCallId").and_then(|t| t.as_str()) else {
                    return vec![];
                };
                let status = update.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let content = tool_update_content(update);
                let is_error = status == "failed";
                // 中间态（in_progress/pending）无内容时不发卡片，等终态
                if content.is_empty() && !matches!(status, "completed" | "failed") {
                    return vec![];
                }
                let content: String = content.chars().take(4_000).collect();
                let raw = serde_json::json!({
                    "type": "user",
                    "uuid": format!("dsh-tr-{}", tool_call_id),
                    "message": { "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
                        "is_error": is_error,
                    }] },
                });
                vec![AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: format!("[tool_result tool_use_id=\"{}\"]{}[/tool_result]", tool_call_id, content),
                    ts: AgentEvent::now(),
                    raw: Some(raw),
                }]
            }
            // usage / commands / mode / plan 等：v1 不上屏
            _ => vec![],
        }
    }

    fn flush_thinking(&mut self, session_id: &SessionId) -> Vec<AgentEvent> {
        if self.thinking.is_empty() {
            return vec![];
        }
        let content = std::mem::take(&mut self.thinking);
        self.thinking_msg = None;
        let raw = self.assistant_raw(serde_json::json!({
            "type": "thinking",
            "thinking": content,
        }));
        vec![AgentEvent::Output {
            session_id: session_id.clone(),
            content: format!("[thinking]{}[/thinking]", content),
            ts: AgentEvent::now(),
            raw: Some(raw),
        }]
    }

    fn flush_text(&mut self, session_id: &SessionId) -> Vec<AgentEvent> {
        if self.text.is_empty() {
            return vec![];
        }
        let content = std::mem::take(&mut self.text);
        self.text_msg = None;
        let raw = self.assistant_raw(serde_json::json!({
            "type": "text",
            "text": content,
        }));
        vec![AgentEvent::Output {
            session_id: session_id.clone(),
            content,
            ts: AgentEvent::now(),
            raw: Some(raw),
        }]
    }

    fn flush_all(&mut self, session_id: &SessionId) -> Vec<AgentEvent> {
        let mut events = self.flush_thinking(session_id);
        events.extend(self.flush_text(session_id));
        events
    }
}

/// tool_call_update 的 content 数组 → 纯文本。
fn tool_update_content(update: &serde_json::Value) -> String {
    let mut out = String::new();
    for item in update.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
        if let Some(text) = item.pointer("/content/text").and_then(|t| t.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[async_trait]
impl AgentAdapter for DeepSeekAdapter {
    fn adapter_id(&self) -> &str {
        "dsh"
    }

    async fn start_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        _claude_session_id: &str,
        _session_key: &str,
        prompt: &str,
        working_dir: Option<String>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, working_dir = ?working_dir, "Starting dsh acp session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let handle = self
            .establish_session(
                config,
                ctx,
                &session_id,
                working_dir.as_deref(),
                Some(prompt),
                None,
                None,
                "",
            )
            .await?;

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(handle)
    }

    async fn send_message(&self, session: &SessionHandle, message: &str) -> Result<()> {
        let (stdin, event_tx, next_id, current_prompt_id, acp_session_id) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (
                state.stdin.clone(),
                state.event_tx.clone(),
                state.next_id.clone(),
                state.current_prompt_id.clone(),
                state_acp_sid(state),
            )
        };
        if acp_session_id.is_empty() {
            anyhow::bail!("dsh session has no acp session id");
        }
        Self::send_prompt(&stdin, &next_id, &current_prompt_id, &acp_session_id, message).await?;
        // 轮次开始：翻回 Running（prompt 结算时已置 Complete）
        let event = AgentEvent::SessionStatus {
            session_id: session.id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        };
        let _ = event_tx.send(event.clone()).await;
        self.event_bus.emit(event);
        Ok(())
    }

    async fn set_config(
        &self,
        session: &SessionHandle,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
        provider: Option<&str>,
    ) -> Result<()> {
        let has_any = [model, base_url, api_key, provider]
            .iter()
            .any(|f| f.map(|v| !v.trim().is_empty()).unwrap_or(false));
        if !has_any {
            anyhow::bail!("set_config: 至少需要提供一个字段（model/base_url/api_key/provider）");
        }
        if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
            self.apply_model(session, m).await
        } else {
            tracing::warn!(session = %session.id, "dsh set_config: only model is supported; ignoring other fields");
            Ok(())
        }
    }

    async fn handle_approval(
        &self,
        session: &SessionHandle,
        request_id: Option<&str>,
        response: ApprovalResponse,
    ) -> Result<()> {
        let (stdin, pending_approval) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (state.stdin.clone(), state.pending_approval.clone())
        };

        let entry = pending_approval.lock().take();
        let Some(entry) = entry else {
            anyhow::bail!("No pending permission request for session {}", session.id);
        };
        let current_id = jsonrpc_id_string(&entry.jsonrpc_id);
        if let Some(rid) = request_id {
            if rid != current_id {
                *pending_approval.lock() = Some(entry);
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
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": entry.jsonrpc_id,
            "result": { "outcome": { "outcome": if approve { "allow" } else { "reject" } } },
        });
        Self::write_stdin(&stdin, frame).await?;

        tracing::info!(
            session = %session.id,
            request_id = %current_id,
            summary = %entry.summary,
            approved = approve,
            "dsh approval decision sent"
        );
        Ok(())
    }

    async fn interrupt(&self, session: &SessionHandle) -> Result<()> {
        let (stdin, pending, next_id, acp_session_id, current_prompt_id) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            let prompt_active = state.current_prompt_id.lock().is_some();
            (
                state.stdin.clone(),
                state.pending.clone(),
                state.next_id.clone(),
                state_acp_sid(state),
                prompt_active,
            )
        };
        if acp_session_id.is_empty() || !current_prompt_id {
            tracing::info!(session = %session.id, "dsh interrupt: no active prompt");
            return Ok(());
        }
        // 回包超时不视为错误：prompt 可能恰好完成
        let _ = Self::request(
            &stdin,
            &pending,
            &next_id,
            "session/cancel",
            serde_json::json!({ "sessionId": acp_session_id }),
            REQUEST_TIMEOUT,
        )
        .await;
        tracing::info!(session = %session.id, "dsh interrupt sent");
        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, _force: bool) -> Result<()> {
        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session.id.clone(),
            status: SessionStatus::Stopped,
            ts: AgentEvent::now(),
        });

        let Some(state) = self.sessions.lock().remove(&session.id) else {
            tracing::info!(session = %session.id, "dsh stop: no live adapter state (process may have already exited)");
            return Ok(());
        };
        state.cancel.cancel();
        *state.suppress_crash.lock() = true;
        // dsh 会话由服务端持久化（session/list 可再 resume），直接硬杀
        state.kill_notify.notify_one();
        Ok(())
    }

    fn has_process(&self, session_id: &SessionId) -> bool {
        let sessions = self.sessions.lock();
        sessions
            .get(session_id)
            .map(|state| state.alive.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    fn output_stream(
        &self,
        session: &SessionHandle,
    ) -> Box<dyn Stream<Item = AgentEvent> + Send + Unpin> {
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
        _session_key: &str,
        working_dir: Option<String>,
        model: Option<&str>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, acp = %claude_session_id, "Restoring dsh session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let handle = self
            .establish_session(
                config,
                ctx,
                &session_id,
                working_dir.as_deref(),
                None,
                Some(claude_session_id),
                model,
                "(restored)",
            )
            .await?;

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(handle)
    }

    async fn resume_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        _session_key: &str,
        working_dir: Option<String>,
        model: Option<&str>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, acp = %claude_session_id, "Resuming dsh session with new config");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Restarting,
            ts: AgentEvent::now(),
        });

        // 先回收旧进程（resume 默认还原上次模型；指定 model 时 establish 内切）
        let existing = self.sessions.lock().remove(&session_id);
        if let Some(state) = existing {
            state.cancel.cancel();
            *state.suppress_crash.lock() = true;
            state.kill_notify.notify_one();
            let dead_rx = state.dead_rx.lock().await.take();
            if let Some(rx) = dead_rx {
                let _ = rx.await;
            }
        }

        self.restore_session(config, ctx, session_id, claude_session_id, _session_key, working_dir, model)
            .await
    }

    /// dsh 的会话句柄：agent 侧 sessionId（restore/resume 同句柄）。
    fn session_ref(&self, session_id: &SessionId) -> Option<String> {
        let sessions = self.sessions.lock();
        let state = sessions.get(session_id)?;
        Some(state_acp_sid(state))
    }

    fn suppress_crash_report(&self, session_id: &SessionId) {
        if let Some(state) = self.sessions.lock().get(session_id) {
            *state.suppress_crash.lock() = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RuntimeContext {
        RuntimeContext {
            hostname: String::new(),
            remote_mode: String::new(),
            binary_path: String::new(),
            ssh_config_path: String::new(),
            ssh_passphrase: None,
            ssh_password: None,
            gitea_env: vec![],
        }
    }

    fn config() -> AgentConfig {
        AgentConfig {
            name: "DeepSeek Harness".to_string(),
            extra_args: vec![],
            api_key: Some("sk-test".to_string()),
            auto_approve: true,
            enabled: true,
            allow_config_overwrite: true,
            path: None,
            remote_path: None,
            release_channel: "stable".to_string(),
        }
    }

    fn sid() -> SessionId {
        SessionId::new("k/p/dsh/s".to_string())
    }

    #[test]
    fn build_command_uses_profile_acp_and_extra_args() {
        let (cmd, args) = DeepSeekAdapter::build_command(&config(), &ctx());
        assert_eq!(cmd, "dsh");
        assert_eq!(args, vec!["--profile", "acp"]);

        let mut cfg = config();
        cfg.extra_args = vec!["--verbose".to_string()];
        let (_, args) = DeepSeekAdapter::build_command(&cfg, &ctx());
        assert_eq!(args, vec!["--profile", "acp", "--verbose"]);
    }

    #[test]
    fn build_command_prefers_binary_path() {
        let mut c = ctx();
        c.binary_path = "/data/data/com.termux/files/usr/bin/dsh".to_string();
        let (cmd, args) = DeepSeekAdapter::build_command(&config(), &c);
        assert_eq!(cmd, "/data/data/com.termux/files/usr/bin/dsh");
        assert_eq!(args, vec!["--profile", "acp"]);
    }

    #[test]
    fn build_env_injects_api_key_and_gitea_env() {
        let mut c = ctx();
        c.gitea_env = vec![("GITEA_TOKEN".to_string(), "tok".to_string())];
        let env = DeepSeekAdapter::build_env(&config(), &c);
        assert!(env.contains(&("OPENAI_API_KEY".to_string(), "sk-test".to_string())));
        assert!(env.contains(&("GITEA_TOKEN".to_string(), "tok".to_string())));
    }

    #[test]
    fn build_env_skips_empty_api_key() {
        let mut cfg = config();
        cfg.api_key = None;
        let env = DeepSeekAdapter::build_env(&cfg, &ctx());
        assert!(env.is_empty());
    }

    #[test]
    fn parse_pair_model_extracts_model_id() {
        assert_eq!(
            parse_pair_model(r#"["milletcode","glm-5.3-flash"]"#),
            Some("glm-5.3-flash".to_string())
        );
        assert_eq!(parse_pair_model(r#"["deepseek-official","deepseek-v4-flash"]"#).as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(parse_pair_model("not-json"), None);
        assert_eq!(parse_pair_model(r#"["only-provider"]"#), None);
    }

    #[test]
    fn parse_model_options_from_session_new_shape() {
        // 形状取自 backend/tests/fixtures/dsh-acp-samples.jsonl
        let config_options = serde_json::json!([{
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "[\"milletcode\",\"glm-5.3-flash\"]",
            "options": [{
                "group": "milletcode", "name": "MilletCode Gateway",
                "options": [
                    { "value": "[\"milletcode\",\"glm-5.3-flash\"]", "name": "GLM 5.3 Flash" },
                    { "value": "[\"milletcode\",\"glm-5.3\"]", "name": "GLM 5.3" },
                ]
            }]
        }, {
            "id": "reasoning_effort", "name": "Reasoning effort", "category": "thought_level",
            "type": "select", "currentValue": "high", "options": []
        }]);
        let opts = DeepSeekAdapter::parse_model_options(&config_options);
        let lookup = |k: &str| opts.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(
            lookup("glm-5.3-flash").as_deref(),
            Some(r#"["milletcode","glm-5.3-flash"]"#)
        );
        assert_eq!(
            lookup("GLM 5.3").as_deref(),
            Some(r#"["milletcode","glm-5.3"]"#)
        );
        // reasoning_effort 选项不进模型目录
        assert!(lookup("high").is_none());

        assert_eq!(
            DeepSeekAdapter::current_model(&config_options).as_deref(),
            Some("glm-5.3-flash")
        );
    }

    #[test]
    fn aggregator_message_chunks_flush_on_message_boundary() {
        let mut agg = DshAggregator::new();
        let s = sid();
        // thinking → text → 新 messageId：先吐 thinking 再吐前一段 text
        let mut events = agg.feed(
            &serde_json::json!({"sessionUpdate":"agent_thought_chunk","messageId":"t1","content":{"type":"text","text":"thinking..."}}),
            &s,
        );
        assert!(events.is_empty());
        events = agg.feed(
            &serde_json::json!({"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"hello "}}),
            &s,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Output { content, .. } if content.contains("[thinking]thinking...")));
        events = agg.feed(
            &serde_json::json!({"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"world"}}),
            &s,
        );
        assert!(events.is_empty());
        events = agg.feed(
            &serde_json::json!({"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{"type":"text","text":"second"}}),
            &s,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Output { content, .. } if content == "hello world"));
        let rest = agg.flush_all(&s);
        assert_eq!(rest.len(), 1);
        assert!(matches!(&rest[0], AgentEvent::Output { content, .. } if content == "second"));
    }

    #[test]
    fn aggregator_tool_call_and_update() {
        let mut agg = DshAggregator::new();
        let s = sid();
        let events = agg.feed(
            &serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"call_x|call_x","title":"write","kind":"other","status":"in_progress","rawInput":{"content":"dsh was here.\n","file_path":"/tmp/hello.txt"}}),
            &s,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Output { content, .. } if content.contains("[tool_use]write") && content.contains("hello.txt")));

        // 中间态无内容：不出卡片
        let events = agg.feed(
            &serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"call_x|call_x","status":"in_progress"}),
            &s,
        );
        assert!(events.is_empty());

        // 终态：tool_result
        let events = agg.feed(
            &serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"call_x|call_x","status":"completed","content":[{"type":"content","content":{"type":"text","text":"perm-test\n"}}]}),
            &s,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AgentEvent::Output { content, .. } if content.contains("[tool_result tool_use_id=\"call_x|call_x\"]") && content.contains("perm-test")));
        if let AgentEvent::Output { raw: Some(raw), .. } = &events[0] {
            assert_eq!(raw["message"]["content"][0]["tool_use_id"], "call_x|call_x");
            assert_eq!(raw["message"]["content"][0]["is_error"], false);
        } else {
            panic!("expected Output with raw");
        }
    }

    #[test]
    fn aggregator_ignores_usage_and_user_echo_and_unknown() {
        let mut agg = DshAggregator::new();
        let s = sid();
        assert!(agg.feed(&serde_json::json!({"sessionUpdate":"usage_update","used":7380,"size":1000000}), &s).is_empty());
        assert!(agg.feed(&serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"echo"}}), &s).is_empty());
        assert!(agg.feed(&serde_json::json!({"sessionUpdate":"whatever_new_thing"}), &s).is_empty());
    }

    #[test]
    fn permission_summary_joins_option_names() {
        let params = serde_json::json!({
            "sessionId": "abc",
            "options": [
                { "optionId": "allow", "name": "Allow once", "kind": "tool_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject" },
            ]
        });
        let summary = permission_summary(&params);
        assert!(summary.contains("Allow once"));
        assert!(summary.contains("Reject"));
    }
}
