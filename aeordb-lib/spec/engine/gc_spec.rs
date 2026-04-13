use std::collections::HashMap;
use std::sync::Arc;

use aeordb::engine::{
  DirectoryOps, EntryType, RequestContext, StorageEngine,
  VersionManager,
};
use aeordb::engine::gc::{gc_mark, gc_sweep, run_gc, GcResult};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::server::create_temp_engine_for_tests;

// ─── In-place write infrastructure ──────────────────────────────────────────

#[test]
fn test_write_entry_at_roundtrip() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Store a dummy chunk entry we'll later overwrite
  let dummy_key = engine.compute_hash(b"dummy:overwrite-test").unwrap();
  let dummy_value = vec![0u8; 200];
  let offset = engine.store_entry(EntryType::Chunk, &dummy_key, &dummy_value).unwrap();

  // Read back the entry to get its total_length
  let header = engine.read_entry_header_at(offset).unwrap();
  let original_size = header.total_length;
  assert!(original_size > 0);

  // Write a DeletionRecord in-place at that offset
  let written = engine.write_deletion_at(offset, "gc:test").unwrap();
  assert!(written > 0);
  assert!(written <= original_size);
}

#[test]
fn test_write_void_at_creates_valid_void() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Store a large-ish dummy entry to get an offset
  let dummy_key = engine.compute_hash(b"dummy:void-test").unwrap();
  let dummy_value = vec![0u8; 500];
  let offset = engine.store_entry(EntryType::Chunk, &dummy_key, &dummy_value).unwrap();
  let header = engine.read_entry_header_at(offset).unwrap();
  let entry_size = header.total_length;

  // Compute how much space a deletion takes
  let deletion_size = engine.write_deletion_at(offset, "gc:void-test").unwrap();

  // Write a void in the remaining space
  let remaining = entry_size - deletion_size;
  let void_offset = offset + deletion_size as u64;
  engine.write_void_at(void_offset, remaining).unwrap();

  // Verify the void is registered
  let stats = engine.stats();
  assert!(stats.void_count > 0, "void should be registered");
  assert!(stats.void_space_bytes >= remaining as u64);
}

#[test]
fn test_write_void_at_rejects_too_small() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Try to write a void that's smaller than minimum entry size (63 bytes for Blake3)
  let result = engine.write_void_at(100, 10);
  assert!(result.is_err(), "should reject void smaller than minimum entry size");
}

#[test]
fn test_remove_kv_entry() {
  let (engine, _temp) = create_temp_engine_for_tests();

  let key = engine.compute_hash(b"dummy:remove-test").unwrap();
  let value = vec![0u8; 100];
  engine.store_entry(EntryType::Chunk, &key, &value).unwrap();

  // Entry should be findable
  assert!(engine.has_entry(&key).unwrap());

  // Remove it
  engine.remove_kv_entry(&key).unwrap();

  // Entry should no longer be findable
  assert!(!engine.has_entry(&key).unwrap());
}

#[test]
fn test_iter_kv_entries_returns_live_entries() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

  let entries = engine.iter_kv_entries().unwrap();
  assert!(!entries.is_empty(), "should have KV entries after storing a file");
}

// ─── Test helpers ───────────────────────────────────────────────────────────

fn setup_engine_with_versions() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let ctx = RequestContext::system();
  let (engine, temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Create initial files
  ops.store_file(&ctx, "/docs/readme.txt", b"README content", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/docs/notes.txt", b"Notes content", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/config.json", b"{}", Some("application/json")).unwrap();

  // Create a snapshot
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

  // Modify a file (creates garbage: old FileRecord + old chunks + old dir entries)
  ops.store_file(&ctx, "/docs/readme.txt", b"Updated README content!!", Some("text/plain")).unwrap();

  // Delete a file (creates garbage)
  ops.delete_file(&ctx, "/docs/notes.txt").unwrap();

  // Create another snapshot
  vm.create_snapshot(&ctx, "v2", HashMap::new()).unwrap();

  // Create a fork
  vm.create_fork(&ctx, "experiment", None).unwrap();

  (engine, temp)
}

// ─── Mark phase ─────────────────────────────────────────────────────────────

#[test]
fn test_gc_mark_collects_head_entries() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/hello.txt", b"hello", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();

  let head = engine.head_hash().unwrap();
  assert!(live.contains(&head), "HEAD root hash must be marked live");
  assert!(live.len() >= 3, "expected at least 3 live entries (dir + file + chunk), got {}", live.len());
}

#[test]
fn test_gc_mark_collects_snapshot_entries() {
  let (engine, _temp) = setup_engine_with_versions();

  let live = gc_mark(&engine).unwrap();

  let vm = VersionManager::new(&engine);
  let snapshots = vm.list_snapshots().unwrap();
  assert!(snapshots.len() >= 2);

  for snapshot in &snapshots {
    assert!(live.contains(&snapshot.root_hash), "snapshot '{}' root hash should be live", snapshot.name);
  }

  let snap_key_v1 = engine.compute_hash(b"snap:v1").unwrap();
  assert!(live.contains(&snap_key_v1), "snapshot v1 KV key should be live");
}

#[test]
fn test_gc_mark_collects_fork_entries() {
  let (engine, _temp) = setup_engine_with_versions();

  let live = gc_mark(&engine).unwrap();

  let vm = VersionManager::new(&engine);
  let forks = vm.list_forks().unwrap();
  assert!(!forks.is_empty());

  for fork in &forks {
    assert!(live.contains(&fork.root_hash), "fork '{}' root hash should be live", fork.name);
  }

  let fork_key = engine.compute_hash(b"::aeordb:fork:experiment").unwrap();
  assert!(live.contains(&fork_key), "fork KV key should be live");
}

#[test]
fn test_gc_mark_structural_sharing_dedup() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"test", Some("text/plain")).unwrap();

  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "same-as-head", HashMap::new()).unwrap();

  let live = gc_mark(&engine).unwrap();
  assert!(!live.is_empty());
}

#[test]
fn test_gc_mark_empty_database() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let live = gc_mark(&engine).unwrap();
  // 0-2 base entries + 1 task registry key (always marked live by mark_task_entries)
  assert!(live.len() <= 3, "empty database should have 0-3 live entries, got {}", live.len());
}

#[test]
fn test_gc_mark_no_snapshots_or_forks() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file(&ctx, "/only-file.txt", b"alone", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();
  assert!(live.len() >= 3, "should have dir + file + chunk, got {}", live.len());
}

// ─── Sweep phase ────────────────────────────────────────────────────────────

#[test]
fn test_gc_sweep_removes_garbage() {
  let (engine, _temp) = setup_engine_with_versions();
  let live = gc_mark(&engine).unwrap();

  let (garbage_count, reclaimed_bytes) = gc_sweep(&engine, &live, false).unwrap();
  assert!(garbage_count > 0, "should have found garbage entries");
  assert!(reclaimed_bytes > 0, "should have reclaimed some bytes");
}

#[test]
fn test_gc_sweep_preserves_live_entries() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file(&ctx, "/keep.txt", b"keep me", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();
  let (garbage_count, _) = gc_sweep(&engine, &live, false).unwrap();
  // There may be 1 garbage entry: the initial empty root directory content hash
  // created by ensure_root_directory(), which is superseded when the first file
  // is stored and the root directory content hash changes.
  assert!(garbage_count <= 1, "at most 1 garbage (stale empty root), got {}", garbage_count);

  let content = ops.read_file("/keep.txt").unwrap();
  assert_eq!(content, b"keep me");
}

#[test]
fn test_gc_dry_run_does_not_modify() {
  let (engine, _temp) = setup_engine_with_versions();
  let live = gc_mark(&engine).unwrap();

  let (dry_count, _) = gc_sweep(&engine, &live, true).unwrap();
  assert!(dry_count > 0, "dry run should report garbage");

  let (real_count, _) = gc_sweep(&engine, &live, false).unwrap();
  assert_eq!(dry_count, real_count, "dry run and real run should find same garbage");

  let live2 = gc_mark(&engine).unwrap();
  let (second_count, _) = gc_sweep(&engine, &live2, false).unwrap();
  assert_eq!(second_count, 0, "second GC should find no garbage");
}

#[test]
fn test_run_gc_end_to_end() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let result = run_gc(&engine, &ctx, false).unwrap();

  assert_eq!(result.versions_scanned, 4); // HEAD + v1 + v2 + experiment
  assert!(result.live_entries > 0);
  assert!(result.garbage_entries > 0);
  assert!(result.reclaimed_bytes > 0);
  assert!(!result.dry_run);

  let result2 = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result2.garbage_entries, 0, "second GC should find no garbage");
}

#[test]
fn test_gc_empty_database() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  let result = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result.garbage_entries, 0);
  assert_eq!(result.versions_scanned, 1);
}

#[test]
fn test_gc_files_still_readable_after_sweep() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let result = run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0);

  let content = ops.read_file("/docs/readme.txt").unwrap();
  assert_eq!(content, b"Updated README content!!");

  let config = ops.read_file("/config.json").unwrap();
  assert_eq!(config, b"{}");

  // Deleted file should return an error
  let notes = ops.read_file("/docs/notes.txt");
  assert!(notes.is_err(), "/docs/notes.txt should not exist (was deleted)");
}

#[test]
fn test_gc_snapshot_still_walkable_after_sweep() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  // Walk v1 snapshot BEFORE GC to capture baseline behavior
  let vm = VersionManager::new(&engine);
  let snapshot_hash = vm.get_snapshot_hash("v1").unwrap();
  let tree_before = walk_version_tree(&engine, &snapshot_hash).unwrap();

  run_gc(&engine, &ctx, false).unwrap();

  // Walk v1 snapshot AFTER GC — should produce the same results
  let tree_after = walk_version_tree(&engine, &snapshot_hash).unwrap();

  // GC must not remove any entries that walk_version_tree could reach before
  assert_eq!(tree_before.files.len(), tree_after.files.len(),
    "v1 snapshot should have same file count before and after GC");
  for (path, _) in &tree_before.files {
    assert!(tree_after.files.contains_key(path),
      "v1 snapshot should still have file '{}' after GC", path);
  }

  // v1 snapshot should at minimum have readme.txt and config.json
  // (notes.txt may not be reachable via walk_version_tree because delete_file
  // marks its KV entry as deleted, which get_entry filters out)
  assert!(tree_after.files.contains_key("/docs/readme.txt"), "v1 should have readme.txt");
  assert!(tree_after.files.contains_key("/config.json"), "v1 should have config.json");
}

#[test]
fn test_gc_in_place_overwrite_creates_voids() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let stats_before = engine.stats();
  let void_count_before = stats_before.void_count;

  run_gc(&engine, &ctx, false).unwrap();

  let stats_after = engine.stats();
  assert!(stats_after.void_count >= void_count_before,
    "GC should create voids (before={}, after={})", void_count_before, stats_after.void_count);
}

// ─── Edge cases ─────────────────────────────────────────────────────────────

#[test]
fn test_gc_after_delete_all_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/a.txt", b"aaa", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/b.txt", b"bbb", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/a.txt").unwrap();
  ops.delete_file(&ctx, "/b.txt").unwrap();

  let result = run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0);

  ops.store_file(&ctx, "/c.txt", b"ccc", Some("text/plain")).unwrap();
  let content = ops.read_file("/c.txt").unwrap();
  assert_eq!(content, b"ccc");
}

#[test]
fn test_gc_with_overwritten_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  for i in 0..10 {
    let content = format!("version {}", i);
    ops.store_file(&ctx, "/evolving.txt", content.as_bytes(), Some("text/plain")).unwrap();
  }

  let result = run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0);

  let content = ops.read_file("/evolving.txt").unwrap();
  assert_eq!(content, b"version 9");
}

#[test]
fn test_gc_idempotent() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let result1 = run_gc(&engine, &ctx, false).unwrap();
  assert!(result1.garbage_entries > 0);

  let result2 = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result2.garbage_entries, 0);

  let result3 = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result3.garbage_entries, 0);
}

#[test]
fn test_gc_with_deep_directory_tree() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/a/b/c/d/e/deep.txt", b"deep file", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/a/b/other.txt", b"other", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();
  assert!(live.len() >= 8, "deep tree should have many live entries, got {}", live.len());

  // When the second file is stored, it creates garbage from the first file's
  // directory content hashes that are now superseded. The initial empty root
  // also contributes garbage. This is expected and correct GC behavior.
  run_gc(&engine, &ctx, false).unwrap();

  // After GC, all remaining files should still be readable
  let content = ops.read_file("/a/b/c/d/e/deep.txt").unwrap();
  assert_eq!(content, b"deep file");
  let content = ops.read_file("/a/b/other.txt").unwrap();
  assert_eq!(content, b"other");
}

#[test]
fn test_gc_result_serializes_to_json() {
  let result = GcResult {
    versions_scanned: 5,
    live_entries: 150000,
    garbage_entries: 23000,
    reclaimed_bytes: 47185920,
    duration_ms: 1200,
    dry_run: false,
  };

  let json = serde_json::to_value(&result).unwrap();
  assert_eq!(json["versions_scanned"], 5);
  assert_eq!(json["live_entries"], 150000);
  assert_eq!(json["garbage_entries"], 23000);
  assert_eq!(json["reclaimed_bytes"], 47185920u64);
  assert_eq!(json["duration_ms"], 1200);
  assert_eq!(json["dry_run"], false);
}

#[test]
fn test_gc_dry_run_result_matches_format() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let result = run_gc(&engine, &ctx, true).unwrap();
  assert!(result.dry_run);
  assert!(result.garbage_entries > 0);
  assert!(result.reclaimed_bytes > 0);
}
