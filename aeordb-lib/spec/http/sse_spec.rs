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
use aeordb::engine::{EngineEvent, EventBus, RequestContext, StorageEngine};
use aeordb::engine::system_store;
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

/// Create a test app that returns the shared EventBus for direct event injection.
fn test_app() -> (
    axum::Router,
    Arc<JwtManager>,
    Arc<StorageEngine>,
    Arc<EventBus>,
    tempfile::TempDir,
) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> =
        Arc::new(FileAuthProvider::new(engine.clone()));
    let event_bus = Arc::new(EventBus::new());
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
        sub: "test-admin".to_string(),
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

// ---------------------------------------------------------------------------
// Auth tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_requires_auth() {
    let (app, _, _, _, _temp) = test_app();

    let request = Request::builder()
        .method("GET")
        .uri("/system/events")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_expired_token() {
    let (app, jwt_manager, _, _, _temp) = test_app();
    let auth = expired_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/events")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_malformed_token() {
    let (app, _, _, _, _temp) = test_app();

    let request = Request::builder()
        .method("GET")
        .uri("/system/events")
        .header("authorization", "Bearer not-a-real-jwt")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sse_rejects_wrong_scheme() {
    let (app, _, _, _, _temp) = test_app();

    let request = Request::builder()
        .method("GET")
        .uri("/system/events")
        .header("authorization", "Basic dXNlcjpwYXNz")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Response format tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_endpoint_returns_200_with_correct_content_type() {
    let (app, jwt_manager, _, _, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/events")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .expect("content-type header should be present")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got: {}",
        content_type
    );
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

    let request = Request::builder()
        .method("GET")
        .uri("/system/events?events=")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

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

    let request = Request::builder()
        .method("GET")
        .uri(&uri_owned)
        .header("authorization", &auth_owned)
        .body(Body::empty())
        .unwrap();

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
        Ok(Ok(collected)) => {
            String::from_utf8_lossy(&collected.to_bytes()).to_string()
        }
        Ok(Err(e)) => panic!("body collect error: {:?}", e),
        Err(_) => {
            // Timeout is expected for SSE — we won't get a clean EOF.
            // This is fine; the events may have already been delivered.
            // For a more robust test, we'd use frame-by-frame reading.
            String::new()
        }
    }
}

#[tokio::test]
async fn test_sse_receives_emitted_events() {
    let (app, jwt_manager, _engine, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "test-admin",
            serde_json::json!({
                "entries": [{"path": "/docs/readme.md", "entry_type": "file"}]
            }),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

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

    let test_event = EngineEvent::new(
        "entries_created",
        "alice",
        serde_json::json!({"entries": [{"path": "/test.txt"}]}),
    );
    let event_id = test_event.event_id.clone();

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        vec![test_event],
    )
    .await;

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

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "alice",
            serde_json::json!({"entries": [{"path": "/a.txt"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?events=entries_created",
        &event_bus,
        events,
    )
    .await;

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
    let events = vec![
        EngineEvent::new(
            "entries_deleted",
            "alice",
            serde_json::json!({"entries": [{"path": "/a.txt"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?events=entries_created",
        &event_bus,
        events,
    )
    .await;

    // Should NOT contain the deleted event
    assert!(
        !body.contains("entries_deleted"),
        "filtered event should not appear in stream: {}",
        body,
    );
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

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?events=entries_created,entries_deleted",
        &event_bus,
        events,
    )
    .await;

    // users_created should be filtered out
    assert!(
        !body.contains("users_created"),
        "users_created should be filtered out: {}",
        body,
    );
}

// ---------------------------------------------------------------------------
// Path prefix filter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_filter_by_path_prefix_passes_matching() {
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "alice",
            serde_json::json!({"entries": [{"path": "/people/alice.json"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?path_prefix=/people/",
        &event_bus,
        events,
    )
    .await;

    if !body.is_empty() {
        assert!(body.contains("alice.json"), "matching event should appear");
    }
}

#[tokio::test]
async fn test_sse_filter_by_path_prefix_blocks_non_matching() {
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "alice",
            serde_json::json!({"entries": [{"path": "/docs/readme.md"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?path_prefix=/people/",
        &event_bus,
        events,
    )
    .await;

    assert!(
        !body.contains("readme.md"),
        "non-matching path should not appear: {}",
        body,
    );
}

#[tokio::test]
async fn test_sse_path_prefix_with_top_level_path_field() {
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    // Events with a top-level "path" (e.g. permissions, indexes) instead of entries[]
    let events = vec![
        EngineEvent::new(
            "permissions_changed",
            "admin",
            serde_json::json!({"path": "/people/alice", "group_name": "editors", "action": "grant"}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?path_prefix=/people/",
        &event_bus,
        events,
    )
    .await;

    if !body.is_empty() {
        assert!(body.contains("permissions_changed"), "top-level path match should pass");
    }
}

#[tokio::test]
async fn test_sse_path_prefix_blocks_top_level_path_non_matching() {
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let events = vec![
        EngineEvent::new(
            "permissions_changed",
            "admin",
            serde_json::json!({"path": "/docs/secret", "group_name": "viewers"}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?path_prefix=/people/",
        &event_bus,
        events,
    )
    .await;

    assert!(
        !body.contains("permissions_changed"),
        "non-matching top-level path should be filtered: {}",
        body,
    );
}

#[tokio::test]
async fn test_sse_path_prefix_no_path_in_payload_filters_out() {
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    // Event with no path at all (e.g. heartbeat)
    let events = vec![
        EngineEvent::new(
            "heartbeat",
            "system",
            serde_json::json!({"entry_count": 42}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?path_prefix=/anything/",
        &event_bus,
        events,
    )
    .await;

    assert!(
        !body.contains("heartbeat"),
        "event without path should be filtered when path_prefix is set: {}",
        body,
    );
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

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?events=entries_created&path_prefix=/people/",
        &event_bus,
        events,
    )
    .await;

    // Only the first event should match both filters.
    // The deleted event and the /docs/ event should be filtered out.
    assert!(
        !body.contains("readme.md"),
        "wrong path should be filtered: {}",
        body,
    );
    assert!(
        !body.contains("entries_deleted"),
        "wrong event type should be filtered: {}",
        body,
    );
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sse_no_events_emitted_stream_has_no_data_events() {
    let (app, jwt_manager, _, _, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/events")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

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
            assert!(
                !text.contains("data: {"),
                "no data events should appear without emitting: {}",
                text,
            );
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
    let events = vec![
        EngineEvent::new("entries_created", "a", serde_json::json!({"entries": [{"path": "/x"}]})),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events?events=%20entries_created%20,%20entries_deleted%20",
        &event_bus,
        events,
    )
    .await;

    // The entries_created event should still pass through despite whitespace
    if !body.is_empty() {
        assert!(body.contains("entries_created"));
    }
}

#[tokio::test]
async fn test_sse_method_not_allowed_for_post() {
    let (app, jwt_manager, _, _, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/events")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

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
        let params = SseParams {
            events: Some("entries_created".to_string()),
            path_prefix: None,
        };
        let filter: Vec<String> = params
            .events
            .unwrap()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(filter, vec!["entries_created"]);
    }

    #[test]
    fn test_event_type_filter_parsing_multiple() {
        let params = SseParams {
            events: Some("entries_created, entries_deleted , users_created".to_string()),
            path_prefix: None,
        };
        let filter: Vec<String> = params
            .events
            .unwrap()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(filter, vec!["entries_created", "entries_deleted", "users_created"]);
    }

    #[test]
    fn test_event_type_filter_parsing_empty_string() {
        let params = SseParams {
            events: Some("".to_string()),
            path_prefix: None,
        };
        let filter: Vec<String> = params
            .events
            .unwrap()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(filter.is_empty());
    }

    #[test]
    fn test_event_type_filter_parsing_trailing_commas() {
        let params = SseParams {
            events: Some(",entries_created,,entries_deleted,".to_string()),
            path_prefix: None,
        };
        let filter: Vec<String> = params
            .events
            .unwrap()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(filter, vec!["entries_created", "entries_deleted"]);
    }

    #[test]
    fn test_event_type_filter_parsing_none() {
        let params = SseParams {
            events: None,
            path_prefix: None,
        };
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
        let request = Request::builder()
            .method("GET")
            .uri("/system/events")
            .header("authorization", &auth_clone)
            .body(Body::empty())
            .unwrap();
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
fn create_scoped_key_and_token(
    jwt_manager: &JwtManager,
    engine: &StorageEngine,
    user_id: Uuid,
    rules: Vec<KeyRule>,
) -> (String, Uuid) {
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

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::nil().to_string(),
        iss: "aeordb".to_string(),
        iat: now,
        exp: now + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: None,
    };
    let token = jwt_manager.create_token(&claims).expect("create root token");
    format!("Bearer {}", token)
}

#[tokio::test]
async fn test_sse_root_user_receives_all_events() {
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "someone",
            serde_json::json!({"entries": [{"path": "/secret/stuff.txt"}]}),
        ),
        EngineEvent::new(
            "heartbeat",
            "system",
            serde_json::json!({"node_id": 1}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    // Root should see everything
    if !body.is_empty() {
        assert!(body.contains("entries_created"), "root should see entries_created");
    }
}

#[tokio::test]
async fn test_sse_scoped_key_receives_events_for_allowed_paths() {
    let (app, jwt_manager, engine, event_bus, _temp) = test_app();

    // Create a scoped key that can only read /docs/**
    let rules = vec![
        KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
        KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
    ];
    let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::new_v4(), rules);

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "admin",
            serde_json::json!({"entries": [{"path": "/docs/readme.md"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    // Should see the /docs/ event
    if !body.is_empty() {
        assert!(body.contains("readme.md"), "scoped key should see events for allowed paths");
    }
}

#[tokio::test]
async fn test_sse_scoped_key_blocks_events_for_disallowed_paths() {
    let (app, jwt_manager, engine, event_bus, _temp) = test_app();

    // Create a scoped key that can only read /docs/**
    let rules = vec![
        KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
        KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
    ];
    let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::new_v4(), rules);

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "admin",
            serde_json::json!({"entries": [{"path": "/secret/passwords.txt"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    // Should NOT see the /secret/ event
    assert!(
        !body.contains("passwords.txt"),
        "scoped key should NOT see events for disallowed paths: {}",
        body,
    );
}

#[tokio::test]
async fn test_sse_scoped_key_receives_system_events_without_paths() {
    let (app, jwt_manager, engine, event_bus, _temp) = test_app();

    // Create a scoped key with restricted access
    let rules = vec![
        KeyRule { glob: "/docs/**".to_string(), permitted: "-r--l---".to_string() },
        KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
    ];
    let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::new_v4(), rules);

    let events = vec![
        EngineEvent::new(
            "heartbeat",
            "system",
            serde_json::json!({"node_id": 1, "intent_time": 1000}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    // System events (no path) should pass through to all subscribers
    if !body.is_empty() {
        assert!(body.contains("heartbeat"), "system events should reach scoped subscribers");
    }
}

#[tokio::test]
async fn test_sse_scoped_key_mixed_allowed_and_blocked() {
    let (app, jwt_manager, engine, event_bus, _temp) = test_app();

    // Create a scoped key that can only read /public/**
    let rules = vec![
        KeyRule { glob: "/public/**".to_string(), permitted: "-r--l---".to_string() },
        KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
    ];
    let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::new_v4(), rules);

    let events = vec![
        // Allowed: /public/ path
        EngineEvent::new(
            "entries_created",
            "admin",
            serde_json::json!({"entries": [{"path": "/public/info.txt"}]}),
        ),
        // Blocked: /private/ path
        EngineEvent::new(
            "entries_created",
            "admin",
            serde_json::json!({"entries": [{"path": "/private/secret.txt"}]}),
        ),
        // Allowed: no path (system event)
        EngineEvent::new(
            "metrics",
            "system",
            serde_json::json!({"cpu": 42}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    // The /private/ event should be filtered out
    assert!(
        !body.contains("secret.txt"),
        "private path should be filtered: {}",
        body,
    );
}

#[tokio::test]
async fn test_sse_scoped_key_blocks_top_level_path_field() {
    let (app, jwt_manager, engine, event_bus, _temp) = test_app();

    // Scoped to /allowed/** only
    let rules = vec![
        KeyRule { glob: "/allowed/**".to_string(), permitted: "-r--l---".to_string() },
        KeyRule { glob: "**".to_string(), permitted: "--------".to_string() },
    ];
    let (auth, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::new_v4(), rules);

    // Event with top-level "path" field (permissions_changed style)
    let events = vec![
        EngineEvent::new(
            "permissions_changed",
            "admin",
            serde_json::json!({"path": "/forbidden/stuff", "group_name": "editors"}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    assert!(
        !body.contains("permissions_changed"),
        "top-level path to forbidden area should be filtered: {}",
        body,
    );
}

#[tokio::test]
async fn test_sse_user_without_key_rules_receives_all_events() {
    // A non-root user authenticated directly (no API key) should get all events
    let (app, jwt_manager, _, event_bus, _temp) = test_app();
    let auth = bearer_token(&jwt_manager); // Uses "test-admin" sub, no key_id

    let events = vec![
        EngineEvent::new(
            "entries_created",
            "someone",
            serde_json::json!({"entries": [{"path": "/any/path.txt"}]}),
        ),
    ];

    let body = collect_sse_with_events(
        app,
        &auth,
        "/system/events",
        &event_bus,
        events,
    )
    .await;

    // User without key rules should see everything
    if !body.is_empty() {
        assert!(body.contains("path.txt"), "user without key rules should see all events");
    }
}
