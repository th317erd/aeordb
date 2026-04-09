use aeordb::engine::{
  DirectoryOps, EntryType, RequestContext,
};
use aeordb::server::create_temp_engine_for_tests;

// ─── In-place write infrastructure ──────────────────────────────────────────

#[test]
fn test_write_entry_at_roundtrip() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Store a dummy chunk entry we'll later overwrite
  let dummy_key = engine.compute_hash(b"dummy:overwrite-test").unwrap();
  let dummy_value = vec![0u8; 200];
  let offset = engine.store_entry(EntryType::Chunk, &dummy_key, &dummy_value).unwrap();

  // Read back the entry to get its total_length
  let header = engine.read_entry_header_at(offset).unwrap();
  let original_size = header.total_length;
  assert!(original_size > 0);

  // Write a DeletionRecord in-place at that offset
  let written = engine.write_deletion_at(offset, "gc:test").unwrap();
  assert!(written > 0);
  assert!(written <= original_size);
}

#[test]
fn test_write_void_at_creates_valid_void() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Store a large-ish dummy entry to get an offset
  let dummy_key = engine.compute_hash(b"dummy:void-test").unwrap();
  let dummy_value = vec![0u8; 500];
  let offset = engine.store_entry(EntryType::Chunk, &dummy_key, &dummy_value).unwrap();
  let header = engine.read_entry_header_at(offset).unwrap();
  let entry_size = header.total_length;

  // Compute how much space a deletion takes
  let deletion_size = engine.write_deletion_at(offset, "gc:void-test").unwrap();

  // Write a void in the remaining space
  let remaining = entry_size - deletion_size;
  let void_offset = offset + deletion_size as u64;
  engine.write_void_at(void_offset, remaining).unwrap();

  // Verify the void is registered
  let stats = engine.stats();
  assert!(stats.void_count > 0, "void should be registered");
  assert!(stats.void_space_bytes >= remaining as u64);
}

#[test]
fn test_write_void_at_rejects_too_small() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Try to write a void that's smaller than minimum entry size (63 bytes for Blake3)
  let result = engine.write_void_at(100, 10);
  assert!(result.is_err(), "should reject void smaller than minimum entry size");
}

#[test]
fn test_remove_kv_entry() {
  let (engine, _temp) = create_temp_engine_for_tests();

  let key = engine.compute_hash(b"dummy:remove-test").unwrap();
  let value = vec![0u8; 100];
  engine.store_entry(EntryType::Chunk, &key, &value).unwrap();

  // Entry should be findable
  assert!(engine.has_entry(&key).unwrap());

  // Remove it
  engine.remove_kv_entry(&key).unwrap();

  // Entry should no longer be findable
  assert!(!engine.has_entry(&key).unwrap());
}

#[test]
fn test_iter_kv_entries_returns_live_entries() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

  let entries = engine.iter_kv_entries().unwrap();
  assert!(!entries.is_empty(), "should have KV entries after storing a file");
}
