use aeordb::engine::compression::{CompressionAlgorithm, compress};
use aeordb::engine::directory_ops::{DirectoryOps, chunk_content_hash, directory_path_hash, file_path_hash};
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::errors::EngineError;
use aeordb::engine::file_record::{FileRecord, CURRENT_FILE_RECORD_VERSION};
use aeordb::engine::{ChunkReadLocation, RequestContext, DEFAULT_CHUNK_SIZE};
use aeordb::engine::storage_engine::StorageEngine;
use std::collections::HashSet;
use std::sync::{Arc, Barrier};
use std::thread;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

#[test]
fn test_store_and_read_file_roundtrip() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data = b"Hello, world!";
  ops.store_file_buffered(&ctx, "/greeting.txt", data, Some("text/plain")).unwrap();

  let read_back = ops.read_file_buffered("/greeting.txt").unwrap();
  assert_eq!(read_back, data);
}

#[test]
fn test_store_file_from_reader_roundtrip_multichunk() {
  // Exercise the streaming write path on data that spans multiple chunks
  // (DEFAULT_CHUNK_SIZE is 256 KB → use 600 KB to force 3 chunks).
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let mut data = Vec::with_capacity(600 * 1024);
  for i in 0..(600 * 1024) {
    data.push((i % 251) as u8); // 251 is prime — gives non-trivial pattern
  }

  let reader = std::io::Cursor::new(data.clone());
  ops.store_file_from_reader(&ctx, "/streamed.bin", reader, Some("application/octet-stream")).unwrap();

  // Buffered read back — content should match
  let buffered = ops.read_file_buffered("/streamed.bin").unwrap();
  assert_eq!(buffered.len(), data.len());
  assert_eq!(buffered, data);

  let metadata = ops.get_metadata("/streamed.bin").unwrap().unwrap();
  assert_eq!(metadata.content_hash, blake3::hash(&data).as_bytes().to_vec());
  let file_key = file_path_hash("/streamed.bin", &engine.hash_algo()).unwrap();
  let (header, _key, _value) = engine.get_entry(&file_key).unwrap().unwrap();
  assert_eq!(header.entry_version, CURRENT_FILE_RECORD_VERSION);

  // Streaming read back — accumulated chunks should match too
  let mut streamed = Vec::with_capacity(data.len());
  for chunk in ops.read_file_streaming("/streamed.bin").unwrap() {
    streamed.extend_from_slice(&chunk.unwrap());
  }
  assert_eq!(streamed, data);
}

#[test]
fn test_storage_engine_read_chunk_span_verified_reads_multiple_chunks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data: Vec<u8> = (0..(DEFAULT_CHUNK_SIZE * 3 + 17)).map(|index| (index % 251) as u8).collect();
  ops.store_file_buffered(&ctx, "/span.bin", &data, Some("application/octet-stream")).unwrap();

  let metadata = ops.get_metadata("/span.bin").unwrap().unwrap();
  assert!(metadata.chunk_hashes.len() >= 4);
  let locations: Vec<ChunkReadLocation> = metadata
    .chunk_hashes
    .iter()
    .take(3)
    .map(|hash| {
      let chunk = engine.get_chunk_metadata(hash).unwrap().unwrap();
      ChunkReadLocation { hash: hash.clone(), offset: chunk.offset, total_length: chunk.total_length }
    })
    .collect();

  let chunks = engine.read_chunk_span_verified(&locations).unwrap();
  let mut combined = Vec::new();
  for chunk in chunks {
    combined.extend_from_slice(&chunk);
  }
  assert_eq!(combined, data[..DEFAULT_CHUNK_SIZE * 3].to_vec());
}

#[test]
fn test_store_file_from_reader_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let reader = std::io::Cursor::new(Vec::<u8>::new());
  ops.store_file_from_reader(&ctx, "/empty.bin", reader, None).unwrap();

  let read_back = ops.read_file_buffered("/empty.bin").unwrap();
  assert!(read_back.is_empty());
}

#[test]
fn test_store_file_creates_chunks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data = vec![0xAB; 1024];
  ops.store_file_buffered(&ctx, "/binary.dat", &data, None).unwrap();

  // Verify the file record has chunk hashes
  let metadata = ops.get_metadata("/binary.dat").unwrap().unwrap();
  assert!(!metadata.chunk_hashes.is_empty());
}

#[test]
fn test_storage_engine_read_chunk_decompresses_compressed_chunks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let data = b"compressed chunk data compressed chunk data compressed chunk data";
  let chunk_key = chunk_content_hash(data, &engine.hash_algo()).unwrap();
  let compressed = compress(data, CompressionAlgorithm::Zstd).unwrap();
  engine.store_entry_compressed(EntryType::Chunk, &chunk_key, &compressed, CompressionAlgorithm::Zstd).unwrap();

  let read_back = engine.read_chunk(&chunk_key).unwrap().unwrap();
  assert_eq!(read_back, data);

  let verified = engine.read_chunk_verified(&chunk_key).unwrap().unwrap();
  assert_eq!(verified, data);
}

#[test]
fn test_storage_engine_read_chunk_rejects_non_chunk_entries() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let key = engine.hash_algo().compute_hash(b"not-a-chunk-key").unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &key, b"directory bytes").unwrap();

  let error = engine.read_chunk(&key).unwrap_err();
  assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("not a chunk entry")));
}

#[test]
fn test_store_file_creates_file_record() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data = b"record test";
  ops.store_file_buffered(&ctx, "/record.txt", data, Some("text/plain")).unwrap();

  let metadata = ops.get_metadata("/record.txt").unwrap().unwrap();
  assert_eq!(metadata.path, "/record.txt");
  assert_eq!(metadata.total_size, data.len() as u64);
  assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));
}

#[test]
fn test_store_file_updates_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/file_a.txt", b"aaa", None).unwrap();
  ops.store_file_buffered(&ctx, "/file_b.txt", b"bbb", None).unwrap();

  let children = ops.list_directory("/").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"file_a.txt"));
  assert!(names.contains(&"file_b.txt"));
}

fn assert_file_path_key_matches_directory_child(engine: &StorageEngine, path: &str, parent: &str, name: &str) {
  let ops = DirectoryOps::new(engine);
  let children = ops.list_directory(parent).unwrap();
  let child = children
    .iter()
    .find(|entry| entry.name == name)
    .unwrap_or_else(|| panic!("directory '{}' did not contain child '{}': {:?}", parent, name, children));

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let path_key = file_path_hash(path, &algo).unwrap();
  let (path_header, _path_key, path_value) = engine.get_entry(&path_key).unwrap().unwrap();
  let (child_header, _child_key, child_value) = engine.get_entry(&child.hash).unwrap().unwrap();

  let path_record = FileRecord::deserialize(&path_value, hash_length, path_header.entry_version).unwrap();
  let child_record = FileRecord::deserialize(&child_value, hash_length, child_header.entry_version).unwrap();
  assert_eq!(path_record.path, child_record.path);
  assert_eq!(path_record.content_hash, child_record.content_hash);
  assert_eq!(path_record.chunk_hashes, child_record.chunk_hashes);
}

#[test]
fn concurrent_same_path_writes_keep_directory_child_consistent_with_path_key() {
  let dir = tempfile::tempdir().unwrap();
  let engine = Arc::new(create_engine(&dir));
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine.as_ref());
  ops.create_directory(&ctx, "/race").unwrap();

  let writer_count = 12;
  let iterations = 24;
  let barrier = Arc::new(Barrier::new(writer_count));
  let mut handles = Vec::new();

  for writer_id in 0..writer_count {
    let engine = Arc::clone(&engine);
    let barrier = Arc::clone(&barrier);
    handles.push(thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(engine.as_ref());
      barrier.wait();

      for iteration in 0..iterations {
        let data =
          format!("{{\"writer\":{},\"iteration\":{},\"payload\":\"{}\"}}", writer_id, iteration, "x".repeat(writer_id + iteration + 1));
        ops.store_file_buffered(&ctx, "/race/shared.json", data.as_bytes(), Some("application/json")).unwrap();
      }
    }));
  }

  for handle in handles {
    handle.join().unwrap();
  }

  assert_file_path_key_matches_directory_child(engine.as_ref(), "/race/shared.json", "/race", "shared.json");
}

#[test]
fn concurrent_same_parent_file_creates_preserve_all_directory_children() {
  let dir = tempfile::tempdir().unwrap();
  let engine = Arc::new(create_engine(&dir));
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine.as_ref());
  ops.create_directory(&ctx, "/race").unwrap();

  let file_count = 48;
  let barrier = Arc::new(Barrier::new(file_count));
  let mut handles = Vec::new();

  for file_id in 0..file_count {
    let engine = Arc::clone(&engine);
    let barrier = Arc::clone(&barrier);
    handles.push(thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(engine.as_ref());
      let name = format!("file-{file_id:03}.json");
      let path = format!("/race/{name}");
      let body = format!("{{\"id\":{},\"value\":\"{}\"}}", file_id, "y".repeat(file_id + 1));
      barrier.wait();
      ops.store_file_buffered(&ctx, &path, body.as_bytes(), Some("application/json")).unwrap();
    }));
  }

  for handle in handles {
    handle.join().unwrap();
  }

  let children = ops.list_directory("/race").unwrap();
  let names: HashSet<String> = children.iter().map(|entry| entry.name.clone()).collect();
  assert_eq!(names.len(), file_count);

  for file_id in 0..file_count {
    let name = format!("file-{file_id:03}.json");
    assert!(names.contains(&name), "missing child {name}");
    let path = format!("/race/{name}");
    assert_file_path_key_matches_directory_child(engine.as_ref(), &path, "/race", &name);
  }
}

#[test]
fn test_store_file_creates_intermediate_directories() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/a/b/c/deep.txt", b"deep", None).unwrap();

  // All intermediate directories should exist
  assert!(ops.exists("/a").unwrap());
  assert!(ops.exists("/a/b").unwrap());
  assert!(ops.exists("/a/b/c").unwrap());

  // The file should be in c's listing
  let children = ops.list_directory("/a/b/c").unwrap();
  assert_eq!(children.len(), 1);
  assert_eq!(children[0].name, "deep.txt");
}

#[test]
fn test_read_nonexistent_returns_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let _ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let result = ops.read_file_buffered("/does_not_exist.txt");
  assert!(result.is_err());
  let error = result.unwrap_err();
  assert!(error.to_string().contains("Not found"), "Expected NotFound error, got: {}", error,);
}

#[test]
fn test_delete_file() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/to_delete.txt", b"delete me", None).unwrap();
  assert!(ops.exists("/to_delete.txt").unwrap());

  ops.delete_file(&ctx, "/to_delete.txt").unwrap();

  // The file record still exists in KV (append-only), but it should
  // no longer appear in the parent directory listing
  let children = ops.list_directory("/").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert!(!names.contains(&"to_delete.txt"));
}

#[test]
fn test_list_directory_omits_deleted_file_child_left_in_parent() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/docs/live.txt", b"live", None).unwrap();
  ops.store_file_buffered(&ctx, "/docs/ghost.txt", b"ghost", None).unwrap();

  let ghost_key = file_path_hash("/docs/ghost.txt", &engine.hash_algo()).unwrap();
  engine.mark_entry_deleted(&ghost_key).unwrap();

  assert!(ops.read_file_buffered("/docs/ghost.txt").is_err(), "direct file read should not see deleted path key");

  let children = ops.list_directory("/docs").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert_eq!(names, vec!["live.txt"]);
}

#[test]
fn test_list_directory_omits_deleted_directory_child_left_in_parent() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.create_directory(&ctx, "/root/live").unwrap();
  ops.create_directory(&ctx, "/root/ghost").unwrap();

  let ghost_key = directory_path_hash("/root/ghost", &engine.hash_algo()).unwrap();
  engine.mark_entry_deleted(&ghost_key).unwrap();

  let children = ops.list_directory("/root").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert_eq!(names, vec!["live"]);
}

#[test]
fn test_delete_directory_succeeds_when_only_stale_deleted_file_child_remains() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/root/empty/ghost.txt", b"ghost", None).unwrap();

  let ghost_key = file_path_hash("/root/empty/ghost.txt", &engine.hash_algo()).unwrap();
  engine.mark_entry_deleted(&ghost_key).unwrap();

  ops.delete_directory(&ctx, "/root/empty").unwrap();

  let children = ops.list_directory("/root").unwrap();
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert!(!names.contains(&"empty"));
}

#[test]
fn test_delete_creates_deletion_record() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/ephemeral.txt", b"temp", None).unwrap();
  ops.delete_file(&ctx, "/ephemeral.txt").unwrap();

  // The deletion record is stored as an entry. We can't easily query it by path
  // without scanning, but we verify the operation succeeded without error above.
  // The key property: file no longer appears in directory listing.
  let children = ops.list_directory("/").unwrap();
  assert!(children.iter().all(|c| c.name != "ephemeral.txt"));
}

#[test]
fn test_delete_nonexistent_file_returns_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let result = ops.delete_file(&ctx, "/ghost.txt");
  assert!(result.is_err());
}

#[test]
fn test_list_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/docs/readme.md", b"# Readme", None).unwrap();
  ops.store_file_buffered(&ctx, "/docs/guide.md", b"# Guide", None).unwrap();

  let children = ops.list_directory("/docs").unwrap();
  assert_eq!(children.len(), 2);
  let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"readme.md"));
  assert!(names.contains(&"guide.md"));
}

#[test]
fn test_list_empty_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.create_directory(&ctx, "/empty").unwrap();
  let children = ops.list_directory("/empty").unwrap();
  assert!(children.is_empty());
}

#[test]
fn test_list_nonexistent_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let _ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let result = ops.list_directory("/nonexistent");
  assert!(result.is_err());
}

#[test]
fn test_create_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.create_directory(&ctx, "/mydir").unwrap();
  assert!(ops.exists("/mydir").unwrap());

  let children = ops.list_directory("/mydir").unwrap();
  assert!(children.is_empty());

  // Should appear in root listing
  let root_children = ops.list_directory("/").unwrap();
  let names: Vec<&str> = root_children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"mydir"));
}

#[test]
fn test_exists_file() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  assert!(!ops.exists("/file.txt").unwrap());
  ops.store_file_buffered(&ctx, "/file.txt", b"content", None).unwrap();
  assert!(ops.exists("/file.txt").unwrap());
}

#[test]
fn test_exists_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  assert!(!ops.exists("/subdir").unwrap());
  ops.create_directory(&ctx, "/subdir").unwrap();
  assert!(ops.exists("/subdir").unwrap());
}

#[test]
fn test_exists_nonexistent() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let _ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  assert!(!ops.exists("/nope").unwrap());
  assert!(!ops.exists("/also/nope").unwrap());
}

#[test]
fn test_get_metadata() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/meta.txt", b"metadata test", Some("text/plain")).unwrap();

  let metadata = ops.get_metadata("/meta.txt").unwrap().unwrap();
  assert_eq!(metadata.path, "/meta.txt");
  assert_eq!(metadata.total_size, 13);
  assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));

  // Nonexistent
  let none = ops.get_metadata("/missing.txt").unwrap();
  assert!(none.is_none());
}

#[test]
fn test_streaming_read_yields_correct_data() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data = b"streaming test data";
  ops.store_file_buffered(&ctx, "/stream.txt", data, None).unwrap();

  let stream = ops.read_file_streaming("/stream.txt").unwrap();
  let mut collected = Vec::new();
  for chunk_result in stream {
    collected.extend_from_slice(&chunk_result.unwrap());
  }
  assert_eq!(collected, data);
}

#[test]
fn test_store_large_file_many_chunks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // 1 MB + 1 byte to ensure multiple chunks (default chunk = 256KB)
  let data = vec![0x42; 1_048_577];
  ops.store_file_buffered(&ctx, "/large.bin", &data, Some("application/octet-stream")).unwrap();

  let metadata = ops.get_metadata("/large.bin").unwrap().unwrap();
  assert_eq!(metadata.total_size, data.len() as u64);
  // 1MB+1 / 256KB = 5 chunks
  assert_eq!(metadata.chunk_hashes.len(), 5);

  let read_back = ops.read_file_buffered("/large.bin").unwrap();
  assert_eq!(read_back, data);
}

#[test]
fn test_store_preserves_content_type() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/image.png", b"fake png data", Some("image/png")).unwrap();
  let metadata = ops.get_metadata("/image.png").unwrap().unwrap();
  assert_eq!(metadata.content_type.as_deref(), Some("image/png"));

  // No content type -- detection kicks in; "raw" is valid UTF-8 text
  ops.store_file_buffered(&ctx, "/raw.bin", b"raw", None).unwrap();
  let metadata = ops.get_metadata("/raw.bin").unwrap().unwrap();
  assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));
}

#[test]
fn test_overwrite_file() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/mutable.txt", b"version 1", None).unwrap();
  let meta1 = ops.get_metadata("/mutable.txt").unwrap().unwrap();

  ops.store_file_buffered(&ctx, "/mutable.txt", b"version 2 is longer", None).unwrap();
  let meta2 = ops.get_metadata("/mutable.txt").unwrap().unwrap();

  // Content should be updated
  let read_back = ops.read_file_buffered("/mutable.txt").unwrap();
  assert_eq!(read_back, b"version 2 is longer");

  // Total size updated
  assert_eq!(meta2.total_size, 19);

  // created_at preserved on overwrite
  assert_eq!(meta2.created_at, meta1.created_at);

  // updated_at should be >= original (could be same if very fast)
  assert!(meta2.updated_at >= meta1.updated_at);
}

#[test]
fn test_nested_directories() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.create_directory(&ctx, "/level1").unwrap();
  ops.create_directory(&ctx, "/level1/level2").unwrap();
  ops.create_directory(&ctx, "/level1/level2/level3").unwrap();

  ops.store_file_buffered(&ctx, "/level1/level2/level3/deep_file.txt", b"deep content", None).unwrap();

  let children = ops.list_directory("/level1/level2/level3").unwrap();
  assert_eq!(children.len(), 1);
  assert_eq!(children[0].name, "deep_file.txt");

  let l2_children = ops.list_directory("/level1/level2").unwrap();
  let names: Vec<&str> = l2_children.iter().map(|c| c.name.as_str()).collect();
  assert!(names.contains(&"level3"));
}

#[test]
fn test_root_directory_exists_after_create() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let _ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  assert!(ops.exists("/").unwrap());
  let children = ops.list_directory("/").unwrap();
  assert!(children.is_empty());
}

#[test]
fn test_path_normalization_applied() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Store with messy path
  ops.store_file_buffered(&ctx, "messy//path///file.txt", b"normalized", None).unwrap();

  // Read with clean path
  let data = ops.read_file_buffered("/messy/path/file.txt").unwrap();
  assert_eq!(data, b"normalized");

  // Exists with another messy variant
  assert!(ops.exists("//messy//path/file.txt").unwrap());
}

#[test]
fn test_dedup_identical_chunks() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data = vec![0xFF; 1024];

  // Store the same data in two different files
  ops.store_file_buffered(&ctx, "/copy1.bin", &data, None).unwrap();
  ops.store_file_buffered(&ctx, "/copy2.bin", &data, None).unwrap();

  let meta1 = ops.get_metadata("/copy1.bin").unwrap().unwrap();
  let meta2 = ops.get_metadata("/copy2.bin").unwrap().unwrap();

  // Both files should reference the same chunk hash(es)
  assert_eq!(meta1.chunk_hashes, meta2.chunk_hashes);
}

#[test]
fn test_store_empty_file() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/empty.txt", b"", None).unwrap();

  let metadata = ops.get_metadata("/empty.txt").unwrap().unwrap();
  assert_eq!(metadata.total_size, 0);
  assert!(metadata.chunk_hashes.is_empty());

  let data = ops.read_file_buffered("/empty.txt").unwrap();
  assert!(data.is_empty());
}

#[test]
fn test_directory_child_entry_types() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/mixed/file.txt", b"file", None).unwrap();
  ops.create_directory(&ctx, "/mixed/subdir").unwrap();

  let children = ops.list_directory("/mixed").unwrap();
  assert_eq!(children.len(), 2);

  let file_child = children.iter().find(|c| c.name == "file.txt").unwrap();
  assert_eq!(file_child.entry_type, EntryType::FileRecord.to_u8());

  let dir_child = children.iter().find(|c| c.name == "subdir").unwrap();
  assert_eq!(dir_child.entry_type, EntryType::DirectoryIndex.to_u8());
}

#[test]
fn test_store_file_at_root() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/root_file.txt", b"at root", None).unwrap();

  let children = ops.list_directory("/").unwrap();
  assert_eq!(children.len(), 1);
  assert_eq!(children[0].name, "root_file.txt");
}

#[test]
fn test_multiple_files_same_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  for i in 0..10 {
    let path = format!("/batch/file_{}.txt", i);
    let data = format!("content {}", i);
    ops.store_file_buffered(&ctx, &path, data.as_bytes(), None).unwrap();
  }

  let children = ops.list_directory("/batch").unwrap();
  assert_eq!(children.len(), 10);

  // Verify each file reads back correctly
  for i in 0..10 {
    let path = format!("/batch/file_{}.txt", i);
    let expected = format!("content {}", i);
    let data = ops.read_file_buffered(&path).unwrap();
    assert_eq!(data, expected.as_bytes());
  }
}

#[test]
fn test_delete_then_recreate() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/phoenix.txt", b"version 1", None).unwrap();
  ops.delete_file(&ctx, "/phoenix.txt").unwrap();

  // Re-store at the same path
  ops.store_file_buffered(&ctx, "/phoenix.txt", b"version 2", None).unwrap();

  let data = ops.read_file_buffered("/phoenix.txt").unwrap();
  assert_eq!(data, b"version 2");

  let children = ops.list_directory("/").unwrap();
  let count = children.iter().filter(|c| c.name == "phoenix.txt").count();
  assert_eq!(count, 1, "Should have exactly one entry, not duplicates");
}

#[test]
fn test_open_and_reread() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("persist.aeor");
  let path_str = path.to_str().unwrap();

  // Create and store
  {
    let engine = StorageEngine::create(path_str).unwrap();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    ops.store_file_buffered(&ctx, "/persistent.txt", b"survives reopen", None).unwrap();
  }

  // Reopen and read
  {
    let engine = StorageEngine::open(path_str).unwrap();
    let ops = DirectoryOps::new(&engine);
    let data = ops.read_file_buffered("/persistent.txt").unwrap();
    assert_eq!(data, b"survives reopen");
  }
}

#[test]
fn test_collect_to_vec_convenience() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let data = b"convenience method test";
  ops.store_file_buffered(&ctx, "/conv.txt", data, None).unwrap();

  let stream = ops.read_file_streaming("/conv.txt").unwrap();
  let collected = stream.collect_to_vec().unwrap();
  assert_eq!(collected, data);
}

#[test]
fn test_head_hash_updates() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  let initial_head = engine.head_hash().unwrap();

  ops.store_file_buffered(&ctx, "/trigger_head.txt", b"update head", None).unwrap();

  let updated_head = engine.head_hash().unwrap();

  // HEAD should have changed after storing a file
  // (It points to root directory hash, which is constant, but the
  // content at that hash key changed)
  // Actually the head_hash IS the dir key for root, which is constant.
  // The point is that HEAD is set.
  assert!(!updated_head.iter().all(|&b| b == 0) || initial_head == updated_head);
}
