//! Comprehensive corruption hardening tests for AeorDB.
//!
//! Tests the scanner recovery, KV rebuild, lost+found quarantine, and
//! directory listing resilience when faced with corrupt data.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::file_header::read_active_header;
use aeordb::engine::gc;
use aeordb::engine::hot_tail::{self, HotTailPayload, VoidRecord};
use aeordb::engine::lost_found;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::verify;
use aeordb::engine::{EntryType, RequestContext, ENTRY_MAGIC};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

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
  ops.store_file_buffered(&ctx, "/docs/a.txt", b"file-a", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/b.txt", b"file-b", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/c.txt", b"file-c", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/images/photo.jpg", b"jpeg-data", Some("image/jpeg")).unwrap();
}

/// Inject garbage bytes at the given offset in the database file.
fn inject_corruption(db_path: &str, offset: u64, size: usize) {
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

fn active_header(db_path: &str) -> aeordb::engine::file_header::FileHeader {
  let mut file = OpenOptions::new().read(true).open(db_path).unwrap();
  read_active_header(&mut file).unwrap().0
}

fn inject_hot_tail_voids(db_path: &str, voids: Vec<VoidRecord>) {
  let mut file = OpenOptions::new().read(true).write(true).open(db_path).unwrap();
  let (header, _) = read_active_header(&mut file).unwrap();
  let payload = HotTailPayload { writes: Vec::new(), voids };
  hot_tail::write_hot_tail(&mut file, header.hot_tail_offset, &payload, header.hash_algo.hash_length()).unwrap();
  file.sync_all().unwrap();
}

fn read_u32_at(db_path: &str, offset: u64) -> u32 {
  let mut file = OpenOptions::new().read(true).open(db_path).unwrap();
  let mut bytes = [0u8; 4];
  file.seek(SeekFrom::Start(offset)).unwrap();
  file.read_exact(&mut bytes).unwrap();
  u32::from_le_bytes(bytes)
}

fn raw_test_db() -> (String, tempfile::TempDir) {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap().to_string();
  let engine = StorageEngine::create(&db_str).unwrap();
  drop(engine);
  (db_str, temp)
}

fn store_raw_directory_entry(engine: &StorageEngine, key_byte: u8, value_len: usize) -> (u64, u32) {
  let key = vec![key_byte; engine.hash_algo().hash_length()];
  let value = vec![key_byte.wrapping_mul(3); value_len];
  let offset = engine.store_entry(EntryType::DirectoryIndex, &key, &value).unwrap();
  let total_length = engine.read_entry_header_at(offset).unwrap().total_length;
  (offset, total_length)
}

fn store_raw_chunk_entry(engine: &StorageEngine, key_byte: u8, value_len: usize) -> (u64, u32) {
  let key = vec![key_byte; engine.hash_algo().hash_length()];
  let value = vec![key_byte.wrapping_mul(5); value_len];
  let offset = engine.store_entry(EntryType::Chunk, &key, &value).unwrap();
  let total_length = engine.read_entry_header_at(offset).unwrap().total_length;
  (offset, total_length)
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

  inject_corruption(db_str, offset, 64); // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

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
  inject_corruption(db_str, 3 * size / 4, 32); // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

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
  ops.store_file_buffered(&ctx, "/alpha.txt", b"alpha-content", Some("text/plain")).unwrap();

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
  let result = ops.store_file_buffered(&ctx, "/beta.txt", b"beta-content", Some("text/plain"));
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
  let file = ops.read_file_buffered("/docs/lost+found/chunk_001.bin");
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
  let file = ops.read_file_buffered("/lost+found/root_chunk.bin");
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

  lost_found::quarantine_metadata(&engine, "/docs", "meta_001.json", "bad checksum", 12345, None);

  let ops = DirectoryOps::new(&engine);
  let file = ops.read_file_buffered("/docs/lost+found/meta_001.json");
  assert!(file.is_ok(), "Quarantine metadata file should be readable");

  let content = file.unwrap();
  let parsed: serde_json::Value = serde_json::from_slice(&content).expect("Quarantine metadata should be valid JSON");

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
  inject_corruption(db_str, size / 2, 48); // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

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
  let before = ops.read_file_buffered("/docs/a.txt").unwrap();
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
  let after = ops2.read_file_buffered("/docs/a.txt");
  assert!(after.is_ok(), "File /docs/a.txt should be readable after rebuild: {:?}", after.err());
  assert_eq!(after.unwrap(), b"file-a");

  let after_b = ops2.read_file_buffered("/docs/b.txt");
  assert!(after_b.is_ok(), "File /docs/b.txt should be readable after rebuild");
  assert_eq!(after_b.unwrap(), b"file-b");

  let after_img = ops2.read_file_buffered("/images/photo.jpg");
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

  lost_found::quarantine_metadata(&engine, "/data", "meta_extra.json", "hash mismatch", 99999, Some(&extra));

  let ops = DirectoryOps::new(&engine);
  let content = ops.read_file_buffered("/data/lost+found/meta_extra.json").unwrap();
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
  let file = ops.read_file_buffered("/lost+found/orphan.bin");
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
  assert_eq!(ops.read_file_buffered("/docs/a.txt").unwrap(), b"file-a");
  assert_eq!(ops.read_file_buffered("/docs/b.txt").unwrap(), b"file-b");
  assert_eq!(ops.read_file_buffered("/docs/c.txt").unwrap(), b"file-c");
  assert_eq!(ops.read_file_buffered("/images/photo.jpg").unwrap(), b"jpeg-data");
}

#[test]
fn rebuild_kv_preserves_newer_entry_written_into_reused_lower_offset() {
  let (engine, _temp) = create_test_db();
  let hash_length = engine.hash_algo().hash_length();

  let filler_key = vec![0xE1; hash_length];
  let target_key = vec![0xA7; hash_length];

  let low_offset = engine.store_entry(EntryType::Chunk, &filler_key, &[0x11; 96]).unwrap();
  let low_length = engine.read_entry_header_at(low_offset).unwrap().total_length;
  let old_offset = engine.store_entry(EntryType::Chunk, &target_key, b"old-visible-value").unwrap();
  assert!(old_offset > low_offset, "setup should place the old target value after the reusable low slot");

  engine.write_void_at(low_offset, low_length).unwrap();
  let reused_offset = engine.store_entry(EntryType::Chunk, &target_key, b"new-visible-value").unwrap();
  assert_eq!(reused_offset, low_offset, "setup should write the newer target value into the lower reused void");

  let before_rebuild = engine.get_entry(&target_key).unwrap().unwrap();
  assert_eq!(before_rebuild.2, b"new-visible-value");

  engine.rebuild_kv().unwrap();

  let after_rebuild = engine.get_entry(&target_key).unwrap().unwrap();
  assert_eq!(
    after_rebuild.2, b"new-visible-value",
    "dirty KV rebuild must use entry chronology, not WAL offset order, because GC can reuse lower offsets for newer entries"
  );
}

#[test]
fn rebuild_kv_preserves_file_deletions() {
  let (engine, _temp) = create_test_db();
  store_test_files(&engine);

  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&ctx, "/docs/b.txt").unwrap();
  assert!(ops.read_file_buffered("/docs/b.txt").is_err());

  engine.rebuild_kv().unwrap();

  let ops = DirectoryOps::new(&engine);
  assert_eq!(ops.read_file_buffered("/docs/a.txt").unwrap(), b"file-a");
  assert!(ops.read_file_buffered("/docs/b.txt").is_err(), "manual/dirty KV rebuild must not resurrect a deleted file path");
}

#[test]
fn rebuild_kv_keeps_recreated_file_live_after_prior_delete() {
  let (engine, _temp) = create_test_db();
  store_test_files(&engine);

  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&ctx, "/docs/b.txt").unwrap();
  ops.store_file_buffered(&ctx, "/docs/b.txt", b"file-b-v2", Some("text/plain")).unwrap();

  engine.rebuild_kv().unwrap();

  let ops = DirectoryOps::new(&engine);
  assert_eq!(ops.read_file_buffered("/docs/b.txt").unwrap(), b"file-b-v2");
}

#[test]
fn rebuild_kv_keeps_snapshot_handle_live_for_post_rebuild_deletes() {
  let (engine, _temp) = create_test_db();
  store_test_files(&engine);

  engine.rebuild_kv().unwrap();

  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.delete_file(&ctx, "/docs/b.txt").unwrap();

  assert!(ops.read_file_buffered("/docs/b.txt").is_err(), "reads after a post-rebuild delete must see the updated KV snapshot");

  let children = ops.list_directory("/docs").unwrap();
  let names: Vec<&str> = children.iter().map(|child| child.name.as_str()).collect();
  assert!(!names.contains(&"b.txt"), "listings after a post-rebuild delete must see the updated KV snapshot: {:?}", names);
}

#[test]
fn rebuild_kv_preserves_symlink_deletions() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_symlink(&ctx, "/docs/link", "/docs/target.txt").unwrap();
  ops.delete_symlink(&ctx, "/docs/link").unwrap();
  assert!(ops.get_symlink("/docs/link").unwrap().is_none());

  engine.rebuild_kv().unwrap();

  let ops = DirectoryOps::new(&engine);
  assert!(ops.get_symlink("/docs/link").unwrap().is_none(), "manual/dirty KV rebuild must not resurrect a deleted symlink path");
}

#[test]
fn rebuild_kv_preserves_directory_deletions() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.create_directory(&ctx, "/docs/empty").unwrap();
  ops.delete_directory(&ctx, "/docs/empty").unwrap();
  assert!(!ops.exists("/docs/empty").unwrap());

  engine.rebuild_kv().unwrap();

  let ops = DirectoryOps::new(&engine);
  assert!(!ops.exists("/docs/empty").unwrap(), "manual/dirty KV rebuild must not resurrect a deleted directory path");
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
  inject_corruption(db_str, 300, 64); // Single-file layout: no separate .kv file. Reopen + rebuild_kv() instead.

  // Should still open (scanner skips corrupt entries)
  let result = StorageEngine::open(db_str);
  assert!(result.is_ok(), "Engine should open despite corruption near start: {:?}", result.err());
}

#[test]
fn verify_reports_raw_hot_tail_void_inside_kv_block() {
  let (db_str, _temp) = raw_test_db();
  let header = active_header(&db_str);
  let invalid_offset = header.kv_block_offset + 4096;

  inject_hot_tail_voids(&db_str, vec![VoidRecord { offset: invalid_offset, size: 512 }]);

  let engine = StorageEngine::open(&db_str).unwrap();
  let report = verify::verify(&engine, &db_str);

  assert!(
    report.invalid_hot_tail_voids.iter().any(|issue| issue.contains(&invalid_offset.to_string())),
    "verify should report raw hot-tail voids inside the KV block: {:?}",
    report.invalid_hot_tail_voids
  );
}

#[test]
fn startup_drops_hot_tail_void_inside_kv_block_before_reuse() {
  let (db_str, _temp) = raw_test_db();
  let header = active_header(&db_str);
  let wal_start = header.kv_block_offset + header.kv_block_length;
  let invalid_offset = header.kv_block_offset + 4096;
  let before = read_u32_at(&db_str, invalid_offset);
  assert_ne!(before, ENTRY_MAGIC, "test setup needs a non-entry byte range inside the KV block");

  inject_hot_tail_voids(&db_str, vec![VoidRecord { offset: invalid_offset, size: 512 }]);

  let engine = StorageEngine::open(&db_str).unwrap();
  let (written_offset, _written_length) = store_raw_directory_entry(&engine, 0xA7, 64);
  let after = read_u32_at(&db_str, invalid_offset);

  assert_ne!(written_offset, invalid_offset, "store_entry must not reuse a void inside the KV block");
  assert!(written_offset >= wal_start, "store_entry should append in the WAL, not in reserved metadata: {}", written_offset);
  assert_eq!(after, before, "invalid KV-block void bytes should not be overwritten by the new entry");
}

#[test]
fn write_void_at_rejects_reserved_kv_block_range() {
  let (db_str, _temp) = raw_test_db();
  let header = active_header(&db_str);
  let invalid_offset = header.kv_block_offset + 4096;

  let engine = StorageEngine::open(&db_str).unwrap();
  let result = engine.write_void_at(invalid_offset, 512);

  assert!(result.is_err(), "write_void_at should reject ranges inside the KV block");
}

#[test]
fn mutable_index_entries_do_not_consume_reusable_voids() {
  let (engine, _temp) = create_test_db();

  let (void_offset, void_size) = store_raw_directory_entry(&engine, 0x22, 256);
  engine.write_void_at(void_offset, void_size).unwrap();

  let (new_offset, _new_size) = store_raw_directory_entry(&engine, 0x33, 64);

  assert_ne!(new_offset, void_offset, "DirectoryIndex entries are mutable/index records and should append instead of reusing voids");
  assert!(new_offset > void_offset, "DirectoryIndex replacement should land at the WAL frontier");
}

#[test]
fn chunk_entries_can_consume_reusable_voids() {
  let (engine, _temp) = create_test_db();

  let (void_offset, void_size) = store_raw_chunk_entry(&engine, 0x44, 256);
  engine.write_void_at(void_offset, void_size).unwrap();

  let (new_offset, _new_size) = store_raw_chunk_entry(&engine, 0x55, 64);

  assert_eq!(new_offset, void_offset, "Chunk entries are content-addressed payloads and may reuse reclaimed void space");
}

#[test]
fn verify_does_not_require_void_entries_in_live_kv() {
  let (engine, temp) = create_test_db();
  store_test_files(&engine);
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap().to_string();

  let key = engine.hash_algo().compute_hash(b"void-bookkeeping").unwrap();
  engine.store_entry(EntryType::Void, &key, b"void").unwrap();
  engine.remove_kv_entry(&key).unwrap();
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(&db_str).unwrap();
  let report = verify::verify(&reopened, &db_str);

  assert_eq!(report.missing_kv_entries, 0, "void records are storage bookkeeping, not required live KV entries");
  assert_eq!(report.stale_kv_entries, 0, "void records should not make live KV appear stale");
}

#[test]
fn verify_does_not_count_deleted_path_entries_as_missing_kv() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/deleted/path.txt", b"gone", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/deleted/path.txt").unwrap();

  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  let report = verify::verify(&engine, db_str);

  assert_eq!(report.missing_kv_entries, 0, "deleted path entries should not be expected in the live KV set");
  assert_eq!(report.stale_kv_entries, 0, "deletion replay should not make live KV appear stale");
}

#[test]
fn verify_does_not_count_gc_voided_entries_as_missing_kv() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  for i in 0..12 {
    let body = format!("version-{i:02}");
    ops.store_file_buffered(&ctx, "/gc/doc.txt", body.as_bytes(), Some("text/plain")).unwrap();
  }

  let result = gc::run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0, "test setup should create garbage entries");

  let db_path = temp.path().join("test.aeordb");
  let report = verify::verify(&engine, db_path.to_str().unwrap());

  assert!(report.void_bytes > 0, "GC should have published reusable void ranges");
  assert_eq!(report.missing_kv_entries, 0, "GC-voided WAL entries are not expected live KV entries");
  assert_eq!(report.stale_kv_entries, 0, "GC-voided WAL entries should not remain live in KV");
  assert_eq!(ops.read_file_buffered("/gc/doc.txt").unwrap(), b"version-11");
}

#[test]
fn clean_startup_masks_page_kv_entries_covered_by_hot_tail_voids() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let db_path = temp.path().join("test.aeordb");
  let copy_path = temp.path().join("gc-copy.aeordb");

  for version in 0..14 {
    let body = format!("version-{version}");
    ops.store_file_buffered(&ctx, "/gc/doc.txt", body.as_bytes(), Some("text/plain")).unwrap();
  }

  let result = gc::run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0, "test needs GC to reclaim old versions");

  // Copy before clean shutdown. The hot-tail void snapshot is durable, but
  // old KV bucket pages can still contain live-looking entries for reclaimed
  // ranges. Clean startup must mask those page entries.
  std::fs::copy(&db_path, &copy_path).unwrap();

  let reopened = StorageEngine::open(copy_path.to_str().unwrap()).unwrap();
  let report = verify::verify(&reopened, copy_path.to_str().unwrap());

  assert_eq!(report.missing_kv_entries, 0);
  assert_eq!(report.stale_kv_entries, 0, "voided page entries should be masked on clean startup: {:?}", report.stale_kv_details);
  let reopened_ops = DirectoryOps::new(&reopened);
  assert_eq!(reopened_ops.read_file_buffered("/gc/doc.txt").unwrap(), b"version-13");
}

#[test]
fn kv_expansion_relocates_reusable_voids_from_growth_zone() {
  let (db_str, _temp) = raw_test_db();
  let header = active_header(&db_str);
  let old_wal_start = header.kv_block_offset + header.kv_block_length;

  let engine = StorageEngine::open(&db_str).unwrap();
  let (void_offset, void_size) = store_raw_chunk_entry(&engine, 0x11, 512);
  assert_eq!(void_offset, old_wal_start, "fresh DB should place the first WAL entry at the old WAL start");
  engine.write_void_at(void_offset, void_size).unwrap();

  let (_filler_offset, _filler_size) = store_raw_directory_entry(&engine, 0x22, 600 * 1024);
  let (sentinel_offset, sentinel_size) = store_raw_directory_entry(&engine, 0x33, 2048);
  let expected_relocated_void_offset = sentinel_offset + sentinel_size as u64;

  engine.expand_kv_block_online(1).unwrap();

  let expanded_header = active_header(&db_str);
  let expected_stage_size =
    aeordb::engine::kv_stages::stage_params(1, aeordb::engine::kv_pages::page_size(expanded_header.hash_algo.hash_length())).0;
  assert_eq!(
    expanded_header.kv_block_length, expected_stage_size,
    "online expansion must reserve the full KV stage size, including page-layout slack"
  );
  let new_wal_start = expanded_header.kv_block_offset + expanded_header.kv_block_length;
  assert!(void_offset < new_wal_start, "the original void offset should now be inside the expanded KV block");
  let mut db_file = OpenOptions::new().read(true).open(&db_str).unwrap();
  let expanded_hot_tail = hot_tail::read_hot_tail(&mut db_file, expanded_header.hot_tail_offset, expanded_header.hash_algo.hash_length())
    .expect("expanded DB should advertise a valid hot tail");
  assert!(expanded_hot_tail.writes.is_empty(), "expanded KV pages should not leave stale pre-expansion writes in the hot tail");

  let (replacement_offset, _replacement_size) = store_raw_chunk_entry(&engine, 0x44, 64);

  assert_ne!(replacement_offset, void_offset, "post-expansion writes must not reuse the old reserved void offset");
  assert_eq!(
    replacement_offset, expected_relocated_void_offset,
    "post-expansion writes should reuse the relocated copy of the growth-zone void"
  );
  assert!(replacement_offset >= new_wal_start, "relocated void should be in the post-expansion WAL region");

  let report = verify::verify(&engine, &db_str);
  assert!(
    report.invalid_kv_offsets.is_empty(),
    "KV expansion should not leave live KV pointers inside the reserved block: {:?}",
    report.invalid_kv_offsets
  );
}

#[test]
fn kv_expansion_dirty_rebuild_keeps_offsets_out_of_reserved_block() {
  let (db_str, _temp) = raw_test_db();
  let engine = StorageEngine::open(&db_str).unwrap();

  for index in 0..1800u64 {
    let key = blake3::hash(&index.to_le_bytes()).as_bytes().to_vec();
    let value = vec![(index % 251) as u8; 256];
    engine.store_entry(EntryType::Chunk, &key, &value).unwrap();
  }

  let expanded_header = active_header(&db_str);
  assert!(expanded_header.kv_block_stage > 0, "test setup should force at least one KV expansion");
  engine.shutdown().unwrap();
  drop(engine);

  {
    let mut file = OpenOptions::new().read(true).write(true).open(&db_str).unwrap();
    file.seek(SeekFrom::Start(expanded_header.hot_tail_offset)).unwrap();
    file.write_all(&[0u8; 5]).unwrap();
    file.sync_all().unwrap();
  }

  let reopened = StorageEngine::open(&db_str).unwrap();
  let report = verify::verify(&reopened, &db_str);

  assert!(
    report.invalid_kv_offsets.is_empty(),
    "dirty rebuild after expansion must not preserve KV offsets inside the reserved block: {:?}",
    report.invalid_kv_offsets
  );
}
