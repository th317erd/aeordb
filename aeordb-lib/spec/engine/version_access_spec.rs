use std::collections::HashMap;

use aeordb::engine::compression::CompressionAlgorithm;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_access::{read_file_at_version, resolve_file_at_version};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let path = dir.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

#[test]
fn test_resolve_file_at_root_level() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file(&ctx, "/readme.txt", b"hello readme", None).unwrap();
    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    let (hash, file_record) = resolve_file_at_version(&engine, &snapshot.root_hash, "/readme.txt").unwrap();
    assert!(!hash.is_empty());
    assert_eq!(file_record.path, "/readme.txt");
    assert_eq!(file_record.total_size, 12);
}

#[test]
fn test_resolve_file_nested() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file(&ctx, "/a/b/c/file.txt", b"deep content", None).unwrap();
    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    let (hash, file_record) = resolve_file_at_version(&engine, &snapshot.root_hash, "/a/b/c/file.txt").unwrap();
    assert!(!hash.is_empty());
    assert_eq!(file_record.path, "/a/b/c/file.txt");
    assert_eq!(file_record.total_size, 12);
}

#[test]
fn test_resolve_file_not_found() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let vm = VersionManager::new(&engine);

    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    let result = resolve_file_at_version(&engine, &snapshot.root_hash, "/nonexistent.txt");
    assert!(result.is_err());
}

#[test]
fn test_resolve_directory_segment_missing() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file(&ctx, "/a/b/file.txt", b"content", None).unwrap();
    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    let result = resolve_file_at_version(&engine, &snapshot.root_hash, "/a/b/missing/file.txt");
    assert!(result.is_err());
}

#[test]
fn test_resolve_file_modified_between_snapshots() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file(&ctx, "/data.txt", b"v1", None).unwrap();
    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    ops.store_file(&ctx, "/data.txt", b"v2", None).unwrap();
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    let (hash1, record1) = resolve_file_at_version(&engine, &snap1.root_hash, "/data.txt").unwrap();
    let (hash2, record2) = resolve_file_at_version(&engine, &snap2.root_hash, "/data.txt").unwrap();

    // The file record hashes should differ because content changed
    assert_ne!(hash1, hash2);
    assert_ne!(record1.chunk_hashes, record2.chunk_hashes);
}

#[test]
fn test_resolve_file_deleted_between_snapshots() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file(&ctx, "/ephemeral.txt", b"now you see me", None).unwrap();
    let snap1 = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    ops.delete_file(&ctx, "/ephemeral.txt").unwrap();
    let snap2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();

    // Should still resolve at snap1
    let result1 = resolve_file_at_version(&engine, &snap1.root_hash, "/ephemeral.txt");
    assert!(result1.is_ok());

    // Should fail at snap2 (file deleted)
    let result2 = resolve_file_at_version(&engine, &snap2.root_hash, "/ephemeral.txt");
    assert!(result2.is_err());
}

#[test]
fn test_read_file_at_version_content() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    ops.store_file(&ctx, "/greeting.txt", b"hello world", None).unwrap();
    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Modify after snapshot
    ops.store_file(&ctx, "/greeting.txt", b"goodbye", None).unwrap();

    // Reading at the snapshot should return the original content
    let content = read_file_at_version(&engine, &snapshot.root_hash, "/greeting.txt").unwrap();
    assert_eq!(content, b"hello world");
}

#[test]
fn test_read_file_at_version_compressed() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);
    let vm = VersionManager::new(&engine);

    let data = b"compressed content that should be stored with zstd";
    ops.store_file_compressed(&ctx, "/compressed.txt", data, None, CompressionAlgorithm::Zstd).unwrap();
    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    let content = read_file_at_version(&engine, &snapshot.root_hash, "/compressed.txt").unwrap();
    assert_eq!(content, data);
}

#[test]
fn test_resolve_empty_path() {
    let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let vm = VersionManager::new(&engine);

    let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

    // Empty string
    let result = resolve_file_at_version(&engine, &snapshot.root_hash, "");
    assert!(result.is_err());

    // Just a slash (normalizes to "/" with no segments)
    let result = resolve_file_at_version(&engine, &snapshot.root_hash, "/");
    assert!(result.is_err());
}
