use aeordb::filesystem::directory_entry::EntryType;
use aeordb::filesystem::path_resolver::{PathError, PathResolver};
use aeordb::storage::ChunkStore;
use redb::Database;
use std::sync::Arc;
use tempfile::NamedTempFile;

fn create_resolver() -> PathResolver {
  let temp_file = NamedTempFile::new().expect("failed to create temp file");
  let database = Arc::new(
    Database::create(temp_file.path()).expect("failed to create database"),
  );
  let chunk_store = ChunkStore::new_in_memory();
  PathResolver::new(database, chunk_store)
}

#[test]
fn test_ensure_root_creates_root_directory() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  // Root should exist now.
  assert!(resolver.exists("/").expect("exists check failed"));
}

#[test]
fn test_store_file_at_root() {
  let resolver = create_resolver();
  let data = b"hello world";

  let entry = resolver
    .store_file("/greeting.txt", data, Some("text/plain"))
    .expect("store_file failed");

  assert_eq!(entry.name, "greeting.txt");
  assert_eq!(entry.entry_type, EntryType::File);
  assert_eq!(entry.total_size, data.len() as u64);
  assert_eq!(entry.content_type, Some("text/plain".to_string()));
  assert!(!entry.chunk_hashes.is_empty());
}

#[test]
fn test_store_file_creates_intermediate_directories() {
  let resolver = create_resolver();
  let data = b"nested content";

  resolver
    .store_file("/myapp/users/abc123", data, None)
    .expect("store_file failed");

  // All intermediate directories should exist.
  assert!(resolver.exists("/myapp").expect("exists failed"));
  assert!(resolver.exists("/myapp/users").expect("exists failed"));
  assert!(resolver.exists("/myapp/users/abc123").expect("exists failed"));

  // Intermediate entries should be directories.
  let myapp_metadata = resolver
    .get_metadata("/myapp")
    .expect("get_metadata failed")
    .expect("myapp should exist");
  assert_eq!(myapp_metadata.entry_type, EntryType::Directory);

  let users_metadata = resolver
    .get_metadata("/myapp/users")
    .expect("get_metadata failed")
    .expect("users should exist");
  assert_eq!(users_metadata.entry_type, EntryType::Directory);
}

#[test]
fn test_store_and_read_file_roundtrip() {
  let resolver = create_resolver();
  let data = b"the quick brown fox jumps over the lazy dog";

  resolver
    .store_file("/docs/animals.txt", data, Some("text/plain"))
    .expect("store_file failed");

  let stream = resolver
    .read_file_streaming("/docs/animals.txt")
    .expect("read_file_streaming failed");

  let read_data = stream.collect_to_vec().expect("collect_to_vec failed");
  assert_eq!(read_data, data);
}

#[test]
fn test_store_and_read_large_file() {
  let resolver = create_resolver();

  // 1MB+ of data, should span multiple chunks.
  let data: Vec<u8> = (0..1_100_000)
    .map(|index| (index % 256) as u8)
    .collect();

  resolver
    .store_file("/large/data.bin", &data, Some("application/octet-stream"))
    .expect("store_file failed");

  let stream = resolver
    .read_file_streaming("/large/data.bin")
    .expect("read_file_streaming failed");

  let read_data = stream.collect_to_vec().expect("collect_to_vec failed");
  assert_eq!(read_data.len(), data.len());
  assert_eq!(read_data, data);
}

#[test]
fn test_read_nonexistent_returns_not_found() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let result = resolver.read_file_streaming("/no/such/file.txt");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotFound(path) => assert!(path.contains("no/such/file.txt")),
    other => panic!("expected NotFound, got: {other:?}"),
  }
}

#[test]
fn test_get_metadata_for_file() {
  let resolver = create_resolver();
  let data = b"metadata test";

  resolver
    .store_file("/meta/test.json", data, Some("application/json"))
    .expect("store_file failed");

  let metadata = resolver
    .get_metadata("/meta/test.json")
    .expect("get_metadata failed")
    .expect("entry should exist");

  assert_eq!(metadata.name, "test.json");
  assert_eq!(metadata.entry_type, EntryType::File);
  assert_eq!(metadata.total_size, data.len() as u64);
  assert_eq!(metadata.content_type, Some("application/json".to_string()));
}

#[test]
fn test_get_metadata_for_directory() {
  let resolver = create_resolver();

  resolver
    .create_directory("/configs/app")
    .expect("create_directory failed");

  let metadata = resolver
    .get_metadata("/configs/app")
    .expect("get_metadata failed")
    .expect("directory should exist");

  assert_eq!(metadata.name, "app");
  assert_eq!(metadata.entry_type, EntryType::Directory);
}

#[test]
fn test_get_metadata_returns_none_for_nonexistent() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let metadata = resolver
    .get_metadata("/nonexistent")
    .expect("get_metadata failed");

  assert!(metadata.is_none());
}

#[test]
fn test_delete_file() {
  let resolver = create_resolver();
  let data = b"to be deleted";

  resolver
    .store_file("/ephemeral.txt", data, None)
    .expect("store_file failed");

  assert!(resolver.exists("/ephemeral.txt").expect("exists failed"));

  let removed = resolver
    .delete_file("/ephemeral.txt")
    .expect("delete_file failed");

  assert_eq!(removed.name, "ephemeral.txt");
  assert_eq!(removed.entry_type, EntryType::File);
  assert!(!resolver.exists("/ephemeral.txt").expect("exists failed"));
}

#[test]
fn test_delete_nonexistent_returns_error() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let result = resolver.delete_file("/ghost.txt");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotFound(path) => assert!(path.contains("ghost.txt")),
    other => panic!("expected NotFound, got: {other:?}"),
  }
}

#[test]
fn test_delete_directory_entry_not_a_file() {
  let resolver = create_resolver();

  resolver
    .create_directory("/mydir")
    .expect("create_directory failed");

  let result = resolver.delete_file("/mydir");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotAFile(_) => {}
    other => panic!("expected NotAFile, got: {other:?}"),
  }
}

#[test]
fn test_list_directory() {
  let resolver = create_resolver();

  resolver
    .store_file("/docs/readme.txt", b"readme", None)
    .expect("store_file failed");
  resolver
    .store_file("/docs/guide.txt", b"guide", None)
    .expect("store_file failed");
  resolver
    .create_directory("/docs/images")
    .expect("create_directory failed");

  let entries = resolver
    .list_directory("/docs")
    .expect("list_directory failed");

  assert_eq!(entries.len(), 3);
  // Sorted by name.
  assert_eq!(entries[0].name, "guide.txt");
  assert_eq!(entries[1].name, "images");
  assert_eq!(entries[2].name, "readme.txt");
}

#[test]
fn test_list_directory_empty() {
  let resolver = create_resolver();

  resolver
    .create_directory("/empty")
    .expect("create_directory failed");

  let entries = resolver
    .list_directory("/empty")
    .expect("list_directory failed");

  assert!(entries.is_empty());
}

#[test]
fn test_list_nonexistent_directory() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let result = resolver.list_directory("/no_such_dir");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotFound(_) => {}
    other => panic!("expected NotFound, got: {other:?}"),
  }
}

#[test]
fn test_create_directory_explicit() {
  let resolver = create_resolver();

  resolver
    .create_directory("/projects/aeordb")
    .expect("create_directory failed");

  assert!(resolver.exists("/projects").expect("exists failed"));
  assert!(resolver.exists("/projects/aeordb").expect("exists failed"));

  let metadata = resolver
    .get_metadata("/projects/aeordb")
    .expect("get_metadata failed")
    .expect("directory should exist");

  assert_eq!(metadata.entry_type, EntryType::Directory);
}

#[test]
fn test_create_directory_already_exists_is_ok() {
  let resolver = create_resolver();

  resolver
    .create_directory("/stable")
    .expect("first create failed");

  // Should not error on second creation.
  resolver
    .create_directory("/stable")
    .expect("second create should succeed");

  assert!(resolver.exists("/stable").expect("exists failed"));
}

#[test]
fn test_exists_file() {
  let resolver = create_resolver();

  resolver
    .store_file("/present.txt", b"data", None)
    .expect("store_file failed");

  assert!(resolver.exists("/present.txt").expect("exists failed"));
}

#[test]
fn test_exists_directory() {
  let resolver = create_resolver();

  resolver
    .create_directory("/somedir")
    .expect("create_directory failed");

  assert!(resolver.exists("/somedir").expect("exists failed"));
}

#[test]
fn test_exists_nonexistent() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  assert!(!resolver.exists("/nope").expect("exists failed"));
}

#[test]
fn test_deep_nested_path() {
  let resolver = create_resolver();
  let data = b"deep data";

  resolver
    .store_file("/a/b/c/d/e/f/deep.txt", data, None)
    .expect("store_file failed");

  // All intermediate directories should exist.
  for path in &["/a", "/a/b", "/a/b/c", "/a/b/c/d", "/a/b/c/d/e", "/a/b/c/d/e/f"] {
    assert!(
      resolver.exists(path).expect("exists failed"),
      "directory '{path}' should exist",
    );
  }

  let stream = resolver
    .read_file_streaming("/a/b/c/d/e/f/deep.txt")
    .expect("read failed");

  let read_data = stream.collect_to_vec().expect("collect failed");
  assert_eq!(read_data, data);
}

#[test]
fn test_store_file_preserves_content_type() {
  let resolver = create_resolver();

  resolver
    .store_file("/typed.json", b"{}", Some("application/json"))
    .expect("store_file failed");

  let metadata = resolver
    .get_metadata("/typed.json")
    .expect("get_metadata failed")
    .expect("entry should exist");

  assert_eq!(metadata.content_type, Some("application/json".to_string()));
}

#[test]
fn test_store_file_without_content_type() {
  let resolver = create_resolver();

  resolver
    .store_file("/untyped.bin", b"\x00\x01\x02", None)
    .expect("store_file failed");

  let metadata = resolver
    .get_metadata("/untyped.bin")
    .expect("get_metadata failed")
    .expect("entry should exist");

  assert_eq!(metadata.content_type, None);
}

#[test]
fn test_overwrite_existing_file() {
  let resolver = create_resolver();

  resolver
    .store_file("/mutable.txt", b"version 1", Some("text/plain"))
    .expect("first store failed");

  let entry_v2 = resolver
    .store_file("/mutable.txt", b"version 2", Some("text/plain"))
    .expect("second store failed");

  assert_eq!(entry_v2.total_size, 9);

  let stream = resolver
    .read_file_streaming("/mutable.txt")
    .expect("read failed");

  let read_data = stream.collect_to_vec().expect("collect failed");
  assert_eq!(read_data, b"version 2");
}

#[test]
fn test_streaming_read_yields_correct_chunks() {
  let resolver = create_resolver();
  let data = b"chunk data for streaming test";

  resolver
    .store_file("/stream_test.txt", data, None)
    .expect("store_file failed");

  let stream = resolver
    .read_file_streaming("/stream_test.txt")
    .expect("read failed");

  let mut collected = Vec::new();
  let mut chunk_count = 0;
  for chunk_result in stream {
    let chunk_data = chunk_result.expect("chunk read failed");
    assert!(!chunk_data.is_empty(), "each chunk should have data");
    collected.extend(chunk_data);
    chunk_count += 1;
  }

  assert!(chunk_count >= 1, "should yield at least one chunk");
  assert_eq!(collected, data);
}

#[test]
fn test_parse_path_handles_slashes() {
  // We test path handling indirectly through store/read roundtrips.
  let resolver = create_resolver();

  // Leading slash.
  resolver
    .store_file("/leading.txt", b"a", None)
    .expect("store with leading slash failed");

  // Trailing slash -- should still work for the file name.
  // (trailing slash is stripped during parse).
  // We store with a clean path and read with trailing slash variations.
  let stream = resolver
    .read_file_streaming("/leading.txt")
    .expect("read with leading slash failed");
  let read_data = stream.collect_to_vec().expect("collect failed");
  assert_eq!(read_data, b"a");

  // Double slashes in path.
  resolver
    .store_file("//double//slashes//file.txt", b"b", None)
    .expect("store with double slashes failed");

  let stream = resolver
    .read_file_streaming("/double/slashes/file.txt")
    .expect("read normalized path failed");
  let read_data = stream.collect_to_vec().expect("collect failed");
  assert_eq!(read_data, b"b");
}

#[test]
fn test_store_at_dot_config_path() {
  let resolver = create_resolver();
  let data = b"config data";

  resolver
    .store_file("/.config/app/settings.json", data, Some("application/json"))
    .expect("store_file failed");

  assert!(resolver.exists("/.config").expect("exists failed"));
  assert!(resolver.exists("/.config/app").expect("exists failed"));

  let stream = resolver
    .read_file_streaming("/.config/app/settings.json")
    .expect("read failed");

  let read_data = stream.collect_to_vec().expect("collect failed");
  assert_eq!(read_data, data);
}

#[test]
fn test_invalid_empty_path() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let result = resolver.store_file("", b"data", None);
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::InvalidPath(_) => {}
    other => panic!("expected InvalidPath, got: {other:?}"),
  }

  let result = resolver.read_file_streaming("");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::InvalidPath(_) => {}
    other => panic!("expected InvalidPath, got: {other:?}"),
  }
}

#[test]
fn test_read_file_streaming_on_directory_returns_not_a_file() {
  let resolver = create_resolver();

  resolver
    .create_directory("/readdir")
    .expect("create_directory failed");

  let result = resolver.read_file_streaming("/readdir");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotAFile(_) => {}
    other => panic!("expected NotAFile, got: {other:?}"),
  }
}

#[test]
fn test_list_directory_on_file_returns_not_a_directory() {
  let resolver = create_resolver();

  resolver
    .store_file("/afile.txt", b"content", None)
    .expect("store_file failed");

  let result = resolver.list_directory("/afile.txt");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotADirectory(_) => {}
    other => panic!("expected NotADirectory, got: {other:?}"),
  }
}

#[test]
fn test_store_file_over_directory_fails() {
  let resolver = create_resolver();

  resolver
    .create_directory("/conflict")
    .expect("create_directory failed");

  let result = resolver.store_file("/conflict", b"data", None);
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotAFile(_) => {}
    other => panic!("expected NotAFile, got: {other:?}"),
  }
}

#[test]
fn test_store_file_through_existing_file_as_directory_fails() {
  let resolver = create_resolver();

  resolver
    .store_file("/blocker.txt", b"I am a file", None)
    .expect("store_file failed");

  // Now try to store a file through blocker.txt as if it were a directory.
  let result = resolver.store_file("/blocker.txt/child.txt", b"data", None);
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::NotADirectory(_) => {}
    other => panic!("expected NotADirectory, got: {other:?}"),
  }
}

#[test]
fn test_list_root_directory() {
  let resolver = create_resolver();

  resolver
    .store_file("/alpha.txt", b"a", None)
    .expect("store failed");
  resolver
    .store_file("/beta.txt", b"b", None)
    .expect("store failed");
  resolver
    .create_directory("/gamma")
    .expect("create_directory failed");

  let entries = resolver
    .list_directory("/")
    .expect("list_directory failed");

  assert_eq!(entries.len(), 3);
  assert_eq!(entries[0].name, "alpha.txt");
  assert_eq!(entries[1].name, "beta.txt");
  assert_eq!(entries[2].name, "gamma");
}

#[test]
fn test_delete_file_with_empty_path() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let result = resolver.delete_file("");
  assert!(result.is_err());
  match result.unwrap_err() {
    PathError::InvalidPath(_) => {}
    other => panic!("expected InvalidPath, got: {other:?}"),
  }
}

#[test]
fn test_store_empty_file() {
  let resolver = create_resolver();

  let entry = resolver
    .store_file("/empty.bin", b"", None)
    .expect("store_file failed");

  assert_eq!(entry.total_size, 0);
  assert!(entry.chunk_hashes.is_empty());

  let stream = resolver
    .read_file_streaming("/empty.bin")
    .expect("read failed");

  let read_data = stream.collect_to_vec().expect("collect failed");
  assert!(read_data.is_empty());
}

#[test]
fn test_ensure_root_is_idempotent() {
  let resolver = create_resolver();

  resolver.ensure_root().expect("first ensure_root failed");
  resolver.ensure_root().expect("second ensure_root failed");
  resolver.ensure_root().expect("third ensure_root failed");

  assert!(resolver.exists("/").expect("exists failed"));
}

#[test]
fn test_get_metadata_for_root() {
  let resolver = create_resolver();
  resolver.ensure_root().expect("ensure_root failed");

  let metadata = resolver
    .get_metadata("/")
    .expect("get_metadata failed")
    .expect("root should exist");

  assert_eq!(metadata.entry_type, EntryType::Directory);
}

#[test]
fn test_get_metadata_for_root_before_ensure() {
  let resolver = create_resolver();

  let metadata = resolver
    .get_metadata("/")
    .expect("get_metadata failed");

  assert!(metadata.is_none());
}
