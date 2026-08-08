use std::sync::Arc;
use std::collections::HashMap;

use aeordb::engine::file_record::FileRecord;
use aeordb::engine::merge::MergeOp;
use aeordb::engine::symlink_record::SymlinkRecord;
use aeordb::engine::sync_apply::apply_merge_operations;
use aeordb::engine::{BufferedFile, DirectoryOps, EventBus, RequestContext, StorageEngine, VersionManager};
use aeordb::server::create_temp_engine_for_tests;

// ─── Helpers ────────────────────────────────────────────────────────────

fn setup_with_events() -> (Arc<StorageEngine>, Arc<EventBus>, RequestContext, tempfile::TempDir) {
  let (engine, temp) = create_temp_engine_for_tests();
  let bus = Arc::new(EventBus::new());
  let ctx = RequestContext::from_claims("test-user", bus.clone());
  (engine, bus, ctx, temp)
}

fn assert_namespace_acknowledgement(event: &aeordb::engine::EngineEvent, expected_kind: &str) -> uuid::Uuid {
  let operation_id = uuid::Uuid::parse_str(
    event.payload["operation_id"].as_str().unwrap_or_else(|| panic!("entry event must carry an operation ID: {}", event.payload)),
  )
  .unwrap();
  assert!(!operation_id.is_nil());
  assert!(event.payload["publication_sequence"].as_u64().expect("entry event must carry a publication sequence") > 0);
  assert_eq!(event.payload["mutation_kind"], expected_kind);
  operation_id
}

async fn assert_one_wave_one_mutation<F>(
  engine: &StorageEngine,
  receiver: &mut tokio::sync::broadcast::Receiver<aeordb::engine::EngineEvent>,
  expected_kind: &str,
  mutation: F,
) -> uuid::Uuid
where
  F: FnOnce(),
{
  let durability_before = engine.durability_snapshot().unwrap();
  let writes_before = engine.counters().snapshot().writes_total;
  mutation();
  let event = receiver.recv().await.unwrap();
  let operation_id = assert_namespace_acknowledgement(&event, expected_kind);
  let durability_after = engine.durability_snapshot().unwrap();
  assert_eq!(durability_after.next_sequence, durability_before.next_sequence + 1);
  assert_eq!(engine.counters().snapshot().writes_total, writes_before + 1, "{expected_kind} must acknowledge one logical write metric");
  assert_eq!(event.payload["publication_sequence"].as_u64().unwrap(), durability_after.hard_frontier);
  operation_id
}

#[tokio::test]
async fn wave_one_entry_mutations_emit_one_exact_acknowledgement_after_hard_publication() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let mut receiver = bus.subscribe();
  let ops = DirectoryOps::new(&engine);
  let mut operation_ids = std::collections::HashSet::new();

  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "file_write", || {
      ops.store_file_buffered(&ctx, "/wave-one/file.txt", b"first", Some("text/plain")).unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "file_write", || {
      ops.store_file_buffered(&ctx, "/wave-one/file.txt", b"second", Some("text/plain")).unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "file_write", || {
      ops.store_file_buffered(&ctx, "/.aeordb-system/wave-one-delete.txt", b"delete me", Some("text/plain")).unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "directory_create", || {
      ops.create_directory(&ctx, "/wave-one/empty").unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "symlink_write", || {
      ops.store_symlink(&ctx, "/wave-one/link", "/wave-one/file.txt").unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "symlink_delete", || {
      ops.delete_symlink(&ctx, "/wave-one/link").unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "file_delete", || {
      ops.delete_file(&ctx, "/.aeordb-system/wave-one-delete.txt").unwrap();
    })
    .await,
  );
  operation_ids.insert(
    assert_one_wave_one_mutation(&engine, &mut receiver, "directory_delete", || {
      ops.delete_directory(&ctx, "/wave-one/empty").unwrap();
    })
    .await,
  );

  assert_eq!(operation_ids.len(), 8);
}

async fn assert_one_wave_two_mutation<F>(
  engine: &StorageEngine,
  receiver: &mut tokio::sync::broadcast::Receiver<aeordb::engine::EngineEvent>,
  expected_kind: &str,
  expected_event_types: &[&str],
  mutation: F,
) -> Vec<aeordb::engine::EngineEvent>
where
  F: FnOnce(),
{
  let durability_before = engine.durability_snapshot().unwrap();
  let writes_before = engine.counters().snapshot().writes_total;
  mutation();

  let mut events = Vec::with_capacity(expected_event_types.len());
  for expected_event_type in expected_event_types {
    let event =
      tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv()).await.expect("mutation event should arrive").unwrap();
    assert_eq!(event.event_type, *expected_event_type);
    events.push(event);
  }

  let operation_ids: std::collections::HashSet<_> =
    events.iter().map(|event| assert_namespace_acknowledgement(event, expected_kind)).collect();
  assert_eq!(operation_ids.len(), 1, "every event from one logical mutation must share one operation ID");
  let publication_sequences: std::collections::HashSet<_> =
    events.iter().map(|event| event.payload["publication_sequence"].as_u64().unwrap()).collect();
  assert_eq!(publication_sequences.len(), 1, "every event from one logical mutation must share one publication sequence");

  let durability_after = engine.durability_snapshot().unwrap();
  assert_eq!(durability_after.next_sequence, durability_before.next_sequence + 1);
  assert_eq!(engine.counters().snapshot().writes_total, writes_before + 1, "{expected_kind} must acknowledge one logical write metric");
  assert_eq!(*publication_sequences.iter().next().unwrap(), durability_after.hard_frontier);
  assert!(
    tokio::time::timeout(std::time::Duration::from_millis(100), receiver.recv()).await.is_err(),
    "one logical mutation emitted an unexpected extra event"
  );
  events
}

async fn assert_one_version_mutation<F>(
  engine: &StorageEngine,
  receiver: &mut tokio::sync::broadcast::Receiver<aeordb::engine::EngineEvent>,
  expected_kind: &str,
  expected_event_types: &[&str],
  mutation: F,
) -> Vec<aeordb::engine::EngineEvent>
where
  F: FnOnce(),
{
  let durability_before = engine.durability_snapshot().unwrap();
  let writes_before = engine.counters().snapshot().writes_total;
  mutation();

  let mut events = Vec::with_capacity(expected_event_types.len());
  for expected_event_type in expected_event_types {
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
      .await
      .expect("version mutation event should arrive")
      .unwrap();
    assert_eq!(event.event_type, *expected_event_type);
    events.push(event);
  }

  let operation_ids: std::collections::HashSet<_> =
    events.iter().map(|event| assert_namespace_acknowledgement(event, expected_kind)).collect();
  assert_eq!(operation_ids.len(), 1, "every event from one version mutation must share one operation ID");
  let publication_sequences: std::collections::HashSet<_> =
    events.iter().map(|event| event.payload["publication_sequence"].as_u64().unwrap()).collect();
  assert_eq!(publication_sequences.len(), 1, "every event from one version mutation must share one publication sequence");

  let durability_after = engine.durability_snapshot().unwrap();
  assert_eq!(durability_after.next_sequence, durability_before.next_sequence + 1);
  assert_eq!(engine.counters().snapshot().writes_total, writes_before + 1, "{expected_kind} must acknowledge one logical write metric");
  assert_eq!(*publication_sequences.iter().next().unwrap(), durability_after.hard_frontier);
  assert!(
    tokio::time::timeout(std::time::Duration::from_millis(100), receiver.recv()).await.is_err(),
    "one logical version mutation emitted an unexpected extra event"
  );
  events
}

#[tokio::test]
async fn wave_two_batch_and_rename_mutations_emit_one_exact_acknowledgement() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let ops = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  ops.store_symlink(&system, "/wave-two/link", "/wave-two/b.txt").unwrap();
  ops.store_file_buffered(&system, "/wave-two/copy-source.txt", b"copy", Some("text/plain")).unwrap();
  ops.store_file_buffered(&system, "/wave-two/merge-single.json", br#"{"base":true}"#, Some("application/json")).unwrap();
  ops.store_file_buffered(&system, "/wave-two/merge-batch.json", br#"{"base":true}"#, Some("application/json")).unwrap();
  let mut receiver = bus.subscribe();

  let batch_events = assert_one_wave_two_mutation(&engine, &mut receiver, "batch_write", &["entries_created"], || {
    ops
      .store_files_buffered_batch(
        &ctx,
        vec![
          BufferedFile { path: "/wave-two/a.txt".to_string(), data: b"a".to_vec(), content_type: Some("text/plain".to_string()) },
          BufferedFile { path: "/wave-two/b.txt".to_string(), data: b"b".to_vec(), content_type: Some("text/plain".to_string()) },
        ],
      )
      .unwrap();
  })
  .await;
  assert_eq!(batch_events[0].payload["entries"].as_array().unwrap().len(), 2);

  let rename_events = assert_one_wave_two_mutation(&engine, &mut receiver, "rename", &["entries_deleted", "entries_created"], || {
    ops.rename_file(&ctx, "/wave-two/a.txt", "/wave-two/renamed.txt").unwrap();
  })
  .await;
  assert_eq!(rename_events[0].payload["entries"][0]["path"], "/wave-two/a.txt");
  assert_eq!(rename_events[1].payload["entries"][0]["path"], "/wave-two/renamed.txt");

  let symlink_events = assert_one_wave_two_mutation(&engine, &mut receiver, "rename", &["entries_deleted", "entries_created"], || {
    ops.rename_symlink(&ctx, "/wave-two/link", "/wave-two/renamed-link").unwrap();
  })
  .await;
  assert_eq!(symlink_events[0].payload["entries"][0]["path"], "/wave-two/link");
  assert_eq!(symlink_events[1].payload["entries"][0]["path"], "/wave-two/renamed-link");

  let copy_events = assert_one_wave_two_mutation(&engine, &mut receiver, "copy", &["entries_created"], || {
    ops.copy_file(&ctx, "/wave-two/copy-source.txt", "/wave-two/copied.txt").unwrap();
  })
  .await;
  assert_eq!(copy_events[0].payload["entries"][0]["path"], "/wave-two/copied.txt");

  let merge_events = assert_one_wave_two_mutation(&engine, &mut receiver, "merge", &["entries_created"], || {
    ops
      .merge_json_file(&ctx, "/wave-two/merge-single.json", serde_json::json!({"single": true}), aeordb::engine::MergeDepth::Unbounded)
      .unwrap();
  })
  .await;
  assert_eq!(merge_events[0].payload["entries"][0]["path"], "/wave-two/merge-single.json");

  let merge_batch_events = assert_one_wave_two_mutation(&engine, &mut receiver, "merge", &["entries_created"], || {
    ops
      .merge_json_files_batch(
        &ctx,
        vec![aeordb::engine::JsonMergeFilePatch {
          path: "/wave-two/merge-batch.json".to_string(),
          patch: serde_json::json!({"batch": true}),
          depth: aeordb::engine::MergeDepth::Unbounded,
        }],
      )
      .unwrap();
  })
  .await;
  assert_eq!(merge_batch_events[0].payload["entries"][0]["path"], "/wave-two/merge-batch.json");
}

// ─── Entry events: store_file ───────────────────────────────────────────

#[tokio::test]
async fn sync_apply_emits_one_exact_acknowledgement_for_a_mixed_receipt() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let ops = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  ops.store_file_buffered(&system, "/sync/source.txt", b"source payload", Some("text/plain")).unwrap();
  ops.store_file_buffered(&system, "/sync/delete.txt", b"delete payload", Some("text/plain")).unwrap();
  ops.store_symlink(&system, "/sync/delete-link", "/sync/delete.txt").unwrap();

  let head = engine.head_hash().unwrap();
  let tree = aeordb::engine::tree_walker::walk_version_tree(&engine, &head).unwrap();
  let (_, mut file_record): (Vec<u8>, FileRecord) = tree.files["/sync/source.txt"].clone();
  file_record.path = "/sync/received.txt".to_string();
  let file_hash = aeordb::engine::file_identity_hash(
    &file_record.path,
    file_record.content_type.as_deref(),
    &file_record.chunk_hashes,
    &engine.hash_algo(),
  )
  .unwrap();
  let symlink_record =
    SymlinkRecord { path: "/sync/received-link".to_string(), target: "/sync/received.txt".to_string(), created_at: 1, updated_at: 1 };
  let symlink_hash = aeordb::engine::symlink_identity_hash(&symlink_record.path, &symlink_record.target, &engine.hash_algo()).unwrap();
  let operations = vec![
    MergeOp::AddFile { path: file_record.path.clone(), file_hash, file_record },
    MergeOp::AddSymlink { path: symlink_record.path.clone(), symlink_hash, symlink_record },
    MergeOp::DeleteFile { path: "/sync/delete.txt".to_string() },
    MergeOp::DeleteSymlink { path: "/sync/delete-link".to_string() },
  ];
  let mut receiver = bus.subscribe();

  let events = assert_one_wave_two_mutation(&engine, &mut receiver, "sync_apply", &["entries_created", "entries_deleted"], || {
    apply_merge_operations(&engine, &ctx, &operations).unwrap()
  })
  .await;

  let created_paths: std::collections::HashSet<_> =
    events[0].payload["entries"].as_array().unwrap().iter().map(|entry| entry["path"].as_str().unwrap()).collect();
  assert_eq!(created_paths, std::collections::HashSet::from(["/sync/received.txt", "/sync/received-link"]));
  let deleted_paths: std::collections::HashSet<_> =
    events[1].payload["entries"].as_array().unwrap().iter().map(|entry| entry["path"].as_str().unwrap()).collect();
  assert_eq!(deleted_paths, std::collections::HashSet::from(["/sync/delete.txt", "/sync/delete-link"]));
}

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
  ops
    .store_file_compressed(
      &ctx,
      "/compressed.txt",
      b"hello world hello world hello world",
      Some("text/plain"),
      aeordb::engine::CompressionAlgorithm::Zstd,
    )
    .unwrap();

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
  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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
async fn wave_three_version_mutations_emit_one_exact_acknowledgement() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let mut receiver = bus.subscribe();
  let vm = VersionManager::new(&engine);

  assert_one_version_mutation(&engine, &mut receiver, "system_write", &["versions_created"], || {
    vm.create_snapshot(&ctx, "wave-three", HashMap::new()).unwrap();
  })
  .await;

  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/wave-three.txt", b"new head", Some("text/plain")).unwrap();
  assert_one_version_mutation(&engine, &mut receiver, "restore", &["versions_restored"], || {
    vm.restore_snapshot(&ctx, "wave-three").unwrap();
  })
  .await;

  assert_one_version_mutation(&engine, &mut receiver, "system_write", &["versions_deleted"], || {
    vm.delete_snapshot(&ctx, "wave-three").unwrap();
  })
  .await;

  assert_one_version_mutation(&engine, &mut receiver, "system_write", &["versions_created"], || {
    vm.create_fork(&ctx, "abandon-me", None).unwrap();
  })
  .await;
  assert_one_version_mutation(&engine, &mut receiver, "system_write", &["versions_deleted"], || {
    vm.abandon_fork(&ctx, "abandon-me").unwrap();
  })
  .await;

  let promote_fork = vm.create_fork(&ctx, "promote-me", None).unwrap();
  let create_event = receiver.recv().await.unwrap();
  assert_namespace_acknowledgement(&create_event, "system_write");
  let promoted_root = promote_fork.root_hash;
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), "/advance-before-promote.txt", b"advance HEAD", Some("text/plain"))
    .unwrap();
  assert_ne!(engine.head_hash().unwrap(), promoted_root);
  assert_one_version_mutation(&engine, &mut receiver, "promote", &["versions_promoted", "versions_deleted"], || {
    vm.promote_fork(&ctx, "promote-me").unwrap();
  })
  .await;

  assert_eq!(engine.head_hash().unwrap(), promoted_root);
  assert!(vm.get_fork_hash("promote-me").unwrap().is_none());
}

#[tokio::test]
async fn wave_three_file_restores_emit_one_exact_acknowledgement() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();

  operations.store_file_buffered(&system, "/historical.txt", b"old", Some("text/plain")).unwrap();
  let historical = operations.get_metadata("/historical.txt").unwrap().unwrap();
  operations.store_file_buffered(&system, "/historical.txt", b"new", Some("text/plain")).unwrap();
  let mut receiver = bus.subscribe();
  let historical_events = assert_one_wave_two_mutation(&engine, &mut receiver, "restore", &["entries_created"], || {
    operations.restore_file_from_record(&ctx, "/historical.txt", &historical).unwrap();
  })
  .await;
  assert_eq!(historical_events[0].payload["entries"][0]["path"], "/historical.txt");
  assert_eq!(operations.read_file_buffered("/historical.txt").unwrap(), b"old");

  operations.store_file_buffered(&system, "/deleted.txt", b"deleted", Some("text/plain")).unwrap();
  operations.delete_file(&system, "/deleted.txt").unwrap();
  let deleted_events = assert_one_wave_two_mutation(&engine, &mut receiver, "restore", &["entries_created"], || {
    operations.restore_deleted_file(&ctx, "/deleted.txt").unwrap();
  })
  .await;
  assert_eq!(deleted_events[0].payload["entries"][0]["path"], "/deleted.txt");
  assert_eq!(operations.read_file_buffered("/deleted.txt").unwrap(), b"deleted");
}

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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
  assert!(result.is_err(), "should timeout — no events for failed operation");
}

#[tokio::test]
async fn test_restore_snapshot_emits_version_restored() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), "/after-snapshot.txt", b"advance HEAD", Some("text/plain"))
    .unwrap();

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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
  assert!(result.is_err(), "should timeout — no events for failed operation");
}

#[tokio::test]
async fn test_restore_current_snapshot_is_a_true_no_op() {
  let (engine, bus, ctx, _temp) = setup_with_events();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "current", HashMap::new()).unwrap();

  let mut rx = bus.subscribe();
  let durability_before = engine.durability_snapshot().unwrap();
  let writes_before = engine.counters().snapshot().writes_total;
  vm.restore_snapshot(&ctx, "current").unwrap();

  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, durability_before.next_sequence);
  assert_eq!(engine.counters().snapshot().writes_total, writes_before);
  assert!(
    tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await.is_err(),
    "an unchanged HEAD must not emit a restore acknowledgement"
  );
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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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
  assert_namespace_acknowledgement(&event, "import");

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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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
  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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
  assert_ne!(event1.payload["entries"][0]["path"], event2.payload["entries"][0]["path"],);
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
  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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

  let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
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
  assert_eq!(event.payload["entries"][0]["hash"], blake3::hash(b"").to_hex().to_string());
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

  assert_eq!(e1.event_type, "entries_created"); // store_file
  assert_eq!(e2.event_type, "versions_created"); // create_snapshot
  assert_eq!(e3.event_type, "entries_created"); // create_directory
  assert_eq!(e4.event_type, "entries_deleted"); // delete_file
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
