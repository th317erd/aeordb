use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use super::state::AppState;
use crate::engine::engine_event::EngineEvent;

#[derive(Debug, Deserialize)]
pub struct SseParams {
    /// Comma-separated list of event types to receive (default: all).
    pub events: Option<String>,
    /// Only receive events whose payload entries match this path prefix.
    pub path_prefix: Option<String>,
}

/// Check whether an event matches the given path prefix filter.
///
/// Looks at `payload.entries[].path` first (batch entry events), then falls
/// back to `payload.path` (single-path events like permissions/indexes).
fn matches_path_prefix(event: &EngineEvent, prefix: &str) -> bool {
    if let Some(Value::Array(entries)) = event.payload.get("entries") {
        return entries.iter().any(|entry| {
            if let Some(Value::String(path)) = entry.get("path") {
                path.starts_with(prefix)
            } else {
                false
            }
        });
    }

    if let Some(Value::String(path)) = event.payload.get("path") {
        return path.starts_with(prefix);
    }

    false
}

/// GET /events/stream -- Server-Sent Events stream of engine events.
///
/// Query parameters:
///   - `events`      : comma-separated event type filter (e.g. `entries_created,entries_deleted`)
///   - `path_prefix` : only deliver events whose payload contains a path starting with this prefix
pub async fn event_stream(
    State(state): State<AppState>,
    Query(params): Query<SseParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_bus.subscribe();

    // Parse the comma-separated event type filter.
    let event_filter: Option<Vec<String>> = params.events.map(|e| {
        e.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let path_prefix = params.path_prefix;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                // Apply event type filter.
                if let Some(ref filter) = event_filter {
                    if !filter.contains(&event.event_type) {
                        return None;
                    }
                }

                // Apply path prefix filter.
                if let Some(ref prefix) = path_prefix {
                    if !matches_path_prefix(&event, prefix) {
                        return None;
                    }
                }

                // Serialize the full event envelope as JSON data.
                match serde_json::to_string(&event) {
                    Ok(json) => Some(Ok(Event::default()
                        .id(event.event_id.clone())
                        .event(event.event_type.clone())
                        .data(json))),
                    Err(_) => None,
                }
            }
            Err(_) => None, // Lagged or closed -- silently skip
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(30))
            .text("ping"),
    )
}
