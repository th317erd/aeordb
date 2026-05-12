//! Comprehensive corruption hardening tests for AeorDB.
//!
//! Tests the scanner recovery, KV rebuild, lost+found quarantine, and
//! directory listing resilience when faced with corrupt data.

use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;
use aeordb::engine::lost_found;

/// Create a fresh test database and return the engine + temp dir.
fn create_test_db() -> (StorageEngine, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    (engine, temp)
}

/// Store a few test files into the engine.
fn store_test_files(engine: &StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, "/docs/a.txt", b"file-a", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/docs/b.txt", b"file-b", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/docs/c.txt", b"file-c", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/images/photo.jpg", b"jpeg-data", Some("image/jpeg")).unwrap();
}

/// Inject garbage bytes at the given offset in the database file.
fn inject_corruption(db_path: &str, offset: u64, size: usize) {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new().write(true).open(db_path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let garbage: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(0x37)).collect();
    file.write_all(&garbage).unwrap();
    file.sync_all().unwrap();
}

/// Return the file size in bytes.
fn file_size(path: &str) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

// ============================================================================
// Test 1: Scanner recovers from corrupt header mid-file
// ============================================================================

#[test]
fn scanner_recovers_from_corrupt_header_mid_file() {
    let (engine, temp) = create_test_db();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Get file size and inject corruption at ~25%
    let size = file_size(db_str);
    let offset = size / 4;

    // Drop the engine so we can manipulate files
    drop(engine);

    inject_corruption(db_str, offset, 64);    // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

    // Reopen should succeed - scanner skips corrupt regions
    let engine = StorageEngine::open(db_str).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();

    // Root listing should work (may have fewer files due to corruption)
    let result = ops.list_directory("/");
    assert!(result.is_ok(), "Root listing should succeed after corruption recovery");
}

// ============================================================================
// Test 2: Scanner recovers from multiple corrupt regions
// ============================================================================

#[test]
fn scanner_recovers_from_multiple_corrupt_regions() {
    let (engine, temp) = create_test_db();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    let size = file_size(db_str);

    drop(engine);

    // Inject corruption at 25%, 50%, and 75%
    inject_corruption(db_str, size / 4, 32);
    inject_corruption(db_str, size / 2, 32);
    inject_corruption(db_str, 3 * size / 4, 32);    // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

    // Reopen should succeed
    let engine = StorageEngine::open(db_str).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();

    // Should not panic
    let _ = ops.list_directory("/");
}

// ============================================================================
// Test 3: Flush recovers from corrupt KV page
// ============================================================================

#[test]
fn flush_recovers_from_corrupt_kv_page() {
    // Single-file layout: the KV block lives inside the main file just past
    // the 256-byte FILE_HEADER. Corrupt some bytes there and verify that
    // a subsequent write still succeeds — the engine should detect the
    // corrupt page, reset it, and retry.
    let (engine, temp) = create_test_db();

    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx, "/alpha.txt", b"alpha-content", Some("text/plain")).unwrap();

    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Drop the engine so we have exclusive access to the file.
    drop(engine);

    // Corrupt 32 bytes well inside the KV block region. FILE_HEADER is 256
    // bytes; the KV block starts at offset 256.
    inject_corruption(db_str, 300, 32);

    // Reopen and write — engine should recover.
    let engine = StorageEngine::open(db_str).unwrap();
    let ops = DirectoryOps::new(&engine);
    let result = ops.store_file(&ctx, "/beta.txt", b"beta-content", Some("text/plain"));
    assert!(result.is_ok(), "Write after KV corruption should succeed: {:?}", result.err());
}

// ============================================================================
// Test 4: Lost+found quarantine writes to sibling directory
// ============================================================================

#[test]
fn lost_found_quarantine_writes_to_sibling_directory() {
    let (engine, _temp) = create_test_db();

    let data = b"corrupt-chunk-data";
    lost_found::quarantine_bytes(&engine, "/docs", "chunk_001.bin", "test corruption", data);

    // Verify the quarantined file exists and is readable
    let ops = DirectoryOps::new(&engine);
    let file = ops.read_file("/docs/lost+found/chunk_001.bin");
    assert!(file.is_ok(), "Quarantined file should be readable");
    let content = file.unwrap();
    assert_eq!(content, data, "Quarantined content should match original data");
}

// ============================================================================
// Test 5: Lost+found quarantine at root
// ============================================================================

#[test]
fn lost_found_quarantine_at_root() {
    let (engine, _temp) = create_test_db();

    let data = b"root-corrupt-data";
    lost_found::quarantine_bytes(&engine, "/", "root_chunk.bin", "root corruption", data);

    let ops = DirectoryOps::new(&engine);
    let file = ops.read_file("/lost+found/root_chunk.bin");
    assert!(file.is_ok(), "Quarantined file at root should be readable");
    let content = file.unwrap();
    assert_eq!(content, data, "Quarantined content at root should match");
}

// ============================================================================
// Test 6: Lost+found metadata is valid JSON
// ============================================================================

#[test]
fn lost_found_metadata_is_valid_json() {
    let (engine, _temp) = create_test_db();

    lost_found::quarantine_metadata(
        &engine,
        "/docs",
        "meta_001.json",
        "bad checksum",
        12345,
        None,
    );

    let ops = DirectoryOps::new(&engine);
    let file = ops.read_file("/docs/lost+found/meta_001.json");
    assert!(file.is_ok(), "Quarantine metadata file should be readable");

    let content = file.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&content)
        .expect("Quarantine metadata should be valid JSON");

    assert_eq!(parsed["reason"], "bad checksum");
    assert_eq!(parsed["offset"], 12345);
    assert!(parsed["timestamp"].is_string(), "timestamp should be a string");
}

// ============================================================================
// Test 7: List directory survives corrupt entry
// ============================================================================

#[test]
fn list_directory_survives_corrupt_entry() {
    let (engine, temp) = create_test_db();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    let size = file_size(db_str);

    drop(engine);

    // Inject corruption mid-file
    inject_corruption(db_str, size / 2, 48);    // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

    // Reopen
    let engine = StorageEngine::open(db_str).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();

    // List /docs should not panic (may have fewer entries)
    let result = ops.list_directory("/docs");
    // Either Ok with some entries, or NotFound if the directory was fully corrupted
    match result {
        Ok(entries) => {
            // Some entries may survive corruption
            assert!(entries.len() <= 3, "Should have at most 3 entries in /docs");
        }
        Err(_) => {
            // Directory may not exist if all its entries were corrupted - that's fine
        }
    }
}

// ============================================================================
// Test 8: rebuild_kv recovers index
// ============================================================================

#[test]
fn rebuild_kv_recovers_index() {
    // Single-file layout: KV pages live inside the main file at
    // [FILE_HEADER_SIZE, kv_block_offset+kv_block_length). Corrupt some
    // bytes there, then call rebuild_kv() which re-scans the WAL and
    // repopulates the KV index. All entries should be readable again.
    let (engine, temp) = create_test_db();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    // Verify files are readable before corruption.
    let ops = DirectoryOps::new(&engine);
    let before = ops.read_file("/docs/a.txt").unwrap();
    assert_eq!(before, b"file-a");

    // Drop engine to release exclusive lock on the file.
    drop(engine);

    // Corrupt bytes inside the KV block region (well past FILE_HEADER).
    inject_corruption(db_str, 300, 64);

    // Reopen and explicitly rebuild the KV index from the WAL.
    let engine = StorageEngine::open(db_str).unwrap();
    let result = engine.rebuild_kv();
    assert!(result.is_ok(), "rebuild_kv should succeed: {:?}", result.err());

    // Files should be readable again after rebuild
    let ops2 = DirectoryOps::new(&engine);
    let after = ops2.read_file("/docs/a.txt");
    assert!(after.is_ok(), "File /docs/a.txt should be readable after rebuild: {:?}", after.err());
    assert_eq!(after.unwrap(), b"file-a");

    let after_b = ops2.read_file("/docs/b.txt");
    assert!(after_b.is_ok(), "File /docs/b.txt should be readable after rebuild");
    assert_eq!(after_b.unwrap(), b"file-b");

    let after_img = ops2.read_file("/images/photo.jpg");
    assert!(after_img.is_ok(), "File /images/photo.jpg should be readable after rebuild");
    assert_eq!(after_img.unwrap(), b"jpeg-data");
}

// ============================================================================
// Test 9: Lost+found metadata with extra fields
// ============================================================================

#[test]
fn lost_found_metadata_includes_extra_fields() {
    let (engine, _temp) = create_test_db();

    let extra = serde_json::json!({
        "entry_type": "chunk",
        "original_hash": "abc123",
    });

    lost_found::quarantine_metadata(
        &engine,
        "/data",
        "meta_extra.json",
        "hash mismatch",
        99999,
        Some(&extra),
    );

    let ops = DirectoryOps::new(&engine);
    let content = ops.read_file("/data/lost+found/meta_extra.json").unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&content).unwrap();

    assert_eq!(parsed["reason"], "hash mismatch");
    assert_eq!(parsed["offset"], 99999);
    assert_eq!(parsed["entry_type"], "chunk");
    assert_eq!(parsed["original_hash"], "abc123");
}

// ============================================================================
// Test 10: Quarantine with empty parent path
// ============================================================================

#[test]
fn quarantine_with_empty_parent_writes_to_root_lost_found() {
    let (engine, _temp) = create_test_db();

    let data = b"orphan-data";
    lost_found::quarantine_bytes(&engine, "", "orphan.bin", "empty parent", data);

    let ops = DirectoryOps::new(&engine);
    let file = ops.read_file("/lost+found/orphan.bin");
    assert!(file.is_ok(), "Quarantine with empty parent should write to /lost+found/");
    assert_eq!(file.unwrap(), data);
}

// ============================================================================
// Test 11: Rebuild KV on clean database is idempotent
// ============================================================================

#[test]
fn rebuild_kv_on_clean_database_is_idempotent() {
    let (engine, _temp) = create_test_db();
    store_test_files(&engine);

    // Rebuild on a clean (non-corrupt) database
    let result = engine.rebuild_kv();
    assert!(result.is_ok(), "rebuild_kv on clean DB should succeed");

    // All files should still be readable
    let ops = DirectoryOps::new(&engine);
    assert_eq!(ops.read_file("/docs/a.txt").unwrap(), b"file-a");
    assert_eq!(ops.read_file("/docs/b.txt").unwrap(), b"file-b");
    assert_eq!(ops.read_file("/docs/c.txt").unwrap(), b"file-c");
    assert_eq!(ops.read_file("/images/photo.jpg").unwrap(), b"jpeg-data");
}

// ============================================================================
// Test 12: Scanner handles corruption at file header (very beginning)
// ============================================================================

#[test]
fn scanner_handles_corruption_at_start_of_data_region() {
    let (engine, temp) = create_test_db();
    store_test_files(&engine);

    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();

    drop(engine);

    // Corrupt near the beginning but after the file header. The header is
    // 256 bytes and now carries a CRC, so corrupting within it correctly
    // refuses to open. Use offset 300 to land in the KV region instead, where
    // dirty startup can recover.
    inject_corruption(db_str, 300, 64);    // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

    // Should still open (scanner skips corrupt entries)
    let result = StorageEngine::open(db_str);
    assert!(result.is_ok(), "Engine should open despite corruption near start: {:?}", result.err());
}
