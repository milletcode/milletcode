use tokio::sync::broadcast;
use tracing;

use crate::agents::AgentEvent;

/// Internal event bus for decoupling agent output from consumers (WebSocket, stream-json).
/// Uses tokio broadcast channel so multiple subscribers can listen.
pub struct EventBus {
    tx: broadcast::Sender<AgentEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Emit an event to all subscribers
    pub fn emit(&self, event: AgentEvent) {
        tracing::trace!("Event emitted: {:?}", event);
        // Send errors (lag) are expected when buffer is full; we drop the event
        let _ = self.tx.send(event);
    }

    /// Subscribe to the event stream
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }
}
