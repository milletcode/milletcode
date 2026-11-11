use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use tokio::sync::Mutex;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;

use crate::agents::{AgentAdapter, AgentEvent, RuntimeContext, SessionHandle, SessionHandleState};
use crate::agents::types::{ApprovalResponse, SessionId, SessionStatus};
use crate::config::{AgentConfig};

pub struct DeepSeekAdapter {
    event_bus: std::sync::Arc<crate::events::EventBus>,
    sessions: parking_lot::Mutex<std::collections::HashMap<SessionId, DeepSeekSessionState>>,
}

impl DeepSeekAdapter {
    pub fn new(event_bus: std::sync::Arc<crate::events::EventBus>) -> Self {
        Self {
            event_bus,
            sessions: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn find_port() -> u16 {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind to port");
        listener.local_addr().unwrap().port()
    }
}

#[async_trait]
impl AgentAdapter for DeepSeekAdapter {
    fn adapter_id(&self) -> &str {
        "codewhale"
    }

    async fn start_session(
        &self,
        config: &AgentConfig,
        _ctx: &RuntimeContext,
        session_id: SessionId,
        _claude_session_id: &str,
        _session_key: &str,
        prompt: &str,
        working_dir: Option<String>,
    ) -> Result<SessionHandle> {
        tracing::info!(session = %session_id, working_dir = ?working_dir, "Starting DeepSeek-TUI session");

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Starting,
            ts: AgentEvent::now(),
        });

        let port = Self::find_port();
        let binary = if config.extra_args.is_empty() {
            "deepseek"
        } else {
            // Use first extra_arg as binary override, else default
            "deepseek"
        };

        let mut cmd = tokio::process::Command::new(binary);
        cmd.arg("serve")
            .arg("--http")
            .arg("--port")
            .arg(port.to_string());

        if config.auto_approve {
            cmd.arg("--yolo");
        }

        if let Some(ref api_key) = config.api_key {
            cmd.env("DEEPSEEK_API_KEY", api_key);
        }

        if let Some(ref dir) = working_dir {
            cmd.current_dir(dir);
        }

        for arg in &config.extra_args {
            cmd.arg(arg);
        }

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn deepseek process: {}. Is 'deepseek' installed?", e)
        })?;

        let pid = child.id().unwrap_or(0) as u32;
        let base_url = format!("http://127.0.0.1:{}", port);

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let (tx, rx) = mpsc::channel::<AgentEvent>(128);
        let cancel = CancellationToken::new();

        let event_bus = self.event_bus.clone();
        let cancel_clone = cancel.clone();
        let session_clone = session_id.clone();
        let tx_clone = tx.clone();
        let base_url_clone = base_url.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let sse_url = format!("{}/api/events", base_url_clone);
            let response = client
                .get(&sse_url)
                .header("Accept", "text/event-stream")
                .send()
                .await;

            if let Ok(resp) = response {
                if resp.status().is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    for line in text.lines() {
                        if cancel_clone.is_cancelled() {
                            break;
                        }
                        if !line.starts_with("data:") {
                            continue;
                        }
                        let data = line.trim_start_matches("data:");
                        if let Some(event) = parse_deepseek_event(data, &session_clone) {
                            let _ = tx_clone.send(event.clone()).await;
                            event_bus.emit(event);
                        }
                    }
                }
            }

            event_bus.emit(AgentEvent::SessionComplete {
                session_id: session_clone,
                exit_code: 0,
                ts: AgentEvent::now(),
            });
        });

        let child = child;
        // Child is stored in state; will be killed on stop_session

        let base_url_for_handle = base_url.clone();

        let client = reqwest::Client::new();
        let _ = client
            .post(format!("{}/api/sessions", base_url))
            .json(&serde_json::json!({
                "prompt": prompt,
                "id": session_id.0
            }))
            .send()
            .await;

        let state = DeepSeekSessionState {
            tx,
            rx: parking_lot::Mutex::new(Some(rx)),
            cancel,
            base_url: base_url.clone(),
            pid,
            child: tokio::sync::Mutex::new(Some(child)),
        };
        self.sessions.lock().insert(session_id.clone(), state);

        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session_id.clone(),
            status: SessionStatus::Running,
            ts: AgentEvent::now(),
        });

        let handle_state = SessionHandleState::Http { base_url: base_url_for_handle };
        Ok(SessionHandle::new(session_id, "codewhale".to_string(), handle_state))
    }

    async fn send_message(&self, session: &SessionHandle, message: &str) -> Result<()> {
        let base_url = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            state.base_url.clone()
        };

        let client = reqwest::Client::new();
        let _ = client
            .post(format!("{}/api/sessions/{}/messages", base_url, session.id.0))
            .json(&serde_json::json!({ "content": message }))
            .send()
            .await;

        Ok(())
    }

    async fn handle_approval(
        &self,
        session: &SessionHandle,
        _request_id: Option<&str>,
        response: ApprovalResponse,
    ) -> Result<()> {
        let base_url = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session.id))?;
            state.base_url.clone()
        };

        let approve = matches!(
            response.selected.to_lowercase().as_str(),
            "approve" | "allow" | "yes" | "y"
        );

        let client = reqwest::Client::new();
        let _ = client
            .post(format!("{}/api/sessions/{}/approval", base_url, session.id.0))
            .json(&serde_json::json!({
                "decision": if approve { "approve" } else { "deny" }
            }))
            .send()
            .await;

        Ok(())
    }

    async fn stop_session(&self, session: &SessionHandle, _force: bool) -> Result<()> {
        self.event_bus.emit(AgentEvent::SessionStatus {
            session_id: session.id.clone(),
            status: SessionStatus::Stopped,
            ts: AgentEvent::now(),
        });

        let state = self.sessions.lock().remove(&session.id);
        if let Some(s) = state {
            if let Some(mut child) = s.child.lock().await.take() {
                let _ = child.kill().await;
            }
        }

        Ok(())
    }

    fn has_process(&self, session_id: &SessionId) -> bool {
        self.sessions.lock().contains_key(session_id)
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
}

fn parse_deepseek_event(data: &str, session_id: &SessionId) -> Option<AgentEvent> {
    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => {
            return Some(AgentEvent::Output {
                session_id: session_id.clone(),
                content: data.to_string(),
                ts: AgentEvent::now(),
                raw: None,
            });
        }
    };

    let event_type = value
        .get("event")
        .or_else(|| value.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("message");

    match event_type {
        "tool_call" | "tool_use" => {
            let tool_name = value
                .get("tool")
                .or_else(|| value.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let args = value
                .get("input")
                .or_else(|| value.get("arguments"))
                .map(|v| v.to_string())
                .unwrap_or_default();

            Some(AgentEvent::ApprovalRequest {
                session_id: session_id.clone(),
                prompt: format!("{} {}", tool_name, args),
                options: vec!["approve".into(), "deny".into()],
                request_id: None,
                tool_name: None,
                ts: AgentEvent::now(),
            })
        }
        "message" | "content" | "chunk" => {
            let content = value
                .get("content")
                .or_else(|| value.get("text"))
                .or_else(|| value.get("delta"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| data.to_string());

            Some(AgentEvent::Output {
                session_id: session_id.clone(),
                content,
                ts: AgentEvent::now(),
                raw: Some(value.clone()),
            })
        }
        "complete" | "done" | "finished" => Some(AgentEvent::SessionComplete {
            session_id: session_id.clone(),
            exit_code: 0,
            ts: AgentEvent::now(),
        }),
        "error" => Some(AgentEvent::Error {
            session_id: session_id.clone(),
            message: data.to_string(),
            ts: AgentEvent::now(),
        }),
        _ => Some(AgentEvent::Output {
            session_id: session_id.clone(),
            content: data.to_string(),
            ts: AgentEvent::now(),
            raw: Some(value.clone()),
        }),
    }
}

struct DeepSeekSessionState {
    tx: mpsc::Sender<AgentEvent>,
    rx: parking_lot::Mutex<Option<mpsc::Receiver<AgentEvent>>>,
    cancel: CancellationToken,
    base_url: String,
    pid: u32,
    child: Mutex<Option<Child>>,
}
