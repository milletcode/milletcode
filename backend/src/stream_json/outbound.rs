use axum::response::sse::{Event, Sse};
use futures::Stream;
use tokio::sync::broadcast;

use crate::events::EventBus;
use crate::stream_json::{event_to_stream, StreamEvent};

/// Convert an internal AgentEvent into an SSE event string
pub fn event_to_sse(event: &crate::agents::AgentEvent) -> String {
    let stream_event = event_to_stream(event);
    stream_event.to_json_line()
}
