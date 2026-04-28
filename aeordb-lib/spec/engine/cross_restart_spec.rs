use std::collections::HashMap;
use std::sync::Arc;

use aeordb::engine::compression::CompressionAlgorithm;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::system_store;
use aeordb::engine::user::User;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::RequestContext;
use aeordb::auth::api_key::{generate_api_key, hash_api_key, verify_api_key, ApiKeyRecord};

use chrono::Utc;
use uuid::Uuid;

/// Create a fresh engine + temp dir for first session.
fn create_engine(temp_dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let engine_file = temp_dir.path().join("test.aeordb");
  let engine_path = engine_file.to_str().unwrap();
  let engine = StorageEngine::create(engine_path).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

/// Reopen an existing engine file (simulates restart).
fn reopen_engine(temp_dir: &tempfile::TempDir) -> Arc<StorageEngine> {
  let engine_file = temp_dir.path().join("test.aeordb");
  let engine_path = engine_file.to_str().unwrap();
  let engine = StorageEngine::open(engine_path).expect("reopen should work");
  Arc::new(engine)
}

// =============================================================================
// Files & Directories
// =============================================================================

#[test]
fn test_files_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store files
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/hello.txt", b"Hello, world!", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/data/record.json", b"{\"id\": 1}", Some("application/json")).unwrap();
    ops.store_file(&ctx, "/empty.txt", b"", Some("text/plain")).unwrap();
  }

  // Session 2: reopen and verify
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let content1 = ops.read_file("/hello.txt").unwrap();
  assert_eq!(content1, b"Hello, world!");

  let content2 = ops.read_file("/data/record.json").unwrap();
  assert_eq!(content2, b"{\"id\": 1}");

  let content3 = ops.read_file("/empty.txt").unwrap();
  assert!(content3.is_empty());
}

#[test]
fn test_directories_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store files creating directory structure
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/docs/readme.txt", b"readme content", None).unwrap();
    ops.store_file(&ctx, "/docs/guide.txt", b"guide content", None).unwrap();
    ops.store_file(&ctx, "/src/main.rs", b"fn main() {}", None).unwrap();
  }

  // Session 2: verify directory listings
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let root_children = ops.list_directory("/").unwrap();
  let root_names: Vec<&str> = root_children.iter().map(|c| c.name.as_str()).collect();
  assert!(root_names.contains(&"docs"), "root should contain 'docs', got: {:?}", root_names);
  assert!(root_names.contains(&"src"), "root should contain 'src', got: {:?}", root_names);

  let docs_children = ops.list_directory("/docs").unwrap();
  let docs_names: Vec<&str> = docs_children.iter().map(|c| c.name.as_str()).collect();
  assert!(docs_names.contains(&"readme.txt"));
  assert!(docs_names.contains(&"guide.txt"));
  assert_eq!(docs_names.len(), 2);
}

#[test]
fn test_head_persists_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let head_before;

  // Session 1: store files, capture HEAD
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/file.txt", b"content", None).unwrap();
    head_before = engine.head_hash().unwrap();
    assert!(!head_before.is_empty());
  }

  // Session 2: HEAD should match
  let engine = reopen_engine(&dir);
  let head_after = engine.head_hash().unwrap();
  assert_eq!(head_before, head_after, "HEAD hash should survive restart");
}

// =============================================================================
// Deletions
// =============================================================================

#[test]
fn test_deleted_files_stay_deleted() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store and delete a file
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/ephemeral.txt", b"temporary data", None).unwrap();

    // Verify it exists before deletion
    let content = ops.read_file("/ephemeral.txt").unwrap();
    assert_eq!(content, b"temporary data");

    ops.delete_file(&ctx, "/ephemeral.txt").unwrap();

    // Verify it's gone in this session
    let result = ops.read_file("/ephemeral.txt");
    assert!(result.is_err(), "deleted file should not be readable");
  }

  // Session 2: deleted file should stay deleted
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let result = ops.read_file("/ephemeral.txt");
  assert!(result.is_err(), "deleted file should remain inaccessible after restart");
}

#[test]
fn test_deleted_files_not_in_directory_listing() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store two files, delete one
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/keep.txt", b"keeper", None).unwrap();
    ops.store_file(&ctx, "/remove.txt", b"removable", None).unwrap();
    ops.delete_file(&ctx, "/remove.txt").unwrap();
  }

  // Session 2: only the surviving file should appear
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let children = ops.list_directory("/").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"keep.txt"), "surviving file should be listed");
  assert!(!names.contains(&"remove.txt"), "deleted file should NOT be listed");
}

#[test]
fn test_has_entry_returns_false_for_deleted() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let file_key;

  // Session 1: store, capture hash, delete
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/doomed.txt", b"doomed", None).unwrap();

    let algo = engine.hash_algo();
    file_key = algo.compute_hash(b"file:/doomed.txt").unwrap();
    assert!(engine.has_entry(&file_key).unwrap(), "file should exist before deletion");

    ops.delete_file(&ctx, "/doomed.txt").unwrap();
    assert!(!engine.has_entry(&file_key).unwrap(), "file should be gone after deletion");
  }

  // Session 2: has_entry should still return false
  let engine = reopen_engine(&dir);
  assert!(!engine.has_entry(&file_key).unwrap(), "deleted file should remain deleted after restart");
}

// =============================================================================
// Snapshots
// =============================================================================

#[test]
fn test_snapshots_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: create snapshots
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/base.txt", b"base content", None).unwrap();

    let vm = VersionManager::new(&engine);
    let mut meta = HashMap::new();
    meta.insert("author".to_string(), "test".to_string());
    vm.create_snapshot(&ctx, "v1.0", meta).unwrap();

    ops.store_file(&ctx, "/extra.txt", b"extra content", None).unwrap();
    vm.create_snapshot(&ctx, "v2.0", HashMap::new()).unwrap();
  }

  // Session 2: list snapshots
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);
  let snapshots = vm.list_snapshots().unwrap();

  assert_eq!(snapshots.len(), 2, "both snapshots should survive restart");

  let names: Vec<&str> = snapshots.iter().map(|s| s.name.as_str()).collect();
  assert!(names.contains(&"v1.0"));
  assert!(names.contains(&"v2.0"));

  // Check metadata survived
  let v1 = snapshots.iter().find(|s| s.name == "v1.0").unwrap();
  assert_eq!(v1.metadata.get("author").map(|s| s.as_str()), Some("test"));
}

#[test]
fn test_snapshot_root_hash_preserved() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let original_root_hash;

  // Session 1: create snapshot, record root hash
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/data.txt", b"snapshot data", None).unwrap();

    let vm = VersionManager::new(&engine);
    let snapshot = vm.create_snapshot(&ctx, "pinned", HashMap::new()).unwrap();
    original_root_hash = snapshot.root_hash.clone();
  }

  // Session 2: root hash should match
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);

  let root_hash = vm.get_snapshot_hash("pinned").unwrap();
  assert_eq!(root_hash, original_root_hash, "snapshot root_hash should be preserved across restart");
}

#[test]
fn test_snapshot_tree_walkable_after_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store files, snapshot, then add more files
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/alpha.txt", b"alpha", None).unwrap();

    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Add more files AFTER snapshot
    ops.store_file(&ctx, "/beta.txt", b"beta", None).unwrap();
  }

  // Session 2: snapshot should resolve
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snap_hash = vm.get_snapshot_hash("snap1").unwrap();
  assert!(!snap_hash.is_empty(), "snapshot root hash should be non-empty");

  // The current HEAD should differ from the snapshot (we added beta.txt after)
  let head_hash = engine.head_hash().unwrap();
  assert_ne!(head_hash, snap_hash, "HEAD should differ from snapshot after adding more files");
}

// =============================================================================
// Forks
// =============================================================================

#[test]
fn test_forks_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: create forks
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/base.txt", b"base", None).unwrap();

    let vm = VersionManager::new(&engine);
    vm.create_fork(&ctx, "feature-a", None).unwrap();
    vm.create_fork(&ctx, "feature-b", None).unwrap();
  }

  // Session 2: forks should be listed
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);
  let forks = vm.list_forks().unwrap();

  assert_eq!(forks.len(), 2, "both forks should survive restart");

  let names: Vec<&str> = forks.iter().map(|f| f.name.as_str()).collect();
  assert!(names.contains(&"feature-a"));
  assert!(names.contains(&"feature-b"));
}

#[test]
fn test_fork_root_hash_preserved() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let original_root_hash;

  // Session 1: create fork, record root hash
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/data.txt", b"fork data", None).unwrap();

    let vm = VersionManager::new(&engine);
    let fork = vm.create_fork(&ctx, "my-fork", None).unwrap();
    original_root_hash = fork.root_hash.clone();
  }

  // Session 2: fork root hash should match
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);

  let root_hash = vm.get_fork_hash("my-fork").unwrap();
  assert_eq!(root_hash.as_ref(), Some(&original_root_hash), "fork root_hash should be preserved across restart");
}

// =============================================================================
// Indexes
// =============================================================================

#[test]
fn test_indexes_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: configure indexes, store JSON files
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);

    // Store index config
    let config = PathIndexConfig {
      indexes: vec![
        IndexFieldConfig {
          name: "age".to_string(),
          index_type: "u32".to_string(),
          source: None,
          min: Some(0.0),
          max: Some(200.0),
        },
      ],
      parser: None,
      parser_memory_limit: None,
      logging: false,
    };
    let config_data = config.serialize();
    ops.store_file(&ctx, "/people/.aeordb-config/indexes.json", &config_data, Some("application/json")).unwrap();

    // Store files with indexing
    ops.store_file_with_indexing(&ctx,
      "/people/alice.json",
      b"{\"age\": 30}",
      Some("application/json"),
    ).unwrap();
    ops.store_file_with_indexing(&ctx,
      "/people/bob.json",
      b"{\"age\": 25}",
      Some("application/json"),
    ).unwrap();
  }

  // Session 2: verify indexes exist
  let engine = reopen_engine(&dir);
  let im = IndexManager::new(&engine);

  let index_names = im.list_indexes("/people").unwrap();
  assert!(!index_names.is_empty(), "indexes should survive restart, got: {:?}", index_names);
}

#[test]
fn test_index_values_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: configure trigram index, store data
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);

    // Store index config with trigram
    let config = PathIndexConfig {
      indexes: vec![
        IndexFieldConfig {
          name: "name".to_string(),
          index_type: "trigram".to_string(),
          source: None,
          min: None,
          max: None,
        },
      ],
      parser: None,
      parser_memory_limit: None,
      logging: false,
    };
    let config_data = config.serialize();
    ops.store_file(&ctx, "/contacts/.aeordb-config/indexes.json", &config_data, Some("application/json")).unwrap();

    ops.store_file_with_indexing(&ctx,
      "/contacts/john.json",
      b"{\"name\": \"Jonathan Smith\"}",
      Some("application/json"),
    ).unwrap();
  }

  // Session 2: index should be loadable with values
  let engine = reopen_engine(&dir);
  let im = IndexManager::new(&engine);

  let index = im.load_index("/contacts", "name").unwrap();
  assert!(index.is_some(), "trigram index should survive restart");

  let idx = index.unwrap();
  assert!(!idx.entries.is_empty(), "index entries should survive restart");
  assert!(!idx.values.is_empty(), "values map should survive restart (used for fuzzy recheck)");
}

// =============================================================================
// Compression
// =============================================================================

#[test]
fn test_compressed_files_readable_after_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store a file with compression
  let large_content = "Hello, compressed world! ".repeat(100);
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file_compressed(&ctx,
      "/compressed.txt",
      large_content.as_bytes(),
      Some("text/plain"),
      CompressionAlgorithm::Zstd,
    ).unwrap();
  }

  // Session 2: read should return decompressed content
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let content = ops.read_file("/compressed.txt").unwrap();
  assert_eq!(content, large_content.as_bytes(), "compressed file should decompress correctly after restart");
}

// =============================================================================
// System tables
// =============================================================================

#[test]
fn test_users_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let user_id;

  // Session 1: create users
  {
    let engine = create_engine(&dir);

    let user = User::new("alice", Some("alice@example.com"));
    user_id = user.user_id;
    system_store::store_user(&engine, &ctx, &user).unwrap();

    let user2 = User::new("bob", None);
    system_store::store_user(&engine, &ctx, &user2).unwrap();
  }

  // Session 2: list users
  let engine = reopen_engine(&dir);

  let users = system_store::list_users(&engine).unwrap();
  assert_eq!(users.len(), 2, "both users should survive restart");

  let usernames: Vec<&str> = users.iter().map(|u| u.username.as_str()).collect();
  assert!(usernames.contains(&"alice"));
  assert!(usernames.contains(&"bob"));

  // Verify we can look up by ID
  let alice = system_store::get_user(&engine, &user_id).unwrap();
  assert!(alice.is_some(), "user should be retrievable by ID after restart");
  assert_eq!(alice.unwrap().email.as_deref(), Some("alice@example.com"));
}

#[test]
fn test_api_keys_persist_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let key_id = Uuid::new_v4();
  let plaintext_key;

  // Session 1: create API key
  {
    let engine = create_engine(&dir);

    // Create a user first (API keys need non-nil user_id)
    let user = User::new("keyowner", None);
    system_store::store_user(&engine, &ctx, &user).unwrap();

    plaintext_key = generate_api_key(key_id);
    let key_hash = hash_api_key(&plaintext_key).unwrap();

    let record = ApiKeyRecord {
      key_id,
      key_hash,
      user_id: Some(user.user_id),
      created_at: Utc::now(),
      is_revoked: false,
      expires_at: i64::MAX,
      label: None,
      rules: vec![],
    };
    system_store::store_api_key(&engine, &ctx, &record).unwrap();
  }

  // Session 2: validate the key
  let engine = reopen_engine(&dir);

  let keys = system_store::list_api_keys(&engine).unwrap();
  assert_eq!(keys.len(), 1, "API key should survive restart");
  assert_eq!(keys[0].key_id, key_id);

  // Verify the hash still validates
  let valid = verify_api_key(&plaintext_key, &keys[0].key_hash).unwrap();
  assert!(valid, "API key hash should still verify after restart");
}

// =============================================================================
// Backup metadata
// =============================================================================

#[test]
fn test_backup_type_persists_across_restart() {
  let dir = tempfile::tempdir().unwrap();
  let base_hash = vec![0xAA; 32];
  let target_hash = vec![0xBB; 32];

  // Session 1: set backup info
  {
    let engine = create_engine(&dir);
    engine.set_backup_info(1, &base_hash, &target_hash).unwrap();

    let (bt, bh, th) = engine.backup_info().unwrap();
    assert_eq!(bt, 1);
    assert_eq!(bh, base_hash);
    assert_eq!(th, target_hash);
  }

  // Session 2: backup info should survive
  let engine = reopen_engine(&dir);
  let (bt, bh, th) = engine.backup_info().unwrap();
  assert_eq!(bt, 1, "backup_type should persist across restart");
  assert_eq!(bh, base_hash, "base_hash should persist across restart");
  assert_eq!(th, target_hash, "target_hash should persist across restart");
}

// =============================================================================
// Complex scenario
// =============================================================================

#[test]
fn test_complex_scenario_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let snapshot_root_hash;
  let fork_root_hash;
  let user_id;

  // Session 1: do many things
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);

    // Store files
    ops.store_file(&ctx, "/project/README.md", b"# My Project", Some("text/markdown")).unwrap();
    ops.store_file(&ctx, "/project/src/lib.rs", b"pub fn hello() {}", Some("text/x-rust")).unwrap();
    ops.store_file(&ctx, "/project/temp.log", b"log data", Some("text/plain")).unwrap();

    // Create snapshot before deletion
    let vm = VersionManager::new(&engine);
    let mut meta = HashMap::new();
    meta.insert("version".to_string(), "1.0".to_string());
    let snap = vm.create_snapshot(&ctx, "release-1.0", meta).unwrap();
    snapshot_root_hash = snap.root_hash.clone();

    // Delete a file
    ops.delete_file(&ctx, "/project/temp.log").unwrap();

    // Create a fork
    let fork = vm.create_fork(&ctx, "experiment", None).unwrap();
    fork_root_hash = fork.root_hash.clone();

    // Create second snapshot after deletion
    vm.create_snapshot(&ctx, "release-1.1", HashMap::new()).unwrap();

    // Store compressed file
    let big_data = "repeated data ".repeat(200);
    ops.store_file_compressed(&ctx,
      "/project/large.bin",
      big_data.as_bytes(),
      Some("application/octet-stream"),
      CompressionAlgorithm::Zstd,
    ).unwrap();

    // Create a user

    let user = User::new("admin", Some("admin@example.com"));
    user_id = user.user_id;
    system_store::store_user(&engine, &ctx, &user).unwrap();
  }

  // Session 2: verify everything
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Files that should exist
  let readme = ops.read_file("/project/README.md").unwrap();
  assert_eq!(readme, b"# My Project");

  let lib = ops.read_file("/project/src/lib.rs").unwrap();
  assert_eq!(lib, b"pub fn hello() {}");

  // Deleted file should NOT exist
  let deleted = ops.read_file("/project/temp.log");
  assert!(deleted.is_err(), "deleted file should remain deleted after restart");

  // Compressed file should be readable
  let large = ops.read_file("/project/large.bin").unwrap();
  let expected = "repeated data ".repeat(200);
  assert_eq!(large, expected.as_bytes());

  // Directory listing should not include deleted file
  let project_children = ops.list_directory("/project").unwrap();
  let names: Vec<&str> = project_children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"README.md"));
  assert!(names.contains(&"src"));
  assert!(names.contains(&"large.bin"));
  assert!(!names.contains(&"temp.log"), "deleted file should not be in directory listing");

  // Snapshots should survive
  let snapshots = vm.list_snapshots().unwrap();
  assert_eq!(snapshots.len(), 2, "both snapshots should survive restart");
  let snap_names: Vec<&str> = snapshots.iter().map(|s| s.name.as_str()).collect();
  assert!(snap_names.contains(&"release-1.0"));
  assert!(snap_names.contains(&"release-1.1"));

  // Snapshot root hash should match
  let restored_hash = vm.get_snapshot_hash("release-1.0").unwrap();
  assert_eq!(restored_hash, snapshot_root_hash);

  // Snapshot metadata should survive
  let release1 = snapshots.iter().find(|s| s.name == "release-1.0").unwrap();
  assert_eq!(release1.metadata.get("version").map(|s| s.as_str()), Some("1.0"));

  // Fork should survive
  let forks = vm.list_forks().unwrap();
  assert_eq!(forks.len(), 1, "fork should survive restart");
  assert_eq!(forks[0].name, "experiment");

  let fork_hash = vm.get_fork_hash("experiment").unwrap();
  assert_eq!(fork_hash.as_ref(), Some(&fork_root_hash));

  // User should survive
  let user = system_store::get_user(&engine, &user_id).unwrap();
  assert!(user.is_some());
  assert_eq!(user.unwrap().username, "admin");
}

// =============================================================================
// Edge cases
// =============================================================================

#[test]
fn test_overwritten_file_persists_latest_version() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store then overwrite a file
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/mutable.txt", b"version 1", None).unwrap();
    ops.store_file(&ctx, "/mutable.txt", b"version 2", None).unwrap();
  }

  // Session 2: should read the latest version
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let content = ops.read_file("/mutable.txt").unwrap();
  assert_eq!(content, b"version 2", "should read latest version after restart");
}

#[test]
fn test_multiple_deletions_stay_deleted() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store and delete multiple files
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/a.txt", b"a", None).unwrap();
    ops.store_file(&ctx, "/b.txt", b"b", None).unwrap();
    ops.store_file(&ctx, "/c.txt", b"c", None).unwrap();
    ops.delete_file(&ctx, "/a.txt").unwrap();
    ops.delete_file(&ctx, "/c.txt").unwrap();
  }

  // Session 2: only /b.txt should survive
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  assert!(ops.read_file("/a.txt").is_err());
  assert_eq!(ops.read_file("/b.txt").unwrap(), b"b");
  assert!(ops.read_file("/c.txt").is_err());

  let children = ops.list_directory("/").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert_eq!(names, vec!["b.txt"]);
}

#[test]
fn test_store_delete_recreate_persists() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: store, delete, then recreate with different content
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/phoenix.txt", b"original", None).unwrap();
    ops.delete_file(&ctx, "/phoenix.txt").unwrap();
    ops.store_file(&ctx, "/phoenix.txt", b"reborn", None).unwrap();
  }

  // Session 2: should read the recreated version
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let content = ops.read_file("/phoenix.txt").unwrap();
  assert_eq!(content, b"reborn", "recreated file should persist after restart");
}

#[test]
fn test_empty_database_reopens_cleanly() {
  let dir = tempfile::tempdir().unwrap();

  // Session 1: create database, store nothing except root directory
  {
    let _engine = create_engine(&dir);
  }

  // Session 2: should open fine
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Root directory should exist with no children
  let root = ops.list_directory("/").unwrap();
  assert!(root.is_empty(), "fresh database should have empty root");
}

#[test]
fn test_large_file_persists_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Create a file larger than the chunk size (256KB) to exercise multi-chunk storage
  let large_data: Vec<u8> = (0..300_000u32).map(|i| (i % 256) as u8).collect();

  // Session 1: store large file
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/big.bin", &large_data, Some("application/octet-stream")).unwrap();
  }

  // Session 2: verify all chunks reassemble correctly
  let engine = reopen_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let content = ops.read_file("/big.bin").unwrap();
  assert_eq!(content.len(), large_data.len(), "large file length should match");
  assert_eq!(content, large_data, "large file content should match byte-for-byte");
}

#[test]
fn test_deleted_snapshot_stays_deleted_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: create then delete a snapshot
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/data.txt", b"data", None).unwrap();

    let vm = VersionManager::new(&engine);
    vm.create_snapshot(&ctx, "doomed-snap", HashMap::new()).unwrap();
    vm.create_snapshot(&ctx, "keeper-snap", HashMap::new()).unwrap();
    vm.delete_snapshot(&ctx, "doomed-snap").unwrap();

    // Verify only one remains in session
    let snaps = vm.list_snapshots().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].name, "keeper-snap");
  }

  // Session 2: deleted snapshot should stay deleted
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);
  let snaps = vm.list_snapshots().unwrap();
  assert_eq!(snaps.len(), 1, "deleted snapshot should not reappear after restart");
  assert_eq!(snaps[0].name, "keeper-snap");
}

#[test]
fn test_abandoned_fork_stays_gone_across_restart() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();

  // Session 1: create then abandon a fork
  {
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/data.txt", b"data", None).unwrap();

    let vm = VersionManager::new(&engine);
    vm.create_fork(&ctx, "keep-fork", None).unwrap();
    vm.create_fork(&ctx, "abandon-fork", None).unwrap();
    vm.abandon_fork(&ctx, "abandon-fork").unwrap();
  }

  // Session 2: abandoned fork should stay gone
  let engine = reopen_engine(&dir);
  let vm = VersionManager::new(&engine);
  let forks = vm.list_forks().unwrap();
  assert_eq!(forks.len(), 1, "abandoned fork should not reappear");
  assert_eq!(forks[0].name, "keep-fork");
}
