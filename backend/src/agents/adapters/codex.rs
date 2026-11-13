//! codex（OpenAI Codex CLI）适配器：Subprocess 形态，驱动 `codex app-server` 长驻进程。
//!
//! 协议（codex-cli app-server，NDJSON JSON-RPC 2.0 over stdio；schema 可由
//! `codex app-server generate-json-schema --out <dir>` 导出核对）：
//! - 客户端请求：initialize / thread/start / thread/resume / turn/start / turn/interrupt
//! - 服务端通知：thread/started（thread 信息在此送达，回包为空对象）、turn/started、
//!   item/*（agentMessage、reasoning、commandExecution、fileChange、mcpToolCall…）、
//!   turn/completed、error
//! - 服务端请求（必须逐条回包，否则 codex 卡死整轮）：item/commandExecution/requestApproval、
//!   item/fileChange/requestApproval，legacy execCommandApproval / applyPatchApproval
//!
//! 与 smartcode 的同构点：stdout 流量聚合为 claude 形状的 raw（前端零改动）；
//! claude_session_id 槽位存 rollout 路径（session_ref 自报，restore/history 同句柄）；
//! 审批沿用平台策略（默认放行，仅保护名单转人工，复用 claude::approval_required）。
//! 关键差异：thread 握手靠 thread/started 通知；模型热切换经 turn/start 的逐轮
//! model 覆盖实现（codex 无即时 set 模型指令）。

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

/// 等待 `thread/started` 通知的超时。codex 需要先完成 initialize + thread/start
/// 握手才创建 thread；超时意味着启动失败（认证缺失 / 安装异常 / 参数错误）。
const THREAD_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// 普通 JSON-RPC 请求的回包超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct CodexAdapter {
    event_bus: std::sync::Arc<crate::events::EventBus>,
    config_manager: std::sync::Arc<crate::config::ConfigManager>,
    sessions: parking_lot::Mutex<HashMap<SessionId, CodexSessionState>>,
}

struct CodexSessionState {
    stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
    /// 只被消费一次：spawn_event_task 通过 output_stream 取走
    rx: parking_lot::Mutex<Option<mpsc::Receiver<AgentEvent>>>,
    /// 事件通道发送端：非 reader 路径（send_message/set_config）投递状态事件要
    /// 同时走通道（session 任务：内存+落库）和 bus（WS 前端），与 reader 双投递一致
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    /// 进程句柄归 wait 任务所有；stop 通过 notify 请求硬杀
    kill_notify: std::sync::Arc<tokio::sync::Notify>,
    /// wait 任务在进程终止后发信号；kill_existing（resume 前置）用它同步等待
    dead_rx: TokioMutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// 进程是否仍在运行：wait 任务结束时置 false（has_process 依据）
    alive: std::sync::Arc<AtomicBool>,
    /// 平台主动杀进程前置位（stop/kill_existing），wait 任务据此抑制崩溃上报
    suppress_crash: std::sync::Arc<parking_lot::Mutex<bool>>,
    /// JSON-RPC：自增请求 id；待回包请求（id → oneshot）
    next_id: std::sync::Arc<AtomicU64>,
    pending: std::sync::Arc<parking_lot::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>>,
    /// 停靠中的人工审批（同一时刻最多一个，与 smartcode 一致）
    pending_approval: std::sync::Arc<parking_lot::Mutex<Option<PendingCodexApproval>>>,
    /// 停靠中的用户提问（item/tool/requestUserInput，同一时刻最多一个）
    pending_question: std::sync::Arc<parking_lot::Mutex<Option<PendingCodexQuestion>>>,
    /// thread/started 通知送达的 thread 标识与 rollout 路径
    thread_id: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    session_path: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    /// 进行中的 turn（turn/started 设置、turn/completed 清除），interrupt 依据
    current_turn_id: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
    /// set_config 暂存的模型：codex 无即时切换指令，下一次 turn/start 携带
    /// （"Override the model for this turn and subsequent turns"，用后即清）
    pending_model: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
}

/// 停靠中的人工审批：jsonrpc_id 原样回显给 codex；kind 决定回包 decision 词汇。
#[derive(Debug, Clone)]
struct PendingCodexApproval {
    jsonrpc_id: serde_json::Value,
    kind: ApprovalKind,
    command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalKind {
    /// item/commandExecution/requestApproval（v2 词汇 accept/decline）
    Command,
    /// legacy execCommandApproval（ReviewDecision 词汇 approved/deny）
    LegacyExec,
}

/// 停靠中的用户提问：jsonrpc_id 原样回显给 codex；questions 保留
/// (codex 问题 id, 问题文本) 对——前端按问题文本作答，回包要翻译回 id。
#[derive(Debug, Clone)]
struct PendingCodexQuestion {
    jsonrpc_id: serde_json::Value,
    request_id: String,
    questions: Vec<(String, String)>,
}

impl CodexAdapter {
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

    /// 构建 `codex app-server` 命令。审批策略与沙箱不在命令行而在 thread/start
    /// 参数里下发（见 approval_policy / sandbox_mode）；extra_args 透传。
    fn build_command(config: &AgentConfig, ctx: &RuntimeContext) -> (String, Vec<String>) {
        let binary = if ctx.binary_path.is_empty() {
            "codex".to_string()
        } else {
            ctx.binary_path.clone()
        };
        let mut args = vec![
            "app-server".to_string(),
            // request_user_input 工具默认只在 Plan mode 可用；此特性旗标在
            // Default mode 解锁（进程级，不动用户 config.toml）
            "--enable".to_string(),
            "default_mode_request_user_input".to_string(),
        ];
        args.extend(config.extra_args.iter().cloned());
        (binary, args)
    }

    /// OPENAI_API_KEY 仅在实例配置了 api_key 时注入；否则依赖宿主机既有登录
    /// （~/.codex/auth.json / ChatGPT 账号）。
    fn build_env(config: &AgentConfig, ctx: &RuntimeContext) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(key) = config.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            env.push(("OPENAI_API_KEY".to_string(), key.to_string()));
        }
        env.extend(ctx.gitea_env.iter().cloned());
        env
    }

    /// 审批策略恒为 untrusted——与 claude 的控制协议对齐：codex 对每个非受信
    /// 命令都发审批请求，适配器前置判断（非保护命令自动 accept，保护名单停靠
    /// 转人工）。不能用 never/on-request：前者 codex 从不发审批请求，后者由模型
    /// 自行决定是否询问，实测只读命令（kubectl get pods）不会触发审批，保护
    /// 名单就失效了。auto_approve 只影响沙箱（见 sandbox_mode）。
    fn approval_policy() -> &'static str {
        "untrusted"
    }

    fn sandbox_mode(auto_approve: bool) -> &'static str {
        if auto_approve {
            "danger-full-access"
        } else {
            "workspace-write"
        }
    }

    /// 命令是否需要人工审批：复用 claude 的保护名单判定（bash_requires_approval
    /// 链式拆分 + 通配匹配）。codex 的 commandExecution 常带 shell 包装
    /// （/bin/zsh -lc 'kubectl …'），匹配前先拆壳对内层命令再判一次。
    /// None/空命令视为无需审批（文件改动等走自动放行）。
    fn codex_approval_required(command: Option<&str>, protected: &[String]) -> bool {
        let Some(cmd) = command.map(str::trim).filter(|c| !c.is_empty()) else {
            return false;
        };
        if super::claude::approval_required(
            "Bash",
            &serde_json::json!({ "command": cmd }),
            protected,
        ) {
            return true;
        }
        Self::unwrap_shell_wrapper(cmd)
            .map(|inner| {
                super::claude::approval_required(
                    "Bash",
                    &serde_json::json!({ "command": inner }),
                    protected,
                )
            })
            .unwrap_or(false)
    }

    /// 拆掉 `sh/zsh/bash [-l] -c 'payload'` 形式的 shell 包装，返回内层命令。
    /// 只拆一层（codex 的实际行为就是单层包装）；非包装形式返回 None。
    fn unwrap_shell_wrapper(command: &str) -> Option<String> {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        if tokens.len() < 3 {
            return None;
        }
        let program = tokens[0].rsplit('/').next().unwrap_or(tokens[0]);
        if !matches!(program, "sh" | "bash" | "zsh" | "dash" | "ksh") {
            return None;
        }
        // 找 -c / -lc / -ic 这类「以 c 结尾的短旗标」，其后的所有内容即内层命令
        let flag_idx = tokens
            .iter()
            .skip(1)
            .position(|t| t.len() <= 3 && t.starts_with('-') && t.ends_with('c'))?;
        let rest = tokens[flag_idx + 2..].join(" ");
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        let inner = rest
            .strip_prefix('\'')
            .and_then(|r| r.strip_suffix('\''))
            .or_else(|| rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
            .unwrap_or(rest);
        Some(inner.to_string())
    }

    /// 审批回包 decision 词汇按请求代次区分。
    fn build_approval_reply(kind: ApprovalKind, approve: bool) -> serde_json::Value {
        match kind {
            ApprovalKind::Command => {
                serde_json::json!({ "decision": if approve { "accept" } else { "decline" } })
            }
            ApprovalKind::LegacyExec => {
                serde_json::json!({ "decision": if approve { "approved" } else { "deny" } })
            }
        }
    }

    /// 从 rollout 路径文件名推导 thread id。
    /// 文件名格式：rollout-<19 字符时间戳>-<36 字符 thread uuid>.jsonl
    fn derive_thread_id_from_path(path: &str) -> Option<String> {
        let file_name = path.rsplit('/').next()?;
        let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
        if stem.len() < 56 || stem.as_bytes()[19] != b'-' {
            return None;
        }
        Some(stem[20..].to_string())
    }

    /// 解析 config.toml 顶层 `model` 键。
    fn codex_config_model_from_str(content: &str) -> Option<String> {
        let cfg: toml::Value = toml::from_str(content).ok()?;
        cfg.get("model")?
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
    }

    /// ~/.codex/config.toml 的默认模型（GPT 设置同步会维护它）。resume 不带
    /// model 时 codex 沿用 rollout 里存的旧模型——key 轮换后旧模型可能已
    /// 无权限（403），record.model 缺省时用它兜底。
    fn codex_config_default_model() -> Option<String> {
        let path = dirs::home_dir()?.join(".codex").join("config.toml");
        let content = std::fs::read_to_string(path).ok()?;
        Self::codex_config_model_from_str(&content)
    }

    /// turn/start 请求参数。model 仅在 Some 时携带（逐轮覆盖）。
    fn build_turn_start_params(
        thread_id: &str,
        text: &str,
        model: Option<&str>,
    ) -> serde_json::Value {
        let mut params = serde_json::json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": text }],
        });
        if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
            params["model"] = serde_json::Value::String(m.to_string());
        }
        params
    }

    /// Spawn codex app-server 进程，返回 (process, stdout, stdin, pid, stderr tail)。
    #[allow(clippy::type_complexity)]
    async fn spawn_codex_process(
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
                .map_err(|e| anyhow::anyhow!("Failed to spawn codex process: {}. Is 'codex' installed?", e))?
        };

        let pid = process.id();
        let stdout = process.take_stdout().ok_or_else(|| anyhow::anyhow!("stdout not available"))?;
        let stdin = process.take_stdin().ok_or_else(|| anyhow::anyhow!("stdin not available"))?;

        // stderr 只留尾部若干行：启动失败 / 认证失败时把原因带进崩溃上报
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
                        "codex stderr {}",
                        suffix
                    );
                }
            });
        }

        Ok((process, stdout, stdin, pid, stderr_tail))
    }

    /// 写一行 NDJSON 到 stdin。
    async fn write_stdin(
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        payload: serde_json::Value,
    ) -> Result<()> {
        let mut guard = stdin.lock().await;
        guard.write_all(format!("{}\n", payload).as_bytes()).await?;
        guard.flush().await?;
        Ok(())
    }

    /// 发送 JSON-RPC 请求并等待回包。回包带 error 时转错误；超时/通道关闭时
    /// 清理 pending 防泄漏。
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
            return Err(anyhow::anyhow!("codex {} 写入失败: {}", method, e));
        }
        let response = match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                pending.lock().remove(&id);
                anyhow::bail!("codex {} 回包通道关闭——进程可能已退出", method);
            }
            Err(_) => {
                pending.lock().remove(&id);
                anyhow::bail!("等待 codex {} 回包超时（{}s）", method, timeout.as_secs());
            }
        };
        if let Some(err) = response.get("error") {
            anyhow::bail!("codex {} 返回错误: {}", method, err);
        }
        Ok(response.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// stdout 读取任务：JSON-RPC 帧分发 + 事件聚合。EOF 后发 SessionComplete
    /// （进程真正退出由 wait 任务负责状态）。
    #[allow(clippy::too_many_arguments)]
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
        pending_approval: std::sync::Arc<parking_lot::Mutex<Option<PendingCodexApproval>>>,
        pending_question: std::sync::Arc<parking_lot::Mutex<Option<PendingCodexQuestion>>>,
        thread_id: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
        session_path: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
        current_turn_id: std::sync::Arc<parking_lot::Mutex<Option<String>>>,
        ready_tx: tokio::sync::oneshot::Sender<Result<(String, String), String>>,
        protected_commands: Vec<String>,
        log_suffix: &str,
    ) {
        let session_for_reader = session_id.clone();
        let suffix = log_suffix.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut aggregator = CodexAggregator::new();
            let mut ready_tx = Some(ready_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                if cancel.is_cancelled() {
                    break;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    tracing::debug!(
                        line_preview = line.chars().take(120).collect::<String>(),
                        "codex stdout 非 JSON 行 {}",
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
                            &pending_question,
                            &tx,
                            &event_bus,
                            &session_for_reader,
                            &protected_commands,
                        )
                        .await;
                    } else {
                        // 通知：thread 握手 / turn 追踪 / 事件聚合
                        match method.as_str() {
                            "thread/started" => {
                                let thread = v.pointer("/params/thread");
                                let tid = thread
                                    .and_then(|t| t.get("id"))
                                    .and_then(|i| i.as_str())
                                    .map(str::to_string);
                                let path = thread
                                    .and_then(|t| t.get("path"))
                                    .and_then(|p| p.as_str())
                                    .map(str::to_string);
                                if let Some(tid) = tid {
                                    *thread_id.lock() = Some(tid);
                                }
                                if let Some(path) = path {
                                    *session_path.lock() = Some(path);
                                }
                                let snapshot = (
                                    thread_id.lock().clone().unwrap_or_default(),
                                    session_path.lock().clone().unwrap_or_default(),
                                );
                                if let Some(rt) = ready_tx.take() {
                                    let _ = rt.send(Ok(snapshot));
                                }
                            }
                            "turn/started" => {
                                if let Some(turn_id) =
                                    v.pointer("/params/turn/id").and_then(|i| i.as_str())
                                {
                                    *current_turn_id.lock() = Some(turn_id.to_string());
                                }
                            }
                            "turn/completed" => {
                                current_turn_id.lock().take();
                                // turn 结束仍未作答的提问作废：回空 answers 释放 codex
                                // 等待，前端卡片随状态刷新消失
                                let stale_question = pending_question.lock().take();
                                if let Some(q) = stale_question {
                                    tracing::info!(session = %session_for_reader, "codex question dropped: turn completed without answer");
                                    let _ = Self::write_stdin(&stdin, serde_json::json!({
                                        "jsonrpc": "2.0", "id": q.jsonrpc_id,
                                        "result": { "answers": {} }
                                    })).await;
                                }
                                for event in aggregator.feed(&method, v.get("params").unwrap_or(&serde_json::Value::Null), &session_for_reader) {
                                    let _ = tx.send(event.clone()).await;
                                    event_bus.emit(event);
                                }
                            }
                            _ => {
                                for event in aggregator.feed(&method, v.get("params").unwrap_or(&serde_json::Value::Null), &session_for_reader) {
                                    let _ = tx.send(event.clone()).await;
                                    event_bus.emit(event);
                                }
                            }
                        }
                    }
                } else if let Some(id) = id {
                    // 我们请求的回包
                    if let Some(n) = id.as_u64() {
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

    /// 解析 requestUserInput 的 questions 数组 → (前端卡片条目, codex 问题
    /// id ↔ 问题文本 对)。缺 question 文本的条目丢弃。
    fn parse_user_input_questions(
        raw: Option<&serde_json::Value>,
    ) -> (
        Vec<crate::agents::types::UserQuestionItem>,
        Vec<(String, String)>,
    ) {
        let mut items = Vec::new();
        let mut pairs = Vec::new();
        for q in raw.and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(text) = q.get("question").and_then(|t| t.as_str()) else { continue };
            let qid = q.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
            let options = q
                .get("options")
                .and_then(|o| o.as_array())
                .map(|list| {
                    list.iter()
                        .map(|o| crate::agents::types::UserQuestionOption {
                            label: o.get("label").and_then(|l| l.as_str()).unwrap_or("").to_string(),
                            description: o
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            pairs.push((qid, text.to_string()));
            items.push(crate::agents::types::UserQuestionItem {
                question: text.to_string(),
                header: q.get("header").and_then(|h| h.as_str()).unwrap_or("Question").to_string(),
                options,
            });
        }
        (items, pairs)
    }

    /// server → client 请求处理。每条请求都必须回包（codex 在等回包期间整轮挂起）。
    /// 审批策略：命令命中保护名单 → 停靠并发 ApprovalRequest 给前端；其余命令与
    /// 文件改动 → 直接放行（平台 bypass 基线，见 claude::approval_required）。
    #[allow(clippy::too_many_arguments)]
    async fn handle_server_request(
        v: &serde_json::Value,
        id: serde_json::Value,
        stdin: &std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        pending_approval: &std::sync::Arc<parking_lot::Mutex<Option<PendingCodexApproval>>>,
        pending_question: &std::sync::Arc<parking_lot::Mutex<Option<PendingCodexQuestion>>>,
        tx: &mpsc::Sender<AgentEvent>,
        event_bus: &std::sync::Arc<crate::events::EventBus>,
        session_id: &SessionId,
        protected: &[String],
    ) {
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
        fn reply(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
        }

        match method {
            "item/commandExecution/requestApproval" => {
                let command = params.get("command").and_then(|c| c.as_str()).unwrap_or("");
                let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
                if !Self::codex_approval_required(Some(command), protected) {
                    tracing::info!(command = %command, "codex command auto-allowed (not in protected commands)");
                    let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "accept" }))).await;
                    return;
                }
                let request_id = jsonrpc_id_string(&id);
                let entry = PendingCodexApproval {
                    jsonrpc_id: id.clone(),
                    kind: ApprovalKind::Command,
                    command: command.to_string(),
                };
                if pending_approval.lock().replace(entry).is_some() {
                    // 已有人工审批挂起：新请求直接拒绝，避免覆盖导致旧回包丢失
                    tracing::warn!(command = %command, "codex approval already pending; auto-declining new request");
                    let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "decline" }))).await;
                    return;
                }
                let prompt = if cwd.is_empty() {
                    format!("codex 请求执行命令：\n{}", command)
                } else {
                    format!("codex 请求执行命令：\n{}\ncwd: {}", command, cwd)
                };
                let event = AgentEvent::ApprovalRequest {
                    session_id: session_id.clone(),
                    prompt,
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
            "execCommandApproval" => {
                let command = params
                    .get("command")
                    .and_then(|c| c.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let cwd = params.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
                if !Self::codex_approval_required(Some(&command), protected) {
                    tracing::info!(command = %command, "codex legacy exec auto-allowed");
                    let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "approved" }))).await;
                    return;
                }
                let request_id = jsonrpc_id_string(&id);
                let entry = PendingCodexApproval {
                    jsonrpc_id: id.clone(),
                    kind: ApprovalKind::LegacyExec,
                    command: command.clone(),
                };
                if pending_approval.lock().replace(entry).is_some() {
                    tracing::warn!(command = %command, "codex approval already pending; auto-denying new request");
                    let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "deny" }))).await;
                    return;
                }
                let prompt = if cwd.is_empty() {
                    format!("codex 请求执行命令：\n{}", command)
                } else {
                    format!("codex 请求执行命令：\n{}\ncwd: {}", command, cwd)
                };
                let event = AgentEvent::ApprovalRequest {
                    session_id: session_id.clone(),
                    prompt,
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
            // 文件改动不进保护名单（平台策略：仅命令审批转人工）
            "item/fileChange/requestApproval" => {
                let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "accept" }))).await;
            }
            "applyPatchApproval" => {
                let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "approved" }))).await;
            }
            // 模型向用户提问（request_user_input 工具）：停靠 + 发 UserQuestion 卡片
            "item/tool/requestUserInput" => {
                let (items, pairs) =
                    Self::parse_user_input_questions(params.get("questions"));
                if items.is_empty() {
                    tracing::warn!("codex requestUserInput carried no parsable questions; replying empty answers");
                    let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "answers": {} }))).await;
                    return;
                }
                let request_id = params
                    .get("itemId")
                    .and_then(|i| i.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| jsonrpc_id_string(&id));
                let entry = PendingCodexQuestion {
                    jsonrpc_id: id.clone(),
                    request_id: request_id.clone(),
                    questions: pairs,
                };
                if pending_question.lock().replace(entry).is_some() {
                    tracing::warn!("codex question already pending; auto-cancelling new request");
                    let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "answers": {} }))).await;
                    return;
                }
                let event = AgentEvent::UserQuestion {
                    session_id: session_id.clone(),
                    tool_use_id: request_id.clone(),
                    request_id,
                    questions: items,
                    ts: AgentEvent::now(),
                };
                let _ = tx.send(event.clone()).await;
                event_bus.emit(event);
            }
            // elicitation / permissions 等：v1 不支持，回 cancel 让 turn 继续
            _ => {
                tracing::warn!(method = %method, "codex server request not supported; replying cancel");
                let _ = Self::write_stdin(stdin, reply(&id, serde_json::json!({ "decision": "cancel" }))).await;
            }
        }
    }

    /// 进程退出监视任务：进程句柄独占于此任务，stop 经 kill_notify 请求硬杀。
    /// 崩溃上报对齐 claude：非 0 / Err 退出 → Error 状态 + stderr tail（保密改写），
    /// 平台主动杀（suppress 置位）不算崩溃。
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
                    tracing::info!(?status, "codex process exited");
                    let code = status.as_ref().ok().copied();
                    let crashed = code != Some(0);
                    if code.is_none() {
                        // wait 本身失败（如通道错误）：状态翻 Error，与 smartcode 一致
                        event_bus.emit(AgentEvent::SessionStatus {
                            session_id: session_id.clone(),
                            status: SessionStatus::Error,
                            ts: AgentEvent::now(),
                        });
                    }
                    if crashed {
                        if *suppress_crash.lock() {
                            tracing::info!(session = %session_id, code = ?code, "codex exit suppressed: platform-initiated stop/resume");
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
                    // notify_one 在无等待者时会存 permit，此分支晚到也能命中
                    if let Err(e) = p.kill().await {
                        tracing::warn!(session = %session_id, error = %e, "codex kill failed");
                    }
                    let _ = p.wait().await;
                    tracing::info!(session = %session_id, "codex process killed by stop");
                }
            }
            alive.store(false, Ordering::Release);
            let _ = dead_tx.send(());
        });
    }

    /// 杀掉现有进程并移除会话状态（resume 前置步骤）。返回 rollout 路径
    /// （restore/resume 仍用同一路径）。
    async fn kill_existing(&self, session_id: &SessionId) -> Option<String> {
        let existing = self.sessions.lock().remove(session_id);
        let state = existing?;
        state.cancel.cancel();
        *state.suppress_crash.lock() = true;
        let path = state.session_path.lock().clone();
        let dead_rx = state.dead_rx.lock().await.take();
        state.kill_notify.notify_one();
        if let Some(rx) = dead_rx {
            // wait 任务任一分支完成都意味着进程已不在运行
            let _ = rx.await;
        }
        path
    }

    /// thread/start（新会话）或 thread/resume（恢复）后等待 thread/started 握手。
    async fn wait_thread_ready(
        ready_rx: tokio::sync::oneshot::Receiver<Result<(String, String), String>>,
    ) -> Result<(String, String)> {
        match tokio::time::timeout(THREAD_READY_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(pair))) => Ok(pair),
            Ok(Ok(Err(e))) => anyhow::bail!("codex thread 启动失败: {}", e),
            Ok(Err(_)) => anyhow::bail!(
                "codex thread 通道关闭——进程可能启动即退出（请检查 codex 安装、登录状态与参数）"
            ),
            Err(_) => anyhow::bail!(
                "等待 codex thread 超时（{}s）——进程可能未正常启动",
                THREAD_READY_TIMEOUT.as_secs()
            ),
        }
    }

    /// 拉起进程并完成 initialize + thread/started 握手（start/restore 共用前半段）。
    /// 返回 (thread_id, rollout_path, 状态组装件)。
    #[allow(clippy::type_complexity)]
    async fn establish_thread(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: &SessionId,
        working_dir: Option<&str>,
        log_suffix: &str,
    ) -> Result<(
        String,
        Option<String>,
        std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        std::sync::Arc<parking_lot::Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>>,
        std::sync::Arc<AtomicU64>,
        (
            tokio::sync::oneshot::Receiver<Result<(String, String), String>>,
            std::sync::Arc<parking_lot::Mutex<Option<String>>>,
            std::sync::Arc<parking_lot::Mutex<Option<String>>>,
            std::sync::Arc<parking_lot::Mutex<Option<String>>>,
            std::sync::Arc<parking_lot::Mutex<Option<PendingCodexApproval>>>,
            std::sync::Arc<parking_lot::Mutex<Option<PendingCodexQuestion>>>,
            mpsc::Receiver<AgentEvent>,
            mpsc::Sender<AgentEvent>,
            CancellationToken,
            std::sync::Arc<tokio::sync::Notify>,
            tokio::sync::oneshot::Receiver<()>,
            std::sync::Arc<AtomicBool>,
            std::sync::Arc<parking_lot::Mutex<bool>>,
            u32,
        ),
    )> {
        let (process, stdout, stdin, pid, stderr_tail) =
            self.spawn_codex_process(config, ctx, working_dir, log_suffix).await?;
        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending = std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let next_id = std::sync::Arc::new(AtomicU64::new(1));
        let pending_approval = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let pending_question = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let thread_id = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let session_path = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let current_turn_id = std::sync::Arc::new(parking_lot::Mutex::new(None));

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(String, String), String>>();

        let protected = self.config_manager.config().permissions.protected_commands.clone();
        Self::spawn_reader_task(
            stdout,
            session_id.clone(),
            tx.clone(),
            self.event_bus.clone(),
            cancel.clone(),
            stdin_arc.clone(),
            pending.clone(),
            pending_approval.clone(),
            pending_question.clone(),
            thread_id.clone(),
            session_path.clone(),
            current_turn_id.clone(),
            ready_tx,
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
            serde_json::json!({
                "clientInfo": {
                    "name": "milletcode-agent-console",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                // thread/resume 的 path 参数要求此 capability（schema 默认 false）
                "capabilities": { "experimentalApi": true },
            }),
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| anyhow::anyhow!("codex initialize 失败: {}", e))?;

        Ok((
            String::new(),
            None,
            stdin_arc,
            pending,
            next_id,
            (
                ready_rx,
                thread_id,
                session_path,
                current_turn_id,
                pending_approval,
                pending_question,
                rx,
                tx,
                cancel,
                kill_notify,
                dead_rx,
                alive,
                suppress_crash,
                pid,
            ),
        ))
    }

    /// 完成 thread/resume 恢复（trait restore / resume 共用）。
    async fn restore_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        rollout_path: &str,
        working_dir: Option<String>,
        model: Option<&str>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, path = %rollout_path, "Restoring codex session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let thread_id0 = Self::derive_thread_id_from_path(rollout_path).ok_or_else(|| {
            anyhow::anyhow!(
                "无法从会话路径推导 codex thread id（期望 rollout-<时间戳>-<uuid>.jsonl）: {}",
                rollout_path
            )
        })?;

        let (_, _, stdin_arc, pending, next_id, bundle) = self
            .establish_thread(config, ctx, &session_id, working_dir.as_deref(), "(restored)")
            .await?;
        let (
            ready_rx,
            thread_id,
            session_path,
            current_turn_id,
            pending_approval,
            pending_question,
            rx,
            event_tx,
            cancel,
            kill_notify,
            dead_rx,
            alive,
            suppress_crash,
            pid,
        ) = bundle;

        let mut resume_params = serde_json::json!({
            "threadId": thread_id0,
            "path": rollout_path,
            "approvalPolicy": Self::approval_policy(),
            "sandbox": Self::sandbox_mode(config.auto_approve),
        });
        if let Some(dir) = working_dir.as_deref().filter(|d| !d.is_empty()) {
            resume_params["cwd"] = serde_json::Value::String(dir.to_string());
        }
        if let Some(m) = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .or_else(Self::codex_config_default_model)
        {
            resume_params["model"] = serde_json::Value::String(m);
        }
        let resp = Self::request(&stdin_arc, &pending, &next_id, "thread/resume", resume_params, REQUEST_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("codex thread/resume 失败: {}", e))?;

        // resume 的 thread 信息同步在响应 result.thread 里（thread/started
        // 通知只有 thread/start 流程才发，等通知只会 30s 超时）。
        let (tid, spath) = match resp.pointer("/thread/id").and_then(|i| i.as_str()).filter(|t| !t.is_empty()) {
            Some(tid) => (
                tid.to_string(),
                resp.pointer("/thread/path")
                    .and_then(|p| p.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
            None => Self::wait_thread_ready(ready_rx).await?,
        };
        if tid.is_empty() {
            anyhow::bail!("codex thread/started 未携带 thread id");
        }
        *thread_id.lock() = Some(tid);
        if !spath.is_empty() {
            *session_path.lock() = Some(spath.clone());
        }
        tracing::info!(session = %session_id, path = %spath, "codex session restored");

        self.sessions.lock().insert(
            session_id.clone(),
            CodexSessionState {
                stdin: stdin_arc,
                rx: parking_lot::Mutex::new(Some(rx)),
                event_tx,
                cancel,
                kill_notify,
                dead_rx: TokioMutex::new(Some(dead_rx)),
                alive,
                suppress_crash,
                next_id,
                pending,
                pending_approval,
                pending_question,
                thread_id,
                session_path,
                current_turn_id,
                pending_model: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            },
        );

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(SessionHandle::new(
            session_id,
            "codex".to_string(),
            SessionHandleState::Subprocess { pid },
        ))
    }
}

/// JSON-RPC 请求 id → 平台审批 request_id 字符串（数字与字符串 id 统一为字符串）。
fn jsonrpc_id_string(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// codex 通知 → 平台 AgentEvent 的聚合器。
///
/// codex 以 delta 流输出 agentMessage/reasoning（每条几个字），item/started/completed
/// 承载工具卡片。聚合到 claude 的"一条 assistant 消息一个 Output"粒度；消息边界是
/// itemId 切换与 item/completed，块顺序 thinking→text→tool_use（Anthropic 块序）。
struct CodexAggregator {
    thinking: String,
    thinking_item: Option<String>,
    text: String,
    text_item: Option<String>,
    /// 实时 Output 的合成 uuid 序号（前端 Svelte each key 需唯一）
    seq: u64,
}

impl CodexAggregator {
    fn new() -> Self {
        Self {
            thinking: String::new(),
            thinking_item: None,
            text: String::new(),
            text_item: None,
            seq: 0,
        }
    }

    /// 构造与 claude JSONL 行同构的 raw：`{type:"assistant", uuid, message:{content:[block]}}`。
    fn assistant_raw(&mut self, block: serde_json::Value) -> serde_json::Value {
        self.seq += 1;
        serde_json::json!({
            "type": "assistant",
            "uuid": format!("codex-{}", self.seq),
            "message": { "content": [block] },
        })
    }

    /// 进一条通知（method + params），出零或多个 AgentEvent。
    /// 未知 method / item 变体（plan、webSearch、hookPrompt 等且枚举还在增长）
    /// → 空 Vec（吞掉，避免泄漏原始帧给用户）。
    fn feed(&mut self, method: &str, params: &serde_json::Value, session_id: &SessionId) -> Vec<AgentEvent> {
        match method {
            "item/agentMessage/delta" => {
                let delta = params.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return vec![];
                }
                let item_id = params.get("itemId").and_then(|i| i.as_str()).unwrap_or("");
                // thinking 段必然在 text 之前结束（Anthropic 块序），先落盘
                let mut events = self.flush_thinking(session_id);
                if !item_id.is_empty() && self.text_item.as_deref() != Some(item_id) {
                    events.extend(self.flush_text(session_id));
                    self.text_item = Some(item_id.to_string());
                }
                self.text.push_str(delta);
                events
            }
            "item/reasoning/summaryTextDelta" => {
                let delta = params.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return vec![];
                }
                let item_id = params.get("itemId").and_then(|i| i.as_str()).unwrap_or("");
                // 新一轮推理意味着上一段文本已完结（块序 thinking→text→下一轮 thinking）
                let mut events = self.flush_text(session_id);
                if !item_id.is_empty() && self.thinking_item.as_deref() != Some(item_id) {
                    events.extend(self.flush_thinking(session_id));
                    self.thinking_item = Some(item_id.to_string());
                }
                self.thinking.push_str(delta);
                events
            }
            "item/started" => {
                let Some(item) = params.get("item") else { return vec![] };
                let itype = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let item_id = item.get("id").and_then(|i| i.as_str()).unwrap_or("");
                if item_id.is_empty() {
                    return vec![];
                }
                let (name, input) = match itype {
                    "commandExecution" => (
                        "Bash",
                        serde_json::json!({
                            "command": item.get("command").and_then(|c| c.as_str()).unwrap_or(""),
                            "cwd": item.get("cwd"),
                        }),
                    ),
                    "fileChange" => (
                        "apply_patch",
                        serde_json::json!({ "changes": item.get("changes").cloned().unwrap_or(serde_json::Value::Null) }),
                    ),
                    "mcpToolCall" => (
                        "MCP",
                        serde_json::json!({
                            "server": item.get("server").and_then(|s| s.as_str()).unwrap_or(""),
                            "tool": item.get("tool").and_then(|t| t.as_str()).unwrap_or(""),
                            "arguments": item.get("arguments").cloned().unwrap_or(serde_json::Value::Null),
                        }),
                    ),
                    _ => return vec![],
                };
                let mut events = self.flush_all(session_id);
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                let raw = self.assistant_raw(serde_json::json!({
                    "type": "tool_use",
                    "id": item_id,
                    "name": name,
                    "input": input,
                }));
                events.push(AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: format!("[tool_use]{} {}[/tool_use]", name, input_str),
                    ts: AgentEvent::now(),
                    raw: Some(raw),
                });
                events
            }
            "item/completed" => {
                let Some(item) = params.get("item") else { return vec![] };
                let itype = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let item_id = item.get("id").and_then(|i| i.as_str()).unwrap_or("");
                match itype {
                    "agentMessage" => {
                        let mut events = self.flush_thinking(session_id);
                        if !self.text.is_empty() {
                            events.extend(self.flush_text(session_id));
                        } else {
                            // 无 delta 路径（错过流式 / 非流式）：用 item 文本兜底
                            let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if !text.is_empty() {
                                self.text.push_str(text);
                                events.extend(self.flush_text(session_id));
                            }
                        }
                        events
                    }
                    "reasoning" => {
                        let mut events = vec![];
                        if !self.thinking.is_empty() {
                            events.extend(self.flush_thinking(session_id));
                        } else {
                            let summary = reasoning_summary_text(item);
                            if !summary.is_empty() {
                                self.thinking.push_str(&summary);
                                events.extend(self.flush_thinking(session_id));
                            }
                        }
                        events
                    }
                    "commandExecution" => {
                        let aggregated = item
                            .get("aggregatedOutput")
                            .and_then(|o| o.as_str())
                            .unwrap_or("")
                            .to_string();
                        let exit_code = item.get("exitCode").and_then(|c| c.as_i64());
                        let is_error = item.get("status").and_then(|s| s.as_str()) == Some("failed")
                            || exit_code.map(|c| c != 0).unwrap_or(false);
                        let content: String = aggregated.chars().take(4_000).collect();
                        if item_id.is_empty() {
                            return vec![];
                        }
                        vec![self.tool_result_event(item_id, &content, is_error, session_id)]
                    }
                    "fileChange" => {
                        let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                        let changes = item.get("changes").cloned().unwrap_or(serde_json::Value::Null);
                        let content: String = serde_json::to_string(&changes)
                            .unwrap_or_default()
                            .chars()
                            .take(2_000)
                            .collect();
                        if item_id.is_empty() {
                            return vec![];
                        }
                        vec![self.tool_result_event(item_id, &content, status == "failed", session_id)]
                    }
                    "mcpToolCall" => {
                        if item_id.is_empty() {
                            return vec![];
                        }
                        let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                        vec![self.tool_result_event(item_id, "", status == "failed", session_id)]
                    }
                    _ => vec![],
                }
            }
            "turn/completed" => {
                let mut events = self.flush_all(session_id);
                let mut raw = params.get("turn").cloned().unwrap_or(serde_json::json!({}));
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert(
                        "type".to_string(),
                        serde_json::Value::String("result".to_string()),
                    );
                }
                events.push(AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: String::new(),
                    ts: AgentEvent::now(),
                    raw: Some(raw),
                });
                let status = params.pointer("/turn/status").and_then(|s| s.as_str()).unwrap_or("");
                match status {
                    "failed" => {
                        let message = params
                            .pointer("/turn/error/message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("turn failed");
                        events.push(AgentEvent::Error {
                            session_id: session_id.clone(),
                            message: message.to_string(),
                            ts: AgentEvent::now(),
                        });
                    }
                    // completed / interrupted（进程仍驻留，翻回 Complete 让前端
                    // 回到可输入状态；下一条消息由 send_message 翻回 Running）
                    _ => {
                        events.push(AgentEvent::SessionStatus {
                            session_id: session_id.clone(),
                            status: SessionStatus::Complete,
                            ts: AgentEvent::now(),
                        });
                    }
                }
                events
            }
            "error" => {
                let message = params
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| params.to_string());
                vec![AgentEvent::Error {
                    session_id: session_id.clone(),
                    message,
                    ts: AgentEvent::now(),
                }]
            }
            _ => vec![],
        }
    }

    fn tool_result_event(
        &self,
        item_id: &str,
        content: &str,
        is_error: bool,
        session_id: &SessionId,
    ) -> AgentEvent {
        let raw = serde_json::json!({
            "type": "user",
            "uuid": format!("codex-tr-{}", item_id),
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": item_id,
                "content": content,
                "is_error": is_error,
            }] },
        });
        AgentEvent::Output {
            session_id: session_id.clone(),
            content: format!("[tool_result tool_use_id=\"{}\"]{}[/tool_result]", item_id, content),
            ts: AgentEvent::now(),
            raw: Some(raw),
        }
    }

    fn flush_thinking(&mut self, session_id: &SessionId) -> Vec<AgentEvent> {
        if self.thinking.is_empty() {
            return vec![];
        }
        let content = std::mem::take(&mut self.thinking);
        self.thinking_item = None;
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
        self.text_item = None;
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

/// reasoning item 的 summary 文本（元素兼容字符串与 {type:"summary_text",text}）。
fn reasoning_summary_text(item: &serde_json::Value) -> String {
    let Some(parts) = item.get("summary").and_then(|s| s.as_array()) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|el| {
            if let Some(s) = el.as_str() {
                Some(s.to_string())
            } else {
                el.get("text").and_then(|t| t.as_str()).map(str::to_string)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
    fn adapter_id(&self) -> &str {
        "codex"
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
        tracing::info!(session = %session_id, working_dir = ?working_dir, "Starting codex app-server session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let (_, _, stdin_arc, pending, next_id, bundle) = self
            .establish_thread(config, ctx, &session_id, working_dir.as_deref(), "")
            .await?;
        let (
            ready_rx,
            thread_id,
            session_path,
            current_turn_id,
            pending_approval,
            pending_question,
            rx,
            event_tx,
            cancel,
            kill_notify,
            dead_rx,
            alive,
            suppress_crash,
            pid,
        ) = bundle;

        let start_params = serde_json::json!({
            "approvalPolicy": Self::approval_policy(),
            "sandbox": Self::sandbox_mode(config.auto_approve),
            "cwd": working_dir.as_deref().filter(|d| !d.is_empty()),
        });
        Self::request(&stdin_arc, &pending, &next_id, "thread/start", start_params, REQUEST_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("codex thread/start 失败: {}", e))?;

        let (tid, spath) = Self::wait_thread_ready(ready_rx).await?;
        if tid.is_empty() {
            anyhow::bail!("codex thread/started 未携带 thread id");
        }
        *thread_id.lock() = Some(tid.clone());
        if !spath.is_empty() {
            *session_path.lock() = Some(spath.clone());
            tracing::info!(session = %session_id, path = %spath, "codex rollout file");
        }

        // 初始 prompt 直接以 turn/start 下发（codex 无 console 输入模式）
        let resp = Self::request(
            &stdin_arc,
            &pending,
            &next_id,
            "turn/start",
            Self::build_turn_start_params(&tid, prompt, None),
            REQUEST_TIMEOUT,
        )
        .await?;
        if let Some(turn_id) = resp.pointer("/turn/id").and_then(|i| i.as_str()) {
            *current_turn_id.lock() = Some(turn_id.to_string());
        }

        self.sessions.lock().insert(
            session_id.clone(),
            CodexSessionState {
                stdin: stdin_arc,
                rx: parking_lot::Mutex::new(Some(rx)),
                event_tx,
                cancel,
                kill_notify,
                dead_rx: TokioMutex::new(Some(dead_rx)),
                alive,
                suppress_crash,
                next_id,
                pending,
                pending_approval,
                pending_question,
                thread_id,
                session_path,
                current_turn_id,
                pending_model: std::sync::Arc::new(parking_lot::Mutex::new(None)),
            },
        );

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(SessionHandle::new(
            session_id,
            "codex".to_string(),
            SessionHandleState::Subprocess { pid },
        ))
    }

    async fn send_message(&self, session: &SessionHandle, message: &str) -> Result<()> {
        let (stdin, event_tx, pending, next_id, thread_id, current_turn_id, pending_model) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (
                state.stdin.clone(),
                state.event_tx.clone(),
                state.pending.clone(),
                state.next_id.clone(),
                state.thread_id.clone(),
                state.current_turn_id.clone(),
                state.pending_model.clone(),
            )
        };
        let tid = thread_id
            .lock()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("codex session has no thread id"))?;
        let model = pending_model.lock().take();
        let resp = Self::request(
            &stdin,
            &pending,
            &next_id,
            "turn/start",
            Self::build_turn_start_params(&tid, message, model.as_deref()),
            REQUEST_TIMEOUT,
        )
        .await?;
        if let Some(turn_id) = resp.pointer("/turn/id").and_then(|i| i.as_str()) {
            *current_turn_id.lock() = Some(turn_id.to_string());
        }
        // 轮次开始：翻回 Running（turn/completed 时已置 Complete），否则前端会一直
        // 显示完成态。双投递与 reader 约定一致：通道给 session 任务，bus 给 WS 前端。
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
        // codex 无即时配置指令：模型暂存，下一次 turn/start 携带并覆盖「本轮及后续轮」；
        // 其余字段（base_url/api_key/provider）v1 不支持，记日志忽略。
        if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
            let event_tx = {
                let sessions = self.sessions.lock();
                let state = sessions
                    .get(&session.id)
                    .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
                state.pending_model.lock().replace(m.to_string());
                state.event_tx.clone()
            };
            let event = AgentEvent::ConfigChanged {
                session_id: session.id.clone(),
                model: Some(m.to_string()),
                provider: None,
                base_url: None,
                ts: AgentEvent::now(),
            };
            let _ = event_tx.send(event.clone()).await;
            self.event_bus.emit(event);
        } else {
            tracing::warn!(session = %session.id, "codex set_config: only model is supported; ignoring other fields");
        }
        Ok(())
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
        let payload = Self::build_approval_reply(entry.kind, approve);
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": entry.jsonrpc_id,
            "result": payload,
        });
        Self::write_stdin(&stdin, frame).await?;

        tracing::info!(
            session = %session.id,
            request_id = %current_id,
            command = %entry.command,
            approved = approve,
            "codex approval decision sent"
        );
        Ok(())
    }

    /// 回答 request_user_input 提问：前端按问题文本作答，回包翻译回
    /// codex 的问题 id（answers: {qid: {answers: [文本]}}）。
    async fn answer_question(
        &self,
        session: &SessionHandle,
        request_id: &str,
        answers: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let (stdin, pending_question) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (state.stdin.clone(), state.pending_question.clone())
        };

        let entry = pending_question.lock().take();
        let Some(entry) = entry else {
            anyhow::bail!("No pending question for session {}", session.id);
        };
        if request_id != entry.request_id {
            let current = entry.request_id.clone();
            *pending_question.lock() = Some(entry);
            anyhow::bail!(
                "Stale question answer: request {} no longer pending (current {})",
                request_id,
                current
            );
        }

        let mut reply_answers = serde_json::Map::new();
        for (qid, qtext) in &entry.questions {
            if let Some(ans) = answers.get(qtext).filter(|a| !a.is_empty()) {
                reply_answers.insert(qid.clone(), serde_json::json!({ "answers": [ans] }));
            }
        }
        // 问题文本对不上（前端改写过文案等）：单问题场景把全部答案值给唯一问题
        if reply_answers.is_empty() && entry.questions.len() == 1 {
            if let Some(ans) = answers.values().find(|a| !a.is_empty()) {
                reply_answers.insert(
                    entry.questions[0].0.clone(),
                    serde_json::json!({ "answers": [ans] }),
                );
            }
        }
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": entry.jsonrpc_id,
            "result": { "answers": reply_answers },
        });
        Self::write_stdin(&stdin, frame).await?;

        tracing::info!(
            session = %session.id,
            request_id = request_id,
            answered = reply_answers.len(),
            "codex question answer sent"
        );
        Ok(())
    }

    async fn interrupt(&self, session: &SessionHandle) -> Result<()> {
        let (stdin, pending, next_id, thread_id, current_turn_id) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (
                state.stdin.clone(),
                state.pending.clone(),
                state.next_id.clone(),
                state.thread_id.clone(),
                state.current_turn_id.clone(),
            )
        };
        let Some(tid) = thread_id.lock().clone() else {
            return Ok(());
        };
        let Some(turn_id) = current_turn_id.lock().clone() else {
            tracing::info!(session = %session.id, "codex interrupt: no active turn");
            return Ok(());
        };
        // 回包超时不视为错误：turn 可能恰好完成，interrupt 的回包晚到
        let _ = Self::request(
            &stdin,
            &pending,
            &next_id,
            "turn/interrupt",
            serde_json::json!({ "threadId": tid, "turnId": turn_id }),
            REQUEST_TIMEOUT,
        )
        .await;
        tracing::info!(session = %session.id, "codex interrupt sent");
        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, _force: bool) -> Result<()> {
        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session.id.clone(),
            status: SessionStatus::Stopped,
            ts: AgentEvent::now(),
        });

        let Some(state) = self.sessions.lock().remove(&session.id) else {
            tracing::info!(session = %session.id, "codex stop: no live adapter state (process may have already exited)");
            return Ok(());
        };
        state.cancel.cancel();
        *state.suppress_crash.lock() = true;
        // rollout 由 codex 增量落盘，无需优雅收尾；直接硬杀
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
        // claude_session_id 槽位存的是 codex 的 rollout 路径
        self.restore_session(config, ctx, session_id, claude_session_id, working_dir, model)
            .await
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
        tracing::info!(session = %session_id, path = %claude_session_id, "Resuming codex session with new config");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Restarting,
            ts: AgentEvent::now(),
        });

        let known_path = self.kill_existing(&session_id).await;
        // 优先用本次持有状态里的路径（与 record 一致；record 缺失时兜底）
        let resume_path = known_path
            .or_else(|| (!claude_session_id.is_empty()).then(|| claude_session_id.to_string()))
            .ok_or_else(|| anyhow::anyhow!("no rollout path available for resume"))?;

        let handle = self
            .restore_session(config, ctx, session_id, &resume_path, working_dir, model)
            .await?;
        Ok(handle)
    }

    fn session_ref(&self, session_id: &SessionId) -> Option<String> {
        let sessions = self.sessions.lock();
        let state = sessions.get(session_id)?;
        let path = state.session_path.lock().clone();
        path
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
    use serde_json::json;

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

    fn config(auto_approve: bool) -> AgentConfig {
        AgentConfig {
            name: "Codex".to_string(),
            extra_args: vec![],
            api_key: None,
            auto_approve,
            enabled: true,
            allow_config_overwrite: true,
            path: None,
            remote_path: None,
            release_channel: "beta".to_string(),
        }
    }

    fn sid() -> SessionId {
        SessionId::new("k/p/codex/s".to_string())
    }

    #[test]
    fn build_command_uses_app_server_and_extra_args() {
        let (cmd, args) = CodexAdapter::build_command(&config(false), &ctx());
        assert_eq!(cmd, "codex");
        assert_eq!(
            args,
            vec!["app-server", "--enable", "default_mode_request_user_input"]
        );

        let mut c = config(true);
        c.extra_args = vec!["--profile".to_string(), "work".to_string()];
        let mut c2 = ctx();
        c2.binary_path = "/opt/codex/bin/codex".to_string();
        let (cmd, args) = CodexAdapter::build_command(&c, &c2);
        assert_eq!(cmd, "/opt/codex/bin/codex");
        assert_eq!(
            args,
            vec![
                "app-server",
                "--enable",
                "default_mode_request_user_input",
                "--profile",
                "work"
            ]
        );
    }

    #[test]
    fn build_env_maps_api_key_and_gitea() {
        let mut c = config(true);
        c.api_key = Some("sk-openai".to_string());
        let mut c2 = ctx();
        c2.gitea_env = vec![("GITEA_TOKEN".to_string(), "t0".to_string())];
        assert_eq!(
            CodexAdapter::build_env(&c, &c2),
            vec![
                ("OPENAI_API_KEY".to_string(), "sk-openai".to_string()),
                ("GITEA_TOKEN".to_string(), "t0".to_string()),
            ]
        );
        // 未配置 / 空白 → 不注入（依赖宿主机 auth.json 登录）
        assert_eq!(CodexAdapter::build_env(&config(true), &ctx()), vec![]);
        let mut blank = config(true);
        blank.api_key = Some("  ".to_string());
        assert_eq!(CodexAdapter::build_env(&blank, &ctx()), vec![]);
    }

    #[test]
    fn approval_policy_and_sandbox_follow_auto_approve() {
        assert_eq!(CodexAdapter::approval_policy(), "untrusted");
        assert_eq!(CodexAdapter::sandbox_mode(true), "danger-full-access");
        assert_eq!(CodexAdapter::sandbox_mode(false), "workspace-write");
    }

    #[test]
    fn codex_approval_required_matches_protected_commands() {
        let protected = vec!["kubectl*".to_string()];
        assert!(CodexAdapter::codex_approval_required(Some("kubectl delete pod x"), &protected));
        assert!(!CodexAdapter::codex_approval_required(Some("ls -la"), &protected));
        // codex 实际下发的是 shell 包装形式，拆壳后内层命中保护名单
        assert!(CodexAdapter::codex_approval_required(
            Some("/bin/zsh -lc 'kubectl get pods'"),
            &protected
        ));
        assert!(CodexAdapter::codex_approval_required(
            Some("bash -c \"kubectl delete pod a\""),
            &protected
        ));
        assert!(!CodexAdapter::codex_approval_required(
            Some("/bin/zsh -lc 'ls -la'"),
            &protected
        ));
        // None / 空白命令 → 无需审批
        assert!(!CodexAdapter::codex_approval_required(None, &protected));
        assert!(!CodexAdapter::codex_approval_required(Some("  "), &protected));
    }

    #[test]
    fn unwrap_shell_wrapper_extracts_inner_command() {
        assert_eq!(
            CodexAdapter::unwrap_shell_wrapper("/bin/zsh -lc 'kubectl get pods'").as_deref(),
            Some("kubectl get pods")
        );
        assert_eq!(
            CodexAdapter::unwrap_shell_wrapper("bash -c \"echo hi\"").as_deref(),
            Some("echo hi")
        );
        // 非包装形式 / 旗标后为空 → None
        assert_eq!(CodexAdapter::unwrap_shell_wrapper("kubectl get pods"), None);
        assert_eq!(CodexAdapter::unwrap_shell_wrapper("/bin/zsh -lc"), None);
        assert_eq!(CodexAdapter::unwrap_shell_wrapper("make -c"), None);
    }

    #[test]
    fn build_approval_reply_uses_per_kind_vocabulary() {
        assert_eq!(
            CodexAdapter::build_approval_reply(ApprovalKind::Command, true),
            json!({ "decision": "accept" })
        );
        assert_eq!(
            CodexAdapter::build_approval_reply(ApprovalKind::Command, false),
            json!({ "decision": "decline" })
        );
        assert_eq!(
            CodexAdapter::build_approval_reply(ApprovalKind::LegacyExec, true),
            json!({ "decision": "approved" })
        );
        assert_eq!(
            CodexAdapter::build_approval_reply(ApprovalKind::LegacyExec, false),
            json!({ "decision": "deny" })
        );
    }

    #[test]
    fn derive_thread_id_from_path_parses_rollout_filename() {
        let path = "/home/u/.codex/sessions/2026/06/25/rollout-2026-06-25T16-26-45-019efde3-7e01-7ff0-abd0-cabaafe86ffd.jsonl";
        assert_eq!(
            CodexAdapter::derive_thread_id_from_path(path),
            Some("019efde3-7e01-7ff0-abd0-cabaafe86ffd".to_string())
        );
        // 非法形态拒绝
        assert_eq!(CodexAdapter::derive_thread_id_from_path("/tmp/rollout-bad.jsonl"), None);
        assert_eq!(CodexAdapter::derive_thread_id_from_path("/tmp/other.jsonl"), None);
        assert_eq!(CodexAdapter::derive_thread_id_from_path("rollout-.jsonl"), None);
    }

    #[test]
    fn codex_config_model_parses_top_level_key_only() {
        let cfg = "model = \"glm-5.3\"\nmodel_provider = \"x\"\n\n[tui]\nmodel = \"ignored\"\n";
        assert_eq!(CodexAdapter::codex_config_model_from_str(cfg).as_deref(), Some("glm-5.3"));
        // 顶层没有 model 键 → None（子表里的不算）
        assert_eq!(CodexAdapter::codex_config_model_from_str("[tui]\nmodel = \"y\"\n"), None);
        // 空值/坏 TOML → None
        assert_eq!(CodexAdapter::codex_config_model_from_str("model = \"\"\n"), None);
        assert_eq!(CodexAdapter::codex_config_model_from_str("not valid"), None);
    }

    #[test]
    fn user_input_questions_parse_items_and_id_pairs() {
        let raw = serde_json::json!([
            {
                "id": "q1", "header": "数据库", "question": "用哪个数据库？",
                "options": [
                    { "label": "MySQL", "description": "熟悉栈" },
                    { "label": "PG", "description": "" }
                ]
            },
            { "id": "q2", "header": "端口", "question": "端口多少？" },
            { "id": "q3", "header": "坏条目" }
        ]);
        let (items, pairs) = CodexAdapter::parse_user_input_questions(Some(&raw));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].question, "用哪个数据库？");
        assert_eq!(items[0].header, "数据库");
        assert_eq!(items[0].options.len(), 2);
        assert_eq!(items[0].options[0].label, "MySQL");
        assert_eq!(items[0].options[0].description, "熟悉栈");
        // 无 options 的自由文本问题 → 空选项列表
        assert!(items[1].options.is_empty());
        // id ↔ 文本 对齐（回包翻译依据），缺 question 的条目丢弃
        assert_eq!(pairs, vec![("q1".to_string(), "用哪个数据库？".to_string()), ("q2".to_string(), "端口多少？".to_string())]);
        // 整个字段缺失 → 空
        let (items, pairs) = CodexAdapter::parse_user_input_questions(None);
        assert!(items.is_empty() && pairs.is_empty());
    }

    #[test]
    fn build_turn_start_params_includes_model_only_when_set() {
        let p = CodexAdapter::build_turn_start_params("t1", "hello", None);
        assert_eq!(p["threadId"], "t1");
        assert_eq!(p["input"][0]["type"], "text");
        assert_eq!(p["input"][0]["text"], "hello");
        assert!(p.get("model").is_none());

        let p = CodexAdapter::build_turn_start_params("t1", "hi", Some("gpt-5.2"));
        assert_eq!(p["model"], "gpt-5.2");

        // 空白模型同样不携带
        let p = CodexAdapter::build_turn_start_params("t1", "hi", Some("  "));
        assert!(p.get("model").is_none());
    }

    fn feed(method: &str, params: serde_json::Value) -> (CodexAggregator, Vec<AgentEvent>) {
        let mut agg = CodexAggregator::new();
        let events = agg.feed(method, &params, &sid());
        (agg, events)
    }

    #[test]
    fn agent_message_deltas_aggregate_into_one_output() {
        let (mut agg, events) = feed(
            "item/agentMessage/delta",
            json!({ "delta": "你", "itemId": "i1" }),
        );
        assert!(events.is_empty());
        let events = agg.feed(
            "item/agentMessage/delta",
            &json!({ "delta": "好", "itemId": "i1" }),
            &sid(),
        );
        assert!(events.is_empty());
        let events = agg.feed(
            "item/completed",
            &json!({ "item": { "type": "agentMessage", "id": "i1", "text": "你好" } }),
            &sid(),
        );
        assert_eq!(events.len(), 1);
        let AgentEvent::Output { content, raw, .. } = &events[0] else {
            panic!("expected Output");
        };
        assert_eq!(content, "你好");
        let raw = raw.as_ref().unwrap();
        assert_eq!(raw["type"], "assistant");
        assert_eq!(raw["message"]["content"][0]["type"], "text");
        assert_eq!(raw["message"]["content"][0]["text"], "你好");
        assert!(raw["uuid"].as_str().unwrap().starts_with("codex-"));
        let _ = agg;
    }

    #[test]
    fn reasoning_flushes_before_text_in_order() {
        let (mut agg, _) = feed(
            "item/reasoning/summaryTextDelta",
            json!({ "delta": "想一下", "itemId": "r1" }),
        );
        // reasoning completed 时缓冲先落（summary 为空则用 delta 缓冲兜底）
        let flushed = agg.feed("item/completed", &json!({ "item": { "type": "reasoning", "id": "r1", "summary": [] } }), &sid());
        assert_eq!(flushed.len(), 1);
        let AgentEvent::Output { content, .. } = &flushed[0] else { panic!() };
        assert_eq!(content, "[thinking]想一下[/thinking]");
        agg.feed("item/agentMessage/delta", &json!({ "delta": "答", "itemId": "a1" }), &sid());
        let events = agg.feed(
            "item/completed",
            &json!({ "item": { "type": "agentMessage", "id": "a1", "text": "答" } }),
            &sid(),
        );
        assert_eq!(events.len(), 1);
        let AgentEvent::Output { content, .. } = &events[0] else { panic!() };
        assert_eq!(content, "答");
    }

    #[test]
    fn item_started_command_emits_tool_use_card() {
        let (_, events) = feed(
            "item/started",
            json!({ "item": { "type": "commandExecution", "id": "c1", "command": "ls -la", "cwd": "/tmp" } }),
        );
        assert_eq!(events.len(), 1);
        let AgentEvent::Output { content, raw, .. } = &events[0] else { panic!() };
        assert_eq!(content, r#"[tool_use]Bash {"command":"ls -la","cwd":"/tmp"}[/tool_use]"#);
        let raw = raw.as_ref().unwrap();
        assert_eq!(raw["message"]["content"][0]["type"], "tool_use");
        assert_eq!(raw["message"]["content"][0]["id"], "c1");
        assert_eq!(raw["message"]["content"][0]["name"], "Bash");
    }

    #[test]
    fn item_completed_command_emits_tool_result_with_error_flag() {
        let (_, events) = feed(
            "item/completed",
            json!({ "item": { "type": "commandExecution", "id": "c1", "status": "completed", "exitCode": 1, "aggregatedOutput": "boom" } }),
        );
        assert_eq!(events.len(), 1);
        let AgentEvent::Output { content, raw, .. } = &events[0] else { panic!() };
        assert_eq!(content, "[tool_result tool_use_id=\"c1\"]boom[/tool_result]");
        let raw = raw.as_ref().unwrap();
        assert_eq!(raw["message"]["content"][0]["type"], "tool_result");
        assert_eq!(raw["message"]["content"][0]["is_error"], true);
    }

    #[test]
    fn item_completed_agent_message_without_deltas_uses_item_text() {
        let (_, events) = feed(
            "item/completed",
            json!({ "item": { "type": "agentMessage", "id": "i1", "text": "直接文本" } }),
        );
        assert_eq!(events.len(), 1);
        let AgentEvent::Output { content, .. } = &events[0] else { panic!() };
        assert_eq!(content, "直接文本");
    }

    #[test]
    fn turn_completed_maps_result_marker_and_status() {
        let (_, events) = feed(
            "turn/completed",
            json!({ "threadId": "t1", "turn": { "id": "tn1", "status": "completed" } }),
        );
        assert_eq!(events.len(), 2);
        let AgentEvent::Output { content, raw, .. } = &events[0] else { panic!() };
        assert_eq!(content, "");
        assert_eq!(raw.as_ref().unwrap()["type"], "result");
        let AgentEvent::SessionStatus { status, .. } = &events[1] else { panic!() };
        assert_eq!(*status, SessionStatus::Complete);
    }

    #[test]
    fn turn_completed_failed_maps_error_event() {
        let (_, events) = feed(
            "turn/completed",
            json!({ "threadId": "t1", "turn": { "id": "tn1", "status": "failed", "error": { "message": "quota exceeded" } } }),
        );
        assert_eq!(events.len(), 2);
        let AgentEvent::Error { message, .. } = &events[1] else { panic!() };
        assert_eq!(message, "quota exceeded");
    }

    #[test]
    fn unknown_items_and_notifications_are_swallowed() {
        // 未知 item 变体
        let (_, e1) = feed(
            "item/started",
            json!({ "item": { "type": "webSearch", "id": "w1" } }),
        );
        assert!(e1.is_empty());
        let (_, e2) = feed(
            "item/completed",
            json!({ "item": { "type": "plan", "id": "p1" } }),
        );
        assert!(e2.is_empty());
        // 未知通知方法
        let (_, e3) = feed("thread/status/changed", json!({ "status": { "type": "idle" } }));
        assert!(e3.is_empty());
    }

    #[test]
    fn error_notification_maps_to_error_event() {
        let (_, events) = feed("error", json!({ "message": "auth required" }));
        assert_eq!(events.len(), 1);
        let AgentEvent::Error { message, .. } = &events[0] else { panic!() };
        assert_eq!(message, "auth required");
    }

    #[test]
    fn reasoning_summary_accepts_both_element_shapes() {
        let item = json!({
            "type": "reasoning",
            "id": "r1",
            "summary": [{ "type": "summary_text", "text": "first" }, "second"],
        });
        assert_eq!(reasoning_summary_text(&item), "first\nsecond");
    }
}
