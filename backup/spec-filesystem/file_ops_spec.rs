use std::sync::Arc;

use aeordb::filesystem::{ChunkList, FileOperations};
use aeordb::storage::{ChunkConfig, ChunkStorage, InMemoryChunkStorage, hash_data};

fn setup() -> FileOperations {
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let chunk_config = ChunkConfig::default();
  FileOperations::new(storage, chunk_config)
}

fn setup_with_small_chunks() -> FileOperations {
  let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  // 64-byte chunks (64 - 33 header = 31 bytes data capacity).
  let chunk_config = ChunkConfig::new(64).unwrap();
  FileOperations::new(storage, chunk_config)
}

// ─── Store operations ───────────────────────────────────────────────────────

#[test]
fn test_store_small_file_inline_chunk_list() {
  let file_ops = setup();
  let data = b"hello, world!";
  let (chunk_list, total_size) = file_ops.store_file(data).unwrap();

  assert_eq!(total_size, 13);
  match &chunk_list {
    ChunkList::Inline(hashes) => {
      assert_eq!(hashes.len(), 1, "small file should be a single chunk");
    }
    ChunkList::Overflow(_) => panic!("small file should use Inline chunk list"),
  }
}

#[test]
fn test_store_large_file_overflow_chunk_list() {
  let file_ops = setup_with_small_chunks();
  // Data capacity is 31 bytes per chunk. 33 chunks * 31 = 1023 bytes.
  // With 33+ chunks (> INLINE_HASH_LIMIT=32), should overflow.
  let data = vec![0xABu8; 1023];
  let (chunk_list, total_size) = file_ops.store_file(&data).unwrap();

  assert_eq!(total_size, 1023);
  match &chunk_list {
    ChunkList::Overflow(_) => {}
    ChunkList::Inline(hashes) => {
      panic!(
        "file with {} chunks should use Overflow, not Inline",
        hashes.len()
      );
    }
  }
}

#[test]
fn test_store_empty_file() {
  let file_ops = setup();
  let (chunk_list, total_size) = file_ops.store_file(b"").unwrap();

  assert_eq!(total_size, 0);
  match &chunk_list {
    ChunkList::Inline(hashes) => assert!(hashes.is_empty()),
    ChunkList::Overflow(_) => panic!("empty file should use Inline with empty vec"),
  }
}

#[test]
fn test_store_single_byte_file() {
  let file_ops = setup();
  let (chunk_list, total_size) = file_ops.store_file(&[42u8]).unwrap();

  assert_eq!(total_size, 1);
  match &chunk_list {
    ChunkList::Inline(hashes) => assert_eq!(hashes.len(), 1),
    ChunkList::Overflow(_) => panic!("single byte file should be inline"),
  }
}

#[test]
fn test_store_exact_chunk_boundary() {
  let file_ops = setup_with_small_chunks();
  // Data capacity is 31 bytes per chunk. Store exactly 31 bytes.
  let data = vec![0xFFu8; 31];
  let (chunk_list, total_size) = file_ops.store_file(&data).unwrap();

  assert_eq!(total_size, 31);
  match &chunk_list {
    ChunkList::Inline(hashes) => {
      assert_eq!(hashes.len(), 1, "exact boundary should produce exactly 1 chunk");
    }
    ChunkList::Overflow(_) => panic!("small file should be inline"),
  }
}

#[test]
fn test_store_one_byte_over_chunk_boundary() {
  let file_ops = setup_with_small_chunks();
  // 31 + 1 = 32 bytes -> should need 2 chunks.
  let data = vec![0xFFu8; 32];
  let (chunk_list, total_size) = file_ops.store_file(&data).unwrap();

  assert_eq!(total_size, 32);
  match &chunk_list {
    ChunkList::Inline(hashes) => {
      assert_eq!(hashes.len(), 2, "one byte over boundary should produce 2 chunks");
    }
    ChunkList::Overflow(_) => panic!("2-chunk file should be inline"),
  }
}

// ─── Streaming reads ────────────────────────────────────────────────────────

#[test]
fn test_read_file_streaming_yields_correct_chunks() {
  let file_ops = setup_with_small_chunks();
  let data = vec![0xCDu8; 93]; // 93 bytes / 31 per chunk = 3 chunks.
  let (chunk_list, _total_size) = file_ops.store_file(&data).unwrap();

  let mut stream = file_ops.read_file_streaming(&chunk_list).unwrap();
  assert_eq!(stream.chunk_count(), 3);

  let mut chunks_read = 0;
  for result in stream.by_ref() {
    let chunk_data = result.unwrap();
    assert_eq!(chunk_data.len(), 31);
    assert!(chunk_data.iter().all(|&byte| byte == 0xCD));
    chunks_read += 1;
  }
  assert_eq!(chunks_read, 3);
}

#[test]
fn test_read_file_streaming_concatenation_matches_original() {
  let file_ops = setup_with_small_chunks();
  let data: Vec<u8> = (0..200).map(|i| (i % 256) as u8).collect();
  let (chunk_list, _total_size) = file_ops.store_file(&data).unwrap();

  let mut stream = file_ops.read_file_streaming(&chunk_list).unwrap();
  let mut reconstructed = Vec::new();
  for result in stream.by_ref() {
    reconstructed.extend_from_slice(&result.unwrap());
  }

  assert_eq!(reconstructed, data);
}

#[test]
fn test_read_file_to_vec_matches_original() {
  let file_ops = setup_with_small_chunks();
  let data: Vec<u8> = (0..150).map(|i| (i % 256) as u8).collect();
  let (chunk_list, total_size) = file_ops.store_file(&data).unwrap();

  let result = file_ops.read_file_to_vec(&chunk_list, total_size).unwrap();
  assert_eq!(result, data);
}

#[test]
fn test_read_file_to_vec_refuses_large_files() {
  let file_ops = setup();
  // Pretend we have a huge file by passing a large total_size.
  let chunk_list = ChunkList::Inline(vec![]);
  let result = file_ops.read_file_to_vec(&chunk_list, 20 * 1024 * 1024);
  assert!(result.is_err(), "should refuse files over 10MB");
}

#[test]
fn test_read_file_streaming_empty_file() {
  let file_ops = setup();
  let (chunk_list, _) = file_ops.store_file(b"").unwrap();

  let mut stream = file_ops.read_file_streaming(&chunk_list).unwrap();
  assert_eq!(stream.chunk_count(), 0);
  assert_eq!(stream.remaining(), 0);
  assert!(stream.next().is_none());
}

// ─── File size ──────────────────────────────────────────────────────────────

#[test]
fn test_file_size_accurate() {
  let file_ops = setup_with_small_chunks();
  let data = vec![0u8; 100]; // 100 bytes.
  let (chunk_list, total_size) = file_ops.store_file(&data).unwrap();
  assert_eq!(total_size, 100);

  let computed_size = file_ops.file_size(&chunk_list).unwrap();
  assert_eq!(computed_size, 100);
}

#[test]
fn test_file_size_empty() {
  let file_ops = setup();
  let (chunk_list, _) = file_ops.store_file(b"").unwrap();
  let computed_size = file_ops.file_size(&chunk_list).unwrap();
  assert_eq!(computed_size, 0);
}

// ─── Resolve chunk list ─────────────────────────────────────────────────────

#[test]
fn test_resolve_chunk_list_inline() {
  let file_ops = setup();
  let hashes = vec![hash_data(b"a"), hash_data(b"b")];
  let chunk_list = ChunkList::Inline(hashes.clone());
  let resolved = file_ops.resolve_chunk_list(&chunk_list).unwrap();
  assert_eq!(resolved, hashes);
}

#[test]
fn test_resolve_chunk_list_overflow() {
  let file_ops = setup_with_small_chunks();
  // Store a file large enough to overflow.
  let data = vec![0xAAu8; 1023]; // 33 chunks with 31 byte capacity.
  let (chunk_list, _) = file_ops.store_file(&data).unwrap();

  match &chunk_list {
    ChunkList::Overflow(_) => {}
    ChunkList::Inline(_) => panic!("expected Overflow for this test"),
  }

  let resolved = file_ops.resolve_chunk_list(&chunk_list).unwrap();
  assert_eq!(resolved.len(), 33);
}

#[test]
fn test_resolve_chunk_list_overflow_missing_chunk_fails() {
  let file_ops = setup();
  let fake_overflow_hash = hash_data(b"nonexistent overflow chunk");
  let chunk_list = ChunkList::Overflow(fake_overflow_hash);

  let result = file_ops.resolve_chunk_list(&chunk_list);
  assert!(result.is_err(), "resolving a missing overflow chunk should fail");
}

// ─── Large file test ────────────────────────────────────────────────────────

#[test]
fn test_store_and_stream_large_file() {
  let file_ops = setup();
  // 1 MB file.
  let data: Vec<u8> = (0..1_048_576).map(|i| (i % 256) as u8).collect();
  let (chunk_list, total_size) = file_ops.store_file(&data).unwrap();
  assert_eq!(total_size, 1_048_576);

  // Stream back and verify.
  let mut stream = file_ops.read_file_streaming(&chunk_list).unwrap();
  let mut reconstructed = Vec::new();
  for result in stream.by_ref() {
    reconstructed.extend_from_slice(&result.unwrap());
  }
  assert_eq!(reconstructed.len(), data.len());
  assert_eq!(reconstructed, data);
}

// ─── Error cases ────────────────────────────────────────────────────────────

#[test]
fn test_read_file_streaming_missing_chunk_fails() {
  let file_ops = setup();
  // Create a chunk list with a hash that doesn't exist in storage.
  let fake_hash = hash_data(b"this chunk does not exist");
  let chunk_list = ChunkList::Inline(vec![fake_hash]);

  let mut stream = file_ops.read_file_streaming(&chunk_list).unwrap();
  let result = stream.next().unwrap();
  assert!(result.is_err(), "streaming a missing chunk should fail");
}

#[test]
fn test_file_size_missing_chunk_fails() {
  let file_ops = setup();
  let fake_hash = hash_data(b"missing chunk for size calc");
  let chunk_list = ChunkList::Inline(vec![fake_hash]);

  let result = file_ops.file_size(&chunk_list);
  assert!(result.is_err());
}

#[test]
fn test_stream_remaining_decrements() {
  let file_ops = setup_with_small_chunks();
  let data = vec![0u8; 62]; // 2 chunks.
  let (chunk_list, _) = file_ops.store_file(&data).unwrap();

  let mut stream = file_ops.read_file_streaming(&chunk_list).unwrap();
  assert_eq!(stream.remaining(), 2);

  stream.next().unwrap().unwrap();
  assert_eq!(stream.remaining(), 1);

  stream.next().unwrap().unwrap();
  assert_eq!(stream.remaining(), 0);

  assert!(stream.next().is_none());
}
