use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::kv_resize::KVResizeManager;
use aeordb::engine::kv_store::{KVEntry, KVStore, KV_TYPE_CHUNK};

fn make_entry(hash_byte: u8, offset: u64) -> KVEntry {
  let mut hash = vec![0u8; 32];
  hash[0] = hash_byte;
  KVEntry {
    type_flags: KV_TYPE_CHUNK,
    hash,
    offset,
    total_length: 64,
        }
}

#[test]
fn test_normal_mode_writes_to_primary() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  let entry = make_entry(0x01, 100);
  manager.insert(entry.clone());

  assert!(manager.primary().contains(&entry.hash));
  assert_eq!(manager.primary().len(), 1);
}

#[test]
fn test_begin_resize_creates_buffer() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  assert!(!manager.is_resizing());
  manager.begin_resize();
  assert!(manager.is_resizing());
}

#[test]
fn test_resize_mode_writes_to_buffer() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  manager.begin_resize();

  let entry = make_entry(0x01, 200);
  manager.insert(entry.clone());

  // Should NOT be in primary
  assert!(!manager.primary().contains(&entry.hash));
  // But should be findable via the manager's get (checks buffer first)
  assert!(manager.get(&entry.hash).is_some());
}

#[test]
fn test_resize_mode_reads_check_buffer_first() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  // Insert into primary before resize
  let primary_entry = make_entry(0x01, 100);
  manager.insert(primary_entry.clone());

  manager.begin_resize();

  // Insert a different offset for the same hash into buffer
  let buffer_entry = make_entry(0x01, 999);
  manager.insert(buffer_entry.clone());

  // get() should return the buffer version (offset 999), not primary (offset 100)
  let found = manager.get(&primary_entry.hash).expect("should find entry");
  assert_eq!(found.offset, 999);
}

#[test]
fn test_end_resize_merges_buffer() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  manager.begin_resize();

  let entry_a = make_entry(0x0A, 300);
  let entry_b = make_entry(0x0B, 400);
  manager.insert(entry_a.clone());
  manager.insert(entry_b.clone());

  // Primary should be empty
  assert_eq!(manager.primary().len(), 0);

  manager.end_resize();

  // After merge, both entries should be in primary
  assert!(!manager.is_resizing());
  assert_eq!(manager.primary().len(), 2);
  assert!(manager.primary().contains(&entry_a.hash));
  assert!(manager.primary().contains(&entry_b.hash));
}

#[test]
fn test_is_resizing_flag() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  assert!(!manager.is_resizing());

  manager.begin_resize();
  assert!(manager.is_resizing());

  manager.end_resize();
  assert!(!manager.is_resizing());
}

#[test]
fn test_get_falls_through_to_primary() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  // Insert into primary
  let entry = make_entry(0x42, 500);
  manager.insert(entry.clone());

  // Enter resize mode — buffer is empty
  manager.begin_resize();

  // get() should fall through buffer (empty) and find in primary
  let found = manager.get(&entry.hash).expect("should fall through to primary");
  assert_eq!(found.offset, 500);
}

#[test]
fn test_contains_checks_buffer_then_primary() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  let primary_entry = make_entry(0x01, 100);
  manager.insert(primary_entry.clone());

  manager.begin_resize();

  let buffer_entry = make_entry(0x02, 200);
  manager.insert(buffer_entry.clone());

  // Both should be found
  assert!(manager.contains(&primary_entry.hash));
  assert!(manager.contains(&buffer_entry.hash));

  // Unknown hash should not be found
  let unknown = make_entry(0xFF, 0);
  assert!(!manager.contains(&unknown.hash));
}

#[test]
fn test_end_resize_merges_updates_over_primary() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  // Insert into primary
  let original = make_entry(0x01, 100);
  manager.insert(original.clone());

  manager.begin_resize();

  // Update same hash in buffer with new offset
  let updated = make_entry(0x01, 999);
  manager.insert(updated.clone());

  manager.end_resize();

  // Primary should now have the updated offset
  let found = manager.primary().get(&original.hash).expect("should exist");
  assert_eq!(found.offset, 999);
}

#[test]
fn test_primary_mut_access() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  let entry = make_entry(0x01, 100);
  manager.insert(entry.clone());

  // Directly mutate primary
  let updated = manager.primary_mut().update_offset(&entry.hash, 999);
  assert!(updated);

  let found = manager.primary().get(&entry.hash).expect("should exist");
  assert_eq!(found.offset, 999);
}

#[test]
fn test_multiple_resize_cycles() {
  let kv_store = KVStore::new(HashAlgorithm::Blake3_256, 64);
  let mut manager = KVResizeManager::new(kv_store);

  // First resize cycle
  manager.begin_resize();
  manager.insert(make_entry(0x01, 100));
  manager.end_resize();
  assert_eq!(manager.primary().len(), 1);

  // Second resize cycle
  manager.begin_resize();
  manager.insert(make_entry(0x02, 200));
  manager.end_resize();
  assert_eq!(manager.primary().len(), 2);

  // Third resize cycle — update existing
  manager.begin_resize();
  manager.insert(make_entry(0x01, 999));
  manager.end_resize();
  assert_eq!(manager.primary().len(), 2);
  let found = manager.primary().get(&make_entry(0x01, 0).hash).unwrap();
  assert_eq!(found.offset, 999);
}
