use std::sync::Arc;

use aeordb::engine::{
  Cache, DirectoryOps, GroupLoader, PathPermissions, PermissionLink,
  PermissionsLoader, RequestContext, StorageEngine,
  directory_path_hash, file_path_hash,
};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a fresh engine with a root directory already bootstrapped.
fn test_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
  create_temp_engine_for_tests()
}

fn system_ctx() -> RequestContext {
  RequestContext::system()
}

/// Write a .aeordb-permissions file at a given directory path.
fn write_permissions(engine: &StorageEngine, dir_path: &str, permissions: &PathPermissions) {
  let ctx = system_ctx();
  let ops = DirectoryOps::new(engine);
  let perm_path = if dir_path == "/" || dir_path.ends_with('/') {
    format!("{}.aeordb-permissions", dir_path)
  } else {
    format!("{}/.aeordb-permissions", dir_path)
  };
  let data = permissions.serialize();
  ops.store_file_buffered(&ctx, &perm_path, &data, Some("application/json")).unwrap();
}

fn member_link(group: &str, allow: &str, deny: &str) -> PermissionLink {
  PermissionLink {
    group: group.to_string(),
    allow: allow.to_string(),
    deny: deny.to_string(),
    others_allow: None,
    others_deny: None,
    path_pattern: None,
  }
}

// ===========================================================================
// Test 1: Cache basic behavior
// ===========================================================================

#[test]
fn test_cache_get_loads_on_miss_and_caches() {
  let (engine, _temp_dir) = test_engine();

  // Write a .aeordb-permissions file at /test/
  let permissions = PathPermissions {
    links: vec![member_link("testers", ".r..l...", "........")],
  };
  write_permissions(&engine, "/test", &permissions);

  let cache = Cache::new(PermissionsLoader);

  // First call should load from disk (cache miss)
  let result1 = cache.get(&"/test".to_string(), &engine).unwrap();
  assert!(result1.is_some(), "First call should load permissions from disk");
  assert_eq!(result1.as_ref().unwrap().links.len(), 1);
  assert_eq!(result1.as_ref().unwrap().links[0].group, "testers");

  // Second call should return cached value (no disk read needed, same result)
  let result2 = cache.get(&"/test".to_string(), &engine).unwrap();
  assert!(result2.is_some(), "Second call should return cached value");
  assert_eq!(result2.as_ref().unwrap().links[0].group, "testers");

  // Evict, then verify third call reloads from disk
  cache.evict(&"/test".to_string());
  let result3 = cache.get(&"/test".to_string(), &engine).unwrap();
  assert!(result3.is_some(), "Third call after eviction should reload");
  assert_eq!(result3.as_ref().unwrap().links[0].group, "testers");
}

#[test]
fn test_cache_returns_none_for_missing_permissions() {
  let (engine, _temp_dir) = test_engine();
  let cache = Cache::new(PermissionsLoader);

  // No permissions file at /nonexistent — should return None
  let result = cache.get(&"/nonexistent".to_string(), &engine).unwrap();
  assert!(result.is_none(), "Missing permissions file should return None");

  // Second call should return cached None
  let result2 = cache.get(&"/nonexistent".to_string(), &engine).unwrap();
  assert!(result2.is_none(), "Cached None should persist across calls");
}

#[test]
fn test_cache_reflects_updated_data_after_eviction() {
  let (engine, _temp_dir) = test_engine();

  // Write v1 permissions
  let perm_v1 = PathPermissions {
    links: vec![member_link("team_v1", ".r......", "........")],
  };
  write_permissions(&engine, "/", &perm_v1);

  let cache = Cache::new(PermissionsLoader);
  let result = cache.get(&"/".to_string(), &engine).unwrap();
  assert_eq!(result.unwrap().links[0].group, "team_v1");

  // Write v2 permissions (overwrites v1)
  let perm_v2 = PathPermissions {
    links: vec![member_link("team_v2", "crudlify", "........")],
  };
  write_permissions(&engine, "/", &perm_v2);

  // Without eviction, stale data
  let stale = cache.get(&"/".to_string(), &engine).unwrap();
  assert_eq!(stale.unwrap().links[0].group, "team_v1", "Cache should serve stale without eviction");

  // After eviction, fresh data
  cache.evict(&"/".to_string());
  let fresh = cache.get(&"/".to_string(), &engine).unwrap();
  assert_eq!(fresh.unwrap().links[0].group, "team_v2", "Cache should serve fresh after eviction");
}

// ===========================================================================
// Test 2: Cache eviction (evict and evict_all)
// ===========================================================================

#[test]
fn test_cache_evict_single_key_leaves_others() {
  let (engine, _temp_dir) = test_engine();

  // Write permissions at /a/ and /b/
  let perm_a = PathPermissions {
    links: vec![member_link("team_a", "crudlify", "........")],
  };
  let perm_b = PathPermissions {
    links: vec![member_link("team_b", ".r..l...", "........")],
  };
  write_permissions(&engine, "/a", &perm_a);
  write_permissions(&engine, "/b", &perm_b);

  let cache = Cache::new(PermissionsLoader);

  // Load both into cache
  cache.get(&"/a".to_string(), &engine).unwrap();
  cache.get(&"/b".to_string(), &engine).unwrap();

  // Update /a/ on disk
  let perm_a_v2 = PathPermissions {
    links: vec![member_link("team_a_v2", "crudlify", "........")],
  };
  write_permissions(&engine, "/a", &perm_a_v2);

  // Evict only /a/
  cache.evict(&"/a".to_string());

  // /a/ should reload and see the update
  let result_a = cache.get(&"/a".to_string(), &engine).unwrap();
  assert_eq!(
    result_a.unwrap().links[0].group, "team_a_v2",
    "/a/ should have reloaded after eviction"
  );

  // /b/ should still be cached (stale is fine since we didn't update it)
  let result_b = cache.get(&"/b".to_string(), &engine).unwrap();
  assert_eq!(
    result_b.unwrap().links[0].group, "team_b",
    "/b/ should still be cached"
  );
}

#[test]
fn test_cache_evict_all_clears_everything() {
  let (engine, _temp_dir) = test_engine();

  let perm_a = PathPermissions {
    links: vec![member_link("original_a", ".r......", "........")],
  };
  let perm_b = PathPermissions {
    links: vec![member_link("original_b", ".r......", "........")],
  };
  write_permissions(&engine, "/a", &perm_a);
  write_permissions(&engine, "/b", &perm_b);

  let cache = Cache::new(PermissionsLoader);

  // Populate cache
  cache.get(&"/a".to_string(), &engine).unwrap();
  cache.get(&"/b".to_string(), &engine).unwrap();

  // Update both on disk
  write_permissions(&engine, "/a", &PathPermissions {
    links: vec![member_link("updated_a", "crudlify", "........")],
  });
  write_permissions(&engine, "/b", &PathPermissions {
    links: vec![member_link("updated_b", "crudlify", "........")],
  });

  // evict_all
  cache.evict_all();

  // Both should reload
  let result_a = cache.get(&"/a".to_string(), &engine).unwrap();
  assert_eq!(result_a.unwrap().links[0].group, "updated_a");
  let result_b = cache.get(&"/b".to_string(), &engine).unwrap();
  assert_eq!(result_b.unwrap().links[0].group, "updated_b");
}

#[test]
fn test_cache_evict_nonexistent_key_is_noop() {
  let (engine, _temp_dir) = test_engine();
  let cache = Cache::new(PermissionsLoader);

  // Evicting a key that was never loaded should not panic
  cache.evict(&"/never_loaded".to_string());

  // Should still be able to load it fresh
  let result = cache.get(&"/never_loaded".to_string(), &engine).unwrap();
  assert!(result.is_none());
}

#[test]
fn test_cache_evict_all_on_empty_cache_is_noop() {
  let cache: Cache<PermissionsLoader> = Cache::new(PermissionsLoader);
  // Should not panic
  cache.evict_all();
}

// ===========================================================================
// Test 3: Directory hard links (store_file creates directory structure)
// ===========================================================================

#[test]
fn test_directory_hard_links_read_write() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Store a file — this should create directory entries at / and /mydir/
  ops.store_file_buffered(&ctx, "/mydir/testfile.json", b"{\"hello\":\"world\"}", Some("application/json")).unwrap();

  // List the root directory — should contain "mydir/"
  let root_entries = ops.list_directory("/").unwrap();
  let dir_names: Vec<&str> = root_entries.iter().map(|e| e.name.as_str()).collect();
  assert!(
    dir_names.contains(&"mydir") || dir_names.contains(&"mydir/"),
    "Root directory should contain mydir/, got: {:?}", dir_names
  );

  // List /mydir/ — should contain "testfile.json"
  let mydir_entries = ops.list_directory("/mydir/").unwrap();
  let file_names: Vec<&str> = mydir_entries.iter().map(|e| e.name.as_str()).collect();
  assert!(
    file_names.contains(&"testfile.json"),
    "/mydir/ should contain testfile.json, got: {:?}", file_names
  );

  // The directory hard link should be readable via the engine's get_entry.
  // directory_path_hash uses the normalized path (no trailing slash).
  let algo = engine.hash_algo();
  let dir_key = directory_path_hash("/mydir", &algo).unwrap();
  let dir_entry = engine.get_entry(&dir_key).unwrap();
  assert!(dir_entry.is_some(), "Directory data should exist at the dir hash key");
}

#[test]
fn test_directory_hard_links_nested_three_levels() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Store a file three levels deep
  ops.store_file_buffered(&ctx, "/a/b/c/deep.txt", b"deep content", Some("text/plain")).unwrap();

  // All intermediate directories should exist
  let root_entries = ops.list_directory("/").unwrap();
  assert!(root_entries.iter().any(|e| e.name == "a"), "Root should contain a");

  let a_entries = ops.list_directory("/a/").unwrap();
  assert!(a_entries.iter().any(|e| e.name == "b"), "/a/ should contain b");

  let b_entries = ops.list_directory("/a/b/").unwrap();
  assert!(b_entries.iter().any(|e| e.name == "c"), "/a/b/ should contain c");

  let c_entries = ops.list_directory("/a/b/c/").unwrap();
  assert!(c_entries.iter().any(|e| e.name == "deep.txt"), "/a/b/c/ should contain deep.txt");
}

#[test]
fn test_directory_entry_nonexistent_returns_none() {
  let (engine, _temp_dir) = test_engine();

  let algo = engine.hash_algo();
  let fake_key = directory_path_hash("/nonexistent/", &algo).unwrap();
  let result = engine.get_entry(&fake_key).unwrap();
  assert!(result.is_none(), "Non-existent directory should return None");
}

// ===========================================================================
// Test 4: Directory content cache (multiple writes to same directory)
// ===========================================================================

#[test]
fn test_directory_content_cache_multiple_writes() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Write first file in /data/
  ops.store_file_buffered(&ctx, "/data/file1.json", b"{\"a\":1}", Some("application/json")).unwrap();

  // Write second file in /data/ — the directory update should use the
  // content cache for the parent directory
  ops.store_file_buffered(&ctx, "/data/file2.json", b"{\"b\":2}", Some("application/json")).unwrap();

  // Write third file to make sure the cache stays consistent
  ops.store_file_buffered(&ctx, "/data/file3.json", b"{\"c\":3}", Some("application/json")).unwrap();

  // Verify all three files appear in the directory listing
  let entries = ops.list_directory("/data/").unwrap();
  let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
  assert!(names.contains(&"file1.json"), "Should contain file1.json, got: {:?}", names);
  assert!(names.contains(&"file2.json"), "Should contain file2.json, got: {:?}", names);
  assert!(names.contains(&"file3.json"), "Should contain file3.json, got: {:?}", names);
  assert_eq!(entries.len(), 3, "Should have exactly 3 entries");
}

#[test]
fn test_directory_content_cache_after_clear() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Write files
  ops.store_file_buffered(&ctx, "/cached/a.txt", b"aaa", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/cached/b.txt", b"bbb", Some("text/plain")).unwrap();

  // Clear the content cache
  engine.clear_dir_content_cache();

  // Write another file — should still work correctly even with a cold cache
  ops.store_file_buffered(&ctx, "/cached/c.txt", b"ccc", Some("text/plain")).unwrap();

  let entries = ops.list_directory("/cached/").unwrap();
  assert_eq!(entries.len(), 3, "All 3 files should be present after cache clear");
}

#[test]
fn test_directory_content_cache_separate_directories() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Write to two separate directories
  ops.store_file_buffered(&ctx, "/alpha/f1.txt", b"a1", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/f1.txt", b"b1", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/alpha/f2.txt", b"a2", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/f2.txt", b"b2", Some("text/plain")).unwrap();

  // Verify each directory has the right files
  let alpha = ops.list_directory("/alpha/").unwrap();
  let beta = ops.list_directory("/beta/").unwrap();
  assert_eq!(alpha.len(), 2, "/alpha/ should have 2 files");
  assert_eq!(beta.len(), 2, "/beta/ should have 2 files");
}

// ===========================================================================
// Test 5: KV expansion online
// ===========================================================================

#[test]
fn test_kv_expansion_online() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Check initial KV stage
  let stats_before = engine.stats();
  let initial_kv_size = stats_before.kv_size_bytes;

  // Write enough entries to trigger KV overflow and expansion.
  // Stage 0 is 64KB. With BLAKE3_256 (32-byte hashes), page_size ~1314 bytes,
  // so we get ~48 buckets, each holding up to 32 entries = ~1536 max entries.
  // In practice, hash collisions reduce effective capacity.
  // Write a generous number of small files to push past the threshold.
  let mut stored_paths = Vec::new();
  for i in 0..1600 {
    let path = format!("/expand/file_{:05}.txt", i);
    let data = format!("data for file {}", i);
    ops.store_file_buffered(&ctx, &path, data.as_bytes(), Some("text/plain")).unwrap();
    stored_paths.push(path);
  }

  // Check if expansion occurred (KV block grew)
  let stats_after = engine.stats();
  assert!(
    stats_after.kv_size_bytes >= initial_kv_size,
    "KV block size should not shrink: before={}, after={}",
    initial_kv_size, stats_after.kv_size_bytes
  );

  // Verify ALL data is still readable after expansion
  for (i, path) in stored_paths.iter().enumerate() {
    let data = ops.read_file_buffered(path).unwrap();
    let expected = format!("data for file {}", i);
    assert_eq!(
      std::str::from_utf8(&data).unwrap(), &expected,
      "File {} should be readable after KV expansion", path
    );
  }

  // Verify directory listing shows all files
  let entries = ops.list_directory("/expand/").unwrap();
  assert_eq!(
    entries.len(), 1600,
    "Directory listing should show all 1600 files, got {}", entries.len()
  );
}

#[test]
fn test_kv_expansion_preserves_system_data() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Write a permissions file (system data) before expansion
  let perms = PathPermissions {
    links: vec![member_link("team", "crudlify", "........")],
  };
  write_permissions(&engine, "/", &perms);

  // Trigger expansion with many files
  for i in 0..1600 {
    let path = format!("/syscheck/f_{:05}.txt", i);
    ops.store_file_buffered(&ctx, &path, b"x", Some("text/plain")).unwrap();
  }

  // Verify the permissions file is still readable
  let perm_cache = Cache::new(PermissionsLoader);
  let result = perm_cache.get(&"/".to_string(), &engine).unwrap();
  assert!(result.is_some(), "Permissions file should survive KV expansion");
  assert_eq!(result.unwrap().links[0].group, "team");
}

// ===========================================================================
// Test 6: Deleted file behavior
// ===========================================================================

#[test]
fn test_deleted_files_not_visible_in_get_entry() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Write a file
  ops.store_file_buffered(&ctx, "/deltest/target.json", b"{\"delete\":\"me\"}", Some("application/json")).unwrap();

  // Verify it exists and is readable
  let data = ops.read_file_buffered("/deltest/target.json").unwrap();
  assert_eq!(std::str::from_utf8(&data).unwrap(), "{\"delete\":\"me\"}");

  // Get its hash for later checking
  let algo = engine.hash_algo();
  let file_key = file_path_hash("/deltest/target.json", &algo).unwrap();

  // Verify it exists in the KV before deletion
  let kv_entry_before = engine.get_kv_entry(&file_key);
  assert!(kv_entry_before.is_some(), "File should exist in KV before deletion");

  // Delete the file
  ops.delete_file(&ctx, "/deltest/target.json").unwrap();

  // After deletion: get_entry should return None (filters deleted)
  let entry = engine.get_entry(&file_key);
  assert!(entry.is_ok());
  assert!(entry.unwrap().is_none(), "Deleted file should not be visible via get_entry");

  // After deletion: get_kv_entry should also return None (filters deleted flag)
  let kv_entry_after = engine.get_kv_entry(&file_key);
  assert!(kv_entry_after.is_none(), "Deleted file should not be visible via get_kv_entry");

  // get_entry_including_deleted should still find it
  let entry_raw = engine.get_entry_including_deleted(&file_key);
  assert!(entry_raw.is_ok());
  assert!(entry_raw.unwrap().is_some(), "Deleted file should be visible via get_entry_including_deleted");
}

#[test]
fn test_deleted_files_removed_from_directory_listing() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  // Write two files
  ops.store_file_buffered(&ctx, "/listing/keep.txt", b"keep", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/listing/remove.txt", b"remove", Some("text/plain")).unwrap();

  // Both should appear
  let entries = ops.list_directory("/listing/").unwrap();
  assert_eq!(entries.len(), 2);

  // Delete one
  ops.delete_file(&ctx, "/listing/remove.txt").unwrap();

  // Only the kept file should appear
  let entries_after = ops.list_directory("/listing/").unwrap();
  assert_eq!(entries_after.len(), 1, "Only one file should remain after deletion");
  assert_eq!(entries_after[0].name, "keep.txt");
}

#[test]
fn test_delete_nonexistent_file_returns_error() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  let result = ops.delete_file(&ctx, "/doesnt/exist.txt");
  assert!(result.is_err(), "Deleting a non-existent file should return an error");
}

#[test]
fn test_deleted_file_cannot_be_read() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/readtest/gone.txt", b"vanishing", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/readtest/gone.txt").unwrap();

  // read_file should fail for deleted files
  let result = ops.read_file_buffered("/readtest/gone.txt");
  assert!(result.is_err(), "Reading a deleted file should return an error");
}

// ===========================================================================
// Edge cases
// ===========================================================================

#[test]
fn test_group_cache_basic() {
  let (engine, _temp_dir) = test_engine();

  let cache = Cache::new(GroupLoader);

  // Non-existent user returns empty groups
  let fake_user = uuid::Uuid::new_v4();
  let groups = cache.get(&fake_user, &engine).unwrap();
  assert!(groups.is_empty(), "Non-existent user should have no groups");
}

#[test]
fn test_store_and_read_empty_file() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/empty/file.bin", b"", Some("application/octet-stream")).unwrap();
  let data = ops.read_file_buffered("/empty/file.bin").unwrap();
  assert!(data.is_empty(), "Empty file should read back as empty");
}

#[test]
fn test_overwrite_file_updates_content() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/overwrite/doc.txt", b"version 1", Some("text/plain")).unwrap();
  let v1 = ops.read_file_buffered("/overwrite/doc.txt").unwrap();
  assert_eq!(std::str::from_utf8(&v1).unwrap(), "version 1");

  ops.store_file_buffered(&ctx, "/overwrite/doc.txt", b"version 2", Some("text/plain")).unwrap();
  let v2 = ops.read_file_buffered("/overwrite/doc.txt").unwrap();
  assert_eq!(std::str::from_utf8(&v2).unwrap(), "version 2");
}

#[test]
fn test_directory_listing_root_after_multiple_dirs() {
  let (engine, _temp_dir) = test_engine();
  let ctx = system_ctx();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/dir1/f.txt", b"1", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/dir2/f.txt", b"2", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/dir3/f.txt", b"3", Some("text/plain")).unwrap();

  let root = ops.list_directory("/").unwrap();
  let names: Vec<&str> = root.iter().map(|e| e.name.as_str()).collect();
  assert!(names.contains(&"dir1"), "Root should contain dir1");
  assert!(names.contains(&"dir2"), "Root should contain dir2");
  assert!(names.contains(&"dir3"), "Root should contain dir3");
}
