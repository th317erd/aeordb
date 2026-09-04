//! Regression tests for the 2026-05-11 xenocept corruption:
//! header.hot_tail_offset > file_size after a kill-mid-write.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use aeordb::engine::file_header::{FileHeader, read_active_header, FILE_HEADER_SIZE, FILE_MAGIC};
use aeordb::engine::{inspect_header, repair_header_in_place, DirectoryOps, HashAlgorithm, RequestContext, StorageEngine};

fn make_temp_db() -> (tempfile::TempDir, String) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("test.aeordb").to_string_lossy().to_string();
  (dir, path)
}

#[test]
fn offline_header_inspection_and_repair_reject_future_versions_without_mutation() {
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("future-header.aeordb");
  StorageEngine::create(path.to_str().unwrap()).unwrap().shutdown().unwrap();
  let mut bytes = std::fs::read(&path).unwrap();
  bytes[4] = 99;
  let checksum = crc32fast::hash(&bytes[..aeordb::engine::file_header::FILE_HEADER_SIZE - 4]);
  bytes[aeordb::engine::file_header::FILE_HEADER_SIZE - 4..aeordb::engine::file_header::FILE_HEADER_SIZE]
    .copy_from_slice(&checksum.to_le_bytes());
  std::fs::write(&path, &bytes).unwrap();
  let before = std::fs::read(&path).unwrap();

  let inspect_error = inspect_header(path.to_str().unwrap()).unwrap_err();
  let repair_error = repair_header_in_place(path.to_str().unwrap()).unwrap_err();

  assert!(matches!(inspect_error, aeordb::engine::EngineError::InvalidEntryVersion(99)));
  assert!(matches!(repair_error, aeordb::engine::EngineError::InvalidEntryVersion(99)));
  assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn offline_header_inspection_and_repair_reject_unsupported_hash_width_without_mutation() {
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("wide-header.aeordb");
  StorageEngine::create(path.to_str().unwrap()).unwrap().shutdown().unwrap();
  let mut bytes = std::fs::read(&path).unwrap();
  bytes[5..7].copy_from_slice(&HashAlgorithm::Sha512.to_u16().to_le_bytes());
  let checksum = crc32fast::hash(&bytes[..aeordb::engine::file_header::FILE_HEADER_SIZE - 4]);
  bytes[aeordb::engine::file_header::FILE_HEADER_SIZE - 4..aeordb::engine::file_header::FILE_HEADER_SIZE]
    .copy_from_slice(&checksum.to_le_bytes());
  std::fs::write(&path, &bytes).unwrap();
  let before = std::fs::read(&path).unwrap();

  let inspect_error = inspect_header(path.to_str().unwrap()).unwrap_err();
  let repair_error = repair_header_in_place(path.to_str().unwrap()).unwrap_err();

  assert!(inspect_error.to_string().contains("64-byte"), "{inspect_error}");
  assert!(repair_error.to_string().contains("64-byte"), "{repair_error}");
  assert_eq!(std::fs::read(&path).unwrap(), before);
}

fn write_v2_fixture(path: &str, payload: &[u8]) {
  let mut header = [0u8; FILE_HEADER_SIZE];
  header[..4].copy_from_slice(FILE_MAGIC);
  header[4] = 2;
  header[5..7].copy_from_slice(&HashAlgorithm::Blake3_256.to_u16().to_le_bytes());
  let mut pos = 7usize;
  for value in [1_700_000_000_000i64, 1_700_000_000_001i64] {
    header[pos..pos + 8].copy_from_slice(&value.to_le_bytes());
    pos += 8;
  }
  header[pos..pos + 8].copy_from_slice(&(FILE_HEADER_SIZE as u64).to_le_bytes());
  pos += 8;
  header[pos..pos + 8].copy_from_slice(&(payload.len() as u64).to_le_bytes());
  pos += 8;
  header[pos] = 1;
  pos += 1;
  pos += 8; // nvt_offset
  pos += 8; // nvt_length
  header[pos] = 1;
  pos += 1;
  pos += HashAlgorithm::Blake3_256.hash_length(); // head_hash
  header[pos..pos + 8].copy_from_slice(&1u64.to_le_bytes());
  pos += 8;
  pos += 1; // resize_in_progress
  pos += 8; // buffer_kvs_offset
  pos += 8; // buffer_nvt_offset
  let old_size = FILE_HEADER_SIZE as u64 + payload.len() as u64;
  header[pos..pos + 8].copy_from_slice(&old_size.to_le_bytes());
  pos += 8;
  header[pos] = 0;
  pos += 1;
  header[pos] = 0;
  pos += 1;
  header[pos] = 0;
  let crc = crc32fast::hash(&header[..FILE_HEADER_SIZE - 4]);
  header[FILE_HEADER_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());

  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  file.write_all(&header).unwrap();
  file.write_all(payload).unwrap();
  file.sync_all().unwrap();
}

#[test]
fn repair_rejects_checksum_failed_v2_header_without_mutation() {
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("checksum-failed-v2.aeordb");
  write_v2_fixture(path.to_str().unwrap(), b"legacy payload");

  let mut bytes = std::fs::read(&path).unwrap();
  bytes[50] ^= 0xA5;
  std::fs::write(&path, &bytes).unwrap();
  let before = std::fs::read(&path).unwrap();

  let report = inspect_header(path.to_str().unwrap()).unwrap();
  let error = repair_header_in_place(path.to_str().unwrap()).unwrap_err();

  assert!(report.crc_failed);
  assert!(error.to_string().contains("no redundant slot"), "{error}");
  assert_eq!(std::fs::read(&path).unwrap(), before);
}

fn write_v2_no_kv_fixture(path: &str, source_header: &FileHeader, wal: &[u8]) {
  let mut header = [0u8; FILE_HEADER_SIZE];
  header[..4].copy_from_slice(FILE_MAGIC);
  header[4] = 2;
  header[5..7].copy_from_slice(&source_header.hash_algo.to_u16().to_le_bytes());
  let hash_length = source_header.hash_algo.hash_length();
  let mut pos = 7usize;
  for value in [source_header.created_at, source_header.updated_at] {
    header[pos..pos + 8].copy_from_slice(&value.to_le_bytes());
    pos += 8;
  }
  pos += 8; // no in-file KV offset
  pos += 8; // no in-file KV length
  header[pos] = source_header.kv_block_version;
  pos += 1;
  pos += 8; // nvt_offset
  pos += 8; // nvt_length
  header[pos] = source_header.nvt_version;
  pos += 1;
  header[pos..pos + hash_length].copy_from_slice(&source_header.head_hash);
  pos += hash_length;
  header[pos..pos + 8].copy_from_slice(&source_header.entry_count.to_le_bytes());
  pos += 8;
  pos += 1; // resize_in_progress
  pos += 8; // buffer_kvs_offset
  pos += 8; // buffer_nvt_offset
  pos += 8; // no in-file hot-tail checkpoint
  pos += 1; // kv_block_stage
  pos += 1; // resize_target_stage
  header[pos] = source_header.backup_type;
  pos += 1;
  header[pos..pos + hash_length].copy_from_slice(&source_header.base_hash);
  pos += hash_length;
  header[pos..pos + hash_length].copy_from_slice(&source_header.target_hash);
  assert!(pos + hash_length <= FILE_HEADER_SIZE - 4);
  let crc = crc32fast::hash(&header[..FILE_HEADER_SIZE - 4]);
  header[FILE_HEADER_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());

  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  file.write_all(&header).unwrap();
  file.write_all(wal).unwrap();
  file.sync_all().unwrap();
}

/// Simulate the xenocept failure mode: corrupt the header so hot_tail_offset
/// points beyond the file's actual EOF.
fn poison_hot_tail_offset_past_eof(path: &str) {
  let file_size = std::fs::metadata(path).unwrap().len();
  let phantom_offset = file_size + 57_064; // arbitrary, just must exceed EOF
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  // hot_tail_offset position depends on header version. v3 inserts a u64
  // sequence field at byte 7, shifting hot_tail_offset from 114 → 122.
  let mut version_byte = [0u8; 1];
  file.seek(SeekFrom::Start(4)).unwrap();
  file.read_exact(&mut version_byte).unwrap();
  let hot_tail_pos: u64 = if version_byte[0] >= 3 { 122 } else { 114 };

  file.seek(SeekFrom::Start(hot_tail_pos)).unwrap();
  file.write_all(&phantom_offset.to_le_bytes()).unwrap();
  // Recompute CRC so the corruption looks like a real fsync-ordering
  // bug, not a CRC failure.
  let mut bytes = [0u8; aeordb::engine::FILE_HEADER_SIZE];
  file.seek(SeekFrom::Start(0)).unwrap();
  file.read_exact(&mut bytes).unwrap();
  let new_crc = crc32fast::hash(&bytes[..aeordb::engine::FILE_HEADER_SIZE - 4]);
  file.seek(SeekFrom::Start((aeordb::engine::FILE_HEADER_SIZE - 4) as u64)).unwrap();
  file.write_all(&new_crc.to_le_bytes()).unwrap();
  file.sync_all().unwrap();
}

#[test]
fn inspect_detects_hot_tail_past_eof() {
  let (_dir, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }

  poison_hot_tail_offset_past_eof(&path);

  let report = inspect_header(&path).unwrap();
  assert!(report.hot_tail_past_eof.is_some());
  let mismatch = report.hot_tail_past_eof.unwrap();
  assert!(mismatch.bytes_past_eof > 0);
  assert_eq!(mismatch.bytes_past_eof, 57_064);
}

#[test]
fn repair_recovers_data_after_hot_tail_past_eof() {
  let (_dir, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/test.txt", b"hello world", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/dir/nested.txt", b"nested", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }

  poison_hot_tail_offset_past_eof(&path);

  // Low-level repair should recover the exact terminal hot-tail boundary.
  let report = repair_header_in_place(&path).unwrap();
  assert!(report.repaired);
  assert!(report.hot_tail_past_eof.is_some());

  // Now StorageEngine::open should succeed and recover the files
  let engine = StorageEngine::open(&path).unwrap();
  let ops = DirectoryOps::new(&engine);
  let recovered = ops.read_file_buffered("/test.txt").unwrap();
  assert_eq!(recovered, b"hello world");
  let recovered_nested = ops.read_file_buffered("/dir/nested.txt").unwrap();
  assert_eq!(recovered_nested, b"nested");
}

#[test]
fn repair_refuses_to_hide_corrupt_durable_wal_behind_a_recovered_hot_tail_boundary() {
  let (_dir, path) = make_temp_db();
  let key = [0xA4; 32];
  let entry_offset;
  let value_offset;
  {
    let engine = StorageEngine::create(&path).unwrap();
    entry_offset = engine.store_entry(aeordb::engine::EntryType::Chunk, &key, b"durable payload").unwrap();
    let header = engine.read_entry_header_at(entry_offset).unwrap();
    value_offset = entry_offset + header.header_size() as u64 + header.key_length as u64;
    engine.shutdown().unwrap();
  }

  {
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(value_offset)).unwrap();
    file.write_all(&[0xFF]).unwrap();
    file.sync_all().unwrap();
  }
  poison_hot_tail_offset_past_eof(&path);

  let error = repair_header_in_place(&path).expect_err("repair must verify the durable WAL before selecting a recovered hot-tail boundary");
  assert!(matches!(error, aeordb::engine::EngineError::CorruptEntry { offset, .. } if offset == entry_offset));
}

#[test]
fn repair_discards_only_a_truncated_terminal_wal_entry() {
  let (_directory, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let operations = DirectoryOps::new(&engine);
    let context = RequestContext::system();
    for index in 0..40 {
      operations
        .store_file_buffered(
          &context,
          &format!("/truncated/data-{index:04}.txt"),
          format!("payload-{index:04}").as_bytes(),
          Some("text/plain"),
        )
        .unwrap();
    }
    engine.shutdown().unwrap();
  }

  let original_header = {
    let mut file = OpenOptions::new().read(true).open(&path).unwrap();
    read_active_header(&mut file).unwrap().0
  };
  let truncated_length = original_header.hot_tail_offset.checked_sub(8).expect("fixture WAL is longer than eight bytes");
  {
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(truncated_length).unwrap();
    file.sync_all().unwrap();
  }

  let before_rejected_open = std::fs::read(&path).unwrap();
  assert!(StorageEngine::open(&path).is_err(), "selected header must reject a WAL frontier beyond physical EOF");
  assert_eq!(
    std::fs::read(&path).unwrap(),
    before_rejected_open,
    "rejecting a selected WAL frontier beyond EOF must not publish partially initialized engine state"
  );
  let report = repair_header_in_place(&path).expect("a terminal partial WAL entry is recoverable by discarding only that entry");
  assert!(report.repaired);
  assert!(report.hot_tail_past_eof.is_some());

  let repaired_header = {
    let mut file = OpenOptions::new().read(true).open(&path).unwrap();
    read_active_header(&mut file).unwrap().0
  };
  assert!(repaired_header.hot_tail_offset < truncated_length, "repair must select the start of the partial terminal entry");

  let reopened = StorageEngine::open(&path).unwrap();
  let operations = DirectoryOps::new(&reopened);
  let surviving = (0..39).filter(|index| operations.read_file_buffered(&format!("/truncated/data-{index:04}.txt")).is_ok()).count();
  assert!(surviving >= 35, "repair should preserve the verified WAL prefix; only {surviving} earlier files survived");
}

#[test]
fn header_crc_catches_single_byte_corruption_in_one_slot() {
  // A/B double-buffer: corrupting ONE slot is recoverable — the engine
  // reads the other slot. inspect_header (which reads slot A only) reports
  // the CRC fail; open succeeds via slot B (which holds the most recent
  // header after any update).
  let (_dir, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }

  // Flip a byte in slot A only (offset 50). Slot B is at 256-511 and untouched.
  {
    let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(50)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(50)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
  }

  let report = inspect_header(&path).unwrap();
  assert!(report.crc_failed, "byte flip in slot A should fail its CRC");

  // Open should SUCCEED because slot B is still valid — that's the entire
  // point of A/B double-buffering.
  let result = StorageEngine::open(&path);
  assert!(result.is_ok(), "open should fall back to slot B");
  let engine = result.unwrap();
  let ops = DirectoryOps::new(&engine);
  assert_eq!(ops.read_file_buffered("/test.txt").unwrap(), b"hello");
}

#[test]
fn repair_recovers_checksum_failed_v3_slot_from_redundant_slot() {
  let (_directory, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let operations = DirectoryOps::new(&engine);
    operations.store_file_buffered(&RequestContext::system(), "/survives.txt", b"redundant header", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }

  {
    let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(50)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(50)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
  }
  let before_repair = std::fs::read(&path).unwrap();
  let surviving_slot = before_repair[FILE_HEADER_SIZE..FILE_HEADER_SIZE * 2].to_vec();

  let report = repair_header_in_place(&path).unwrap();

  assert!(report.crc_failed);
  assert!(report.repaired);
  let after_repair = std::fs::read(&path).unwrap();
  assert_eq!(&after_repair[FILE_HEADER_SIZE..FILE_HEADER_SIZE * 2], surviving_slot.as_slice());
  let reopened = StorageEngine::open(&path).unwrap();
  assert_eq!(DirectoryOps::new(&reopened).read_file_buffered("/survives.txt").unwrap(), b"redundant header");
  reopened.shutdown().unwrap();
}

#[test]
fn corrupting_both_slots_refuses_open() {
  // The A/B fallback only protects against single-slot torn writes.
  // Corrupting BOTH slots leaves the engine with no valid header to read.
  let (_dir, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }

  // Flip a byte in slot A AND in slot B.
  {
    let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    for offset in [50u64, 256 + 50] {
      file.seek(SeekFrom::Start(offset)).unwrap();
      let mut byte = [0u8; 1];
      file.read_exact(&mut byte).unwrap();
      byte[0] ^= 0xFF;
      file.seek(SeekFrom::Start(offset)).unwrap();
      file.write_all(&byte).unwrap();
    }
    file.sync_all().unwrap();
  }

  let result = StorageEngine::open(&path);
  assert!(result.is_err(), "open should refuse when both slots fail CRC");
}

#[test]
fn repair_writes_v2_header_with_valid_crc() {
  let (_dir, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/x.txt", b"x", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }
  poison_hot_tail_offset_past_eof(&path);

  let repaired = repair_header_in_place(&path).unwrap();
  assert!(repaired.repaired);

  // Re-inspect — should now be already_ok
  let post = inspect_header(&path).unwrap();
  assert!(post.already_ok, "post-repair header should be clean, got {:?}", post);
}

#[test]
fn inspect_reports_already_ok_on_clean_db() {
  let (_dir, path) = make_temp_db();
  {
    let engine = StorageEngine::create(&path).unwrap();
    engine.shutdown().unwrap();
  }

  let report = inspect_header(&path).unwrap();
  assert!(report.already_ok);
  assert!(report.hot_tail_past_eof.is_none());
  assert!(report.upgraded_version.is_none());
  assert!(!report.crc_failed);
}

#[test]
fn legacy_header_repair_streams_multiple_bounded_copy_windows_exactly() {
  let (_dir, path) = make_temp_db();
  let payload: Vec<u8> = (0..(256 * 1024 * 3 + 17)).map(|index| (index % 251) as u8).collect();
  write_v2_fixture(&path, &payload);

  let report = repair_header_in_place(&path).unwrap();
  assert!(report.repaired);
  assert_eq!(report.upgraded_version, Some((2, 3)));

  let mut file = OpenOptions::new().read(true).open(&path).unwrap();
  let (header, _) = read_active_header(&mut file).unwrap();
  assert_eq!(header.kv_block_offset, (FILE_HEADER_SIZE * 2) as u64);
  assert_eq!(header.hot_tail_offset, (FILE_HEADER_SIZE * 2 + payload.len()) as u64);
  assert_eq!(file.metadata().unwrap().len(), (FILE_HEADER_SIZE * 2 + payload.len()) as u64);
  file.seek(SeekFrom::Start((FILE_HEADER_SIZE * 2) as u64)).unwrap();
  let mut migrated = Vec::new();
  file.read_to_end(&mut migrated).unwrap();
  assert_eq!(migrated, payload);
}

#[test]
fn repaired_v2_sidecar_layout_bootstraps_in_file_kv_without_losing_wal() {
  let (dir, source_path) = make_temp_db();
  let legacy_path = dir.path().join("legacy-v2.aeordb").to_string_lossy().to_string();
  {
    let engine = StorageEngine::create(&source_path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/legacy/state.json", br#"{"from":"v2-sidecar","exact":true}"#, Some("application/json")).unwrap();
    ops.store_file_buffered(&ctx, "/legacy/readme.txt", b"sidecar WAL survived", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();
  }

  let mut source = OpenOptions::new().read(true).open(&source_path).unwrap();
  let (source_header, _) = read_active_header(&mut source).unwrap();
  let wal_start = source_header.kv_block_offset + source_header.kv_block_length;
  let wal_length = source_header.hot_tail_offset - wal_start;
  let mut wal = vec![0u8; wal_length as usize];
  source.seek(SeekFrom::Start(wal_start)).unwrap();
  source.read_exact(&mut wal).unwrap();
  write_v2_no_kv_fixture(&legacy_path, &source_header, &wal);

  assert!(StorageEngine::open(&legacy_path).is_err(), "legacy headers require the explicit repair gate");
  let repair = repair_header_in_place(&legacy_path).unwrap();
  assert_eq!(repair.upgraded_version, Some((2, 3)));

  let engine = StorageEngine::open(&legacy_path).unwrap();
  let ops = DirectoryOps::new(&engine);
  assert_eq!(ops.read_file_buffered("/legacy/state.json").unwrap(), br#"{"from":"v2-sidecar","exact":true}"#);
  assert_eq!(ops.read_file_buffered("/legacy/readme.txt").unwrap(), b"sidecar WAL survived");
  engine.shutdown().unwrap();
  drop(engine);

  let reopened = StorageEngine::open(&legacy_path).unwrap();
  assert_eq!(DirectoryOps::new(&reopened).read_file_buffered("/legacy/readme.txt").unwrap(), b"sidecar WAL survived");
  assert!(!aeordb::engine::verify::verify_checked(&reopened, &legacy_path).unwrap().has_issues());
}
