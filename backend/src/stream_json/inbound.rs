use axum::response::sse::{Event, Sse};
use futures::Stream;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};

use crate::AppState;
use crate::stream_json::{StreamCommand, StreamEvent, event_to_stream};

/// SSE endpoint: GET /api/stream-json/:session_id
pub async fn subscribe_session_sse(
    State(state): State<Arc<AppState>>,
    Path(session_id_str): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = crate::agents::types::SessionId(session_id_str);
    let mut rx = state.event_bus.subscribe();

    let stream = async_stream::stream! {
        loop {
            if let Ok(event) = rx.recv().await {
                if event.session_id().map(|s| s.0.as_str()) == Some(session_id.0.as_str()) {
                    let stream_event = event_to_stream(&event);
                    let data = stream_event.to_json_line();
                    yield Ok(Event::default().data(data));
                }
            }
        }
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new())
}

/// HTTP SSE endpoint for streaming all events
pub async fn subscribe_all_sse(
    State(state): State<Arc<AppState>>,
    _query: Query<std::collections::HashMap<String, String>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.event_bus.subscribe();

    let stream = async_stream::stream! {
        loop {
            if let Ok(event) = rx.recv().await {
                let stream_event = event_to_stream(&event);
                let data = stream_event.to_json_line();
                yield Ok(Event::default().data(data));
            }
        }
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new())
}

/// POST /api/stream-json - Accepts a stream-json command
pub async fn handle_stream_command(
    State(state): State<Arc<AppState>>,
    axum::Json(cmd): axum::Json<StreamCommand>,
) -> impl axum::response::IntoResponse {
    let result = match cmd {
        StreamCommand::CreateSession { agent_id, prompt } => {
            match state.session_manager.create_session(None, agent_id.clone(), &prompt, "default").await {
                Ok((session_id, _key)) => {
                    let stream_event = StreamEvent::SessionCreated {
                        session_id: session_id.0.clone(),
                        agent_id,
                        ts: crate::agents::AgentEvent::now(),
                    };
                    (axum::http::StatusCode::CREATED, axum::Json(stream_event)).into_response()
                }
                Err(e) => {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({ "error": e.to_string() }))).into_response()
                }
            }
        }
        StreamCommand::SendMessage { session_id, content } => {
            let sid = crate::agents::types::SessionId(session_id);
            match state.session_manager.send_message(&sid, &content).await {
                Ok(()) => (axum::http::StatusCode::ACCEPTED, axum::Json(serde_json::json!({ "status": "sent" }))).into_response(),
                Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
            }
        }
        StreamCommand::Approve { session_id, decision, comment } => {
            let sid = crate::agents::types::SessionId(session_id);
            let response = crate::agents::types::ApprovalResponse { selected: decision, comment };
            match state.session_manager.handle_approval(&sid, None, response).await {
                Ok(()) => (axum::http::StatusCode::ACCEPTED, axum::Json(serde_json::json!({ "status": "approved" }))).into_response(),
                Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
            }
        }
        StreamCommand::StopSession { session_id } => {
            let sid = crate::agents::types::SessionId(session_id);
            match state.session_manager.stop_session(&sid, false).await {
                Ok(_) => (axum::http::StatusCode::ACCEPTED, axum::Json(serde_json::json!({ "status": "stopped" }))).into_response(),
                Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
            }
        }
    };

    result
}
