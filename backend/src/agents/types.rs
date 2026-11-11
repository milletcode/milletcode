use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::projects::types::ProjectPath;

/// Session identifier — wraps composite key "org/project/agent-name/session-name"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Create from composite key (no longer generates UUID)
    pub fn new(composite_key: String) -> Self {
        Self(composite_key)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self(String::new())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Agent session status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    WaitingApproval,
    Stopped,
    Complete,
    Error,
    Restarting,
}

/// Response from user for an approval request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub selected: String,
    pub comment: Option<String>,
}

/// Internal events emitted by agents
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Output {
        session_id: SessionId,
        content: String,
        ts: u64,
        /// Original JSONL line — preserves all fields from Claude CLI (uuid, parentUuid,
        /// userType, model, usage, stop_reason, sessionId, version, gitBranch, cwd, etc.)
        /// Frontend can use these for thread rendering, display metadata, etc.
        #[serde(skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
    },
    ApprovalRequest {
        session_id: SessionId,
        prompt: String,
        options: Vec<String>,
        /// In-band control protocol request id (control_request). None for legacy
        /// heuristic paths that don't participate in the control_response round trip.
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        ts: u64,
    },
    /// A pending control request was resolved (approved/denied/answered) — lets
    /// observer clients clear their approval UI and re-enable the composer.
    ApprovalResolved {
        session_id: SessionId,
        ts: u64,
    },
    SessionComplete {
        session_id: SessionId,
        exit_code: i32,
        ts: u64,
    },
    SessionStatus {
        session_id: SessionId,
        status: SessionStatus,
        ts: u64,
    },
    Error {
        session_id: SessionId,
        message: String,
        ts: u64,
    },
    UserQuestion {
        session_id: SessionId,
        tool_use_id: String,
        /// In-band control protocol request id — the answer must be written back
        /// as a control_response referencing this id.
        request_id: String,
        questions: Vec<UserQuestionItem>,
        ts: u64,
    },
    GlobalNotification {
        level: String,
        message: String,
        ts: u64,
    },
    /// smartcode console 热切换配置已生效（set_config 的上游回执，api_key 永不回传）。
    /// 字段为 None 表示该项未变化。
    ConfigChanged {
        session_id: SessionId,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        ts: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestionItem {
    pub question: String,
    pub header: String,
    pub options: Vec<UserQuestionOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestionOption {
    pub label: String,
    pub description: String,
}

impl AgentEvent {
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            AgentEvent::Output { session_id, .. }
            | AgentEvent::ApprovalRequest { session_id, .. }
            | AgentEvent::ApprovalResolved { session_id, .. }
            | AgentEvent::SessionComplete { session_id, .. }
            | AgentEvent::SessionStatus { session_id, .. }
            | AgentEvent::Error { session_id, .. }
            | AgentEvent::UserQuestion { session_id, .. }
            | AgentEvent::ConfigChanged { session_id, .. } => Some(session_id),
            AgentEvent::GlobalNotification { .. } => None,
        }
    }
}

/// Metadata for a running session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Composite key: "org/project/agent-name/session-name"
    pub id: SessionId,
    /// UUID for Claude CLI (used for --session-id/--resume, disk paths, hook URLs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    pub project_path: Option<ProjectPath>,
    pub agent_id: String,
    pub agent_name: String,
    pub status: SessionStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub prompt: String,
}

/// One entry of the /api/sessions/idle response. Deliberately free of
/// pid/process details — only the composite id
/// ("keyword/project/agent/session") and how long it has been idle. The
/// frontend composes the notification link from its own origin: production
/// domains are dynamic, so no static server-side URL can be authoritative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleSessionInfo {
    pub id: String,
    pub idle_seconds: u64,
}
