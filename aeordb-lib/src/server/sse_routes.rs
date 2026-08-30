use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Extension;
use futures_util::stream::Stream;
use futures_util::stream::unfold;
use serde::Deserialize;
use serde_json::Value;
use tokio_stream::once;
use tokio_stream::StreamExt;
use uuid::Uuid;

use super::state::AppState;
use super::legacy_v3_root_adapter::LegacyV3SelectedRootAdapterV1;
use super::root_api::RequestedRootSelectorV1;
use super::root_public_schema::{PublicAffectedRelationshipChangeV1, PublicAffectedRelationshipV1};
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
use crate::engine::{ProjectedEvent, StorageEngine, SystemFamilyPolicyResolver};
use crate::server::responses::{engine_error_response, ErrorResponse, RouteResponseError};
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
  fn from_request(state: &AppState, claims: &TokenClaims) -> Result<Self, RouteResponseError> {
    let identity = if claims.sub.starts_with("share:") {
      SseSubscriberIdentity::Share
    } else {
      SseSubscriberIdentity::User(parse_user_id(claims, "Invalid user identity")?)
    };

    if let Some(key_id) = claims.key_id.as_deref() {
      let record = require_active_api_key(state, key_id)?;
      if !Self::record_matches_identity(&identity, &claims.sub, &record) {
        tracing::warn!(sub = %claims.sub, key_id, "Rejected SSE subscriber with mismatched API-key identity");
        return Err(ErrorResponse::new("API key identity mismatch").with_status(StatusCode::FORBIDDEN).into_response().into());
      }
      if matches!(identity, SseSubscriberIdentity::Share) && record.rules.is_empty() {
        return Err(ErrorResponse::new("Share key has no permission rules").with_status(StatusCode::FORBIDDEN).into_response().into());
      }
    } else if matches!(identity, SseSubscriberIdentity::Share) {
      return Err(ErrorResponse::new("Share key has no permission rules").with_status(StatusCode::FORBIDDEN).into_response().into());
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

  fn path_is_permitted(&self, path: &str) -> bool {
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
  if !path_satisfies_subscriber_outer_bounds(path, path_prefix, subscriber_rules, authority, family_policy) {
    return false;
  }
  authority.path_is_permitted(path)
}

fn path_satisfies_subscriber_outer_bounds(
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
  subscriber_rules.is_empty() || match_rules(subscriber_rules, path).is_some_and(|rule| check_operation_permitted(&rule.permitted, 'r'))
}

fn previous_root_authority_for_event<'authority>(
  event: &EngineEvent,
  authority: &'authority SseSubscriberAuthority,
) -> Option<(LegacyV3SelectedRootAdapterV1<'authority>, Vec<String>)> {
  let SseSubscriberIdentity::User(user_id) = authority.identity else {
    return None;
  };
  if crate::engine::user::is_root(&user_id) {
    return None;
  }
  let Some(Value::String(previous_root_hash)) = event.payload.get("previous_root_hash") else {
    tracing::error!(event_type = %event.event_type, "Refused prior-audience SSE projection without a previous root hash");
    return None;
  };
  let expected_hexadecimal_length = authority.engine.hash_algo().hash_length().checked_mul(2)?;
  if previous_root_hash.len() != expected_hexadecimal_length
    || !previous_root_hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
  {
    tracing::error!(event_type = %event.event_type, "Refused prior-audience SSE projection with a non-canonical previous root hash");
    return None;
  }
  let previous_root = match hex::decode(previous_root_hash) {
    Ok(previous_root) if previous_root.iter().any(|byte| *byte != 0) => previous_root,
    Ok(_) => {
      tracing::error!(event_type = %event.event_type, "Refused prior-audience SSE projection with an invalid previous root hash");
      return None;
    }
    Err(error) => {
      tracing::error!(event_type = %event.event_type, %error, "Refused prior-audience SSE projection with a malformed previous root hash");
      return None;
    }
  };
  let selected = match LegacyV3SelectedRootAdapterV1::resolve(
    authority.engine.as_ref(),
    &RequestedRootSelectorV1::ExplicitRoot(previous_root),
  ) {
    Ok(selected) => selected,
    Err(error) => {
      tracing::error!(event_type = %event.event_type, %error, "Refused prior-audience SSE projection because the previous root was unavailable");
      return None;
    }
  };
  let current_groups = match authority.group_cache.get(&user_id, &authority.engine) {
    Ok(groups) => groups,
    Err(error) => {
      tracing::error!(event_type = %event.event_type, %user_id, %error, "Refused prior-audience SSE projection after group authority lookup failed");
      return None;
    }
  };
  Some((selected, current_groups))
}

fn previous_root_authorizes_path(
  selected: &LegacyV3SelectedRootAdapterV1<'_>,
  current_groups: &[String],
  path: &str,
  event_type: &str,
) -> bool {
  match selected.authorize_path(path, CrudlifyOp::Read, current_groups) {
    Ok(Some(_)) => true,
    Ok(None) => false,
    Err(error) => {
      tracing::error!(event_type, path, %error, "Refused prior-audience SSE path after immutable permission lookup failed");
      false
    }
  }
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
  if event.is_recipient_addressed() {
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
  if event.payload.get("entries").is_some_and(|entries| !entries.is_array()) {
    tracing::error!(event_type = %event.event_type, "Refused SSE mutation event whose entries were not an array");
    return None;
  }
  if event.payload.get("affected_relationships").is_some_and(|relationships| !relationships.is_array()) {
    tracing::error!(event_type = %event.event_type, "Refused SSE mutation event whose affected relationships were not an array");
    return None;
  }

  let raw_relationships = event.payload.get("affected_relationships").and_then(Value::as_array).cloned();
  let mut relationship_visibility = std::collections::HashMap::new();
  let mut projected_relationships = Vec::new();
  let mut previous_root_authority = None;
  let mut previous_root_authority_loaded = false;
  if let Some(relationships) = raw_relationships.as_ref() {
    projected_relationships.reserve(relationships.len());
    for raw_relationship in relationships {
      let relationship = match serde_json::from_value::<PublicAffectedRelationshipV1>(raw_relationship.clone()) {
        Ok(relationship) => relationship,
        Err(error) => {
          tracing::error!(event_type = %event.event_type, %error, "Refused SSE mutation event with malformed affected relationship");
          return None;
        }
      };
      let path = &relationship.path;
      had_path = true;
      let currently_visible = path_is_visible_to_subscriber(path, path_prefix, &subscriber_rules, authority, family_policy);
      let visible = if currently_visible {
        true
      } else if relationship.change == PublicAffectedRelationshipChangeV1::Deleted {
        if !previous_root_authority_loaded {
          previous_root_authority = previous_root_authority_for_event(&event, authority);
          previous_root_authority_loaded = true;
        }
        previous_root_authority.as_ref().is_some_and(|(selected, current_groups)| {
          // Current API-key rules, path-prefix selection, and SystemFamily
          // visibility remain outer bounds even when permission documents are
          // evaluated at the exact previous root.
          if !path_satisfies_subscriber_outer_bounds(path, path_prefix, &subscriber_rules, authority, family_policy) {
            return false;
          }
          previous_root_authorizes_path(selected, current_groups, path, &event.event_type)
        })
      } else {
        false
      };
      if relationship_visibility.insert(path.clone(), visible).is_some() {
        tracing::error!(event_type = %event.event_type, path, "Refused SSE mutation event with duplicate affected relationships");
        return None;
      }
      if visible {
        let projected = match serde_json::to_value(relationship) {
          Ok(projected) => projected,
          Err(error) => {
            tracing::error!(event_type = %event.event_type, %error, "Refused SSE mutation event whose affected relationship could not be serialized");
            return None;
          }
        };
        projected_relationships.push(projected);
      }
    }
    if !relationships.is_empty() && projected_relationships.is_empty() {
      return None;
    }
    event.payload["affected_relationships"] = Value::Array(projected_relationships);
  }

  if let Some(Value::Array(entries)) = event.payload.get_mut("entries") {
    let original_had_entries = !entries.is_empty();
    entries.retain(|entry| {
      let Some(Value::String(path)) = entry.get("path") else {
        return authority.is_root() && subscriber_rules.is_empty() && path_prefix.is_none();
      };
      had_path = true;
      if raw_relationships.is_some() {
        return match relationship_visibility.get(path) {
          Some(visible) => *visible,
          None => false,
        };
      }
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

fn stream_gap_to_sse(missed_events: u64, disclose_missed_event_count: bool) -> Option<Result<Event, Infallible>> {
  metrics::counter!("aeordb_sse_stream_gaps_total").increment(1);
  metrics::counter!("aeordb_sse_missed_events_total").increment(missed_events);
  let payload = if disclose_missed_event_count {
    serde_json::json!({"missed_events": missed_events, "action": "refresh"})
  } else {
    serde_json::json!({"action": "refresh"})
  };
  event_to_sse(EngineEvent::new(EVENT_STREAM_GAP, "system", payload))
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
  let authority = SseSubscriberAuthority::from_request(&state, &claims).map_err(RouteResponseError::into_response)?;
  let family_policy = SystemFamilyPolicyResolver::new(state.engine.hash_algo())
    .map_err(|error| engine_error_response("Cannot establish SSE path policy", &error))?;

  // Parse the comma-separated event type filter.
  let event_filter = parse_event_filter(params.events);

  let path_prefix = params.path_prefix;
  let disclose_missed_event_count = authority.is_root() && authority.key_id.is_none();
  let ready_event = server_ready_event(&state);
  let initial_ready =
    project_event_for_subscriber(ready_event, &event_filter, &path_prefix, &authority, family_policy).and_then(event_to_sse);
  let projected_event_filter = event_filter.clone();
  let projected_path_prefix = path_prefix.clone();
  let projected_authority = authority.clone();
  let receiver = state
    .event_bus
    .subscribe_projected(move |event| {
      project_event_for_subscriber(event.clone(), &projected_event_filter, &projected_path_prefix, &projected_authority, family_policy)
    })
    .map_err(|error| ErrorResponse::new(error.to_string()).with_status(StatusCode::SERVICE_UNAVAILABLE).into_response())?;
  let live_stream = unfold(receiver, |mut receiver| async move { receiver.receive().await.map(|delivery| (delivery, receiver)) })
    .filter_map(move |delivery| match delivery {
      ProjectedEvent::Event(event) => event_to_sse(event),
      ProjectedEvent::Gap { missed_events } => stream_gap_to_sse(missed_events, disclose_missed_event_count),
    });
  let stream = once(initial_ready).filter_map(|event| event).chain(live_stream);

  Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(30)).text("ping")))
}

/// GET /events/me — per-user SSE channel for events addressed to the
/// authenticated user. Used for things like file share notifications.
///
/// Authorization: requires a valid user JWT. Direct recipients must match the
/// JWT subject; group recipients require current membership. Active API-key
/// rules and SystemFamily concealment remain outer bounds for path notices.
/// Generic events with no recipient are not delivered here.
pub async fn user_event_stream(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
  let authority = SseSubscriberAuthority::from_request(&state, &claims).map_err(RouteResponseError::into_response)?;
  let SseSubscriberIdentity::User(user_id) = authority.identity else {
    return Err(ErrorResponse::new("Per-user events require a user identity").with_status(StatusCode::FORBIDDEN).into_response());
  };
  let family_policy = SystemFamilyPolicyResolver::new(state.engine.hash_algo())
    .map_err(|error| engine_error_response("Cannot establish recipient-event path policy", &error))?;
  let user_subject = claims.sub.clone();
  let projected_authority = authority.clone();
  let receiver = state
    .event_bus
    .subscribe_projected(move |event| {
      let current_key_rules = projected_authority.current_key_rules()?;
      if let Some(path) = event.payload.get("path").and_then(Value::as_str) {
        if !path_satisfies_subscriber_outer_bounds(path, &None, &current_key_rules, &projected_authority, family_policy) {
          return None;
        }
      } else if !current_key_rules.is_empty() {
        tracing::error!(event_type = %event.event_type, "Refused scoped recipient SSE event without an authorizable path");
        return None;
      }

      if event.recipient_user_id.as_ref() == Some(&user_subject) {
        return Some(event.clone());
      }
      let recipient_groups = event.recipient_groups()?;
      let current_groups = match projected_authority.group_cache.get(&user_id, &projected_authority.engine) {
        Ok(groups) => groups,
        Err(error) => {
          tracing::error!(%user_id, %error, "Refused group-addressed SSE event after membership authority lookup failed");
          return None;
        }
      };
      if !recipient_groups.iter().any(|recipient_group| current_groups.contains(recipient_group)) {
        return None;
      }
      Some(event.clone())
    })
    .map_err(|error| ErrorResponse::new(error.to_string()).with_status(StatusCode::SERVICE_UNAVAILABLE).into_response())?;
  let stream = unfold(receiver, |mut receiver| async move { receiver.receive().await.map(|delivery| (delivery, receiver)) }).filter_map(
    move |delivery| match delivery {
      ProjectedEvent::Event(event) => event_to_sse(event),
      ProjectedEvent::Gap { missed_events } => stream_gap_to_sse(missed_events, false),
    },
  );

  Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(30)).text("ping")))
}
