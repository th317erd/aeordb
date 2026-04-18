use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::engine::{EventBus, StorageEngine};
use aeordb::plugins::PluginManager;
use aeordb::auth::FileAuthProvider;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> =
        Arc::new(FileAuthProvider::new(engine.clone()));
    let app = create_app_with_all(
        auth_provider,
        jwt_manager.clone(),
        plugin_manager,
        rate_limiter,
        make_prometheus_handle(),
        engine.clone(),
        Arc::new(EventBus::new()),
        CorsState {
            default_origins: None,
            rules: vec![],
        },
    );
    (app, jwt_manager, engine, temp_dir)
}

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: "00000000-0000-0000-0000-000000000000".to_string(),
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

fn non_root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: uuid::Uuid::new_v4().to_string(),
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

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ===========================================================================
// GET /admin/cluster -- cluster status
// ===========================================================================

#[tokio::test]
async fn test_cluster_status_returns_node_info() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::get("/sync/status")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert!(json.get("peer_count").is_some());
    assert_eq!(json["peer_count"], 0);
    assert!(json.get("peers").is_some());
}

#[tokio::test]
async fn test_cluster_status_requires_root() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = non_root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::get("/sync/status")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_cluster_status_requires_auth() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let response = app
        .oneshot(
            Request::get("/sync/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// POST /admin/cluster/peers -- add a peer
// ===========================================================================

#[tokio::test]
async fn test_add_peer_returns_201() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.5:9000",
                        "label": "us-west"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response.into_body()).await;
    assert!(json["node_id"].is_number());
    assert_eq!(json["address"], "10.0.0.5:9000");
    assert_eq!(json["label"], "us-west");
    assert_eq!(json["state"], "disconnected");
}

#[tokio::test]
async fn test_add_peer_missing_address_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "label": "no-address"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("address"));
}

#[tokio::test]
async fn test_add_peer_requires_root() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = non_root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.5:9000"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_add_peer_without_label() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.5:9000"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response.into_body()).await;
    assert!(json["label"].is_null());
}

// ===========================================================================
// GET /admin/cluster/peers -- list peers
// ===========================================================================

#[tokio::test]
async fn test_list_peers_empty() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::get("/sync/peers")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert!(json["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_list_peers_after_add() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    // Add a peer first
    let add_response = app
        .clone()
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.5:9000",
                        "label": "replica-1"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add_response.status(), StatusCode::CREATED);

    // List peers
    let list_response = app
        .oneshot(
            Request::get("/sync/peers")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(list_response.status(), StatusCode::OK);
    let json = body_json(list_response.into_body()).await;
    let peers = json["items"].as_array().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0]["address"], "10.0.0.5:9000");
    assert_eq!(peers[0]["label"], "replica-1");
    assert_eq!(peers[0]["state"], "disconnected");
}

#[tokio::test]
async fn test_list_peers_requires_root() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = non_root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::get("/sync/peers")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// DELETE /admin/cluster/peers/{node_id} -- remove a peer
// ===========================================================================

#[tokio::test]
async fn test_remove_peer_returns_200() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    // Add a peer first
    let add_response = app
        .clone()
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.5:9000"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add_response.status(), StatusCode::CREATED);
    let add_json = body_json(add_response.into_body()).await;
    let node_id = add_json["node_id"].as_u64().unwrap();

    // Remove the peer
    let response = app
        .oneshot(
            Request::delete(&format!("/sync/peers/{}", node_id))
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert_eq!(json["removed"], true);
    assert_eq!(json["node_id"], node_id);
}

#[tokio::test]
async fn test_remove_peer_not_found_returns_404() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::delete("/sync/peers/999999999")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_remove_peer_invalid_node_id_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::delete("/sync/peers/not-a-number")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_remove_peer_requires_root() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = non_root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::delete("/sync/peers/12345")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// POST /admin/cluster/sync -- trigger sync (placeholder)
// ===========================================================================

#[tokio::test]
async fn test_trigger_sync_returns_not_implemented() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::post("/sync/trigger")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn test_trigger_sync_requires_root() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = non_root_bearer_token(&jwt_manager);

    let response = app
        .oneshot(
            Request::post("/sync/trigger")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// GET /admin/cluster -- sync_status in cluster status response
// ===========================================================================

#[tokio::test]
async fn test_admin_cluster_includes_sync_status() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    // Add a peer first
    let add_response = app
        .clone()
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.5:9000",
                        "label": "sync-test-peer"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add_response.status(), StatusCode::CREATED);

    // Get cluster status
    let response = app
        .oneshot(
            Request::get("/sync/status")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let peers = json["peers"].as_array().expect("peers should be an array");
    assert_eq!(peers.len(), 1);

    let peer = &peers[0];
    assert!(peer.get("sync_status").is_some(), "peer should have sync_status field");

    let sync_status = &peer["sync_status"];
    assert_eq!(sync_status["consecutive_failures"], 0);
    assert_eq!(sync_status["total_syncs"], 0);
    assert_eq!(sync_status["total_failures"], 0);
    assert!(sync_status["last_success_at"].is_null());
    assert!(sync_status["last_attempt_at"].is_null());
    assert!(sync_status["last_error"].is_null());
}

#[tokio::test]
async fn test_list_peers_includes_sync_status() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let token = root_bearer_token(&jwt_manager);

    // Add a peer first
    let add_response = app
        .clone()
        .oneshot(
            Request::post("/sync/peers")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "address": "10.0.0.6:9000"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add_response.status(), StatusCode::CREATED);

    // List peers
    let response = app
        .oneshot(
            Request::get("/sync/peers")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let peers = json["items"].as_array().expect("response should be an array");
    assert_eq!(peers.len(), 1);

    let peer = &peers[0];
    assert!(peer.get("sync_status").is_some(), "peer should have sync_status field");

    let sync_status = &peer["sync_status"];
    assert_eq!(sync_status["consecutive_failures"], 0);
    assert_eq!(sync_status["total_syncs"], 0);
    assert_eq!(sync_status["total_failures"], 0);
}
