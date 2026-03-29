use aeordb::filesystem::directory_entry::{DirectoryEntry, EntryType};
use aeordb::filesystem::redb_directory::RedbDirectory;
use aeordb::storage::ChunkHash;
use redb::backends::InMemoryBackend;
use redb::Database;
use std::sync::Arc;

fn create_test_directory() -> RedbDirectory {
  let backend = InMemoryBackend::new();
  let database = Database::builder()
    .create_with_backend(backend)
    .expect("failed to create in-memory database");
  RedbDirectory::new(Arc::new(database))
}

#[test]
fn test_create_directory() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();
  assert!(directory.directory_exists("/").unwrap());
}

#[test]
fn test_insert_and_get_entry() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let entry = DirectoryEntry::new_file(
    "hello.txt",
    vec![],
    Some("text/plain".to_string()),
    42,
  );
  directory.insert_entry("/", &entry).unwrap();

  let retrieved = directory.get_entry("/", "hello.txt").unwrap().unwrap();
  assert_eq!(retrieved.name, "hello.txt");
  assert_eq!(retrieved.entry_type, EntryType::File);
  assert_eq!(retrieved.content_type, Some("text/plain".to_string()));
  assert_eq!(retrieved.total_size, 42);
  assert_eq!(retrieved.document_id, entry.document_id);
}

#[test]
fn test_insert_multiple_entries() {
  let directory = create_test_directory();
  directory.create_directory("/docs").unwrap();

  let file_a = DirectoryEntry::new_file("a.txt", vec![], None, 10);
  let file_b = DirectoryEntry::new_file("b.txt", vec![], None, 20);
  let file_c = DirectoryEntry::new_file("c.txt", vec![], None, 30);

  directory.insert_entry("/docs", &file_a).unwrap();
  directory.insert_entry("/docs", &file_b).unwrap();
  directory.insert_entry("/docs", &file_c).unwrap();

  assert_eq!(directory.count_entries("/docs").unwrap(), 3);
}

#[test]
fn test_get_nonexistent_returns_none() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let result = directory.get_entry("/", "ghost.txt").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_get_from_nonexistent_directory_returns_none() {
  let directory = create_test_directory();
  let result = directory.get_entry("/nowhere", "file.txt").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_remove_entry() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let entry = DirectoryEntry::new_file("doomed.txt", vec![], None, 0);
  directory.insert_entry("/", &entry).unwrap();

  let removed = directory.remove_entry("/", "doomed.txt").unwrap();
  assert!(removed.is_some());
  assert_eq!(removed.unwrap().name, "doomed.txt");

  let gone = directory.get_entry("/", "doomed.txt").unwrap();
  assert!(gone.is_none());
}

#[test]
fn test_remove_nonexistent_returns_none() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let result = directory.remove_entry("/", "ghost.txt").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_list_entries_sorted_by_name() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  // Insert in reverse order
  let file_z = DirectoryEntry::new_file("zebra.txt", vec![], None, 0);
  let file_a = DirectoryEntry::new_file("alpha.txt", vec![], None, 0);
  let file_m = DirectoryEntry::new_file("middle.txt", vec![], None, 0);

  directory.insert_entry("/", &file_z).unwrap();
  directory.insert_entry("/", &file_a).unwrap();
  directory.insert_entry("/", &file_m).unwrap();

  let entries = directory.list_entries("/").unwrap();
  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0].name, "alpha.txt");
  assert_eq!(entries[1].name, "middle.txt");
  assert_eq!(entries[2].name, "zebra.txt");
}

#[test]
fn test_list_entries_empty_directory() {
  let directory = create_test_directory();
  directory.create_directory("/empty").unwrap();

  let entries = directory.list_entries("/empty").unwrap();
  assert!(entries.is_empty());
}

#[test]
fn test_list_entries_nonexistent_directory() {
  let directory = create_test_directory();
  let entries = directory.list_entries("/nope").unwrap();
  assert!(entries.is_empty());
}

#[test]
fn test_count_entries() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  assert_eq!(directory.count_entries("/").unwrap(), 0);

  let file = DirectoryEntry::new_file("one.txt", vec![], None, 0);
  directory.insert_entry("/", &file).unwrap();
  assert_eq!(directory.count_entries("/").unwrap(), 1);

  let file2 = DirectoryEntry::new_file("two.txt", vec![], None, 0);
  directory.insert_entry("/", &file2).unwrap();
  assert_eq!(directory.count_entries("/").unwrap(), 2);
}

#[test]
fn test_count_entries_nonexistent_directory() {
  let directory = create_test_directory();
  assert_eq!(directory.count_entries("/nope").unwrap(), 0);
}

#[test]
fn test_directory_exists() {
  let directory = create_test_directory();
  directory.create_directory("/myapp").unwrap();
  assert!(directory.directory_exists("/myapp").unwrap());
}

#[test]
fn test_directory_not_exists() {
  let directory = create_test_directory();
  assert!(!directory.directory_exists("/nonexistent").unwrap());
}

#[test]
fn test_delete_directory() {
  let directory = create_test_directory();
  directory.create_directory("/temp").unwrap();

  let file = DirectoryEntry::new_file("data.bin", vec![], None, 100);
  directory.insert_entry("/temp", &file).unwrap();

  directory.delete_directory("/temp").unwrap();
  assert!(!directory.directory_exists("/temp").unwrap());

  // Entries should be gone
  let entries = directory.list_entries("/temp").unwrap();
  assert!(entries.is_empty());
}

#[test]
fn test_list_subdirectories() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let subdir_a = DirectoryEntry::new_directory("apps");
  let subdir_b = DirectoryEntry::new_directory("config");
  let file = DirectoryEntry::new_file("readme.txt", vec![], None, 0);

  directory.insert_entry("/", &subdir_a).unwrap();
  directory.insert_entry("/", &subdir_b).unwrap();
  directory.insert_entry("/", &file).unwrap();

  let subdirs = directory.list_subdirectories("/").unwrap();
  assert_eq!(subdirs.len(), 2);
  assert!(subdirs.contains(&"apps".to_string()));
  assert!(subdirs.contains(&"config".to_string()));
  assert!(!subdirs.contains(&"readme.txt".to_string()));
}

#[test]
fn test_overwrite_existing_entry() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let entry_v1 = DirectoryEntry::new_file("config.json", vec![], None, 100);
  directory.insert_entry("/", &entry_v1).unwrap();

  let hash: ChunkHash = [0xAB; 32];
  let entry_v2 = DirectoryEntry::new_file(
    "config.json",
    vec![hash],
    Some("application/json".to_string()),
    200,
  );
  directory.insert_entry("/", &entry_v2).unwrap();

  // Should have overwritten, not duplicated
  assert_eq!(directory.count_entries("/").unwrap(), 1);

  let retrieved = directory.get_entry("/", "config.json").unwrap().unwrap();
  assert_eq!(retrieved.total_size, 200);
  assert_eq!(retrieved.chunk_hashes.len(), 1);
  assert_eq!(retrieved.document_id, entry_v2.document_id);
}

#[test]
fn test_entry_metadata_preserved() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let hash: ChunkHash = [0xFF; 32];
  let entry = DirectoryEntry::new_file(
    "precious.dat",
    vec![hash],
    Some("application/octet-stream".to_string()),
    9999,
  );
  let original_id = entry.document_id;
  let original_created = entry.created_at;

  directory.insert_entry("/", &entry).unwrap();

  let retrieved = directory.get_entry("/", "precious.dat").unwrap().unwrap();
  assert_eq!(retrieved.document_id, original_id);
  assert_eq!(retrieved.created_at, original_created);
  assert_eq!(retrieved.total_size, 9999);
  assert_eq!(retrieved.content_type, Some("application/octet-stream".to_string()));
  assert_eq!(retrieved.chunk_hashes, vec![hash]);
}

#[test]
fn test_multiple_directories_isolated() {
  let directory = create_test_directory();
  directory.create_directory("/alpha").unwrap();
  directory.create_directory("/beta").unwrap();

  let file_a = DirectoryEntry::new_file("shared_name.txt", vec![], None, 10);
  let file_b = DirectoryEntry::new_file("shared_name.txt", vec![], None, 20);

  directory.insert_entry("/alpha", &file_a).unwrap();
  directory.insert_entry("/beta", &file_b).unwrap();

  let from_alpha = directory.get_entry("/alpha", "shared_name.txt").unwrap().unwrap();
  let from_beta = directory.get_entry("/beta", "shared_name.txt").unwrap().unwrap();

  assert_eq!(from_alpha.total_size, 10);
  assert_eq!(from_beta.total_size, 20);
  assert_ne!(from_alpha.document_id, from_beta.document_id);

  // Deleting from one shouldn't affect the other
  directory.remove_entry("/alpha", "shared_name.txt").unwrap();
  assert!(directory.get_entry("/alpha", "shared_name.txt").unwrap().is_none());
  assert!(directory.get_entry("/beta", "shared_name.txt").unwrap().is_some());
}

#[test]
fn test_large_directory_100_entries() {
  let directory = create_test_directory();
  directory.create_directory("/big").unwrap();

  for i in 0..100 {
    let name = format!("file_{:04}.txt", i);
    let entry = DirectoryEntry::new_file(name, vec![], None, i as u64);
    directory.insert_entry("/big", &entry).unwrap();
  }

  assert_eq!(directory.count_entries("/big").unwrap(), 100);

  let entries = directory.list_entries("/big").unwrap();
  assert_eq!(entries.len(), 100);
  // Verify sorted order
  for i in 0..100 {
    assert_eq!(entries[i].name, format!("file_{:04}.txt", i));
    assert_eq!(entries[i].total_size, i as u64);
  }
}

#[test]
fn test_insert_and_get_with_chunk_hashes() {
  let directory = create_test_directory();
  directory.create_directory("/data").unwrap();

  let hashes: Vec<ChunkHash> = (0..5u8)
    .map(|i| {
      let mut hash = [0u8; 32];
      hash[0] = i;
      hash[31] = i * 10;
      hash
    })
    .collect();

  let entry = DirectoryEntry::new_file(
    "chunked_file.bin",
    hashes.clone(),
    Some("application/octet-stream".to_string()),
    5 * 65536,
  );

  directory.insert_entry("/data", &entry).unwrap();

  let retrieved = directory.get_entry("/data", "chunked_file.bin").unwrap().unwrap();
  assert_eq!(retrieved.chunk_hashes, hashes);
  assert_eq!(retrieved.chunk_hashes.len(), 5);
}

#[test]
fn test_create_directory_idempotent() {
  let directory = create_test_directory();
  directory.create_directory("/mydir").unwrap();

  // Insert an entry
  let entry = DirectoryEntry::new_file("data.txt", vec![], None, 0);
  directory.insert_entry("/mydir", &entry).unwrap();

  // Creating the directory again should not destroy existing entries
  directory.create_directory("/mydir").unwrap();
  assert_eq!(directory.count_entries("/mydir").unwrap(), 1);
}

#[test]
fn test_insert_creates_directory_implicitly() {
  let directory = create_test_directory();

  // Insert into a directory that hasn't been explicitly created
  let entry = DirectoryEntry::new_file("surprise.txt", vec![], None, 0);
  directory.insert_entry("/implicit", &entry).unwrap();

  let retrieved = directory.get_entry("/implicit", "surprise.txt").unwrap();
  assert!(retrieved.is_some());
  assert!(directory.directory_exists("/implicit").unwrap());
}

#[test]
fn test_delete_nonexistent_directory() {
  let directory = create_test_directory();
  // Deleting a directory that never existed should not panic.
  // redb's delete_table returns false if table didn't exist -- no error.
  let result = directory.delete_directory("/ghost");
  assert!(result.is_ok());
}

#[test]
fn test_list_subdirectories_empty() {
  let directory = create_test_directory();
  directory.create_directory("/empty_parent").unwrap();

  let subdirs = directory.list_subdirectories("/empty_parent").unwrap();
  assert!(subdirs.is_empty());
}

#[test]
fn test_list_subdirectories_only_files() {
  let directory = create_test_directory();
  directory.create_directory("/files_only").unwrap();

  let file = DirectoryEntry::new_file("a.txt", vec![], None, 0);
  directory.insert_entry("/files_only", &file).unwrap();
  let file2 = DirectoryEntry::new_file("b.txt", vec![], None, 0);
  directory.insert_entry("/files_only", &file2).unwrap();

  let subdirs = directory.list_subdirectories("/files_only").unwrap();
  assert!(subdirs.is_empty());
}

#[test]
fn test_hard_link_entry_in_directory() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let hash: ChunkHash = [0xDE; 32];
  let target = DirectoryEntry::new_file("original.dat", vec![hash], None, 512);
  let link = DirectoryEntry::new_hard_link("link.dat", &target);

  directory.insert_entry("/", &target).unwrap();
  directory.insert_entry("/", &link).unwrap();

  let retrieved_link = directory.get_entry("/", "link.dat").unwrap().unwrap();
  assert_eq!(retrieved_link.entry_type, EntryType::HardLink);
  assert_eq!(retrieved_link.chunk_hashes, vec![hash]);
}

#[test]
fn test_remove_then_reinsert() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let entry = DirectoryEntry::new_file("cycle.txt", vec![], None, 10);
  directory.insert_entry("/", &entry).unwrap();

  directory.remove_entry("/", "cycle.txt").unwrap();
  assert!(directory.get_entry("/", "cycle.txt").unwrap().is_none());

  let entry2 = DirectoryEntry::new_file("cycle.txt", vec![], None, 20);
  directory.insert_entry("/", &entry2).unwrap();

  let retrieved = directory.get_entry("/", "cycle.txt").unwrap().unwrap();
  assert_eq!(retrieved.total_size, 20);
  assert_eq!(retrieved.document_id, entry2.document_id);
}

#[test]
fn test_nested_directory_paths() {
  let directory = create_test_directory();
  directory.create_directory("/a/b/c/d").unwrap();

  let entry = DirectoryEntry::new_file("deep.txt", vec![], None, 0);
  directory.insert_entry("/a/b/c/d", &entry).unwrap();

  let retrieved = directory.get_entry("/a/b/c/d", "deep.txt").unwrap();
  assert!(retrieved.is_some());

  // The intermediate paths are not automatically created -- only the leaf table
  assert!(!directory.directory_exists("/a/b/c").unwrap());
}

#[test]
fn test_special_characters_in_entry_name() {
  let directory = create_test_directory();
  directory.create_directory("/").unwrap();

  let entry = DirectoryEntry::new_file(
    "file with spaces & (parens) [brackets].txt",
    vec![],
    None,
    0,
  );
  directory.insert_entry("/", &entry).unwrap();

  let retrieved = directory
    .get_entry("/", "file with spaces & (parens) [brackets].txt")
    .unwrap();
  assert!(retrieved.is_some());
  assert_eq!(retrieved.unwrap().name, "file with spaces & (parens) [brackets].txt");
}
