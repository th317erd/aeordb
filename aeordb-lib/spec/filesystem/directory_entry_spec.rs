use aeordb::filesystem::directory_entry::{DirectoryEntry, EntryType};
use aeordb::storage::ChunkHash;

#[test]
fn test_new_file_entry_has_uuid() {
  let entry = DirectoryEntry::new_file("test.txt", vec![], None, 0);
  assert!(!entry.document_id.is_nil());

  // Two entries should have different UUIDs
  let entry2 = DirectoryEntry::new_file("test2.txt", vec![], None, 0);
  assert_ne!(entry.document_id, entry2.document_id);
}

#[test]
fn test_new_file_entry_has_timestamps() {
  let before = chrono::Utc::now();
  let entry = DirectoryEntry::new_file("test.txt", vec![], None, 0);
  let after = chrono::Utc::now();

  assert!(entry.created_at >= before);
  assert!(entry.created_at <= after);
  assert!(entry.updated_at >= before);
  assert!(entry.updated_at <= after);
  // created_at and updated_at should be equal on creation
  assert_eq!(entry.created_at, entry.updated_at);
}

#[test]
fn test_new_directory_entry_has_empty_chunks() {
  let entry = DirectoryEntry::new_directory("mydir");
  assert_eq!(entry.entry_type, EntryType::Directory);
  assert!(entry.chunk_hashes.is_empty());
  assert_eq!(entry.total_size, 0);
  assert!(entry.content_type.is_none());
  assert_eq!(entry.name, "mydir");
}

#[test]
fn test_new_hard_link_copies_chunk_hashes() {
  let hash1: ChunkHash = [1u8; 32];
  let hash2: ChunkHash = [2u8; 32];
  let target = DirectoryEntry::new_file(
    "original.dat",
    vec![hash1, hash2],
    Some("application/octet-stream".to_string()),
    1024,
  );

  let link = DirectoryEntry::new_hard_link("link.dat", &target);
  assert_eq!(link.entry_type, EntryType::HardLink);
  assert_eq!(link.chunk_hashes, target.chunk_hashes);
  assert_eq!(link.content_type, target.content_type);
  assert_eq!(link.total_size, target.total_size);
  assert_ne!(link.document_id, target.document_id);
  assert_eq!(link.name, "link.dat");
}

#[test]
fn test_serialize_deserialize_roundtrip_file() {
  let hash: ChunkHash = [42u8; 32];
  let entry = DirectoryEntry::new_file(
    "data.json",
    vec![hash],
    Some("application/json".to_string()),
    512,
  );

  let bytes = entry.serialize_to_bytes().unwrap();
  let restored = DirectoryEntry::deserialize_from_bytes(&bytes).unwrap();

  assert_eq!(restored.name, entry.name);
  assert_eq!(restored.entry_type, entry.entry_type);
  assert_eq!(restored.chunk_hashes, entry.chunk_hashes);
  assert_eq!(restored.document_id, entry.document_id);
  assert_eq!(restored.content_type, entry.content_type);
  assert_eq!(restored.total_size, entry.total_size);
}

#[test]
fn test_serialize_deserialize_roundtrip_directory() {
  let entry = DirectoryEntry::new_directory("subdir");

  let bytes = entry.serialize_to_bytes().unwrap();
  let restored = DirectoryEntry::deserialize_from_bytes(&bytes).unwrap();

  assert_eq!(restored.name, "subdir");
  assert_eq!(restored.entry_type, EntryType::Directory);
  assert!(restored.chunk_hashes.is_empty());
  assert_eq!(restored.document_id, entry.document_id);
}

#[test]
fn test_serialize_deserialize_roundtrip_hard_link() {
  let hash: ChunkHash = [99u8; 32];
  let target = DirectoryEntry::new_file("target.bin", vec![hash], None, 256);
  let link = DirectoryEntry::new_hard_link("alias.bin", &target);

  let bytes = link.serialize_to_bytes().unwrap();
  let restored = DirectoryEntry::deserialize_from_bytes(&bytes).unwrap();

  assert_eq!(restored.entry_type, EntryType::HardLink);
  assert_eq!(restored.chunk_hashes, link.chunk_hashes);
  assert_eq!(restored.name, "alias.bin");
}

#[test]
fn test_entry_with_content_type() {
  let entry = DirectoryEntry::new_file(
    "image.png",
    vec![],
    Some("image/png".to_string()),
    2048,
  );
  assert_eq!(entry.content_type, Some("image/png".to_string()));

  let bytes = entry.serialize_to_bytes().unwrap();
  let restored = DirectoryEntry::deserialize_from_bytes(&bytes).unwrap();
  assert_eq!(restored.content_type, Some("image/png".to_string()));
}

#[test]
fn test_entry_without_content_type() {
  let entry = DirectoryEntry::new_file("unknown", vec![], None, 0);
  assert_eq!(entry.content_type, None);

  let bytes = entry.serialize_to_bytes().unwrap();
  let restored = DirectoryEntry::deserialize_from_bytes(&bytes).unwrap();
  assert_eq!(restored.content_type, None);
}

#[test]
fn test_entry_with_many_chunk_hashes() {
  let hashes: Vec<ChunkHash> = (0..100u8)
    .map(|i| {
      let mut hash = [0u8; 32];
      hash[0] = i;
      hash[31] = 255 - i;
      hash
    })
    .collect();

  let total_size = 100 * 64 * 1024; // 100 chunks of 64KB
  let entry = DirectoryEntry::new_file(
    "large_file.dat",
    hashes.clone(),
    Some("application/octet-stream".to_string()),
    total_size,
  );

  assert_eq!(entry.chunk_hashes.len(), 100);

  let bytes = entry.serialize_to_bytes().unwrap();
  let restored = DirectoryEntry::deserialize_from_bytes(&bytes).unwrap();
  assert_eq!(restored.chunk_hashes.len(), 100);
  assert_eq!(restored.chunk_hashes, hashes);
  assert_eq!(restored.total_size, total_size);
}

#[test]
fn test_deserialize_invalid_bytes() {
  let garbage = b"this is not valid json";
  let result = DirectoryEntry::deserialize_from_bytes(garbage);
  assert!(result.is_err());
}

#[test]
fn test_deserialize_empty_bytes() {
  let result = DirectoryEntry::deserialize_from_bytes(b"");
  assert!(result.is_err());
}

#[test]
fn test_deserialize_partial_json() {
  let partial = b"{\"name\":\"test\"";
  let result = DirectoryEntry::deserialize_from_bytes(partial);
  assert!(result.is_err());
}

#[test]
fn test_file_entry_preserves_name() {
  let entry = DirectoryEntry::new_file("my-special-file.txt", vec![], None, 0);
  assert_eq!(entry.name, "my-special-file.txt");
  assert_eq!(entry.entry_type, EntryType::File);
}

#[test]
fn test_hard_link_to_empty_file() {
  let target = DirectoryEntry::new_file("empty.txt", vec![], None, 0);
  let link = DirectoryEntry::new_hard_link("link_to_empty.txt", &target);
  assert!(link.chunk_hashes.is_empty());
  assert_eq!(link.total_size, 0);
}

#[test]
fn test_entry_type_equality() {
  assert_eq!(EntryType::File, EntryType::File);
  assert_eq!(EntryType::Directory, EntryType::Directory);
  assert_eq!(EntryType::HardLink, EntryType::HardLink);
  assert_ne!(EntryType::File, EntryType::Directory);
  assert_ne!(EntryType::File, EntryType::HardLink);
  assert_ne!(EntryType::Directory, EntryType::HardLink);
}
