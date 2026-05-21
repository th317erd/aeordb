use std::sync::Arc;
use std::collections::HashMap;

use aeordb::engine::{
    DirectoryOps, EventBus, RequestContext, StorageEngine, VersionManager,
};
use aeordb::server::create_temp_engine_for_tests;

// ─── Helpers ────────────────────────────────────────────────────────────

fn setup_with_events() -> (Arc<StorageEngine>, Arc<EventBus>, RequestContext, tempfile::TempDir) {
    let (engine, temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());
    let ctx = RequestContext::from_claims("test-user", bus.clone());
    (engine, bus, ctx, temp)
}

// ─── Entry events: store_file ───────────────────────────────────────────

#[tokio::test]
async fn test_store_file_emits_entries_created() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.user_id, "test-user");

    let entries = event.payload["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "/test.txt");
    assert_eq!(entries[0]["entry_type"], "file");
    assert_eq!(entries[0]["content_type"], "text/plain");
    assert!(entries[0]["size"].as_u64().unwrap() > 0);
    assert!(entries[0]["created_at"].as_i64().unwrap() > 0);
    assert!(entries[0]["updated_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn test_store_file_compressed_emits_entries_created() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_compressed(
        &ctx,
        "/compressed.txt",
        b"hello world hello world hello world",
        Some("text/plain"),
        aeordb::engine::CompressionAlgorithm::Zstd,
    ).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.payload["entries"][0]["path"], "/compressed.txt");
}

#[tokio::test]
async fn test_store_file_overwrite_emits_entries_created() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"version1", Some("text/plain")).unwrap();

    let mut rx = bus.subscribe(); // subscribe AFTER first store
    ops.store_file_buffered(&ctx, "/test.txt", b"version2", Some("text/plain")).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.payload["entries"][0]["path"], "/test.txt");
}

// ─── Entry events: delete_file ──────────────────────────────────────────


#[tokio::test]
async fn test_delete_file_emits_entries_deleted() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    let mut rx = bus.subscribe(); // subscribe AFTER store to skip create event
    ops.delete_file(&ctx, "/test.txt").unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_deleted");
    assert_eq!(event.user_id, "test-user");

    let entries = event.payload["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "/test.txt");
    assert_eq!(entries[0]["entry_type"], "file");
    // Deleted event should carry the original file metadata
    assert_eq!(entries[0]["content_type"], "text/plain");
    assert!(entries[0]["size"].as_u64().unwrap() > 0);
}


#[tokio::test]
async fn test_delete_file_not_found_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    let result = ops.delete_file(&ctx, "/nonexistent.txt");
    assert!(result.is_err());

    // No event should be emitted for failed deletion
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}


#[tokio::test]
async fn test_delete_file_with_indexing_emits_entries_deleted() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/indexed.txt", b"data", Some("text/plain")).unwrap();

    let mut rx = bus.subscribe();
    ops.delete_file_with_indexing(&ctx, "/indexed.txt").unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_deleted");
    assert_eq!(event.payload["entries"][0]["path"], "/indexed.txt");
}

// ─── Entry events: create_directory ─────────────────────────────────────

#[tokio::test]
async fn test_create_directory_emits_entries_created() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.create_directory(&ctx, "/mydir/").unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.user_id, "test-user");

    let entries = event.payload["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["entry_type"], "directory");
    assert_eq!(entries[0]["size"], 0);
    assert!(entries[0]["created_at"].as_i64().unwrap() > 0);
}

// ─── Version events: snapshots ──────────────────────────────────────────

#[tokio::test]
async fn test_create_snapshot_emits_version_created() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "versions_created");
    assert_eq!(event.user_id, "test-user");
    assert_eq!(event.payload["versions"][0]["name"], "v1");
    assert_eq!(event.payload["versions"][0]["version_type"], "snapshot");
    assert!(!event.payload["versions"][0]["root_hash"].as_str().unwrap().is_empty());
    assert!(event.payload["versions"][0]["created_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn test_create_snapshot_duplicate_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let mut rx = bus.subscribe();
    let result = vm.create_snapshot(&ctx, "v1", HashMap::new());
    assert!(result.is_err()); // AlreadyExists

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}

#[tokio::test]
async fn test_delete_snapshot_emits_version_deleted() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let mut rx = bus.subscribe();
    vm.delete_snapshot(&ctx, "v1").unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "versions_deleted");
    assert_eq!(event.payload["versions"][0]["name"], "v1");
    assert_eq!(event.payload["versions"][0]["version_type"], "snapshot");
}

#[tokio::test]
async fn test_delete_snapshot_not_found_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    let result = vm.delete_snapshot(&ctx, "nonexistent");
    assert!(result.is_err());

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}

#[tokio::test]
async fn test_restore_snapshot_emits_version_restored() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let mut rx = bus.subscribe();
    vm.restore_snapshot(&ctx, "v1").unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "versions_restored");
    assert_eq!(event.payload["versions"][0]["name"], "v1");
    assert_eq!(event.payload["versions"][0]["version_type"], "snapshot");
    assert!(!event.payload["versions"][0]["root_hash"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn test_restore_nonexistent_snapshot_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    let result = vm.restore_snapshot(&ctx, "nonexistent");
    assert!(result.is_err());

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}

// ─── Version events: forks ──────────────────────────────────────────────

#[tokio::test]
async fn test_create_fork_emits_version_created() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    vm.create_fork(&ctx, "feature", None).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "versions_created");
    assert_eq!(event.payload["versions"][0]["name"], "feature");
    assert_eq!(event.payload["versions"][0]["version_type"], "fork");
    assert!(event.payload["versions"][0]["created_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn test_create_fork_duplicate_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_fork(&ctx, "feature", None).unwrap();

    let mut rx = bus.subscribe();
    let result = vm.create_fork(&ctx, "feature", None);
    assert!(result.is_err());

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}

#[tokio::test]
async fn test_abandon_fork_emits_version_deleted() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_fork(&ctx, "feature", None).unwrap();

    let mut rx = bus.subscribe();
    vm.abandon_fork(&ctx, "feature").unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "versions_deleted");
    assert_eq!(event.payload["versions"][0]["name"], "feature");
    assert_eq!(event.payload["versions"][0]["version_type"], "fork");
}

#[tokio::test]
async fn test_abandon_fork_not_found_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    let result = vm.abandon_fork(&ctx, "nonexistent");
    assert!(result.is_err());

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}

#[tokio::test]
async fn test_promote_fork_emits_promoted_and_deleted() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_fork(&ctx, "feature", None).unwrap();

    let mut rx = bus.subscribe();
    vm.promote_fork(&ctx, "feature").unwrap();

    // First event should be versions_promoted
    let event1 = rx.recv().await.unwrap();
    assert_eq!(event1.event_type, "versions_promoted");
    assert_eq!(event1.payload["versions"][0]["name"], "feature");
    assert_eq!(event1.payload["versions"][0]["version_type"], "fork");

    // Second event should be versions_deleted (from abandon_fork)
    let event2 = rx.recv().await.unwrap();
    assert_eq!(event2.event_type, "versions_deleted");
    assert_eq!(event2.payload["versions"][0]["name"], "feature");
}

#[tokio::test]
async fn test_promote_nonexistent_fork_no_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    let result = vm.promote_fork(&ctx, "nonexistent");
    assert!(result.is_err());

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events for failed operation");
}

// ─── Import events ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_import_backup_emits_imports_completed() {
    let (source, _source_temp) = create_temp_engine_for_tests();
    let sys_ctx = RequestContext::system();
    let ops = DirectoryOps::new(&source);
    ops.store_file_buffered(&sys_ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    // Export
    let export_temp = tempfile::tempdir().unwrap();
    let export_path = export_temp.path().join("export.aeordb").to_str().unwrap().to_string();
    let head = source.head_hash().unwrap();
    aeordb::engine::export_version(&source, &head, &export_path, false).unwrap();

    // Import with events
    let (target, _target_temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());
    let ctx = RequestContext::from_claims("importer", bus.clone());
    let mut rx = bus.subscribe();

    aeordb::engine::import_backup(&ctx, &target, &export_path, false, true, false).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "imports_completed");
    assert_eq!(event.user_id, "importer");

    let imports = event.payload["imports"].as_array().unwrap();
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0]["backup_type"], "export");
    assert!(imports[0]["entries_imported"].as_u64().unwrap() > 0);
    assert_eq!(imports[0]["head_promoted"], true);
}

#[tokio::test]
async fn test_import_backup_no_event_with_system_ctx() {
    let (source, _source_temp) = create_temp_engine_for_tests();
    let sys_ctx = RequestContext::system();
    let ops = DirectoryOps::new(&source);
    ops.store_file_buffered(&sys_ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    let export_temp = tempfile::tempdir().unwrap();
    let export_path = export_temp.path().join("export.aeordb").to_str().unwrap().to_string();
    let head = source.head_hash().unwrap();
    aeordb::engine::export_version(&source, &head, &export_path, false).unwrap();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let (target, _target_temp) = create_temp_engine_for_tests();

    // Import with system context (no bus)
    aeordb::engine::import_backup(&sys_ctx, &target, &export_path, false, true, false).unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events when using system context");
}

// ─── System context tests ───────────────────────────────────────────────

#[tokio::test]
async fn test_no_events_with_system_context() {
    let (engine, bus, _, _temp) = setup_with_events();
    let ctx = RequestContext::system(); // no bus
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    ops.create_directory(&ctx, "/somedir/").unwrap();

    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "snap", HashMap::new()).unwrap();
    vm.create_fork(&ctx, "fork1", None).unwrap();

    // Should timeout — no events emitted
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — no events when using system context");
}

// ─── User ID propagation ───────────────────────────────────────────────

#[tokio::test]
async fn test_event_user_id_from_context() {
    let (engine, _, _, _temp) = setup_with_events();
    let bus = Arc::new(EventBus::new());
    let ctx = RequestContext::from_claims("alice-uuid-123", bus.clone());
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.user_id, "alice-uuid-123");
}

#[tokio::test]
async fn test_different_users_produce_correct_user_ids() {
    let (engine, _, _, _temp) = setup_with_events();
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let ctx_alice = RequestContext::from_claims("alice", bus.clone());
    let ctx_bob = RequestContext::from_claims("bob", bus.clone());

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx_alice, "/alice.txt", b"a", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx_bob, "/bob.txt", b"b", Some("text/plain")).unwrap();

    let event1 = rx.recv().await.unwrap();
    let event2 = rx.recv().await.unwrap();
    assert_eq!(event1.user_id, "alice");
    assert_eq!(event2.user_id, "bob");
}

// ─── Multiple operations / unique event IDs ─────────────────────────────

#[tokio::test]
async fn test_multiple_operations_produce_multiple_events() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/a.txt", b"aaa", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/b.txt", b"bbb", Some("text/plain")).unwrap();

    let event1 = rx.recv().await.unwrap();
    let event2 = rx.recv().await.unwrap();
    assert_eq!(event1.event_type, "entries_created");
    assert_eq!(event2.event_type, "entries_created");
    assert_ne!(event1.event_id, event2.event_id);
    assert_ne!(
        event1.payload["entries"][0]["path"],
        event2.payload["entries"][0]["path"],
    );
}

// ─── No double-emission from wrapper methods ────────────────────────────

#[tokio::test]
async fn test_store_file_with_indexing_emits_once() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_with_indexing(&ctx, "/indexed.json", b"{\"name\":\"test\"}", Some("application/json")).unwrap();

    // Should get exactly one entries_created event
    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.payload["entries"][0]["path"], "/indexed.json");

    // No second event within 100ms
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — only one event for store_file_with_indexing");
}

#[tokio::test]
async fn test_store_file_with_full_pipeline_emits_once() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_with_full_pipeline(&ctx, "/piped.json", b"{\"key\":\"val\"}", Some("application/json"), None).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.payload["entries"][0]["path"], "/piped.json");

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        rx.recv(),
    ).await;
    assert!(result.is_err(), "should timeout — only one event for store_file_with_full_pipeline");
}

// ─── Empty file edge case ───────────────────────────────────────────────

#[tokio::test]
async fn test_store_empty_file_emits_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/empty.txt", b"", Some("text/plain")).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.payload["entries"][0]["path"], "/empty.txt");
    assert_eq!(event.payload["entries"][0]["size"], 0);
    // Empty file has no chunks, so hash should be empty
    assert_eq!(event.payload["entries"][0]["hash"], "");
}

// ─── Event payload structure validation ─────────────────────────────────

#[tokio::test]
async fn test_entry_event_has_no_previous_hash() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    let event = rx.recv().await.unwrap();
    // previous_hash should not be present (skip_serializing_if = None)
    assert!(event.payload["entries"][0].get("previous_hash").is_none());
}

#[tokio::test]
async fn test_version_event_created_at_present_on_create() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let event = rx.recv().await.unwrap();
    assert!(event.payload["versions"][0]["created_at"].as_i64().is_some());
}

#[tokio::test]
async fn test_version_event_created_at_absent_on_delete() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

    let mut rx = bus.subscribe();
    vm.delete_snapshot(&ctx, "v1").unwrap();

    let event = rx.recv().await.unwrap();
    // created_at is None for deletes, so it should be absent (skip_serializing_if)
    assert!(event.payload["versions"][0].get("created_at").is_none());
}

// ─── Snapshot with metadata ─────────────────────────────────────────────

#[tokio::test]
async fn test_create_snapshot_with_metadata_emits_event() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let mut meta = HashMap::new();
    meta.insert("description".to_string(), "release".to_string());

    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "release-v1", meta).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "versions_created");
    assert_eq!(event.payload["versions"][0]["name"], "release-v1");
}

// ─── Mixed operations event ordering ────────────────────────────────────


#[tokio::test]
async fn test_mixed_operations_event_ordering() {
    let (engine, bus, ctx, _temp) = setup_with_events();
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file_buffered(&ctx, "/file1.txt", b"data", Some("text/plain")).unwrap();
    vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
    ops.create_directory(&ctx, "/newdir/").unwrap();
    ops.delete_file(&ctx, "/file1.txt").unwrap();

    let e1 = rx.recv().await.unwrap();
    let e2 = rx.recv().await.unwrap();
    let e3 = rx.recv().await.unwrap();
    let e4 = rx.recv().await.unwrap();

    assert_eq!(e1.event_type, "entries_created");   // store_file
    assert_eq!(e2.event_type, "versions_created");  // create_snapshot
    assert_eq!(e3.event_type, "entries_created");   // create_directory
    assert_eq!(e4.event_type, "entries_deleted");   // delete_file
}

// ─── with_bus context emits events ──────────────────────────────────────

#[tokio::test]
async fn test_with_bus_context_emits_events() {
    let (engine, _, _, _temp) = setup_with_events();
    let bus = Arc::new(EventBus::new());
    let ctx = RequestContext::with_bus(bus.clone());
    let mut rx = bus.subscribe();

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    let event = rx.recv().await.unwrap();
    assert_eq!(event.event_type, "entries_created");
    assert_eq!(event.user_id, "system"); // with_bus uses "system" user_id
}
