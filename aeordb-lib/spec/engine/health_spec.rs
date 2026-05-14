use std::sync::Arc;

use aeordb::auth::jwt::JwtManager;
use aeordb::engine::health::{
    check_auth, check_disk, check_engine, check_sync, compute_overall_status,
    full_health_check, AuthHealth, DiskHealth, EngineHealth, HealthChecks, HealthStatus,
    SyncHealth,
};
use aeordb::engine::peer_connection::{PeerConfig, PeerManager};
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::system_store;
use aeordb::engine::DirectoryOps;
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn store_signing_key(engine: &StorageEngine) {
    let manager = JwtManager::generate();
    let key_bytes = manager.to_bytes();
    let context = RequestContext::system();
    system_store::store_config(engine, &context, "jwt_signing_key", &key_bytes)
        .expect("failed to store signing key");
}

fn store_peer_configs(engine: &StorageEngine, peers: &[PeerConfig]) {
    let ctx = RequestContext::system();
    system_store::store_peer_configs(engine, &ctx, peers)
        .expect("failed to store peer configs");
}

fn make_peer_config(node_id: u64) -> PeerConfig {
    PeerConfig {
        node_id,
        address: format!("http://localhost:{}", 9000 + node_id),
        label: Some(format!("peer-{}", node_id)),
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    }
}

fn make_health_checks(
    engine: HealthStatus,
    disk: HealthStatus,
    sync: HealthStatus,
    auth: HealthStatus,
) -> HealthChecks {
    HealthChecks {
        engine: EngineHealth {
            status: engine,
            entry_count: 0,
            db_file_size_bytes: 0,
        },
        disk: DiskHealth {
            status: disk,
            available_bytes: 0,
            total_bytes: 0,
            usage_percent: 0.0,
        },
        sync: SyncHealth {
            status: sync,
            active_peers: 0,
            failing_peers: 0,
            details: None,
        },
        auth: AuthHealth {
            status: auth,
            mode: "standalone".to_string(),
            signing_key_present: true,
        },
    }
}

// ===========================================================================
// check_engine
// ===========================================================================

#[test]
fn test_engine_health_returns_healthy() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    let health = check_engine(&engine, db_path_str);
    assert_eq!(health.status, HealthStatus::Healthy);
    // Fresh engine should have a non-zero WAL file on disk.
    assert!(health.db_file_size_bytes > 0);
}

#[test]
fn test_engine_health_reflects_entries() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();

    // Store some data to increase entry count.
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test/data.json", b"{}", Some("application/json"))
        .unwrap();

    let health = check_engine(&engine, db_path_str);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert!(health.entry_count > 0);
    assert!(health.db_file_size_bytes > 0);
}

// ===========================================================================
// check_disk
// ===========================================================================

#[test]
fn test_disk_health_with_valid_path() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();

    let health = check_disk(db_path_str);
    // On Linux CI, we should get real values.
    #[cfg(target_os = "linux")]
    {
        assert!(health.total_bytes > 0);
        assert!(health.available_bytes > 0);
        assert!(health.usage_percent > 0.0);
        assert!(health.usage_percent < 100.0);
        // Typical dev/CI machine should be healthy.
        assert_eq!(health.status, HealthStatus::Healthy);
    }
}

#[test]
fn test_disk_health_fallback_with_invalid_path() {
    // A path with a null byte in it will fail the CString conversion,
    // triggering the fallback path.
    let health = check_disk("/nonexistent/path/\0with/null");
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.available_bytes, 0);
    assert_eq!(health.total_bytes, 0);
    assert_eq!(health.usage_percent, 0.0);
}

#[test]
fn test_disk_health_with_empty_path() {
    // Empty path should still not panic; falls back to "/" parent.
    let health = check_disk("");
    // On Linux, statvfs on "/" should succeed.
    #[cfg(target_os = "linux")]
    {
        assert!(health.total_bytes > 0);
    }
}

#[test]
fn test_disk_health_with_root_path() {
    let health = check_disk("/");
    #[cfg(target_os = "linux")]
    {
        assert!(health.total_bytes > 0);
        assert!(health.available_bytes > 0);
    }
}

// ===========================================================================
// check_sync
// ===========================================================================

#[test]
fn test_sync_health_no_peers() {
    let peer_manager = PeerManager::new();
    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.active_peers, 0);
    assert_eq!(health.failing_peers, 0);
    assert!(health.details.is_none());
}

#[test]
fn test_sync_health_active_peers_healthy() {
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1));
    peer_manager.activate_peer(1);
    peer_manager.add_peer(&make_peer_config(2));
    peer_manager.activate_peer(2);

    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.active_peers, 2);
    assert_eq!(health.failing_peers, 0);
    assert!(health.details.is_none());
}

#[test]
fn test_sync_health_failing_peers_degraded() {
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1));
    peer_manager.activate_peer(1);

    // Simulate >10 consecutive failures on peer 1.
    for _ in 0..11 {
        peer_manager.record_sync_failure(1, "connection refused".to_string());
    }

    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Degraded);
    assert_eq!(health.failing_peers, 1);
    assert!(health.details.is_some());
    assert!(health.details.unwrap().contains("1 peer(s)"));
}

#[test]
fn test_sync_health_exactly_10_failures_is_healthy() {
    // 10 failures is NOT >10, so still healthy.
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1));
    peer_manager.activate_peer(1);

    for _ in 0..10 {
        peer_manager.record_sync_failure(1, "timeout".to_string());
    }

    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.failing_peers, 0);
}

#[test]
fn test_sync_health_recovery_after_success() {
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1));

    // Accumulate 15 failures.
    for _ in 0..15 {
        peer_manager.record_sync_failure(1, "timeout".to_string());
    }
    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Degraded);

    // A single success resets consecutive_failures.
    peer_manager.record_sync_success(1);
    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.failing_peers, 0);
}

#[test]
fn test_sync_health_multiple_failing_peers() {
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1));
    peer_manager.add_peer(&make_peer_config(2));
    peer_manager.add_peer(&make_peer_config(3));

    // Peers 1 and 3 fail, peer 2 is fine.
    for _ in 0..11 {
        peer_manager.record_sync_failure(1, "err".to_string());
        peer_manager.record_sync_failure(3, "err".to_string());
    }

    let health = check_sync(&peer_manager);
    assert_eq!(health.status, HealthStatus::Degraded);
    assert_eq!(health.failing_peers, 2);
    assert!(health.details.unwrap().contains("2 peer(s)"));
}

#[test]
fn test_sync_health_disconnected_peer_not_active() {
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1)); // starts Disconnected

    let health = check_sync(&peer_manager);
    assert_eq!(health.active_peers, 0); // Disconnected, not Active
}

// ===========================================================================
// check_auth
// ===========================================================================

#[test]
fn test_auth_health_standalone_no_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    // Standalone mode, no signing key — still healthy.
    let health = check_auth(&engine);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.mode, "standalone");
    assert!(!health.signing_key_present);
}

#[test]
fn test_auth_health_standalone_with_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_signing_key(&engine);

    let health = check_auth(&engine);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.mode, "standalone");
    assert!(health.signing_key_present);
}

#[test]
fn test_auth_health_cluster_no_key_unhealthy() {
    let (engine, _temp) = create_temp_engine_for_tests();
    // Set up cluster mode (store peer configs).
    store_peer_configs(&engine, &[make_peer_config(2)]);

    let health = check_auth(&engine);
    assert_eq!(health.status, HealthStatus::Unhealthy);
    assert_eq!(health.mode, "cluster");
    assert!(!health.signing_key_present);
}

#[test]
fn test_auth_health_cluster_with_key_healthy() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_peer_configs(&engine, &[make_peer_config(2)]);
    store_signing_key(&engine);

    let health = check_auth(&engine);
    assert_eq!(health.status, HealthStatus::Healthy);
    assert_eq!(health.mode, "cluster");
    assert!(health.signing_key_present);
}

#[test]
fn test_auth_health_cluster_with_short_key_unhealthy() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_peer_configs(&engine, &[make_peer_config(2)]);

    // Store a key that's too short (31 bytes).
    let ctx = RequestContext::system();
    system_store::store_config(&engine, &ctx, "jwt_signing_key", &[0xFFu8; 31]).unwrap();

    let health = check_auth(&engine);
    assert_eq!(health.status, HealthStatus::Unhealthy);
    assert!(!health.signing_key_present);
}

// ===========================================================================
// compute_overall_status
// ===========================================================================

#[test]
fn test_overall_status_all_healthy() {
    let checks = make_health_checks(
        HealthStatus::Healthy,
        HealthStatus::Healthy,
        HealthStatus::Healthy,
        HealthStatus::Healthy,
    );
    assert_eq!(compute_overall_status(&checks), HealthStatus::Healthy);
}

#[test]
fn test_overall_status_one_degraded() {
    let checks = make_health_checks(
        HealthStatus::Healthy,
        HealthStatus::Degraded,
        HealthStatus::Healthy,
        HealthStatus::Healthy,
    );
    assert_eq!(compute_overall_status(&checks), HealthStatus::Degraded);
}

#[test]
fn test_overall_status_one_unhealthy() {
    let checks = make_health_checks(
        HealthStatus::Healthy,
        HealthStatus::Healthy,
        HealthStatus::Healthy,
        HealthStatus::Unhealthy,
    );
    assert_eq!(compute_overall_status(&checks), HealthStatus::Unhealthy);
}

#[test]
fn test_overall_status_unhealthy_trumps_degraded() {
    // If one is degraded and another is unhealthy, overall = unhealthy.
    let checks = make_health_checks(
        HealthStatus::Healthy,
        HealthStatus::Degraded,
        HealthStatus::Unhealthy,
        HealthStatus::Healthy,
    );
    assert_eq!(compute_overall_status(&checks), HealthStatus::Unhealthy);
}

#[test]
fn test_overall_status_multiple_degraded() {
    let checks = make_health_checks(
        HealthStatus::Degraded,
        HealthStatus::Degraded,
        HealthStatus::Healthy,
        HealthStatus::Healthy,
    );
    assert_eq!(compute_overall_status(&checks), HealthStatus::Degraded);
}

#[test]
fn test_overall_status_all_unhealthy() {
    let checks = make_health_checks(
        HealthStatus::Unhealthy,
        HealthStatus::Unhealthy,
        HealthStatus::Unhealthy,
        HealthStatus::Unhealthy,
    );
    assert_eq!(compute_overall_status(&checks), HealthStatus::Unhealthy);
}

// ===========================================================================
// full_health_check
// ===========================================================================

#[test]
fn test_full_health_check_standalone_fresh() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    let peer_manager = PeerManager::new();
    let startup_time = chrono::Utc::now().timestamp_millis() as u64;

    let report = full_health_check(&engine, db_path_str, &peer_manager, startup_time);

    assert_eq!(report.status, HealthStatus::Healthy);
    assert_eq!(report.checks.engine.status, HealthStatus::Healthy);
    assert_eq!(report.checks.sync.status, HealthStatus::Healthy);
    assert_eq!(report.checks.auth.status, HealthStatus::Healthy);
    assert_eq!(report.checks.auth.mode, "standalone");
    assert!(!report.version.is_empty());
    // Uptime should be close to 0 since startup was just now.
    assert!(report.uptime_seconds <= 2);
}

#[test]
fn test_full_health_check_cluster_no_key() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    store_peer_configs(&engine, &[make_peer_config(2)]);

    let peer_manager = PeerManager::new();
    let startup_time = chrono::Utc::now().timestamp_millis() as u64;

    let report = full_health_check(&engine, db_path_str, &peer_manager, startup_time);

    // Auth unhealthy should make overall unhealthy.
    assert_eq!(report.status, HealthStatus::Unhealthy);
    assert_eq!(report.checks.auth.status, HealthStatus::Unhealthy);
    assert_eq!(report.checks.auth.mode, "cluster");
}

#[test]
fn test_full_health_check_with_failing_peers() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    let peer_manager = PeerManager::new();
    peer_manager.add_peer(&make_peer_config(1));

    for _ in 0..15 {
        peer_manager.record_sync_failure(1, "conn refused".to_string());
    }

    let startup_time = chrono::Utc::now().timestamp_millis() as u64;
    let report = full_health_check(&engine, db_path_str, &peer_manager, startup_time);

    assert_eq!(report.status, HealthStatus::Degraded);
    assert_eq!(report.checks.sync.status, HealthStatus::Degraded);
    assert_eq!(report.checks.sync.failing_peers, 1);
}

#[test]
fn test_full_health_check_uptime_calculation() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    let peer_manager = PeerManager::new();

    // Fake a startup time 60 seconds ago.
    let startup_time =
        (chrono::Utc::now().timestamp_millis() as u64).saturating_sub(60_000);

    let report = full_health_check(&engine, db_path_str, &peer_manager, startup_time);
    // Uptime should be approximately 60 seconds (allow a small window for test execution).
    assert!(report.uptime_seconds >= 59);
    assert!(report.uptime_seconds <= 62);
}

#[test]
fn test_full_health_check_serializes_to_json() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    let peer_manager = PeerManager::new();
    let startup_time = chrono::Utc::now().timestamp_millis() as u64;

    let report = full_health_check(&engine, db_path_str, &peer_manager, startup_time);
    let json = serde_json::to_value(&report).expect("should serialize to JSON");

    // Verify key fields exist and have correct serde rename_all = lowercase.
    assert_eq!(json["status"], "healthy");
    assert!(json["checks"]["engine"]["entry_count"].is_number());
    assert!(json["checks"]["disk"]["usage_percent"].is_number());
    assert!(json["checks"]["sync"]["active_peers"].is_number());
    assert!(json["checks"]["auth"]["mode"].is_string());
    assert!(json["uptime_seconds"].is_number());
    assert!(json["version"].is_string());
}

#[test]
fn test_full_health_check_version_matches_crate_version() {
    let (engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap();
    let peer_manager = PeerManager::new();
    let startup_time = chrono::Utc::now().timestamp_millis() as u64;

    let report = full_health_check(&engine, db_path_str, &peer_manager, startup_time);
    // The version should be the crate version from Cargo.toml.
    assert!(!report.version.is_empty());
    // It should look like a semver (e.g. "0.1.0").
    assert!(report.version.contains('.'));
}

// ===========================================================================
// HTTP endpoint test
// ===========================================================================

#[tokio::test]
async fn test_health_endpoint_returns_full_report() {
    // L1 security fix: the public health endpoint only exposes {status, version}.
    // Detailed checks (engine, disk, sync, auth) are NOT returned via HTTP
    // to avoid leaking internal state. The full HealthReport is tested above
    // via direct full_health_check() calls.
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (engine, _temp) = create_temp_engine_for_tests();
    let jwt_manager = Arc::new(JwtManager::generate());
    let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager, engine);

    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

    // Public endpoint returns only status + version (L1 security fix)
    assert!(json["status"].is_string());
    assert_eq!(json["status"], "healthy");
    assert!(json["version"].is_string());

    // Detailed checks must NOT be exposed publicly
    assert!(json.get("checks").is_none(), "checks should not be in public health response");
    assert!(json.get("uptime_seconds").is_none(), "uptime should not be in public health response");
}

#[tokio::test]
async fn test_health_endpoint_no_auth_required() {
    // Health endpoint must be public (no Bearer token needed).
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (engine, _temp) = create_temp_engine_for_tests();
    let jwt_manager = Arc::new(JwtManager::generate());
    let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager, engine);

    // No Authorization header.
    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // Should succeed without auth.
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(json["status"], "healthy");
}

#[tokio::test]
async fn test_health_endpoint_cluster_mode_with_key_healthy() {
    // When peer configs are stored but a signing key exists (bootstrapped by
    // FileAuthProvider::new), auth should be healthy even in cluster mode.
    // L1 security fix: the HTTP endpoint only exposes {status, version},
    // so we verify the top-level status is "healthy" and that detailed checks
    // are NOT leaked. The full HealthReport (including auth mode/key checks)
    // is tested via direct full_health_check() calls above.
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (engine, _temp) = create_temp_engine_for_tests();
    store_peer_configs(&engine, &[make_peer_config(2)]);

    let jwt_manager = Arc::new(JwtManager::generate());
    let app = aeordb::server::create_app_with_jwt_and_engine(jwt_manager, engine);

    let request = Request::builder()
        .method("GET")
        .uri("/system/health")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

    // With a bootstrapped signing key, the overall status should be healthy
    assert_eq!(json["status"], "healthy");
    // Detailed checks must NOT be exposed publicly
    assert!(json.get("checks").is_none(), "checks should not be in public health response");
}
