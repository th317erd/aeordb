use std::io::{Read, Seek, SeekFrom, Write};

use aeordb::engine::append_writer::AppendWriter;
use aeordb::engine::durability_coordinator::{DurabilityCoordinator, DurabilityOperation};
use aeordb::engine::entry_header::CURRENT_ENTRY_VERSION;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::file_header::{
  FILE_HEADER_SIZE, FileHeader, HEADER_REGION_SIZE, read_active_header, write_header_to_inactive_slot_coordinated, write_initial_header,
};
use aeordb::engine::hash_algorithm::HashAlgorithm;

fn create_temp_path() -> tempfile::TempDir {
  tempfile::tempdir().expect("Failed to create temp dir")
}

fn test_key(seed: u8) -> [u8; 32] {
  [seed; 32]
}

#[test]
fn test_create_new_file_writes_header() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  let writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let header = writer.file_header();
  assert_eq!(header.header_version, 3);
  assert_eq!(header.hash_algo, HashAlgorithm::Blake3_256);
  assert_eq!(header.entry_count, 0);
  assert!(!header.resize_in_progress);

  // File should exist and be the size of the header region (both slots)
  let metadata = std::fs::metadata(&file_path).expect("Failed to read metadata");
  assert_eq!(metadata.len(), HEADER_REGION_SIZE as u64);
  // FILE_HEADER_SIZE is the slot size; verify it's half the region.
  assert_eq!(FILE_HEADER_SIZE * 2, HEADER_REGION_SIZE);

  let durability = writer.durability_snapshot().unwrap();
  assert_eq!(durability.hard_frontier, 1);
  assert_eq!(durability.ledger.last().unwrap().operation, DurabilityOperation::AuthorityReadback);
}

#[test]
fn test_open_existing_file_reads_header() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  {
    let _writer = AppendWriter::create(&file_path).expect("Failed to create file");
  }

  let writer = AppendWriter::open(&file_path).expect("Failed to open file");

  let header = writer.file_header();
  assert_eq!(header.header_version, 3);
  assert_eq!(header.hash_algo, HashAlgorithm::Blake3_256);
}

#[test]
fn test_append_entry_returns_offset() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let (offset, _) = writer.append_entry(EntryType::Chunk, &test_key(1), b"value1", 0).expect("Failed to append entry");

  // First entry should start right after the header region (both A/B slots)
  assert_eq!(offset, HEADER_REGION_SIZE as u64);
}

#[test]
fn entry_writers_reject_malformed_key_width_before_mutation() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("key-width.aeor");
  let mut writer = AppendWriter::create(&file_path).unwrap();
  let initial_offset = writer.current_offset();
  let initial_entry_count = writer.file_header().entry_count;
  let initial_bytes = std::fs::read(&file_path).unwrap();

  let append_error = writer.append_entry(EntryType::Chunk, b"short", b"payload", 0).unwrap_err();
  assert!(append_error.to_string().contains("key length"));
  assert_eq!(writer.current_offset(), initial_offset);
  assert_eq!(writer.file_header().entry_count, initial_entry_count);
  assert_eq!(std::fs::read(&file_path).unwrap(), initial_bytes);

  let overwrite_error = writer.write_entry_at_nosync(initial_offset, EntryType::FileRecord, b"short", b"payload").unwrap_err();
  assert!(overwrite_error.to_string().contains("key length"));
  assert_eq!(writer.current_offset(), initial_offset);
  assert_eq!(writer.file_header().entry_count, initial_entry_count);
  assert_eq!(std::fs::read(&file_path).unwrap(), initial_bytes);

  let void_error = writer.append_entry(EntryType::Void, &[0xA5; 32], b"payload", 0).unwrap_err();
  assert!(void_error.to_string().contains("key length"));
  assert_eq!(writer.current_offset(), initial_offset);
  assert_eq!(writer.file_header().entry_count, initial_entry_count);
  assert_eq!(std::fs::read(&file_path).unwrap(), initial_bytes);
}

#[test]
fn test_append_and_read_back_roundtrip() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let key = test_key(2);
  let value = b"my-value-data";
  let (offset, _) = writer.append_entry(EntryType::FileRecord, &key, value, 0x42).expect("Failed to append entry");

  let (header, read_key, read_value) = writer.read_entry_at(offset).expect("Failed to read entry");

  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(header.flags, 0x42);
  assert_eq!(header.entry_version, CURRENT_ENTRY_VERSION);
  assert_eq!(read_key, key);
  assert_eq!(read_value, value);
  assert!(header.verify(&read_key, &read_value));
}

#[test]
fn test_append_multiple_entries() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let keys = [test_key(1), test_key(2), test_key(3)];
  let (offset1, _) = writer.append_entry(EntryType::Chunk, &keys[0], b"value1", 0).expect("Failed to append entry 1");
  let (offset2, _) = writer.append_entry(EntryType::Chunk, &keys[1], b"value2", 0).expect("Failed to append entry 2");
  let (offset3, _) = writer.append_entry(EntryType::FileRecord, &keys[2], b"value3", 0).expect("Failed to append entry 3");

  // Offsets should be strictly increasing
  assert!(offset2 > offset1);
  assert!(offset3 > offset2);

  // Entry count should be 3
  assert_eq!(writer.file_header().entry_count, 3);

  // Read back each entry
  let (_, key1, value1) = writer.read_entry_at(offset1).expect("Failed to read entry 1");
  assert_eq!(key1, keys[0]);
  assert_eq!(value1, b"value1");

  let (_, key2, value2) = writer.read_entry_at(offset2).expect("Failed to read entry 2");
  assert_eq!(key2, keys[1]);
  assert_eq!(value2, b"value2");

  let (header3, key3, value3) = writer.read_entry_at(offset3).expect("Failed to read entry 3");
  assert_eq!(key3, keys[2]);
  assert_eq!(value3, b"value3");
  assert_eq!(header3.entry_type, EntryType::FileRecord);
}

#[test]
fn test_scan_entries_iterates_all() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let keys = [test_key(1), test_key(2), test_key(3)];
  writer.append_entry(EntryType::Chunk, &keys[0], b"v1", 0).unwrap();
  writer.append_entry(EntryType::FileRecord, &keys[1], b"v2", 0).unwrap();
  writer.append_entry(EntryType::Chunk, &keys[2], b"v3", 0).unwrap();

  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>().expect("Failed to scan entries");

  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0].key, keys[0]);
  assert_eq!(entries[0].value, b"v1");
  assert_eq!(entries[0].header.entry_type, EntryType::Chunk);
  assert_eq!(entries[1].key, keys[1]);
  assert_eq!(entries[1].header.entry_type, EntryType::FileRecord);
  assert_eq!(entries[2].key, keys[2]);
}

#[test]
fn test_scan_skips_corrupt_entries() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let keys = [test_key(1), test_key(2), test_key(3)];
  let (_offset1, _) = writer.append_entry(EntryType::Chunk, &keys[0], b"val1", 0).unwrap();
  let (offset2, _) = writer.append_entry(EntryType::Chunk, &keys[1], b"will-be-bad", 0).unwrap();
  let (_offset3, _) = writer.append_entry(EntryType::Chunk, &keys[2], b"val2", 0).unwrap();

  // Read entry 2's header to get its size, then corrupt the value portion
  let (header2, _, _) = writer.read_entry_at(offset2).unwrap();
  let value_offset = offset2 + header2.header_size() as u64 + header2.key_length as u64;

  // Directly corrupt the file at the value offset
  {
    let mut file = std::fs::OpenOptions::new().write(true).open(&file_path).unwrap();
    file.seek(SeekFrom::Start(value_offset)).unwrap();
    file.write_all(b"CORRUPTED!!").unwrap();
    file.sync_all().unwrap();
  }

  // Re-open and scan — the corrupt entry should be skipped
  let writer = AppendWriter::open(&file_path).expect("Failed to open file");
  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>().expect("Failed to scan entries");

  // Should have 2 valid entries (corrupt one skipped)
  assert_eq!(entries.len(), 2);
  assert_eq!(entries[0].key, keys[0]);
  assert_eq!(entries[1].key, keys[2]);
}

#[test]
fn test_write_void_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  // Minimum size = 31 (fixed header) + 32 (blake3 hash) = 63
  let void_size: u32 = 100;
  let (offset, _) = writer.write_void(void_size).expect("Failed to write void");

  let (header, key, value) = writer.read_entry_at(offset).expect("Failed to read void");
  assert_eq!(header.entry_type, EntryType::Void);
  assert_eq!(key.len(), 0);
  assert_eq!(header.total_length, void_size);
  assert_eq!(value.len(), (void_size as usize) - 31 - 32); // total - fixed - hash
}

#[test]
fn test_write_void_too_small() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  // Size too small for a header
  let result = writer.write_void(10);
  assert!(result.is_err());
}

#[test]
fn test_file_header_update() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let mut updated_header = writer.file_header().clone();
  updated_header.entry_count = 999;
  updated_header.kv_block_offset = 4096;
  updated_header.resize_in_progress = true;

  writer.update_file_header(&updated_header).expect("Failed to update header");

  assert_eq!(writer.file_header().entry_count, 999);
  assert_eq!(writer.file_header().kv_block_offset, 4096);
  assert!(writer.file_header().resize_in_progress);

  // Re-open and verify persistence
  drop(writer);
  let reopened = AppendWriter::open(&file_path).expect("Failed to reopen file");
  assert_eq!(reopened.file_header().entry_count, 999);
  assert_eq!(reopened.file_header().kv_block_offset, 4096);
  assert!(reopened.file_header().resize_in_progress);
}

#[test]
fn coordinated_header_publication_preserves_v3_bytes_and_records_readback() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("coordinated-header.aeor");
  let mut writer = AppendWriter::create(&file_path).unwrap();
  let old_sequence = writer.file_header().sequence;
  let mut updated = writer.file_header().clone();
  updated.entry_count = 77;

  writer.update_header(&updated).unwrap();
  let snapshot = writer.durability_snapshot().unwrap();
  assert_eq!(snapshot.hard_frontier, 2, "initial creation and the update must each publish hard authority");
  assert_eq!(snapshot.failed, 0);
  assert_eq!(snapshot.ledger.last().unwrap().operation, DurabilityOperation::AuthorityReadback);
  assert!(snapshot.ledger.last().unwrap().succeeded);

  drop(writer);
  let mut file = std::fs::OpenOptions::new().read(true).open(&file_path).unwrap();
  let (active, slot) = read_active_header(&mut file).unwrap();
  assert_eq!(slot, 1);
  assert_eq!(active.sequence, old_sequence + 1);
  assert_eq!(active.entry_count, 77);
  let mut bytes = vec![0u8; FILE_HEADER_SIZE];
  file.seek(SeekFrom::Start(FILE_HEADER_SIZE as u64)).unwrap();
  file.read_exact(&mut bytes).unwrap();
  assert_eq!(bytes, active.serialize());
}

#[test]
fn coordinated_header_publication_refuses_write_failure_without_touching_inactive_slot() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("read-only-header.aeor");
  let mut initial = FileHeader::new(HashAlgorithm::Blake3_256);
  {
    let mut file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&file_path).unwrap();
    write_initial_header(&mut file, &mut initial).unwrap();
  }

  let coordinator = DurabilityCoordinator::new();
  let mut read_only = std::fs::OpenOptions::new().read(true).open(&file_path).unwrap();
  let mut replacement = initial.clone();
  replacement.entry_count = 99;
  assert!(write_header_to_inactive_slot_coordinated(&mut read_only, &mut replacement, 0, &coordinator).is_err());

  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.hard_frontier, 0);
  assert_eq!(snapshot.failed, 1);
  assert_eq!(snapshot.ledger.last().unwrap().operation, DurabilityOperation::AuthorityWrite);
  assert!(!snapshot.ledger.last().unwrap().succeeded);
  let bytes = std::fs::read(&file_path).unwrap();
  assert!(bytes[FILE_HEADER_SIZE..HEADER_REGION_SIZE].iter().all(|byte| *byte == 0));
}

#[test]
fn test_entry_at_offset() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let keys = [test_key(1), test_key(2), test_key(3)];
  let (_offset1, _) = writer.append_entry(EntryType::Chunk, &keys[0], b"data1", 0).unwrap();
  let (offset2, _) = writer.append_entry(EntryType::FileRecord, &keys[1], b"data2", 0).unwrap();
  let (_offset3, _) = writer.append_entry(EntryType::Chunk, &keys[2], b"data3", 0).unwrap();

  // Read specifically entry 2
  let (header, key, value) = writer.read_entry_at(offset2).expect("Failed to read entry at offset");
  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(key, keys[1]);
  assert_eq!(value, b"data2");
}

#[test]
fn test_append_chunk_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let chunk_data = b"This is raw chunk data for a file.";
  let chunk_key = blake3::hash(chunk_data).as_bytes().to_vec();

  let (offset, _) = writer.append_entry(EntryType::Chunk, &chunk_key, chunk_data, 0).expect("Failed to append chunk");

  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(header.entry_type, EntryType::Chunk);
  assert_eq!(key, chunk_key);
  assert_eq!(value, chunk_data);
  assert!(header.verify(&key, &value));
}

#[test]
fn test_append_file_record_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let file_key = test_key(4);
  let file_metadata = b"{\"content_type\":\"text/plain\",\"size\":1024}";

  let (offset, _) = writer.append_entry(EntryType::FileRecord, &file_key, file_metadata, 0).expect("Failed to append file record");

  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(key, file_key);
  assert_eq!(value, file_metadata);
}

#[test]
fn test_empty_value_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let key = test_key(5);
  let (offset, _) = writer.append_entry(EntryType::Snapshot, &key, b"", 0).expect("Failed to append empty value entry");

  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(header.entry_type, EntryType::Snapshot);
  assert_eq!(header.key_length, 32);
  assert_eq!(header.value_length, 0);
  assert_eq!(key, test_key(5));
  assert!(value.is_empty());
  assert!(header.verify(&key, &value));
}

#[test]
fn test_large_value_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let key = test_key(6);
  // 1 MB of data
  let large_value = vec![0xAB; 1024 * 1024];

  let (offset, _) = writer.append_entry(EntryType::Chunk, &key, &large_value, 0).expect("Failed to append large entry");

  let (header, read_key, read_value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(read_key, key);
  assert_eq!(read_value.len(), 1024 * 1024);
  assert_eq!(read_value, large_value);
  assert!(header.verify(&read_key, &read_value));
}

#[test]
fn test_create_fails_on_existing_file() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  let _writer = AppendWriter::create(&file_path).expect("Failed to create file");
  drop(_writer);

  // Creating again should fail (create_new)
  let result = AppendWriter::create(&file_path);
  assert!(result.is_err());
}

#[test]
fn test_open_nonexistent_file_fails() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("nonexistent.aeor");

  let result = AppendWriter::open(&file_path);
  assert!(result.is_err());
}

#[test]
fn test_reopen_after_writes_preserves_data() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  let offset;
  let expected_key = test_key(7);
  {
    let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");
    let (written_offset, _) = writer.append_entry(EntryType::Chunk, &expected_key, b"persist-value", 0).expect("Failed to append entry");
    offset = written_offset;
  }

  // Reopen and read
  let mut writer = AppendWriter::open(&file_path).expect("Failed to reopen file");
  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(key, expected_key);
  assert_eq!(value, b"persist-value");
  assert!(header.verify(&key, &value));
}

#[test]
fn test_append_after_reopen_continues_at_end() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  let offset1;
  let first_key = test_key(8);
  {
    let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");
    let (written_offset, _) = writer.append_entry(EntryType::Chunk, &first_key, b"data1", 0).expect("Failed to append entry");
    offset1 = written_offset;
  }

  let offset2;
  let second_key = test_key(9);
  {
    let mut writer = AppendWriter::open(&file_path).expect("Failed to reopen file");
    let (written_offset, _) = writer.append_entry(EntryType::Chunk, &second_key, b"data2", 0).expect("Failed to append entry");
    offset2 = written_offset;
  }

  assert!(offset2 > offset1);

  // Verify both entries
  let mut writer = AppendWriter::open(&file_path).expect("Failed to reopen file");

  let (_, key1, _) = writer.read_entry_at(offset1).unwrap();
  assert_eq!(key1, first_key);

  let (_, key2, _) = writer.read_entry_at(offset2).unwrap();
  assert_eq!(key2, second_key);
}

#[test]
fn test_scan_empty_file_returns_no_entries() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>().expect("Failed to scan entries");

  assert!(entries.is_empty());
}

#[test]
fn test_read_entry_at_invalid_offset() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  // Try reading at offset 0 (file header, not an entry) — should fail with invalid magic
  let result = writer.read_entry_at(0);
  assert!(result.is_err());

  // Try reading past end of file
  let result = writer.read_entry_at(99999);
  assert!(result.is_err());
}

#[test]
fn test_void_and_data_entries_interleaved() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path).expect("Failed to create file");

  let (offset1, _) = writer.append_entry(EntryType::Chunk, &test_key(10), b"v1", 0).unwrap();
  let (void_offset, _) = writer.write_void(100).unwrap();
  let (offset2, _) = writer.append_entry(EntryType::Chunk, &test_key(11), b"v2", 0).unwrap();

  assert!(void_offset > offset1);
  assert!(offset2 > void_offset);

  // Scan should return all 3 (void is a valid entry type)
  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>().expect("Failed to scan entries");

  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0].header.entry_type, EntryType::Chunk);
  assert_eq!(entries[1].header.entry_type, EntryType::Void);
  assert_eq!(entries[2].header.entry_type, EntryType::Chunk);
}

#[test]
fn hot_tail_reader_does_not_turn_an_unreadable_offset_into_an_empty_payload() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("strict-hot-tail.aeor");
  let writer = AppendWriter::create(&file_path).unwrap();

  assert!(writer.read_hot_tail_payload(u64::MAX - 1, 32).is_err());
  assert!(writer.read_hot_tail_entries(u64::MAX - 1, 32).is_err());
}
