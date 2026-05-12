use std::sync::Arc;
use crate::engine::event_bus::EventBus;
use crate::engine::engine_event::EngineEvent;

/// Request context threaded through all engine operations.
/// Carries user identity, event bus, and collected events for batching.
///
/// Created at the HTTP handler or CLI command level, passed to every
/// engine method as `ctx: &RequestContext`.
#[derive(Clone)]
pub struct RequestContext {
    pub user_id: String,
    event_bus: Option<Arc<EventBus>>,
}

impl RequestContext {
    /// Default context for engine-internal operations and tests.
    /// No event bus, user_id = "system". Zero overhead.
    pub fn system() -> Self {
        RequestContext {
            user_id: "system".to_string(),
            event_bus: None,
        }
    }

    /// Context with event bus for background tasks / CLI tools.
    /// user_id = "system".
    pub fn with_bus(bus: Arc<EventBus>) -> Self {
        RequestContext {
            user_id: "system".to_string(),
            event_bus: Some(bus),
        }
    }

    /// Full context from HTTP request claims.
    pub fn from_claims(user_id: &str, bus: Arc<EventBus>) -> Self {
        RequestContext {
            user_id: user_id.to_string(),
            event_bus: Some(bus),
        }
    }

    /// Emit an event through the bus (if one exists).
    /// No-op if no event bus is configured (tests, system context).
    pub fn emit(&self, event_type: &str, payload: serde_json::Value) {
        if let Some(ref bus) = self.event_bus {
            let event = EngineEvent::new(event_type, &self.user_id, payload);
            bus.emit(event);
        }
    }

    /// Check if events are enabled (bus is present).
    pub fn events_enabled(&self) -> bool {
        self.event_bus.is_some()
    }

    /// Get a reference to the event bus (if present).
    pub fn event_bus(&self) -> Option<&Arc<EventBus>> {
        self.event_bus.as_ref()
    }
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("user_id", &self.user_id)
            .field("events_enabled", &self.events_enabled())
            .finish()
    }
}
