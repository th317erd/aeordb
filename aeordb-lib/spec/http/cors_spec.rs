use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;
use aeordb::engine::StorageEngine;
use aeordb::server::{
    create_app_with_jwt_engine_and_cors, create_temp_engine_for_tests, CorsRule, CorsState,
};

// ===========================================================================
// Helpers
// ===========================================================================

fn test_app_with_cors(
    cors_state: CorsState,
) -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_engine_and_cors(jwt_manager.clone(), engine.clone(), cors_state);
    (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app_with_cors(
    jwt_manager: &Arc<JwtManager>,
    engine: &Arc<StorageEngine>,
    cors_state: CorsState,
) -> axum::Router {
    create_app_with_jwt_engine_and_cors(jwt_manager.clone(), engine.clone(), cors_state)
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

fn no_cors_state() -> CorsState {
    CorsState {
        default_origins: None,
        rules: vec![],
    }
}

fn wildcard_cors_state() -> CorsState {
    CorsState {
        default_origins: Some(vec!["*".to_string()]),
        rules: vec![],
    }
}

fn specific_origin_cors_state(origin: &str) -> CorsState {
    CorsState {
        default_origins: Some(vec![origin.to_string()]),
        rules: vec![],
    }
}

// ===========================================================================
// 1. No --cors flag: no CORS headers
// ===========================================================================

#[tokio::test]
async fn test_no_cors_flag_no_cors_headers() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app_with_cors(no_cors_state());
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://attacker.com")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // No CORS headers should be present
    assert!(
        response.headers().get("access-control-allow-origin").is_none(),
        "Expected no Access-Control-Allow-Origin header when CORS is disabled"
    );
}

// ===========================================================================
// 2. --cors "*": wildcard allows any origin
// ===========================================================================

#[tokio::test]
async fn test_cors_wildcard_allows_any_origin() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(wildcard_cors_state());

    // Use a public endpoint (health) so we don't need auth
    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://anything.example.com")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected Access-Control-Allow-Origin header");
    assert_eq!(allow_origin, "*");
}

// ===========================================================================
// 3. --cors "https://myapp.com": specific origin allowed
// ===========================================================================

#[tokio::test]
async fn test_cors_specific_origin_allowed() {
    let (app, _jwt_manager, _engine, _temp_dir) =
        test_app_with_cors(specific_origin_cors_state("https://myapp.com"));

    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://myapp.com")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected Access-Control-Allow-Origin header");
    assert_eq!(allow_origin, "https://myapp.com");
}

// ===========================================================================
// 4. --cors "https://myapp.com": different origin denied (no CORS headers)
// ===========================================================================

#[tokio::test]
async fn test_cors_specific_origin_denied() {
    let (app, _jwt_manager, _engine, _temp_dir) =
        test_app_with_cors(specific_origin_cors_state("https://myapp.com"));

    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://evil.com")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Origin not allowed -- no CORS headers
    assert!(
        response.headers().get("access-control-allow-origin").is_none(),
        "Expected no Access-Control-Allow-Origin header for disallowed origin"
    );
}

// ===========================================================================
// 5. OPTIONS preflight returns 204 with all CORS headers
// ===========================================================================

#[tokio::test]
async fn test_cors_preflight_options_returns_204() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(wildcard_cors_state());

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/files/test.txt")
        .header("origin", "https://myapp.com")
        .header("access-control-request-method", "PUT")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let hdrs = response.headers();
    assert_eq!(hdrs.get("access-control-allow-origin").unwrap(), "*");
    assert!(hdrs.get("access-control-allow-methods").is_some());
    assert!(hdrs.get("access-control-allow-headers").is_some());
    assert!(hdrs.get("access-control-max-age").is_some());
    assert_eq!(hdrs.get("access-control-max-age").unwrap(), "3600");
}

// ===========================================================================
// 6. Config file rule overrides CLI default for matching path
// ===========================================================================

#[tokio::test]
async fn test_cors_config_file_overrides_default() {
    // CLI default: allow https://default.com
    // Config rule: /engine/* allows only https://special.com
    let cors_state = CorsState {
        default_origins: Some(vec!["https://default.com".to_string()]),
        rules: vec![CorsRule {
            path: "/files/*".to_string(),
            origins: vec!["https://special.com".to_string()],
            methods: vec!["GET".to_string(), "PUT".to_string()],
            allow_headers: vec!["Content-Type".to_string()],
            max_age: 600,
            allow_credentials: false,
        }],
    };

    let (app, jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state.clone());
    let auth = root_bearer_token(&jwt_manager);

    // Request to /engine/* from special.com -- should be allowed by config rule
    let request = Request::builder()
        .method("GET")
        .uri("/files/test.txt")
        .header("origin", "https://special.com")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected CORS header for matching config rule");
    assert_eq!(allow_origin, "https://special.com");

    // Request to /admin/health from default.com -- should use CLI default
    let app2 = rebuild_app_with_cors(&jwt_manager, &_engine, cors_state.clone());
    let request2 = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://default.com")
        .body(Body::empty())
        .unwrap();

    let response2 = app2.oneshot(request2).await.unwrap();
    let allow_origin2 = response2
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected CORS header from CLI default for non-matching path");
    assert_eq!(allow_origin2, "https://default.com");

    // Request to /engine/* from default.com -- should be DENIED by config rule
    // (config rule only allows special.com)
    let app3 = rebuild_app_with_cors(&jwt_manager, &_engine, cors_state);
    let request3 = Request::builder()
        .method("GET")
        .uri("/files/test.txt")
        .header("origin", "https://default.com")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response3 = app3.oneshot(request3).await.unwrap();
    assert!(
        response3
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "Config rule should deny default.com on /engine/* path"
    );
}

// ===========================================================================
// 7. Config file path matching: /engine/* matches /engine/files/test.txt
// ===========================================================================

#[tokio::test]
async fn test_cors_config_file_path_matching() {
    let cors_state = CorsState {
        default_origins: None,
        rules: vec![CorsRule {
            path: "/files/*".to_string(),
            origins: vec!["https://myapp.com".to_string()],
            methods: vec!["GET".to_string()],
            allow_headers: vec!["Content-Type".to_string()],
            max_age: 3600,
            allow_credentials: false,
        }],
    };

    let (app, jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state);
    let auth = root_bearer_token(&jwt_manager);

    // Deep nested path should match /engine/*
    let request = Request::builder()
        .method("GET")
        .uri("/files/files/nested/deep/test.txt")
        .header("origin", "https://myapp.com")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected CORS header for nested path matching /engine/*");
    assert_eq!(allow_origin, "https://myapp.com");
}

// ===========================================================================
// 8. allow_credentials: true includes the header
// ===========================================================================

#[tokio::test]
async fn test_cors_credentials_header() {
    let cors_state = CorsState {
        default_origins: None,
        rules: vec![CorsRule {
            path: "/system/*".to_string(),
            origins: vec!["https://admin.myapp.com".to_string()],
            methods: vec!["GET".to_string(), "POST".to_string()],
            allow_headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            max_age: 3600,
            allow_credentials: true,
        }],
    };

    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state.clone());

    // Normal request
    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://admin.myapp.com")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-credentials")
            .unwrap(),
        "true"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://admin.myapp.com"
    );

    // Preflight request
    let app2 = rebuild_app_with_cors(&_jwt_manager, &_engine, cors_state);
    let request2 = Request::builder()
        .method("OPTIONS")
        .uri("/system/health")
        .header("origin", "https://admin.myapp.com")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();

    let response2 = app2.oneshot(request2).await.unwrap();
    assert_eq!(response2.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response2
            .headers()
            .get("access-control-allow-credentials")
            .unwrap(),
        "true"
    );
}

// ===========================================================================
// 9. Preflight from disallowed origin returns 403
// ===========================================================================

#[tokio::test]
async fn test_cors_preflight_disallowed_origin_returns_403() {
    let (app, _jwt_manager, _engine, _temp_dir) =
        test_app_with_cors(specific_origin_cors_state("https://myapp.com"));

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/system/health")
        .header("origin", "https://evil.com")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// 10. No origin header: CORS headers still set for wildcard (browser-less request)
// ===========================================================================

#[tokio::test]
async fn test_cors_no_origin_header_wildcard() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(wildcard_cors_state());

    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // With wildcard, even without origin header, CORS headers should be present
    // (origin is empty but "*" matches everything)
    let allow_origin = response
        .headers()
        .get("access-control-allow-origin")
        .expect("Expected CORS header even without Origin for wildcard");
    assert_eq!(allow_origin, "*");
}

// ===========================================================================
// 11. Multiple allowed origins: correct origin reflected
// ===========================================================================

#[tokio::test]
async fn test_cors_multiple_origins() {
    let cors_state = CorsState {
        default_origins: Some(vec![
            "https://app1.com".to_string(),
            "https://app2.com".to_string(),
        ]),
        rules: vec![],
    };

    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state.clone());

    // First origin
    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://app1.com")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app1.com"
    );

    // Second origin
    let app2 = rebuild_app_with_cors(&_jwt_manager, &_engine, cors_state.clone());
    let request2 = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://app2.com")
        .body(Body::empty())
        .unwrap();

    let response2 = app2.oneshot(request2).await.unwrap();
    assert_eq!(
        response2
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app2.com"
    );

    // Third origin -- not allowed
    let app3 = rebuild_app_with_cors(&_jwt_manager, &_engine, cors_state);
    let request3 = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://app3.com")
        .body(Body::empty())
        .unwrap();

    let response3 = app3.oneshot(request3).await.unwrap();
    assert!(response3
        .headers()
        .get("access-control-allow-origin")
        .is_none());
}

// ===========================================================================
// 12. Preflight with config rule returns rule-specific methods and headers
// ===========================================================================

#[tokio::test]
async fn test_cors_preflight_uses_config_rule_values() {
    let cors_state = CorsState {
        default_origins: Some(vec!["*".to_string()]),
        rules: vec![CorsRule {
            path: "/files/*".to_string(),
            origins: vec!["https://myapp.com".to_string()],
            methods: vec!["GET".to_string(), "PUT".to_string()],
            allow_headers: vec!["X-Custom-Header".to_string()],
            max_age: 1800,
            allow_credentials: false,
        }],
    };

    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state);

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/files/test.txt")
        .header("origin", "https://myapp.com")
        .header("access-control-request-method", "PUT")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let hdrs = response.headers();
    assert_eq!(hdrs.get("access-control-allow-methods").unwrap(), "GET, PUT");
    assert_eq!(
        hdrs.get("access-control-allow-headers").unwrap(),
        "X-Custom-Header"
    );
    assert_eq!(hdrs.get("access-control-max-age").unwrap(), "1800");
    // Credentials not set
    assert!(hdrs.get("access-control-allow-credentials").is_none());
}

// ===========================================================================
// 13. Exact path matching (no wildcard)
// ===========================================================================

#[tokio::test]
async fn test_cors_exact_path_matching() {
    let cors_state = CorsState {
        default_origins: None,
        rules: vec![CorsRule {
            path: "/system/health".to_string(),
            origins: vec!["https://monitor.com".to_string()],
            methods: vec!["GET".to_string()],
            allow_headers: vec!["Content-Type".to_string()],
            max_age: 3600,
            allow_credentials: false,
        }],
    };

    let (app, _jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state.clone());

    // Exact match
    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .header("origin", "https://monitor.com")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert!(response
        .headers()
        .get("access-control-allow-origin")
        .is_some());

    // Non-matching path -- no default, so no CORS
    let app2 = rebuild_app_with_cors(&_jwt_manager, &_engine, cors_state);
    let request2 = Request::builder()
        .method("GET")
        .uri("/auth/token")
        .header("origin", "https://monitor.com")
        .body(Body::empty())
        .unwrap();

    let response2 = app2.oneshot(request2).await.unwrap();
    assert!(response2
        .headers()
        .get("access-control-allow-origin")
        .is_none());
}

// ===========================================================================
// 14. First rule wins when multiple rules match
// ===========================================================================

#[tokio::test]
async fn test_cors_first_rule_wins() {
    let cors_state = CorsState {
        default_origins: None,
        rules: vec![
            CorsRule {
                path: "/files/*".to_string(),
                origins: vec!["https://first.com".to_string()],
                methods: vec!["GET".to_string()],
                allow_headers: vec!["Content-Type".to_string()],
                max_age: 100,
                allow_credentials: false,
            },
            CorsRule {
                path: "/files/*".to_string(),
                origins: vec!["https://second.com".to_string()],
                methods: vec!["POST".to_string()],
                allow_headers: vec!["Authorization".to_string()],
                max_age: 200,
                allow_credentials: true,
            },
        ],
    };

    let (app, jwt_manager, _engine, _temp_dir) = test_app_with_cors(cors_state);
    let auth = root_bearer_token(&jwt_manager);

    // https://first.com should be allowed (first rule)
    let request = Request::builder()
        .method("GET")
        .uri("/files/test.txt")
        .header("origin", "https://first.com")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://first.com"
    );
}

// ===========================================================================
// 15. load_cors_config from engine (integration)
// ===========================================================================

#[tokio::test]
async fn test_load_cors_config_from_engine() {
    let (engine, _temp_dir) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    let config_json = r#"{
        "rules": [
            {
                "path": "/files/*",
                "origins": ["https://myapp.com"],
                "methods": ["GET", "PUT"],
                "max_age": 600
            },
            {
                "path": "/admin/*",
                "origins": ["https://admin.myapp.com"],
                "allow_credentials": true
            }
        ]
    }"#;

    ops.store_file(&ctx, "/.aeordb-config/cors.json", config_json.as_bytes(), Some("application/json"))
        .unwrap();

    let rules = aeordb::server::load_cors_config(&engine);
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].path, "/files/*");
    assert_eq!(rules[0].origins, vec!["https://myapp.com"]);
    assert_eq!(rules[0].methods, vec!["GET", "PUT"]);
    assert_eq!(rules[0].max_age, 600);
    assert!(!rules[0].allow_credentials);

    assert_eq!(rules[1].path, "/admin/*");
    assert_eq!(rules[1].origins, vec!["https://admin.myapp.com"]);
    // Defaults should be applied
    assert_eq!(rules[1].methods.len(), 6); // default methods
    assert_eq!(rules[1].allow_headers.len(), 2); // default headers
    assert_eq!(rules[1].max_age, 3600); // default max_age
    assert!(rules[1].allow_credentials);
}

// ===========================================================================
// 16. load_cors_config missing file returns empty
// ===========================================================================

#[tokio::test]
async fn test_load_cors_config_missing_file() {
    let (engine, _temp_dir) = create_temp_engine_for_tests();
    let rules = aeordb::server::load_cors_config(&engine);
    assert!(rules.is_empty());
}

// ===========================================================================
// 17. load_cors_config with invalid JSON returns empty (with warning)
// ===========================================================================

#[tokio::test]
async fn test_load_cors_config_invalid_json() {
    let (engine, _temp_dir) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    ops.store_file(&ctx, "/.aeordb-config/cors.json", b"not valid json", Some("application/json"))
        .unwrap();

    let rules = aeordb::server::load_cors_config(&engine);
    assert!(rules.is_empty());
}

// ===========================================================================
// 18. build_cors_state wires CLI + config file together
// ===========================================================================

#[tokio::test]
async fn test_build_cors_state() {
    let (engine, _temp_dir) = create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    let config_json = r#"{"rules": [{"path": "/api/*", "origins": ["https://api.com"]}]}"#;
    ops.store_file(&ctx, "/.aeordb-config/cors.json", config_json.as_bytes(), Some("application/json"))
        .unwrap();

    let state = aeordb::server::build_cors_state(Some("https://default.com"), &engine);
    assert_eq!(
        state.default_origins,
        Some(vec!["https://default.com".to_string()])
    );
    assert_eq!(state.rules.len(), 1);
    assert_eq!(state.rules[0].path, "/api/*");
}

// ===========================================================================
// 19. build_cors_state with no flag and no file
// ===========================================================================

#[tokio::test]
async fn test_build_cors_state_no_cors() {
    let (engine, _temp_dir) = create_temp_engine_for_tests();
    let state = aeordb::server::build_cors_state(None, &engine);
    assert!(state.default_origins.is_none());
    assert!(state.rules.is_empty());
}

// ===========================================================================
// 20. parse_cors_origins
// ===========================================================================

#[test]
fn test_parse_cors_origins_wildcard() {
    let origins = aeordb::server::parse_cors_origins("*");
    assert_eq!(origins, vec!["*"]);
}

#[test]
fn test_parse_cors_origins_comma_separated() {
    let origins = aeordb::server::parse_cors_origins("https://a.com,https://b.com");
    assert_eq!(origins, vec!["https://a.com", "https://b.com"]);
}

#[test]
fn test_parse_cors_origins_with_whitespace() {
    let origins = aeordb::server::parse_cors_origins("  https://a.com , https://b.com  ");
    assert_eq!(origins, vec!["https://a.com", "https://b.com"]);
}

#[test]
fn test_parse_cors_origins_empty() {
    let origins = aeordb::server::parse_cors_origins("");
    assert!(origins.is_empty());
}
