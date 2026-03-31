use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::kv_store::{
  KVEntry, KVStore,
  KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_DELETION,
  KV_TYPE_SNAPSHOT, KV_TYPE_VOID, KV_TYPE_HEAD, KV_TYPE_FORK, KV_TYPE_VERSION,
  KV_FLAG_PENDING, KV_FLAG_DELETED,
};

fn make_hash(value: u8) -> Vec<u8> {
  let mut hash = vec![0u8; 32];
  hash[0] = value;
  hash
}

fn make_entry(hash_byte: u8, offset: u64) -> KVEntry {
  KVEntry {
    type_flags: KV_TYPE_CHUNK,
    hash: make_hash(hash_byte),
    offset,
  }
}

fn make_entry_with_type(hash_byte: u8, offset: u64, type_flags: u8) -> KVEntry {
  KVEntry {
    type_flags,
    hash: make_hash(hash_byte),
    offset,
  }
}

fn make_blake3_hash(data: &[u8]) -> Vec<u8> {
  blake3::hash(data).as_bytes().to_vec()
}

// --- Basic CRUD tests ---

#[test]
fn test_insert_and_get() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  let entry = make_entry(0xAA, 1000);
  store.insert(entry.clone());

  let found = store.get(&make_hash(0xAA));
  assert!(found.is_some(), "inserted entry should be found");
  let found = found.unwrap();
  assert_eq!(found.offset, 1000);
  assert_eq!(found.type_flags, KV_TYPE_CHUNK);
}

#[test]
fn test_insert_maintains_sorted_order() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);

  // Insert in reverse order
  store.insert(make_entry(0xFF, 300));
  store.insert(make_entry(0x80, 200));
  store.insert(make_entry(0x01, 100));

  let hashes: Vec<u8> = store.iter().map(|entry| entry.hash[0]).collect();
  assert_eq!(hashes, vec![0x01, 0x80, 0xFF], "entries should be sorted by hash");
}

#[test]
fn test_get_nonexistent_returns_none() {
  let store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  let result = store.get(&make_hash(0xDE));
  assert!(result.is_none(), "nonexistent key should return None");
}

#[test]
fn test_remove_entry() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0xAA, 1000));
  store.insert(make_entry(0xBB, 2000));

  let removed = store.remove(&make_hash(0xAA));
  assert!(removed.is_some(), "remove should return the removed entry");
  assert_eq!(removed.unwrap().offset, 1000);
  assert_eq!(store.len(), 1);
  assert!(store.get(&make_hash(0xAA)).is_none(), "removed entry should not be found");
  assert!(store.get(&make_hash(0xBB)).is_some(), "other entry should still exist");
}

#[test]
fn test_remove_nonexistent_returns_none() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0xAA, 1000));

  let result = store.remove(&make_hash(0xFF));
  assert!(result.is_none(), "removing nonexistent key should return None");
  assert_eq!(store.len(), 1, "store should be unchanged");
}

#[test]
fn test_update_offset() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0xAA, 1000));

  let updated = store.update_offset(&make_hash(0xAA), 5000);
  assert!(updated, "update_offset should return true for existing key");

  let found = store.get(&make_hash(0xAA)).unwrap();
  assert_eq!(found.offset, 5000, "offset should be updated");
}

#[test]
fn test_update_offset_nonexistent() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  let updated = store.update_offset(&make_hash(0xAA), 5000);
  assert!(!updated, "update_offset should return false for nonexistent key");
}

#[test]
fn test_update_flags() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0xAA, 1000));

  let updated = store.update_flags(&make_hash(0xAA), KV_FLAG_PENDING);
  assert!(updated, "update_flags should return true for existing key");

  let found = store.get(&make_hash(0xAA)).unwrap();
  assert!(found.is_pending(), "entry should be marked pending");
  assert_eq!(found.entry_type(), KV_TYPE_CHUNK, "entry type should be preserved");
}

#[test]
fn test_update_flags_nonexistent() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  let updated = store.update_flags(&make_hash(0xAA), KV_FLAG_PENDING);
  assert!(!updated, "update_flags should return false for nonexistent key");
}

#[test]
fn test_contains() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0xAA, 1000));

  assert!(store.contains(&make_hash(0xAA)), "should contain inserted hash");
  assert!(!store.contains(&make_hash(0xBB)), "should not contain non-inserted hash");
}

#[test]
fn test_len() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  assert_eq!(store.len(), 0);
  assert!(store.is_empty());

  store.insert(make_entry(0xAA, 1000));
  assert_eq!(store.len(), 1);
  assert!(!store.is_empty());

  store.insert(make_entry(0xBB, 2000));
  assert_eq!(store.len(), 2);

  store.remove(&make_hash(0xAA));
  assert_eq!(store.len(), 1);
}

#[test]
fn test_iterate_all() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0x01, 100));
  store.insert(make_entry(0x02, 200));
  store.insert(make_entry(0x03, 300));

  let all_entries: Vec<&KVEntry> = store.iter().collect();
  assert_eq!(all_entries.len(), 3);

  // Should be sorted
  assert_eq!(all_entries[0].hash[0], 0x01);
  assert_eq!(all_entries[1].hash[0], 0x02);
  assert_eq!(all_entries[2].hash[0], 0x03);
}

#[test]
fn test_entries_in_range() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0x01, 100));
  store.insert(make_entry(0x02, 500));
  store.insert(make_entry(0x03, 1000));
  store.insert(make_entry(0x04, 2000));

  let in_range = store.entries_in_range(200, 1500);
  assert_eq!(in_range.len(), 2, "should find entries at offset 500 and 1000");
  assert_eq!(in_range[0].offset, 500);
  assert_eq!(in_range[1].offset, 1000);
}

#[test]
fn test_entries_in_range_empty() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry(0x01, 100));
  store.insert(make_entry(0x02, 500));

  let in_range = store.entries_in_range(200, 400);
  assert_eq!(in_range.len(), 0, "should find no entries in empty range");
}

// --- NVT-accelerated lookup tests ---

#[test]
fn test_nvt_accelerated_lookup() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 32);

  // Insert 100 entries with realistic BLAKE3 hashes
  let mut hashes = Vec::new();
  for index in 0..100u32 {
    let hash = make_blake3_hash(&index.to_le_bytes());
    hashes.push(hash.clone());
    store.insert(KVEntry {
      type_flags: KV_TYPE_CHUNK,
      hash,
      offset: index as u64 * 1000,
    });
  }

  // Verify all 100 entries can be found via NVT-accelerated lookup
  for (index, hash) in hashes.iter().enumerate() {
    let found = store.get(hash);
    assert!(found.is_some(), "entry {} should be found via NVT lookup", index);
    assert_eq!(found.unwrap().offset, index as u64 * 1000);
  }

  // Verify a hash that was never inserted returns None
  let missing_hash = make_blake3_hash(b"not-inserted");
  assert!(store.get(&missing_hash).is_none(), "missing hash should return None");
}

// --- Serialization tests ---

#[test]
fn test_serialize_deserialize_roundtrip() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  store.insert(make_entry_with_type(0x10, 100, KV_TYPE_CHUNK));
  store.insert(make_entry_with_type(0x50, 500, KV_TYPE_FILE_RECORD | KV_FLAG_PENDING));
  store.insert(make_entry_with_type(0xA0, 1000, KV_TYPE_DIRECTORY));

  let serialized = store.serialize();
  let deserialized = KVStore::deserialize(&serialized)
    .expect("deserialization should succeed");

  assert_eq!(deserialized.len(), 3);
  assert_eq!(deserialized.version(), 1);

  let first = deserialized.get(&make_hash(0x10)).unwrap();
  assert_eq!(first.offset, 100);
  assert_eq!(first.entry_type(), KV_TYPE_CHUNK);

  let second = deserialized.get(&make_hash(0x50)).unwrap();
  assert_eq!(second.offset, 500);
  assert_eq!(second.entry_type(), KV_TYPE_FILE_RECORD);
  assert!(second.is_pending());

  let third = deserialized.get(&make_hash(0xA0)).unwrap();
  assert_eq!(third.offset, 1000);
  assert_eq!(third.entry_type(), KV_TYPE_DIRECTORY);
}

#[test]
fn test_serialize_deserialize_empty() {
  let store = KVStore::new(HashAlgorithm::Blake3_256, 8);
  let serialized = store.serialize();
  let deserialized = KVStore::deserialize(&serialized)
    .expect("empty store deserialization should succeed");
  assert_eq!(deserialized.len(), 0);
}

// --- Rebuild NVT tests ---

#[test]
fn test_rebuild_nvt() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);

  for index in 0..50u32 {
    let hash = make_blake3_hash(&index.to_le_bytes());
    store.insert(KVEntry {
      type_flags: KV_TYPE_CHUNK,
      hash,
      offset: index as u64 * 100,
    });
  }

  // Force rebuild
  store.rebuild_nvt();

  // Verify all entries are still findable after rebuild
  for index in 0..50u32 {
    let hash = make_blake3_hash(&index.to_le_bytes());
    assert!(store.get(&hash).is_some(), "entry {} should be found after rebuild", index);
  }

  // Verify NVT bucket entries sum to total entries
  let total_in_nvt: u32 = (0..store.nvt().bucket_count())
    .map(|index| store.nvt().get_bucket(index).entry_count)
    .sum();
  assert_eq!(total_in_nvt, 50, "NVT total entries should match store length");
}

// --- Bulk insert test ---

#[test]
fn test_bulk_insert_1000_entries() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 64);

  let mut hashes = Vec::with_capacity(1000);
  for index in 0..1000u32 {
    let hash = make_blake3_hash(&index.to_le_bytes());
    hashes.push(hash.clone());
    store.insert(KVEntry {
      type_flags: KV_TYPE_CHUNK,
      hash,
      offset: index as u64 * 512,
    });
  }

  assert_eq!(store.len(), 1000);

  // Verify all 1000 entries are retrievable
  for (index, hash) in hashes.iter().enumerate() {
    let found = store.get(hash);
    assert!(found.is_some(), "entry {} should be found in 1000-entry store", index);
    assert_eq!(found.unwrap().offset, index as u64 * 512);
  }
}

// --- Type flags encoding tests ---

#[test]
fn test_type_flags_encoding() {
  // Type constants should be in lower 4 bits (0x0-0xF)
  assert_eq!(KV_TYPE_CHUNK, 0x0);
  assert_eq!(KV_TYPE_FILE_RECORD, 0x1);
  assert_eq!(KV_TYPE_DIRECTORY, 0x2);
  assert_eq!(KV_TYPE_DELETION, 0x3);
  assert_eq!(KV_TYPE_SNAPSHOT, 0x4);
  assert_eq!(KV_TYPE_VOID, 0x5);
  assert_eq!(KV_TYPE_HEAD, 0x6);
  assert_eq!(KV_TYPE_FORK, 0x7);
  assert_eq!(KV_TYPE_VERSION, 0x8);

  // Flag constants should be in upper 4 bits
  assert_eq!(KV_FLAG_PENDING, 0x10);
  assert_eq!(KV_FLAG_DELETED, 0x20);

  // Combined type + flags should not overlap
  let combined = KV_TYPE_FILE_RECORD | KV_FLAG_PENDING | KV_FLAG_DELETED;
  assert_eq!(combined & 0x0F, KV_TYPE_FILE_RECORD);
  assert_eq!(combined & 0xF0, KV_FLAG_PENDING | KV_FLAG_DELETED);
}

#[test]
fn test_pending_flag() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 8);
  store.insert(KVEntry {
    type_flags: KV_TYPE_CHUNK | KV_FLAG_PENDING,
    hash: make_hash(0xAA),
    offset: 100,
  });

  let found = store.get(&make_hash(0xAA)).unwrap();
  assert!(found.is_pending());
  assert!(!found.is_deleted());
  assert_eq!(found.entry_type(), KV_TYPE_CHUNK);
}

#[test]
fn test_deleted_flag() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 8);
  store.insert(KVEntry {
    type_flags: KV_TYPE_FILE_RECORD | KV_FLAG_DELETED,
    hash: make_hash(0xBB),
    offset: 200,
  });

  let found = store.get(&make_hash(0xBB)).unwrap();
  assert!(found.is_deleted());
  assert!(!found.is_pending());
  assert_eq!(found.entry_type(), KV_TYPE_FILE_RECORD);
}

#[test]
fn test_insert_duplicate_hash_updates() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 8);

  store.insert(KVEntry {
    type_flags: KV_TYPE_CHUNK,
    hash: make_hash(0xAA),
    offset: 100,
  });
  assert_eq!(store.len(), 1);

  // Insert same hash with different offset — should update, not add
  store.insert(KVEntry {
    type_flags: KV_TYPE_FILE_RECORD,
    hash: make_hash(0xAA),
    offset: 999,
  });
  assert_eq!(store.len(), 1, "duplicate insert should update, not add");

  let found = store.get(&make_hash(0xAA)).unwrap();
  assert_eq!(found.offset, 999, "offset should be updated");
  assert_eq!(found.entry_type(), KV_TYPE_FILE_RECORD, "type should be updated");
}

// --- Deserialization error tests ---

#[test]
fn test_deserialize_truncated_header() {
  let data = vec![0x01, 0x01]; // too short
  let result = KVStore::deserialize(&data);
  assert!(result.is_err(), "truncated header should fail");
}

#[test]
fn test_deserialize_invalid_version() {
  let mut data = Vec::new();
  data.push(0); // invalid version
  data.extend_from_slice(&1u16.to_le_bytes());
  data.extend_from_slice(&0u64.to_le_bytes());
  data.extend_from_slice(&0u32.to_le_bytes()); // nvt length = 0
  // (would still fail because nvt data is missing, but version check comes first)

  let result = KVStore::deserialize(&data);
  assert!(result.is_err(), "version 0 should fail");
}

#[test]
fn test_deserialize_invalid_hash_algorithm() {
  let mut data = Vec::new();
  data.push(1); // valid version
  data.extend_from_slice(&0xFFFFu16.to_le_bytes()); // invalid hash algo
  data.extend_from_slice(&0u64.to_le_bytes());

  let result = KVStore::deserialize(&data);
  assert!(result.is_err(), "invalid hash algo should fail");
}

#[test]
fn test_deserialize_truncated_entries() {
  let mut data = Vec::new();
  data.push(1); // version
  data.extend_from_slice(&1u16.to_le_bytes()); // BLAKE3
  data.extend_from_slice(&100u64.to_le_bytes()); // claims 100 entries
  // but no actual entry data

  let result = KVStore::deserialize(&data);
  assert!(result.is_err(), "truncated entries should fail");
}

#[test]
fn test_update_flags_preserves_type() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 8);
  store.insert(KVEntry {
    type_flags: KV_TYPE_SNAPSHOT,
    hash: make_hash(0xCC),
    offset: 300,
  });

  store.update_flags(&make_hash(0xCC), KV_FLAG_PENDING | KV_FLAG_DELETED);
  let found = store.get(&make_hash(0xCC)).unwrap();
  assert_eq!(found.entry_type(), KV_TYPE_SNAPSHOT, "type should be preserved");
  assert!(found.is_pending(), "pending flag should be set");
  assert!(found.is_deleted(), "deleted flag should be set");
}

#[test]
fn test_entries_in_range_boundary() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 8);
  store.insert(make_entry(0x01, 100)); // at start boundary
  store.insert(make_entry(0x02, 200)); // in range
  store.insert(make_entry(0x03, 300)); // at end boundary (exclusive)

  // Range is [100, 300) — should include 100 and 200, but not 300
  let in_range = store.entries_in_range(100, 300);
  assert_eq!(in_range.len(), 2);
  assert_eq!(in_range[0].offset, 100);
  assert_eq!(in_range[1].offset, 200);
}

#[test]
fn test_get_from_empty_store() {
  let store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  assert!(store.get(&make_hash(0xAA)).is_none());
  assert!(!store.contains(&make_hash(0xAA)));
}

#[test]
fn test_remove_from_empty_store() {
  let mut store = KVStore::new(HashAlgorithm::Blake3_256, 16);
  assert!(store.remove(&make_hash(0xAA)).is_none());
}
