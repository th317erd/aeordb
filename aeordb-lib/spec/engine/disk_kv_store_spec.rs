use aeordb::engine::disk_kv_store::DiskKVStore;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::kv_pages::*;
use aeordb::engine::kv_store::{KVEntry, KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_FLAG_DELETED, KV_FLAG_PENDING};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
use tempfile::tempdir;

// ============================================================================
// Helper
// ============================================================================

fn make_hash(seed: u8) -> Vec<u8> {
    let data = vec![seed; 32];
    blake3::hash(&data).as_bytes().to_vec()
}

fn make_entry(seed: u8, offset: u64) -> KVEntry {
    KVEntry {
        type_flags: KV_TYPE_CHUNK,
        hash: make_hash(seed),
        offset,
    }
}

// ============================================================================
// Page tests (kv_pages)
// ============================================================================

#[test]
fn test_serialize_deserialize_empty_page() {
    let hash_length = 32; // BLAKE3
    let entries: Vec<KVEntry> = vec![];
    let page = serialize_page(&entries, hash_length);
    assert_eq!(page.len(), page_size(hash_length));

    let result = deserialize_page(&page, hash_length).unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_serialize_deserialize_with_entries() {
    let hash_length = 32;
    let entries = vec![
        make_entry(1, 100),
        make_entry(2, 200),
        make_entry(3, 300),
    ];

    let page = serialize_page(&entries, hash_length);
    assert_eq!(page.len(), page_size(hash_length));

    let result = deserialize_page(&page, hash_length).unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].hash, entries[0].hash);
    assert_eq!(result[0].offset, 100);
    assert_eq!(result[1].hash, entries[1].hash);
    assert_eq!(result[1].offset, 200);
    assert_eq!(result[2].hash, entries[2].hash);
    assert_eq!(result[2].offset, 300);
}

#[test]
fn test_find_in_page() {
    let entries = vec![
        make_entry(1, 100),
        make_entry(2, 200),
        make_entry(3, 300),
    ];
    let hash = make_hash(2);
    let found = find_in_page(&entries, &hash);
    assert!(found.is_some());
    assert_eq!(found.unwrap().offset, 200);
}

#[test]
fn test_find_in_page_missing() {
    let entries = vec![
        make_entry(1, 100),
        make_entry(2, 200),
    ];
    let hash = make_hash(99);
    let found = find_in_page(&entries, &hash);
    assert!(found.is_none());
}

#[test]
fn test_find_in_page_skips_deleted() {
    let mut entry = make_entry(5, 500);
    entry.type_flags |= KV_FLAG_DELETED;
    let entries = vec![entry];

    let hash = make_hash(5);
    let found = find_in_page(&entries, &hash);
    assert!(found.is_none(), "Deleted entries should not be found");
}

#[test]
fn test_upsert_insert() {
    let mut entries: Vec<KVEntry> = vec![];
    let entry = make_entry(1, 100);
    let result = upsert_in_page(&mut entries, entry);
    assert!(result);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].offset, 100);
}

#[test]
fn test_upsert_update() {
    let mut entries = vec![make_entry(1, 100)];
    let mut updated = make_entry(1, 999);
    updated.type_flags = KV_TYPE_FILE_RECORD;

    let result = upsert_in_page(&mut entries, updated);
    assert!(result);
    assert_eq!(entries.len(), 1); // no new entry
    assert_eq!(entries[0].offset, 999);
    assert_eq!(entries[0].entry_type(), KV_TYPE_FILE_RECORD);
}

#[test]
fn test_upsert_full() {
    let mut entries: Vec<KVEntry> = (0..MAX_ENTRIES_PER_PAGE as u8)
        .map(|i| make_entry(i, i as u64 * 100))
        .collect();
    assert_eq!(entries.len(), MAX_ENTRIES_PER_PAGE);

    // Updating existing should still work
    let update = make_entry(0, 9999);
    assert!(upsert_in_page(&mut entries, update));
    assert_eq!(entries[0].offset, 9999);

    // Inserting a new entry when full should fail
    let new_entry = make_entry(200, 5000);
    assert!(!upsert_in_page(&mut entries, new_entry));
}

#[test]
fn test_page_size() {
    // BLAKE3: 32-byte hash
    // page_size = 2 + 32 * (32 + 1 + 8) = 2 + 32 * 41 = 2 + 1312 = 1314
    assert_eq!(page_size(32), 2 + 32 * (32 + 1 + 8));
    assert_eq!(page_size(32), 1314);

    // SHA-512: 64-byte hash
    // page_size = 2 + 32 * (64 + 1 + 8) = 2 + 32 * 73 = 2 + 2336 = 2338
    assert_eq!(page_size(64), 2338);
}

#[test]
fn test_stage_for_count() {
    let hl = 32;

    // Stage 0: 1024 buckets * 32 entries = 32,768 capacity
    assert_eq!(stage_for_count(0, hl), 0);
    assert_eq!(stage_for_count(100, hl), 0);
    assert_eq!(stage_for_count(32_000, hl), 0);

    // Stage 1: 4096 * 32 = 131,072
    assert_eq!(stage_for_count(32_768, hl), 1);
    assert_eq!(stage_for_count(100_000, hl), 1);

    // Stage 2: 8192 * 32 = 262,144
    assert_eq!(stage_for_count(131_072, hl), 2);

    // Very large count should return last stage
    assert_eq!(stage_for_count(100_000_000, hl), KV_STAGES.len() - 1);
}

#[test]
fn test_bucket_page_offset() {
    let hl = 32;
    let ps = page_size(hl);
    assert_eq!(bucket_page_offset(0, hl), 0);
    assert_eq!(bucket_page_offset(1, hl), ps as u64);
    assert_eq!(bucket_page_offset(10, hl), (10 * ps) as u64);
}

#[test]
fn test_deserialize_page_too_short() {
    let result = deserialize_page(&[0], 32);
    assert!(result.is_err());
}

#[test]
fn test_deserialize_page_corrupt_count() {
    // Set entry count to 255 (way more than MAX_ENTRIES_PER_PAGE)
    let mut data = vec![0u8; page_size(32)];
    data[0] = 0xFF;
    data[1] = 0x00;
    let result = deserialize_page(&data, 32);
    assert!(result.is_err());
}

#[test]
fn test_serialize_preserves_type_flags() {
    let hash_length = 32;
    let mut entry = make_entry(1, 100);
    entry.type_flags = KV_TYPE_FILE_RECORD | KV_FLAG_PENDING;

    let page = serialize_page(&[entry.clone()], hash_length);
    let result = deserialize_page(&page, hash_length).unwrap();
    assert_eq!(result[0].type_flags, entry.type_flags);
    assert!(result[0].is_pending());
    assert_eq!(result[0].entry_type(), KV_TYPE_FILE_RECORD);
}

#[test]
fn test_stage_table_monotonic() {
    // Verify stages are monotonically increasing in both size and bucket count
    for i in 1..KV_STAGES.len() {
        let (prev_size, _prev_buckets) = KV_STAGES[i - 1];
        let (cur_size, cur_buckets) = KV_STAGES[i];
        assert!(cur_size > prev_size, "Stage {} size should exceed stage {}", i, i - 1);
        // Buckets should be >= previous (stages 5 and 6 have same bucket count)
        let (_p_size, prev_bc) = KV_STAGES[i - 1];
        assert!(cur_buckets >= prev_bc, "Stage {} buckets should be >= stage {}", i, i - 1);
    }
}

// ============================================================================
// DiskKVStore tests
// ============================================================================

#[test]
fn test_create_and_open() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Create and insert
    {
        let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        store.insert(make_entry(1, 100));
        store.flush().unwrap();
    }

    // Reopen and verify
    {
        let mut store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
        let entry = store.get(&make_hash(1));
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().offset, 100);
    }
}

#[test]
fn test_insert_and_get() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    let entry = make_entry(42, 12345);
    store.insert(entry.clone());

    let result = store.get(&make_hash(42));
    assert!(result.is_some());
    let found = result.unwrap();
    assert_eq!(found.hash, entry.hash);
    assert_eq!(found.offset, 12345);
    assert_eq!(found.entry_type(), KV_TYPE_CHUNK);
}

#[test]
fn test_insert_multiple() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    for i in 0..100u8 {
        store.insert(make_entry(i, i as u64 * 100));
    }
    store.flush().unwrap();

    for i in 0..100u8 {
        let result = store.get(&make_hash(i));
        assert!(result.is_some(), "Entry {} should exist", i);
        assert_eq!(result.unwrap().offset, i as u64 * 100);
    }
}

#[test]
fn test_get_missing() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    let result = store.get(&make_hash(99));
    assert!(result.is_none());
}

#[test]
fn test_contains() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    store.insert(make_entry(1, 100));
    assert!(store.contains(&make_hash(1)));
    assert!(!store.contains(&make_hash(99)));
}

#[test]
fn test_mark_deleted() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    store.insert(make_entry(1, 100));
    assert!(store.contains(&make_hash(1)));

    store.mark_deleted(&make_hash(1));
    assert!(!store.contains(&make_hash(1)));
    assert!(store.get(&make_hash(1)).is_none());
}

#[test]
fn test_flush_persists() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Create, insert, flush
    {
        let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        store.insert(make_entry(7, 777));
        store.flush().unwrap();
    }

    // Reopen and verify
    {
        let mut store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
        let entry = store.get(&make_hash(7));
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().offset, 777);
    }
}

#[test]
fn test_auto_flush() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Insert > WRITE_BUFFER_THRESHOLD entries to trigger auto-flush
    {
        let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        // We need unique hashes, use blake3 on different data
        for i in 0..1050u32 {
            let hash = blake3::hash(&i.to_le_bytes()).as_bytes().to_vec();
            store.insert(KVEntry {
                type_flags: KV_TYPE_CHUNK,
                hash,
                offset: i as u64,
            });
        }
        // Some should have been auto-flushed already
        // Flush remaining
        store.flush().unwrap();
    }

    // Reopen and verify some entries persist
    {
        let mut store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
        // Check a few entries
        for i in [0u32, 500, 999, 1049] {
            let hash = blake3::hash(&i.to_le_bytes()).as_bytes().to_vec();
            let entry = store.get(&hash);
            assert!(entry.is_some(), "Entry {} should persist after auto-flush", i);
            assert_eq!(entry.unwrap().offset, i as u64);
        }
    }
}

#[test]
fn test_iter_all() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    for i in 0..10u8 {
        store.insert(make_entry(i, i as u64 * 10));
    }
    store.flush().unwrap();

    let all = store.iter_all().unwrap();
    assert_eq!(all.len(), 10);
}

#[test]
fn test_iter_all_with_unflushed() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    // Flush some entries to disk
    for i in 0..5u8 {
        store.insert(make_entry(i, i as u64 * 10));
    }
    store.flush().unwrap();

    // Add more to buffer (not flushed)
    for i in 5..10u8 {
        store.insert(make_entry(i, i as u64 * 10));
    }

    let all = store.iter_all().unwrap();
    assert_eq!(all.len(), 10, "iter_all should include unflushed buffer entries");
}

#[test]
fn test_upsert_same_hash() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    store.insert(make_entry(1, 100));
    store.flush().unwrap();

    // Update same hash with different offset
    let mut updated = make_entry(1, 999);
    updated.type_flags = KV_TYPE_FILE_RECORD;
    store.insert(updated);
    store.flush().unwrap();

    let result = store.get(&make_hash(1)).unwrap();
    assert_eq!(result.offset, 999);
    assert_eq!(result.entry_type(), KV_TYPE_FILE_RECORD);
}

#[test]
fn test_large_dataset() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;
    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    let count = 5000u32;
    let mut hashes = Vec::with_capacity(count as usize);

    for i in 0..count {
        let hash = blake3::hash(&i.to_le_bytes()).as_bytes().to_vec();
        hashes.push(hash.clone());
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    // Verify all entries are findable
    for (i, hash) in hashes.iter().enumerate() {
        let entry = store.get(hash);
        assert!(entry.is_some(), "Entry {} should be findable", i);
        assert_eq!(entry.unwrap().offset, i as u64);
    }
}

#[test]
fn test_entry_count() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    assert_eq!(store.len(), 0);
    assert!(store.is_empty());

    store.insert(make_entry(1, 100));
    assert_eq!(store.len(), 1);
    assert!(!store.is_empty());

    store.insert(make_entry(2, 200));
    assert_eq!(store.len(), 2);

    // Updating existing entry should not change count
    store.insert(make_entry(1, 999));
    assert_eq!(store.len(), 2);
}

#[test]
fn test_update_flags() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;
    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    store.insert(make_entry(1, 100));
    store.flush().unwrap();

    // Update flags
    let result = store.update_flags(&make_hash(1), KV_FLAG_PENDING);
    assert!(result);

    let entry = store.get(&make_hash(1)).unwrap();
    assert!(entry.is_pending());
    assert_eq!(entry.entry_type(), KV_TYPE_CHUNK); // type preserved

    // Flush and reopen to verify persistence
    store.flush().unwrap();
    drop(store);

    let mut store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
    let entry = store.get(&make_hash(1)).unwrap();
    assert!(entry.is_pending());
}

#[test]
fn test_update_offset() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    store.insert(make_entry(1, 100));

    let result = store.update_offset(&make_hash(1), 5555);
    assert!(result);

    let entry = store.get(&make_hash(1)).unwrap();
    assert_eq!(entry.offset, 5555);

    // Update non-existent entry
    let result = store.update_offset(&make_hash(99), 1234);
    assert!(!result);
}

#[test]
fn test_update_flags_missing() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    let result = store.update_flags(&make_hash(99), KV_FLAG_PENDING);
    assert!(!result);
}

#[test]
fn test_mark_deleted_missing() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    // Should not panic on missing entry
    store.mark_deleted(&make_hash(99));
    assert_eq!(store.len(), 0);
}

#[test]
fn test_mark_deleted_persists() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    {
        let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        store.insert(make_entry(1, 100));
        store.flush().unwrap();
        store.mark_deleted(&make_hash(1));
        store.flush().unwrap();
    }

    {
        let mut store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
        assert!(store.get(&make_hash(1)).is_none(), "Deleted entry should not be found after reopen");
    }
}

#[test]
fn test_iter_all_excludes_deleted() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    store.insert(make_entry(1, 100));
    store.insert(make_entry(2, 200));
    store.insert(make_entry(3, 300));
    store.flush().unwrap();

    store.mark_deleted(&make_hash(2));
    let all = store.iter_all().unwrap();
    assert_eq!(all.len(), 2, "iter_all should exclude deleted entries");
}

#[test]
fn test_create_existing_file_fails() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");

    let _store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();
    // Creating again at same path should fail
    let result = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None);
    assert!(result.is_err());
}

#[test]
fn test_open_nonexistent_fails() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("nonexistent.kv");

    let result = DiskKVStore::open(&kv_path, HashAlgorithm::Blake3_256, None);
    assert!(result.is_err());
}

#[test]
fn test_entry_count_after_reopen() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    {
        let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        for i in 0..50u8 {
            store.insert(make_entry(i, i as u64));
        }
        store.flush().unwrap();
    }

    {
        let store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
        assert_eq!(store.len(), 50);
    }
}

#[test]
fn test_cache_eviction() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();

    // Insert and flush enough entries
    for i in 0..100u32 {
        let hash = blake3::hash(&i.to_le_bytes()).as_bytes().to_vec();
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    // Access them all to fill cache
    for i in 0..100u32 {
        let hash = blake3::hash(&i.to_le_bytes()).as_bytes().to_vec();
        let _ = store.get(&hash);
    }

    // All should still be findable
    for i in 0..100u32 {
        let hash = blake3::hash(&i.to_le_bytes()).as_bytes().to_vec();
        assert!(store.get(&hash).is_some());
    }
}

#[test]
fn test_stage_accessor() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();
    assert_eq!(store.stage(), 0);
    assert_eq!(store.bucket_count(), 1024);
}

// ============================================================================
// Task 4: Startup without full entry scan
// ============================================================================

#[test]
fn test_open_existing_kv_skips_rebuild() {
    let ctx = RequestContext::system();
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();
    let kv_path = dir.path().join("test.aeordb.kv");

    // Session 1: create engine, store files, close
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        ops.store_file(&ctx, "/file1.txt", b"hello", Some("text/plain")).unwrap();
        ops.store_file(&ctx, "/file2.txt", b"world", Some("text/plain")).unwrap();
    }

    // The .kv file should exist on disk
    assert!(kv_path.exists(), ".kv file should exist after engine close");

    // Record the .kv file's modification time
    let kv_metadata_before = std::fs::metadata(&kv_path).unwrap();
    let kv_mtime_before = kv_metadata_before.modified().unwrap();

    // Session 2: reopen — .kv exists, should open without full KV rebuild
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);

        // Verify all entries still accessible
        let content1 = ops.read_file("/file1.txt").unwrap();
        assert_eq!(content1, b"hello");
        let content2 = ops.read_file("/file2.txt").unwrap();
        assert_eq!(content2, b"world");
    }

    // The .kv file's modification time should NOT have changed (no rebuild)
    let kv_metadata_after = std::fs::metadata(&kv_path).unwrap();
    let kv_mtime_after = kv_metadata_after.modified().unwrap();
    assert_eq!(
        kv_mtime_before, kv_mtime_after,
        ".kv file should not be modified when reopening with existing .kv"
    );
}

#[test]
fn test_open_missing_kv_rebuilds_from_scan() {
    let ctx = RequestContext::system();
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();
    let kv_path = dir.path().join("test.aeordb.kv");

    // Session 1: create engine, store files, close
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        ops.store_file(&ctx, "/alpha.txt", b"alpha data", Some("text/plain")).unwrap();
        ops.store_file(&ctx, "/beta.txt", b"beta data", Some("text/plain")).unwrap();
    }

    // Delete the .kv file to force a rebuild
    assert!(kv_path.exists());
    std::fs::remove_file(&kv_path).unwrap();
    assert!(!kv_path.exists());

    // Session 2: reopen — .kv missing, should rebuild from scan
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);

        // Verify all entries still accessible after rebuild
        let content1 = ops.read_file("/alpha.txt").unwrap();
        assert_eq!(content1, b"alpha data");
        let content2 = ops.read_file("/beta.txt").unwrap();
        assert_eq!(content2, b"beta data");
    }

    // .kv should have been recreated
    assert!(kv_path.exists(), ".kv file should be recreated after rebuild");
}

#[test]
fn test_cross_restart_with_disk_kv_500_files() {
    let ctx = RequestContext::system();
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();

    // Session 1: store 500 files
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();

        for i in 0..500u32 {
            let path = format!("/data/file_{:04}.txt", i);
            let content = format!("Content for file {}", i);
            ops.store_file(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
        }
    }

    // Session 2: reopen (uses existing .kv), verify all 500 readable
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);

        for i in 0..500u32 {
            let path = format!("/data/file_{:04}.txt", i);
            let expected = format!("Content for file {}", i);
            let content = ops.read_file(&path).unwrap();
            assert_eq!(content, expected.as_bytes(), "File {} mismatch after restart", i);
        }
    }
}

#[test]
fn test_stale_kv_detected_and_rebuilt() {
    let ctx = RequestContext::system();
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();
    let kv_path = dir.path().join("test.aeordb.kv");

    // Session 1: create engine with data
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        ops.store_file(&ctx, "/important.txt", b"critical data", None).unwrap();
    }

    // Simulate a stale .kv by replacing it with an empty one
    // (same stage-0 size but all zeros)
    let hash_algo = HashAlgorithm::Blake3_256;
    std::fs::remove_file(&kv_path).unwrap();
    {
        let _empty_kv = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        // Drops immediately — empty .kv with no entries
    }

    // Session 2: should detect stale .kv and rebuild
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        let content = ops.read_file("/important.txt").unwrap();
        assert_eq!(content, b"critical data", "Data should be accessible after stale .kv rebuild");
    }
}

#[test]
fn test_corrupt_kv_triggers_rebuild() {
    let ctx = RequestContext::system();
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();
    let kv_path = dir.path().join("test.aeordb.kv");

    // Session 1: create engine with data
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        ops.store_file(&ctx, "/data.txt", b"test data", None).unwrap();
    }

    // Corrupt the .kv file by truncating it
    std::fs::write(&kv_path, b"garbage").unwrap();

    // Session 2: should detect corrupt .kv and rebuild
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        let content = ops.read_file("/data.txt").unwrap();
        assert_eq!(content, b"test data", "Data should be accessible after corrupt .kv rebuild");
    }
}

#[test]
fn test_deletion_replay_on_kv_rebuild() {
    let ctx = RequestContext::system();
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();
    let kv_path = dir.path().join("test.aeordb.kv");

    // Session 1: store file, delete it, close
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        ops.ensure_root_directory(&ctx).unwrap();
        ops.store_file(&ctx, "/ephemeral.txt", b"gone soon", None).unwrap();
        ops.delete_file(&ctx, "/ephemeral.txt").unwrap();
        ops.store_file(&ctx, "/survivor.txt", b"still here", None).unwrap();
    }

    // Delete .kv to force a rebuild with deletion replay
    std::fs::remove_file(&kv_path).unwrap();

    // Session 2: deleted file should stay deleted after rebuild
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        assert!(
            ops.read_file("/ephemeral.txt").is_err(),
            "Deleted file should remain deleted after .kv rebuild"
        );
        let content = ops.read_file("/survivor.txt").unwrap();
        assert_eq!(content, b"still here");
    }
}

#[test]
fn test_empty_database_kv_reuse() {
    let dir = tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine_path_str = engine_path.to_str().unwrap();

    // Session 1: create engine with no data (just root dir)
    {
        let engine = StorageEngine::create(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.ensure_root_directory(&ctx).unwrap();
    }

    // Session 2: reopen should work cleanly
    {
        let engine = StorageEngine::open(engine_path_str).unwrap();
        let ops = DirectoryOps::new(&engine);
        let children = ops.list_directory("/").unwrap();
        assert!(children.is_empty(), "Empty database root should have no children after reopen");
    }
}

// ============================================================================
// Task 5: KV resize on overflow
// ============================================================================

/// Generate a unique hash for a given index using blake3.
fn make_unique_hash(index: u32) -> Vec<u8> {
    blake3::hash(&index.to_le_bytes()).as_bytes().to_vec()
}

#[test]
fn test_resize_on_overflow() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
    assert_eq!(store.stage(), 0);
    assert_eq!(store.bucket_count(), 1024);

    // Stage 0: 1024 buckets * 32 entries per page = 32,768 max entries.
    // Insert 35,000 entries to force at least one overflow and resize.
    let count = 35_000u32;
    let mut hashes = Vec::with_capacity(count as usize);

    for i in 0..count {
        let hash = make_unique_hash(i);
        hashes.push(hash.clone());
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64 * 100,
        });
    }
    store.flush().unwrap();

    // Verify resize happened — stage should have increased
    assert!(
        store.stage() >= 1,
        "Store should have resized to at least stage 1, got stage {}",
        store.stage()
    );

    // Verify all entries are findable
    for (i, hash) in hashes.iter().enumerate() {
        let entry = store.get(hash);
        assert!(
            entry.is_some(),
            "Entry {} should be findable after resize",
            i
        );
        assert_eq!(entry.unwrap().offset, i as u64 * 100);
    }
}

#[test]
fn test_resize_preserves_all_entries() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    let count = 5000u32;
    let mut hashes = Vec::with_capacity(count as usize);

    for i in 0..count {
        let hash = make_unique_hash(i);
        hashes.push(hash.clone());
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    // Even if resize was triggered by auto-flush, all 5000 should be findable
    for (i, hash) in hashes.iter().enumerate() {
        let entry = store.get(hash);
        assert!(entry.is_some(), "Entry {} should be preserved", i);
        assert_eq!(entry.unwrap().offset, i as u64);
    }
    assert_eq!(store.len(), count as usize);
}

#[test]
fn test_resize_stage_increases() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
    assert_eq!(store.stage(), 0);

    // Force overflow: stage 0 has 1024 buckets * 32 = 32,768 capacity.
    // The pigeonhole principle means some buckets may fill before the
    // theoretical max. Insert enough to guarantee overflow.
    for i in 0..35_000u32 {
        let hash = make_unique_hash(i);
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    assert!(
        store.stage() >= 1,
        "Stage should increase after overflow, got {}",
        store.stage()
    );
    assert!(
        store.bucket_count() > 1024,
        "Bucket count should increase after resize, got {}",
        store.bucket_count()
    );
}

#[test]
fn test_resize_persists_across_reopen() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let count = 35_000u32;
    let mut hashes = Vec::with_capacity(count as usize);

    // Create, fill to overflow, close
    {
        let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
        for i in 0..count {
            let hash = make_unique_hash(i);
            hashes.push(hash.clone());
            store.insert(KVEntry {
                type_flags: KV_TYPE_CHUNK,
                hash,
                offset: i as u64,
            });
        }
        store.flush().unwrap();
        assert!(store.stage() >= 1);
    }

    // Reopen and verify
    {
        let mut store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
        assert!(
            store.stage() >= 1,
            "Stage should persist across reopen, got {}",
            store.stage()
        );

        // Verify all entries findable after reopen
        for (i, hash) in hashes.iter().enumerate() {
            let entry = store.get(hash);
            assert!(
                entry.is_some(),
                "Entry {} should be findable after reopen of resized store",
                i
            );
        }
    }
}

#[test]
fn test_flush_after_resize_works() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    // Trigger resize
    for i in 0..35_000u32 {
        let hash = make_unique_hash(i);
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    let stage_after_resize = store.stage();
    assert!(stage_after_resize >= 1);

    // Now insert more entries after resize
    let extra_hashes: Vec<Vec<u8>> = (100_000..100_500u32)
        .map(|i| make_unique_hash(i))
        .collect();

    for (i, hash) in extra_hashes.iter().enumerate() {
        store.insert(KVEntry {
            type_flags: KV_TYPE_FILE_RECORD,
            hash: hash.clone(),
            offset: (i as u64 + 1_000_000),
        });
    }
    store.flush().unwrap();

    // Verify new entries are findable
    for (i, hash) in extra_hashes.iter().enumerate() {
        let entry = store.get(hash);
        assert!(entry.is_some(), "Post-resize entry {} should be findable", i);
        let found = entry.unwrap();
        assert_eq!(found.offset, i as u64 + 1_000_000);
        assert_eq!(found.entry_type(), KV_TYPE_FILE_RECORD);
    }

    // Stage should not have changed (500 more entries shouldn't trigger another resize)
    assert_eq!(store.stage(), stage_after_resize);
}

#[test]
fn test_create_at_stage() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Create directly at stage 2
    let mut store = DiskKVStore::create_at_stage(&kv_path, hash_algo, 2).unwrap();
    assert_eq!(store.stage(), 2);
    assert_eq!(store.bucket_count(), KV_STAGES[2].1);

    // Insert and verify
    let hash = make_unique_hash(42);
    store.insert(KVEntry {
        type_flags: KV_TYPE_CHUNK,
        hash: hash.clone(),
        offset: 999,
    });
    store.flush().unwrap();

    let entry = store.get(&hash).unwrap();
    assert_eq!(entry.offset, 999);
}

#[test]
fn test_create_at_stage_clamps_to_max() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Request a stage beyond the max — should clamp
    let store = DiskKVStore::create_at_stage(&kv_path, hash_algo, 999).unwrap();
    assert_eq!(store.stage(), KV_STAGES.len() - 1);
}

#[test]
fn test_resize_at_max_stage_returns_error() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let last_stage = KV_STAGES.len() - 1;
    let mut store = DiskKVStore::create_at_stage(&kv_path, hash_algo, last_stage).unwrap();
    assert_eq!(store.stage(), last_stage);

    // Attempting to resize when already at max stage should error
    let result = store.resize_to_next_stage();
    assert!(result.is_err(), "Resize at max stage should return an error");
}

#[test]
fn test_deleted_entries_not_migrated_on_resize() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    // Insert entries, delete some
    for i in 0..100u32 {
        let hash = make_unique_hash(i);
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    // Delete entries 0-49
    for i in 0..50u32 {
        let hash = make_unique_hash(i);
        store.mark_deleted(&hash);
    }
    store.flush().unwrap();

    let count_before = store.len();
    assert_eq!(count_before, 50, "Should have 50 non-deleted entries");

    // Trigger resize
    for i in 1000..35_000u32 {
        let hash = make_unique_hash(i);
        store.insert(KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64,
        });
    }
    store.flush().unwrap();

    // Deleted entries should NOT be in the new store
    for i in 0..50u32 {
        let hash = make_unique_hash(i);
        assert!(
            store.get(&hash).is_none(),
            "Deleted entry {} should not exist after resize",
            i
        );
    }

    // Non-deleted entries should still exist
    for i in 50..100u32 {
        let hash = make_unique_hash(i);
        assert!(
            store.get(&hash).is_some(),
            "Non-deleted entry {} should survive resize",
            i
        );
    }
}

// ============================================================================
// Hot file (write-ahead journal) tests
// ============================================================================

#[test]
fn test_hot_file_created_on_insert() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();

    // Hot file should exist after creation
    let hot_path = store.hot_path().unwrap().to_path_buf();
    assert!(hot_path.exists(), "Hot file should exist after init");

    // Insert one entry
    store.insert(make_entry(1, 100));

    // Hot buffer should have the entry (not yet flushed to file since < threshold)
    assert_eq!(store.hot_buffer_len(), 1);

    // Force flush the hot buffer
    store.flush_hot_buffer().unwrap();
    assert_eq!(store.hot_buffer_len(), 0);

    // Hot file should have data now
    let metadata = std::fs::metadata(&hot_path).unwrap();
    assert!(metadata.len() > 0, "Hot file should have data after flush");
}

#[test]
fn test_hot_file_contains_entries() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    let hot_path = store.hot_path().unwrap().to_path_buf();

    // Insert 5 entries
    for i in 0..5u8 {
        store.insert(make_entry(i, (i as u64) * 100));
    }

    // Force flush hot buffer to file
    store.flush_hot_buffer().unwrap();

    // Read back the hot file
    let hash_length = hash_algo.hash_length();
    let entries = DiskKVStore::read_hot_file(&hot_path, hash_length).unwrap();
    assert_eq!(entries.len(), 5, "Hot file should contain 5 entries");

    // Verify entries match
    for (i, entry) in entries.iter().enumerate() {
        let expected = make_entry(i as u8, (i as u64) * 100);
        assert_eq!(entry.hash, expected.hash, "Hash mismatch at index {}", i);
        assert_eq!(entry.type_flags, expected.type_flags, "Flags mismatch at index {}", i);
        assert_eq!(entry.offset, expected.offset, "Offset mismatch at index {}", i);
    }
}

#[test]
fn test_hot_file_truncated_on_flush() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    let hot_path = store.hot_path().unwrap().to_path_buf();

    // Insert entries
    for i in 0..5u8 {
        store.insert(make_entry(i, (i as u64) * 100));
    }

    // Force hot buffer flush so file has data
    store.flush_hot_buffer().unwrap();
    let size_before = std::fs::metadata(&hot_path).unwrap().len();
    assert!(size_before > 0, "Hot file should have data before KV flush");

    // Now flush the KV store — this should truncate the hot file
    store.flush().unwrap();

    let size_after = std::fs::metadata(&hot_path).unwrap().len();
    assert_eq!(size_after, 0, "Hot file should be empty after KV flush");
}

#[test]
fn test_hot_file_replay_on_open() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("replay_test.aeordb");
    let db_str = db_path.to_str().unwrap();
    let hot_dir = dir.path();

    // Create a StorageEngine with hot dir
    {
        let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ctx = RequestContext::system();
        let directory_ops = DirectoryOps::new(&engine);
        directory_ops.ensure_root_directory(&ctx).unwrap();

        // Store some entries (they go to the KV store's write buffer and hot file)
        for i in 0..50u8 {
            let key = engine.compute_hash(&[i]).unwrap();
            engine.store_entry(
                aeordb::engine::entry_type::EntryType::Chunk,
                &key,
                &vec![i; 64],
            ).unwrap();
        }

        // DON'T flush — simulate crash by dropping without explicit flush
        // The write buffer gets flushed by Drop, but the hot file has entries
        // Note: We need to prevent Drop from flushing to simulate a true crash.
        // Instead, let's verify the hot file mechanism by writing entries to a
        // manual hot file after the engine is dropped.
    }

    // Manually create a hot file with known entries for replay testing
    let hash_algo = HashAlgorithm::Blake3_256;
    let hash_length = hash_algo.hash_length();

    // Create entries to replay
    let replay_entries: Vec<KVEntry> = (200..210u8).map(|i| {
        let hash = blake3::hash(&[i]).as_bytes().to_vec();
        KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash,
            offset: i as u64 * 1000,
        }
    }).collect();

    // Write a hot file manually
    let hot_file_path = dir.path().join("replay_test-hot001");
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&hot_file_path)
            .unwrap();
        use std::io::Write;
        for entry in &replay_entries {
            let mut buf = Vec::with_capacity(hash_length + 1 + 8);
            buf.extend_from_slice(&entry.hash[..hash_length]);
            buf.push(entry.type_flags);
            buf.extend_from_slice(&entry.offset.to_le_bytes());
            file.write_all(&buf).unwrap();
        }
        file.sync_all().unwrap();
    }

    assert!(hot_file_path.exists(), "Hot file should exist before replay");

    // Open with hot_dir — should replay the hot file entries
    let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();

    // After replay, the old hot file is deleted but a new empty hot file is
    // created at the same path for the new session. Verify the file is empty
    // (replayed data was consumed) and entries are in the KV store.
    let hot_file_size = std::fs::metadata(&hot_file_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(hot_file_size, 0, "Hot file should be empty after replay (old data consumed, new empty file created)");

    // Verify replay entries are in the KV store
    for entry in &replay_entries {
        let result = engine.has_entry(&entry.hash).unwrap();
        assert!(result, "Replayed entry should exist in KV store");
    }
}

#[test]
fn test_hot_file_deleted_after_replay() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("delete_test.aeordb");
    let db_str = db_path.to_str().unwrap();
    let hot_dir = dir.path();

    // Create database first
    {
        let _engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
    }

    // Create a hot file with one entry
    let hash_algo = HashAlgorithm::Blake3_256;
    let hash_length = hash_algo.hash_length();
    let hot_file_path = dir.path().join("delete_test-hot001");
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&hot_file_path)
            .unwrap();
        use std::io::Write;
        let hash = blake3::hash(&[42u8]).as_bytes().to_vec();
        let mut buf = Vec::with_capacity(hash_length + 1 + 8);
        buf.extend_from_slice(&hash[..hash_length]);
        buf.push(KV_TYPE_CHUNK);
        buf.extend_from_slice(&1000u64.to_le_bytes());
        file.write_all(&buf).unwrap();
    }

    assert!(hot_file_path.exists());

    // Open — should replay and delete
    let _engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();

    // After replay, the old hot file is deleted but a new empty hot file is
    // created at the same path for the new session.
    let hot_file_size = std::fs::metadata(&hot_file_path)
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(hot_file_size, 0, "Hot file should be empty after replay");
}

#[test]
fn test_no_hot_file_when_disabled() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Create with hot_dir = None
    let store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    assert!(store.hot_path().is_none(), "No hot path when disabled");
    assert_eq!(store.hot_buffer_len(), 0);
}

#[test]
fn test_hot_buffer_threshold_auto_flush() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    let hot_path = store.hot_path().unwrap().to_path_buf();

    // Insert 9 entries — should stay in hot buffer
    for i in 0..9u8 {
        store.insert(make_entry(i, (i as u64) * 100));
    }
    assert_eq!(store.hot_buffer_len(), 9, "Should have 9 entries in buffer");
    let hot_size = std::fs::metadata(&hot_path).unwrap().len();
    assert_eq!(hot_size, 0, "Hot file should be empty before threshold");

    // Insert 10th entry — should trigger auto-flush of hot buffer
    store.insert(make_entry(9, 900));
    assert_eq!(store.hot_buffer_len(), 0, "Buffer should be empty after auto-flush");

    let hot_size = std::fs::metadata(&hot_path).unwrap().len();
    let hash_length = hash_algo.hash_length();
    let entry_size = hash_length + 1 + 8;
    assert_eq!(hot_size as usize, 10 * entry_size, "Hot file should have 10 entries");
}

#[test]
fn test_hot_file_survives_no_flush() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;
    let hash_length = hash_algo.hash_length();

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    let hot_path = store.hot_path().unwrap().to_path_buf();

    // Insert enough to trigger hot buffer flush (threshold=10), but not KV flush (threshold=1000)
    for i in 0..15u8 {
        store.insert(make_entry(i, (i as u64) * 100));
    }

    // Force-flush remaining buffer entries to hot file
    store.flush_hot_buffer().unwrap();

    // Verify hot file has entries BEFORE any KV flush
    let entries = DiskKVStore::read_hot_file(&hot_path, hash_length).unwrap();
    assert_eq!(entries.len(), 15, "Hot file should have 15 entries before KV flush");

    // Now simulate what would survive a crash: use std::mem::forget to skip Drop
    // (which would call flush and truncate the hot file).
    std::mem::forget(store);

    // Hot file should still have data since Drop was skipped
    let entries_after = DiskKVStore::read_hot_file(&hot_path, hash_length).unwrap();
    assert_eq!(entries_after.len(), 15, "Hot file should survive when Drop is skipped (simulated crash)");
}

#[test]
fn test_read_hot_file_empty() {
    let dir = tempdir().unwrap();
    let hot_path = dir.path().join("empty-hot001");
    std::fs::write(&hot_path, b"").unwrap();

    let entries = DiskKVStore::read_hot_file(&hot_path, 32).unwrap();
    assert!(entries.is_empty(), "Reading empty hot file should return no entries");
}

#[test]
fn test_read_hot_file_truncated_entry() {
    let dir = tempdir().unwrap();
    let hot_path = dir.path().join("truncated-hot001");
    let hash_length = 32;
    let _entry_size = hash_length + 1 + 8; // 41 bytes

    // Write 2 full entries + 10 bytes of a truncated third entry
    let mut data = Vec::new();
    for i in 0..2u8 {
        let hash = blake3::hash(&[i]).as_bytes().to_vec();
        data.extend_from_slice(&hash[..hash_length]);
        data.push(KV_TYPE_CHUNK);
        data.extend_from_slice(&((i as u64) * 100).to_le_bytes());
    }
    // Add partial entry (only 10 bytes)
    data.extend_from_slice(&[0u8; 10]);

    std::fs::write(&hot_path, &data).unwrap();

    let entries = DiskKVStore::read_hot_file(&hot_path, hash_length).unwrap();
    assert_eq!(entries.len(), 2, "Should only read 2 complete entries, skipping truncated tail");
}

#[test]
fn test_read_hot_file_nonexistent() {
    let dir = tempdir().unwrap();
    let hot_path = dir.path().join("nonexistent-hot001");

    let result = DiskKVStore::read_hot_file(&hot_path, 32);
    assert!(result.is_err(), "Reading nonexistent hot file should error");
}

#[test]
fn test_hot_file_insert_without_hot_file_does_not_crash() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    let mut store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

    // Insert many entries — should work fine without hot file
    for i in 0..50u8 {
        store.insert(make_entry(i, (i as u64) * 100));
    }

    // Flush should also work fine
    store.flush().unwrap();

    assert_eq!(store.hot_buffer_len(), 0);
}

#[test]
fn test_hot_file_open_with_hot_dir() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;

    // Create and close
    {
        let store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
        let hot_path = store.hot_path().unwrap().to_path_buf();
        assert!(hot_path.exists());
    }

    // Open with hot_dir
    let store = DiskKVStore::open(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    assert!(store.hot_path().is_some(), "Hot path should be set after open with hot_dir");
    assert!(store.hot_path().unwrap().exists(), "Hot file should exist after open");
}

#[test]
fn test_hot_file_open_without_hot_dir() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hash_algo = HashAlgorithm::Blake3_256;

    // Create (without hot dir) and close
    {
        let _store = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();
    }

    // Open without hot_dir
    let store = DiskKVStore::open(&kv_path, hash_algo, None).unwrap();
    assert!(store.hot_path().is_none(), "No hot path when opened without hot_dir");
}

#[test]
fn test_hot_file_multiple_flush_cycles() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;
    let hash_length = hash_algo.hash_length();

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    let hot_path = store.hot_path().unwrap().to_path_buf();

    // Cycle 1: insert, flush hot, verify, flush KV
    for i in 0..5u8 {
        store.insert(make_entry(i, (i as u64) * 100));
    }
    store.flush_hot_buffer().unwrap();
    let entries = DiskKVStore::read_hot_file(&hot_path, hash_length).unwrap();
    assert_eq!(entries.len(), 5);

    store.flush().unwrap();
    let hot_size = std::fs::metadata(&hot_path).unwrap().len();
    assert_eq!(hot_size, 0, "Hot file should be empty after KV flush");

    // Cycle 2: insert more, flush hot, verify
    for i in 10..18u8 {
        store.insert(make_entry(i, (i as u64) * 200));
    }
    store.flush_hot_buffer().unwrap();
    let entries = DiskKVStore::read_hot_file(&hot_path, hash_length).unwrap();
    assert_eq!(entries.len(), 8, "Cycle 2 should have 8 entries");

    store.flush().unwrap();
    let hot_size = std::fs::metadata(&hot_path).unwrap().len();
    assert_eq!(hot_size, 0, "Hot file should be empty after second KV flush");
}

#[test]
fn test_hot_file_entry_size_correctness() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("test.kv");
    let hot_dir = dir.path();
    let hash_algo = HashAlgorithm::Blake3_256;
    let hash_length = hash_algo.hash_length();
    let expected_entry_size = hash_length + 1 + 8; // 32 + 1 + 8 = 41

    let mut store = DiskKVStore::create(&kv_path, hash_algo, Some(hot_dir)).unwrap();
    let hot_path = store.hot_path().unwrap().to_path_buf();

    // Insert exactly 1 entry and flush to hot file
    store.insert(make_entry(42, 9999));
    store.flush_hot_buffer().unwrap();

    let file_size = std::fs::metadata(&hot_path).unwrap().len();
    assert_eq!(
        file_size as usize, expected_entry_size,
        "Single entry should be exactly {} bytes (hash_length={} + 1 + 8)",
        expected_entry_size, hash_length
    );
}
