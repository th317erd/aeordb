use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::StorageEngine;
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
    (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(
    jwt_manager: &Arc<JwtManager>,
    engine: &Arc<StorageEngine>,
) -> axum::Router {
    create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
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
    };
    let token = jwt_manager.create_token(&claims).expect("create token");
    format!("Bearer {}", token)
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    let config_path = if parent_path.ends_with('/') {
        format!("{}.config/indexes.json", parent_path)
    } else {
        format!("{}/.config/indexes.json", parent_path)
    };
    let config_data = config.serialize();
    ops.store_file(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

fn make_person_json(age: u64, department: &str, salary: u64) -> Vec<u8> {
    format!(
        r#"{{"age":{},"department":"{}","salary":{}}}"#,
        age, department, salary,
    ).into_bytes()
}

fn setup_people(engine: &StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);

    let config = PathIndexConfig {
        parser: None,
        parser_memory_limit: None,
        logging: false,
        indexes: vec![
            IndexFieldConfig {
                name: "age".to_string(),
                index_type: "u64".to_string(),
                source: None,
                min: Some(0.0),
                max: Some(200.0),
            },
            IndexFieldConfig {
                name: "department".to_string(),
                index_type: "string".to_string(),
                source: None,
                min: None,
                max: None,
            },
            IndexFieldConfig {
                name: "salary".to_string(),
                index_type: "u64".to_string(),
                source: None,
                min: Some(0.0),
                max: Some(200000.0),
            },
        ],
    };
    store_index_config(engine, "/people", &config);

    for i in 0..10u64 {
        let age = 20 + i;
        let department = if i % 2 == 0 { "engineering" } else { "sales" };
        let salary = 50000 + i * 5000;
        let path = format!("/people/person_{:02}.json", i);
        let data = make_person_json(age, department, salary);
        ops.store_file_with_indexing(&ctx, &path, &data, Some("application/json")).unwrap();
    }
}

// ============================================================================
// 1. test_aggregate_count_http
// ============================================================================
#[tokio::test]
async fn test_aggregate_count_http() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "count": true
        }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["count"], serde_json::json!(10));
    assert_eq!(json["has_more"], serde_json::json!(false));
}

// ============================================================================
// 2. test_aggregate_sum_http
// ============================================================================
#[tokio::test]
async fn test_aggregate_sum_http() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "sum": ["salary"]
        }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    // salary = 50000 + i*5000 for i in 0..10
    // sum = 10*50000 + 5000*(0+1+...+9) = 500000 + 225000 = 725000
    let sum = json["sum"]["salary"].as_f64().unwrap();
    assert_eq!(sum, 725_000.0);
}

// ============================================================================
// 3. test_aggregate_group_by_http
// ============================================================================
#[tokio::test]
async fn test_aggregate_group_by_http() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "count": true,
            "group_by": ["department"]
        }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let groups = json["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "should have 2 groups");

    // Verify each group has a key and count
    for group in groups {
        assert!(group["key"]["department"].is_string());
        assert!(group["count"].is_u64());
        assert_eq!(group["count"].as_u64().unwrap(), 5);
    }
}

// ============================================================================
// 4. test_aggregate_no_field_returns_error
// ============================================================================
#[tokio::test]
async fn test_aggregate_no_field_returns_error() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "sum": ["nonexistent_field"]
        }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(error_msg.contains("No index found"), "error should mention missing index: {}", error_msg);
}

// ============================================================================
// 5. test_aggregate_response_shape
// ============================================================================
#[tokio::test]
async fn test_aggregate_response_shape() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "count": true,
            "sum": ["salary"],
            "avg": ["age"],
            "min": ["age"],
            "max": ["salary"]
        }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;

    // Verify response structure
    assert!(json["count"].is_u64(), "count should be a number");
    assert_eq!(json["count"].as_u64().unwrap(), 10);

    assert!(json["sum"].is_object(), "sum should be an object");
    assert!(json["sum"]["salary"].is_f64() || json["sum"]["salary"].is_u64(), "sum.salary should be numeric");

    assert!(json["avg"].is_object(), "avg should be an object");
    assert!(json["avg"]["age"].is_f64() || json["avg"]["age"].is_u64(), "avg.age should be numeric");

    assert!(json["min"].is_object(), "min should be an object");
    assert!(json["min"]["age"].is_number(), "min.age should be a number");

    assert!(json["max"].is_object(), "max should be an object");
    assert!(json["max"]["salary"].is_number(), "max.salary should be a number");

    assert_eq!(json["has_more"], serde_json::json!(false));

    // No groups when group_by is not specified -- groups key should not be present
    assert!(json.get("groups").is_none(), "groups should not be in response without group_by");
}

// ============================================================================
// Additional HTTP tests
// ============================================================================

#[tokio::test]
async fn test_aggregate_sum_on_string_returns_error() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "sum": ["department"]
        }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_aggregate_group_by_with_limit() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 },
        "aggregate": {
            "count": true,
            "group_by": ["age"]
        },
        "limit": 3
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let groups = json["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 3, "should be limited to 3 groups");
    assert_eq!(json["has_more"], serde_json::json!(true));
}

#[tokio::test]
async fn test_regular_query_still_works_without_aggregate() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_people(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    // Normal query without aggregate should still return documents
    let body = serde_json::json!({
        "path": "/people",
        "where": { "field": "age", "op": "gt", "value": 0 }
    });

    let request = Request::builder()
        .method("POST")
        .uri("/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    // Regular query returns "results" array, not aggregation
    assert!(json["results"].is_array(), "regular query should return results array");
    assert_eq!(json["results"].as_array().unwrap().len(), 10);
}
