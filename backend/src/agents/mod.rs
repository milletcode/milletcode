use async_trait::async_trait;
use futures::Stream;
use anyhow::Result;

use crate::config::AgentConfig;

pub mod types;
pub use types::*;

mod approval;
pub use approval::{PendingControlInfo, PendingKind};

mod history;
pub use history::{
    read_session_history, parse_jsonl_history, read_smartcode_session_history, parse_smartcode_history,
    HistoryResponse,
};


pub mod adapters;
mod registry;
mod session_manager;

pub use adapters::claude::ClaudeAdapter;
pub use adapters::deepseek::DeepSeekAdapter;
pub use adapters::smartcode::SmartcodeAdapter;
pub use registry::AgentRegistry;
pub use session_manager::SessionManager;

/// Runtime context passed to adapters — contains per-session execution details
/// that come from Project/DiscoveryPath rather than AgentConfig.
pub struct RuntimeContext {
    /// Hostname for remote execution (empty for local)
    pub hostname: String,
    /// Remote mode: "" = local, "ssh" = SSH remote
    pub remote_mode: String,
    /// Path to agent binary on the target host
    pub binary_path: String,
    /// SSH config file path (for remote execution)
    pub ssh_config_path: String,
    /// SSH passphrase (for remote execution)
    pub ssh_passphrase: Option<String>,
    /// SSH password (for password authentication)
    pub ssh_password: Option<String>,
    /// Extra env vars injected into the agent process (e.g. gitea credentials
    /// for /git issue — project-level 需求接收 config, falling back to [git.issue]).
    pub gitea_env: Vec<(String, String)>,
}

/// Abstract interface for agent adapters.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Unique adapter identifier (e.g. "claude", "codewhale")
    fn adapter_id(&self) -> &str;
    async fn start_session(
        &self,
        config: &AgentConfig,
        ctx: &RuntimeContext,
        session_id: SessionId,
        claude_session_id: &str,
        session_key: &str,
        prompt: &str,
        working_dir: Option<String>,
    ) -> Result<SessionHandle>;
    async fn send_message(&self, session: &SessionHandle, message: &str) -> Result<()>;
    async fn handle_approval(
        &self,
        session: &SessionHandle,
        request_id: Option<&str>,
        response: ApprovalResponse,
    ) -> Result<()>;
    async fn stop_session(&self, session: &SessionHandle, force: bool) -> Result<()>;
    fn output_stream(&self, session: &SessionHandle) -> Box<dyn Stream<Item = AgentEvent> + Send + Unpin>;

    /// Check whether the adapter has an active subprocess for this session.
    fn has_process(&self, session_id: &SessionId) -> bool;

    /// 平台主动终止进程（stop/restore 回收/resume 热切换）前调用：adapter
    /// 应抑制随之而来的"进程异常退出"上报——自己杀的不算崩溃。
    fn suppress_crash_report(&self, _session_id: &SessionId) {}

    /// Restore a previously closed session. Default is no-op (not all agents support restore).
    /// `model` 是该会话持久化的模型（record.model），adapter 应在启动参数中注入，
    /// 保证恢复后的首轮请求就使用正确模型；None 时退回实例配置的 model。
    async fn restore_session(
        &self,
        _config: &AgentConfig,
        _ctx: &RuntimeContext,
        _session_id: SessionId,
        _claude_session_id: &str,
        _session_key: &str,
        _working_dir: Option<String>,
        _model: Option<&str>,
    ) -> Result<SessionHandle> {
        anyhow::bail!("restore not supported for this agent")
    }

    /// Resume a running session with updated config (kill + restart). Default is no-op.
    /// `model` 语义同 restore_session。
    async fn resume_session(
        &self,
        _config: &AgentConfig,
        _ctx: &RuntimeContext,
        _session_id: SessionId,
        _claude_session_id: &str,
        _session_key: &str,
        _working_dir: Option<String>,
        _model: Option<&str>,
    ) -> Result<SessionHandle> {
        anyhow::bail!("resume not supported for this agent")
    }

    /// Send an answer to an AskUserQuestion tool_use back to the agent process.
    /// `request_id` identifies the in-band control_request; `answers` maps each
    /// question text (full text, as sent by the CLI) to the chosen option label.
    async fn answer_question(
        &self,
        session: &SessionHandle,
        request_id: &str,
        answers: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        let _ = (session, request_id, answers);
        anyhow::bail!("answer_question not supported for this agent")
    }

    /// Abort the current turn without killing the session process. The user can
    /// then send a corrected message to start a fresh turn.
    async fn interrupt(&self, session: &SessionHandle) -> Result<()> {
        let _ = session;
        anyhow::bail!("interrupt not supported for this agent")
    }

    /// Hot-switch runtime config on a long-running session (smartcode console
    /// `set_config`). Fields None = unchanged; upstream confirms with a
    /// config_changed event.
    async fn set_config(
        &self,
        session: &SessionHandle,
        _model: Option<&str>,
        _base_url: Option<&str>,
        _api_key: Option<&str>,
        _provider: Option<&str>,
    ) -> Result<()> {
        let _ = session;
        anyhow::bail!("set_config not supported for this agent")
    }

    /// Delete the local session directory (e.g. ~/.claude/projects/<path>/<uuid>.jsonl).
    /// For Claude Code, this takes the working directory (to derive project path) plus session UUID.
    /// Default is no-op.
    async fn cleanup_session(&self, session_id: &str, working_dir: Option<&str>) {
        // Not all agents have local session data
        let _ = (session_id, working_dir);
    }

    /// Agent 进程自报的会话句柄（如 smartcode 在 stderr 上报告的会话 JSONL 路径）。
    /// 返回 Some 时平台在创建会话后用它覆盖预生成的 UUID 持久化到
    /// record.claude_session_id；restore/resume 时原样传回 adapter 作为句柄。
    /// 只对"会话标识由 agent 侧生成"的 agent 返回 Some。
    fn session_ref(&self, _session_id: &SessionId) -> Option<String> {
        None
    }
}

pub struct SessionHandle {
    pub id: SessionId,
    pub adapter_id: String,
    pub state: SessionHandleState,
}

pub enum SessionHandleState {
    Subprocess { pid: u32 },
    Http { base_url: String },
    Pty { pty_id: u32 },
}

impl SessionHandle {
    pub fn new(id: SessionId, adapter_id: String, state: SessionHandleState) -> Self {
        Self { id, adapter_id, state }
    }
}
