use std::sync::Arc;

use aeordb::auth::jwt::JwtManager;
use aeordb::engine::cluster_join::{get_cluster_mode, has_signing_key, is_ready_for_traffic};
use aeordb::engine::peer_connection::{PeerConfig, PeerManager};
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::sync_engine::{SyncConfig, SyncEngine};
use aeordb::engine::system_store;
use aeordb::engine::virtual_clock::PeerClockTracker;
use aeordb::engine::DirectoryOps;
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn store_signing_key(engine: &StorageEngine) -> Vec<u8> {
    let manager = JwtManager::generate();
    let key_bytes = manager.to_bytes();
    let context = RequestContext::system();

    system_store::store_config(engine, &context, "jwt_signing_key", &key_bytes)
        .expect("failed to store signing key");
    key_bytes
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

fn make_sync_engine(engine: Arc<StorageEngine>) -> (SyncEngine, Arc<PeerManager>) {
    let peer_manager = Arc::new(PeerManager::new());
    let clock_tracker = Arc::new(PeerClockTracker::new(30_000));
    let config = SyncConfig {
        periodic_interval_secs: 30,
    };
    let sync_engine = SyncEngine::new(
        engine,
        Arc::clone(&peer_manager),
        Arc::clone(&clock_tracker),
        config,
    );
    (sync_engine, peer_manager)
}

fn add_active_peer(peer_manager: &PeerManager, node_id: u64) {
    peer_manager.add_peer(&PeerConfig {
        node_id,
        address: format!("http://localhost:{}", 9000 + node_id),
        label: Some(format!("peer-{}", node_id)),
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    });
    peer_manager.start_honeymoon(node_id, 1000);
    peer_manager.activate_peer(node_id);
}

fn store_file(engine: &StorageEngine, path: &str, data: &[u8]) {
    let context = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&context, path, data, Some("text/plain"))
        .unwrap();
}

/// Simulate the system-table portion of a join sync by copying the signing key
/// from one engine to another. In production, this happens when system table
/// entries are synced alongside file data. This helper isolates the cluster_join
/// logic from the sync engine's current limitation (it only syncs directory
/// tree data, not loose system table KV entries).
fn simulate_signing_key_sync(source: &StorageEngine, destination: &StorageEngine) {

    if let Ok(Some(key_bytes)) = system_store::get_config(source, "jwt_signing_key") {
        let context = RequestContext::system();

        system_store::store_config(destination, &context, "jwt_signing_key", &key_bytes)
            .expect("failed to sync signing key to destination");
    }
}

// ===========================================================================
// has_signing_key
// ===========================================================================

#[test]
fn test_has_signing_key_present() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_signing_key(&engine);
    assert!(has_signing_key(&engine));
}

#[test]
fn test_has_signing_key_absent() {
    let (engine, _temp) = create_temp_engine_for_tests();
    assert!(!has_signing_key(&engine));
}

#[test]
fn test_has_signing_key_too_short() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    system_store::store_config(&engine, &context, "jwt_signing_key", &[0u8; 16])
        .unwrap();
    assert!(!has_signing_key(&engine));
}

#[test]
fn test_has_signing_key_empty() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    system_store::store_config(&engine, &context, "jwt_signing_key", &[])
        .unwrap();
    assert!(!has_signing_key(&engine));
}

#[test]
fn test_has_signing_key_exactly_32_bytes() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    system_store::store_config(&engine, &context, "jwt_signing_key", &[0xABu8; 32])
        .unwrap();
    assert!(has_signing_key(&engine));
}

#[test]
fn test_has_signing_key_31_bytes_rejected() {
    // 31 bytes is one byte short of the minimum.
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    system_store::store_config(&engine, &context, "jwt_signing_key", &[0xFFu8; 31])
        .unwrap();
    assert!(!has_signing_key(&engine));
}

#[test]
fn test_has_signing_key_wrong_config_key() {
    // A key stored under a different config name should not be found.
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    system_store::store_config(&engine, &context, "not_jwt_signing_key", &[0xABu8; 32])
        .unwrap();
    assert!(!has_signing_key(&engine));
}

// ===========================================================================
// is_ready_for_traffic
// ===========================================================================

#[test]
fn test_is_ready_standalone_always_true() {
    let (engine, _temp) = create_temp_engine_for_tests();
    assert!(is_ready_for_traffic(&engine, false));
}

#[test]
fn test_is_ready_standalone_with_key_also_true() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_signing_key(&engine);
    assert!(is_ready_for_traffic(&engine, false));
}

#[test]
fn test_is_ready_cluster_no_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    assert!(!is_ready_for_traffic(&engine, true));
}

#[test]
fn test_is_ready_cluster_with_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_signing_key(&engine);
    assert!(is_ready_for_traffic(&engine, true));
}

#[test]
fn test_is_ready_cluster_with_short_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    system_store::store_config(&engine, &context, "jwt_signing_key", &[0u8; 31])
        .unwrap();
    assert!(!is_ready_for_traffic(&engine, true));
}

// ===========================================================================
// get_cluster_mode
// ===========================================================================

#[test]
fn test_get_cluster_mode_standalone() {
    let (engine, _temp) = create_temp_engine_for_tests();
    assert_eq!(get_cluster_mode(&engine), "standalone");
}

#[test]
fn test_get_cluster_mode_cluster() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_peer_configs(&engine, &[make_peer_config(2)]);
    assert_eq!(get_cluster_mode(&engine), "cluster");
}

#[test]
fn test_get_cluster_mode_empty_peers_is_standalone() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_peer_configs(&engine, &[]);
    assert_eq!(get_cluster_mode(&engine), "standalone");
}

#[test]
fn test_get_cluster_mode_multiple_peers() {
    let (engine, _temp) = create_temp_engine_for_tests();
    store_peer_configs(
        &engine,
        &[make_peer_config(2), make_peer_config(3), make_peer_config(4)],
    );
    assert_eq!(get_cluster_mode(&engine), "cluster");
}

#[test]
fn test_get_cluster_mode_independent_of_signing_key() {
    // Cluster mode is determined by peer configs, not signing key presence.
    let (engine, _temp) = create_temp_engine_for_tests();

    // Has signing key but no peers -> standalone.
    store_signing_key(&engine);
    assert_eq!(get_cluster_mode(&engine), "standalone");

    // Add peers -> cluster.
    store_peer_configs(&engine, &[make_peer_config(5)]);
    assert_eq!(get_cluster_mode(&engine), "cluster");
}

// ===========================================================================
// Signing key syncs between engines (join flow simulation)
// ===========================================================================

#[test]
fn test_signing_key_syncs_between_engines() {
    // Engine A: the existing cluster member, has the signing key.
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let original_key = store_signing_key(&engine_a);
    store_file(&engine_a, "/hello.txt", b"hello world");

    // Engine B: the joining node, fresh — no signing key.
    let (engine_b, _temp_b) = create_temp_engine_for_tests();
    assert!(!has_signing_key(&engine_b));

    // Sync file data via the actual sync engine.
    let (sync_engine_b, peer_manager_b) = make_sync_engine(engine_b.clone());
    add_active_peer(&peer_manager_b, 1);
    let result = sync_engine_b
        .sync_with_local_engine(1, &engine_a)
        .expect("file sync should succeed");
    assert!(result.changes_applied);

    // Simulate system table sync (signing key transfer).
    // In the full implementation, system table entries will be synced
    // alongside file data. For now, we simulate this step.
    simulate_signing_key_sync(&engine_a, &engine_b);

    // Verify: engine B now has the signing key.
    assert!(has_signing_key(&engine_b));

    // Verify: the synced key matches the original.

    let synced_key = system_store::get_config(&engine_b, "jwt_signing_key")
        .expect("get_config should succeed")
        .expect("key should exist after sync");
    assert_eq!(synced_key, original_key);

    // Verify: the synced key can reconstruct a valid JwtManager.
    let manager = JwtManager::from_bytes(&synced_key).expect("should be valid Ed25519 seed");
    assert_eq!(manager.to_bytes(), original_key);
}

#[test]
fn test_signing_key_syncs_and_is_usable_for_jwt() {
    // End-to-end: sign a token on A, sync key to B, verify on B.
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    store_signing_key(&engine_a);
    store_file(&engine_a, "/data.json", b"{}");

    // Load the manager from A's key.

    let key_a = system_store::get_config(&engine_a, "jwt_signing_key")
        .unwrap()
        .unwrap();
    let manager_a = JwtManager::from_bytes(&key_a).unwrap();

    // Sign a token on A.
    let claims = aeordb::auth::jwt::TokenClaims {
        sub: "test-user".to_string(),
        iss: "aeordb".to_string(),
        iat: chrono::Utc::now().timestamp(),
        exp: chrono::Utc::now().timestamp() + 3600,
        scope: None,
        permissions: None,
        key_id: None,
    };
    let token = manager_a
        .create_token(&claims)
        .expect("should create token");

    // Sync to B: file data via sync engine, system tables simulated.
    let (engine_b, _temp_b) = create_temp_engine_for_tests();
    let (sync_engine_b, peer_manager_b) = make_sync_engine(engine_b.clone());
    add_active_peer(&peer_manager_b, 1);
    sync_engine_b
        .sync_with_local_engine(1, &engine_a)
        .expect("sync should succeed");
    simulate_signing_key_sync(&engine_a, &engine_b);

    // B loads the synced key and verifies the token signed by A.

    let key_b = system_store::get_config(&engine_b, "jwt_signing_key")
        .unwrap()
        .unwrap();
    let manager_b = JwtManager::from_bytes(&key_b).unwrap();
    let verified_claims = manager_b
        .verify_token(&token)
        .expect("B should verify token signed by A");
    assert_eq!(verified_claims.sub, "test-user");
}

#[test]
fn test_node_not_ready_before_sync_ready_after() {
    // Simulates the join flow: B starts in cluster mode, is NOT ready, syncs, then IS ready.
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    store_signing_key(&engine_a);
    store_file(&engine_a, "/init.txt", b"cluster bootstrap data");

    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // B is in cluster mode but has no signing key yet — not ready.
    assert!(!is_ready_for_traffic(&engine_b, true));

    // Sync B from A (file data + simulated system table sync).
    let (sync_engine_b, peer_manager_b) = make_sync_engine(engine_b.clone());
    add_active_peer(&peer_manager_b, 1);
    sync_engine_b
        .sync_with_local_engine(1, &engine_a)
        .expect("sync should succeed");
    simulate_signing_key_sync(&engine_a, &engine_b);

    // After sync, B is ready.
    assert!(is_ready_for_traffic(&engine_b, true));
}

#[test]
fn test_double_sync_preserves_signing_key() {
    // Syncing twice should not corrupt or lose the signing key.
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let original_key = store_signing_key(&engine_a);
    store_file(&engine_a, "/file1.txt", b"data1");

    let (engine_b, _temp_b) = create_temp_engine_for_tests();
    let (sync_engine_b, peer_manager_b) = make_sync_engine(engine_b.clone());
    add_active_peer(&peer_manager_b, 1);

    // First sync.
    sync_engine_b
        .sync_with_local_engine(1, &engine_a)
        .expect("first sync should succeed");
    simulate_signing_key_sync(&engine_a, &engine_b);
    assert!(has_signing_key(&engine_b));

    // Add more data to A and sync again.
    store_file(&engine_a, "/file2.txt", b"data2");
    sync_engine_b
        .sync_with_local_engine(1, &engine_a)
        .expect("second sync should succeed");
    // Simulate system table sync again (idempotent).
    simulate_signing_key_sync(&engine_a, &engine_b);

    // Key should still be intact.

    let key_after = system_store::get_config(&engine_b, "jwt_signing_key")
        .unwrap()
        .unwrap();
    assert_eq!(key_after, original_key);
}

#[test]
fn test_mismatched_signing_keys_between_nodes() {
    // Two nodes with different signing keys: tokens from one can't verify on the other.
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_signing_key(&engine_a);
    store_signing_key(&engine_b); // Different key!

    let key_a = system_store::get_config(&engine_a, "jwt_signing_key").unwrap().unwrap();
    let manager_a = JwtManager::from_bytes(&key_a).unwrap();

    let key_b = system_store::get_config(&engine_b, "jwt_signing_key").unwrap().unwrap();
    let manager_b = JwtManager::from_bytes(&key_b).unwrap();

    // Keys should be different (cryptographically random).
    assert_ne!(key_a, key_b);

    // A token signed by A should fail verification on B.
    let claims = aeordb::auth::jwt::TokenClaims {
        sub: "user-x".to_string(),
        iss: "aeordb".to_string(),
        iat: chrono::Utc::now().timestamp(),
        exp: chrono::Utc::now().timestamp() + 3600,
        scope: None,
        permissions: None,
        key_id: None,
    };
    let token = manager_a.create_token(&claims).unwrap();
    assert!(manager_b.verify_token(&token).is_err());
}

#[test]
fn test_signing_key_overwrite_updates_readiness() {
    // Overwriting the signing key with garbage makes the node un-ready if
    // the replacement is too short, and ready again with a valid key.
    let (engine, _temp) = create_temp_engine_for_tests();
    let context = RequestContext::system();

    // No key: not ready.
    assert!(!is_ready_for_traffic(&engine, true));

    // Store valid key: ready.
    store_signing_key(&engine);
    assert!(is_ready_for_traffic(&engine, true));

    // Overwrite with too-short value: not ready.
    system_store::store_config(&engine, &context, "jwt_signing_key", &[0u8; 10])
        .unwrap();
    assert!(!is_ready_for_traffic(&engine, true));

    // Overwrite with valid key again: ready.
    let manager = JwtManager::generate();
    system_store::store_config(&engine, &context, "jwt_signing_key", &manager.to_bytes())
        .unwrap();
    assert!(is_ready_for_traffic(&engine, true));
}
