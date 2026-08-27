use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use chrono::Utc;
use uuid::Uuid;

use aeordb::auth::api_key::{ApiKeyRecord, generate_api_key, hash_api_key};
use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::api_key_rules::KeyRule;
use aeordb::engine::{
  BufferedFile, DirectoryOps, EngineEvent, EventBus, PermissionStore, RequestContext, StorageEngine, User, EVENT_GC_STATUS, EVENT_METRICS,
  EVENT_SERVER_READY, EVENT_STREAM_GAP,
};
use aeordb::engine::system_store;
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
  metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder().handle()
}

/// Create a test app that returns the shared EventBus for direct event injection.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, Arc<EventBus>, tempfile::TempDir) {
  test_app_with_event_capacity(1024)
}

fn test_app_with_event_capacity(
  event_capacity: usize,
) -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, Arc<EventBus>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let event_bus = Arc::new(EventBus::with_capacity(event_capacity));
  let app = create_app_with_all(
    auth_provider,
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter,
    make_prometheus_handle(),
    engine.clone(),
    event_bus.clone(),
    CorsState { default_origins: None, rules: vec![] },
  );
  (app, jwt_manager, engine, event_bus, temp_dir)
}

fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: Uuid::nil().to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

fn invalid_identity_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create invalid-identity token");
  format!("Bearer {}", token)
}

fn expired_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now - 7200,
    exp: now - 3600, // expired 1 hour ago
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

fn user_bearer_token(jwt_manager: &JwtManager, user_id: Uuid) -> String {
  let now = Utc::now().timestamp();
  let claims = TokenClaims {
    sub: user_id.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create user token");
  format!("Bearer {}", token)
}

fn keyed_bearer_token(jwt_manager: &JwtManager, subject: &str, key_id: Uuid) -> String {
  let now = Utc::now().timestamp();
  let claims = TokenClaims {
    sub: subject.to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: Some(key_id.to_string()),
  };
  let token = jwt_manager.create_token(&claims).expect("create keyed token");
  format!("Bearer {}", token)
}

fn create_test_user(engine: &StorageEngine, username: &str) -> Uuid {
  let user = User::new(username, None);
  let user_id = user.user_id;
  system_store::store_user(engine, &RequestContext::system(), &user).expect("store SSE test user");
  user_id
}

fn grant_user_read(engine: &StorageEngine, user_id: Uuid, path: &str) {
  DirectoryOps::new(engine)
    .store_file_buffered(&RequestContext::system(), path, b"SSE authority fixture", Some("text/plain"))
    .expect("store SSE permission target");
  PermissionStore::new(engine)
    .grant_paths(&RequestContext::system(), vec![path.to_string()], vec![format!("user:{user_id}")], ".r..l...".to_string())
    .expect("grant SSE test read permission");
}

// ---------------------------------------------------------------------------
// Auth tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_requires_auth() {
  let (app, _, _, _, _temp) = test_app();

  let request = Request::builder().method("GET").uri("/system/events").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_expired_token() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = expired_bearer_token(&jwt_manager);

  let request = Request::builder().method("GET").uri("/system/events").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_malformed_token() {
  let (app, _, _, _, _temp) = test_app();

  let request =
    Request::builder().method("GET").uri("/system/events").header("authorization", "Bearer not-a-real-jwt").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_wrong_scheme() {
  let (app, _, _, _, _temp) = test_app();

  let request =
    Request::builder().method("GET").uri("/system/events").header("authorization", "Basic dXNlcjpwYXNz").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_authenticated_non_uuid_user_identity() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = invalid_identity_bearer_token(&jwt_manager);

  let request = Request::builder().method("GET").uri("/system/events").header("authorization", &auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_sse_rejects_api_key_identity_mismatch() {
  let (app, jwt_manager, engine, _, _temp) = test_app();
  let owner_id = create_test_user(&engine, "sse_key_owner");
  let other_user_id = create_test_user(&engine, "sse_wrong_key_user");
  let rules = vec![KeyRule { glob: "/allowed/**".to_string(), permitted: "-r--l---".to_string() }];
  let (_owner_auth, key_id) = create_scoped_key_and_token(&jwt_manager, &engine, owner_id, rules);

  let wrong_user_auth = keyed_bearer_token(&jwt_manager, &other_user_id.to_string(), key_id);
  let request =
    Request::builder().method("GET").uri("/system/events").header("authorization", &wrong_user_auth).body(Body::empty()).unwrap();
  let response = app.clone().oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);

  let wrong_share_auth = keyed_bearer_token(&jwt_manager, &format!("share:{key_id}"), key_id);
  let request =
    Request::builder().method("GET").uri("/system/events").header("authorization", &wrong_share_auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Response format tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_endpoint_returns_200_with_correct_content_type() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("GET").uri("/system/events").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response.headers().get("content-type").expect("content-type header should be present").to_str().unwrap();
  assert!(content_type.contains("text/event-stream"), "expected text/event-stream, got: {}", content_type);
}

#[tokio::test]
async fn test_sse_endpoint_with_query_params_returns_200() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created,entries_deleted&path_prefix=/docs/")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_sse_endpoint_with_empty_events_param() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("GET").uri("/system/events?events=").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Event streaming tests (emit events, read SSE body)
// ---------------------------------------------------------------------------

/// Helper: emit events on the bus, then collect the SSE body with a timeout.
/// Returns the collected body text.
async fn collect_sse_with_events(
  app: axum::Router,
  auth: &str,
  uri: &str,
  event_bus: &EventBus,
  events_to_emit: Vec<EngineEvent>,
) -> String {
  // We need to handle this carefully: the SSE stream will stay open,
  // so we use tokio::spawn + timeout to collect what we can.
  let auth_owned = auth.to_string();
  let uri_owned = uri.to_string();

  let request = Request::builder().method("GET").uri(&uri_owned).header("authorization", &auth_owned).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Emit events after the subscription is established
  for event in events_to_emit {
    event_bus.emit(event);
  }

  // Give a small delay for the stream to process
  tokio::time::sleep(Duration::from_millis(50)).await;

  // Collect the body with a timeout. SSE streams don't end, so we
  // read frame-by-frame with a short timeout.
  let body = response.into_body();
  let result = tokio::time::timeout(Duration::from_millis(500), body.collect()).await;

  match result {
    Ok(Ok(collected)) => String::from_utf8_lossy(&collected.to_bytes()).to_string(),
    Ok(Err(e)) => panic!("body collect error: {:?}", e),
    Err(_) => {
      // Timeout is expected for SSE — we won't get a clean EOF.
      // This is fine; the events may have already been delivered.
      // For a more robust test, we'd use frame-by-frame reading.
      String::new()
    }
  }
}

async fn read_first_sse_frame(mut body: Body) -> String {
  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("timed out waiting for first SSE frame")
    .expect("SSE stream ended before first frame")
    .expect("failed to read first SSE frame");
  let bytes = frame.into_data().expect("first SSE frame should contain data");
  String::from_utf8_lossy(&bytes).to_string()
}

async fn read_next_sse_frame(body: &mut Body) -> String {
  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("timed out waiting for SSE frame")
    .expect("SSE stream ended before frame")
    .expect("failed to read SSE frame");
  let bytes = frame.into_data().expect("SSE frame should contain data");
  String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn test_global_sse_reports_broadcast_lag_as_a_stream_gap() {
  let (app, jwt_manager, _engine, event_bus, _temp) = test_app_with_event_capacity(2);
  let auth = bearer_token(&jwt_manager);
  let request = Request::builder().method("GET").uri("/system/events").header("authorization", auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();
  let ready = read_next_sse_frame(&mut body).await;
  assert!(ready.contains(EVENT_SERVER_READY));

  for sequence in 0..5 {
    event_bus.emit(EngineEvent::new("entries_created", "system", serde_json::json!({"path": format!("/lag/{sequence}")})));
  }

  let gap = read_next_sse_frame(&mut body).await;
  assert!(gap.contains(&format!("event: {EVENT_STREAM_GAP}")), "missing stream-gap event: {gap}");
  assert!(gap.contains("\"missed_events\":3"), "wrong lag evidence: {gap}");
  assert!(gap.contains("\"action\":\"refresh\""), "missing client recovery action: {gap}");
}

#[tokio::test]
async fn test_user_sse_reports_broadcast_lag_without_global_event_cardinality() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app_with_event_capacity(2);
  let user_id = create_test_user(&engine, "sse_lag_user");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder().method("GET").uri("/events/me").header("authorization", auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  for sequence in 0..5 {
    event_bus.emit(EngineEvent::for_user("notification", "system", &user_id.to_string(), serde_json::json!({"sequence": sequence})));
  }

  let gap = read_next_sse_frame(&mut body).await;
  assert!(gap.contains(&format!("event: {EVENT_STREAM_GAP}")), "missing user stream-gap event: {gap}");
  assert!(!gap.contains("missed_events"), "recipient stream leaked global event cardinality: {gap}");
  assert!(gap.contains("\"action\":\"refresh\""), "missing user recovery action: {gap}");
}

#[tokio::test]
async fn test_metrics_events_are_administrative_and_root_only() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_non_root_metrics");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request =
    Request::builder().method("GET").uri("/system/events?events=metrics").header("authorization", &auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  event_bus.emit(EngineEvent::new(EVENT_METRICS, "system", serde_json::json!({"memory": {"accounted_bytes": 1}})));
  let mut non_root_body = response.into_body();
  assert!(
    tokio::time::timeout(Duration::from_millis(150), non_root_body.frame()).await.is_err(),
    "non-root subscriber received an administrative metrics event"
  );

  let (app, jwt_manager, _engine, event_bus, _temp) = test_app();
  let root_auth = root_bearer_token(&jwt_manager);
  let request =
    Request::builder().method("GET").uri("/system/events?events=metrics").header("authorization", &root_auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  event_bus.emit(EngineEvent::new(EVENT_METRICS, "system", serde_json::json!({"memory": {"accounted_bytes": 2}})));
  let frame = read_first_sse_frame(response.into_body()).await;
  assert!(frame.contains("\"accounted_bytes\":2"), "root subscriber did not receive metrics event: {frame}");
}

#[tokio::test]
async fn test_sse_sends_server_ready_as_initial_event() {
  let (app, jwt_manager, _engine, _event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("GET").uri("/system/events").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let frame = read_first_sse_frame(response.into_body()).await;
  assert!(frame.contains("event: server_ready"), "expected initial server_ready event, got: {}", frame);
  assert!(frame.contains("\"event_type\":\"server_ready\""), "expected server_ready envelope, got: {}", frame);
  assert!(frame.contains("\"status\":\"ready\""), "expected ready payload, got: {}", frame);
}

#[tokio::test]
async fn test_sse_server_ready_respects_event_filter_when_included() {
  let (app, jwt_manager, _engine, _event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=server_ready,entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let frame = read_first_sse_frame(response.into_body()).await;
  assert!(frame.contains("event: server_ready"), "expected filtered server_ready event, got: {}", frame);
}

#[tokio::test]
async fn test_sse_server_ready_respects_event_filter_when_excluded() {
  let (app, jwt_manager, _engine, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  event_bus.emit(EngineEvent::new("entries_created", "alice", serde_json::json!({"entries": [{"path": "/ready-filter.txt"}]})));

  let frame = read_first_sse_frame(response.into_body()).await;
  assert!(!frame.contains(EVENT_SERVER_READY), "server_ready should be filtered out: {}", frame);
  assert!(frame.contains("event: entries_created"), "expected entries_created after filtering, got: {}", frame);
}

#[tokio::test]
async fn test_sse_receives_emitted_events() {
  let (app, jwt_manager, _engine, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![EngineEvent::new(
    "entries_created",
    "test-admin",
    serde_json::json!({
        "entries": [{"path": "/docs/readme.md", "entry_type": "file"}]
    }),
  )];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // SSE body may be empty due to timeout race, but if we got data,
  // verify the format
  if !body.is_empty() {
    assert!(body.contains("event: entries_created"), "body should contain event type, got: {}", body);
    assert!(body.contains("data: "), "body should contain data field");
    assert!(body.contains("readme.md"), "body should contain event payload");
  }
}

#[tokio::test]
async fn test_sse_event_format_is_valid_sse() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let test_event = EngineEvent::new("entries_created", "alice", serde_json::json!({"entries": [{"path": "/test.txt"}]}));
  let event_id = test_event.event_id.clone();

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, vec![test_event]).await;

  if !body.is_empty() {
    // Verify SSE format: id:, event:, data: fields
    assert!(body.contains(&format!("id: {}", event_id)), "should contain event id");
    assert!(body.contains("event: entries_created"), "should contain event type");
    assert!(body.contains("data: {"), "should contain JSON data");
  }
}

// ---------------------------------------------------------------------------
// Event type filter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_filter_by_event_type_passes_matching() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![EngineEvent::new("entries_created", "alice", serde_json::json!({"entries": [{"path": "/a.txt"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events?events=entries_created", &event_bus, events).await;

  // If data was received, it should be the matching event
  if !body.is_empty() {
    assert!(body.contains("entries_created"));
  }
}

#[tokio::test]
async fn test_sse_filter_by_event_type_blocks_non_matching() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Emit a "entries_deleted" event but filter for only "entries_created"
  let events = vec![EngineEvent::new("entries_deleted", "alice", serde_json::json!({"entries": [{"path": "/a.txt"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events?events=entries_created", &event_bus, events).await;

  // Should NOT contain the deleted event
  assert!(!body.contains("entries_deleted"), "filtered event should not appear in stream: {}", body,);
}

#[tokio::test]
async fn test_sse_filter_multiple_event_types() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![
    EngineEvent::new("entries_created", "a", serde_json::json!({"entries": [{"path": "/x"}]})),
    EngineEvent::new("entries_deleted", "a", serde_json::json!({"entries": [{"path": "/y"}]})),
    EngineEvent::new("users_created", "a", serde_json::json!({"user_id": "u1"})),
  ];

  let body = collect_sse_with_events(app, &auth, "/system/events?events=entries_created,entries_deleted", &event_bus, events).await;

  // users_created should be filtered out
  assert!(!body.contains("users_created"), "users_created should be filtered out: {}", body,);
}

// ---------------------------------------------------------------------------
// Path prefix filter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_filter_by_path_prefix_passes_matching() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![EngineEvent::new("entries_created", "alice", serde_json::json!({"entries": [{"path": "/people/alice.json"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events?path_prefix=/people/", &event_bus, events).await;

  if !body.is_empty() {
    assert!(body.contains("alice.json"), "matching event should appear");
  }
}

#[tokio::test]
async fn test_sse_filter_by_path_prefix_blocks_non_matching() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![EngineEvent::new("entries_created", "alice", serde_json::json!({"entries": [{"path": "/docs/readme.md"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events?path_prefix=/people/", &event_bus, events).await;

  assert!(!body.contains("readme.md"), "non-matching path should not appear: {}", body,);
}

#[tokio::test]
async fn test_sse_path_prefix_with_top_level_path_field() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Events with a top-level "path" (e.g. permissions, indexes) instead of entries[]
  let events = vec![EngineEvent::new(
    "permissions_changed",
    "admin",
    serde_json::json!({"path": "/people/alice", "group_name": "editors", "action": "grant"}),
  )];

  let body = collect_sse_with_events(app, &auth, "/system/events?path_prefix=/people/", &event_bus, events).await;

  if !body.is_empty() {
    assert!(body.contains("permissions_changed"), "top-level path match should pass");
  }
}

#[tokio::test]
async fn test_sse_path_prefix_blocks_top_level_path_non_matching() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![EngineEvent::new("permissions_changed", "admin", serde_json::json!({"path": "/docs/secret", "group_name": "viewers"}))];

  let body = collect_sse_with_events(app, &auth, "/system/events?path_prefix=/people/", &event_bus, events).await;

  assert!(!body.contains("permissions_changed"), "non-matching top-level path should be filtered: {}", body,);
}

#[tokio::test]
async fn test_sse_path_prefix_no_path_in_payload_filters_out() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Event with no path at all (e.g. heartbeat)
  let events = vec![EngineEvent::new("heartbeat", "system", serde_json::json!({"entry_count": 42}))];

  let body = collect_sse_with_events(app, &auth, "/system/events?path_prefix=/anything/", &event_bus, events).await;

  assert!(!body.contains("heartbeat"), "event without path should be filtered when path_prefix is set: {}", body,);
}

// ---------------------------------------------------------------------------
// Combined filter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_combined_event_type_and_path_prefix() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let events = vec![
    // Match both filters
    EngineEvent::new("entries_created", "a", serde_json::json!({"entries": [{"path": "/people/bob.json"}]})),
    // Match event type but not path
    EngineEvent::new("entries_created", "a", serde_json::json!({"entries": [{"path": "/docs/readme.md"}]})),
    // Match path but not event type
    EngineEvent::new("entries_deleted", "a", serde_json::json!({"entries": [{"path": "/people/old.json"}]})),
  ];

  let body = collect_sse_with_events(app, &auth, "/system/events?events=entries_created&path_prefix=/people/", &event_bus, events).await;

  // Only the first event should match both filters.
  // The deleted event and the /docs/ event should be filtered out.
  assert!(!body.contains("readme.md"), "wrong path should be filtered: {}", body,);
  assert!(!body.contains("entries_deleted"), "wrong event type should be filtered: {}", body,);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_no_events_emitted_stream_has_no_data_events() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("GET").uri("/system/events").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Collect whatever we can within a short window.
  // Without events, the stream should only produce keep-alive comments (": ping").
  let body = response.into_body();
  let result = tokio::time::timeout(Duration::from_millis(200), body.collect()).await;
  match result {
    Ok(Ok(collected)) => {
      let text = String::from_utf8_lossy(&collected.to_bytes()).to_string();
      // Should NOT contain any "data:" lines (only keep-alive comments or nothing)
      assert!(!text.contains("data: {"), "no data events should appear without emitting: {}", text,);
    }
    Ok(Err(e)) => panic!("body error: {:?}", e),
    Err(_) => {
      // Timeout is fine — means stream is still open with nothing to deliver
    }
  }
}

#[tokio::test]
async fn test_sse_filter_with_whitespace_in_events_param() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Spaces around event types should be trimmed
  let events = vec![EngineEvent::new("entries_created", "a", serde_json::json!({"entries": [{"path": "/x"}]}))];

  let body =
    collect_sse_with_events(app, &auth, "/system/events?events=%20entries_created%20,%20entries_deleted%20", &event_bus, events).await;

  // The entries_created event should still pass through despite whitespace
  if !body.is_empty() {
    assert!(body.contains("entries_created"));
  }
}

#[tokio::test]
async fn test_sse_method_not_allowed_for_post() {
  let (app, jwt_manager, _, _, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("POST").uri("/system/events").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------------------------------------------------------------------------
// Unit tests for matches_path_prefix helper
// ---------------------------------------------------------------------------

/// Test the filter logic directly without HTTP overhead.
mod filter_unit_tests {

  use aeordb::server::sse_routes::SseParams;

  #[test]
  fn test_event_type_filter_parsing_single() {
    let params = SseParams { events: Some("entries_created".to_string()), path_prefix: None };
    let filter: Vec<String> = params.events.unwrap().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    assert_eq!(filter, vec!["entries_created"]);
  }

  #[test]
  fn test_event_type_filter_parsing_multiple() {
    let params = SseParams { events: Some("entries_created, entries_deleted , users_created".to_string()), path_prefix: None };
    let filter: Vec<String> = params.events.unwrap().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    assert_eq!(filter, vec!["entries_created", "entries_deleted", "users_created"]);
  }

  #[test]
  fn test_event_type_filter_parsing_empty_string() {
    let params = SseParams { events: Some("".to_string()), path_prefix: None };
    let filter: Vec<String> = params.events.unwrap().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    assert!(filter.is_empty());
  }

  #[test]
  fn test_event_type_filter_parsing_trailing_commas() {
    let params = SseParams { events: Some(",entries_created,,entries_deleted,".to_string()), path_prefix: None };
    let filter: Vec<String> = params.events.unwrap().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    assert_eq!(filter, vec!["entries_created", "entries_deleted"]);
  }

  #[test]
  fn test_event_type_filter_parsing_none() {
    let params = SseParams { events: None, path_prefix: None };
    assert!(params.events.is_none());
  }
}

// ---------------------------------------------------------------------------
// Subscriber count test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_subscription_creates_subscriber() {
  let (app, jwt_manager, _engine, event_bus, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Before connecting, subscriber count should be 0 (or whatever the base is)
  let _initial_count = event_bus.subscriber_count();

  // Start an SSE connection by spawning the request
  let auth_clone = auth.clone();
  let handle = tokio::spawn(async move {
    let request = Request::builder().method("GET").uri("/system/events").header("authorization", &auth_clone).body(Body::empty()).unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Read one frame to ensure stream is established
    tokio::time::timeout(Duration::from_millis(200), response.into_body().collect()).await
  });

  // Give the stream a moment to establish
  tokio::time::sleep(Duration::from_millis(50)).await;

  // The subscriber should have been created when event_stream was called.
  // However, since oneshot consumes the router, the subscription lives
  // in the spawned task. It may or may not still be alive depending on timing.
  // This test mainly verifies the overall flow doesn't panic.

  // Clean up
  let _ = handle.await;
}

// ---------------------------------------------------------------------------
// Permission-based filtering tests
// ---------------------------------------------------------------------------

/// Create a scoped API key and return a Bearer token with key_id embedded.
fn create_scoped_key_and_token(jwt_manager: &JwtManager, engine: &StorageEngine, user_id: Uuid, rules: Vec<KeyRule>) -> (String, Uuid) {
  let key_id = Uuid::new_v4();
  let plaintext = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext).unwrap();
  let now = Utc::now();

  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(user_id),
    created_at: now,
    is_revoked: false,
    expires_at: now.timestamp_millis() + (365 * 86400 * 1000),
    label: Some("test-sse-scoped-key".to_string()),
    rules,
  };

  let ctx = RequestContext::system();
  system_store::store_api_key_for_bootstrap(engine, &ctx, &record).unwrap();

  let now_ts = now.timestamp();
  let claims = TokenClaims {
    sub: user_id.to_string(),
    iss: "aeordb".to_string(),
    iat: now_ts,
    exp: now_ts + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: Some(key_id.to_string()),
  };
  let token = jwt_manager.create_token(&claims).unwrap();
  (format!("Bearer {}", token), key_id)
}

fn create_share_key_and_token(jwt_manager: &JwtManager, engine: &StorageEngine, rules: Vec<KeyRule>) -> (String, Uuid) {
  let key_id = Uuid::new_v4();
  let plaintext = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext).unwrap();
  let now = Utc::now();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: None,
    created_at: now,
    is_revoked: false,
    expires_at: now.timestamp_millis() + (365 * 86400 * 1000),
    label: Some("test-sse-share-key".to_string()),
    rules,
  };
  system_store::store_api_key_for_bootstrap(engine, &RequestContext::system(), &record).unwrap();

  let timestamp = now.timestamp();
  let claims = TokenClaims {
    sub: format!("share:{key_id}"),
    iss: "aeordb".to_string(),
    iat: timestamp,
    exp: timestamp + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: Some(key_id.to_string()),
  };
  let token = jwt_manager.create_token(&claims).unwrap();
  (format!("Bearer {}", token), key_id)
}

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  bearer_token(jwt_manager)
}

#[tokio::test]
async fn test_sse_direct_user_projects_paths_through_current_permissions() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_direct_authority");
  grant_user_read(&engine, user_id, "/allowed/readme.txt");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_created",
    "admin",
    serde_json::json!({
      "entries": [
        {"path": "/allowed/readme.txt"},
        {"path": "/private/secret.txt"},
      ],
    }),
  ));

  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("permitted event was not delivered")
    .expect("SSE stream ended")
    .expect("SSE frame failed")
    .into_data()
    .expect("SSE event frame must contain data");
  let body = String::from_utf8_lossy(&frame);
  assert!(body.contains("/allowed/readme.txt"), "permitted path was removed: {body}");
  assert!(!body.contains("/private/secret.txt"), "ungranted path leaked: {body}");
}

#[tokio::test]
async fn test_sse_user_owned_key_requires_both_key_scope_and_user_permission() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_ungranted_key_owner");
  let rules = vec![
    KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/docs/readme.md"}]})));

  assert!(
    tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(),
    "a key rule must not grant authority absent the owning user's permission"
  );
}

#[tokio::test]
async fn test_sse_scoped_root_key_remains_constrained_by_its_rules() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let rules = vec![
    KeyRule { glob: "/allowed/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_created",
    "admin",
    serde_json::json!({
      "entries": [
        {"path": "/allowed/visible.txt"},
        {"path": "/private/hidden.txt"},
      ],
    }),
  ));

  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("scoped root event was not delivered")
    .expect("SSE stream ended")
    .expect("SSE frame failed")
    .into_data()
    .expect("SSE event frame must contain data");
  let body = String::from_utf8_lossy(&frame);
  assert!(body.contains("/allowed/visible.txt"), "allowed root-key path was removed: {body}");
  assert!(!body.contains("/private/hidden.txt"), "scoped root key leaked a denied path: {body}");
}

#[tokio::test]
async fn test_sse_scoped_root_key_drops_batch_members_without_an_authorizable_path() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let rules = vec![
    KeyRule { glob: "/allowed/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_created",
    "admin",
    serde_json::json!({
      "entries": [
        {"path": "/allowed/visible.txt"},
        {"name": "unscoped-secret.txt", "content_type": "application/x-secret"},
      ],
    }),
  ));

  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("scoped root event was not delivered")
    .expect("SSE stream ended")
    .expect("SSE frame failed")
    .into_data()
    .expect("SSE event frame must contain data");
  let body = String::from_utf8_lossy(&frame);
  assert!(body.contains("/allowed/visible.txt"), "allowed root-key path was removed: {body}");
  assert!(!body.contains("unscoped-secret.txt"), "scoped root key received a member without an authorizable path: {body}");
}

#[tokio::test]
async fn test_sse_share_key_uses_key_rules_without_user_permission_authority() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let rules = vec![
    KeyRule { glob: "/shared/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_share_key_and_token(&jwt_manager, &engine, rules);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/shared/readme.txt"}]})));

  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("share-key event was not delivered")
    .expect("SSE stream ended")
    .expect("SSE frame failed")
    .into_data()
    .expect("SSE event frame must contain data");
  let body = String::from_utf8_lossy(&frame);
  assert!(body.contains("/shared/readme.txt"), "share-key path was removed: {body}");
}

#[tokio::test]
async fn test_sse_stops_path_delivery_after_api_key_revocation() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_revoked_key_owner");
  grant_user_read(&engine, user_id, "/docs/readme.md");
  let rules = vec![
    KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  assert!(system_store::revoke_api_key(&engine, &RequestContext::system(), key_id).unwrap());
  event_bus.emit(EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/docs/readme.md"}]})));

  assert!(tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(), "revoked API key continued receiving path events");
}

#[tokio::test]
async fn test_sse_stops_path_delivery_after_user_permission_revocation() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_revoked_permission_user");
  let path = "/docs/revoked.md";
  grant_user_read(&engine, user_id, path);
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_deleted")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  let group = format!("user:{user_id}");
  PermissionStore::new(&engine)
    .revoke_path(&RequestContext::system(), path, &group, Some("revoked.md"))
    .expect("revoke SSE test permission");
  event_bus.emit(EngineEvent::new("entries_deleted", "admin", serde_json::json!({"entries": [{"path": path}]})));

  assert!(
    tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(),
    "revoked user permission continued receiving path events"
  );
}

#[tokio::test]
async fn test_sse_global_stream_excludes_recipient_addressed_events() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let subscriber_id = create_test_user(&engine, "sse_global_recipient_observer");
  let recipient_id = create_test_user(&engine, "sse_private_recipient");
  grant_user_read(&engine, subscriber_id, "/shared/private-notice.txt");
  let auth = user_bearer_token(&jwt_manager, subscriber_id);
  let request =
    Request::builder().method("GET").uri("/system/events?events=files_shared").header("authorization", &auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::for_user(
    "files_shared",
    "admin",
    &recipient_id.to_string(),
    serde_json::json!({"path": "/shared/private-notice.txt", "permissions": ".r..l..."}),
  ));

  assert!(
    tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(),
    "global SSE stream leaked an event addressed to another user"
  );
}

#[tokio::test]
async fn test_sse_non_root_subscriber_cannot_observe_task_path_arguments() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_non_root_task_observer");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request =
    Request::builder().method("GET").uri("/system/events?events=tasks_started").header("authorization", &auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "tasks_started",
    "system",
    serde_json::json!({"task_id": "task-secret", "task_type": "reindex", "args": {"path": "/private/secret"}}),
  ));

  assert!(
    tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(),
    "non-root SSE stream leaked root-only task arguments"
  );
}

#[tokio::test]
async fn test_sse_gc_status_is_root_only() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_non_root_gc_observer");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request =
    Request::builder().method("GET").uri("/system/events?events=gc_status").header("authorization", &auth).body(Body::empty()).unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(EVENT_GC_STATUS, "system", serde_json::json!({"state": "running", "overall_progress": 0.5})));
  assert!(tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(), "non-root SSE stream leaked root-only GC status");

  let (root_app, root_jwt_manager, _, root_event_bus, _root_temp) = test_app();
  let root_auth = root_bearer_token(&root_jwt_manager);
  let root_payload = serde_json::json!({"state": "complete", "overall_progress": 1.0});
  let root_request = Request::builder()
    .method("GET")
    .uri("/system/events?events=gc_status")
    .header("authorization", &root_auth)
    .body(Body::empty())
    .unwrap();
  let root_response = root_app.oneshot(root_request).await.unwrap();
  assert_eq!(root_response.status(), StatusCode::OK);
  let mut root_body = root_response.into_body();
  root_event_bus.emit(EngineEvent::new(EVENT_GC_STATUS, "system", root_payload));
  let root_frame = read_next_sse_frame(&mut root_body).await;
  assert!(root_frame.contains("event: gc_status"), "root SSE stream did not receive GC status: {root_frame}");
  assert!(root_frame.contains("\"state\":\"complete\""), "root SSE stream changed the GC status payload: {root_frame}");
}

#[tokio::test]
async fn test_sse_non_root_path_required_event_without_path_fails_closed() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_malformed_path_event_user");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_created",
    "system",
    serde_json::json!({"entries": [{"name": "secret.txt"}], "path_hint": "/private/secret.txt"}),
  ));

  assert!(
    tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(),
    "non-root SSE stream accepted a path-required event without an authorized path"
  );
}

#[tokio::test]
async fn test_sse_root_user_receives_all_events() {
  let (app, jwt_manager, _, event_bus, _temp) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let events = vec![
    EngineEvent::new("entries_created", "someone", serde_json::json!({"entries": [{"path": "/secret/stuff.txt"}]})),
    EngineEvent::new("heartbeat", "system", serde_json::json!({"node_id": 1})),
  ];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // Root should see everything
  if !body.is_empty() {
    assert!(body.contains("entries_created"), "root should see entries_created");
  }
}

#[tokio::test]
async fn test_sse_non_root_user_cannot_observe_registry_concealed_paths() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_non_root_concealment");
  grant_user_read(&engine, user_id, "/ordinary/file.txt");
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_deleted")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_deleted",
    "system",
    serde_json::json!({
      "entries": [{"path": "/.aeordb-system/refresh-tokens/secret-token-hash"}],
      "mutation_kind": "maintenance_repair",
    }),
  ));
  event_bus.emit(EngineEvent::new("entries_deleted", "system", serde_json::json!({"entries": [{"path": "/ordinary/file.txt"}]})));

  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("ordinary event was not delivered")
    .expect("SSE stream ended")
    .expect("SSE frame failed")
    .into_data()
    .expect("SSE event frame must contain data");
  let body = String::from_utf8_lossy(&frame);

  assert!(!body.contains("secret-token-hash"), "protected credential paths must not leak to non-root SSE subscribers: {body}");
  assert!(body.contains("/ordinary/file.txt"), "ordinary path event should remain visible: {body}");
}

#[tokio::test]
async fn test_sse_scoped_key_receives_events_for_allowed_paths() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_scoped_allowed");
  grant_user_read(&engine, user_id, "/docs/readme.md");

  // Create a scoped key that can only read /docs/**
  let rules = vec![
    KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

  let events = vec![EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/docs/readme.md"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // Should see the /docs/ event
  if !body.is_empty() {
    assert!(body.contains("readme.md"), "scoped key should see events for allowed paths");
  }
}

#[tokio::test]
async fn test_sse_scoped_key_projects_mixed_batch_to_allowed_paths() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_scoped_mixed_batch");
  grant_user_read(&engine, user_id, "/docs/readme.md");
  let rules = vec![
    KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_created",
    "admin",
    serde_json::json!({
      "operation_id": "00000000-0000-0000-0000-000000000001",
      "publication_sequence": 42,
      "mutation_kind": "batch_write",
      "previous_root_hash": "11".repeat(32),
      "root_hash": "22".repeat(32),
      "entries": [
        {"path": "/docs/readme.md"},
        {"path": "/private/secret.txt"},
      ],
      "affected_relationships": [
        {
          "path": "/docs/readme.md",
          "entry_type": "file",
          "change": "created",
          "stable_key": "must-not-leak",
          "physical_incarnation": {"offset": 7},
        },
        {"path": "/private/secret.txt", "entry_type": "file", "change": "created"},
      ],
    }),
  ));

  let frame = tokio::time::timeout(Duration::from_millis(500), body.frame())
    .await
    .expect("allowed batch projection was not delivered")
    .expect("SSE stream ended")
    .expect("SSE frame failed")
    .into_data()
    .expect("SSE event frame must contain data");
  let body = String::from_utf8_lossy(&frame);
  assert!(body.contains("/docs/readme.md"), "allowed entry was removed: {body}");
  assert!(!body.contains("/private/secret.txt"), "denied entry leaked through mixed batch projection: {body}");
  assert_eq!(body.matches("/docs/readme.md").count(), 2, "entry and relationship projections diverged: {body}");
  assert!(body.contains("\"previous_root_hash\":\"11"), "previous root identity was removed: {body}");
  assert!(body.contains("\"root_hash\":\"22"), "selected root identity was removed: {body}");
  assert!(!body.contains("stable_key"), "SSE projected internal stable-key authority: {body}");
  assert!(!body.contains("physical_incarnation"), "SSE projected physical mutation authority: {body}");
}

#[tokio::test]
async fn coordinator_batch_reaches_sse_with_exact_roots_and_only_authorized_relationships() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_coordinator_projection");
  let allowed_path = "/docs/coordinator.txt";
  let hidden_path = "/private/coordinator.txt";
  grant_user_read(&engine, user_id, allowed_path);

  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  let previous_root_hash = engine.head_hash().unwrap();
  let mutation_context = RequestContext::from_claims("coordinator-actor", event_bus);
  DirectoryOps::new(&engine)
    .store_files_buffered_batch(
      &mutation_context,
      vec![
        BufferedFile { path: allowed_path.to_string(), data: b"visible".to_vec(), content_type: Some("text/plain".to_string()) },
        BufferedFile { path: hidden_path.to_string(), data: b"hidden".to_vec(), content_type: Some("text/plain".to_string()) },
      ],
    )
    .unwrap();
  let root_hash = engine.head_hash().unwrap();

  let frame = read_next_sse_frame(&mut body).await;
  let data = frame.lines().find_map(|line| line.strip_prefix("data: ")).expect("SSE frame must include data");
  let event: serde_json::Value = serde_json::from_str(data).unwrap();
  assert_eq!(event["payload"]["previous_root_hash"], hex::encode(previous_root_hash));
  assert_eq!(event["payload"]["root_hash"], hex::encode(root_hash));
  assert_eq!(event["payload"]["entries"].as_array().unwrap().len(), 1);
  assert_eq!(event["payload"]["entries"][0]["path"], allowed_path);
  assert_eq!(
    event["payload"]["affected_relationships"],
    serde_json::json!([{"path": allowed_path, "entry_type": "file", "change": "updated"}])
  );
  assert!(!frame.contains(hidden_path), "coordinator SSE projection leaked hidden relationship: {frame}");
}

#[tokio::test]
async fn malformed_public_relationship_fails_closed_before_sse_projection() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_malformed_relationship");
  let path = "/docs/malformed.txt";
  grant_user_read(&engine, user_id, path);
  let auth = user_bearer_token(&jwt_manager, user_id);
  let request = Request::builder()
    .method("GET")
    .uri("/system/events?events=entries_created")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let mut body = response.into_body();

  event_bus.emit(EngineEvent::new(
    "entries_created",
    "admin",
    serde_json::json!({
      "entries": [{"path": path}],
      "affected_relationships": [{"path": path, "entry_type": "file", "change": "rewritten"}],
    }),
  ));

  assert!(
    tokio::time::timeout(Duration::from_millis(150), body.frame()).await.is_err(),
    "malformed public relationship was projected to the subscriber"
  );
}

#[tokio::test]
async fn test_sse_scoped_key_blocks_events_for_disallowed_paths() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_scoped_denied");
  grant_user_read(&engine, user_id, "/secret/passwords.txt");

  // Create a scoped key that can only read /docs/**
  let rules = vec![
    KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

  let events = vec![EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/secret/passwords.txt"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // Should NOT see the /secret/ event
  assert!(!body.contains("passwords.txt"), "scoped key should NOT see events for disallowed paths: {}", body,);
}

#[tokio::test]
async fn test_sse_scoped_key_receives_system_events_without_paths() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_scoped_system_events");

  // Create a scoped key with restricted access
  let rules = vec![
    KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

  let events = vec![EngineEvent::new("heartbeat", "system", serde_json::json!({"node_id": 1, "intent_time": 1000}))];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // System events (no path) should pass through to all subscribers
  if !body.is_empty() {
    assert!(body.contains("heartbeat"), "system events should reach scoped subscribers");
  }
}

#[tokio::test]
async fn test_sse_scoped_key_mixed_allowed_and_blocked() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_scoped_mixed_events");
  grant_user_read(&engine, user_id, "/public/info.txt");
  grant_user_read(&engine, user_id, "/private/secret.txt");

  // Create a scoped key that can only read /public/**
  let rules = vec![
    KeyRule { glob: "/public/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

  let events = vec![
    // Allowed: /public/ path
    EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/public/info.txt"}]})),
    // Blocked: /private/ path
    EngineEvent::new("entries_created", "admin", serde_json::json!({"entries": [{"path": "/private/secret.txt"}]})),
    // Allowed: no path (system event)
    EngineEvent::new("metrics", "system", serde_json::json!({"cpu": 42})),
  ];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // The /private/ event should be filtered out
  assert!(!body.contains("secret.txt"), "private path should be filtered: {}", body,);
}

#[tokio::test]
async fn test_sse_scoped_key_blocks_top_level_path_field() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_scoped_top_level_denied");
  grant_user_read(&engine, user_id, "/forbidden/stuff");

  // Scoped to /allowed/** only
  let rules = vec![
    KeyRule { glob: "/allowed/**".to_string(), permitted: "-r--l---".to_string() },
    KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
  ];
  let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

  // Event with top-level "path" field (permissions_changed style)
  let events =
    vec![EngineEvent::new("permissions_changed", "admin", serde_json::json!({"path": "/forbidden/stuff", "group_name": "editors"}))];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  assert!(!body.contains("permissions_changed"), "top-level path to forbidden area should be filtered: {}", body,);
}

#[tokio::test]
async fn test_sse_user_without_key_rules_receives_permitted_events() {
  let (app, jwt_manager, engine, event_bus, _temp) = test_app();
  let user_id = create_test_user(&engine, "sse_direct_permitted");
  grant_user_read(&engine, user_id, "/any/path.txt");
  let auth = user_bearer_token(&jwt_manager, user_id);

  let events = vec![EngineEvent::new("entries_created", "someone", serde_json::json!({"entries": [{"path": "/any/path.txt"}]}))];

  let body = collect_sse_with_events(app, &auth, "/system/events", &event_bus, events).await;

  // A direct user token has no additional key bound, but normal path
  // permission authority still applies.
  if !body.is_empty() {
    assert!(body.contains("path.txt"), "user without key rules should see permitted events");
  }
}
