/// Stream-JSON protocol support layer.
///
/// This module provides:
/// - Inbound: parsing stream-json requests from HTTP body or stdin
/// - Outbound: converting AgentEvent to stream-json format for gateway consumption
///
/// Protocol format: one JSON object per line, fields:
///   type: "create_session" | "send_message" | "approve" | "stop_session"
///   session_id, content, decision, etc.

use serde::{Deserialize, Serialize};

/// Inbound stream-json commands (Gateway → Console)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamCommand {
    /// Create a new agent session
    CreateSession {
        agent_id: String,
        prompt: String,
    },
    /// Send a message to a running session
    SendMessage {
        session_id: String,
        content: String,
    },
    /// Respond to an approval request
    Approve {
        session_id: String,
        decision: String,
        comment: Option<String>,
    },
    /// Stop a running session
    StopSession {
        session_id: String,
    },
}

/// Outbound stream-json events (Console → Gateway)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Text output from agent
    Output {
        session_id: String,
        content: String,
        ts: u64,
    },
    /// Agent needs approval
    ApprovalRequest {
        session_id: String,
        prompt: String,
        options: Vec<String>,
        ts: u64,
    },
    /// Session completed
    SessionComplete {
        session_id: String,
        exit_code: i32,
        ts: u64,
    },
    /// Session status changed
    SessionStatus {
        session_id: String,
        status: String,
        ts: u64,
    },
    /// Error occurred
    Error {
        session_id: String,
        message: String,
        ts: u64,
    },
    /// Session created confirmation
    SessionCreated {
        session_id: String,
        agent_id: String,
        ts: u64,
    },
}

impl StreamEvent {
    /// Convert to JSON line
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Convert internal AgentEvent to StreamEvent
pub fn event_to_stream(event: &crate::agents::AgentEvent) -> Option<StreamEvent> {
    Some(match event {
        crate::agents::AgentEvent::Output { session_id, content, ts, raw: _ } => StreamEvent::Output {
            session_id: session_id.0.clone(),
            content: content.clone(),
            ts: *ts,
        },
        crate::agents::AgentEvent::ApprovalRequest { session_id, prompt, options, ts, .. } => {
            StreamEvent::ApprovalRequest {
                session_id: session_id.0.clone(),
                prompt: prompt.clone(),
                options: options.clone(),
                ts: *ts,
            }
        }
        crate::agents::AgentEvent::SessionComplete { session_id, exit_code, ts } => {
            StreamEvent::SessionComplete {
                session_id: session_id.0.clone(),
                exit_code: *exit_code,
                ts: *ts,
            }
        }
        crate::agents::AgentEvent::SessionStatus { session_id, status, ts } => {
            StreamEvent::SessionStatus {
                session_id: session_id.0.clone(),
                status: format!("{:?}", status),
                ts: *ts,
            }
        }
        crate::agents::AgentEvent::Error { session_id, message, ts } => StreamEvent::Error {
            session_id: session_id.0.clone(),
            message: message.clone(),
            ts: *ts,
        },
        crate::agents::AgentEvent::UserQuestion { session_id, .. } => StreamEvent::Output {
            session_id: session_id.0.clone(),
            content: String::new(),
            ts: 0,
        },
        crate::agents::AgentEvent::ApprovalResolved { .. } => {
            // Internal UI-clearing signal; stream-json consumers don't need it
            return None;
        }
        crate::agents::AgentEvent::GlobalNotification { .. } => {
            // Global notifications are not streamed to stream-json
            return None;
        }
        crate::agents::AgentEvent::ConfigChanged { .. } => {
            // Hot-switch receipt is WS-only; stream-json consumers don't need it
            return None;
        }
    })
}
