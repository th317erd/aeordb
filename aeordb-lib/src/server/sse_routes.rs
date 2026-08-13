use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Extension;
use futures_util::stream::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::once;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use super::state::AppState;
use crate::auth::permission_middleware::require_active_api_key;
use crate::auth::TokenClaims;
use crate::engine::api_key_rules::{check_operation_permitted, match_rules, KeyRule};
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{ApiKeyLoader, GroupLoader};
use crate::engine::engine_event::{
  EngineEvent, EVENT_ENTRIES_CREATED, EVENT_ENTRIES_DELETED, EVENT_ENTRIES_UPDATED, EVENT_GC_STATUS, EVENT_HEARTBEAT,
  EVENT_INDEXES_UPDATED, EVENT_PERMISSIONS_CHANGED, EVENT_SERVER_READY, EVENT_STREAM_GAP,
};
use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
use crate::engine::{StorageEngine, SystemFamilyPolicyResolver};
use crate::server::responses::{engine_error_response, ErrorResponse};
use crate::server::route_permissions::parse_user_id;

#[derive(Debug, Deserialize)]
pub struct SseParams {
  /// Comma-separated list of event types to receive (default: all).
  pub events: Option<String>,
  /// Only receive events whose payload entries match this path prefix.
  pub path_prefix: Option<String>,
}

fn parse_event_filter(events: Option<String>) -> Option<Vec<String>> {
  events.map(|e| e.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NonRootEventVisibility {
  Public,
  PathRequired,
  RootOnly,
}

fn non_root_event_visibility(event_type: &str) -> NonRootEventVisibility {
  match event_type {
    EVENT_SERVER_READY | EVENT_HEARTBEAT => NonRootEventVisibility::Public,
    EVENT_GC_STATUS => NonRootEventVisibility::RootOnly,
    EVENT_ENTRIES_CREATED | EVENT_ENTRIES_UPDATED | EVENT_ENTRIES_DELETED | EVENT_PERMISSIONS_CHANGED | EVENT_INDEXES_UPDATED => {
      NonRootEventVisibility::PathRequired
    }
    _ => NonRootEventVisibility::RootOnly,
  }
}

#[derive(Clone, Copy)]
enum SseSubscriberIdentity {
  User(Uuid),
  Share,
}

#[derive(Clone)]
struct SseSubscriberAuthority {
  identity: SseSubscriberIdentity,
  subject: String,
  key_id: Option<String>,
  engine: Arc<StorageEngine>,
  auth_engine: Arc<StorageEngine>,
  group_cache: Arc<Cache<GroupLoader>>,
  api_key_cache: Arc<Cache<ApiKeyLoader>>,
}

impl SseSubscriberAuthority {
  fn from_request(state: &AppState, claims: &TokenClaims) -> Result<Self, Response> {
    let identity = if claims.sub.starts_with("share:") {
      SseSubscriberIdentity::Share
    } else {
      SseSubscriberIdentity::User(parse_user_id(claims, "Invalid user identity")?)
    };

    if let Some(key_id) = claims.key_id.as_deref() {
      let record = require_active_api_key(state, key_id)?;
      if !Self::record_matches_identity(&identity, &claims.sub, &record) {
        tracing::warn!(sub = %claims.sub, key_id, "Rejected SSE subscriber with mismatched API-key identity");
        return Err(ErrorResponse::new("API key identity mismatch").with_status(StatusCode::FORBIDDEN).into_response());
      }
      if matches!(identity, SseSubscriberIdentity::Share) && record.rules.is_empty() {
        return Err(ErrorResponse::new("Share key has no permission rules").with_status(StatusCode::FORBIDDEN).into_response());
      }
    } else if matches!(identity, SseSubscriberIdentity::Share) {
      return Err(ErrorResponse::new("Share key has no permission rules").with_status(StatusCode::FORBIDDEN).into_response());
    }

    Ok(Self {
      identity,
      subject: claims.sub.clone(),
      key_id: claims.key_id.clone(),
      engine: Arc::clone(&state.engine),
      auth_engine: Arc::clone(&state.auth_engine),
      group_cache: Arc::clone(&state.group_cache),
      api_key_cache: Arc::clone(&state.api_key_cache),
    })
  }

  fn record_matches_identity(identity: &SseSubscriberIdentity, subject: &str, record: &crate::auth::api_key::ApiKeyRecord) -> bool {
    match identity {
      SseSubscriberIdentity::User(user_id) => record.user_id == Some(*user_id),
      SseSubscriberIdentity::Share => record.user_id.is_none() && subject == format!("share:{}", record.key_id),
    }
  }

  fn is_root(&self) -> bool {
    matches!(self.identity, SseSubscriberIdentity::User(user_id) if crate::engine::user::is_root(&user_id))
  }

  /// Return current key rules for one event. `None` means the keyed stream's
  /// authority disappeared or became invalid after it connected, so the event
  /// must be dropped. Direct JWT subscribers return an empty rule set.
  fn current_key_rules(&self) -> Option<Vec<KeyRule>> {
    let Some(key_id) = self.key_id.as_ref() else {
      return Some(Vec::new());
    };
    let record = match self.api_key_cache.get(key_id, &self.auth_engine) {
      Ok(Some(record)) => record,
      Ok(None) => {
        tracing::warn!(key_id, "Stopped SSE delivery because API-key authority no longer exists");
        return None;
      }
      Err(error) => {
        tracing::error!(key_id, %error, "Stopped SSE delivery after API-key authority lookup failed");
        return None;
      }
    };
    let now = chrono::Utc::now().timestamp_millis();
    if record.is_revoked || record.expires_at <= now || !Self::record_matches_identity(&self.identity, &self.subject, &record) {
      tracing::warn!(key_id, "Stopped SSE delivery because API-key authority is inactive or mismatched");
      return None;
    }
    if matches!(self.identity, SseSubscriberIdentity::Share) && record.rules.is_empty() {
      tracing::warn!(key_id, "Stopped SSE delivery because share-key rules are empty");
      return None;
    }
    Some(record.rules)
  }

  fn path_is_permitted(&self, path: &str, subscriber_rules: &[KeyRule]) -> bool {
    if !subscriber_rules.is_empty() {
      let permitted = match_rules(subscriber_rules, path).is_some_and(|rule| check_operation_permitted(&rule.permitted, 'r'));
      if !permitted {
        return false;
      }
    }

    match self.identity {
      SseSubscriberIdentity::Share => true,
      SseSubscriberIdentity::User(user_id) if crate::engine::user::is_root(&user_id) => true,
      SseSubscriberIdentity::User(user_id) => {
        let resolver = PermissionResolver::new(&self.engine, &self.group_cache);
        match resolver.check_permission(&user_id, path, CrudlifyOp::Read) {
          Ok(permitted) => permitted,
          Err(error) => {
            tracing::error!(%user_id, path, %error, "Refused SSE path after permission authority lookup failed");
            false
          }
        }
      }
    }
  }
}

fn path_is_visible_to_subscriber(
  path: &str,
  path_prefix: &Option<String>,
  subscriber_rules: &[KeyRule],
  authority: &SseSubscriberAuthority,
  family_policy: SystemFamilyPolicyResolver,
) -> bool {
  if path_prefix.as_ref().is_some_and(|prefix| !path.starts_with(prefix)) {
    return false;
  }
  if !authority.is_root() {
    match family_policy.generic_data_path_is_visible(path) {
      Ok(true) => {}
      Ok(false) => return false,
      Err(error) => {
        tracing::error!(path, error = %error, "Refused SSE path after SystemFamily classification failure");
        return false;
      }
    }
  }
  authority.path_is_permitted(path, subscriber_rules)
}

fn project_event_for_subscriber(
  mut event: EngineEvent,
  event_filter: &Option<Vec<String>>,
  path_prefix: &Option<String>,
  authority: &SseSubscriberAuthority,
  family_policy: SystemFamilyPolicyResolver,
) -> Option<EngineEvent> {
  // Recipient-addressed events belong exclusively to `/events/me`. They must
  // never also enter the global stream, even when the subscriber can read a
  // path carried by the payload.
  if event.recipient_user_id.is_some() {
    return None;
  }
  if event_filter.as_ref().is_some_and(|filter| !filter.contains(&event.event_type)) {
    return None;
  }
  let visibility = non_root_event_visibility(&event.event_type);
  if !authority.is_root() && visibility == NonRootEventVisibility::RootOnly {
    return None;
  }
  let subscriber_rules = authority.current_key_rules()?;

  let mut had_path = false;
  if let Some(Value::Array(entries)) = event.payload.get_mut("entries") {
    let original_had_entries = !entries.is_empty();
    entries.retain(|entry| {
      let Some(Value::String(path)) = entry.get("path") else {
        return authority.is_root() && subscriber_rules.is_empty() && path_prefix.is_none();
      };
      had_path = true;
      path_is_visible_to_subscriber(path, path_prefix, &subscriber_rules, authority, family_policy)
    });
    if original_had_entries && entries.is_empty() {
      return None;
    }
  }

  if let Some(Value::String(path)) = event.payload.get("path") {
    had_path = true;
    if !path_is_visible_to_subscriber(path, path_prefix, &subscriber_rules, authority, family_policy) {
      return None;
    }
  }

  if path_prefix.is_some() && !had_path {
    return None;
  }
  if !authority.is_root() && visibility == NonRootEventVisibility::PathRequired && !had_path {
    tracing::error!(event_type = %event.event_type, "Refused non-root SSE event whose path-required payload contained no path");
    return None;
  }
  Some(event)
}

fn event_to_sse(event: EngineEvent) -> Option<Result<Event, Infallible>> {
  match serde_json::to_string(&event) {
    Ok(json) => Some(Ok(Event::default().id(event.event_id.clone()).event(event.event_type.clone()).data(json))),
    Err(_) => None,
  }
}

fn server_ready_event(state: &AppState) -> EngineEvent {
  EngineEvent::new(
    EVENT_SERVER_READY,
    "system",
    serde_json::json!({
      "status": "ready",
      "version": env!("CARGO_PKG_VERSION"),
      "startup_time": state.startup_time,
      "uptime_ms": state.startup_instant.elapsed().as_millis() as u64,
    }),
  )
}

fn stream_gap_to_sse(error: BroadcastStreamRecvError) -> Option<Result<Event, Infallible>> {
  let BroadcastStreamRecvError::Lagged(missed_events) = error;
  metrics::counter!("aeordb_sse_stream_gaps_total").increment(1);
  metrics::counter!("aeordb_sse_missed_events_total").increment(missed_events);
  event_to_sse(EngineEvent::new(
    EVENT_STREAM_GAP,
    "system",
    serde_json::json!({
      "missed_events": missed_events,
      "action": "refresh",
    }),
  ))
}

/// GET /events/stream -- Server-Sent Events stream of engine events.
///
/// Query parameters:
///   - `events`      : comma-separated event type filter (e.g. `entries_created,entries_deleted`)
///   - `path_prefix` : only deliver events whose payload contains a path starting with this prefix
///
/// Permission filtering:
///   - Root users (nil UUID) bypass user/group path permissions; API-key rules
///     still constrain a scoped root key.
///   - Normal users receive path events only when current user/group authority
///     permits read access. User-owned API-key rules are an additional bound.
///   - Share keys use their active key rules as their sole path authority.
///   - Events with no path info (system/heartbeat) are delivered to active,
///     authenticated subscribers except for root-only administrative events.
pub async fn event_stream(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Query(params): Query<SseParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
  let authority = SseSubscriberAuthority::from_request(&state, &claims)?;
  let family_policy = SystemFamilyPolicyResolver::new(state.engine.hash_algo())
    .map_err(|error| engine_error_response("Cannot establish SSE path policy", &error))?;
  let rx = state.event_bus.subscribe();

  // Parse the comma-separated event type filter.
  let event_filter = parse_event_filter(params.events);

  let path_prefix = params.path_prefix;
  let ready_event = server_ready_event(&state);
  let initial_ready =
    project_event_for_subscriber(ready_event, &event_filter, &path_prefix, &authority, family_policy).and_then(event_to_sse);

  let live_stream = BroadcastStream::new(rx).filter_map(move |result| match result {
    Ok(event) => project_event_for_subscriber(event, &event_filter, &path_prefix, &authority, family_policy).and_then(event_to_sse),
    Err(error) => stream_gap_to_sse(error),
  });
  let stream = once(initial_ready).filter_map(|event| event).chain(live_stream);

  Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(30)).text("ping")))
}

/// GET /events/me — per-user SSE channel for events addressed to the
/// authenticated user. Used for things like file share notifications.
///
/// Authorization: requires a valid JWT. The route only delivers events
/// whose `recipient_user_id` matches the JWT's `sub` claim. Generic
/// events with no recipient (system/heartbeat/etc.) are NOT delivered
/// here — those go through /system/events.
///
/// This means the JWT proves identity AND scopes delivery to that user.
pub async fn user_event_stream(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
  let rx = state.event_bus.subscribe();
  let user_id = claims.sub.clone();

  let stream = BroadcastStream::new(rx).filter_map(move |result| {
    match result {
      Ok(event) => {
        // Only deliver events explicitly addressed to this user.
        let recipient = match &event.recipient_user_id {
          Some(r) => r,
          None => return None,
        };
        if recipient != &user_id {
          return None;
        }

        match serde_json::to_string(&event) {
          Ok(json) => Some(Ok(Event::default().id(event.event_id.clone()).event(event.event_type.clone()).data(json))),
          Err(_) => None,
        }
      }
      Err(error) => stream_gap_to_sse(error),
    }
  });

  Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(30)).text("ping"))
}
