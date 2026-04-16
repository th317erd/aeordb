use std::sync::Arc;

use aeordb::engine::conflict_store::list_conflicts;
use aeordb::engine::system_store;
use aeordb::engine::peer_connection::{PeerConfig, PeerManager};
use aeordb::engine::sync_engine::{PeerSyncState, SyncConfig, SyncEngine};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::virtual_clock::PeerClockTracker;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_sync_engine(engine: Arc<StorageEngine>) -> (SyncEngine, Arc<PeerManager>) {
    let peer_manager = Arc::new(PeerManager::new());
    let clock_tracker = Arc::new(PeerClockTracker::new(30_000));
    let config = SyncConfig {
        periodic_interval_secs: 30,
        cluster_secret: None,
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
    ops.store_file(&context, path, data, Some("text/plain")).unwrap();
}

fn read_file(engine: &StorageEngine, path: &str) -> Vec<u8> {
    let ops = DirectoryOps::new(engine);
    ops.read_file(path).unwrap()
}

fn file_exists(engine: &StorageEngine, path: &str) -> bool {
    let head = engine.head_hash().unwrap();
    let tree = walk_version_tree(engine, &head).unwrap();
    tree.files.contains_key(path)
}

// ---------------------------------------------------------------------------
// Test: SyncEngine creation doesn't panic
// ---------------------------------------------------------------------------

#[test]
fn test_sync_engine_creation() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, _peer_manager) = make_sync_engine(engine);

    // Verify accessors work
    assert!(sync_engine.engine().head_hash().is_ok());
    assert!(sync_engine.peer_manager().all_peers().is_empty());
}

// ---------------------------------------------------------------------------
// Test: sync returns error for non-Active peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_with_non_active_peer_disconnected() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, peer_manager) = make_sync_engine(engine);

    // Add peer but leave it Disconnected
    peer_manager.add_peer(&PeerConfig {
        node_id: 42,
        address: "http://localhost:9042".to_string(),
        label: None,
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    });

    let result = sync_engine.sync_with_peer(42).await;
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(
        error.contains("not Active"),
        "Expected 'not Active' error, got: {}",
        error
    );
}

#[tokio::test]
async fn test_sync_with_non_active_peer_honeymoon() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, peer_manager) = make_sync_engine(engine);

    peer_manager.add_peer(&PeerConfig {
        node_id: 43,
        address: "http://localhost:9043".to_string(),
        label: None,
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    });
    peer_manager.start_honeymoon(43, 1000);

    let result = sync_engine.sync_with_peer(43).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not Active"));
}

// ---------------------------------------------------------------------------
// Test: sync returns error for unknown peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_with_unknown_peer() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, _peer_manager) = make_sync_engine(engine);

    let result = sync_engine.sync_with_peer(999).await;
    assert!(result.is_err());
    assert!(
        result.unwrap_err().contains("not found"),
        "Expected 'not found' error"
    );
}

// ---------------------------------------------------------------------------
// Test: sync_all_peers skips inactive peers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_all_peers_skips_inactive() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, peer_manager) = make_sync_engine(engine);

    // Add one disconnected, one honeymoon, one active peer
    peer_manager.add_peer(&PeerConfig {
        node_id: 1,
        address: "http://localhost:9001".to_string(),
        label: Some("disconnected".to_string()),
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    });
    // node_id=1 stays Disconnected

    peer_manager.add_peer(&PeerConfig {
        node_id: 2,
        address: "http://localhost:9002".to_string(),
        label: Some("honeymoon".to_string()),
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    });
    peer_manager.start_honeymoon(2, 1000);
    // node_id=2 stays in Honeymoon

    peer_manager.add_peer(&PeerConfig {
        node_id: 3,
        address: "http://localhost:9003".to_string(),
        label: Some("active".to_string()),
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    });
    peer_manager.start_honeymoon(3, 1000);
    peer_manager.activate_peer(3);

    let results = sync_engine.sync_all_peers().await;

    // Only the active peer (node_id=3) should have been attempted
    assert_eq!(results.len(), 1, "Only active peers should be synced");
    assert_eq!(results[0].0, 3);
    // It will error because remote HTTP sync is not implemented, which is expected
    assert!(results[0].1.is_err());
}

// ---------------------------------------------------------------------------
// Test: sync_all_peers with no peers returns empty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_all_peers_empty() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, _peer_manager) = make_sync_engine(engine);

    let results = sync_engine.sync_all_peers().await;
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// Test: peer sync state persistence
// ---------------------------------------------------------------------------

#[test]
fn test_peer_sync_state_persistence() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, _peer_manager) = make_sync_engine(engine);

    // No state initially
    assert!(sync_engine.load_peer_sync_state(42).is_none());

    // After a sync, state should be persisted
    // We test this through system_store directly
    let ctx = aeordb::engine::RequestContext::system();
    let state = PeerSyncState {
        last_synced_root_hash: Some("deadbeef".to_string()),
        last_sync_at: Some(1234567890),
    };
    aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 42, &state).unwrap();

    let loaded = sync_engine.load_peer_sync_state(42);
    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.last_synced_root_hash, Some("deadbeef".to_string()));
    assert_eq!(loaded.last_sync_at, Some(1234567890));
}

#[test]
fn test_peer_sync_state_overwrite() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, _peer_manager) = make_sync_engine(engine);
    let ctx = aeordb::engine::RequestContext::system();

    // Store initial state
    let state1 = PeerSyncState {
        last_synced_root_hash: Some("aaa".to_string()),
        last_sync_at: Some(100),
    };
    aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 42, &state1).unwrap();

    // Overwrite with new state
    let state2 = PeerSyncState {
        last_synced_root_hash: Some("bbb".to_string()),
        last_sync_at: Some(200),
    };
    aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 42, &state2).unwrap();

    let loaded = sync_engine.load_peer_sync_state(42).unwrap();
    assert_eq!(loaded.last_synced_root_hash, Some("bbb".to_string()));
    assert_eq!(loaded.last_sync_at, Some(200));
}

#[test]
fn test_peer_sync_state_multiple_peers() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, _peer_manager) = make_sync_engine(engine);
    let ctx = aeordb::engine::RequestContext::system();

    aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 1, &PeerSyncState {
        last_synced_root_hash: Some("hash1".to_string()),
        last_sync_at: Some(100),
    }).unwrap();

    aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 2, &PeerSyncState {
        last_synced_root_hash: Some("hash2".to_string()),
        last_sync_at: Some(200),
    }).unwrap();

    let state1 = sync_engine.load_peer_sync_state(1).unwrap();
    let state2 = sync_engine.load_peer_sync_state(2).unwrap();

    assert_eq!(state1.last_synced_root_hash, Some("hash1".to_string()));
    assert_eq!(state2.last_synced_root_hash, Some("hash2".to_string()));
    assert!(sync_engine.load_peer_sync_state(3).is_none());
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync cycle — two engines, no HTTP
// Engine A has file /a.txt, Engine B has file /b.txt.
// After sync, Engine A should have both files.
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_both_add_different_files() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Store different files in each engine
    store_file(&engine_a, "/a.txt", b"content from A");
    store_file(&engine_b, "/b.txt", b"content from B");

    // Set up sync engine for A
    let (sync_engine_a, _peer_manager_a) = make_sync_engine(Arc::clone(&engine_a));

    // Sync A with B's engine directly
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    assert!(result.changes_applied, "Changes should have been applied");
    assert_eq!(result.conflicts_detected, 0, "No conflicts expected");
    assert!(result.operations_applied > 0, "Operations should have been applied");

    // Engine A should now have both files
    assert!(file_exists(&engine_a, "/a.txt"), "A should still have /a.txt");
    assert!(file_exists(&engine_a, "/b.txt"), "A should now have /b.txt from B");

    // Verify content
    assert_eq!(read_file(&engine_a, "/a.txt"), b"content from A");
    assert_eq!(read_file(&engine_a, "/b.txt"), b"content from B");
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync when engines are already identical
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_identical_engines() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Both engines start with the same empty state (identical HEAD)
    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

    let head_a = engine_a.head_hash().unwrap();
    let head_b = engine_b.head_hash().unwrap();
    assert_eq!(head_a, head_b, "Fresh engines should have identical HEAD");

    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
    assert!(!result.changes_applied, "No changes expected for identical engines");
    assert_eq!(result.conflicts_detected, 0);
    assert_eq!(result.operations_applied, 0);
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync with conflict (same file, different content)
// Both engines modify the same path => LWW conflict
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_conflict_same_file() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Both engines write to the same path with different content
    // and different timestamps (engine B writes later => B wins)
    store_file(&engine_a, "/shared.txt", b"version from A");
    store_file(&engine_b, "/shared.txt", b"version from B");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    // There should be a conflict detected (both added the same path)
    assert!(result.conflicts_detected > 0, "Should detect a conflict");

    // The file should still exist
    assert!(file_exists(&engine_a, "/shared.txt"));

    // Conflicts should be stored in /.conflicts/
    let conflicts = list_conflicts(&engine_a).unwrap();
    assert!(
        !conflicts.is_empty(),
        "Conflicts should be stored in /.conflicts/"
    );
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync — one side adds, other side is empty (initial sync)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_one_side_empty() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Only B has files
    store_file(&engine_b, "/from_b_1.txt", b"data 1");
    store_file(&engine_b, "/from_b_2.txt", b"data 2");
    store_file(&engine_b, "/subdir/nested.txt", b"nested data");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    assert!(result.changes_applied);
    assert_eq!(result.conflicts_detected, 0);

    // A should now have all of B's files
    assert!(file_exists(&engine_a, "/from_b_1.txt"));
    assert!(file_exists(&engine_a, "/from_b_2.txt"));
    assert!(file_exists(&engine_a, "/subdir/nested.txt"));

    assert_eq!(read_file(&engine_a, "/from_b_1.txt"), b"data 1");
    assert_eq!(read_file(&engine_a, "/subdir/nested.txt"), b"nested data");
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync updates peer sync state
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_updates_peer_state() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_file(&engine_b, "/b.txt", b"hello");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

    // No state before sync
    assert!(sync_engine_a.load_peer_sync_state(2).is_none());

    sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    // State should be recorded after sync
    let state = sync_engine_a.load_peer_sync_state(2);
    assert!(state.is_some(), "Sync state should be saved");
    let state = state.unwrap();
    assert!(state.last_synced_root_hash.is_some());
    assert!(state.last_sync_at.is_some());
}

// ---------------------------------------------------------------------------
// Test: Subsequent sync after initial sync (incremental)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_incremental_second_sync() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Initial: B has one file
    store_file(&engine_b, "/first.txt", b"first");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

    // First sync
    let result1 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
    assert!(result1.changes_applied);
    assert!(file_exists(&engine_a, "/first.txt"));

    // Now B adds another file
    store_file(&engine_b, "/second.txt", b"second");

    // Second sync should pick up only the new file
    let result2 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
    assert!(result2.changes_applied);
    assert!(file_exists(&engine_a, "/second.txt"));
    assert_eq!(read_file(&engine_a, "/second.txt"), b"second");
}

// ---------------------------------------------------------------------------
// Test: Bidirectional sync (sync A->B, then B->A)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_bidirectional_convergence() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_file(&engine_a, "/from_a.txt", b"A's data");
    store_file(&engine_b, "/from_b.txt", b"B's data");

    // Sync A <- B (A gets B's files)
    let (sync_engine_a, _pm_a) = make_sync_engine(Arc::clone(&engine_a));
    sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    // Sync B <- A (B gets A's files)
    let (sync_engine_b, _pm_b) = make_sync_engine(Arc::clone(&engine_b));
    sync_engine_b.sync_with_local_engine(1, &engine_a).unwrap();

    // Both engines should now have both files
    assert!(file_exists(&engine_a, "/from_a.txt"));
    assert!(file_exists(&engine_a, "/from_b.txt"));
    assert!(file_exists(&engine_b, "/from_a.txt"));
    assert!(file_exists(&engine_b, "/from_b.txt"));

    // Content should match
    assert_eq!(read_file(&engine_a, "/from_a.txt"), b"A's data");
    assert_eq!(read_file(&engine_a, "/from_b.txt"), b"B's data");
    assert_eq!(read_file(&engine_b, "/from_a.txt"), b"A's data");
    assert_eq!(read_file(&engine_b, "/from_b.txt"), b"B's data");
}

// ---------------------------------------------------------------------------
// Test: Sync with large file (multiple chunks)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_large_file_multiple_chunks() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Create a file larger than the default chunk size (256KB)
    let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
    store_file(&engine_b, "/large.bin", &large_data);

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    assert!(result.changes_applied);
    assert!(file_exists(&engine_a, "/large.bin"));

    let synced_data = read_file(&engine_a, "/large.bin");
    assert_eq!(synced_data.len(), large_data.len());
    assert_eq!(synced_data, large_data);
}

// ---------------------------------------------------------------------------
// Test: Sync when one side deletes a file (remote delete applied locally)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_remote_deletion() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Both start with a file
    store_file(&engine_a, "/shared.txt", b"shared content");
    store_file(&engine_b, "/shared.txt", b"shared content");

    // Sync so they have a common base
    let (sync_engine_a, _pm_a) = make_sync_engine(Arc::clone(&engine_a));
    sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    // Now B deletes the file
    let context = RequestContext::system();
    let ops_b = DirectoryOps::new(&engine_b);
    ops_b.delete_file(&context, "/shared.txt").unwrap();

    // Sync A <- B: A should see the delete
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
    assert!(result.changes_applied);
    assert!(!file_exists(&engine_a, "/shared.txt"), "File should be deleted after sync");
}

// ---------------------------------------------------------------------------
// Test: Sync when one side modifies and the other deletes (modify wins)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_modify_vs_delete_conflict() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // Both start with a file
    store_file(&engine_a, "/conflict.txt", b"original");
    store_file(&engine_b, "/conflict.txt", b"original");

    // Sync to establish common base
    let (sync_engine_a, _pm_a) = make_sync_engine(Arc::clone(&engine_a));
    sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    // A modifies the file, B deletes it
    store_file(&engine_a, "/conflict.txt", b"modified by A");
    let context = RequestContext::system();
    let ops_b = DirectoryOps::new(&engine_b);
    ops_b.delete_file(&context, "/conflict.txt").unwrap();

    // Sync A <- B: modify should win (safety-first rule)
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    // The file should still exist (modify wins over delete)
    assert!(
        file_exists(&engine_a, "/conflict.txt"),
        "Modified file should survive (modify wins over delete)"
    );
    assert!(result.conflicts_detected > 0, "Should detect modify-delete conflict");
}

// ---------------------------------------------------------------------------
// Test: Remote HTTP sync returns connection error for unreachable peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_remote_sync_returns_connection_error() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, peer_manager) = make_sync_engine(engine);

    add_active_peer(&peer_manager, 10);

    let result = sync_engine.sync_with_peer(10).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("Failed to contact peer"),
        "Should indicate connection failure, got: {}",
        err
    );
}

// ---------------------------------------------------------------------------
// Test: Sync with multiple files and nested directories
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_nested_directories() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    // B has a complex directory structure
    store_file(&engine_b, "/docs/readme.txt", b"readme content");
    store_file(&engine_b, "/docs/api/endpoints.json", b"{}");
    store_file(&engine_b, "/src/main.rs", b"fn main() {}");
    store_file(&engine_b, "/config.toml", b"[settings]");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    assert!(result.changes_applied);
    assert!(file_exists(&engine_a, "/docs/readme.txt"));
    assert!(file_exists(&engine_a, "/docs/api/endpoints.json"));
    assert!(file_exists(&engine_a, "/src/main.rs"));
    assert!(file_exists(&engine_a, "/config.toml"));

    assert_eq!(read_file(&engine_a, "/docs/readme.txt"), b"readme content");
    assert_eq!(read_file(&engine_a, "/src/main.rs"), b"fn main() {}");
}

// ---------------------------------------------------------------------------
// Test: Sync twice with no changes second time
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_no_changes_second_time() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_file(&engine_b, "/file.txt", b"data");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

    // First sync applies changes
    let result1 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
    assert!(result1.changes_applied);

    // Second sync: no new changes from B
    let result2 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
    assert!(!result2.changes_applied, "No changes on second sync");
    assert_eq!(result2.operations_applied, 0);
}

// ---------------------------------------------------------------------------
// Test: SyncCycleResult fields are populated correctly
// ---------------------------------------------------------------------------

#[test]
fn test_sync_cycle_result_fields() {
    let (engine_a, _temp_a) = create_temp_engine_for_tests();
    let (engine_b, _temp_b) = create_temp_engine_for_tests();

    store_file(&engine_b, "/x.txt", b"x");
    store_file(&engine_b, "/y.txt", b"y");

    let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
    let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

    assert!(result.changes_applied);
    assert_eq!(result.conflicts_detected, 0);
    // At least 2 operations (add x.txt and y.txt)
    assert!(result.operations_applied >= 2, "Expected at least 2 ops, got {}", result.operations_applied);
}
