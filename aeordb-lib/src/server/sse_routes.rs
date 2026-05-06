use std::convert::Infallible;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Extension;
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::api_key_rules::{match_rules, KeyRule};
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

/// Extract all path strings from an event payload.
///
/// Returns paths from `payload.entries[].path` (batch events) and/or
/// `payload.path` (single-path events like permissions/indexes).
/// Returns an empty vec for events with no path information (e.g. heartbeat).
fn extract_event_paths(event: &EngineEvent) -> Vec<String> {
    let mut paths = Vec::new();

    if let Some(Value::Array(entries)) = event.payload.get("entries") {
        for entry in entries {
            if let Some(Value::String(path)) = entry.get("path") {
                paths.push(path.clone());
            }
        }
    }

    if let Some(Value::String(path)) = event.payload.get("path") {
        paths.push(path.clone());
    }

    paths
}

/// Check whether any of the given paths are allowed by the subscriber's key rules.
///
/// A path is allowed if `match_rules` finds a matching rule with the read flag
/// set (position 1 != '-'). If no rule matches, the path is denied.
fn any_path_allowed_by_rules(paths: &[String], rules: &[KeyRule]) -> bool {
    paths.iter().any(|path| {
        match match_rules(rules, path) {
            Some(rule) => {
                // Check if the read flag (position 1) is set
                rule.permitted.chars().nth(1).map(|ch| ch != '-').unwrap_or(false)
            }
            None => false,
        }
    })
}

/// GET /events/stream -- Server-Sent Events stream of engine events.
///
/// Query parameters:
///   - `events`      : comma-separated event type filter (e.g. `entries_created,entries_deleted`)
///   - `path_prefix` : only deliver events whose payload contains a path starting with this prefix
///
/// Permission filtering:
///   - Root users (nil UUID) receive all events unfiltered.
///   - Non-root users with API key rules only receive events whose paths are
///     readable under those rules. Events with no path info (system/heartbeat)
///     are delivered to all authenticated subscribers.
pub async fn event_stream(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Query(params): Query<SseParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_bus.subscribe();

    // Determine the subscriber's permission scope.
    let is_root = Uuid::parse_str(&claims.sub)
        .map(|uid| crate::engine::user::is_root(&uid))
        .unwrap_or(false);

    // Load API key rules for scoped subscribers.
    let subscriber_rules: Vec<KeyRule> = if is_root {
        vec![] // Root gets everything
    } else if let Some(ref key_id) = claims.key_id {
        match state.api_key_cache.get(&key_id.to_string(), &state.engine) {
            Ok(Some(record)) if !record.is_revoked && !record.rules.is_empty() => {
                record.rules.clone()
            }
            _ => vec![],
        }
    } else {
        vec![] // No key_id = direct user auth, no key-level restrictions
    };

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

                // Apply permission-based filtering for non-root users with key rules.
                if !subscriber_rules.is_empty() {
                    let paths = extract_event_paths(&event);
                    // Events with no path info (heartbeat, metrics, etc.) pass through.
                    // Events with paths are filtered: at least one path must be readable.
                    if !paths.is_empty() && !any_path_allowed_by_rules(&paths, &subscriber_rules) {
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
