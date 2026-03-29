use std::sync::Arc;

use aeordb::filesystem::{ChunkList, Directory, EntryType, IndexEntry};
use aeordb::storage::{ChunkConfig, ChunkStorage, InMemoryChunkStorage, hash_data};
use chrono::Utc;
use uuid::Uuid;

fn setup() -> (Directory, Arc<dyn ChunkStorage>) {
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let chunk_config = ChunkConfig::default();
  let directory = Directory::with_minimum_degree(storage.clone(), chunk_config, 3);
  (directory, storage)
}

fn setup_with_degree(minimum_degree: usize) -> (Directory, Arc<dyn ChunkStorage>) {
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let chunk_config = ChunkConfig::default();
  let directory = Directory::with_minimum_degree(storage.clone(), chunk_config, minimum_degree);
  (directory, storage)
}

fn make_entry(name: &str) -> IndexEntry {
  IndexEntry {
    name: name.to_string(),
    entry_type: EntryType::File,
    chunk_list: ChunkList::Inline(vec![hash_data(name.as_bytes())]),
    document_id: Uuid::new_v4(),
    created_at: Utc::now(),
    updated_at: Utc::now(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: 256,
  }
}

fn make_directory_entry(name: &str) -> IndexEntry {
  IndexEntry {
    name: name.to_string(),
    entry_type: EntryType::Directory,
    chunk_list: ChunkList::Inline(vec![hash_data(b"child-root")]),
    document_id: Uuid::new_v4(),
    created_at: Utc::now(),
    updated_at: Utc::now(),
    content_type: None,
    total_size: 0,
  }
}

fn make_hard_link_entry(name: &str) -> IndexEntry {
  IndexEntry {
    name: name.to_string(),
    entry_type: EntryType::HardLink,
    chunk_list: ChunkList::Inline(vec![hash_data(b"shared-data")]),
    document_id: Uuid::new_v4(),
    created_at: Utc::now(),
    updated_at: Utc::now(),
    content_type: Some("text/plain".to_string()),
    total_size: 128,
  }
}

// ─── Basic operations ───────────────────────────────────────────────────────

#[test]
fn test_create_empty_directory() {
  let (directory, storage) = setup();
  let root = directory.create_empty().expect("create_empty should succeed");
  assert!(storage.has_chunk(&root).unwrap(), "root chunk should exist in storage");
}

#[test]
fn test_insert_and_get_entry() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let entry = make_entry("hello.txt");
  let document_id = entry.document_id;

  let new_root = directory.insert(&root, entry).unwrap();
  let retrieved = directory.get(&new_root, "hello.txt").unwrap();

  assert!(retrieved.is_some(), "entry should be found");
  let retrieved = retrieved.unwrap();
  assert_eq!(retrieved.name, "hello.txt");
  assert_eq!(retrieved.document_id, document_id);
}

#[test]
fn test_insert_multiple_entries_sorted() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  let names = vec!["charlie", "alpha", "bravo", "delta"];
  for name in &names {
    root = directory.insert(&root, make_entry(name)).unwrap();
  }

  let entries = directory.list(&root).unwrap();
  let entry_names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
  assert_eq!(entry_names, vec!["alpha", "bravo", "charlie", "delta"]);
}

#[test]
fn test_get_nonexistent_returns_none() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let result = directory.get(&root, "does_not_exist").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_remove_entry() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();
  root = directory.insert(&root, make_entry("removeme.txt")).unwrap();
  root = directory.insert(&root, make_entry("keepme.txt")).unwrap();

  let (new_root, removed) = directory.remove(&root, "removeme.txt").unwrap();
  assert!(removed.is_some());
  assert_eq!(removed.unwrap().name, "removeme.txt");

  let gone = directory.get(&new_root, "removeme.txt").unwrap();
  assert!(gone.is_none());

  let kept = directory.get(&new_root, "keepme.txt").unwrap();
  assert!(kept.is_some());
}

#[test]
fn test_remove_nonexistent_returns_none_in_tuple() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root = directory.insert(&root, make_entry("exists.txt")).unwrap();

  let (new_root, removed) = directory.remove(&root, "nope.txt").unwrap();
  assert!(removed.is_none());
  // The tree should still be valid.
  let entry = directory.get(&new_root, "exists.txt").unwrap();
  assert!(entry.is_some());
}

#[test]
fn test_list_entries_returns_all() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();
  for i in 0..5 {
    root = directory.insert(&root, make_entry(&format!("file_{i:03}"))).unwrap();
  }

  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 5);
}

#[test]
fn test_list_entries_sorted_lexicographically() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  let names = vec!["zulu", "alpha", "mike", "foxtrot"];
  for name in &names {
    root = directory.insert(&root, make_entry(name)).unwrap();
  }

  let entries = directory.list(&root).unwrap();
  let entry_names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
  assert_eq!(entry_names, vec!["alpha", "foxtrot", "mike", "zulu"]);
}

#[test]
fn test_list_range() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  for name in &["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"] {
    root = directory.insert(&root, make_entry(name)).unwrap();
  }

  let range_entries = directory.list_range(&root, "bravo", "echo").unwrap();
  let names: Vec<&str> = range_entries.iter().map(|entry| entry.name.as_str()).collect();
  assert_eq!(names, vec!["bravo", "charlie", "delta"]);
}

#[test]
fn test_count_entries() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  for i in 0..7 {
    root = directory.insert(&root, make_entry(&format!("entry_{i}"))).unwrap();
  }

  let count = directory.count(&root).unwrap();
  assert_eq!(count, 7);
}

// ─── Node splits and merges ─────────────────────────────────────────────────

#[test]
fn test_insert_many_entries_causes_node_split() {
  // Use small minimum degree to trigger splits quickly.
  let (directory, _storage) = setup_with_degree(3);
  let mut root = directory.create_empty().unwrap();

  for i in 0..100 {
    root = directory.insert(&root, make_entry(&format!("entry_{i:04}"))).unwrap();
  }

  // Verify all entries are still retrievable.
  for i in 0..100 {
    let name = format!("entry_{i:04}");
    let result = directory.get(&root, &name).unwrap();
    assert!(result.is_some(), "entry {name} should exist after mass insert");
  }

  // Verify they're sorted.
  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 100);
  for window in entries.windows(2) {
    assert!(
      window[0].name < window[1].name,
      "entries should be sorted: {} should be before {}",
      window[0].name,
      window[1].name,
    );
  }
}

#[test]
fn test_remove_entries_causes_node_merge() {
  let (directory, _storage) = setup_with_degree(3);
  let mut root = directory.create_empty().unwrap();

  // Insert enough to create a multi-level tree.
  for i in 0..50 {
    root = directory.insert(&root, make_entry(&format!("entry_{i:04}"))).unwrap();
  }

  // Remove most entries to trigger merges.
  for i in 0..40 {
    let name = format!("entry_{i:04}");
    let (new_root, removed) = directory.remove(&root, &name).unwrap();
    assert!(removed.is_some(), "entry {name} should be removable");
    root = new_root;
  }

  // Verify remaining entries.
  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 10);

  for i in 40..50 {
    let name = format!("entry_{i:04}");
    let result = directory.get(&root, &name).unwrap();
    assert!(result.is_some(), "entry {name} should still exist");
  }
}

// ─── COW (Copy-on-Write) properties ────────────────────────────────────────

#[test]
fn test_cow_old_root_still_valid() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();

  let root_v1 = directory.insert(&root, make_entry("version1.txt")).unwrap();
  let root_v2 = directory.insert(&root_v1, make_entry("version2.txt")).unwrap();

  // Old root (v1) should still have only version1.txt.
  let v1_entries = directory.list(&root_v1).unwrap();
  assert_eq!(v1_entries.len(), 1);
  assert_eq!(v1_entries[0].name, "version1.txt");

  // New root (v2) should have both.
  let v2_entries = directory.list(&root_v2).unwrap();
  assert_eq!(v2_entries.len(), 2);
}

#[test]
fn test_cow_new_root_has_new_data() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root_v1 = directory.insert(&root, make_entry("first.txt")).unwrap();
  let root_v2 = directory.insert(&root_v1, make_entry("second.txt")).unwrap();

  let result = directory.get(&root_v2, "second.txt").unwrap();
  assert!(result.is_some());
  assert_eq!(result.unwrap().name, "second.txt");
}

#[test]
fn test_cow_shared_nodes() {
  let (directory, storage) = setup_with_degree(3);
  let mut root = directory.create_empty().unwrap();

  // Insert enough entries to build a multi-level tree.
  for i in 0..20 {
    root = directory.insert(&root, make_entry(&format!("entry_{i:04}"))).unwrap();
  }

  let chunks_before = storage.chunk_count().unwrap();

  // Insert one more entry — only some nodes should change.
  let root_v2 = directory.insert(&root, make_entry("entry_9999")).unwrap();
  let chunks_after = storage.chunk_count().unwrap();

  // The number of new chunks should be small (logarithmic), not 20+.
  let new_chunks = chunks_after - chunks_before;
  assert!(
    new_chunks < 10,
    "inserting one entry should create few new chunks (got {new_chunks}), not rebuild the whole tree",
  );

  // Both versions should be independently valid.
  assert_eq!(directory.count(&root).unwrap(), 20);
  assert_eq!(directory.count(&root_v2).unwrap(), 21);
}

#[test]
fn test_cow_remove_preserves_old_version() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root_v1 = directory.insert(&root, make_entry("to_remove.txt")).unwrap();
  let root_v1 = directory.insert(&root_v1, make_entry("to_keep.txt")).unwrap();

  let (root_v2, _removed) = directory.remove(&root_v1, "to_remove.txt").unwrap();

  // v1 should still have both entries.
  let v1_entries = directory.list(&root_v1).unwrap();
  assert_eq!(v1_entries.len(), 2);

  // v2 should have only one.
  let v2_entries = directory.list(&root_v2).unwrap();
  assert_eq!(v2_entries.len(), 1);
  assert_eq!(v2_entries[0].name, "to_keep.txt");
}

// ─── Entry types ────────────────────────────────────────────────────────────

#[test]
fn test_hard_link_entry() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root = directory.insert(&root, make_hard_link_entry("shortcut.txt")).unwrap();

  let result = directory.get(&root, "shortcut.txt").unwrap().unwrap();
  assert_eq!(result.entry_type, EntryType::HardLink);
}

#[test]
fn test_directory_entry_type() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root = directory.insert(&root, make_directory_entry("subdir")).unwrap();

  let result = directory.get(&root, "subdir").unwrap().unwrap();
  assert_eq!(result.entry_type, EntryType::Directory);
}

// ─── Scale ──────────────────────────────────────────────────────────────────

#[test]
fn test_large_directory_1000_entries() {
  let (directory, _storage) = setup_with_degree(16);
  let mut root = directory.create_empty().unwrap();

  for i in 0..1000 {
    root = directory.insert(&root, make_entry(&format!("file_{i:06}"))).unwrap();
  }

  // All entries retrievable.
  for i in 0..1000 {
    let name = format!("file_{i:06}");
    let result = directory.get(&root, &name).unwrap();
    assert!(result.is_some(), "entry {name} should be retrievable");
  }

  // Sorted.
  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 1000);
  for window in entries.windows(2) {
    assert!(window[0].name < window[1].name);
  }

  // Count.
  assert_eq!(directory.count(&root).unwrap(), 1000);
}

// ─── Overwrite and metadata ─────────────────────────────────────────────────

#[test]
fn test_overwrite_existing_entry() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();

  let entry_v1 = make_entry("config.json");
  let document_id_v1 = entry_v1.document_id;
  let root = directory.insert(&root, entry_v1).unwrap();

  let entry_v2 = make_entry("config.json");
  let document_id_v2 = entry_v2.document_id;
  let root = directory.insert(&root, entry_v2).unwrap();

  let result = directory.get(&root, "config.json").unwrap().unwrap();
  assert_eq!(result.document_id, document_id_v2);
  assert_ne!(document_id_v1, document_id_v2);

  // Should be only one entry with that name.
  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 1);
}

#[test]
fn test_entry_metadata_preserved() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();

  let document_id = Uuid::new_v4();
  let created_at = Utc::now();
  let updated_at = Utc::now();

  let entry = IndexEntry {
    name: "metadata_test.json".to_string(),
    entry_type: EntryType::File,
    chunk_list: ChunkList::Inline(vec![hash_data(b"data")]),
    document_id,
    created_at,
    updated_at,
    content_type: Some("application/json".to_string()),
    total_size: 999,
  };

  let root = directory.insert(&root, entry).unwrap();
  let retrieved = directory.get(&root, "metadata_test.json").unwrap().unwrap();

  assert_eq!(retrieved.document_id, document_id);
  assert_eq!(retrieved.created_at, created_at);
  assert_eq!(retrieved.updated_at, updated_at);
  assert_eq!(retrieved.content_type, Some("application/json".to_string()));
  assert_eq!(retrieved.total_size, 999);
}

// ─── Edge cases ─────────────────────────────────────────────────────────────

#[test]
fn test_empty_directory_list_returns_empty() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let entries = directory.list(&root).unwrap();
  assert!(entries.is_empty());
}

#[test]
fn test_empty_directory_count_returns_zero() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  assert_eq!(directory.count(&root).unwrap(), 0);
}

#[test]
fn test_get_with_invalid_root_hash_fails() {
  let (directory, _storage) = setup();
  let fake_hash = hash_data(b"nonexistent root");
  let result = directory.get(&fake_hash, "anything");
  assert!(result.is_err(), "get with invalid root should error");
}

#[test]
fn test_insert_with_invalid_root_hash_fails() {
  let (directory, _storage) = setup();
  let fake_hash = hash_data(b"nonexistent root");
  let result = directory.insert(&fake_hash, make_entry("test"));
  assert!(result.is_err(), "insert with invalid root should error");
}

#[test]
fn test_remove_with_invalid_root_hash_fails() {
  let (directory, _storage) = setup();
  let fake_hash = hash_data(b"nonexistent root");
  let result = directory.remove(&fake_hash, "anything");
  assert!(result.is_err(), "remove with invalid root should error");
}

#[test]
fn test_insert_and_remove_all_entries() {
  let (directory, _storage) = setup_with_degree(3);
  let mut root = directory.create_empty().unwrap();

  let names: Vec<String> = (0..20).map(|i| format!("entry_{i:03}")).collect();
  for name in &names {
    root = directory.insert(&root, make_entry(name)).unwrap();
  }
  assert_eq!(directory.count(&root).unwrap(), 20);

  // Remove all entries.
  for name in &names {
    let (new_root, removed) = directory.remove(&root, name).unwrap();
    assert!(removed.is_some(), "{name} should be removable");
    root = new_root;
  }

  assert_eq!(directory.count(&root).unwrap(), 0);
  assert!(directory.list(&root).unwrap().is_empty());
}

#[test]
fn test_insert_reverse_order() {
  let (directory, _storage) = setup_with_degree(3);
  let mut root = directory.create_empty().unwrap();

  // Insert in reverse lexicographic order to stress the tree differently.
  for i in (0..50).rev() {
    root = directory.insert(&root, make_entry(&format!("entry_{i:04}"))).unwrap();
  }

  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 50);
  for window in entries.windows(2) {
    assert!(window[0].name < window[1].name, "entries should be sorted");
  }
}

#[test]
fn test_mixed_entry_types_in_same_directory() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  root = directory.insert(&root, make_entry("file.txt")).unwrap();
  root = directory.insert(&root, make_directory_entry("subdir")).unwrap();
  root = directory.insert(&root, make_hard_link_entry("link.txt")).unwrap();

  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 3);

  let file_entry = directory.get(&root, "file.txt").unwrap().unwrap();
  assert_eq!(file_entry.entry_type, EntryType::File);

  let dir_entry = directory.get(&root, "subdir").unwrap().unwrap();
  assert_eq!(dir_entry.entry_type, EntryType::Directory);

  let link_entry = directory.get(&root, "link.txt").unwrap().unwrap();
  assert_eq!(link_entry.entry_type, EntryType::HardLink);
}

#[test]
fn test_list_range_empty_result() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  root = directory.insert(&root, make_entry("alpha")).unwrap();
  root = directory.insert(&root, make_entry("zulu")).unwrap();

  // Range that matches nothing.
  let range_entries = directory.list_range(&root, "mmm", "nnn").unwrap();
  assert!(range_entries.is_empty());
}

#[test]
fn test_list_range_full_overlap() {
  let (directory, _storage) = setup();
  let mut root = directory.create_empty().unwrap();

  for name in &["alpha", "bravo", "charlie"] {
    root = directory.insert(&root, make_entry(name)).unwrap();
  }

  // Range that includes everything.
  let range_entries = directory.list_range(&root, "a", "z").unwrap();
  assert_eq!(range_entries.len(), 3);
}

#[test]
fn test_single_entry_directory() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root = directory.insert(&root, make_entry("only_one")).unwrap();

  assert_eq!(directory.count(&root).unwrap(), 1);
  let entries = directory.list(&root).unwrap();
  assert_eq!(entries.len(), 1);
  assert_eq!(entries[0].name, "only_one");
}

#[test]
fn test_remove_from_single_entry_directory() {
  let (directory, _storage) = setup();
  let root = directory.create_empty().unwrap();
  let root = directory.insert(&root, make_entry("sole")).unwrap();

  let (new_root, removed) = directory.remove(&root, "sole").unwrap();
  assert!(removed.is_some());
  assert_eq!(directory.count(&new_root).unwrap(), 0);
}
