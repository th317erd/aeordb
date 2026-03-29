use aeordb::filesystem::{ChunkList, EntryType, IndexEntry};
use aeordb::storage::hash_data;
use chrono::Utc;
use uuid::Uuid;

fn make_file_entry(name: &str) -> IndexEntry {
  IndexEntry {
    name: name.to_string(),
    entry_type: EntryType::File,
    chunk_list: ChunkList::Inline(vec![hash_data(b"chunk1"), hash_data(b"chunk2")]),
    document_id: Uuid::new_v4(),
    created_at: Utc::now(),
    updated_at: Utc::now(),
    content_type: Some("application/json".to_string()),
    total_size: 1024,
  }
}

fn make_directory_entry(name: &str) -> IndexEntry {
  IndexEntry {
    name: name.to_string(),
    entry_type: EntryType::Directory,
    chunk_list: ChunkList::Inline(vec![hash_data(b"root-node")]),
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
    chunk_list: ChunkList::Inline(vec![hash_data(b"shared-chunk")]),
    document_id: Uuid::new_v4(),
    created_at: Utc::now(),
    updated_at: Utc::now(),
    content_type: Some("text/plain".to_string()),
    total_size: 512,
  }
}

#[test]
fn test_index_entry_serialize_deserialize_roundtrip() {
  let entry = make_file_entry("test.json");
  let serialized = entry.serialize().expect("serialization should succeed");
  let deserialized = IndexEntry::deserialize(&serialized)
    .expect("deserialization should succeed");
  assert_eq!(entry, deserialized);
}

#[test]
fn test_index_entry_file_type() {
  let entry = make_file_entry("data.bin");
  assert_eq!(entry.entry_type, EntryType::File);
}

#[test]
fn test_index_entry_directory_type() {
  let entry = make_directory_entry("subdir");
  assert_eq!(entry.entry_type, EntryType::Directory);
}

#[test]
fn test_index_entry_hard_link_type() {
  let entry = make_hard_link_entry("link.txt");
  assert_eq!(entry.entry_type, EntryType::HardLink);
}

#[test]
fn test_chunk_list_inline() {
  let hashes = vec![hash_data(b"a"), hash_data(b"b"), hash_data(b"c")];
  let chunk_list = ChunkList::Inline(hashes.clone());
  match &chunk_list {
    ChunkList::Inline(stored_hashes) => {
      assert_eq!(stored_hashes, &hashes);
    }
    ChunkList::Overflow(_) => panic!("expected Inline variant"),
  }
}

#[test]
fn test_chunk_list_overflow() {
  let overflow_hash = hash_data(b"overflow-chunk");
  let chunk_list = ChunkList::Overflow(overflow_hash);
  match &chunk_list {
    ChunkList::Overflow(stored_hash) => {
      assert_eq!(stored_hash, &overflow_hash);
    }
    ChunkList::Inline(_) => panic!("expected Overflow variant"),
  }
}

#[test]
fn test_index_entry_with_content_type() {
  let entry = make_file_entry("image.png");
  let serialized = entry.serialize().unwrap();
  let deserialized = IndexEntry::deserialize(&serialized).unwrap();
  assert_eq!(deserialized.content_type, Some("application/json".to_string()));
}

#[test]
fn test_index_entry_without_content_type() {
  let entry = make_directory_entry("configs");
  assert!(entry.content_type.is_none());
  let serialized = entry.serialize().unwrap();
  let deserialized = IndexEntry::deserialize(&serialized).unwrap();
  assert!(deserialized.content_type.is_none());
}

#[test]
fn test_serialize_empty_bytes_fails() {
  let result = IndexEntry::deserialize(b"");
  assert!(result.is_err());
}

#[test]
fn test_serialize_garbage_bytes_fails() {
  let result = IndexEntry::deserialize(b"not valid json at all {{{");
  assert!(result.is_err());
}

#[test]
fn test_index_entry_preserves_document_id() {
  let document_id = Uuid::new_v4();
  let entry = IndexEntry {
    name: "preserved.txt".to_string(),
    entry_type: EntryType::File,
    chunk_list: ChunkList::Inline(vec![]),
    document_id,
    created_at: Utc::now(),
    updated_at: Utc::now(),
    content_type: None,
    total_size: 0,
  };
  let roundtripped = IndexEntry::deserialize(&entry.serialize().unwrap()).unwrap();
  assert_eq!(roundtripped.document_id, document_id);
}

#[test]
fn test_index_entry_preserves_timestamps() {
  let now = Utc::now();
  let entry = IndexEntry {
    name: "timestamps.txt".to_string(),
    entry_type: EntryType::File,
    chunk_list: ChunkList::Inline(vec![]),
    document_id: Uuid::new_v4(),
    created_at: now,
    updated_at: now,
    content_type: None,
    total_size: 42,
  };
  let roundtripped = IndexEntry::deserialize(&entry.serialize().unwrap()).unwrap();
  assert_eq!(roundtripped.created_at, now);
  assert_eq!(roundtripped.updated_at, now);
  assert_eq!(roundtripped.total_size, 42);
}

#[test]
fn test_chunk_list_inline_empty() {
  let chunk_list = ChunkList::Inline(vec![]);
  match &chunk_list {
    ChunkList::Inline(hashes) => assert!(hashes.is_empty()),
    ChunkList::Overflow(_) => panic!("expected Inline variant"),
  }
}
