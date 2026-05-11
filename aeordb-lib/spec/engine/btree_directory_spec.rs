use std::collections::HashMap;

use aeordb::engine::btree::{BTREE_CONVERSION_THRESHOLD, is_btree_format};
use aeordb::engine::directory_ops::{DirectoryOps, directory_path_hash};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
    let path = dir.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

fn store_n_files(engine: &StorageEngine, dir: &str, n: usize) {
    let ops = DirectoryOps::new(engine);
    let ctx = RequestContext::system();
    for i in 0..n {
        let name = format!("{}/file_{:05}.json", dir, i);
        let data = format!("{{\"idx\":{}}}", i);
        ops.store_file(&ctx, &name, data.as_bytes(), Some("application/json")).unwrap();
    }
}

/// Resolve a directory's raw value, following hard links if present. Directory
/// entries can be stored as either inline data or as a hash pointer to the
/// content-addressed entry — tests that check the on-disk format must follow
/// the link to see the actual btree/flat bytes.
fn resolve_directory_value(engine: &StorageEngine, dir_key: &[u8]) -> Vec<u8> {
    let (_, _, raw) = engine.get_entry(dir_key).unwrap().unwrap();
    let hash_length = engine.hash_algo().hash_length();
    if raw.len() == hash_length {
        // Hard link to content-hashed entry — follow it.
        engine.get_entry(&raw).unwrap().unwrap().2
    } else {
        raw
    }
}

// ---------------------------------------------------------------------------
// Flat format tests (below threshold)
// ---------------------------------------------------------------------------

#[test]
fn test_small_directory_stays_flat() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/small", 100);

    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/small/").unwrap();
    assert_eq!(children.len(), 100);

    // Verify it's still flat format by reading the raw directory data
    let algo = engine.hash_algo();
    let dir_key = directory_path_hash("/small", &algo).unwrap();
    let raw_data = resolve_directory_value(&engine, &dir_key);
    assert!(!is_btree_format(&raw_data), "directory with 100 entries should remain flat");
}

#[test]
fn test_directory_at_threshold_minus_one_stays_flat() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let count = BTREE_CONVERSION_THRESHOLD - 1;
    store_n_files(&engine, "/edge", count);

    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/edge/").unwrap();
    assert_eq!(children.len(), count);

    let algo = engine.hash_algo();
    let dir_key = directory_path_hash("/edge", &algo).unwrap();
    let raw_data = resolve_directory_value(&engine, &dir_key);
    assert!(!is_btree_format(&raw_data), "directory at threshold-1 should remain flat");
}

// ---------------------------------------------------------------------------
// B-tree conversion tests
// ---------------------------------------------------------------------------

#[test]
fn test_large_directory_converts_to_btree() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let count = BTREE_CONVERSION_THRESHOLD + 10;
    store_n_files(&engine, "/large", count);

    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/large/").unwrap();
    assert_eq!(children.len(), count);

    // Verify it's in B-tree format
    let algo = engine.hash_algo();
    let dir_key = directory_path_hash("/large", &algo).unwrap();
    let raw_data = resolve_directory_value(&engine, &dir_key);
    assert!(is_btree_format(&raw_data), "directory with {} entries should be B-tree format", count);
}

#[test]
fn test_exact_threshold_converts_to_btree() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/exact", BTREE_CONVERSION_THRESHOLD);

    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/exact/").unwrap();
    assert_eq!(children.len(), BTREE_CONVERSION_THRESHOLD);

    let algo = engine.hash_algo();
    let dir_key = directory_path_hash("/exact", &algo).unwrap();
    let raw_data = resolve_directory_value(&engine, &dir_key);
    assert!(is_btree_format(&raw_data), "directory at exact threshold should be B-tree format");
}

// ---------------------------------------------------------------------------
// Sorted output
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_list_sorted() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/sorted", 300);

    let ops = DirectoryOps::new(&engine);
    let children = ops.list_directory("/sorted/").unwrap();
    assert_eq!(children.len(), 300);

    // Verify sorted by name
    for i in 1..children.len() {
        assert!(
            children[i - 1].name <= children[i].name,
            "children should be sorted: {} <= {}",
            children[i - 1].name,
            children[i].name
        );
    }
}

// ---------------------------------------------------------------------------
// Insert after conversion
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_add_file_after_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let initial = BTREE_CONVERSION_THRESHOLD + 5;
    store_n_files(&engine, "/grow", initial);

    // Add one more after conversion
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/grow/extra.json", b"{}", Some("application/json"))
        .unwrap();

    let children = ops.list_directory("/grow/").unwrap();
    assert_eq!(children.len(), initial + 1);
    assert!(children.iter().any(|c| c.name == "extra.json"));
}

#[test]
fn test_btree_directory_add_many_after_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/grow2", BTREE_CONVERSION_THRESHOLD + 1);

    // Add 100 more files after B-tree conversion
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    for i in 0..100 {
        ops.store_file(
            &ctx,
            &format!("/grow2/extra_{:03}.json", i),
            b"{}",
            Some("application/json"),
        )
        .unwrap();
    }

    let children = ops.list_directory("/grow2/").unwrap();
    assert_eq!(children.len(), BTREE_CONVERSION_THRESHOLD + 1 + 100);

    // Still in B-tree format
    let algo = engine.hash_algo();
    let dir_key = directory_path_hash("/grow2", &algo).unwrap();
    let raw_data = resolve_directory_value(&engine, &dir_key);
    assert!(is_btree_format(&raw_data));
}

// ---------------------------------------------------------------------------
// Delete from B-tree directory
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_delete_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/del", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.delete_file(&ctx, "/del/file_00100.json").unwrap();

    let children = ops.list_directory("/del/").unwrap();
    assert_eq!(children.len(), 299);
    assert!(!children.iter().any(|c| c.name == "file_00100.json"));
}

#[test]
fn test_btree_directory_delete_first_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/delfirst", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.delete_file(&ctx, "/delfirst/file_00000.json").unwrap();

    let children = ops.list_directory("/delfirst/").unwrap();
    assert_eq!(children.len(), 299);
    assert!(!children.iter().any(|c| c.name == "file_00000.json"));
}

#[test]
fn test_btree_directory_delete_last_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/dellast", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.delete_file(&ctx, "/dellast/file_00299.json").unwrap();

    let children = ops.list_directory("/dellast/").unwrap();
    assert_eq!(children.len(), 299);
    assert!(!children.iter().any(|c| c.name == "file_00299.json"));
}

#[test]
fn test_btree_directory_delete_multiple_files() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/delmulti", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    for i in [0, 50, 100, 150, 200, 250, 299] {
        ops.delete_file(&ctx, &format!("/delmulti/file_{:05}.json", i))
            .unwrap();
    }

    let children = ops.list_directory("/delmulti/").unwrap();
    assert_eq!(children.len(), 293);
}

// ---------------------------------------------------------------------------
// Overwrite in B-tree directory
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_overwrite_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/overwrite", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(
        &ctx,
        "/overwrite/file_00050.json",
        b"new_data",
        Some("text/plain"),
    )
    .unwrap();

    let children = ops.list_directory("/overwrite/").unwrap();
    assert_eq!(children.len(), 300); // same count, not duplicated

    // Verify the content was updated
    let data = ops.read_file("/overwrite/file_00050.json").unwrap();
    assert_eq!(data, b"new_data");
}

// ---------------------------------------------------------------------------
// Read file in B-tree directory
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_read_file_works() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/read", 300);

    let ops = DirectoryOps::new(&engine);
    let data = ops.read_file("/read/file_00150.json").unwrap();
    assert_eq!(data, b"{\"idx\":150}");
}

#[test]
fn test_btree_directory_read_first_and_last_files() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/readfl", 300);

    let ops = DirectoryOps::new(&engine);
    assert_eq!(ops.read_file("/readfl/file_00000.json").unwrap(), b"{\"idx\":0}");
    assert_eq!(ops.read_file("/readfl/file_00299.json").unwrap(), b"{\"idx\":299}");
}

// ---------------------------------------------------------------------------
// Snapshot with B-tree directory
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_snapshot_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/snap", 300);

    let vm = VersionManager::new(&engine);
    let ctx = RequestContext::system();
    vm.create_snapshot(&ctx, "before", HashMap::new()).unwrap();

    // Add more files after the snapshot
    let ops = DirectoryOps::new(&engine);
    for i in 300..310 {
        ops.store_file(
            &ctx,
            &format!("/snap/file_{:05}.json", i),
            b"{}",
            Some("application/json"),
        )
        .unwrap();
    }

    // Walk the snapshot -- should see 300 files, not 310
    let snapshots = vm.list_snapshots().unwrap();
    let snap = snapshots.iter().find(|s| s.name == "before").unwrap();

    let tree = walk_version_tree(&engine, &snap.root_hash).unwrap();
    let snap_files: Vec<_> = tree
        .files
        .keys()
        .filter(|p| p.starts_with("/snap/"))
        .collect();
    assert_eq!(
        snap_files.len(),
        300,
        "snapshot should have 300 files, got {}",
        snap_files.len()
    );
}

// ---------------------------------------------------------------------------
// Mixed format coexistence
// ---------------------------------------------------------------------------

#[test]
fn test_mixed_format_coexistence() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Small directory (flat)
    store_n_files(&engine, "/small", 50);
    // Large directory (B-tree)
    store_n_files(&engine, "/large", 300);

    let ops = DirectoryOps::new(&engine);
    let small = ops.list_directory("/small/").unwrap();
    let large = ops.list_directory("/large/").unwrap();
    assert_eq!(small.len(), 50);
    assert_eq!(large.len(), 300);

    // Verify format types
    let algo = engine.hash_algo();
    let small_key = directory_path_hash("/small", &algo).unwrap();
    let large_key = directory_path_hash("/large", &algo).unwrap();
    let small_data = resolve_directory_value(&engine, &small_key);
    let large_data = resolve_directory_value(&engine, &large_key);
    assert!(!is_btree_format(&small_data));
    assert!(is_btree_format(&large_data));
}

// ---------------------------------------------------------------------------
// Root directory stays flat
// ---------------------------------------------------------------------------

#[test]
fn test_root_directory_stays_flat() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Store files in a few subdirectories
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    for i in 0..5 {
        ops.store_file(
            &ctx,
            &format!("/dir{}/file.json", i),
            b"{}",
            Some("application/json"),
        )
        .unwrap();
    }

    // Root should have 5 subdirectory children -- still flat
    let root_children = ops.list_directory("/").unwrap();
    assert_eq!(root_children.len(), 5);

    let algo = engine.hash_algo();
    let root_key = directory_path_hash("/", &algo).unwrap();
    let (_, _, root_data) = engine.get_entry(&root_key).unwrap().unwrap();
    assert!(!is_btree_format(&root_data), "root with 5 children should remain flat");
}

// ---------------------------------------------------------------------------
// Tree walker with B-tree directories
// ---------------------------------------------------------------------------

#[test]
fn test_tree_walker_traverses_btree_directories() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/walkme", 300);

    // Get current HEAD and walk the tree
    let head = engine.head_hash().unwrap();
    let tree = walk_version_tree(&engine, &head).unwrap();

    // Should find all 300 files
    let walk_files: Vec<_> = tree
        .files
        .keys()
        .filter(|p| p.starts_with("/walkme/"))
        .collect();
    assert_eq!(walk_files.len(), 300);

    // Should find the /walkme directory
    assert!(tree.directories.contains_key("/walkme"));
}

// ---------------------------------------------------------------------------
// Edge cases and error paths
// ---------------------------------------------------------------------------

#[test]
fn test_empty_directory_not_btree() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.create_directory(&ctx, "/empty").unwrap();

    let children = ops.list_directory("/empty/").unwrap();
    assert!(children.is_empty());
}

#[test]
fn test_btree_directory_file_not_found_after_delete() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/notfound", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.delete_file(&ctx, "/notfound/file_00050.json").unwrap();

    // File should not be readable
    let result = ops.read_file("/notfound/file_00050.json");
    assert!(result.is_err(), "deleted file should not be readable");
}

#[test]
fn test_btree_directory_delete_nonexistent_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/noent", 300);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    let result = ops.delete_file(&ctx, "/noent/nonexistent.json");
    assert!(result.is_err(), "deleting a nonexistent file should fail");
}

#[test]
fn test_btree_directory_exists_check() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/exists", 300);

    let ops = DirectoryOps::new(&engine);
    assert!(ops.exists("/exists/file_00000.json").unwrap());
    assert!(ops.exists("/exists/file_00299.json").unwrap());
    assert!(!ops.exists("/exists/nonexistent.json").unwrap());
}

#[test]
fn test_btree_directory_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);
    store_n_files(&engine, "/meta", 300);

    let ops = DirectoryOps::new(&engine);
    let metadata = ops.get_metadata("/meta/file_00100.json").unwrap();
    assert!(metadata.is_some());
    let record = metadata.unwrap();
    assert_eq!(record.total_size, b"{\"idx\":100}".len() as u64);
}

// ---------------------------------------------------------------------------
// Insert and delete interleaved
// ---------------------------------------------------------------------------

#[test]
fn test_btree_directory_interleaved_insert_delete() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Start with enough to trigger B-tree
    store_n_files(&engine, "/interleave", BTREE_CONVERSION_THRESHOLD + 50);

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Delete some, add some, interleaved
    for i in 0..20 {
        ops.delete_file(&ctx, &format!("/interleave/file_{:05}.json", i * 5))
            .unwrap();
        ops.store_file(
            &ctx,
            &format!("/interleave/new_{:03}.json", i),
            b"{}",
            Some("application/json"),
        )
        .unwrap();
    }

    let children = ops.list_directory("/interleave/").unwrap();
    // Original count - 20 deleted + 20 added = same count
    assert_eq!(children.len(), BTREE_CONVERSION_THRESHOLD + 50);

    // Verify new files exist
    assert!(children.iter().any(|c| c.name == "new_000.json"));
    assert!(children.iter().any(|c| c.name == "new_019.json"));

    // Verify deleted files are gone
    assert!(!children.iter().any(|c| c.name == "file_00000.json"));
    assert!(!children.iter().any(|c| c.name == "file_00095.json"));
}

// ---------------------------------------------------------------------------
// Snapshot before and after B-tree conversion
// ---------------------------------------------------------------------------

#[test]
fn test_snapshot_before_btree_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&dir);

    // Store below threshold
    let below = BTREE_CONVERSION_THRESHOLD - 1;
    store_n_files(&engine, "/conv", below);

    let vm = VersionManager::new(&engine);
    let ctx = RequestContext::system();
    vm.create_snapshot(&ctx, "flat", HashMap::new()).unwrap();

    // Now push over the threshold
    let ops = DirectoryOps::new(&engine);
    for i in below..(BTREE_CONVERSION_THRESHOLD + 50) {
        ops.store_file(
            &ctx,
            &format!("/conv/file_{:05}.json", i),
            b"{}",
            Some("application/json"),
        )
        .unwrap();
    }

    vm.create_snapshot(&ctx, "btree", HashMap::new()).unwrap();

    let snapshots = vm.list_snapshots().unwrap();
    let flat_snap = snapshots.iter().find(|s| s.name == "flat").unwrap();
    let btree_snap = snapshots.iter().find(|s| s.name == "btree").unwrap();

    let flat_tree = walk_version_tree(&engine, &flat_snap.root_hash).unwrap();
    let btree_tree = walk_version_tree(&engine, &btree_snap.root_hash).unwrap();

    let flat_files: Vec<_> = flat_tree.files.keys().filter(|p| p.starts_with("/conv/")).collect();
    let btree_files: Vec<_> = btree_tree.files.keys().filter(|p| p.starts_with("/conv/")).collect();

    assert_eq!(flat_files.len(), below);
    assert_eq!(btree_files.len(), BTREE_CONVERSION_THRESHOLD + 50);
}
