//! Comprehensive corruption hardening tests for AeorDB.
//!
//! Tests the scanner recovery, KV rebuild, lost+found quarantine, and
//! directory listing resilience when faced with corrupt data.

use aeordb::engine::btree::{BTreeNode, BTREE_CONVERSION_THRESHOLD};
use aeordb::engine::append_writer::AppendWriter;
use aeordb::engine::directory_ops::{DirectoryOps, directory_path_hash};
use aeordb::engine::file_header::{HEADER_REGION_SIZE, read_active_header};
use aeordb::engine::gc;
use aeordb::engine::hot_tail::{self, HotTailPayload, VoidRecord};
use aeordb::engine::kv_pages::{bucket_page_offset, page_size, PAGE_HEADER_SIZE, PAGE_MAGIC};
use aeordb::engine::kv_stages::stage_params;
use aeordb::engine::kv_store::{KVEntry, KV_TYPE_CHUNK};
use aeordb::engine::lost_found;
use aeordb::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::verify;
use aeordb::engine::{EntryType, RequestContext, ENTRY_MAGIC, file_path_hash};
use aeordb::engine::file_record::FileRecord;
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

fn first_non_empty_kv_page_offset(db_path: &str) -> u64 {
  let header = active_header(db_path);
  let hash_length = header.hash_algo.hash_length();
  let (_, bucket_count) = stage_params(header.kv_block_stage as usize, page_size(hash_length));
  let mut file = OpenOptions::new().read(true).open(db_path).unwrap();
  let mut magic = [0u8; 4];

  for bucket in 0..bucket_count {
    let offset = header.kv_block_offset + bucket_page_offset(bucket, hash_length);
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.read_exact(&mut magic).unwrap();
    if u32::from_le_bytes(magic) == PAGE_MAGIC {
      return offset;
    }
  }

  panic!("test database did not contain a non-empty KV page");
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

fn create_no_kv_wal(db_path: &std::path::Path) -> Vec<(Vec<u8>, Vec<u8>)> {
  let records = vec![(vec![0x31; 32], vec![0xA1; 48 * 1024]), (vec![0x42; 32], b"legacy-tail-record".to_vec())];
  let mut writer = AppendWriter::create(db_path).unwrap();
  for (key, value) in &records {
    writer.append_entry(EntryType::DirectoryIndex, key, value, 0).unwrap();
  }
  writer.sync().unwrap();
  assert_eq!(writer.file_header().kv_block_offset, 0);
  assert_eq!(writer.file_header().kv_block_length, 0);
  assert_eq!(writer.file_header().hot_tail_offset, 0);
  records
}

// ============================================================================
// Test 1: Scanner recovers from corrupt header mid-file
// ============================================================================

#[test]
fn opening_no_kv_database_migrates_wal_without_clobbering_and_remains_appendable() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("no-kv.aeordb");
  let records = create_no_kv_wal(&db_path);
  let db_str = db_path.to_str().unwrap();

  let engine = StorageEngine::open(db_str).unwrap();
  for (key, expected) in &records {
    assert_eq!(engine.get_entry(key).unwrap().unwrap().2, *expected);
  }

  let migrated = active_header(db_str);
  assert_eq!(migrated.kv_block_offset, HEADER_REGION_SIZE as u64);
  assert!(migrated.kv_block_length >= aeordb::engine::kv_stages::initial_block_size());
  assert!(
    migrated.hot_tail_offset >= migrated.kv_block_offset + migrated.kv_block_length,
    "standard layout must place the WAL frontier after the complete KV block"
  );
  assert!(!migrated.resize_in_progress);
  assert_eq!(migrated.resize_target_stage, 0);

  let appended_key = vec![0x53; 32];
  let appended_value = b"written-after-migration".to_vec();
  let appended_offset = engine.store_entry(EntryType::DirectoryIndex, &appended_key, &appended_value).unwrap();
  assert!(appended_offset >= migrated.kv_block_offset + migrated.kv_block_length);
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(db_str).unwrap();
  for (key, expected) in &records {
    assert_eq!(reopened.get_entry(key).unwrap().unwrap().2, *expected);
  }
  assert_eq!(reopened.get_entry(&appended_key).unwrap().unwrap().2, appended_value);
  assert!(!verify::verify_checked(&reopened, db_str).unwrap().has_issues());
}

#[test]
fn opening_empty_no_kv_database_creates_a_standard_verified_layout() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("empty-no-kv.aeordb");
  let mut writer = AppendWriter::create(&db_path).unwrap();
  writer.sync().unwrap();
  drop(writer);

  let db_str = db_path.to_str().unwrap();
  let engine = StorageEngine::open(db_str).unwrap();
  let migrated = active_header(db_str);
  assert_eq!(migrated.kv_block_offset, HEADER_REGION_SIZE as u64);
  assert!(migrated.kv_block_length >= aeordb::engine::kv_stages::initial_block_size());
  assert!(!migrated.resize_in_progress);
  assert!(!verify::verify_checked(&engine, db_str).unwrap().has_issues());
}

#[test]
fn opening_post_wal_kv_layout_migrates_only_the_authoritative_wal() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("post-wal-kv.aeordb");
  let records = create_no_kv_wal(&db_path);
  let db_str = db_path.to_str().unwrap();

  let mut writer = AppendWriter::open(&db_path).unwrap();
  let wal_end = writer.current_offset();
  let old_kv_length = aeordb::engine::kv_stages::initial_block_size();
  let mut transitional = writer.file_header().clone();
  transitional.kv_block_offset = wal_end;
  transitional.kv_block_length = old_kv_length;
  transitional.kv_block_stage = 0;
  transitional.hot_tail_offset = wal_end;
  writer.update_header(&transitional).unwrap();
  drop(writer);

  let mut file = OpenOptions::new().read(true).write(true).open(&db_path).unwrap();
  file.seek(SeekFrom::Start(wal_end)).unwrap();
  file.write_all(&vec![0xA5; old_kv_length as usize]).unwrap();
  file.sync_all().unwrap();
  drop(file);

  let engine = StorageEngine::open(db_str).unwrap();
  for (key, expected) in &records {
    assert_eq!(engine.get_entry(key).unwrap().unwrap().2, *expected);
  }
  let migrated = active_header(db_str);
  assert_eq!(migrated.kv_block_offset, HEADER_REGION_SIZE as u64);
  assert!(migrated.hot_tail_offset >= migrated.kv_block_offset + migrated.kv_block_length);
  assert!(!verify::verify_checked(&engine, db_str).unwrap().has_issues());
}

#[test]
fn truncated_post_wal_kv_layout_is_refused_without_mutation() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("truncated-post-wal-kv.aeordb");
  create_no_kv_wal(&db_path);

  let mut writer = AppendWriter::open(&db_path).unwrap();
  let wal_end = writer.current_offset();
  let old_kv_length = aeordb::engine::kv_stages::initial_block_size();
  let mut transitional = writer.file_header().clone();
  transitional.kv_block_offset = wal_end;
  transitional.kv_block_length = old_kv_length;
  transitional.kv_block_stage = 0;
  transitional.hot_tail_offset = wal_end;
  writer.update_header(&transitional).unwrap();
  drop(writer);

  let file = OpenOptions::new().read(true).write(true).open(&db_path).unwrap();
  file.set_len(wal_end + old_kv_length / 2).unwrap();
  file.sync_all().unwrap();
  drop(file);
  let before = std::fs::read(&db_path).unwrap();

  let error = match StorageEngine::open(db_path.to_str().unwrap()) {
    Ok(_) => panic!("a truncated post-WAL KV extent must require explicit repair"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("KV block"), "unexpected migration error: {error}");
  assert_eq!(std::fs::read(&db_path).unwrap(), before);
}

#[test]
fn startup_resumes_selected_pre_relocation_initial_kv_marker() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("initial-kv-pre-relocation.aeordb");
  let records = create_no_kv_wal(&db_path);

  let mut writer = AppendWriter::open(&db_path).unwrap();
  let mut marker = writer.file_header().clone();
  marker.kv_block_offset = HEADER_REGION_SIZE as u64;
  marker.kv_block_length = 0;
  marker.kv_block_stage = 0;
  marker.hot_tail_offset = writer.current_offset();
  marker.resize_in_progress = true;
  marker.resize_target_stage = 0;
  writer.update_header(&marker).unwrap();
  drop(writer);

  let engine = StorageEngine::open(db_path.to_str().unwrap()).unwrap();
  for (key, expected) in &records {
    assert_eq!(engine.get_entry(key).unwrap().unwrap().2, *expected);
  }
  let recovered = active_header(db_path.to_str().unwrap());
  assert_eq!(recovered.kv_block_offset, HEADER_REGION_SIZE as u64);
  assert!(recovered.kv_block_length >= aeordb::engine::kv_stages::initial_block_size());
  assert!(!recovered.resize_in_progress);
}

#[test]
fn startup_resumes_selected_relocation_durable_initial_kv_marker_without_copying_twice() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("initial-kv-relocation-durable.aeordb");
  let records = create_no_kv_wal(&db_path);
  let header_end = HEADER_REGION_SIZE as u64;
  let block_length = aeordb::engine::kv_stages::initial_block_size();

  let writer = AppendWriter::open(&db_path).unwrap();
  let old_wal_end = writer.current_offset();
  drop(writer);
  let old_wal = std::fs::read(&db_path).unwrap()[header_end as usize..old_wal_end as usize].to_vec();
  let new_wal_start = header_end + block_length;
  let new_hot_tail = new_wal_start + old_wal.len() as u64;
  let uncommitted_key = vec![0x77; 32];
  let uncommitted_path = temp.path().join("uncommitted-entry.aeordb");
  let mut uncommitted_writer = AppendWriter::create(&uncommitted_path).unwrap();
  uncommitted_writer.append_entry(EntryType::DirectoryIndex, &uncommitted_key, b"past-selected-frontier", 0).unwrap();
  uncommitted_writer.sync().unwrap();
  drop(uncommitted_writer);
  let uncommitted_bytes = std::fs::read(&uncommitted_path).unwrap()[header_end as usize..].to_vec();

  let mut file = OpenOptions::new().read(true).write(true).open(&db_path).unwrap();
  file.seek(SeekFrom::Start(new_wal_start)).unwrap();
  file.write_all(&old_wal).unwrap();
  let physical_end = hot_tail::write_hot_tail(&mut file, new_hot_tail, &HotTailPayload::default(), 32).unwrap();
  file.seek(SeekFrom::Start(physical_end)).unwrap();
  file.write_all(&uncommitted_bytes).unwrap();
  file.sync_all().unwrap();
  drop(file);

  let mut writer = AppendWriter::open(&db_path).unwrap();
  let mut marker = writer.file_header().clone();
  marker.kv_block_offset = header_end;
  marker.kv_block_length = block_length;
  marker.kv_block_stage = 0;
  marker.hot_tail_offset = new_hot_tail;
  marker.resize_in_progress = true;
  marker.resize_target_stage = 0;
  writer.update_header(&marker).unwrap();
  drop(writer);

  let engine = StorageEngine::open(db_path.to_str().unwrap()).unwrap();
  for (key, expected) in &records {
    assert_eq!(engine.get_entry(key).unwrap().unwrap().2, *expected);
  }
  assert!(!engine.has_entry(&uncommitted_key).unwrap(), "expansion rebuild must not index bytes past its selected WAL frontier");
  let recovered = active_header(db_path.to_str().unwrap());
  assert_eq!(recovered.hot_tail_offset, new_hot_tail, "relocation-durable recovery must not move the WAL a second time");
  assert!(!recovered.resize_in_progress);
}

#[test]
fn corrupt_no_kv_wal_is_refused_before_migration_mutates_database_bytes() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("corrupt-no-kv.aeordb");
  create_no_kv_wal(&db_path);

  let mut file = OpenOptions::new().read(true).write(true).open(&db_path).unwrap();
  file.seek(SeekFrom::Start(HEADER_REGION_SIZE as u64)).unwrap();
  file.write_all(&0xDEADBEEFu32.to_le_bytes()).unwrap();
  file.sync_all().unwrap();
  drop(file);
  let before = std::fs::read(&db_path).unwrap();

  let error = match StorageEngine::open(db_path.to_str().unwrap()) {
    Ok(_) => panic!("corrupt no-KV WAL must not be migrated"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("WAL") || error.to_string().contains("entry"), "unexpected migration error: {error}");
  assert_eq!(std::fs::read(&db_path).unwrap(), before, "read-only migration preflight failure must preserve every database byte");
}

#[test]
fn ambiguous_no_kv_hot_tail_layout_is_refused_without_mutation() {
  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("ambiguous-no-kv-hot-tail.aeordb");
  create_no_kv_wal(&db_path);

  let mut writer = AppendWriter::open(&db_path).unwrap();
  let hot_tail_offset = writer.current_offset();
  let mut file = OpenOptions::new().read(true).write(true).open(&db_path).unwrap();
  let physical_end = hot_tail::write_hot_tail(&mut file, hot_tail_offset, &HotTailPayload::default(), 32).unwrap();
  file.set_len(physical_end).unwrap();
  file.sync_all().unwrap();
  drop(file);
  let mut header = writer.file_header().clone();
  header.hot_tail_offset = hot_tail_offset;
  writer.update_header(&header).unwrap();
  drop(writer);
  let before = std::fs::read(&db_path).unwrap();

  let error = match StorageEngine::open(db_path.to_str().unwrap()) {
    Ok(_) => panic!("ambiguous no-KV hot-tail layout must require explicit repair"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("ambiguous"), "unexpected migration error: {error}");
  assert_eq!(std::fs::read(&db_path).unwrap(), before);
}

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

#[test]
fn open_rebuilds_corrupt_kv_before_reading_initial_counters() {
  let (engine, temp) = create_test_db();
  store_test_files(&engine);
  engine.shutdown().unwrap();

  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  let page_offset = first_non_empty_kv_page_offset(db_str);
  drop(engine);

  // Damage entry bytes while preserving page magic. Open must detect the CRC
  // failure and rebuild from the authoritative WAL before any snapshot reader,
  // including startup counter initialization, observes the damaged page.
  inject_corruption(db_str, page_offset + PAGE_HEADER_SIZE as u64, 1);

  let reopened = StorageEngine::open(db_str).expect("known-corrupt KV must rebuild before startup readers run");
  let ops = DirectoryOps::new(&reopened);
  for path in ["/docs/a.txt", "/docs/b.txt", "/docs/c.txt", "/images/photo.jpg"] {
    assert!(ops.exists(path).unwrap(), "WAL rebuild lost {path}");
  }
  let stats = reopened.stats().unwrap();
  assert!(stats.file_count >= 4, "startup counters were not initialized from the rebuilt KV: {stats:?}");
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
fn rebuild_kv_memory_refusal_precedes_database_mutation_and_releases_cleanly() {
  let (engine, temp) = create_test_db();
  store_test_files(&engine);
  let db_path = temp.path().join("test.aeordb");
  let before = std::fs::read(&db_path).unwrap();

  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining_critical = policy.emergency_reserve_bytes.checked_sub(snapshot.critical_reserved_bytes).unwrap();
  let pressure =
    coordinator.reserve(MemoryOwner::Repair, remaining_critical, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery)).unwrap();

  let error = engine.rebuild_kv().unwrap_err();
  assert!(matches!(error, aeordb::engine::errors::EngineError::ResourceExhausted(_)));
  assert_eq!(std::fs::read(&db_path).unwrap(), before, "memory refusal must occur before the live KV block is touched");

  drop(pressure);
  engine.rebuild_kv().unwrap();
  let ops = DirectoryOps::new(&engine);
  assert_eq!(ops.read_file_buffered("/docs/a.txt").unwrap(), b"file-a");
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert_eq!(owner.active_reservations, 0);
  assert_eq!(owner.reserved_bytes, 0);
}

#[test]
fn rebuild_kv_completion_restores_a_valid_hot_tail_checkpoint() {
  let (engine, temp) = create_test_db();
  store_test_files(&engine);
  let db_path = temp.path().join("test.aeordb");

  engine.rebuild_kv().unwrap();

  let header = active_header(db_path.to_str().unwrap());
  let mut file = OpenOptions::new().read(true).open(&db_path).unwrap();
  assert!(
    hot_tail::read_hot_tail(&mut file, header.hot_tail_offset, header.hash_algo.hash_length()).is_some(),
    "a completed rebuild must not leave the durable dirty-startup marker active"
  );
}

#[test]
fn durable_missing_hot_tail_marker_recovers_partially_rewritten_kv_on_reopen() {
  let (engine, temp) = create_test_db();
  store_test_files(&engine);
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  engine.shutdown().unwrap();
  drop(engine);

  let header = active_header(db_str);
  let mut file = OpenOptions::new().read(true).write(true).open(&db_path).unwrap();
  file.set_len(header.hot_tail_offset).unwrap();
  file.seek(SeekFrom::Start(header.kv_block_offset)).unwrap();
  file.write_all(&vec![0u8; page_size(header.hash_algo.hash_length())]).unwrap();
  file.sync_all().unwrap();
  drop(file);

  let reopened = StorageEngine::open(db_str).unwrap();
  let ops = DirectoryOps::new(&reopened);
  assert_eq!(ops.read_file_buffered("/docs/a.txt").unwrap(), b"file-a");
  assert_eq!(ops.read_file_buffered("/images/photo.jpg").unwrap(), b"jpeg-data");
  let recovered = active_header(db_str);
  let mut file = OpenOptions::new().read(true).open(&db_path).unwrap();
  assert!(hot_tail::read_hot_tail(&mut file, recovered.hot_tail_offset, recovered.hash_algo.hash_length()).is_some());
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
fn reusable_void_split_materializes_a_parseable_remainder() {
  let (engine, temp) = create_test_db();
  let old_key = vec![0x61; engine.hash_algo().hash_length()];
  let (void_offset, void_size) = store_raw_chunk_entry(&engine, 0x61, 512);
  engine.remove_kv_entry(&old_key).unwrap();
  engine.write_void_at(void_offset, void_size).unwrap();

  let new_key = vec![0x62; engine.hash_algo().hash_length()];
  let new_value = vec![0xA6; 128];
  let needed = aeordb::engine::entry_header::EntryHeader::compute_total_length(engine.hash_algo(), new_key.len(), new_value.len()).unwrap();
  let new_offset = engine.store_entry(EntryType::Chunk, &new_key, &new_value).unwrap();
  assert_eq!(new_offset, void_offset);
  let remainder_size = void_size - needed;
  assert!(remainder_size >= aeordb::engine::void_manager::MINIMUM_USEFUL_VOID_SIZE);

  let remainder = engine.read_entry_header_at(void_offset + needed as u64).unwrap();
  assert_eq!(remainder.entry_type, EntryType::Void);
  assert_eq!(remainder.total_length, remainder_size);

  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap().to_string();
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(&db_str).unwrap();
  assert_eq!(reopened.get_entry(&new_key).unwrap().unwrap().2, new_value);
  let report = verify::verify_checked(&reopened, &db_str).unwrap();
  assert_eq!(report.corrupt_header, 0);
  assert!(report.skipped_regions.is_empty());
}

#[test]
fn reusable_void_with_unencodable_remainder_is_preserved_and_write_appends() {
  let (engine, temp) = create_test_db();
  let old_key = vec![0x63; engine.hash_algo().hash_length()];
  let (void_offset, void_size) = store_raw_chunk_entry(&engine, 0x63, 256);
  engine.remove_kv_entry(&old_key).unwrap();
  engine.write_void_at(void_offset, void_size).unwrap();

  let new_key = vec![0x64; engine.hash_algo().hash_length()];
  let new_value = vec![0xA7; 232];
  let needed = aeordb::engine::entry_header::EntryHeader::compute_total_length(engine.hash_algo(), new_key.len(), new_value.len()).unwrap();
  assert!(void_size - needed < aeordb::engine::void_manager::MINIMUM_USEFUL_VOID_SIZE);

  let new_offset = engine.store_entry(EntryType::Chunk, &new_key, &new_value).unwrap();
  assert_ne!(new_offset, void_offset, "allocator must not strand an unencodable remainder");
  let preserved = engine.read_entry_header_at(void_offset).unwrap();
  assert_eq!(preserved.entry_type, EntryType::Void);
  assert_eq!(preserved.total_length, void_size);

  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap().to_string();
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(&db_str).unwrap();
  assert_eq!(reopened.get_entry(&new_key).unwrap().unwrap().2, new_value);
  let report = verify::verify_checked(&reopened, &db_str).unwrap();
  assert_eq!(report.corrupt_header, 0);
  assert!(report.skipped_regions.is_empty());
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
fn rebuild_kv_skips_keyless_physical_void_records() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  for version in 0..12 {
    let body = format!("version-{version:02}");
    ops.store_file_buffered(&ctx, "/gc/rebuild.txt", body.as_bytes(), Some("text/plain")).unwrap();
  }
  let gc_result = gc::run_gc(&engine, &ctx, false).unwrap();
  assert!(gc_result.garbage_entries > 0, "test setup should create physical void records");

  engine.rebuild_kv().unwrap();

  let db_path = temp.path().join("test.aeordb");
  let db_path = db_path.to_str().unwrap().to_string();
  let report = verify::verify_checked(&engine, &db_path).unwrap();
  assert!(!report.has_issues(), "rebuilding across physical voids damaged the live KV view: {report:?}");
  assert_eq!(ops.read_file_buffered("/gc/rebuild.txt").unwrap(), b"version-11");

  drop(ops);
  engine.shutdown().unwrap();
  drop(engine);
  let reopened = StorageEngine::open(&db_path).unwrap();
  let reopened_report = verify::verify_checked(&reopened, &db_path).unwrap();
  assert!(!reopened_report.has_issues(), "GC-void filtering was not durably published: {reopened_report:?}");
  assert_eq!(DirectoryOps::new(&reopened).read_file_buffered("/gc/rebuild.txt").unwrap(), b"version-11");
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
fn rebuild_directory_tree_uses_current_path_records_once() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();

  for version in 0..30 {
    let body = format!("version-{version:02}");
    ops.store_file_buffered(&ctx, "/repair/doc.txt", body.as_bytes(), Some("text/plain")).unwrap();
  }

  let before = file_size(db_str);
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let dirs_written = ops.rebuild_directory_tree(&ctx).unwrap();
  let after = file_size(db_str);

  assert_eq!(dirs_written, 2, "repair should rewrite only /repair and /, not every FileRecord copy");
  assert_eq!(
    engine.durability_snapshot().unwrap().next_sequence,
    sequence_before + dirs_written as u64,
    "a bounded full rebuild must publish exactly once per rebuilt directory"
  );
  assert!(after - before < 8192, "directory rebuild should append a small fixed amount, appended {} bytes", after - before);
  assert_eq!(ops.read_file_buffered("/repair/doc.txt").unwrap(), b"version-29");
}

#[test]
fn targeted_directory_repair_publishes_rebuilt_directory_and_ancestors_once() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/repair/targeted.txt", b"targeted", Some("text/plain")).unwrap();

  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  assert_eq!(ops.repair_directory_index_from_path_records("/repair").unwrap(), 1);
  assert_eq!(
    engine.durability_snapshot().unwrap().next_sequence,
    sequence_before + 1,
    "one targeted repair must publish the repaired directory and every ancestor through one hard-authority ticket"
  );
  assert_eq!(ops.read_file_buffered("/repair/targeted.txt").unwrap(), b"targeted");
}

#[test]
fn full_directory_repair_preserves_current_symlink_path_records() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/repair/target.txt", b"target", Some("text/plain")).unwrap();
  ops.store_symlink(&ctx, "/repair/current-link", "/repair/target.txt").unwrap();

  let dirs_written = ops.rebuild_directory_tree(&ctx).unwrap();

  assert_eq!(dirs_written, 2);
  let children = ops.list_directory("/repair").unwrap();
  assert!(children.iter().any(|child| child.name == "current-link" && child.entry_type == EntryType::Symlink.to_u8()));
  assert_eq!(ops.get_symlink("/repair/current-link").unwrap().unwrap().target, "/repair/target.txt");
}

#[test]
fn directory_repairs_rebuild_only_registry_selected_namespace_paths() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/.aeordb-permissions", b"permissions", Some("application/json")).unwrap();
  ops.store_file_buffered(&ctx, "/.aeordb-conflicts/item.json", b"conflict", Some("application/json")).unwrap();
  ops.store_file_buffered(&ctx, "/repair/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/repair/.aeordb-permissions", b"nested permissions", Some("application/json")).unwrap();
  ops.store_file_buffered(&ctx, "/repair/.aeordb-indexes/private.idx", b"derived", Some("application/octet-stream")).unwrap();

  ops.rebuild_directory_tree(&ctx).unwrap();
  let root = ops.list_directory("/").unwrap();
  assert!(root.iter().any(|child| child.name == ".aeordb-permissions"));
  assert!(!root.iter().any(|child| child.name == ".aeordb-conflicts"));
  let repaired = ops.list_directory("/repair").unwrap();
  assert!(repaired.iter().any(|child| child.name == "ordinary.txt"));
  assert!(repaired.iter().any(|child| child.name == ".aeordb-permissions"));
  assert!(!repaired.iter().any(|child| child.name == ".aeordb-indexes"));

  ops.repair_directory_index_from_path_records("/").unwrap();
  ops.repair_directory_index_from_path_records("/repair").unwrap();
  let targeted_root = ops.list_directory("/").unwrap();
  let targeted_repair = ops.list_directory("/repair").unwrap();
  assert!(targeted_root.iter().any(|child| child.name == ".aeordb-permissions"));
  assert!(!targeted_root.iter().any(|child| child.name == ".aeordb-conflicts"));
  assert!(targeted_repair.iter().any(|child| child.name == ".aeordb-permissions"));
  assert!(!targeted_repair.iter().any(|child| child.name == ".aeordb-indexes"));
}

#[test]
fn full_directory_repair_rejects_unknown_protected_paths_before_mutation() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/ordinary.txt", b"ordinary", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/.aeordb-future/item.json", b"unknown", Some("application/json")).unwrap();
  let root_key = directory_path_hash("/", &engine.hash_algo()).unwrap();
  let root_before = engine.get_entry(&root_key).unwrap().unwrap().2;
  let head_before = engine.head_hash().unwrap();

  let error = ops.rebuild_directory_tree(&ctx).unwrap_err();

  assert!(matches!(error, aeordb::engine::EngineError::SystemFamilyPolicy { code: "unknown_protected_system_family", .. }));
  assert_eq!(engine.get_entry(&root_key).unwrap().unwrap().2, root_before);
  assert_eq!(engine.head_hash().unwrap(), head_before);
}

#[test]
fn full_directory_repair_rebuilds_every_deep_ancestor_bottom_up() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/repair/a/b/c/deep.txt", b"deep", Some("text/plain")).unwrap();
  ops.store_symlink(&ctx, "/repair/a/current-link", "/repair/a/b/c/deep.txt").unwrap();

  let dirs_written = ops.rebuild_directory_tree(&ctx).unwrap();

  assert_eq!(dirs_written, 5, "repair must write /repair/a/b/c, each ancestor, and root exactly once");
  for (path, child) in [("/", "repair"), ("/repair", "a"), ("/repair/a", "b"), ("/repair/a/b", "c"), ("/repair/a/b/c", "deep.txt")] {
    let children = ops.list_directory(path).unwrap();
    assert!(children.iter().any(|entry| entry.name == child), "{path} did not contain rebuilt child {child}: {children:?}");
  }
  let repair_a = ops.list_directory("/repair/a").unwrap();
  assert!(repair_a.iter().any(|entry| entry.name == "current-link" && entry.entry_type == EntryType::Symlink.to_u8()));
  assert_eq!(ops.read_file_buffered("/repair/a/b/c/deep.txt").unwrap(), b"deep");
  assert_eq!(ops.get_symlink("/repair/a/current-link").unwrap().unwrap().target, "/repair/a/b/c/deep.txt");
}

#[test]
fn rebuild_directory_tree_skips_path_records_with_missing_chunks() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();

  ops.store_file_buffered(&ctx, "/broken/dangling.txt", b"missing chunk body", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let path_key = file_path_hash("/broken/dangling.txt", &algo).unwrap();
  let (header, _key, value) = engine.get_entry(&path_key).unwrap().expect("path FileRecord should exist");
  let record = FileRecord::deserialize(&value, algo.hash_length(), header.entry_version).unwrap();
  let chunk_hash = record.chunk_hashes.first().expect("test file should have one chunk").clone();
  let chunk_kv = engine.get_kv_entry(&chunk_hash).unwrap().expect("chunk should be live before corruption");

  engine.remove_kv_entry(&chunk_hash).unwrap();
  engine.write_void_at(chunk_kv.offset, chunk_kv.total_length).unwrap();

  let report = verify::verify(&engine, db_str);
  assert!(
    report.dangling_file_records.iter().any(|issue| issue.contains("/broken/dangling.txt")),
    "verify should report live path FileRecords with missing chunks: {:?}",
    report.dangling_file_records
  );

  let dirs_written = ops.rebuild_directory_tree(&ctx).unwrap();
  assert_eq!(dirs_written, 1, "only an empty root should be rebuilt when every path record is dangling");

  let root = ops.list_directory("/").unwrap();
  assert!(!root.iter().any(|child| child.name == "broken"), "repair must not re-list a file whose chunks are missing: {:?}", root);
  assert!(ops.read_file_buffered("/broken/dangling.txt").is_err(), "direct read should still report the underlying chunk loss");
}

#[test]
fn verify_uses_registry_policy_for_strict_rebuildable_and_unknown_paths() {
  let ctx = RequestContext::system();

  let (permissions_engine, permissions_temp) = create_test_db();
  let permissions_ops = DirectoryOps::new(&permissions_engine);
  permissions_ops.store_file_buffered(&ctx, "/.aeordb-permissions", b"strict permissions", Some("application/json")).unwrap();
  let permissions_key = file_path_hash("/.aeordb-permissions", &permissions_engine.hash_algo()).unwrap();
  let (permissions_header, _, permissions_value) = permissions_engine.get_entry(&permissions_key).unwrap().unwrap();
  let permissions_record =
    FileRecord::deserialize(&permissions_value, permissions_engine.hash_algo().hash_length(), permissions_header.entry_version).unwrap();
  permissions_engine.remove_kv_entry(&permissions_record.chunk_hashes[0]).unwrap();
  let permissions_report =
    verify::verify_checked(&permissions_engine, permissions_temp.path().join("test.aeordb").to_str().unwrap()).unwrap();
  assert!(
    permissions_report.dangling_file_records.iter().any(|issue| issue.contains("/.aeordb-permissions")),
    "strict registry family was not verified: {:?}",
    permissions_report.dangling_file_records
  );

  let (derived_engine, derived_temp) = create_test_db();
  let derived_ops = DirectoryOps::new(&derived_engine);
  let derived_path = "/docs/.aeordb-indexes/text.idx";
  derived_ops.store_file_buffered(&ctx, derived_path, b"rebuildable index", Some("application/octet-stream")).unwrap();
  let derived_key = file_path_hash(derived_path, &derived_engine.hash_algo()).unwrap();
  let (derived_header, _, derived_value) = derived_engine.get_entry(&derived_key).unwrap().unwrap();
  let derived_record =
    FileRecord::deserialize(&derived_value, derived_engine.hash_algo().hash_length(), derived_header.entry_version).unwrap();
  derived_engine.remove_kv_entry(&derived_record.chunk_hashes[0]).unwrap();
  let derived_report = verify::verify_checked(&derived_engine, derived_temp.path().join("test.aeordb").to_str().unwrap()).unwrap();
  assert!(
    !derived_report.dangling_file_records.iter().any(|issue| issue.contains(derived_path)),
    "rebuildable registry family was promoted to fatal dangling data: {:?}",
    derived_report.dangling_file_records
  );

  let (unknown_engine, unknown_temp) = create_test_db();
  DirectoryOps::new(&unknown_engine)
    .store_file_buffered(&ctx, "/.aeordb-future/item.json", b"unknown protected", Some("application/json"))
    .unwrap();
  let error = verify::verify_checked(&unknown_engine, unknown_temp.path().join("test.aeordb").to_str().unwrap()).unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::SystemFamilyPolicy { code: "unknown_protected_system_family", .. }));
}

#[test]
fn full_directory_repair_refuses_memory_pressure_before_mutation() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/pressure/full.txt", b"unchanged", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let root_before = engine.head_hash().unwrap();
  let pressure_dir_key = directory_path_hash("/pressure", &algo).unwrap();
  let pressure_dir_before = engine.get_entry(&pressure_dir_key).unwrap().unwrap().2;
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining_critical = policy.emergency_reserve_bytes.checked_sub(snapshot.critical_reserved_bytes).unwrap();
  let pressure =
    coordinator.reserve(MemoryOwner::Repair, remaining_critical, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery)).unwrap();

  let error = ops.rebuild_directory_tree(&ctx).unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::ResourceExhausted(_)), "unexpected repair error: {error}");
  assert_eq!(engine.head_hash().unwrap(), root_before, "failed repair must not advance HEAD");
  assert_eq!(engine.get_entry(&pressure_dir_key).unwrap().unwrap().2, pressure_dir_before, "failed repair must not rewrite a directory");

  drop(pressure);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0, "failed repair must release every owned reservation");
  assert_eq!(ops.read_file_buffered("/pressure/full.txt").unwrap(), b"unchanged");
}

#[test]
fn targeted_directory_repair_refuses_memory_pressure_before_mutation() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/pressure/targeted.txt", b"unchanged", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let pressure_dir_key = directory_path_hash("/pressure", &algo).unwrap();
  let pressure_dir_before = engine.get_entry(&pressure_dir_key).unwrap().unwrap().2;
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining_critical = policy.emergency_reserve_bytes.checked_sub(snapshot.critical_reserved_bytes).unwrap();
  let pressure =
    coordinator.reserve(MemoryOwner::Repair, remaining_critical, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery)).unwrap();

  let error = ops.repair_directory_index_from_path_records("/pressure").unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::ResourceExhausted(_)), "unexpected repair error: {error}");
  assert_eq!(
    engine.get_entry(&pressure_dir_key).unwrap().unwrap().2,
    pressure_dir_before,
    "failed targeted repair must not rewrite a directory"
  );

  drop(pressure);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0, "failed targeted repair must release every owned reservation");
  assert_eq!(ops.read_file_buffered("/pressure/targeted.txt").unwrap(), b"unchanged");
}

#[test]
fn verify_reports_btree_directory_issue_and_repair_rebuilds_tree() {
  let (engine, temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap();
  let count = BTREE_CONVERSION_THRESHOLD + 100;

  for i in 0..count {
    ops.store_file_buffered(&ctx, &format!("/btree-repair/file_{:05}.txt", i), b"content", Some("text/plain")).unwrap();
  }

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let dir_key = directory_path_hash("/btree-repair", &algo).unwrap();
  let (_header, _key, raw) = engine.get_entry(&dir_key).unwrap().unwrap();
  let root_data = if raw.len() == hash_length { engine.get_entry(&raw).unwrap().unwrap().2 } else { raw };
  let root_node = BTreeNode::deserialize(&root_data, hash_length, 0).unwrap();
  let child_to_delete = match root_node {
    BTreeNode::Internal(internal) => internal.children[1].clone(),
    BTreeNode::Leaf(_) => panic!("expected internal B-tree root"),
  };

  engine.mark_entry_deleted(&child_to_delete).unwrap();

  let report = verify::verify(&engine, db_str);
  assert!(
    report.btree_directory_issues.iter().any(|issue| issue.contains("/btree-repair")),
    "verify should report damaged B-tree directories: {:?}",
    report.btree_directory_issues
  );

  let repair_report = verify::verify_and_repair(&engine, db_str);
  assert!(
    repair_report.repairs.iter().any(|repair| repair.contains("B-tree directory repaired from path records: /btree-repair")),
    "repair should use targeted B-tree directory repair when only B-tree branches are damaged: {:?}",
    repair_report.repairs
  );
  assert!(
    !repair_report.repairs.iter().any(|repair| repair.contains("Directory tree rebuilt")),
    "targeted B-tree repair should avoid the full directory tree rebuild fallback when it succeeds: {:?}",
    repair_report.repairs
  );

  drop(engine);
  let reopened = StorageEngine::open(db_str).unwrap();
  let reopened_ops = DirectoryOps::new(&reopened);
  let repaired_children = reopened_ops.list_directory("/btree-repair").unwrap();
  assert_eq!(repaired_children.len(), count, "targeted repair should restore every live path-key child");
}

#[test]
fn targeted_btree_repair_preserves_symlinks_and_implied_child_directories() {
  let (engine, _temp) = create_test_db();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let count = BTREE_CONVERSION_THRESHOLD + 100;

  for i in 0..count {
    ops.store_file_buffered(&ctx, &format!("/btree-mixed/file_{:05}.txt", i), b"content", Some("text/plain")).unwrap();
  }
  ops.store_file_buffered(&ctx, "/btree-mixed/nested/deep.txt", b"deep", Some("text/plain")).unwrap();
  ops.store_symlink(&ctx, "/btree-mixed/link", "/btree-mixed/file_00000.txt").unwrap();

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let dir_key = directory_path_hash("/btree-mixed", &algo).unwrap();
  let (_header, _key, raw) = engine.get_entry(&dir_key).unwrap().unwrap();
  let root_data = if raw.len() == hash_length { engine.get_entry(&raw).unwrap().unwrap().2 } else { raw };
  let root_node = BTreeNode::deserialize(&root_data, hash_length, 0).unwrap();
  let child_to_delete = match root_node {
    BTreeNode::Internal(internal) => internal.children[1].clone(),
    BTreeNode::Leaf(_) => panic!("expected internal B-tree root"),
  };
  engine.mark_entry_deleted(&child_to_delete).unwrap();

  let partial_children = ops.list_directory("/btree-mixed").unwrap();
  assert!(partial_children.len() < count + 2, "test setup should damage one B-tree branch");

  let repaired = ops.repair_directory_index_from_path_records("/btree-mixed").unwrap();
  assert_eq!(repaired, 1);

  let children = ops.list_directory("/btree-mixed").unwrap();
  assert_eq!(children.len(), count + 2);
  assert!(children.iter().any(|child| child.name == "link" && child.entry_type == EntryType::Symlink.to_u8()));
  assert!(children.iter().any(|child| child.name == "nested" && child.entry_type == EntryType::DirectoryIndex.to_u8()));
  assert_eq!(ops.read_file_buffered("/btree-mixed/nested/deep.txt").unwrap(), b"deep");
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
  assert!(expanded_header.kv_block_length >= expected_stage_size, "online expansion must reserve at least the full KV stage size");
  let new_wal_start = expanded_header.kv_block_offset + expanded_header.kv_block_length;
  assert_eq!(read_u32_at(&db_str, new_wal_start), ENTRY_MAGIC, "expanded KV slack must end on a complete WAL entry boundary");
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
fn kv_expansion_relocates_the_complete_final_straddling_entry() {
  let (db_str, _temp) = raw_test_db();
  let initial_header = active_header(&db_str);
  let old_wal_start = initial_header.kv_block_offset + initial_header.kv_block_length;
  let engine = StorageEngine::open(&db_str).unwrap();
  let key = vec![0x6Au8; engine.hash_algo().hash_length()];
  let value = vec![0xC3u8; 600 * 1024];
  let offset = engine.store_entry(EntryType::DirectoryIndex, &key, &value).unwrap();
  assert_eq!(offset, old_wal_start, "the final test entry must begin at the old WAL frontier");

  engine.expand_kv_block_online(1).unwrap();

  let (_header, stored_key, stored_value) = engine.get_entry(&key).unwrap().expect("relocated entry must remain indexed and readable");
  assert_eq!(stored_key, key);
  assert_eq!(stored_value, value, "expansion must relocate the tail of the last entry even when no later entry marks its boundary");
  let report = verify::verify(&engine, &db_str);
  assert!(report.invalid_kv_offsets.is_empty(), "relocated entry must remain outside the expanded KV block");
}

#[test]
fn kv_expansion_preflight_failure_preserves_the_published_kv_view() {
  let (db_str, _temp) = raw_test_db();
  let engine = StorageEngine::open(&db_str).unwrap();
  let key = vec![0x71u8; engine.hash_algo().hash_length()];
  let offset = engine.store_entry(EntryType::DirectoryIndex, &key, b"boundary-check").unwrap();

  {
    let mut file = OpenOptions::new().write(true).open(&db_str).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    file.sync_all().unwrap();
  }

  let error = engine.expand_kv_block_online(1).expect_err("corrupt WAL boundary must refuse expansion before mutation");
  assert!(error.to_string().to_ascii_lowercase().contains("magic"), "unexpected preflight error: {error}");
  assert_eq!(engine.is_entry_deleted(&key).unwrap(), false, "preflight refusal must leave the old KV snapshot readable");
  assert!(engine.kv_page_provider_stats().unwrap().is_some(), "preflight refusal must keep the bounded provider active");
}

#[test]
fn kv_expansion_preflight_rejects_internally_inconsistent_entry_lengths_before_mutation() {
  let (db_str, _temp) = raw_test_db();
  let engine = StorageEngine::open(&db_str).unwrap();
  let key = vec![0x72u8; engine.hash_algo().hash_length()];
  let offset = engine.store_entry(EntryType::DirectoryIndex, &key, b"length-consistency").unwrap();
  let header = engine.read_entry_header_at(offset).unwrap();

  {
    let mut file = OpenOptions::new().write(true).open(&db_str).unwrap();
    file.seek(SeekFrom::Start(offset + 11)).unwrap();
    file.write_all(&header.key_length.saturating_add(1).to_le_bytes()).unwrap();
    file.sync_all().unwrap();
  }
  let before = std::fs::read(&db_str).unwrap();

  let error = engine.expand_kv_block_online(1).expect_err("inconsistent entry lengths must fail read-only expansion preflight");
  assert!(error.to_string().contains("exceeds total_length"), "unexpected preflight error: {error}");
  assert!(engine.durability_failure().is_none(), "read-only boundary refusal must not latch write admission");
  assert_eq!(std::fs::read(&db_str).unwrap(), before, "preflight failure must not publish a resize marker or relocate bytes");
  assert!(engine.kv_page_provider_stats().unwrap().is_some(), "preflight refusal must leave the bounded provider active");
}

#[test]
fn kv_expansion_places_a_short_wal_after_the_new_block() {
  let (db_str, _temp) = raw_test_db();
  let engine = StorageEngine::open(&db_str).unwrap();
  let key = vec![0x91u8; engine.hash_algo().hash_length()];
  let value = vec![0x4Du8; 16 * 1024];
  engine.store_entry(EntryType::DirectoryIndex, &key, &value).unwrap();

  engine.expand_kv_block_online(1).unwrap();

  let header = active_header(&db_str);
  let new_kv_end = header.kv_block_offset + header.kv_block_length;
  assert!(header.hot_tail_offset >= new_kv_end, "expanded hot tail must not remain inside the new KV block");
  let mut file = OpenOptions::new().read(true).open(&db_str).unwrap();
  assert!(
    hot_tail::read_hot_tail(&mut file, header.hot_tail_offset, header.hash_algo.hash_length()).is_some(),
    "completed expansion must publish a valid hot tail after the new block"
  );
  assert_eq!(engine.get_entry(&key).unwrap().unwrap().2, value, "short-WAL relocation must preserve the copied entry");
  let report = verify::verify(&engine, &db_str);
  assert!(report.invalid_kv_offsets.is_empty());
}

#[test]
fn startup_retries_a_pre_relocation_expansion_marker_and_clears_both_phase_fields() {
  let (db_str, _temp) = raw_test_db();
  let key = vec![0xA1u8; 32];
  let value = b"pre-relocation-recovery".to_vec();
  {
    let engine = StorageEngine::open(&db_str).unwrap();
    engine.store_entry(EntryType::DirectoryIndex, &key, &value).unwrap();
    engine.shutdown().unwrap();
  }

  {
    let mut writer = AppendWriter::open(std::path::Path::new(&db_str)).unwrap();
    let mut marker = writer.file_header().clone();
    marker.resize_in_progress = true;
    marker.resize_target_stage = 1;
    writer.update_file_header(&marker).unwrap();
  }

  let reopened = StorageEngine::open(&db_str).unwrap();
  assert_eq!(reopened.get_entry(&key).unwrap().unwrap().2, value);
  let recovered = active_header(&db_str);
  assert_eq!(recovered.kv_block_stage, 1);
  assert!(!recovered.resize_in_progress, "completed recovery must clear the pre-relocation phase marker");
  assert_eq!(recovered.resize_target_stage, 0);
  let report = verify::verify(&reopened, &db_str);
  assert!(report.invalid_kv_offsets.is_empty());
}

#[test]
fn pre_relocation_recovery_does_not_trust_an_unselected_future_hot_tail() {
  let (db_str, _temp) = raw_test_db();
  {
    let engine = StorageEngine::open(&db_str).unwrap();
    engine.store_entry(EntryType::DirectoryIndex, &[0xA3; 32], b"selected-old-hot-tail").unwrap();
    engine.shutdown().unwrap();
  }

  let original = active_header(&db_str);
  let old_kv_end = original.kv_block_offset + original.kv_block_length;
  let target_block_length = stage_params(1, page_size(original.hash_algo.hash_length())).0;
  let new_kv_end = original.kv_block_offset + target_block_length;
  assert!(original.hot_tail_offset < new_kv_end, "test requires a short WAL relocated beyond the expanded block");
  let relocation_bytes = original.hot_tail_offset - old_kv_end;
  let future_hot_tail = new_kv_end + relocation_bytes;

  {
    let mut writer = AppendWriter::open(std::path::Path::new(&db_str)).unwrap();
    let mut marker = writer.file_header().clone();
    marker.resize_in_progress = true;
    marker.resize_target_stage = 1;
    writer.update_file_header(&marker).unwrap();
  }
  {
    let mut file = OpenOptions::new().read(true).write(true).open(&db_str).unwrap();
    let stale = HotTailPayload {
      writes: vec![KVEntry { type_flags: KV_TYPE_CHUNK, hash: vec![0xEE; 32], offset: new_kv_end, total_length: 64 }],
      voids: Vec::new(),
    };
    hot_tail::write_hot_tail(&mut file, future_hot_tail, &stale, original.hash_algo.hash_length()).unwrap();
    file.sync_all().unwrap();
  }

  aeordb::engine::kv_expand::expand_kv_block(&db_str, 1, original.hash_algo.hash_length()).unwrap();

  let recovered = active_header(&db_str);
  let mut file = OpenOptions::new().read(true).open(&db_str).unwrap();
  let payload = hot_tail::read_hot_tail(&mut file, recovered.hot_tail_offset, recovered.hash_algo.hash_length()).unwrap();
  assert!(
    payload.writes.iter().all(|entry| entry.hash != vec![0xEE; 32]),
    "bytes at an unselected future offset must never become recovery authority"
  );
}

#[test]
fn startup_finalizes_a_relocation_durable_marker_without_relocating_wal_twice() {
  let (db_str, _temp) = raw_test_db();
  let initial = active_header(&db_str);
  let key = vec![0xA2u8; 32];
  let value = vec![0x5Du8; 96 * 1024];
  {
    let engine = StorageEngine::open(&db_str).unwrap();
    engine.store_entry(EntryType::DirectoryIndex, &key, &value).unwrap();
    engine.expand_kv_block_online(1).unwrap();
    engine.shutdown().unwrap();
  }

  let expanded = active_header(&db_str);
  let relocation_frontier = expanded.hot_tail_offset;
  {
    let mut writer = AppendWriter::open(std::path::Path::new(&db_str)).unwrap();
    let mut marker = writer.file_header().clone();
    marker.kv_block_stage = initial.kv_block_stage;
    marker.resize_in_progress = false;
    marker.resize_target_stage = 1;
    marker.hot_tail_offset = relocation_frontier;
    writer.update_file_header(&marker).unwrap();
  }

  let reopened = StorageEngine::open(&db_str).unwrap();
  let recovered = active_header(&db_str);
  assert_eq!(recovered.kv_block_stage, 1);
  assert_eq!(recovered.hot_tail_offset, relocation_frontier, "relocation-durable recovery must not shift the WAL a second time");
  assert!(!recovered.resize_in_progress);
  assert_eq!(recovered.resize_target_stage, 0);
  assert_eq!(reopened.get_entry(&key).unwrap().unwrap().2, value);
  let report = verify::verify(&reopened, &db_str);
  assert!(report.invalid_kv_offsets.is_empty());
}

#[test]
fn startup_refuses_a_corrupt_pre_relocation_boundary_without_mutating_the_database() {
  let (db_str, _temp) = raw_test_db();
  let old_wal_start = {
    let engine = StorageEngine::open(&db_str).unwrap();
    let header = active_header(&db_str);
    let old_wal_start = header.kv_block_offset + header.kv_block_length;
    engine.store_entry(EntryType::DirectoryIndex, &[0xB1; 32], b"must-not-move").unwrap();
    engine.shutdown().unwrap();
    old_wal_start
  };
  {
    let mut writer = AppendWriter::open(std::path::Path::new(&db_str)).unwrap();
    let mut marker = writer.file_header().clone();
    marker.resize_in_progress = true;
    marker.resize_target_stage = 1;
    writer.update_file_header(&marker).unwrap();
  }
  {
    let mut file = OpenOptions::new().write(true).open(&db_str).unwrap();
    file.seek(SeekFrom::Start(old_wal_start)).unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    file.sync_all().unwrap();
  }
  let before = std::fs::read(&db_str).unwrap();

  let error = match StorageEngine::open(&db_str) {
    Ok(_) => panic!("corrupt expansion boundary must abort startup"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("requires explicit repair"), "unexpected startup error: {error}");
  assert_eq!(std::fs::read(&db_str).unwrap(), before, "failed expansion preflight must not mutate database bytes");
}

#[test]
fn startup_refuses_an_out_of_range_expansion_stage_before_allocation_or_mutation() {
  let (db_str, _temp) = raw_test_db();
  {
    let mut writer = AppendWriter::open(std::path::Path::new(&db_str)).unwrap();
    let mut marker = writer.file_header().clone();
    marker.resize_in_progress = true;
    marker.resize_target_stage = u8::MAX;
    writer.update_file_header(&marker).unwrap();
  }
  let before = std::fs::read(&db_str).unwrap();

  let error = match StorageEngine::open(&db_str) {
    Ok(_) => panic!("unsupported expansion target must abort startup"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("outside the supported stage table"), "unexpected startup error: {error}");
  assert_eq!(std::fs::read(&db_str).unwrap(), before, "malformed target refusal must happen before mutation");
}

#[test]
fn startup_refuses_a_corrupt_relocation_durable_wal_before_zeroing_the_block() {
  let (db_str, _temp) = raw_test_db();
  let initial = active_header(&db_str);
  {
    let engine = StorageEngine::open(&db_str).unwrap();
    engine.store_entry(EntryType::DirectoryIndex, &[0xB2; 32], &vec![0x6E; 96 * 1024]).unwrap();
    engine.expand_kv_block_online(1).unwrap();
    engine.shutdown().unwrap();
  }
  let expanded = active_header(&db_str);
  let relocated_wal_start = expanded.kv_block_offset + expanded.kv_block_length;
  {
    let mut writer = AppendWriter::open(std::path::Path::new(&db_str)).unwrap();
    let mut marker = writer.file_header().clone();
    marker.kv_block_stage = initial.kv_block_stage;
    marker.resize_in_progress = false;
    marker.resize_target_stage = 1;
    writer.update_file_header(&marker).unwrap();
  }
  {
    let mut file = OpenOptions::new().write(true).open(&db_str).unwrap();
    file.seek(SeekFrom::Start(relocated_wal_start)).unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    file.sync_all().unwrap();
  }
  let before = std::fs::read(&db_str).unwrap();

  let error = match StorageEngine::open(&db_str) {
    Ok(_) => panic!("corrupt relocation-durable WAL must abort startup"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("requires explicit repair"), "unexpected startup error: {error}");
  assert_eq!(std::fs::read(&db_str).unwrap(), before, "phase-2 WAL refusal must happen before zeroing the KV block");
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

  assert!(active_header(&db_str).kv_block_stage > 0, "test setup should force at least one KV expansion");
  engine.shutdown().unwrap();
  drop(engine);
  let expanded_header = active_header(&db_str);

  {
    let mut file = OpenOptions::new().read(true).write(true).open(&db_str).unwrap();
    file.seek(SeekFrom::Start(expanded_header.hot_tail_offset)).unwrap();
    file.write_all(&[0u8; 5]).unwrap();
    file.sync_all().unwrap();
  }

  let reopened = StorageEngine::open(&db_str).unwrap();
  let report = verify::verify(&reopened, &db_str);
  let rebuilt_header = active_header(&db_str);

  assert!(
    report.invalid_kv_offsets.is_empty(),
    "dirty rebuild after expansion must not preserve KV offsets inside the reserved block: {:?}",
    report.invalid_kv_offsets
  );
  assert_eq!(
    rebuilt_header.kv_block_length, expanded_header.kv_block_length,
    "dirty rebuild must preserve the selected boundary-aligned KV span instead of reclassifying reserved slack as WAL"
  );
  assert_eq!(
    report.corrupt_header, 0,
    "reserved KV slack must not be reported as a corrupt WAL header (expanded_length={}, rebuilt_length={}, skipped={:?})",
    expanded_header.kv_block_length, rebuilt_header.kv_block_length, report.skipped_regions
  );
  assert!(report.skipped_regions.is_empty(), "reserved KV slack must not become a skipped WAL region: {:?}", report.skipped_regions);
}

#[test]
fn dirty_recovery_accepts_only_a_contiguous_verified_tail_after_the_durable_frontier() {
  let (engine, temp) = create_test_db();
  let db_path = temp.path().join("test.aeordb");
  let db_str = db_path.to_str().unwrap().to_string();
  engine.shutdown().unwrap();
  drop(engine);
  let selected_tail = active_header(&db_str).hot_tail_offset;

  let recovered_key = vec![0xA5; 32];
  let recovered_value = b"acknowledged-after-selected-frontier";
  let mut writer = AppendWriter::open(&db_path).unwrap();
  writer.set_offset(selected_tail);
  writer.append_entry(EntryType::Chunk, &recovered_key, recovered_value, 0).unwrap();
  let recovered_end = writer.current_offset();
  writer.sync().unwrap();
  drop(writer);

  let stale_key = vec![0x5A; 32];
  {
    let mut file = OpenOptions::new().write(true).open(&db_path).unwrap();
    file.seek(SeekFrom::Start(recovered_end)).unwrap();
    file.write_all(&[0xFF]).unwrap();
    file.sync_data().unwrap();
  }
  let mut writer = AppendWriter::open(&db_path).unwrap();
  writer.set_offset(recovered_end + 1);
  writer.append_entry(EntryType::Chunk, &stale_key, b"valid-looking-stale-tail-bytes", 0).unwrap();
  writer.sync().unwrap();
  drop(writer);

  let reopened = StorageEngine::open(&db_str).unwrap();
  assert_eq!(reopened.get_entry(&recovered_key).unwrap().unwrap().2, recovered_value);
  assert!(
    reopened.get_entry(&stale_key).unwrap().is_none(),
    "recovery must not scan past a tail discontinuity into stale valid-looking bytes"
  );
  let report = verify::verify_checked(&reopened, &db_str).unwrap();
  assert!(!report.has_issues(), "dirty recovery must truncate stale tail residue at the last contiguous verified entry: {report:?}");
}

#[cfg(unix)]
#[test]
fn dirty_rebuild_post_marker_failure_spills_before_runtime_configuration_is_loaded() {
  const CHILD_MARKER: &str = "AEORDB_DIRTY_REBUILD_SPILL_CHILD";
  if std::env::var_os(CHILD_MARKER).is_none() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
      .arg("--exact")
      .arg("dirty_rebuild_post_marker_failure_spills_before_runtime_configuration_is_loaded")
      .arg("--nocapture")
      .env(CHILD_MARKER, "1")
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "isolated dirty-rebuild fault child failed\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
    return;
  }

  let temp = tempfile::tempdir().unwrap();
  let db_path = temp.path().join("dirty-rebuild-spill.aeordb");
  let spill_path = temp.path().join("spill");
  let temp_fallback = temp.path().join("tmp");
  std::fs::create_dir(&temp_fallback).unwrap();
  unsafe {
    std::env::set_var("AEORDB_RECOVERY_EMERGENCY_SPILL_DIR", &spill_path);
    std::env::set_var("XDG_DATA_HOME", temp.path().join("xdg"));
    std::env::set_var("TMPDIR", temp_fallback);
  }

  let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
  for index in 0..700u64 {
    engine.store_entry(EntryType::Chunk, &index.to_le_bytes(), b"dirty rebuild spill fixture").unwrap();
  }
  engine.shutdown().unwrap();
  drop(engine);
  let header = active_header(db_path.to_str().unwrap());
  {
    let mut file = OpenOptions::new().write(true).open(&db_path).unwrap();
    file.seek(SeekFrom::Start(header.hot_tail_offset)).unwrap();
    file.write_all(&[0u8; 4]).unwrap();
    file.sync_all().unwrap();
  }

  let constrained = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
  let constrained_by_callback = std::sync::Arc::clone(&constrained);
  let progress = std::sync::Arc::new(move |progress: aeordb::engine::storage_engine::EngineStartupProgress| {
    if progress.phase == "rebuild_kv_insert" && !constrained_by_callback.swap(true, std::sync::atomic::Ordering::AcqRel) {
      unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let mut current = std::mem::zeroed::<libc::rlimit>();
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut current), 0);
        let limit = libc::rlimit { rlim_cur: (64 * 1024) as libc::rlim_t, rlim_max: current.rlim_max };
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
      }
    }
  });

  let error = match StorageEngine::open_with_hot_dir_and_progress(db_path.to_str().unwrap(), None, Some(progress)) {
    Ok(_) => panic!("the injected post-marker KV page write must abort dirty startup"),
    Err(error) => error,
  };
  assert!(constrained.load(std::sync::atomic::Ordering::Acquire), "fault was not installed after marker publication");
  assert!(error.to_string().contains("KV rebuild failed after dirty-startup marker publication"), "unexpected open failure: {error}");

  let artifacts = aeordb::engine::emergency_spill::scan_for_database_with_dirs(&db_path, &[spill_path]).unwrap();
  assert_eq!(artifacts.len(), 1, "post-marker startup failure must leave one restart-blocking spill incident");
  assert!(artifacts[0].manifest_path.is_file());
}
