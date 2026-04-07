use aeordb::engine::compression::{
  CompressionAlgorithm, compress, decompress, should_compress,
};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

// --- CompressionAlgorithm enum tests ---

#[test]
fn test_compression_algorithm_enum_roundtrip() {
  let algorithms = [
    (0x00, CompressionAlgorithm::None),
    (0x01, CompressionAlgorithm::Zstd),
  ];

  for (byte_value, expected) in algorithms {
    let parsed = CompressionAlgorithm::from_u8(byte_value)
      .expect("Failed to parse compression algorithm");
    assert_eq!(parsed, expected);
    assert_eq!(parsed.to_u8(), byte_value);
  }
}

#[test]
fn test_compression_algorithm_from_u8_invalid() {
  assert!(CompressionAlgorithm::from_u8(0x02).is_none());
  assert!(CompressionAlgorithm::from_u8(0xFF).is_none());
  assert!(CompressionAlgorithm::from_u8(0x80).is_none());
}

// --- compress/decompress unit tests ---

#[test]
fn test_compress_decompress_roundtrip_zstd() {
  let original = b"Hello, world! This is a test of zstd compression.";
  let compressed = compress(original, CompressionAlgorithm::Zstd)
    .expect("Compression failed");
  let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd)
    .expect("Decompression failed");
  assert_eq!(decompressed, original);
}

#[test]
fn test_compress_none_is_identity() {
  let original = b"No compression applied here.";
  let compressed = compress(original, CompressionAlgorithm::None)
    .expect("Compression failed");
  assert_eq!(compressed, original);

  let decompressed = decompress(&compressed, CompressionAlgorithm::None)
    .expect("Decompression failed");
  assert_eq!(decompressed, original);
}

#[test]
fn test_compress_empty_data_zstd() {
  let original: &[u8] = b"";
  let compressed = compress(original, CompressionAlgorithm::Zstd)
    .expect("Compression failed");
  let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd)
    .expect("Decompression failed");
  assert_eq!(decompressed, original);
}

#[test]
fn test_compressed_file_smaller_than_original() {
  // Highly compressible data: repeated pattern
  let original: Vec<u8> = "abcdefghij".repeat(1000).into_bytes();
  let compressed = compress(&original, CompressionAlgorithm::Zstd)
    .expect("Compression failed");
  assert!(
    compressed.len() < original.len(),
    "Compressed ({}) should be smaller than original ({})",
    compressed.len(),
    original.len()
  );
}

#[test]
fn test_decompress_invalid_zstd_data() {
  let garbage = b"this is not valid zstd data";
  let result = decompress(garbage, CompressionAlgorithm::Zstd);
  assert!(result.is_err(), "Decompressing garbage should fail");
}

#[test]
fn test_compress_large_data_roundtrip() {
  // 1 MB of pseudo-random-ish data (still compressible due to patterns)
  let original: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
  let compressed = compress(&original, CompressionAlgorithm::Zstd)
    .expect("Compression failed");
  let decompressed = decompress(&compressed, CompressionAlgorithm::Zstd)
    .expect("Decompression failed");
  assert_eq!(decompressed, original);
}

// --- should_compress tests ---

#[test]
fn test_should_compress_skips_small_data() {
  assert!(!should_compress(Some("application/json"), 100));
  assert!(!should_compress(Some("text/plain"), 499));
  assert!(!should_compress(None, 0));
}

#[test]
fn test_should_compress_allows_json() {
  assert!(should_compress(Some("application/json"), 1000));
  assert!(should_compress(Some("application/json; charset=utf-8"), 500));
}

#[test]
fn test_should_compress_allows_text() {
  assert!(should_compress(Some("text/plain"), 1000));
  assert!(should_compress(Some("text/html"), 5000));
  assert!(should_compress(Some("text/xml"), 800));
}

#[test]
fn test_should_compress_allows_no_content_type() {
  assert!(should_compress(None, 1000));
}

#[test]
fn test_should_compress_skips_jpeg() {
  assert!(!should_compress(Some("image/jpeg"), 100_000));
  assert!(!should_compress(Some("Image/JPEG"), 100_000));
}

#[test]
fn test_should_compress_skips_png() {
  assert!(!should_compress(Some("image/png"), 100_000));
}

#[test]
fn test_should_compress_skips_gif() {
  assert!(!should_compress(Some("image/gif"), 100_000));
}

#[test]
fn test_should_compress_skips_webp() {
  assert!(!should_compress(Some("image/webp"), 100_000));
}

#[test]
fn test_should_compress_skips_video() {
  assert!(!should_compress(Some("video/mp4"), 100_000));
  assert!(!should_compress(Some("video/webm"), 100_000));
}

#[test]
fn test_should_compress_skips_audio() {
  assert!(!should_compress(Some("audio/mpeg"), 100_000));
  assert!(!should_compress(Some("audio/ogg"), 100_000));
}

#[test]
fn test_should_compress_skips_zip() {
  assert!(!should_compress(Some("application/zip"), 100_000));
  assert!(!should_compress(Some("application/x-zip-compressed"), 100_000));
}

#[test]
fn test_should_compress_skips_gzip() {
  assert!(!should_compress(Some("application/gzip"), 100_000));
  assert!(!should_compress(Some("application/x-gzip"), 100_000));
}

#[test]
fn test_should_compress_skips_zstd() {
  assert!(!should_compress(Some("application/zstd"), 100_000));
}

#[test]
fn test_should_compress_skips_compressed() {
  assert!(!should_compress(Some("application/x-compressed"), 100_000));
}

#[test]
fn test_should_compress_boundary_500_bytes() {
  assert!(!should_compress(Some("text/plain"), 499));
  assert!(should_compress(Some("text/plain"), 500));
  assert!(should_compress(Some("text/plain"), 501));
}

// --- Store/read with compression through the engine ---

#[test]
fn test_store_file_with_compression() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let data = "Hello, this is test data for compression!".repeat(50);
  let data_bytes = data.as_bytes();

  ops.store_file_compressed(&ctx,
    "/compressed.txt",
    data_bytes,
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  let read_back = ops.read_file("/compressed.txt").unwrap();
  assert_eq!(read_back, data_bytes);
}

#[test]
fn test_hash_is_on_uncompressed_data() {
  let ctx = RequestContext::system();
  // Store the same data both compressed and uncompressed.
  // The chunk hashes should be identical (hash is on raw data).
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let data = "Dedup test data repeated many times. ".repeat(100);
  let data_bytes = data.as_bytes();

  // Store uncompressed first
  ops.store_file(&ctx, "/uncompressed.txt", data_bytes, Some("text/plain")).unwrap();
  let meta_uncompressed = ops.get_metadata("/uncompressed.txt").unwrap().unwrap();

  // Store compressed version at a different path
  ops.store_file_compressed(&ctx,
    "/compressed.txt",
    data_bytes,
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();
  let meta_compressed = ops.get_metadata("/compressed.txt").unwrap().unwrap();

  // Chunk hashes should match (hash is on uncompressed data)
  assert_eq!(meta_uncompressed.chunk_hashes, meta_compressed.chunk_hashes);
}

#[test]
fn test_read_compressed_file_streaming() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let data = "Streaming test data. ".repeat(200);
  let data_bytes = data.as_bytes();

  ops.store_file_compressed(&ctx,
    "/streamed.txt",
    data_bytes,
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  // Read via streaming iterator
  let stream = ops.read_file_streaming("/streamed.txt").unwrap();
  let read_back = stream.collect_to_vec().unwrap();
  assert_eq!(read_back, data_bytes);
}

#[test]
fn test_mixed_compressed_and_uncompressed() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store uncompressed
  let data_a = b"File A: uncompressed content that is reasonably sized.";
  ops.store_file(&ctx, "/file_a.txt", data_a, Some("text/plain")).unwrap();

  // Store compressed
  let data_b = "File B: compressed content. ".repeat(100);
  let data_b_bytes = data_b.as_bytes();
  ops.store_file_compressed(&ctx,
    "/file_b.txt",
    data_b_bytes,
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  // Store another uncompressed
  let data_c = b"File C: also uncompressed.";
  ops.store_file(&ctx, "/file_c.txt", data_c, None).unwrap();

  // Read all back
  assert_eq!(ops.read_file("/file_a.txt").unwrap(), data_a);
  assert_eq!(ops.read_file("/file_b.txt").unwrap(), data_b_bytes);
  assert_eq!(ops.read_file("/file_c.txt").unwrap(), data_c);
}

#[test]
fn test_store_empty_file_with_compression() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  ops.store_file_compressed(&ctx,
    "/empty.txt",
    &[],
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  let read_back = ops.read_file("/empty.txt").unwrap();
  assert!(read_back.is_empty());
}

#[test]
fn test_store_large_file_with_compression_multiple_chunks() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // 512 KB of data = 2 chunks at 256 KB each
  let data: Vec<u8> = (0..524_288).map(|i| (i % 256) as u8).collect();

  ops.store_file_compressed(&ctx,
    "/large.bin",
    &data,
    Some("application/octet-stream"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  let read_back = ops.read_file("/large.bin").unwrap();
  assert_eq!(read_back, data);

  let metadata = ops.get_metadata("/large.bin").unwrap().unwrap();
  assert!(metadata.chunk_hashes.len() >= 2);
}

#[test]
fn test_overwrite_compressed_file() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let data_v1 = "Version 1 content. ".repeat(50);
  ops.store_file_compressed(&ctx,
    "/versioned.txt",
    data_v1.as_bytes(),
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  let data_v2 = "Version 2 content that is different. ".repeat(50);
  ops.store_file_compressed(&ctx,
    "/versioned.txt",
    data_v2.as_bytes(),
    Some("text/plain"),
    CompressionAlgorithm::Zstd,
  ).unwrap();

  let read_back = ops.read_file("/versioned.txt").unwrap();
  assert_eq!(read_back, data_v2.as_bytes());
}

#[test]
fn test_compression_with_indexing_via_config() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store a config that enables zstd compression
  let config_json = r#"{"compression":"zstd","indexes":[{"name":"name","type":"string"}]}"#;
  ops.store_file(&ctx,
    "/data/.config/indexes.json",
    config_json.as_bytes(),
    Some("application/json"),
  ).unwrap();

  // Store a file that should be auto-compressed (> 500 bytes, text/json)
  let data = format!(r#"{{"name":"test","payload":"{}"}}"#, "x".repeat(1000));
  ops.store_file_with_indexing(&ctx,
    "/data/record.json",
    data.as_bytes(),
    Some("application/json"),
  ).unwrap();

  // Read back and verify
  let read_back = ops.read_file("/data/record.json").unwrap();
  assert_eq!(read_back, data.as_bytes());
}

#[test]
fn test_compression_config_skips_small_data() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config enables compression
  let config_json = r#"{"compression":"zstd","indexes":[]}"#;
  ops.store_file(&ctx,
    "/data/.config/indexes.json",
    config_json.as_bytes(),
    Some("application/json"),
  ).unwrap();

  // Store a small file (< 500 bytes) - should NOT be compressed per should_compress
  let small_data = r#"{"name":"tiny"}"#;
  ops.store_file_with_indexing(&ctx,
    "/data/small.json",
    small_data.as_bytes(),
    Some("application/json"),
  ).unwrap();

  let read_back = ops.read_file("/data/small.json").unwrap();
  assert_eq!(read_back, small_data.as_bytes());
}

#[test]
fn test_compression_config_skips_images() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config enables compression
  let config_json = r#"{"compression":"zstd","indexes":[]}"#;
  ops.store_file(&ctx,
    "/images/.config/indexes.json",
    config_json.as_bytes(),
    Some("application/json"),
  ).unwrap();

  // Store a "JPEG" (fake data, but content-type signals image/jpeg)
  let jpeg_data = vec![0xFF; 10_000];
  ops.store_file_with_indexing(&ctx,
    "/images/photo.jpg",
    &jpeg_data,
    Some("image/jpeg"),
  ).unwrap();

  let read_back = ops.read_file("/images/photo.jpg").unwrap();
  assert_eq!(read_back, jpeg_data);
}

#[test]
fn test_reopen_engine_reads_compressed_entries() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("test.aeor");
  let path_str = path.to_str().unwrap();

  // Create engine, store compressed file
  {
    let engine = StorageEngine::create(path_str).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();

    let data = "Persistent compressed data. ".repeat(100);
    ops.store_file_compressed(&ctx,
      "/persistent.txt",
      data.as_bytes(),
      Some("text/plain"),
      CompressionAlgorithm::Zstd,
    ).unwrap();
  }

  // Re-open and read back
  {
    let engine = StorageEngine::open(path_str).unwrap();
    let ops = DirectoryOps::new(&engine);

    let data = "Persistent compressed data. ".repeat(100);
    let read_back = ops.read_file("/persistent.txt").unwrap();
    assert_eq!(read_back, data.as_bytes());
  }
}

#[test]
fn test_no_compression_config_means_no_compression() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config WITHOUT compression field
  let config_json = r#"{"indexes":[{"name":"name","type":"string"}]}"#;
  ops.store_file(&ctx,
    "/data/.config/indexes.json",
    config_json.as_bytes(),
    Some("application/json"),
  ).unwrap();

  let data = format!(r#"{{"name":"test","payload":"{}"}}"#, "x".repeat(1000));
  ops.store_file_with_indexing(&ctx,
    "/data/record.json",
    data.as_bytes(),
    Some("application/json"),
  ).unwrap();

  // Should still read back fine (stored uncompressed)
  let read_back = ops.read_file("/data/record.json").unwrap();
  assert_eq!(read_back, data.as_bytes());
}
