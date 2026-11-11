//! smartcode（原 millet-ai）适配器：Subprocess 形态，驱动 `smartcode console` 长驻进程。
//!
//! 协议（millet-ai docs/ndjson-events.md）：
//! - stdin 指令（NDJSON）：user_message / control_response / interrupt / shutdown
//! - stdout 事件（NDJSON LoopEvent）：text_delta / thinking_delta / tool_use_received /
//!   permission_request / done 等 16 种
//! - stderr 报告 `session: <path>`（smartcode 自生成的会话 JSONL 路径），平台把它持久化
//!   为 record.claude_session_id —— restore/history 都以它为句柄（见 trait `session_ref`）。
//!
//! 与 claude 的关键差异：console 模式下 smartcode 的 `done` 表示一轮结束而进程继续驻留，
//! 因此 done 映射为空 Output（raw.type 改写为 "result"）作为前端 turn 标记；
//! SessionComplete 只由进程真正退出（wait 任务）触发。

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

/// 等待 stderr `session: <path>` 的超时。smartcode 在 Session::create 后立即打印，
/// 正常情况下毫秒级；超时意味着启动失败（磁盘错误 / 参数错误）。
const SESSION_PATH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SmartcodeAdapter {
    event_bus: std::sync::Arc<crate::events::EventBus>,
    config_manager: std::sync::Arc<crate::config::ConfigManager>,
    /// 系统设置（GPT API 配置）——启动注入 model/base_url/api_key 的权威来源
    settings: crate::db::SettingsStore,
    sessions: parking_lot::Mutex<std::collections::HashMap<SessionId, MilletSessionState>>,
}

struct MilletSessionState {
    stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
    /// 只被消费一次：spawn_event_task 通过 output_stream 取走
    rx: parking_lot::Mutex<Option<mpsc::Receiver<AgentEvent>>>,
    /// 事件通道发送端：send_message 等非 reader 路径投递状态事件要同时走
    /// 通道（session 任务更新内存/落库）和 bus（WS 前端），与 reader 的双投递一致
    event_tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    /// 进程句柄归 wait 任务所有；stop 通过 notify 请求硬杀（kill 兜底）
    kill_notify: std::sync::Arc<tokio::sync::Notify>,
    /// wait 任务在进程终止（wait 或 kill 任一分支完成）后发信号；
    /// kill_existing（resume 前置）用它同步等待旧进程退出
    dead_rx: TokioMutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// 进程是否仍在运行：构造为 true，wait 任务结束时置 false（has_process 依据）
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pending_permission: std::sync::Arc<parking_lot::Mutex<Option<PendingPermission>>>,
    /// stderr `session: <path>` 报告的会话 JSONL 路径
    session_path: parking_lot::Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    request_id: String,
    tool_name: String,
}

impl SmartcodeAdapter {
    pub fn new(
        event_bus: std::sync::Arc<crate::events::EventBus>,
        config_manager: std::sync::Arc<crate::config::ConfigManager>,
        settings: crate::db::SettingsStore,
    ) -> Self {
        Self {
            event_bus,
            config_manager,
            settings,
            sessions: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 从系统设置解析启动注入值。权威来源是 GPT API 配置（settings.gpt_api.<active_role>）：
    /// base_url=host、api_key=api_key；model 取 ~/.claude/settings.json 的当前模型
    /// （模型选择器与 claude 会话共用），未写入时按家族优先级从模型列表挑选。
    /// GPT API 未配置时全部返回 None，退回 smartcode CLI 默认值。
    fn resolve_system_injection(&self) -> (Option<String>, Option<String>, Option<String>) {
        let role = self
            .settings
            .get("settings.gpt_api.active_role")
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<String>(&v).ok())
            .unwrap_or_else(|| "primary".to_string());
        let Ok(Some(v)) = self.settings.get(&format!("settings.gpt_api.{}", role)) else {
            return (None, None, None);
        };
        let Ok(config) = serde_json::from_str::<serde_json::Value>(&v) else {
            return (None, None, None);
        };
        let host = config.get("host").and_then(|h| h.as_str()).map(str::trim).filter(|h| !h.is_empty());
        let api_key = config.get("api_key").and_then(|k| k.as_str()).map(str::trim).filter(|k| !k.is_empty());
        let models = config.get("model").and_then(|m| m.as_array());

        // 当前模型：claude settings.json 是平台级「当前模型」的持久化位置（与
        // api::config::get_current_model / 模型选择器一致），缺失时按优先级挑选。
        let settings_model = std::fs::read_to_string(dirs::home_dir().map(|h| h.join(".claude/settings.json")).unwrap_or_default())
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|s| s.get("model").and_then(|m| m.as_str()).map(str::to_string))
            .filter(|m| !m.is_empty());
        let model = settings_model.or_else(|| crate::api::config::select_model_by_priority(models));

        (
            model,
            host.map(str::to_string),
            api_key.map(str::to_string),
        )
    }

    /// 构建 `smartcode console` 命令。auto_approve=true → danger-full-access（smartcode
    /// 自身不再询问）；false → workspace-write（受保护操作经 permission_request 走
    /// 平台审批流）。model（恢复会话的 record.model 或系统设置当前模型）与 base_url
    /// （系统设置 GPT API host）统一放在 extra_args 之后，重复的 CLI flag 以注入值为准。
    fn build_command(
        config: &AgentConfig,
        ctx: &RuntimeContext,
        resume_path: Option<&str>,
        model: Option<&str>,
        base_url: Option<&str>,
    ) -> (String, Vec<String>) {
        let binary = if ctx.binary_path.is_empty() {
            "smartcode".to_string()
        } else {
            ctx.binary_path.clone()
        };
        let mut args = vec!["console".to_string()];
        if let Some(path) = resume_path {
            args.push("--resume".to_string());
            args.push(path.to_string());
        }
        args.push("--permission-mode".to_string());
        args.push(
            if config.auto_approve {
                "danger-full-access"
            } else {
                "workspace-write"
            }
            .to_string(),
        );
        args.extend(config.extra_args.iter().cloned());
        if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
            args.push("--model".to_string());
            args.push(m.to_string());
        }
        if let Some(b) = base_url.map(str::trim).filter(|b| !b.is_empty()) {
            args.push("--base-url".to_string());
            args.push(b.to_string());
        }
        (binary, args)
    }

    /// SMART_API_KEY 取系统设置（GPT API active role）解析出的 api_key，None 时退回实例配置。
    fn build_env(config: &AgentConfig, ctx: &RuntimeContext, api_key: Option<&str>) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(api_key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
            env.push(("SMART_API_KEY".to_string(), api_key.to_string()));
        }
        env.extend(ctx.gitea_env.iter().cloned());
        env
    }

    /// Spawn smartcode 进程，返回 (process, stdout, stdin, pid, session_path receiver)。
    /// stderr 中 `session: <path>` 行经 oneshot 回传；其余 stderr 行仅记日志。
    #[allow(clippy::type_complexity)]
    async fn spawn_smartcode_process(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        resume_path: Option<&str>,
        working_dir: Option<&str>,
        log_suffix: &str,
        model_override: Option<&str>,
    ) -> Result<(
        Box<dyn Process>,
        Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
        u32,
        tokio::sync::oneshot::Receiver<Result<String, String>>,
    )> {
        // 注入值统一来自系统设置：会话持久化模型（record.model）优先于
        // 系统设置当前模型；base_url/api_key 取 GPT API active role 配置，
        // api_key 缺失时退回实例自身配置。
        let (sys_model, sys_base_url, sys_api_key) = self.resolve_system_injection();
        let model = model_override.or(sys_model.as_deref());
        let base_url = sys_base_url.as_deref();
        let api_key = sys_api_key.as_deref().or(config.api_key.as_deref());

        let (cmd, args) = Self::build_command(config, ctx, resume_path, model, base_url);
        let env = Self::build_env(config, ctx, api_key);
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
                .map_err(|e| {
                    anyhow::anyhow!("Failed to spawn smartcode process: {}. Is 'smartcode' installed?", e)
                })?
        };

        let pid = process.id();
        let stdout = process.take_stdout().ok_or_else(|| anyhow::anyhow!("stdout not available"))?;
        let stdin = process.take_stdin().ok_or_else(|| anyhow::anyhow!("stdin not available"))?;

        let (path_tx, path_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
        let suffix = log_suffix.to_string();
        if let Some(stderr) = process.take_stderr() {
            tokio::spawn(async move {
                let mut announced = false;
                let mut path_tx = Some(path_tx);
                // 失败诊断用：进程启动即退出时，把 stderr 前几行带进错误消息
                // （如 "no such file or directory"、"command not found"、坏参数）
                let mut preview: Vec<String> = Vec::new();
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !announced {
                        if let Some(path) = line.strip_prefix("session: ") {
                            announced = true;
                            if let Some(tx) = path_tx.take() {
                                let _ = tx.send(Ok(path.trim().to_string()));
                            }
                            continue;
                        }
                        if preview.len() < 8 {
                            preview.push(line.chars().take(300).collect());
                        }
                    }
                    tracing::debug!(line_preview = line.chars().take(200).collect::<String>(), "smartcode stderr {}", suffix);
                }
                if let Some(tx) = path_tx.take() {
                    let _ = tx.send(Err(preview.join(" | ")));
                }
            });
        } else {
            drop(path_tx);
        }

        Ok((process, stdout, stdin, pid, path_rx))
    }

    /// 等待 stderr 会话路径。失败（超时 / 进程未报告）返回错误；stderr 有输出时
    /// 把前几行带进错误消息，远程「二进制不存在 / 参数错误」可直接定位。
    async fn wait_session_path(
        path_rx: tokio::sync::oneshot::Receiver<Result<String, String>>,
    ) -> Result<String> {
        match tokio::time::timeout(SESSION_PATH_TIMEOUT, path_rx).await {
            Ok(Ok(Ok(path))) if !path.is_empty() => Ok(path),
            Ok(Ok(Ok(_))) => anyhow::bail!("smartcode reported an empty session path"),
            Ok(Ok(Err(stderr_preview))) if !stderr_preview.is_empty() => anyhow::bail!(
                "smartcode 启动失败（未报告会话路径）。stderr: {}",
                stderr_preview
            ),
            Ok(Ok(Err(_))) => anyhow::bail!(
                "smartcode 未报告会话路径且 stderr 无输出——进程可能启动即退出，请检查 smartcode 安装（远程主机需 discovery paths.<agent> 或实例 remote_path 指向有效二进制）与参数"
            ),
            Ok(Err(_)) => anyhow::bail!(
                "smartcode 未报告会话路径（stderr 通道关闭）——进程可能启动即退出，请检查 smartcode 安装与参数"
            ),
            Err(_) => anyhow::bail!(
                "等待 smartcode 会话路径超时（{}s）——进程可能未正常启动",
                SESSION_PATH_TIMEOUT.as_secs()
            ),
        }
    }

    /// 启动 stdout NDJSON 读取任务：LoopEvent → AgentEvent 映射见 `map_loop_event`。
    /// permission_request 先经 `approval_required` 过滤（对齐 claude 基线：只读/
    /// 写文件/普通 Bash 自动放行，仅保护名单转人工），未命中的直接写 control_response
    /// allow 回 stdin，不转发给前端。
    #[allow(clippy::too_many_arguments)]
    fn spawn_reader_task(
        stdout: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        session_id: SessionId,
        tx: mpsc::Sender<AgentEvent>,
        event_bus: std::sync::Arc<crate::events::EventBus>,
        cancel: CancellationToken,
        pending_permission: std::sync::Arc<parking_lot::Mutex<Option<PendingPermission>>>,
        stdin: std::sync::Arc<TokioMutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>,
        protected_commands: Vec<String>,
        log_suffix: &str,
    ) {
        let session_for_reader = session_id.clone();
        let suffix = log_suffix.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut aggregator = DeltaAggregator::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if cancel.is_cancelled() {
                    break;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                    tracing::debug!(line_preview = line.chars().take(120).collect::<String>(), "smartcode stdout 非 JSON 行 {}", suffix);
                    continue;
                };
                if v.get("type").and_then(|t| t.as_str()) == Some("permission_request") {
                    let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("unknown").to_string();
                    let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
                    if !super::claude::approval_required(&tool, &input, &protected_commands) {
                        let request_id = v.get("request_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
                        if !request_id.is_empty() {
                            let allow = serde_json::json!({
                                "type": "control_response",
                                "request_id": request_id,
                                "decision": "allow",
                            });
                            match Self::write_stdin(&stdin, allow).await {
                                Ok(()) => {
                                    tracing::info!(tool = %tool, "Auto-allowed by approval policy (not in protected commands)");
                                    continue;
                                }
                                Err(e) => {
                                    tracing::warn!("auto-allow write failed ({}); falling back to manual approval", e);
                                }
                            }
                        }
                    }
                }
                for event in aggregator.feed(&v, &session_for_reader) {
                    // reader 侧登记待决审批，供 handle_approval 校验 request_id
                    if let AgentEvent::ApprovalRequest { request_id: Some(rid), tool_name: Some(tool), .. } = &event {
                        *pending_permission.lock() = Some(PendingPermission {
                            request_id: rid.clone(),
                            tool_name: tool.clone(),
                        });
                    }
                    let _ = tx.send(event.clone()).await;
                    event_bus.emit(event);
                }
            }
            event_bus.emit(AgentEvent::SessionComplete {
                session_id: session_for_reader.clone(),
                exit_code: 0,
                ts: AgentEvent::now(),
            });
        });
    }

    /// 进程退出监视任务：进程句柄在此任务独占（wait 需要 &mut），stop 的硬杀
    /// 请求经 kill_notify 传入 —— select! 两条分支，谁先到走谁。异常退出（wait
    /// Err）发 Error 状态；正常退出只记日志（console 的 done 不退出进程，
    /// 一轮完成 ≠ 进程结束）。
    fn spawn_wait_task(
        kill_notify: std::sync::Arc<tokio::sync::Notify>,
        dead_tx: tokio::sync::oneshot::Sender<()>,
        alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
        event_bus: std::sync::Arc<crate::events::EventBus>,
        session_id: SessionId,
        process: Box<dyn Process>,
    ) {
        tokio::spawn(async move {
            let mut p = process;
            use std::sync::atomic::Ordering;
            tokio::select! {
                status = p.wait() => {
                    tracing::info!(?status, "smartcode process exited");
                    if status.is_err() {
                        event_bus.emit(AgentEvent::SessionStatus {
                            session_id,
                            status: SessionStatus::Error,
                            ts: AgentEvent::now(),
                        });
                    }
                }
                _ = kill_notify.notified() => {
                    // notify_one 在无等待者时会存 permit，此分支晚到也能命中
                    if let Err(e) = p.kill().await {
                        tracing::warn!(session = %session_id, error = %e, "smartcode kill failed");
                    }
                    let _ = p.wait().await;
                    tracing::info!(session = %session_id, "smartcode process killed by stop");
                }
            }
            alive.store(false, Ordering::Release);
            let _ = dead_tx.send(());
        });
    }

    /// 杀掉现有进程并移除会话状态（resume 前置步骤）。返回被杀会话的
    /// session_path（restore/resume 仍用同一路径）。进程句柄在 wait 任务手里，
    /// 经 kill_notify 请求硬杀并等它确认退出后再返回。
    async fn kill_existing(&self, session_id: &SessionId) -> Option<String> {
        let existing = self.sessions.lock().remove(session_id);
        let state = existing?;
        state.cancel.cancel();
        let path = state.session_path.lock().clone();
        let dead_rx = state.dead_rx.lock().await.take();
        state.kill_notify.notify_one();
        if let Some(rx) = dead_rx {
            // wait 任务任一分支完成都意味着进程已不在运行
            let _ = rx.await;
        }
        path
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

    /// 组装 `set_config` 指令（空串视为未提供）；全部为空时返回 None ——
    /// 上游要求至少给一个字段，否则整行按坏指令跳过。
    fn build_set_config(
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
        provider: Option<&str>,
    ) -> Option<serde_json::Value> {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("set_config".to_string()),
        );
        for (key, value) in [
            ("model", model),
            ("base_url", base_url),
            ("api_key", api_key),
            ("provider", provider),
        ] {
            if let Some(v) = value {
                if !v.trim().is_empty() {
                    obj.insert(
                        key.to_string(),
                        serde_json::Value::String(v.to_string()),
                    );
                }
            }
        }
        if obj.len() == 1 {
            None
        } else {
            Some(serde_json::Value::Object(obj))
        }
    }
}

/// smartcode LoopEvent（NDJSON）→ 平台 AgentEvent 的聚合器。
///
/// smartcode 以 delta 流输出 text/thinking（每条只有几个字），而平台前端把每个
/// Output 渲染为一条消息——直接透传会产生"一个字一个对话框"。聚合到 claude
/// 的"一条 assistant 消息一个 Output"粒度：消息边界是 thinking→text 切换、
/// tool_use_received 与 assistant_done/done，块顺序 thinking→text→tool_use。
struct DeltaAggregator {
    thinking: String,
    text: String,
    /// 实时 Output 的合成 uuid 序号（前端 Svelte each key 需唯一）
    seq: u64,
}

impl DeltaAggregator {
    fn new() -> Self {
        Self { thinking: String::new(), text: String::new(), seq: 0 }
    }

    /// 构造与 claude JSONL 行同构的 raw：`{type:"assistant", uuid, message:{content:[block]}}`。
    /// 前端 wsBridge 从 raw.message.content 取结构化块渲染卡片；uuid 用作消息 key。
    fn assistant_raw(&mut self, block: serde_json::Value) -> serde_json::Value {
        self.seq += 1;
        serde_json::json!({
            "type": "assistant",
            "uuid": format!("smartcode-{}", self.seq),
            "message": { "content": [block] },
        })
    }

    /// 进一条 LoopEvent，出零或多个 AgentEvent。
    /// 未知 type / 纯进度事件（turn_started、tool_started 等）→ 空 Vec（吞掉，
    /// 避免泄漏原始行给用户）。
    fn feed(&mut self, v: &serde_json::Value, session_id: &SessionId) -> Vec<AgentEvent> {
        let evt = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match evt {
            "text_delta" => {
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                // thinking 段必然在 text 之前结束（Anthropic 块序），先落盘
                let events = self.flush_thinking(session_id);
                self.text.push_str(text);
                events
            }
            "thinking_delta" => {
                let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                self.thinking.push_str(text);
                vec![]
            }
            "tool_use_received" => {
                let mut events = self.flush_all(session_id);
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                let raw = self.assistant_raw(serde_json::json!({
                    "type": "tool_use",
                    "id": id,
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
            "tool_finished" => {
                // 结果内容不在事件里（只有 id/is_error/duration_ms），发空内容块
                // 供前端把工具卡片标记为完成；完整结果以历史加载为准。
                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
                if id.is_empty() {
                    return vec![];
                }
                let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                let raw = serde_json::json!({
                    "type": "user",
                    "uuid": format!("smartcode-tr-{}", id),
                    "message": { "content": [{
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": "",
                        "is_error": is_error,
                    }] },
                });
                vec![AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: format!("[tool_result tool_use_id=\"{}\"][/tool_result]", id),
                    ts: AgentEvent::now(),
                    raw: Some(raw),
                }]
            }
            "assistant_done" => self.flush_all(session_id),
            "permission_request" => {
                let request_id = v.get("request_id").and_then(|r| r.as_str()).unwrap_or("").to_string();
                if request_id.is_empty() {
                    tracing::warn!("permission_request without request_id — ignored");
                    return vec![];
                }
                let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("unknown").to_string();
                let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
                let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string();
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                let prompt = if reason.is_empty() {
                    format!("Tool '{}' requested: {}", tool, input_str.chars().take(500).collect::<String>())
                } else {
                    format!(
                        "Tool '{}' requested: {}\n{}",
                        tool,
                        input_str.chars().take(500).collect::<String>(),
                        reason
                    )
                };
                vec![AgentEvent::ApprovalRequest {
                    session_id: session_id.clone(),
                    prompt,
                    options: vec![
                        "Approve".to_string(),
                        "Deny".to_string(),
                        "Provide feedback...".to_string(),
                    ],
                    request_id: Some(request_id),
                    tool_name: Some(tool),
                    ts: AgentEvent::now(),
                }]
            }
            "done" => {
                // 兜底 flush（assistant_done 缺席的异常路径不丢内容），随后发
                // 轮次结束标记。raw.type 改写为 "result"（前端 turn 完成信号），
                // 保留 reason/total_turns/usage 供展示。console 模式 done 后进程
                // 驻留，不发 SessionComplete —— 那由进程退出的 wait 任务负责。
                let mut events = self.flush_all(session_id);
                let mut raw = v.clone();
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert("type".to_string(), serde_json::Value::String("result".to_string()));
                }
                events.push(AgentEvent::Output {
                    session_id: session_id.clone(),
                    content: String::new(),
                    ts: AgentEvent::now(),
                    raw: Some(raw),
                });
                let kind = v.pointer("/reason/kind").and_then(|k| k.as_str()).unwrap_or("");
                let message = v.pointer("/reason/message").and_then(|m| m.as_str()).unwrap_or("");
                match kind {
                    "provider_error" => events.push(AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: if message.is_empty() { "provider error".to_string() } else { format!("provider error: {}", message) },
                        ts: AgentEvent::now(),
                    }),
                    "fatal" => events.push(AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: if message.is_empty() { "fatal error".to_string() } else { format!("fatal: {}", message) },
                        ts: AgentEvent::now(),
                    }),
                    "context_too_long" => events.push(AgentEvent::Error {
                        session_id: session_id.clone(),
                        message: "context too long: 会话上下文已超限，请新开会话或精简历史".to_string(),
                        ts: AgentEvent::now(),
                    }),
                    _ => {
                        // 驻留型轮次结束（completed/aborted/max_turns_reached）：
                        // 进程仍存活，翻回 Complete 让前端回到可输入状态；
                        // 下一条 user_message 由 send_message 翻回 Running。
                        events.push(AgentEvent::SessionStatus {
                            session_id: session_id.clone(),
                            status: SessionStatus::Complete,
                            ts: AgentEvent::now(),
                        });
                    }
                }
                events
            }
            "config_changed" => {
                vec![AgentEvent::ConfigChanged {
                    session_id: session_id.clone(),
                    model: v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()),
                    provider: v.get("provider").and_then(|p| p.as_str()).map(|s| s.to_string()),
                    base_url: v.get("base_url").and_then(|b| b.as_str()).map(|s| s.to_string()),
                    ts: AgentEvent::now(),
                }]
            }
            _ => vec![],
        }
    }

    fn flush_thinking(&mut self, session_id: &SessionId) -> Vec<AgentEvent> {
        if self.thinking.is_empty() {
            return vec![];
        }
        let content = std::mem::take(&mut self.thinking);
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

#[async_trait]
impl AgentAdapter for SmartcodeAdapter {
    fn adapter_id(&self) -> &str {
        "smartcode"
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
        tracing::info!(session = %session_id, working_dir = ?working_dir, "Starting smartcode console session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let (process, stdout, stdin, pid, path_rx) = self
            .spawn_smartcode_process(config, ctx, None, working_dir.as_deref(), "", None)
            .await?;

        let session_path = Self::wait_session_path(path_rx).await?;
        tracing::info!(session = %session_id, path = %session_path, "smartcode session file");

        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending_permission = std::sync::Arc::new(parking_lot::Mutex::new(None));

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();
        let event_tx = tx.clone();

        Self::spawn_reader_task(
            stdout,
            session_id.clone(),
            tx,
            self.event_bus.clone(),
            cancel.clone(),
            pending_permission.clone(),
            stdin_arc.clone(),
            self.config_manager.config().permissions.protected_commands,
            "",
        );

        let kill_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let (dead_tx, dead_rx) = tokio::sync::oneshot::channel();
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        Self::spawn_wait_task(
            kill_notify.clone(),
            dead_tx,
            alive.clone(),
            self.event_bus.clone(),
            session_id.clone(),
            process,
        );

        let state = MilletSessionState {
            stdin: stdin_arc.clone(),
            rx: parking_lot::Mutex::new(Some(rx)),
            event_tx,
            cancel,
            kill_notify,
            dead_rx: TokioMutex::new(Some(dead_rx)),
            alive,
            pending_permission,
            session_path: parking_lot::Mutex::new(Some(session_path)),
        };
        self.sessions.lock().insert(session_id.clone(), state);

        // console 模式等 stdin 指令：初始 prompt 以 user_message 下发
        Self::write_stdin(
            &stdin_arc,
            serde_json::json!({ "type": "user_message", "text": prompt }),
        )
        .await?;

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(SessionHandle::new(
            session_id,
            "smartcode".to_string(),
            SessionHandleState::Subprocess { pid },
        ))
    }

    async fn send_message(&self, session: &SessionHandle, message: &str) -> Result<()> {
        let (stdin, event_tx) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (state.stdin.clone(), state.event_tx.clone())
        };
        Self::write_stdin(
            &stdin,
            serde_json::json!({ "type": "user_message", "text": message }),
        )
        .await?;
        // 轮次开始：翻回 Running（done 时已置 Complete），否则前端会一直
        // 显示完成态、输入框看似发出却没有进行中的轮次。双投递与 reader
        // 任务一致：通道给 session 任务（内存+落库），bus 给 WS 前端。
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
        let payload = Self::build_set_config(model, base_url, api_key, provider)
            .ok_or_else(|| anyhow::anyhow!("set_config: 至少需要提供一个字段（model/base_url/api_key/provider）"))?;
        let stdin = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            state.stdin.clone()
        };
        Self::write_stdin(&stdin, payload).await
    }

    async fn handle_approval(
        &self,
        session: &SessionHandle,
        request_id: Option<&str>,
        response: ApprovalResponse,
    ) -> Result<()> {
        let (stdin, pending_permission) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            (state.stdin.clone(), state.pending_permission.clone())
        };

        let entry = pending_permission.lock().take();
        let Some(entry) = entry else {
            anyhow::bail!("No pending permission request for session {}", session.id);
        };
        let current_id = entry.request_id.clone();
        if let Some(rid) = request_id {
            if rid != current_id {
                *pending_permission.lock() = Some(entry);
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
        let payload = if approve {
            serde_json::json!({
                "type": "control_response",
                "request_id": entry.request_id,
                "decision": "allow",
            })
        } else {
            let comment = response
                .comment
                .clone()
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| "User denied this tool use".to_string());
            serde_json::json!({
                "type": "control_response",
                "request_id": entry.request_id,
                "decision": "deny",
                "comment": comment,
            })
        };
        Self::write_stdin(&stdin, payload).await?;

        tracing::info!(
            session = %session.id,
            request_id = %entry.request_id,
            tool = %entry.tool_name,
            approved = approve,
            "smartcode control_response sent"
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
        Self::write_stdin(&stdin, serde_json::json!({ "type": "interrupt" })).await?;
        tracing::info!(session = %session.id, "smartcode interrupt sent");
        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, _force: bool) -> Result<()> {
        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session.id.clone(),
            status: SessionStatus::Stopped,
            ts: AgentEvent::now(),
        });

        let Some(state) = self.sessions.lock().remove(&session.id) else {
            tracing::info!(session = %session.id, "smartcode stop: no live adapter state (process may have already exited)");
            return Ok(());
        };
        state.cancel.cancel();
        let stdin = state.stdin.clone();
        let kill_notify = state.kill_notify.clone();
        // 优雅收尾先行：shutdown 让 console 落盘后自行退出；wait 任务持有进程
        // 句柄，若 shutdown 未生效，下面的 kill_notify 触发硬杀兜底。
        tokio::spawn(async move {
            let payload = serde_json::json!({ "type": "shutdown" });
            let result = async {
                let mut guard = stdin.lock().await;
                guard.write_all(format!("{}\n", payload).as_bytes()).await?;
                guard.flush().await?;
                Ok::<(), std::io::Error>(())
            };
            if let Err(e) = result.await {
                tracing::warn!(error = %e, "smartcode shutdown write failed; relying on kill fallback");
            }
        });
        // 给 shutdown 一点落盘时间（turn 进行中的优雅中止含断开 provider 请求，
        // 可能超过 1s），随后硬杀（幂等；进程已自行退出时为无害 no-op）
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            kill_notify.notify_one();
        });

        Ok(())
    }

    fn has_process(&self, session_id: &SessionId) -> bool {
        use std::sync::atomic::Ordering;
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
        // claude_session_id 槽位存的是 smartcode 的会话 JSONL 路径
        Self::restore_session(
            self, config, ctx, session_id, claude_session_id, working_dir, model,
        )
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
        tracing::info!(session = %session_id, path = %claude_session_id, "Resuming smartcode session with new config");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Restarting,
            ts: AgentEvent::now(),
        });

        let known_path = self.kill_existing(&session_id).await;
        // 优先用本次持有状态里的路径（与 record 一致；record 缺失时兜底）
        let resume_path = known_path
            .or_else(|| (!claude_session_id.is_empty()).then(|| claude_session_id.to_string()))
            .ok_or_else(|| anyhow::anyhow!("no session path available for resume"))?;

        let handle = Self::restore_session(self, config, ctx, session_id, &resume_path, working_dir, model).await?;
        Ok(handle)
    }

    fn session_ref(&self, session_id: &SessionId) -> Option<String> {
        let sessions = self.sessions.lock();
        let state = sessions.get(session_id)?;
        let path = state.session_path.lock().clone();
        path
    }
}

impl SmartcodeAdapter {
    /// 以 `--resume <path>` 重新拉起 console 进程（restore / resume 共用）。
    /// 不下发初始 prompt —— 上下文来自会话文件。model 为该会话持久化的模型，
    /// 随 `--model` 注入，恢复后的首轮请求即使用正确模型。
    async fn restore_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        resume_path: &str,
        working_dir: Option<String>,
        model: Option<&str>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, path = %resume_path, "Restoring smartcode session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let (process, stdout, stdin, pid, path_rx) = self
            .spawn_smartcode_process(
                config,
                ctx,
                Some(resume_path),
                working_dir.as_deref(),
                "(restored)",
                model,
            )
            .await?;

        let session_path = Self::wait_session_path(path_rx).await?;

        let stdin_arc = std::sync::Arc::new(TokioMutex::new(stdin));
        let pending_permission = std::sync::Arc::new(parking_lot::Mutex::new(None));

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();
        let event_tx = tx.clone();

        Self::spawn_reader_task(
            stdout,
            session_id.clone(),
            tx,
            self.event_bus.clone(),
            cancel.clone(),
            pending_permission.clone(),
            stdin_arc.clone(),
            self.config_manager.config().permissions.protected_commands,
            "(restored)",
        );

        let kill_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let (dead_tx, dead_rx) = tokio::sync::oneshot::channel();
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        Self::spawn_wait_task(
            kill_notify.clone(),
            dead_tx,
            alive.clone(),
            self.event_bus.clone(),
            session_id.clone(),
            process,
        );

        let state = MilletSessionState {
            stdin: stdin_arc,
            rx: parking_lot::Mutex::new(Some(rx)),
            event_tx,
            cancel,
            kill_notify,
            dead_rx: TokioMutex::new(Some(dead_rx)),
            alive,
            pending_permission,
            session_path: parking_lot::Mutex::new(Some(session_path)),
        };
        self.sessions.lock().insert(session_id.clone(), state);

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        Ok(SessionHandle::new(
            session_id,
            "smartcode".to_string(),
            SessionHandleState::Subprocess { pid },
        ))
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
            name: "MilletCode".to_string(),
            extra_args: vec![],
            api_key: None,
            auto_approve,
            enabled: true,
            allow_config_overwrite: true,
            path: None,
            remote_path: None,
            release_channel: "alpha".to_string(),
        }
    }

    #[test]
    fn build_command_maps_permission_mode_and_resume() {
        let (cmd, args) = SmartcodeAdapter::build_command(&config(false), &ctx(), None, None, None);
        assert_eq!(cmd, "smartcode");
        assert_eq!(args, vec!["console", "--permission-mode", "workspace-write"]);

        let (cmd, args) =
            SmartcodeAdapter::build_command(&config(true), &ctx(), Some("/tmp/s.jsonl"), None, None);
        assert_eq!(cmd, "smartcode");
        assert_eq!(
            args,
            vec!["console", "--resume", "/tmp/s.jsonl", "--permission-mode", "danger-full-access"]
        );
    }

    #[test]
    fn build_command_uses_binary_path_and_extra_args() {
        let mut c = config(true);
        c.extra_args = vec!["--model".to_string(), "kimi-k2".to_string()];
        let mut c2 = ctx();
        c2.binary_path = "/opt/smartcode/bin/smartcode".to_string();
        let (cmd, args) = SmartcodeAdapter::build_command(&c, &c2, None, None, None);
        assert_eq!(cmd, "/opt/smartcode/bin/smartcode");
        assert_eq!(
            args,
            vec![
                "console",
                "--permission-mode",
                "danger-full-access",
                "--model",
                "kimi-k2"
            ]
        );
    }

    #[test]
    fn build_command_appends_model_and_base_url_after_extra_args() {
        // 注入值（模型/base_url）在 extra_args 之后：clap 重复 flag 以最后一次为准，
        // 系统设置的注入值不会被 extra_args 残留覆盖
        let mut c = config(false);
        c.extra_args = vec!["--model".to_string(), "glm-5.3".to_string()];
        let (_, args) = SmartcodeAdapter::build_command(
            &c,
            &ctx(),
            None,
            Some("glm-5.3-flash"),
            Some("https://llm.example.com"),
        );
        let model_pos = args
            .iter()
            .rposition(|a| a == "--model")
            .expect("model flag present");
        assert_eq!(args[model_pos + 1], "glm-5.3-flash");
        assert_eq!(
            args,
            vec![
                "console",
                "--permission-mode",
                "workspace-write",
                "--model",
                "glm-5.3",
                "--model",
                "glm-5.3-flash",
                "--base-url",
                "https://llm.example.com"
            ]
        );
    }

    #[test]
    fn build_command_skips_blank_model_fields() {
        let (_, args) = SmartcodeAdapter::build_command(&config(false), &ctx(), None, Some("  "), Some(""));
        assert!(!args.contains(&"--model".to_string()));
        assert!(!args.contains(&"--base-url".to_string()));
    }

    #[test]
    fn build_env_maps_api_key_and_gitea() {
        // api_key 已在 spawn 时解析（系统设置优先、实例配置兜底），build_env 只负责注入
        let mut c2 = ctx();
        c2.gitea_env = vec![("GITEA_TOKEN".to_string(), "t0".to_string())];
        let env = SmartcodeAdapter::build_env(&config(true), &c2, Some("sk-settings"));
        assert_eq!(
            env,
            vec![
                ("SMART_API_KEY".to_string(), "sk-settings".to_string()),
                ("GITEA_TOKEN".to_string(), "t0".to_string()),
            ]
        );

        // None（系统设置与实例都没有）→ 不注入
        assert_eq!(SmartcodeAdapter::build_env(&config(true), &ctx(), None), vec![]);
        // 空白串视为未提供
        assert_eq!(SmartcodeAdapter::build_env(&config(true), &ctx(), Some("  ")), vec![]);
    }

    fn sid() -> SessionId {
        SessionId::new("k/p/smartcode/s".to_string())
    }

    fn feed_all(aggregator: &mut DeltaAggregator, events: &[serde_json::Value]) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for v in events {
            out.extend(aggregator.feed(v, &sid()));
        }
        out
    }

    #[test]
    fn consecutive_text_deltas_aggregate_into_one_output() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({"type":"text_delta","turn":1,"text":"你"}),
            json!({"type":"text_delta","turn":1,"text":"好"}),
            json!({"type":"text_delta","turn":1,"text":"，世界"}),
            json!({"type":"done","reason":{"kind":"completed"},"total_turns":1,"usage":{}}),
        ]);
        // 1 条完整文本 + 1 个 result 标记 + 1 个 Complete 状态（轮次结束、进程驻留）
        assert_eq!(events.len(), 3);
        match &events[0] {
            AgentEvent::Output { content, raw, .. } => {
                assert_eq!(content, "你好，世界");
                let raw = raw.as_ref().expect("text Output 应携带 claude 同构 raw");
                assert_eq!(raw["type"], "assistant");
                assert_eq!(raw["message"]["content"][0]["type"], "text");
                assert_eq!(raw["message"]["content"][0]["text"], "你好，世界");
            }
            other => panic!("应为 Output: {other:?}"),
        }
        assert!(matches!(&events[1], AgentEvent::Output { raw: Some(_), .. }));
        assert!(matches!(&events[2], AgentEvent::SessionStatus { status: SessionStatus::Complete, .. }));
    }

    #[test]
    fn empty_text_delta_is_dropped() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({"type":"text_delta","turn":1,"text":""}),
            json!({"type":"assistant_done","turn":1,"stop_reason":"end_turn","usage":{}}),
        ]);
        assert!(events.is_empty(), "空 delta 不应产出空消息");
    }

    #[test]
    fn thinking_flushes_on_text_transition_in_order() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({"type":"thinking_delta","turn":1,"text":"先想"}),
            json!({"type":"thinking_delta","turn":1,"text":"一下"}),
            json!({"type":"text_delta","turn":1,"text":"回答"}),
            json!({"type":"done","reason":{"kind":"completed"},"total_turns":1,"usage":{}}),
        ]);
        // thinking + 文本 + result 标记 + Complete 状态
        assert_eq!(events.len(), 4);
        match &events[0] {
            AgentEvent::Output { content, .. } => assert_eq!(content, "[thinking]先想一下[/thinking]"),
            other => panic!("应为 thinking Output: {other:?}"),
        }
        match &events[1] {
            AgentEvent::Output { content, .. } => assert_eq!(content, "回答"),
            other => panic!("应为 text Output: {other:?}"),
        }
    }

    #[test]
    fn tool_use_flushes_pending_text_first() {
        let mut agg = DeltaAggregator::new();
        let tool_v = json!({"type":"tool_use_received","turn":1,"id":"t1","name":"Read","input":{"path":"/a"}});
        let events = feed_all(&mut agg, &[
            json!({"type":"text_delta","turn":1,"text":"我来看看"}),
            tool_v.clone(),
        ]);
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::Output { content, .. } => assert_eq!(content, "我来看看"),
            other => panic!("应为 text Output: {other:?}"),
        }
        match &events[1] {
            AgentEvent::Output { content, raw, .. } => {
                assert_eq!(content, r#"[tool_use]Read {"path":"/a"}[/tool_use]"#);
                let raw = raw.as_ref().expect("tool_use Output 应携带 claude 同构 raw");
                assert_eq!(raw["type"], "assistant");
                assert_eq!(raw["message"]["content"][0]["type"], "tool_use");
                assert_eq!(raw["message"]["content"][0]["id"], "t1");
                assert_eq!(raw["message"]["content"][0]["name"], "Read");
            }
            other => panic!("应为 tool_use Output: {other:?}"),
        }
    }

    #[test]
    fn assistant_done_flushes_pending_buffer() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({"type":"text_delta","turn":1,"text":"第一段"}),
            json!({"type":"assistant_done","turn":1,"stop_reason":"end_turn","usage":{}}),
            json!({"type":"text_delta","turn":1,"text":"第二段"}),
            json!({"type":"assistant_done","turn":1,"stop_reason":"end_turn","usage":{}}),
        ]);
        // 两条 assistant 消息各自独立成段（对齐 claude 的一条消息一个 Output）
        assert_eq!(events.len(), 2);
        assert_eq!(as_output_content(&events[0]), "第一段");
        assert_eq!(as_output_content(&events[1]), "第二段");
    }

    fn as_output_content(event: &AgentEvent) -> &str {
        match event {
            AgentEvent::Output { content, .. } => content,
            other => panic!("应为 Output: {other:?}"),
        }
    }

    #[test]
    fn permission_request_maps_to_approval() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({
                "type":"permission_request",
                "request_id":"p1",
                "tool":"Bash",
                "input":{"command":"rm -rf /tmp/x"},
                "reason":"command `rm` not in safe lists"
            }),
        ]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ApprovalRequest { prompt, options, request_id, tool_name, .. } => {
                assert_eq!(request_id.as_deref(), Some("p1"));
                assert_eq!(tool_name.as_deref(), Some("Bash"));
                assert!(prompt.contains("Bash"));
                assert!(prompt.contains("rm` not in safe lists"));
                assert_eq!(options.len(), 3);
            }
            other => panic!("应为 ApprovalRequest: {other:?}"),
        }
    }

    #[test]
    fn permission_request_without_id_is_dropped() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({"type":"permission_request","tool":"Bash","input":{},"reason":"r"}),
        ]);
        assert!(events.is_empty());
    }

    #[test]
    fn config_changed_maps_to_config_changed_event() {
        let mut agg = DeltaAggregator::new();
        let events = feed_all(&mut agg, &[
            json!({"type":"config_changed","model":"glm-5.3","provider":"anthropic","base_url":"https://llm.example.com"}),
        ]);
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ConfigChanged { model, provider, base_url, .. } => {
                assert_eq!(model.as_deref(), Some("glm-5.3"));
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(base_url.as_deref(), Some("https://llm.example.com"));
            }
            other => panic!("应为 ConfigChanged: {other:?}"),
        }
    }

    #[test]
    fn build_set_config_includes_only_provided_fields() {
        let payload = SmartcodeAdapter::build_set_config(Some("glm-5.3"), None, None, None).unwrap();
        assert_eq!(
            payload,
            json!({"type":"set_config","model":"glm-5.3"})
        );

        let payload = SmartcodeAdapter::build_set_config(
            Some("glm-5.3-flash"),
            Some("https://open.bigmodel.cn/api/anthropic"),
            Some("sk-x"),
            Some("openai"),
        )
        .unwrap();
        assert_eq!(
            payload,
            json!({"type":"set_config","model":"glm-5.3-flash","base_url":"https://open.bigmodel.cn/api/anthropic","api_key":"sk-x","provider":"openai"})
        );
    }

    #[test]
    fn build_set_config_all_empty_is_none() {
        assert!(SmartcodeAdapter::build_set_config(None, None, None, None).is_none());
        // 空串视为未提供
        assert!(SmartcodeAdapter::build_set_config(Some(""), Some(" "), None, None).is_none());
    }

    #[test]
    fn done_with_empty_buffer_maps_to_result_marker_and_complete() {
        let mut agg = DeltaAggregator::new();
        let v = json!({
            "type":"done",
            "reason":{"kind":"completed"},
            "total_turns":3,
            "usage":{"input_tokens":10,"output_tokens":5}
        });
        let events = agg.feed(&v, &sid());
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::Output { content, raw, .. } => {
                assert_eq!(content, "");
                let raw = raw.as_ref().unwrap();
                assert_eq!(raw["type"], "result");
                assert_eq!(raw["reason"]["kind"], "completed");
                assert_eq!(raw["total_turns"], 3);
            }
            other => panic!("应为 Output: {other:?}"),
        }
        assert!(matches!(&events[1], AgentEvent::SessionStatus { status: SessionStatus::Complete, .. }));
    }

    #[test]
    fn done_stay_alive_kinds_emit_complete_status() {
        for kind in ["completed", "aborted", "max_turns_reached"] {
            let mut agg = DeltaAggregator::new();
            let v = json!({
                "type":"done",
                "reason":{"kind":kind},
                "total_turns":1,
                "usage":{"input_tokens":0,"output_tokens":0}
            });
            let events = agg.feed(&v, &sid());
            // result 标记 + Complete 状态（进程驻留，等下一条消息）
            assert_eq!(events.len(), 2, "kind={kind}");
            assert!(
                matches!(&events[1], AgentEvent::SessionStatus { status: SessionStatus::Complete, .. }),
                "kind={kind} 应发 Complete"
            );
        }
    }

    #[test]
    fn done_fatal_and_provider_error_add_error_events() {
        for (kind, prefix) in [("fatal", "fatal"), ("provider_error", "provider error")] {
            let mut agg = DeltaAggregator::new();
            let v = json!({
                "type":"done",
                "reason":{"kind":kind,"message":"boom"},
                "total_turns":1,
                "usage":{"input_tokens":0,"output_tokens":0}
            });
            let events = agg.feed(&v, &sid());
            assert_eq!(events.len(), 2, "kind={kind}");
            match &events[1] {
                AgentEvent::Error { message, .. } => {
                    assert_eq!(message, &format!("{prefix}: boom"));
                }
                other => panic!("应为 Error: {other:?}"),
            }
        }
    }

    #[test]
    fn done_context_too_long_adds_error() {
        let mut agg = DeltaAggregator::new();
        let v = json!({
            "type":"done",
            "reason":{"kind":"context_too_long"},
            "total_turns":1,
            "usage":{"input_tokens":0,"output_tokens":0}
        });
        let events = agg.feed(&v, &sid());
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], AgentEvent::Error { .. }));
    }

    #[test]
    fn progress_events_and_unknown_types_are_swallowed() {
        let mut agg = DeltaAggregator::new();
        for v in [
            json!({"type":"session_started","session_id":"s","model":"m","cwd":"/"}),
            json!({"type":"turn_started","turn":1}),
            json!({"type":"request_started","turn":1,"attempt":1}),
            json!({"type":"tool_started","turn":1,"id":"t","name":"Read"}),
            json!({"type":"turn_finished","turn":1,"cumulative_usage":{}}),
            json!({"type":"retrying","turn":1,"attempt":2,"backoff_ms":100,"reason":"x"}),
            json!({"type":"aborted","turn":1}),
            json!({"type":"from_the_future","x":1}),
        ] {
            let events = agg.feed(&v, &sid());
            assert!(events.is_empty(), "应吞掉: {v}");
        }
    }

    #[test]
    fn tool_finished_emits_tool_result_block() {
        let mut agg = DeltaAggregator::new();
        let events = agg.feed(
            &json!({"type":"tool_finished","turn":1,"id":"t9","is_error":true,"duration_ms":3}),
            &sid(),
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Output { content, raw, .. } => {
                assert!(content.contains(r#"tool_use_id="t9""#));
                let raw = raw.as_ref().expect("tool_finished 应携带 raw");
                assert_eq!(raw["type"], "user");
                assert_eq!(raw["message"]["content"][0]["type"], "tool_result");
                assert_eq!(raw["message"]["content"][0]["tool_use_id"], "t9");
                assert_eq!(raw["message"]["content"][0]["is_error"], true);
            }
            other => panic!("应为 Output: {other:?}"),
        }
        // 无 id 的 tool_finished 仍被吞掉
        let mut agg = DeltaAggregator::new();
        let events = agg.feed(
            &json!({"type":"tool_finished","turn":1,"is_error":false,"duration_ms":1}),
            &sid(),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn missing_type_field_is_swallowed() {
        let mut agg = DeltaAggregator::new();
        let events = agg.feed(&json!({"turn":1}), &sid());
        assert!(events.is_empty());
    }
}
