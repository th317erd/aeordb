use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};

use aeordb_cli::commands::status::{fetch_status, render_human_status, StatusRequest};

async fn spawn_server(router: Router) -> String {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
  format!("http://{address}")
}

fn stats() -> serde_json::Value {
  serde_json::json!({
    "identity": {"version": "0.9.5", "database_path": "test.aeordb", "uptime_seconds": 12},
    "memory": {
      "process": {"rss_bytes": 1048576, "private_bytes": 524288},
      "coordinator": {"pressure": "normal", "accounted_bytes": 786432, "unaccounted_rss_bytes": 262144, "owners": []}
    },
    "durability": {
      "frontier": {"hard_frontier": 7, "waiter_depth": 0},
      "latch": {"read_only": false},
      "spill": {"count": 0, "total_bytes": 0},
      "repair": {"required": false, "state": "not_required"}
    },
    "configuration": {
      "runtime": {"status": {"valid": true, "degraded": false, "pending_restart": {}, "pending_convergence": {}, "disabled_capabilities": []}},
      "lifecycle": {"status": {"valid": true, "degraded": false, "pending_restart": {}, "pending_convergence": {}, "disabled_capabilities": []}}
    }
  })
}

#[tokio::test]
async fn status_fetch_supports_direct_bearer_tokens() {
  let router = Router::new().route(
    "/system/stats",
    get(|request: Request<Body>| async move {
      if request.headers().get("authorization").and_then(|value| value.to_str().ok()) != Some("Bearer direct-token") {
        return StatusCode::UNAUTHORIZED.into_response();
      }
      Json(stats()).into_response()
    }),
  );
  let target = spawn_server(router).await;
  let response = fetch_status(StatusRequest { target, api_key: None, token: Some("direct-token".to_string()) }).await.unwrap();
  assert_eq!(response["durability"]["frontier"]["hard_frontier"], 7);
}

#[tokio::test]
async fn status_fetch_exchanges_api_keys_without_echoing_them() {
  let router = Router::new()
    .route(
      "/auth/token",
      post(|Json(body): Json<serde_json::Value>| async move {
        if body["api_key"] == "root-secret" {
          Json(serde_json::json!({"token": "exchanged-token"})).into_response()
        } else {
          StatusCode::UNAUTHORIZED.into_response()
        }
      }),
    )
    .route(
      "/system/stats",
      get(|request: Request<Body>| async move {
        if request.headers().get("authorization").and_then(|value| value.to_str().ok()) == Some("Bearer exchanged-token") {
          Json(stats()).into_response()
        } else {
          StatusCode::UNAUTHORIZED.into_response()
        }
      }),
    );
  let target = spawn_server(router).await;
  let response = fetch_status(StatusRequest { target, api_key: Some("root-secret".to_string()), token: None }).await.unwrap();
  assert_eq!(response["identity"]["database_path"], "test.aeordb");
}

#[tokio::test]
async fn status_fetch_redacts_an_api_key_echoed_by_a_failed_token_endpoint() {
  let router = Router::new().route(
    "/auth/token",
    post(|Json(body): Json<serde_json::Value>| async move {
      (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
          "error": format!("rejected credential {}", body["api_key"].as_str().unwrap_or_default()),
        })),
      )
    }),
  );
  let target = spawn_server(router).await;
  let api_key = "root-secret-must-never-be-printed";

  let error = fetch_status(StatusRequest { target, api_key: Some(api_key.to_string()), token: None }).await.unwrap_err();

  assert!(error.contains("401"), "status code was lost while redacting the token response: {error}");
  assert!(!error.contains(api_key), "status exposed the root API key in a token-endpoint error: {error}");
}

#[tokio::test]
async fn status_fetch_redacts_a_bearer_token_echoed_by_a_failed_stats_endpoint() {
  let router = Router::new().route(
    "/system/stats",
    get(|request: Request<Body>| async move {
      let authorization = request.headers().get("authorization").and_then(|value| value.to_str().ok()).unwrap_or_default();
      (StatusCode::SERVICE_UNAVAILABLE, format!("diagnostic reflected {authorization}"))
    }),
  );
  let target = spawn_server(router).await;
  let token = "bearer-secret-must-never-be-printed";

  let error = fetch_status(StatusRequest { target, api_key: None, token: Some(token.to_string()) }).await.unwrap_err();

  assert!(error.contains("503"), "status code was lost while redacting the stats response: {error}");
  assert!(error.contains("diagnostic reflected"), "non-secret status diagnostics were discarded: {error}");
  assert!(!error.contains(token), "status exposed the bearer token in a stats-endpoint error: {error}");
}

#[tokio::test]
async fn status_fetch_never_forwards_credentials_through_redirects() {
  let redirected_requests = Arc::new(AtomicUsize::new(0));
  let redirected_requests_for_route = Arc::clone(&redirected_requests);
  let redirect_target = spawn_server(Router::new().route(
    "/capture",
    post(move || {
      let redirected_requests = Arc::clone(&redirected_requests_for_route);
      async move {
        redirected_requests.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({"token": "redirected-token"}))
      }
    }),
  ))
  .await;
  let redirect_location = format!("{redirect_target}/capture");
  let target = spawn_server(
    Router::new()
      .route(
        "/auth/token",
        post(move || {
          let redirect_location = redirect_location.clone();
          async move { Redirect::temporary(&redirect_location) }
        }),
      )
      .route("/system/stats", get(|| async { Json(stats()) })),
  )
  .await;

  let error = fetch_status(StatusRequest { target, api_key: Some("root-secret".to_string()), token: None }).await.unwrap_err();
  assert!(error.contains("307"), "unexpected redirect error: {error}");
  assert_eq!(redirected_requests.load(Ordering::SeqCst), 0, "status forwarded the API-key request to a redirected origin");
}

#[tokio::test]
async fn status_fetch_surfaces_auth_malformed_and_unreachable_failures() {
  let unauthorized = spawn_server(Router::new().route("/system/stats", get(|| async { StatusCode::UNAUTHORIZED }))).await;
  let error = fetch_status(StatusRequest { target: unauthorized, api_key: None, token: None }).await.unwrap_err();
  assert!(error.contains("401"));

  let malformed = spawn_server(Router::new().route("/system/stats", get(|| async { "<html>not-json</html>" }))).await;
  let error = fetch_status(StatusRequest { target: malformed, api_key: None, token: None }).await.unwrap_err();
  assert!(error.contains("valid JSON"));

  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let target = format!("http://{}", listener.local_addr().unwrap());
  drop(listener);
  let error = fetch_status(StatusRequest { target, api_key: None, token: None }).await.unwrap_err();
  assert!(error.contains("could not reach"));
}

#[tokio::test]
async fn status_fetch_rejects_credential_conflicts_invalid_targets_and_missing_tokens() {
  let error = fetch_status(StatusRequest {
    target: "http://127.0.0.1:6830".to_string(),
    api_key: Some("api-key".to_string()),
    token: Some("token".to_string()),
  })
  .await
  .unwrap_err();
  assert!(error.contains("cannot be used together"));

  let error = fetch_status(StatusRequest { target: "file:///tmp/database".to_string(), api_key: None, token: None }).await.unwrap_err();
  assert!(error.contains("http or https"));

  let target =
    spawn_server(Router::new().route("/auth/token", post(|| async { Json(serde_json::json!({"unexpected": "response"})) }))).await;
  let error = fetch_status(StatusRequest { target, api_key: Some("api-key".to_string()), token: None }).await.unwrap_err();
  assert!(error.contains("did not contain a token"));
}

#[tokio::test]
async fn status_fetch_rejects_oversized_responses_before_buffering_them() {
  let router = Router::new().route(
    "/system/stats",
    get(|| async {
      let oversized = vec![b'x'; 4 * 1024 * 1024 + 1];
      axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-length", oversized.len().to_string())
        .body(Body::from(oversized))
        .unwrap()
    }),
  );
  let target = spawn_server(router).await;
  let error = fetch_status(StatusRequest { target, api_key: None, token: None }).await.unwrap_err();
  assert!(error.contains("4194304-byte diagnostic limit"), "unexpected oversized-response error: {error}");
}

#[test]
fn human_status_contains_operational_decisions_without_dumping_json() {
  let output = render_human_status(&stats()).unwrap();
  assert!(output.contains("AeorDB 0.9.5"));
  assert!(output.contains("Memory: 1.00 MiB RSS"));
  assert!(output.contains("pressure normal"));
  assert!(output.contains("Durability: writable"));
  assert!(output.contains("Configuration: runtime valid, lifecycle valid"));
  assert!(!output.contains("{\"identity\""));
}
