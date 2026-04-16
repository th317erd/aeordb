use std::io::{Seek, SeekFrom, Write};

use aeordb::engine::append_writer::AppendWriter;
use aeordb::engine::entry_header::CURRENT_ENTRY_VERSION;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::file_header::FILE_HEADER_SIZE;
use aeordb::engine::hash_algorithm::HashAlgorithm;

fn create_temp_path() -> tempfile::TempDir {
  tempfile::tempdir().expect("Failed to create temp dir")
}

#[test]
fn test_create_new_file_writes_header() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  let writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let header = writer.file_header();
  assert_eq!(header.header_version, 1);
  assert_eq!(header.hash_algo, HashAlgorithm::Blake3_256);
  assert_eq!(header.entry_count, 0);
  assert!(!header.resize_in_progress);

  // File should exist and be at least 256 bytes
  let metadata = std::fs::metadata(&file_path).expect("Failed to read metadata");
  assert_eq!(metadata.len(), FILE_HEADER_SIZE as u64);
}

#[test]
fn test_open_existing_file_reads_header() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  {
    let _writer = AppendWriter::create(&file_path)
      .expect("Failed to create file");
  }

  let writer = AppendWriter::open(&file_path)
    .expect("Failed to open file");

  let header = writer.file_header();
  assert_eq!(header.header_version, 1);
  assert_eq!(header.hash_algo, HashAlgorithm::Blake3_256);
}

#[test]
fn test_append_entry_returns_offset() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let offset = writer
    .append_entry(EntryType::Chunk, b"key1", b"value1", 0)
    .expect("Failed to append entry");

  // First entry should start right after the file header
  assert_eq!(offset, FILE_HEADER_SIZE as u64);
}

#[test]
fn test_append_and_read_back_roundtrip() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let key = b"my-key";
  let value = b"my-value-data";
  let offset = writer
    .append_entry(EntryType::FileRecord, key, value, 0x42)
    .expect("Failed to append entry");

  let (header, read_key, read_value) = writer
    .read_entry_at(offset)
    .expect("Failed to read entry");

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
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let offset1 = writer
    .append_entry(EntryType::Chunk, b"key1", b"value1", 0)
    .expect("Failed to append entry 1");
  let offset2 = writer
    .append_entry(EntryType::Chunk, b"key2", b"value2", 0)
    .expect("Failed to append entry 2");
  let offset3 = writer
    .append_entry(EntryType::FileRecord, b"key3", b"value3", 0)
    .expect("Failed to append entry 3");

  // Offsets should be strictly increasing
  assert!(offset2 > offset1);
  assert!(offset3 > offset2);

  // Entry count should be 3
  assert_eq!(writer.file_header().entry_count, 3);

  // Read back each entry
  let (_, key1, value1) = writer.read_entry_at(offset1).expect("Failed to read entry 1");
  assert_eq!(key1, b"key1");
  assert_eq!(value1, b"value1");

  let (_, key2, value2) = writer.read_entry_at(offset2).expect("Failed to read entry 2");
  assert_eq!(key2, b"key2");
  assert_eq!(value2, b"value2");

  let (header3, key3, value3) = writer.read_entry_at(offset3).expect("Failed to read entry 3");
  assert_eq!(key3, b"key3");
  assert_eq!(value3, b"value3");
  assert_eq!(header3.entry_type, EntryType::FileRecord);
}

#[test]
fn test_scan_entries_iterates_all() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  writer.append_entry(EntryType::Chunk, b"k1", b"v1", 0).unwrap();
  writer.append_entry(EntryType::FileRecord, b"k2", b"v2", 0).unwrap();
  writer.append_entry(EntryType::Chunk, b"k3", b"v3", 0).unwrap();

  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>()
    .expect("Failed to scan entries");

  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0].key, b"k1");
  assert_eq!(entries[0].value, b"v1");
  assert_eq!(entries[0].header.entry_type, EntryType::Chunk);
  assert_eq!(entries[1].key, b"k2");
  assert_eq!(entries[1].header.entry_type, EntryType::FileRecord);
  assert_eq!(entries[2].key, b"k3");
}

#[test]
fn test_scan_skips_corrupt_entries() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let _offset1 = writer.append_entry(EntryType::Chunk, b"good1", b"val1", 0).unwrap();
  let offset2 = writer.append_entry(EntryType::Chunk, b"corrupt", b"will-be-bad", 0).unwrap();
  let _offset3 = writer.append_entry(EntryType::Chunk, b"good2", b"val2", 0).unwrap();

  // Read entry 2's header to get its size, then corrupt the value portion
  let (header2, _, _) = writer.read_entry_at(offset2).unwrap();
  let value_offset = offset2 + header2.header_size() as u64 + header2.key_length as u64;

  // Directly corrupt the file at the value offset
  {
    let mut file = std::fs::OpenOptions::new()
      .write(true)
      .open(&file_path)
      .unwrap();
    file.seek(SeekFrom::Start(value_offset)).unwrap();
    file.write_all(b"CORRUPTED!!").unwrap();
    file.sync_all().unwrap();
  }

  // Re-open and scan — the corrupt entry should be skipped
  let writer = AppendWriter::open(&file_path).expect("Failed to open file");
  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>()
    .expect("Failed to scan entries");

  // Should have 2 valid entries (corrupt one skipped)
  assert_eq!(entries.len(), 2);
  assert_eq!(entries[0].key, b"good1");
  assert_eq!(entries[1].key, b"good2");
}

#[test]
fn test_write_void_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  // Minimum size = 31 (fixed header) + 32 (blake3 hash) = 63
  let void_size: u32 = 100;
  let offset = writer.write_void(void_size)
    .expect("Failed to write void");

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
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  // Size too small for a header
  let result = writer.write_void(10);
  assert!(result.is_err());
}

#[test]
fn test_file_header_update() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let mut updated_header = writer.file_header().clone();
  updated_header.entry_count = 999;
  updated_header.kv_block_offset = 4096;
  updated_header.resize_in_progress = true;

  writer.update_file_header(&updated_header)
    .expect("Failed to update header");

  assert_eq!(writer.file_header().entry_count, 999);
  assert_eq!(writer.file_header().kv_block_offset, 4096);
  assert!(writer.file_header().resize_in_progress);

  // Re-open and verify persistence
  drop(writer);
  let reopened = AppendWriter::open(&file_path)
    .expect("Failed to reopen file");
  assert_eq!(reopened.file_header().entry_count, 999);
  assert_eq!(reopened.file_header().kv_block_offset, 4096);
  assert!(reopened.file_header().resize_in_progress);
}

#[test]
fn test_entry_at_offset() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let _offset1 = writer.append_entry(EntryType::Chunk, b"first", b"data1", 0).unwrap();
  let offset2 = writer.append_entry(EntryType::FileRecord, b"second", b"data2", 0).unwrap();
  let _offset3 = writer.append_entry(EntryType::Chunk, b"third", b"data3", 0).unwrap();

  // Read specifically entry 2
  let (header, key, value) = writer.read_entry_at(offset2)
    .expect("Failed to read entry at offset");
  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(key, b"second");
  assert_eq!(value, b"data2");
}

#[test]
fn test_append_chunk_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let chunk_data = b"This is raw chunk data for a file.";
  let chunk_key = blake3::hash(chunk_data).as_bytes().to_vec();

  let offset = writer
    .append_entry(EntryType::Chunk, &chunk_key, chunk_data, 0)
    .expect("Failed to append chunk");

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
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let file_key = b"file:/documents/readme.txt";
  let file_metadata = b"{\"content_type\":\"text/plain\",\"size\":1024}";

  let offset = writer
    .append_entry(EntryType::FileRecord, file_key, file_metadata, 0)
    .expect("Failed to append file record");

  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(header.entry_type, EntryType::FileRecord);
  assert_eq!(key, file_key);
  assert_eq!(value, file_metadata);
}

#[test]
fn test_empty_key_empty_value_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let offset = writer
    .append_entry(EntryType::Snapshot, b"", b"", 0)
    .expect("Failed to append empty entry");

  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(header.entry_type, EntryType::Snapshot);
  assert_eq!(header.key_length, 0);
  assert_eq!(header.value_length, 0);
  assert!(key.is_empty());
  assert!(value.is_empty());
  assert!(header.verify(&key, &value));
}

#[test]
fn test_large_value_entry() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let key = b"large-chunk";
  // 1 MB of data
  let large_value = vec![0xAB; 1024 * 1024];

  let offset = writer
    .append_entry(EntryType::Chunk, key, &large_value, 0)
    .expect("Failed to append large entry");

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

  let _writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");
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
  {
    let mut writer = AppendWriter::create(&file_path)
      .expect("Failed to create file");
    offset = writer
      .append_entry(EntryType::Chunk, b"persist-key", b"persist-value", 0)
      .expect("Failed to append entry");
  }

  // Reopen and read
  let mut writer = AppendWriter::open(&file_path)
    .expect("Failed to reopen file");
  let (header, key, value) = writer.read_entry_at(offset).unwrap();
  assert_eq!(key, b"persist-key");
  assert_eq!(value, b"persist-value");
  assert!(header.verify(&key, &value));
}

#[test]
fn test_append_after_reopen_continues_at_end() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");

  let offset1;
  {
    let mut writer = AppendWriter::create(&file_path)
      .expect("Failed to create file");
    offset1 = writer
      .append_entry(EntryType::Chunk, b"first", b"data1", 0)
      .expect("Failed to append entry");
  }

  let offset2;
  {
    let mut writer = AppendWriter::open(&file_path)
      .expect("Failed to reopen file");
    offset2 = writer
      .append_entry(EntryType::Chunk, b"second", b"data2", 0)
      .expect("Failed to append entry");
  }

  assert!(offset2 > offset1);

  // Verify both entries
  let mut writer = AppendWriter::open(&file_path)
    .expect("Failed to reopen file");

  let (_, key1, _) = writer.read_entry_at(offset1).unwrap();
  assert_eq!(key1, b"first");

  let (_, key2, _) = writer.read_entry_at(offset2).unwrap();
  assert_eq!(key2, b"second");
}

#[test]
fn test_scan_empty_file_returns_no_entries() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>()
    .expect("Failed to scan entries");

  assert!(entries.is_empty());
}

#[test]
fn test_read_entry_at_invalid_offset() {
  let temp_directory = create_temp_path();
  let file_path = temp_directory.path().join("test.aeor");
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

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
  let mut writer = AppendWriter::create(&file_path)
    .expect("Failed to create file");

  let offset1 = writer.append_entry(EntryType::Chunk, b"k1", b"v1", 0).unwrap();
  let void_offset = writer.write_void(100).unwrap();
  let offset2 = writer.append_entry(EntryType::Chunk, b"k2", b"v2", 0).unwrap();

  assert!(void_offset > offset1);
  assert!(offset2 > void_offset);

  // Scan should return all 3 (void is a valid entry type)
  let scanner = writer.scan_entries().expect("Failed to create scanner");
  let entries: Vec<_> = scanner.collect::<Result<Vec<_>, _>>()
    .expect("Failed to scan entries");

  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0].header.entry_type, EntryType::Chunk);
  assert_eq!(entries[1].header.entry_type, EntryType::Void);
  assert_eq!(entries[2].header.entry_type, EntryType::Chunk);
}
