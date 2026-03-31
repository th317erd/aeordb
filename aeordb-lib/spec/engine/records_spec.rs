use aeordb::engine::{
  FileRecord, DeletionRecord, ChildEntry,
  serialize_child_entries, deserialize_child_entries,
  normalize_path, parent_path, file_name, path_segments,
};

// ─── FileRecord tests ───────────────────────────────────────────────────────

#[test]
fn test_file_record_serialize_deserialize_roundtrip() {
  let hash_length = 32;
  let chunk_hash = vec![0xAB_u8; hash_length];
  let record = FileRecord {
    path: "/myapp/data.json".to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 1024,
    created_at: 1700000000000,
    updated_at: 1700000001000,
    metadata: Vec::new(),
    chunk_hashes: vec![chunk_hash],
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(record, deserialized);
}

#[test]
fn test_file_record_with_chunks() {
  let hash_length = 32;
  let chunks: Vec<Vec<u8>> = (0..5)
    .map(|index| vec![index as u8; hash_length])
    .collect();

  let record = FileRecord {
    path: "/files/large.bin".to_string(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: 5 * 1024 * 1024,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    metadata: Vec::new(),
    chunk_hashes: chunks.clone(),
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.chunk_hashes.len(), 5);
  assert_eq!(deserialized.chunk_hashes, chunks);
}

#[test]
fn test_file_record_without_content_type() {
  let hash_length = 32;
  let record = FileRecord {
    path: "/data/blob".to_string(),
    content_type: None,
    total_size: 256,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    metadata: Vec::new(),
    chunk_hashes: vec![vec![0xFF; hash_length]],
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.content_type, None);
  assert_eq!(deserialized.path, "/data/blob");
}

#[test]
fn test_file_record_with_metadata() {
  let hash_length = 32;
  let metadata = br#"{"author":"alice","version":3}"#.to_vec();
  let record = FileRecord {
    path: "/docs/readme.md".to_string(),
    content_type: Some("text/markdown".to_string()),
    total_size: 512,
    created_at: 1700000000000,
    updated_at: 1700000002000,
    metadata: metadata.clone(),
    chunk_hashes: vec![vec![0x01; hash_length]],
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.metadata, metadata);
}

#[test]
fn test_file_record_empty_chunks() {
  let hash_length = 32;
  let record = FileRecord {
    path: "/empty".to_string(),
    content_type: None,
    total_size: 0,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    metadata: Vec::new(),
    chunk_hashes: Vec::new(),
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert!(deserialized.chunk_hashes.is_empty());
  assert_eq!(deserialized.total_size, 0);
}

#[test]
fn test_file_record_many_chunks() {
  let hash_length = 32;
  let chunks: Vec<Vec<u8>> = (0..150)
    .map(|index| {
      let mut hash = vec![0u8; hash_length];
      hash[0] = (index % 256) as u8;
      hash[1] = (index / 256) as u8;
      hash
    })
    .collect();

  let record = FileRecord {
    path: "/big/file.dat".to_string(),
    content_type: Some("application/octet-stream".to_string()),
    total_size: 150 * 64 * 1024,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    metadata: Vec::new(),
    chunk_hashes: chunks.clone(),
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.chunk_hashes.len(), 150);
  assert_eq!(deserialized.chunk_hashes, chunks);
}

#[test]
fn test_file_record_new_sets_timestamps() {
  let record = FileRecord::new(
    "/test/path".to_string(),
    Some("text/plain".to_string()),
    100,
    vec![vec![0xAA; 32]],
  );

  assert!(record.created_at > 0);
  assert_eq!(record.created_at, record.updated_at);
  assert!(record.metadata.is_empty());
}

#[test]
fn test_file_record_with_64_byte_hash() {
  let hash_length = 64;
  let chunk_hash = vec![0xCD_u8; hash_length];
  let record = FileRecord {
    path: "/sha512/file.bin".to_string(),
    content_type: None,
    total_size: 2048,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    metadata: Vec::new(),
    chunk_hashes: vec![chunk_hash.clone()],
  };

  let serialized = record.serialize(hash_length);
  let deserialized = FileRecord::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.chunk_hashes[0].len(), 64);
  assert_eq!(deserialized.chunk_hashes[0], chunk_hash);
}

#[test]
fn test_file_record_deserialize_truncated_data() {
  let result = FileRecord::deserialize(&[0x00], 32);
  assert!(result.is_err());
}

#[test]
fn test_file_record_deserialize_empty_data() {
  let result = FileRecord::deserialize(&[], 32);
  assert!(result.is_err());
}

// ─── DeletionRecord tests ──────────────────────────────────────────────────

#[test]
fn test_deletion_record_serialize_deserialize_roundtrip() {
  let record = DeletionRecord {
    path: "/myapp/old-file.json".to_string(),
    deleted_at: 1700000005000,
    reason: Some("cleanup".to_string()),
  };

  let serialized = record.serialize();
  let deserialized = DeletionRecord::deserialize(&serialized).unwrap();

  assert_eq!(record, deserialized);
}

#[test]
fn test_deletion_record_with_reason() {
  let record = DeletionRecord {
    path: "/archive/stale.txt".to_string(),
    deleted_at: 1700000010000,
    reason: Some("Expired after 30 days retention policy".to_string()),
  };

  let serialized = record.serialize();
  let deserialized = DeletionRecord::deserialize(&serialized).unwrap();

  assert_eq!(
    deserialized.reason,
    Some("Expired after 30 days retention policy".to_string())
  );
}

#[test]
fn test_deletion_record_without_reason() {
  let record = DeletionRecord {
    path: "/tmp/scratch".to_string(),
    deleted_at: 1700000020000,
    reason: None,
  };

  let serialized = record.serialize();
  let deserialized = DeletionRecord::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.reason, None);
  assert_eq!(deserialized.path, "/tmp/scratch");
}

#[test]
fn test_deletion_record_new_sets_timestamp() {
  let record = DeletionRecord::new(
    "/test/delete-me".to_string(),
    Some("test reason".to_string()),
  );

  assert!(record.deleted_at > 0);
  assert_eq!(record.reason, Some("test reason".to_string()));
}

#[test]
fn test_deletion_record_deserialize_truncated_data() {
  let result = DeletionRecord::deserialize(&[0x00]);
  assert!(result.is_err());
}

#[test]
fn test_deletion_record_deserialize_empty_data() {
  let result = DeletionRecord::deserialize(&[]);
  assert!(result.is_err());
}

// ─── ChildEntry tests ───────────────────────────────────────────────────────

#[test]
fn test_child_entry_serialize_deserialize_roundtrip() {
  let hash_length = 32;
  let entry = ChildEntry {
    entry_type: 1,
    hash: vec![0xAA; hash_length],
    total_size: 4096,
    created_at: 1700000000000,
    updated_at: 1700000001000,
    name: "data.json".to_string(),
    content_type: Some("application/json".to_string()),
  };

  let serialized = entry.serialize(hash_length);
  let (deserialized, bytes_consumed) =
    ChildEntry::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(entry, deserialized);
  assert_eq!(bytes_consumed, serialized.len());
}

#[test]
fn test_child_entry_file_type() {
  let hash_length = 32;
  let entry = ChildEntry {
    entry_type: 1,
    hash: vec![0x11; hash_length],
    total_size: 512,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    name: "readme.md".to_string(),
    content_type: Some("text/markdown".to_string()),
  };

  let serialized = entry.serialize(hash_length);
  let (deserialized, _) =
    ChildEntry::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.entry_type, 1);
  assert_eq!(deserialized.name, "readme.md");
}

#[test]
fn test_child_entry_directory_type() {
  let hash_length = 32;
  let entry = ChildEntry {
    entry_type: 2,
    hash: vec![0x22; hash_length],
    total_size: 0,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    name: "subdir".to_string(),
    content_type: None,
  };

  let serialized = entry.serialize(hash_length);
  let (deserialized, _) =
    ChildEntry::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.entry_type, 2);
  assert_eq!(deserialized.content_type, None);
}

#[test]
fn test_multiple_child_entries_roundtrip() {
  let hash_length = 32;
  let entries = vec![
    ChildEntry {
      entry_type: 1,
      hash: vec![0x01; hash_length],
      total_size: 1024,
      created_at: 1700000000000,
      updated_at: 1700000000000,
      name: "file1.txt".to_string(),
      content_type: Some("text/plain".to_string()),
    },
    ChildEntry {
      entry_type: 2,
      hash: vec![0x02; hash_length],
      total_size: 0,
      created_at: 1700000001000,
      updated_at: 1700000001000,
      name: "subdir".to_string(),
      content_type: None,
    },
    ChildEntry {
      entry_type: 1,
      hash: vec![0x03; hash_length],
      total_size: 2048,
      created_at: 1700000002000,
      updated_at: 1700000003000,
      name: "image.png".to_string(),
      content_type: Some("image/png".to_string()),
    },
  ];

  let serialized = serialize_child_entries(&entries, hash_length);
  let deserialized = deserialize_child_entries(&serialized, hash_length).unwrap();

  assert_eq!(entries, deserialized);
}

#[test]
fn test_child_entry_with_64_byte_hash() {
  let hash_length = 64;
  let entry = ChildEntry {
    entry_type: 1,
    hash: vec![0xEF; hash_length],
    total_size: 8192,
    created_at: 1700000000000,
    updated_at: 1700000000000,
    name: "sha512-file.bin".to_string(),
    content_type: None,
  };

  let serialized = entry.serialize(hash_length);
  let (deserialized, _) =
    ChildEntry::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.hash.len(), 64);
  assert_eq!(deserialized, entry);
}

#[test]
fn test_child_entry_empty_list_roundtrip() {
  let serialized = serialize_child_entries(&[], 32);
  let deserialized = deserialize_child_entries(&serialized, 32).unwrap();
  assert!(deserialized.is_empty());
}

#[test]
fn test_child_entry_deserialize_truncated_data() {
  let result = ChildEntry::deserialize(&[0x01], 32);
  assert!(result.is_err());
}

// ─── Path utility tests ────────────────────────────────────────────────────

#[test]
fn test_path_normalize_basic() {
  assert_eq!(normalize_path("/myapp/users"), "/myapp/users");
  assert_eq!(normalize_path("myapp/users"), "/myapp/users");
}

#[test]
fn test_path_normalize_double_slashes() {
  assert_eq!(normalize_path("/myapp//users"), "/myapp/users");
  assert_eq!(normalize_path("//myapp///users//"), "/myapp/users");
}

#[test]
fn test_path_normalize_trailing_slash() {
  assert_eq!(normalize_path("/myapp/users/"), "/myapp/users");
  assert_eq!(normalize_path("/myapp/"), "/myapp");
}

#[test]
fn test_path_normalize_root() {
  assert_eq!(normalize_path("/"), "/");
  assert_eq!(normalize_path("///"), "/");
}

#[test]
fn test_path_normalize_whitespace() {
  assert_eq!(normalize_path("  /myapp  "), "/myapp");
  assert_eq!(normalize_path("  "), "/");
}

#[test]
fn test_path_normalize_empty() {
  assert_eq!(normalize_path(""), "/");
}

#[test]
fn test_parent_path() {
  assert_eq!(parent_path("/myapp/users/alice.json"), Some("/myapp/users".to_string()));
  assert_eq!(parent_path("/myapp"), Some("/".to_string()));
  assert_eq!(parent_path("/"), None);
}

#[test]
fn test_parent_path_deep_nesting() {
  assert_eq!(
    parent_path("/a/b/c/d/e"),
    Some("/a/b/c/d".to_string())
  );
}

#[test]
fn test_file_name_extraction() {
  assert_eq!(file_name("/myapp/users/alice.json"), Some("alice.json"));
  assert_eq!(file_name("/myapp"), Some("myapp"));
  assert_eq!(file_name("/"), None);
}

#[test]
fn test_file_name_no_slashes() {
  assert_eq!(file_name("standalone.txt"), Some("standalone.txt"));
}

#[test]
fn test_path_segments() {
  assert_eq!(path_segments("/myapp/users"), vec!["myapp", "users"]);
  assert_eq!(path_segments("/"), Vec::<&str>::new());
  assert_eq!(path_segments("/a/b/c"), vec!["a", "b", "c"]);
}

#[test]
fn test_path_segments_no_leading_slash() {
  assert_eq!(path_segments("myapp/users"), vec!["myapp", "users"]);
}

#[test]
fn test_path_segments_single() {
  assert_eq!(path_segments("/myapp"), vec!["myapp"]);
}
