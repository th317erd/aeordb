use std::collections::HashMap;

use aeordb::engine::{
  DirectoryOps, RequestContext, VersionManager,
  file_path_hash, file_content_hash, file_identity_hash,
};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::gc::run_gc;
use aeordb::server::create_temp_engine_for_tests;

// ─── Dual-key storage ───────────────────────────────────────────────────────

#[test]
fn test_file_stored_at_both_path_and_content_keys() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/dual.txt", b"dual key data", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();

  // Path-based key should resolve
  let path_key = file_path_hash("/dual.txt", &algo).unwrap();
  let path_entry = engine.get_entry(&path_key).unwrap();
  assert!(path_entry.is_some(), "path-based key should resolve");

  // Content-addressed key should also resolve
  let (_header, _key, value) = path_entry.unwrap();
  let content_key = file_content_hash(&value, &algo).unwrap();
  let content_entry = engine.get_entry(&content_key).unwrap();
  assert!(content_entry.is_some(), "content-addressed key should resolve");

  // Both should contain the same serialized FileRecord
  let (_h2, _k2, v2) = content_entry.unwrap();
  assert_eq!(value, v2, "path and content keys should store identical data");
}

#[test]
fn test_child_entry_uses_identity_hash_not_path_hash() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/check.txt", b"check content", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let _hash_length = algo.hash_length();

  // Get the path hash
  let path_key = file_path_hash("/check.txt", &algo).unwrap();

  // Walk the tree to find the ChildEntry
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();

  let (file_hash, record) = tree.files.get("/check.txt")
    .expect("file should appear in version tree");

  // The ChildEntry hash (stored in tree.files) should NOT equal the path hash
  assert_ne!(file_hash, &path_key, "ChildEntry.hash should be identity hash, not path hash");

  // It should NOT be the content hash (which includes timestamps)
  let (_header, _key, value) = engine.get_entry(&path_key).unwrap().unwrap();
  let content_key = file_content_hash(&value, &algo).unwrap();
  assert_ne!(file_hash, &content_key, "ChildEntry.hash should be identity hash, not content hash");

  // It should be the identity hash (excludes timestamps)
  let expected_identity_key = file_identity_hash("/check.txt", Some("text/plain"), &record.chunk_hashes, &algo).unwrap();
  assert_eq!(file_hash, &expected_identity_key, "ChildEntry.hash should equal computed identity hash");

  // The identity key should also resolve in the KV store (stored for tree walker lookups)
  assert!(engine.get_entry(&expected_identity_key).unwrap().is_some(),
    "identity hash should be stored in KV store for tree walker access");
}

#[test]
fn test_read_file_still_works_via_path_key() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/readable.txt", b"read me via path", Some("text/plain")).unwrap();

  // read_file uses path-based key internally — should still work
  let content = ops.read_file("/readable.txt").unwrap();
  assert_eq!(content, b"read me via path");
}

#[test]
fn test_overwrite_changes_content_key_but_path_key_still_works() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let algo = engine.hash_algo();

  // Store v1
  ops.store_file(&ctx, "/versioned.txt", b"version 1", Some("text/plain")).unwrap();
  let path_key = file_path_hash("/versioned.txt", &algo).unwrap();
  let v1_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v1_content_key = file_content_hash(&v1_value, &algo).unwrap();

  // Store v2
  ops.store_file(&ctx, "/versioned.txt", b"version 2", Some("text/plain")).unwrap();
  let v2_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v2_content_key = file_content_hash(&v2_value, &algo).unwrap();

  // Content keys should differ
  assert_ne!(v1_content_key, v2_content_key, "v1 and v2 content keys should differ");

  // Both content keys should still resolve
  assert!(engine.get_entry(&v1_content_key).unwrap().is_some(), "v1 content key should still resolve");
  assert!(engine.get_entry(&v2_content_key).unwrap().is_some(), "v2 content key should still resolve");

  // Path key should point to latest version
  let content = ops.read_file("/versioned.txt").unwrap();
  assert_eq!(content, b"version 2");
}

// ─── Snapshot versioning ────────────────────────────────────────────────────

#[test]
fn test_snapshot_preserves_historical_file_version() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store v1 and snapshot
  ops.store_file(&ctx, "/doc.txt", b"original content", Some("text/plain")).unwrap();
  vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

  // Store v2 (overwrite)
  ops.store_file(&ctx, "/doc.txt", b"updated content", Some("text/plain")).unwrap();

  // Walk snapshot — should have v1's content (size)
  let snap_hash = vm.get_snapshot_hash("snap1").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();

  let (_hash, record) = snap_tree.files.get("/doc.txt")
    .expect("snapshot should contain /doc.txt");
  assert_eq!(record.total_size, b"original content".len() as u64,
    "snapshot should have v1's size, not v2's");

  // Walk HEAD — should have v2
  let head = engine.head_hash().unwrap();
  let head_tree = walk_version_tree(&engine, &head).unwrap();
  let (_hash2, record2) = head_tree.files.get("/doc.txt")
    .expect("HEAD should contain /doc.txt");
  assert_eq!(record2.total_size, b"updated content".len() as u64,
    "HEAD should have v2's size");
}

#[test]
fn test_snapshot_file_content_readable() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store original, snapshot, overwrite
  ops.store_file(&ctx, "/data.bin", b"original bytes here", Some("application/octet-stream")).unwrap();
  vm.create_snapshot(&ctx, "before-overwrite", HashMap::new()).unwrap();
  ops.store_file(&ctx, "/data.bin", b"new bytes completely different", Some("application/octet-stream")).unwrap();

  // Walk snapshot to get the file record
  let snap_hash = vm.get_snapshot_hash("before-overwrite").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  let (_file_hash, file_record) = snap_tree.files.get("/data.bin")
    .expect("snapshot should contain /data.bin");

  // Read chunks from the snapshot's file record
  let mut content = Vec::new();
  for chunk_hash in &file_record.chunk_hashes {
    let (_header, _key, chunk_data) = engine.get_entry(chunk_hash).unwrap()
      .expect("chunk should exist");
    content.extend_from_slice(&chunk_data);
  }

  assert_eq!(content, b"original bytes here",
    "snapshot chunks should yield original content");
}

#[test]
fn test_deleted_file_snapshot_still_has_it() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Store file, snapshot, delete
  ops.store_file(&ctx, "/ephemeral.txt", b"here today", Some("text/plain")).unwrap();
  vm.create_snapshot(&ctx, "has-file", HashMap::new()).unwrap();
  ops.delete_file(&ctx, "/ephemeral.txt").unwrap();

  // HEAD should NOT have the file
  let head = engine.head_hash().unwrap();
  let head_tree = walk_version_tree(&engine, &head).unwrap();
  assert!(!head_tree.files.contains_key("/ephemeral.txt"),
    "HEAD should not contain deleted file");

  // Snapshot should still have it
  let snap_hash = vm.get_snapshot_hash("has-file").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  assert!(snap_tree.files.contains_key("/ephemeral.txt"),
    "snapshot should still contain the file");

  let (_hash, record) = snap_tree.files.get("/ephemeral.txt").unwrap();
  assert_eq!(record.total_size, b"here today".len() as u64);
}

// ─── GC ─────────────────────────────────────────────────────────────────────

#[test]
fn test_gc_preserves_path_keys_for_live_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/survive.txt", b"survive gc", Some("text/plain")).unwrap();

  run_gc(&engine, &ctx, false).unwrap();

  // Path key should still work
  let algo = engine.hash_algo();
  let path_key = file_path_hash("/survive.txt", &algo).unwrap();
  assert!(engine.has_entry(&path_key).unwrap(), "path key should survive GC");

  // read_file should still work
  let content = ops.read_file("/survive.txt").unwrap();
  assert_eq!(content, b"survive gc");
}

#[test]
fn test_gc_sweeps_old_content_keys_after_overwrite() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let algo = engine.hash_algo();

  // Store v1
  ops.store_file(&ctx, "/mutable.txt", b"v1 data", Some("text/plain")).unwrap();
  let path_key = file_path_hash("/mutable.txt", &algo).unwrap();
  let v1_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v1_content_key = file_content_hash(&v1_value, &algo).unwrap();

  // Store v2 (overwrite)
  ops.store_file(&ctx, "/mutable.txt", b"v2 data updated", Some("text/plain")).unwrap();
  let v2_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v2_content_key = file_content_hash(&v2_value, &algo).unwrap();

  // Before GC, v1 content key should still exist
  assert!(engine.has_entry(&v1_content_key).unwrap(), "v1 content key should exist before GC");

  // Run GC — no snapshots reference v1, so it should be swept
  run_gc(&engine, &ctx, false).unwrap();

  // v1 content key should be swept (no snapshot references it)
  assert!(!engine.has_entry(&v1_content_key).unwrap(),
    "v1 content key should be swept by GC (unreferenced)");

  // v2 should still be readable
  let content = ops.read_file("/mutable.txt").unwrap();
  assert_eq!(content, b"v2 data updated");
  assert!(engine.has_entry(&v2_content_key).unwrap(), "v2 content key should survive GC");
}

#[test]
fn test_gc_preserves_snapshot_content_keys() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  let algo = engine.hash_algo();

  // Store v1 and snapshot
  ops.store_file(&ctx, "/pinned.txt", b"v1 pinned", Some("text/plain")).unwrap();
  let path_key = file_path_hash("/pinned.txt", &algo).unwrap();
  let v1_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v1_content_key = file_content_hash(&v1_value, &algo).unwrap();

  vm.create_snapshot(&ctx, "pin", HashMap::new()).unwrap();

  // Store v2
  ops.store_file(&ctx, "/pinned.txt", b"v2 pinned updated", Some("text/plain")).unwrap();

  // Run GC
  run_gc(&engine, &ctx, false).unwrap();

  // v1 content key should NOT be swept — snapshot references it
  assert!(engine.has_entry(&v1_content_key).unwrap(),
    "v1 content key should survive GC because snapshot references it");

  // Snapshot tree should still be walkable and have v1 data
  let snap_hash = vm.get_snapshot_hash("pin").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  let (_hash, record) = snap_tree.files.get("/pinned.txt")
    .expect("snapshot should still contain /pinned.txt after GC");
  assert_eq!(record.total_size, b"v1 pinned".len() as u64,
    "snapshot should have v1 size after GC");

  // HEAD should have v2
  let content = ops.read_file("/pinned.txt").unwrap();
  assert_eq!(content, b"v2 pinned updated");
}
