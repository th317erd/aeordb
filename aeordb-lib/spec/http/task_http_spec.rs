use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::{RequestContext, StorageEngine, TaskQueue};
use aeordb::server::{create_app_with_jwt_engine_and_task_queue, create_temp_engine_for_tests};

/// Create a fresh in-memory app with engine and task queue support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, Arc<TaskQueue>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let task_queue = Arc::new(TaskQueue::new(engine.clone()));
    let app = create_app_with_jwt_engine_and_task_queue(jwt_manager.clone(), engine.clone(), task_queue.clone());
    (app, jwt_manager, engine, task_queue, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>, task_queue: &Arc<TaskQueue>) -> axum::Router {
    create_app_with_jwt_engine_and_task_queue(jwt_manager.clone(), engine.clone(), task_queue.clone())
}

/// Root user Bearer token (nil UUID).
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

/// Collect response body into JSON.
async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ===========================================================================
// 1. test_trigger_reindex_via_http
// ===========================================================================

#[tokio::test]
async fn test_trigger_reindex_via_http() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/tasks/reindex")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"path":"/data/"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert!(json.get("id").is_some(), "response should contain task id");
    assert_eq!(json["task_type"], "reindex");
    assert_eq!(json["status"], "pending");
}

// ===========================================================================
// 2. test_trigger_gc_via_http
// ===========================================================================

#[tokio::test]
async fn test_trigger_gc_via_http() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/tasks/gc")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"dry_run":false}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert!(json.get("id").is_some());
    assert_eq!(json["task_type"], "gc");
    assert_eq!(json["status"], "pending");
}

// ===========================================================================
// 3. test_list_tasks
// ===========================================================================

#[tokio::test]
async fn test_list_tasks() {
    let (app, jwt_manager, _engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Create some tasks directly on the queue
    task_queue.enqueue("reindex", serde_json::json!({"path": "/a/"})).unwrap();
    task_queue.enqueue("gc", serde_json::json!({"dry_run": true})).unwrap();

    let request = Request::builder()
        .method("GET")
        .uri("/system/tasks")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let tasks = json["items"].as_array().expect("response should have items array");
    assert_eq!(tasks.len(), 2, "should have 2 tasks");

    // Verify task types present
    let types: Vec<&str> = tasks.iter()
        .map(|t| t["task_type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"reindex"));
    assert!(types.contains(&"gc"));
}

// ===========================================================================
// 4. test_cancel_task_via_http
// ===========================================================================

#[tokio::test]
async fn test_cancel_task_via_http() {
    let (app, jwt_manager, _engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Create a task
    let record = task_queue.enqueue("reindex", serde_json::json!({"path": "/x/"})).unwrap();
    let task_id = record.id.clone();

    // Cancel it via HTTP
    let request = Request::builder()
        .method("DELETE")
        .uri(&format!("/system/tasks/{}", task_id))
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["id"], task_id);
    assert_eq!(json["status"], "cancelled");

    // Verify the task is actually cancelled
    let task = task_queue.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.status, aeordb::engine::TaskStatus::Cancelled);
}

// ===========================================================================
// 5. test_get_task_by_id
// ===========================================================================

#[tokio::test]
async fn test_get_task_by_id() {
    let (app, jwt_manager, _engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let record = task_queue.enqueue("reindex", serde_json::json!({"path": "/docs/"})).unwrap();
    let task_id = record.id.clone();

    let request = Request::builder()
        .method("GET")
        .uri(&format!("/system/tasks/{}", task_id))
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["id"], task_id);
    assert_eq!(json["task_type"], "reindex");
    assert_eq!(json["status"], "pending");
    assert_eq!(json["args"]["path"], "/docs/");
}

// ===========================================================================
// 6. test_cron_create_list_delete
// ===========================================================================

#[tokio::test]
async fn test_cron_create_list_delete() {
    let (app, jwt_manager, engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Create a cron schedule
    let cron_body = serde_json::json!({
        "id": "nightly-reindex",
        "task_type": "reindex",
        "schedule": "0 2 * * *",
        "args": {"path": "/data/"},
        "enabled": true,
    });

    let request = Request::builder()
        .method("POST")
        .uri("/system/cron")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&cron_body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["id"], "nightly-reindex");
    assert_eq!(json["task_type"], "reindex");

    // List cron schedules
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("GET")
        .uri("/system/cron")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let schedules = json["items"].as_array().expect("should have items array");
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0]["id"], "nightly-reindex");

    // Delete the cron schedule
    let app3 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("DELETE")
        .uri("/system/cron/nightly-reindex")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app3.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["deleted"], true);

    // Verify it was deleted
    let app4 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("GET")
        .uri("/system/cron")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app4.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let schedules = json["items"].as_array().expect("should have items array");
    assert_eq!(schedules.len(), 0);
}

// ===========================================================================
// 7. test_task_endpoints_require_auth
// ===========================================================================

#[tokio::test]
async fn test_task_endpoints_require_auth() {
    let (app, jwt_manager, engine, task_queue, _temp_dir) = test_app();

    // No auth token -- should get 401
    let request = Request::builder()
        .method("GET")
        .uri("/system/tasks")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Same for POST /admin/tasks/reindex
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("POST")
        .uri("/system/tasks/reindex")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"path":"/data/"}"#))
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Same for POST /admin/cron
    let app3 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("GET")
        .uri("/system/cron")
        .body(Body::empty())
        .unwrap();

    let response = app3.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// 8. test_auto_trigger_on_indexes_json_store
// ===========================================================================

#[tokio::test]
async fn test_auto_trigger_on_indexes_json_store() {
    let (_app, jwt_manager, engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Ensure the parent directory exists
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/data/test.txt", b"hello", Some("text/plain")).unwrap();

    // Store indexes.json via the engine PUT endpoint
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let indexes_json = serde_json::json!({
        "fields": [
            {"field": "name", "type": "string"}
        ]
    });

    let request = Request::builder()
        .method("PUT")
        .uri("/files/data/.config/indexes.json")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&indexes_json).unwrap()))
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED, "store indexes.json should succeed");

    // Now check that a reindex task was auto-enqueued
    let tasks = task_queue.list_tasks().unwrap();
    let reindex_tasks: Vec<_> = tasks.iter()
        .filter(|t| t.task_type == "reindex")
        .collect();

    assert!(!reindex_tasks.is_empty(), "should have auto-enqueued a reindex task");

    // The reindex path should be /data
    let reindex_path = reindex_tasks[0].args.get("path").and_then(|v| v.as_str());
    assert_eq!(reindex_path, Some("/data"));
}

// ===========================================================================
// Additional tests for robustness
// ===========================================================================

#[tokio::test]
async fn test_get_nonexistent_task_returns_404() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/tasks/nonexistent-id")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_non_root_user_forbidden_for_tasks() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();

    // Non-root user
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
    let auth = format!("Bearer {}", token);

    let request = Request::builder()
        .method("GET")
        .uri("/system/tasks")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_cron_create_with_invalid_expression() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let cron_body = serde_json::json!({
        "id": "bad-cron",
        "task_type": "reindex",
        "schedule": "not a cron expression",
        "args": {"path": "/data/"},
        "enabled": true,
    });

    let request = Request::builder()
        .method("POST")
        .uri("/system/cron")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&cron_body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("Invalid cron expression"));
}

#[tokio::test]
async fn test_cron_update_enabled_flag() {
    let (app, jwt_manager, engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Create a cron schedule first
    let cron_body = serde_json::json!({
        "id": "test-cron",
        "task_type": "gc",
        "schedule": "0 3 * * *",
        "args": {"dry_run": false},
        "enabled": true,
    });

    let request = Request::builder()
        .method("POST")
        .uri("/system/cron")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&cron_body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Disable it via PATCH
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let update_body = serde_json::json!({"enabled": false});

    let request = Request::builder()
        .method("PATCH")
        .uri("/system/cron/test-cron")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&update_body).unwrap()))
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["enabled"], false);
    assert_eq!(json["id"], "test-cron");

    // Verify via list
    let app3 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("GET")
        .uri("/system/cron")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app3.oneshot(request).await.unwrap();
    let json = body_json(response.into_body()).await;
    let schedules = json["items"].as_array().unwrap();
    assert_eq!(schedules[0]["enabled"], false);
}

#[tokio::test]
async fn test_cron_delete_nonexistent_returns_404() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("DELETE")
        .uri("/system/cron/nonexistent")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cron_duplicate_id_returns_conflict() {
    let (app, jwt_manager, engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let cron_body = serde_json::json!({
        "id": "dup-cron",
        "task_type": "gc",
        "schedule": "0 4 * * *",
        "args": {},
        "enabled": true,
    });

    let request = Request::builder()
        .method("POST")
        .uri("/system/cron")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&cron_body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Try to create the same id again
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("POST")
        .uri("/system/cron")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&cron_body).unwrap()))
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_auto_trigger_cancels_existing_reindex() {
    let (_app, jwt_manager, engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Pre-enqueue a reindex for /data
    let first = task_queue.enqueue("reindex", serde_json::json!({"path": "/data"})).unwrap();
    let first_id = first.id.clone();

    // Ensure the parent directory exists
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/data/test.txt", b"hello", Some("text/plain")).unwrap();

    // Store indexes.json which should cancel the existing reindex and create a new one
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("PUT")
        .uri("/files/data/.config/indexes.json")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"fields":[]}"#))
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // The first task should be cancelled
    let first_task = task_queue.get_task(&first_id).unwrap().unwrap();
    assert_eq!(first_task.status, aeordb::engine::TaskStatus::Cancelled);

    // A new reindex task should exist
    let tasks = task_queue.list_tasks().unwrap();
    let active_reindex: Vec<_> = tasks.iter()
        .filter(|t| t.task_type == "reindex"
            && t.status == aeordb::engine::TaskStatus::Pending
            && t.args.get("path").and_then(|v| v.as_str()) == Some("/data"))
        .collect();
    assert_eq!(active_reindex.len(), 1, "should have exactly one pending reindex task");
}
