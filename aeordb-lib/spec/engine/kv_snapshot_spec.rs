use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use aeordb::engine::disk_kv_store::DiskKVStore;
use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::kv_pages::{bucket_page_offset, page_size};
use aeordb::engine::kv_snapshot::ReadSnapshot;
use aeordb::engine::kv_store::{KVEntry, KV_TYPE_CHUNK, KV_FLAG_DELETED};
use aeordb::engine::nvt::NormalizedVectorTable;
use aeordb::engine::scalar_converter::HashConverter;
use tempfile::tempdir;

// ============================================================================
// Helpers
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

fn make_deleted_entry(seed: u8, offset: u64) -> KVEntry {
    KVEntry {
        type_flags: KV_TYPE_CHUNK | KV_FLAG_DELETED,
        hash: make_hash(seed),
        offset,
    }
}

/// Read all pages from a KV file into memory.
fn read_pages_from_kv(kv_path: &std::path::Path, bucket_count: usize, hash_length: usize) -> Vec<Vec<u8>> {
    let mut file = File::open(kv_path).unwrap();
    let psize = page_size(hash_length);
    let mut pages = Vec::with_capacity(bucket_count);
    for bucket in 0..bucket_count {
        let offset = bucket_page_offset(bucket, hash_length);
        let mut page_data = vec![0u8; psize];
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact(&mut page_data).unwrap();
        pages.push(page_data);
    }
    pages
}

/// Create a DiskKVStore with entries flushed to disk, then return
/// the store's bucket_count and in-memory pages.
fn create_flushed_store(
    dir: &std::path::Path,
    entries: &[KVEntry],
) -> (usize, Arc<Vec<Vec<u8>>>) {
    let kv_path = dir.join("test.kv");
    let mut store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();
    for entry in entries {
        store.insert(entry.clone());
    }
    store.flush().unwrap();
    let bucket_count = store.bucket_count();
    let hash_length = HashAlgorithm::Blake3_256.hash_length();
    let pages = read_pages_from_kv(&kv_path, bucket_count, hash_length);
    (bucket_count, Arc::new(pages))
}

fn make_nvt(bucket_count: usize) -> Arc<NormalizedVectorTable> {
    Arc::new(NormalizedVectorTable::new(
        Box::new(HashConverter),
        bucket_count,
    ))
}

/// Create empty pages for a given bucket count and hash length.
fn empty_pages(bucket_count: usize, hash_length: usize) -> Arc<Vec<Vec<u8>>> {
    Arc::new(vec![vec![0u8; page_size(hash_length)]; bucket_count])
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn test_snapshot_get_finds_entry_in_buffer() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("empty.kv");
    // Create an empty store just so we have a valid .kv file
    let store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();
    let bucket_count = store.bucket_count();
    drop(store);

    let entry = make_entry(42, 12345);
    let mut buffer = HashMap::new();
    buffer.insert(entry.hash.clone(), entry.clone());

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        1,
        empty_pages(bucket_count, 32),
    );

    let result = snap.get(&make_hash(42));
    assert!(result.is_some());
    assert_eq!(result.unwrap().offset, 12345);
}

#[test]
fn test_snapshot_get_returns_none_for_deleted_in_buffer() {
    let dir = tempdir().unwrap();
    let kv_path = dir.path().join("empty.kv");
    let store = DiskKVStore::create(&kv_path, HashAlgorithm::Blake3_256, None).unwrap();
    let bucket_count = store.bucket_count();
    drop(store);

    let entry = make_deleted_entry(42, 12345);
    let mut buffer = HashMap::new();
    buffer.insert(entry.hash.clone(), entry.clone());

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        0,
        empty_pages(bucket_count, 32),
    );

    let result = snap.get(&make_hash(42));
    assert!(result.is_none(), "Deleted entry in buffer should return None from get()");

    // But get_raw should still find it
    let raw = snap.get_raw(&make_hash(42));
    assert!(raw.is_some(), "get_raw should return deleted entries");
    assert!(raw.unwrap().is_deleted());

    // And is_deleted_in_buffer should confirm it
    assert!(snap.is_deleted_in_buffer(&make_hash(42)));
}

#[test]
fn test_snapshot_get_falls_through_to_disk() {
    let dir = tempdir().unwrap();
    let entries = vec![make_entry(1, 100), make_entry(2, 200), make_entry(3, 300)];
    let (bucket_count, pages) = create_flushed_store(dir.path(), &entries);

    // Empty buffer — all lookups must hit pages
    let snap = ReadSnapshot::new(
        HashMap::new(),
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        3,
        pages,
    );

    let r1 = snap.get(&make_hash(1));
    assert!(r1.is_some());
    assert_eq!(r1.unwrap().offset, 100);

    let r2 = snap.get(&make_hash(2));
    assert!(r2.is_some());
    assert_eq!(r2.unwrap().offset, 200);

    let r3 = snap.get(&make_hash(3));
    assert!(r3.is_some());
    assert_eq!(r3.unwrap().offset, 300);
}

#[test]
fn test_snapshot_get_returns_none_for_missing() {
    let dir = tempdir().unwrap();
    let entries = vec![make_entry(1, 100)];
    let (bucket_count, pages) = create_flushed_store(dir.path(), &entries);

    let snap = ReadSnapshot::new(
        HashMap::new(),
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        1,
        pages,
    );

    // Hash 99 was never inserted
    let result = snap.get(&make_hash(99));
    assert!(result.is_none(), "Missing hash should return None");

    // Also check buffer miss for a hash not in buffer or disk
    let result2 = snap.get(&make_hash(200));
    assert!(result2.is_none());
}

#[test]
fn test_snapshot_buffer_wins_over_disk() {
    let dir = tempdir().unwrap();

    // Put entry with seed=10, offset=1000 on disk
    let disk_entry = make_entry(10, 1000);
    let (bucket_count, pages) = create_flushed_store(dir.path(), &[disk_entry]);

    // Put entry with same hash but different offset in buffer
    let buffer_entry = make_entry(10, 9999);
    let mut buffer = HashMap::new();
    buffer.insert(buffer_entry.hash.clone(), buffer_entry.clone());

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        1,
        pages,
    );

    let result = snap.get(&make_hash(10));
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().offset, 9999,
        "Buffer entry should win over disk entry"
    );
}

#[test]
fn test_snapshot_iter_all_merges_buffer_and_disk() {
    let dir = tempdir().unwrap();

    // Disk: entries 1, 2, 3
    let disk_entries = vec![make_entry(1, 100), make_entry(2, 200), make_entry(3, 300)];
    let (bucket_count, pages) = create_flushed_store(dir.path(), &disk_entries);

    // Buffer: entry 4 (new) and entry 2 with updated offset
    let mut buffer = HashMap::new();
    let new_entry = make_entry(4, 400);
    let updated_entry = make_entry(2, 2222);
    buffer.insert(new_entry.hash.clone(), new_entry);
    buffer.insert(updated_entry.hash.clone(), updated_entry);

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        4,
        pages,
    );

    let all = snap.iter_all().unwrap();
    assert_eq!(all.len(), 4, "Should have 4 unique entries (3 disk + 1 new, with 1 overlap)");

    // Verify the buffer version of entry 2 won
    let entry2 = all.iter().find(|e| e.hash == make_hash(2)).unwrap();
    assert_eq!(entry2.offset, 2222, "Buffer version should override disk version");

    // Verify entry 4 is present
    let entry4 = all.iter().find(|e| e.hash == make_hash(4));
    assert!(entry4.is_some(), "New buffer entry should appear in iter_all");
}

#[test]
fn test_snapshot_iter_all_excludes_deleted() {
    let dir = tempdir().unwrap();

    // Disk: entries 1, 2
    let disk_entries = vec![make_entry(1, 100), make_entry(2, 200)];
    let (bucket_count, pages) = create_flushed_store(dir.path(), &disk_entries);

    // Buffer: delete entry 2 via tombstone
    let mut buffer = HashMap::new();
    let tombstone = make_deleted_entry(2, 200);
    buffer.insert(tombstone.hash.clone(), tombstone);

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        1,
        pages,
    );

    let all = snap.iter_all().unwrap();
    assert_eq!(all.len(), 1, "Deleted entry should be excluded from iter_all");

    // Only entry 1 should remain
    assert_eq!(all[0].hash, make_hash(1));
    assert_eq!(all[0].offset, 100);

    // Confirm the tombstone is visible through is_deleted_in_buffer
    assert!(snap.is_deleted_in_buffer(&make_hash(2)));
}

#[test]
fn test_snapshot_get_concurrent_file_handles() {
    let dir = tempdir().unwrap();

    let disk_entries = vec![
        make_entry(10, 1000),
        make_entry(20, 2000),
        make_entry(30, 3000),
    ];
    let (bucket_count, pages) = create_flushed_store(dir.path(), &disk_entries);

    let snap = ReadSnapshot::new(
        HashMap::new(),
        make_nvt(bucket_count),
        bucket_count,
        HashAlgorithm::Blake3_256,
        3,
        pages,
    );

    // Multiple sequential get() calls — all served from in-memory pages.
    for _ in 0..5 {
        let r10 = snap.get(&make_hash(10));
        assert!(r10.is_some());
        assert_eq!(r10.unwrap().offset, 1000);

        let r20 = snap.get(&make_hash(20));
        assert!(r20.is_some());
        assert_eq!(r20.unwrap().offset, 2000);

        let r30 = snap.get(&make_hash(30));
        assert!(r30.is_some());
        assert_eq!(r30.unwrap().offset, 3000);
    }
}

// ============================================================================
// Accessor tests
// ============================================================================

#[test]
fn test_snapshot_accessors() {
    let snap = ReadSnapshot::new(
        HashMap::new(),
        make_nvt(1024),
        1024,
        HashAlgorithm::Blake3_256,
        0,
        empty_pages(1024, 32),
    );

    assert_eq!(snap.len(), 0);
    assert!(snap.is_empty());
    assert_eq!(snap.bucket_count(), 1024);
    assert_eq!(snap.hash_algo(), HashAlgorithm::Blake3_256);
    assert_eq!(snap.buffer_len(), 0);
}

#[test]
fn test_snapshot_accessors_with_data() {
    let mut buffer = HashMap::new();
    let e1 = make_entry(1, 100);
    let e2 = make_entry(2, 200);
    buffer.insert(e1.hash.clone(), e1);
    buffer.insert(e2.hash.clone(), e2);

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(2048),
        2048,
        HashAlgorithm::Blake3_256,
        5,
        empty_pages(2048, 32),
    );

    assert_eq!(snap.len(), 5);
    assert!(!snap.is_empty());
    assert_eq!(snap.bucket_count(), 2048);
    assert_eq!(snap.buffer_len(), 2);
}

#[test]
fn test_snapshot_is_deleted_in_buffer_false_for_missing() {
    let snap = ReadSnapshot::new(
        HashMap::new(),
        make_nvt(1024),
        1024,
        HashAlgorithm::Blake3_256,
        0,
        empty_pages(1024, 32),
    );

    assert!(!snap.is_deleted_in_buffer(&make_hash(99)));
}

#[test]
fn test_snapshot_is_deleted_in_buffer_false_for_live_entry() {
    let mut buffer = HashMap::new();
    let entry = make_entry(5, 500);
    buffer.insert(entry.hash.clone(), entry);

    let snap = ReadSnapshot::new(
        buffer,
        make_nvt(1024),
        1024,
        HashAlgorithm::Blake3_256,
        1,
        empty_pages(1024, 32),
    );

    assert!(
        !snap.is_deleted_in_buffer(&make_hash(5)),
        "Live entry should not be reported as deleted"
    );
}
