use tokio::sync::broadcast;
use crate::engine::engine_event::EngineEvent;

const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Central event bus for distributing engine events to subscribers.
/// Uses tokio::broadcast for fire-and-forget delivery.
pub struct EventBus {
    sender: broadcast::Sender<EngineEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        EventBus { sender }
    }

    /// Emit an event to all subscribers. Fire-and-forget — returns immediately.
    /// If no subscribers exist, the event is silently dropped.
    pub fn emit(&self, event: EngineEvent) {
        let _ = self.sender.send(event); // ignore error (no receivers)
    }

    /// Subscribe to events. Returns a broadcast Receiver.
    /// If the subscriber falls behind, it receives Lagged(n).
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.sender.subscribe()
    }

    /// Get the current number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("subscriber_count", &self.subscriber_count())
            .finish()
    }
}
