use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_store::{FieldIndex, IndexEntry, IndexManager};
use aeordb::engine::memory_coordinator::MemoryOwner;
use aeordb::engine::scalar_converter::{HashConverter, PhoneticConverter, StringConverter, TrigramConverter, U64Converter};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

#[test]
fn test_create_empty_index() {
  let converter = Box::new(U64Converter::with_range(0, 1000));
  let index = FieldIndex::new("age".to_string(), converter);

  assert_eq!(index.field_name, "age");
  assert_eq!(index.len(), 0);
  assert!(index.is_empty());
}

#[test]
fn test_scalar_exact_lookup_capability_excludes_tokenizing_indexes() {
  let string_index = FieldIndex::new("name".to_string(), Box::new(StringConverter::new(256)));
  let u64_index = FieldIndex::new("age".to_string(), Box::new(U64Converter::with_range(0, 200)));
  let trigram_index = FieldIndex::new("name".to_string(), Box::new(TrigramConverter));
  let phonetic_index = FieldIndex::new("name".to_string(), Box::new(PhoneticConverter::soundex()));

  assert!(string_index.supports_scalar_exact_lookup());
  assert!(u64_index.supports_scalar_exact_lookup());
  assert!(!trigram_index.supports_scalar_exact_lookup());
  assert!(!phonetic_index.supports_scalar_exact_lookup());
}

#[test]
fn test_insert_and_lookup_exact() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  index.insert(&30u64.to_be_bytes(), hash_a.clone());

  let results = index.lookup_exact(&30u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_a);
}

#[test]
fn test_direct_insert_remove_without_values() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  index.insert(&30u64.to_be_bytes(), hash_a.clone());
  assert_eq!(index.len(), 1);

  index.remove(&hash_a);
  assert!(index.is_empty(), "direct inserts without stored values must still be removable");
}

#[test]
fn test_insert_many_sorted() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  // Insert out of order
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&90u64.to_be_bytes(), vec![0x90; 32]);
  index.insert(&30u64.to_be_bytes(), vec![0x30; 32]);

  assert_eq!(index.len(), 4);

  // Verify sorted order by scalar
  for window in index.entries.windows(2) {
    assert!(window[0].scalar <= window[1].scalar, "Entries not sorted: {} > {}", window[0].scalar, window[1].scalar,);
  }
}

#[test]
fn test_remove_entry() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];
  index.insert(&25u64.to_be_bytes(), hash_a.clone());
  index.insert(&30u64.to_be_bytes(), hash_b.clone());

  assert_eq!(index.len(), 2);

  index.remove(&hash_a);
  assert_eq!(index.len(), 1);
  assert_eq!(index.entries[0].file_hash, hash_b);
}

#[test]
fn test_lookup_range() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&20u64.to_be_bytes(), vec![0x20; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&80u64.to_be_bytes(), vec![0x80; 32]);

  let results = index.lookup_range(&15u64.to_be_bytes(), &55u64.to_be_bytes()).unwrap();

  assert_eq!(results.len(), 2);
  // Should include 20 and 50
  let hashes: Vec<&Vec<u8>> = results.iter().map(|entry| &entry.file_hash).collect();
  assert!(hashes.contains(&&vec![0x20; 32]));
  assert!(hashes.contains(&&vec![0x50; 32]));
}

#[test]
fn test_lookup_gt() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&80u64.to_be_bytes(), vec![0x80; 32]);

  let results = index.lookup_gt(&40u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 2);
}

#[test]
fn test_lookup_lt() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&80u64.to_be_bytes(), vec![0x80; 32]);

  let results = index.lookup_lt(&60u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 2);
}

#[test]
fn test_range_query_on_non_order_preserving_refuses() {
  let converter = Box::new(HashConverter);
  let mut index = FieldIndex::new("hash_field".to_string(), converter);

  index.insert(&[0xAA; 8], vec![0x01; 32]);

  let result = index.lookup_range(&[0x00; 8], &[0xFF; 8]);
  assert!(result.is_err());

  let result = index.lookup_gt(&[0x00; 8]);
  assert!(result.is_err());

  let result = index.lookup_lt(&[0xFF; 8]);
  assert!(result.is_err());
}

#[test]
fn test_serialize_deserialize_roundtrip() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];
  index.insert(&25u64.to_be_bytes(), hash_a.clone());
  index.insert(&50u64.to_be_bytes(), hash_b.clone());

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.field_name, "age");
  assert_eq!(deserialized.len(), 2);
  assert_eq!(deserialized.entries[0].file_hash, hash_a);
  assert_eq!(deserialized.entries[1].file_hash, hash_b);

  // Converter should produce same results
  let original_scalar = index.converter.to_scalar(&25u64.to_be_bytes());
  let deserialized_scalar = deserialized.converter.to_scalar(&25u64.to_be_bytes());
  assert!((original_scalar - deserialized_scalar).abs() < f64::EPSILON);
}

#[test]
fn test_empty_index_lookup_returns_empty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let results = index.lookup_exact(&42u64.to_be_bytes());
  assert!(results.is_empty());

  let results = index.lookup_range(&0u64.to_be_bytes(), &100u64.to_be_bytes()).unwrap();
  assert!(results.is_empty());

  let results = index.lookup_gt(&0u64.to_be_bytes()).unwrap();
  assert!(results.is_empty());

  let results = index.lookup_lt(&100u64.to_be_bytes()).unwrap();
  assert!(results.is_empty());
}

#[test]
fn test_duplicate_scalars_handled() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];

  // Two files with the same age
  index.insert(&30u64.to_be_bytes(), hash_a.clone());
  index.insert(&30u64.to_be_bytes(), hash_b.clone());

  assert_eq!(index.len(), 2);

  let results = index.lookup_exact(&30u64.to_be_bytes());
  assert_eq!(results.len(), 2);

  // Remove one, other remains
  index.remove(&hash_a);
  assert_eq!(index.len(), 1);
  let results = index.lookup_exact(&30u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_b);
}

#[test]
fn test_save_and_load_index_via_engine() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);
  index.insert(&25u64.to_be_bytes(), vec![0xAA; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0xBB; 32]);

  // Save
  index_manager.save_index("/users", &index).unwrap();

  // Load
  let loaded = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(loaded.field_name, "age");
  assert_eq!(loaded.len(), 2);
}

#[test]
fn test_list_indexes() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  // Create two indexes
  let converter_age = Box::new(U64Converter::with_range(0, 200));
  index_manager.create_index("/users", "age", converter_age).unwrap();

  let converter_name = Box::new(StringConverter::new(256));
  index_manager.create_index("/users", "name", converter_name).unwrap();

  let indexes = index_manager.list_indexes("/users").unwrap();
  assert_eq!(indexes.len(), 2);
  // New format: field.strategy
  assert!(indexes.contains(&"age.u64".to_string()));
  assert!(indexes.contains(&"name.string".to_string()));
}

// --- Additional edge case / failure tests ---

#[test]
fn test_load_nonexistent_index_returns_none() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let result = index_manager.load_index("/nonexistent", "age").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_list_indexes_empty_path_returns_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let indexes = index_manager.list_indexes("/nothing").unwrap();
  assert!(indexes.is_empty());
}

#[test]
fn test_deserialize_corrupt_data_returns_error() {
  let result = FieldIndex::deserialize(&[0x00], 32);
  assert!(result.is_err());
}

#[test]
fn test_deserialize_empty_data_returns_error() {
  let result = FieldIndex::deserialize(&[], 32);
  assert!(result.is_err());
}

#[test]
fn test_serialize_empty_index_roundtrip() {
  let converter = Box::new(StringConverter::new(512));
  let index = FieldIndex::new("email".to_string(), converter);

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.field_name, "email");
  assert_eq!(deserialized.len(), 0);
}

#[test]
fn test_remove_nonexistent_hash_is_noop() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&30u64.to_be_bytes(), vec![0xAA; 32]);
  assert_eq!(index.len(), 1);

  // Remove a hash that doesn't exist
  index.remove(&[0xFF; 32]);
  assert_eq!(index.len(), 1);
}

#[test]
fn test_delete_index() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 200));
  index_manager.create_index("/users", "age", converter).unwrap();

  // Verify it exists
  let loaded = index_manager.load_index("/users", "age").unwrap();
  assert!(loaded.is_some());

  // Delete it
  index_manager.delete_index("/users", "age", "u64").unwrap();

  // Verify it's gone
  let loaded = index_manager.load_index("/users", "age").unwrap();
  assert!(loaded.is_none());
}

#[test]
fn test_overwrite_index_via_save() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);
  index.insert(&25u64.to_be_bytes(), vec![0xAA; 32]);
  index_manager.save_index("/users", &index).unwrap();

  // Modify and save again
  index.insert(&50u64.to_be_bytes(), vec![0xBB; 32]);
  index_manager.save_index("/users", &index).unwrap();

  let loaded = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(loaded.len(), 2);
}

#[test]
fn test_clean_index_cache_eviction_keeps_dirty_indexes() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let mut index = FieldIndex::new("name".to_string(), Box::new(StringConverter::new(256)));
  index.insert_expanded(b"alice", vec![0xAA; 32]);
  index_manager.save_index("/users", &index).unwrap();

  let stats = index_manager.buffered_index_stats().unwrap();
  assert_eq!(stats.cached_indexes, 1);
  assert_eq!(stats.dirty_indexes, 1);
  assert_eq!(stats.estimated_clean_bytes, 0);
  assert_eq!(stats.estimated_dirty_bytes, stats.estimated_bytes);

  let evicted = index_manager.evict_clean_indexes_with_policy(0, Duration::ZERO).unwrap();
  assert_eq!(evicted, 0, "dirty indexes must never be evicted");

  let stats = index_manager.buffered_index_stats().unwrap();
  assert_eq!(stats.cached_indexes, 1);
  assert_eq!(stats.dirty_indexes, 1);
}

#[test]
fn test_clean_index_cache_eviction_drops_flushed_indexes_only_from_memory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let mut index = FieldIndex::new("name".to_string(), Box::new(StringConverter::new(256)));
  index.insert_expanded(b"alice", vec![0xAA; 32]);
  index_manager.save_index("/users", &index).unwrap();
  index_manager.flush_buffered_indexes().unwrap();

  let stats = index_manager.buffered_index_stats().unwrap();
  assert_eq!(stats.cached_indexes, 1);
  assert_eq!(stats.dirty_indexes, 0);
  assert_eq!(stats.estimated_dirty_bytes, 0);
  assert_eq!(stats.estimated_clean_bytes, stats.estimated_bytes);

  let evicted = index_manager.evict_clean_indexes_with_policy(0, Duration::ZERO).unwrap();
  assert_eq!(evicted, 1, "flushed clean index should be evicted from memory");

  let stats = index_manager.buffered_index_stats().unwrap();
  assert_eq!(stats.cached_indexes, 0);
  assert_eq!(stats.dirty_indexes, 0);

  let loaded = index_manager.load_index("/users", "name").unwrap();
  assert!(loaded.is_some(), "eviction must not delete the persisted index");
}

#[test]
fn index_cache_transitions_exact_reservations_only_after_successful_flush() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let mut index = FieldIndex::new("name".to_string(), Box::new(StringConverter::new(256)));
  index.insert_expanded(b"alice", vec![0xAA; 32]);
  index_manager.save_index("/users", &index).unwrap();

  let dirty = index_manager.buffered_index_stats().unwrap();
  assert!(dirty.mutation_max_bytes > 0);
  assert!(dirty.dirty_reserved_bytes >= dirty.estimated_dirty_bytes);
  assert_eq!(dirty.clean_reserved_bytes, 0);
  let dirty_owner = engine.memory_coordinator_snapshot().unwrap();
  let dirty_owner = dirty_owner.owner(MemoryOwner::IndexDirtyBuffers).unwrap();
  assert_eq!(dirty_owner.reserved_bytes, dirty.dirty_reserved_bytes);
  assert_eq!(dirty_owner.observed.resident_bytes, 0, "reservation-owned dirty indexes must not be double-counted");

  index_manager.flush_buffered_indexes().unwrap();
  let clean = index_manager.buffered_index_stats().unwrap();
  assert_eq!(clean.dirty_indexes, 0);
  assert_eq!(clean.dirty_reserved_bytes, 0);
  assert!(clean.clean_reserved_bytes >= clean.estimated_clean_bytes);
  let clean_owner = engine.memory_coordinator_snapshot().unwrap();
  assert_eq!(clean_owner.owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
  assert_eq!(clean_owner.owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, clean.clean_reserved_bytes);
  assert_eq!(clean_owner.owner(MemoryOwner::IndexCleanCache).unwrap().observed.resident_bytes, 0);

  assert_eq!(index_manager.evict_clean_indexes_with_policy(0, Duration::ZERO).unwrap(), 1);
  let evicted_owner = engine.memory_coordinator_snapshot().unwrap();
  assert_eq!(evicted_owner.owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, 0);
}

#[test]
fn index_flush_admission_failure_restores_every_selected_dirty_index() {
  const CHILD_MARKER: &str = "AEORDB_INDEX_FLUSH_ROLLBACK_CHILD";
  if std::env::var_os(CHILD_MARKER).is_none() {
    let status = Command::new(std::env::current_exe().unwrap())
      .arg("--exact")
      .arg("index_flush_admission_failure_restores_every_selected_dirty_index")
      .arg("--nocapture")
      .env(CHILD_MARKER, "1")
      .env("AEORDB_INDEX_PUBLICATION_BATCH_MAX_BYTES", (1024 * 1024).to_string())
      .status()
      .unwrap();
    assert!(status.success(), "isolated index-flush rollback child failed: {status}");
    return;
  }

  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let mut small = FieldIndex::new("a".to_string(), Box::new(StringConverter::new(256)));
  let mut sequence = 0u64;
  while small.serialized_size(32) < 300 * 1024 {
    let value = format!("small-{sequence:016x}");
    small.insert_expanded(value.as_bytes(), blake3::hash(value.as_bytes()).as_bytes().to_vec());
    sequence += 1;
  }
  let mut oversized = FieldIndex::new("z".to_string(), Box::new(StringConverter::new(256)));
  while oversized.serialized_size(32) <= 1024 * 1024 {
    let value = format!("oversized-{sequence:016x}");
    oversized.insert_expanded(value.as_bytes(), blake3::hash(value.as_bytes()).as_bytes().to_vec());
    sequence += 1;
  }

  index_manager.save_index("/users", &small).unwrap();
  index_manager.save_index("/users", &oversized).unwrap();
  let before = index_manager.buffered_index_stats().unwrap();
  assert_eq!(before.dirty_indexes, 2);
  assert_eq!(before.pending_mutations, 2);
  assert_eq!(before.publication_batch_max_bytes, 1024 * 1024);

  let error = index_manager.flush_buffered_indexes().unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::ResourceExhausted(_)), "unexpected error: {error}");

  let after = index_manager.buffered_index_stats().unwrap();
  assert_eq!(after.dirty_indexes, 2, "a rejected publication batch must restore every selected dirty key");
  assert_eq!(after.pending_mutations, 2);
  assert_eq!(after.flushing_indexes, 0);
  assert_eq!(after.flush_reserved_bytes, 0);
  assert_eq!(after.clean_reserved_bytes, 0);
  assert!(after.dirty_reserved_bytes >= after.estimated_dirty_bytes);
  assert_eq!(
    index_manager.evict_clean_indexes_with_policy(0, Duration::ZERO).unwrap(),
    0,
    "dirty rollback state must remain non-evictable"
  );

  let memory = engine.memory_coordinator_snapshot().unwrap();
  let dirty = memory.owner(MemoryOwner::IndexDirtyBuffers).unwrap();
  assert_eq!(dirty.reserved_bytes, after.dirty_reserved_bytes);
  assert_eq!(dirty.critical_reserved_bytes, 0, "rejected flush scratch reservations must be released");
}

#[test]
fn index_mutation_cap_rejects_without_retaining_unreserved_state() {
  const CHILD_MARKER: &str = "AEORDB_INDEX_MUTATION_CAP_CHILD";
  if std::env::var_os(CHILD_MARKER).is_none() {
    let status = Command::new(std::env::current_exe().unwrap())
      .arg("--exact")
      .arg("index_mutation_cap_rejects_without_retaining_unreserved_state")
      .arg("--nocapture")
      .env(CHILD_MARKER, "1")
      .env("AEORDB_INDEX_MUTATION_BUFFER_MAX_BYTES", (16 * 1024 * 1024).to_string())
      .status()
      .unwrap();
    assert!(status.success(), "isolated index-mutation cap child failed: {status}");
    return;
  }

  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);
  let mut index = FieldIndex::new("large".to_string(), Box::new(StringConverter::new(256)));
  let entry_count = (18 * 1024 * 1024) / (8 + 32) + 1;
  index.entries.reserve(entry_count);
  for sequence in 0..entry_count {
    let mut hash = vec![0u8; 32];
    hash[..8].copy_from_slice(&(sequence as u64).to_be_bytes());
    index.entries.push(IndexEntry { scalar: sequence as f64 / entry_count as f64, file_hash: hash });
  }
  assert!(index.serialized_size(32) > 18 * 1024 * 1024);

  let error = index_manager.save_index("/users", &index).unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  let stats = index_manager.buffered_index_stats().unwrap();
  assert_eq!(stats.mutation_max_bytes, 16 * 1024 * 1024);
  assert_eq!(stats.cached_indexes, 0);
  assert_eq!(stats.dirty_indexes, 0);
  assert_eq!(stats.pending_mutations, 0);
  assert_eq!(stats.dirty_reserved_bytes, 0);
  assert_eq!(engine.memory_coordinator_snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
}

#[test]
fn mutation_during_flush_remains_dirty_until_the_new_generation_is_durable() {
  let dir = tempfile::tempdir().unwrap();
  let engine = Arc::new(create_engine(&dir));
  let manager = IndexManager::new(&engine);
  let mut original = FieldIndex::new("race".to_string(), Box::new(StringConverter::new(256)));
  let entry_count = (12 * 1024 * 1024) / (8 + 32) + 1;
  original.entries.reserve(entry_count);
  for sequence in 0..entry_count {
    let hash = blake3::hash(&(sequence as u64).to_be_bytes()).as_bytes().to_vec();
    original.entries.push(IndexEntry { scalar: sequence as f64 / entry_count as f64, file_hash: hash });
  }
  let serialized_bytes = original.serialized_size(32) as u64;
  manager.save_index("/users", &original).unwrap();

  let flush_engine = Arc::clone(&engine);
  let flush = thread::spawn(move || IndexManager::new(&flush_engine).flush_buffered_indexes());
  let started = std::time::Instant::now();
  loop {
    let in_flight = manager.buffered_index_stats().unwrap();
    if in_flight.flushing_indexes == 1 {
      assert!(
        in_flight.flush_reserved_bytes >= serialized_bytes.saturating_add(64 * 1024),
        "flush admission must cover the publication buffer and bounded NVT/converter serialization scratch"
      );
      let memory = engine.memory_coordinator_snapshot().unwrap();
      let dirty = memory.owner(MemoryOwner::IndexDirtyBuffers).unwrap();
      assert_eq!(dirty.reserved_bytes, in_flight.dirty_reserved_bytes.saturating_add(in_flight.flush_reserved_bytes));
      assert_eq!(dirty.critical_reserved_bytes, in_flight.flush_reserved_bytes);
      break;
    }
    assert!(started.elapsed() < Duration::from_secs(5), "flush did not expose its in-flight generation");
    thread::sleep(Duration::from_millis(1));
  }

  let mut replacement = FieldIndex::new("race".to_string(), Box::new(StringConverter::new(256)));
  replacement.insert_expanded(b"replacement", vec![0xCC; 32]);
  manager.save_index("/users", &replacement).unwrap();
  assert_eq!(flush.join().unwrap().unwrap(), 1);

  let redirtied = manager.buffered_index_stats().unwrap();
  assert_eq!(redirtied.flushing_indexes, 0);
  assert_eq!(redirtied.dirty_indexes, 1, "the generation written during the race must not clean a newer mutation");
  assert_eq!(redirtied.clean_reserved_bytes, 0);
  assert!(redirtied.dirty_reserved_bytes >= redirtied.estimated_dirty_bytes);
  let mut visible = manager.load_index_by_strategy("/users", "race", "string").unwrap().unwrap();
  assert_eq!(visible.lookup_exact(b"replacement").len(), 1, "queries must observe the newer buffered generation");

  assert_eq!(manager.flush_buffered_indexes().unwrap(), 1);
  assert_eq!(manager.evict_clean_indexes_with_policy(0, Duration::ZERO).unwrap(), 1);
  let mut persisted = manager.load_index_by_strategy("/users", "race", "string").unwrap().unwrap();
  assert_eq!(persisted.lookup_exact(b"replacement").len(), 1, "the follow-up flush must persist the newer generation");
}

#[test]
fn deletion_during_flush_remains_dirty_until_the_delete_is_durable() {
  let dir = tempfile::tempdir().unwrap();
  let engine = Arc::new(create_engine(&dir));
  let manager = IndexManager::new(&engine);
  let mut original = FieldIndex::new("race-delete".to_string(), Box::new(StringConverter::new(256)));
  let entry_count = (12 * 1024 * 1024) / (8 + 32) + 1;
  original.entries.reserve(entry_count);
  for sequence in 0..entry_count {
    let hash = blake3::hash(&(sequence as u64).to_be_bytes()).as_bytes().to_vec();
    original.entries.push(IndexEntry { scalar: sequence as f64 / entry_count as f64, file_hash: hash });
  }
  manager.save_index("/users", &original).unwrap();

  let flush_engine = Arc::clone(&engine);
  let flush = thread::spawn(move || IndexManager::new(&flush_engine).flush_buffered_indexes());
  let started = std::time::Instant::now();
  loop {
    if manager.buffered_index_stats().unwrap().flushing_indexes == 1 {
      break;
    }
    assert!(started.elapsed() < Duration::from_secs(5), "flush did not expose its in-flight generation");
    thread::sleep(Duration::from_millis(1));
  }

  manager.delete_index("/users", "race-delete", "string").unwrap();
  assert_eq!(flush.join().unwrap().unwrap(), 1);

  let pending_delete = manager.buffered_index_stats().unwrap();
  assert_eq!(pending_delete.flushing_indexes, 0);
  assert_eq!(pending_delete.deleted_indexes, 1, "the old generation must not clean a newer delete");
  assert_eq!(pending_delete.pending_mutations, 1);
  assert_eq!(pending_delete.clean_reserved_bytes, 0);
  assert!(pending_delete.dirty_reserved_bytes > 0);
  assert!(manager.load_index_by_strategy("/users", "race-delete", "string").unwrap().is_none());

  assert_eq!(manager.flush_buffered_indexes().unwrap(), 0);
  let durable_delete = manager.buffered_index_stats().unwrap();
  assert_eq!(durable_delete.deleted_indexes, 0);
  assert_eq!(durable_delete.pending_mutations, 0);
  assert_eq!(durable_delete.dirty_reserved_bytes, 0);
  assert!(manager.load_index_by_strategy("/users", "race-delete", "string").unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn disk_write_failure_after_flush_snapshot_restores_dirty_state_and_reservations() {
  const CHILD_MARKER: &str = "AEORDB_INDEX_DISK_FAILURE_CHILD";
  if std::env::var_os(CHILD_MARKER).is_none() {
    // Use pipes rather than inherited regular files: this child deliberately
    // lowers RLIMIT_FSIZE, which must constrain the database write without
    // preventing the test harness from reporting its result to a redirected
    // parent log.
    let output = Command::new(std::env::current_exe().unwrap())
      .arg("--exact")
      .arg("disk_write_failure_after_flush_snapshot_restores_dirty_state_and_reservations")
      .arg("--nocapture")
      .env(CHILD_MARKER, "1")
      .output()
      .unwrap();
    assert!(
      output.status.success(),
      "isolated index disk-failure child failed: {}\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
    return;
  }

  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let manager = IndexManager::new(&engine);
  let mut index = FieldIndex::new("disk-failure".to_string(), Box::new(StringConverter::new(256)));
  let entry_count = (2 * 1024 * 1024) / (8 + 32) + 1;
  index.entries.reserve(entry_count);
  for sequence in 0..entry_count {
    let hash = blake3::hash(&(sequence as u64).to_be_bytes()).as_bytes().to_vec();
    index.entries.push(IndexEntry { scalar: sequence as f64 / entry_count as f64, file_hash: hash });
  }
  manager.save_index("/users", &index).unwrap();
  let before = manager.buffered_index_stats().unwrap();
  assert_eq!(before.dirty_indexes, 1);
  assert_eq!(before.pending_mutations, 1);

  let database_length = std::fs::metadata(dir.path().join("test.aeor")).unwrap().len();
  unsafe {
    libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    let limit = libc::rlimit { rlim_cur: database_length as libc::rlim_t, rlim_max: database_length as libc::rlim_t };
    assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0, "failed to install isolated file-size limit");
  }

  let error = manager.flush_buffered_indexes().unwrap_err();
  assert!(
    matches!(error, aeordb::engine::EngineError::IoError(_) | aeordb::engine::EngineError::DurabilityFailure(_)),
    "unexpected persistence failure: {error}"
  );
  let restored = manager.buffered_index_stats().unwrap();
  assert_eq!(restored.dirty_indexes, 1);
  assert_eq!(restored.pending_mutations, 1);
  assert_eq!(restored.flushing_indexes, 0);
  assert_eq!(restored.flush_reserved_bytes, 0);
  assert_eq!(restored.clean_reserved_bytes, 0);
  assert!(restored.dirty_reserved_bytes >= restored.estimated_dirty_bytes);
  let memory = engine.memory_coordinator_snapshot().unwrap();
  let dirty = memory.owner(MemoryOwner::IndexDirtyBuffers).unwrap();
  assert_eq!(dirty.reserved_bytes, restored.dirty_reserved_bytes);
  assert_eq!(dirty.critical_reserved_bytes, 0);
}

#[test]
fn forced_flush_batches_to_the_resolved_publication_limit() {
  const CHILD_MARKER: &str = "AEORDB_INDEX_PUBLICATION_BATCH_CHILD";
  if std::env::var_os(CHILD_MARKER).is_none() {
    let status = Command::new(std::env::current_exe().unwrap())
      .arg("--exact")
      .arg("forced_flush_batches_to_the_resolved_publication_limit")
      .arg("--nocapture")
      .env(CHILD_MARKER, "1")
      .env("AEORDB_INDEX_PUBLICATION_BATCH_MAX_BYTES", (1024 * 1024).to_string())
      .status()
      .unwrap();
    assert!(status.success(), "isolated index-publication batch child failed: {status}");
    return;
  }

  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let manager = IndexManager::new(&engine);
  for field in ["a", "b"] {
    let mut index = FieldIndex::new(field.to_string(), Box::new(StringConverter::new(256)));
    let entry_count = (600 * 1024) / (8 + 32) + 1;
    index.entries.reserve(entry_count);
    for sequence in 0..entry_count {
      let hash = blake3::hash(format!("{field}-{sequence}").as_bytes()).as_bytes().to_vec();
      index.entries.push(IndexEntry { scalar: sequence as f64 / entry_count as f64, file_hash: hash });
    }
    assert!((512 * 1024..1024 * 1024).contains(&index.serialized_size(32)));
    manager.save_index("/users", &index).unwrap();
  }

  assert_eq!(manager.flush_buffered_indexes().unwrap(), 2);
  let stats = manager.buffered_index_stats().unwrap();
  assert_eq!(stats.pending_mutations, 0);
  assert_eq!(stats.dirty_indexes, 0);
  assert_eq!(stats.flushes, 2, "the one-MiB publication cap should split two 600-KiB indexes into two generations");
  assert_eq!(stats.flushing_indexes, 0);
  assert_eq!(stats.flush_reserved_bytes, 0);
  assert_eq!(stats.dirty_reserved_bytes, 0);
  assert!(stats.clean_reserved_bytes >= stats.estimated_clean_bytes);
}

// --- NVT-backed lookup tests ---

#[test]
fn test_field_index_nvt_lookup_exact() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  // Insert several values
  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&25u64.to_be_bytes(), vec![0x25; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&75u64.to_be_bytes(), vec![0x75; 32]);
  index.insert(&99u64.to_be_bytes(), vec![0x99; 32]);

  // Exact lookup should find the right entry via NVT bucket
  let results = index.lookup_exact(&50u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, vec![0x50; 32]);

  // Lookup a value that doesn't exist
  let results = index.lookup_exact(&42u64.to_be_bytes());
  assert_eq!(results.len(), 0);

  // Lookup at boundaries
  let results = index.lookup_exact(&10u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, vec![0x10; 32]);

  let results = index.lookup_exact(&99u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, vec![0x99; 32]);
}

#[test]
fn test_field_index_nvt_lookup_range() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  for value in (0..=100).step_by(5) {
    let hash_byte = (value & 0xFF) as u8;
    index.insert(&(value as u64).to_be_bytes(), vec![hash_byte; 32]);
  }

  // Range query spanning multiple NVT buckets
  let results = index.lookup_range(&20u64.to_be_bytes(), &40u64.to_be_bytes()).unwrap();

  // Should include 20, 25, 30, 35, 40
  assert_eq!(results.len(), 5);
  for entry in &results {
    assert!(entry.scalar >= 0.2 - f64::EPSILON);
    assert!(entry.scalar <= 0.4 + f64::EPSILON);
  }

  // Range at the very start
  let results = index.lookup_range(&0u64.to_be_bytes(), &5u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 2); // 0 and 5
}

#[test]
fn test_field_index_nvt_rebuild_on_dirty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  // Initially not dirty (empty, nothing to rebuild)
  assert!(!index.is_dirty());

  // Insert marks dirty
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  assert!(index.is_dirty());

  // A lookup triggers rebuild, clears dirty
  let _results = index.lookup_exact(&50u64.to_be_bytes());
  assert!(!index.is_dirty());

  // Insert again marks dirty
  index.insert(&25u64.to_be_bytes(), vec![0x25; 32]);
  assert!(index.is_dirty());

  // Another lookup clears dirty and returns correct results
  let result_count = index.lookup_exact(&25u64.to_be_bytes()).len();
  assert!(!index.is_dirty());
  assert_eq!(result_count, 1);
}

#[test]
fn test_field_index_nvt_insert_marks_dirty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  assert!(!index.is_dirty());

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  assert!(index.is_dirty());

  // Force rebuild
  index.ensure_nvt_current();
  assert!(!index.is_dirty());

  // Remove marks dirty
  index.remove(&[0x10; 32]);
  assert!(index.is_dirty());

  // Removing a non-existent hash does NOT mark dirty
  index.ensure_nvt_current();
  assert!(!index.is_dirty());
  index.remove(&[0xFF; 32]);
  assert!(!index.is_dirty());
}

// ===========================================================================
// Task 10: Index serialization with NVT
// ===========================================================================

#[test]
fn test_field_index_serialization_with_nvt() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];
  let hash_c = vec![0xCC; 32];
  index.insert(&25u64.to_be_bytes(), hash_a.clone());
  index.insert(&50u64.to_be_bytes(), hash_b.clone());
  index.insert(&75u64.to_be_bytes(), hash_c.clone());

  // Force NVT rebuild before serialize so NVT is current.
  index.ensure_nvt_current();

  let hash_length = 32;
  let serialized = index.serialize(hash_length);

  // The new format should be larger than the old format because it includes NVT data.
  // Minimum NVT overhead: 4 (nvt_length) + version(1) + converter_length(4) + converter_data + bucket_count(4) + buckets
  assert!(serialized.len() > 100);

  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();
  assert_eq!(deserialized.field_name, "age");
  assert_eq!(deserialized.len(), 3);
  assert_eq!(deserialized.entries[0].file_hash, hash_a);
  assert_eq!(deserialized.entries[1].file_hash, hash_b);
  assert_eq!(deserialized.entries[2].file_hash, hash_c);

  // NVT should be functional after deserialization.
  let mut deserialized = deserialized;
  let results = deserialized.lookup_exact(&50u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_b);

  // Converter should produce the same scalars.
  let original_scalar = index.converter.to_scalar(&75u64.to_be_bytes());
  let deserialized_scalar = deserialized.converter.to_scalar(&75u64.to_be_bytes());
  assert!((original_scalar - deserialized_scalar).abs() < f64::EPSILON);
}

#[test]
fn test_field_index_serialization_with_nvt_empty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let index = FieldIndex::new("score".to_string(), converter);

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.field_name, "score");
  assert_eq!(deserialized.len(), 0);
  assert!(deserialized.is_empty());
}

#[test]
fn test_field_index_serialization_roundtrip_preserves_lookups() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  for value in (0..=100).step_by(10) {
    let hash_byte = (value & 0xFF) as u8;
    index.insert(&(value as u64).to_be_bytes(), vec![hash_byte; 32]);
  }
  index.ensure_nvt_current();

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let mut deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  // Verify all lookups work after roundtrip.
  let results = deserialized.lookup_exact(&50u64.to_be_bytes());
  assert_eq!(results.len(), 1);

  let results = deserialized.lookup_range(&20u64.to_be_bytes(), &60u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 5); // 20, 30, 40, 50, 60

  let results = deserialized.lookup_gt(&70u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 3); // 80, 90, 100

  let results = deserialized.lookup_lt(&30u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 3); // 0, 10, 20
}

// ===========================================================================
// Task 3: Direct scalar jump lookups
// ===========================================================================

#[test]
fn test_field_index_scalar_jump_lookup() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  let hash_a = vec![0x10; 32];
  let hash_b = vec![0x50; 32];
  let hash_c = vec![0x99; 32];
  index.insert(&10u64.to_be_bytes(), hash_a.clone());
  index.insert(&50u64.to_be_bytes(), hash_b.clone());
  index.insert(&99u64.to_be_bytes(), hash_c.clone());

  // Compute the scalar for value 50, then look up by scalar directly.
  let scalar_50 = index.converter.to_scalar(&50u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_50);
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_b);

  // Scalar for a value that doesn't exist should return empty.
  let scalar_42 = index.converter.to_scalar(&42u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_42);
  assert_eq!(results.len(), 0);
}

#[test]
fn test_field_index_scalar_range_lookup() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  for value in (0..=100).step_by(10) {
    let hash_byte = (value & 0xFF) as u8;
    index.insert(&(value as u64).to_be_bytes(), vec![hash_byte; 32]);
  }

  let min_scalar = index.converter.to_scalar(&20u64.to_be_bytes());
  let max_scalar = index.converter.to_scalar(&50u64.to_be_bytes());
  let results = index.lookup_by_scalar_range(min_scalar, max_scalar);

  // Should include 20, 30, 40, 50
  assert_eq!(results.len(), 4);
  for entry in &results {
    assert!(entry.scalar >= min_scalar - f64::EPSILON);
    assert!(entry.scalar <= max_scalar + f64::EPSILON);
  }
}

#[test]
fn test_field_index_scalar_jump_empty_index() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  let results = index.lookup_by_scalar(0.5);
  assert!(results.is_empty());

  let results = index.lookup_by_scalar_range(0.0, 1.0);
  assert!(results.is_empty());
}

#[test]
fn test_field_index_scalar_jump_boundary_values() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  index.insert(&0u64.to_be_bytes(), vec![0x00; 32]);
  index.insert(&100u64.to_be_bytes(), vec![0xFF; 32]);

  // Lookup at the minimum scalar
  let scalar_0 = index.converter.to_scalar(&0u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_0);
  assert_eq!(results.len(), 1);

  // Lookup at the maximum scalar
  let scalar_100 = index.converter.to_scalar(&100u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_100);
  assert_eq!(results.len(), 1);

  // Full range should return both
  let results = index.lookup_by_scalar_range(scalar_0, scalar_100);
  assert_eq!(results.len(), 2);
}
