use std::sync::Arc;

use serde::Serialize;

use super::{engine_file_response_with_hash, required_dispatch_value, serialize_response_value, EngineByteRangeStream, HttpByteRange};
use crate::engine::compression::compress;
use crate::engine::memory_coordinator::MemoryOwner;
use crate::engine::{
  CompressionAlgorithm, DirectoryOps, EntryType, FileRecord, HashAlgorithm, RequestContext, StorageEngine, DEFAULT_CHUNK_SIZE,
};

struct SerializationFailure;

impl Serialize for SerializationFailure {
  fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    Err(serde::ser::Error::custom("injected serialization failure"))
  }
}

#[test]
fn internal_dispatch_state_failure_returns_an_http_error() {
  let response = required_dispatch_value::<Vec<u8>>(None, "DirectoryIndex", "deadbeef").unwrap_err();

  assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn response_serialization_failure_returns_an_http_error() {
  let response = serialize_response_value(&SerializationFailure, "query result").unwrap_err();

  assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn file_response_hash_construction_propagates_serialization_and_hash_failures() {
  let malformed = FileRecord::new("/malformed.txt".to_string(), Some("text/plain".to_string()), 0, Vec::new());
  let serialization_error = engine_file_response_with_hash(&malformed, HashAlgorithm::Blake3_256).unwrap_err();
  assert!(serialization_error.to_string().contains("Content hash length"));

  let mut valid = malformed;
  valid.content_hash = vec![0; HashAlgorithm::Blake3_256.hash_length()];
  let hash_error = engine_file_response_with_hash(&valid, HashAlgorithm::Sha256).unwrap_err();
  assert!(hash_error.to_string().contains("Invalid hash algorithm"));
}

#[test]
fn production_hash_and_query_routes_contain_no_dispatch_or_serialization_panics() {
  let source = include_str!("../../src/server/engine_routes.rs");
  let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);

  assert!(!production.contains(".expect(\"FileRecord dispatch"));
  assert!(!production.contains(".expect(\"raw entry dispatch"));
  assert!(!production.contains("serde_json::to_value(&result).unwrap()"));
  assert!(!production.contains("serde_json::to_value(meta).unwrap()"));
  assert!(!production.contains("serde_json::to_value(generation.matches).unwrap_or_else"));
}

#[test]
fn coalesced_file_stream_survives_kv_expansion_after_layout_planning() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("stream-layout-expansion.aeordb");
  let engine = Arc::new(StorageEngine::create(database.to_str().unwrap()).unwrap());
  let data: Vec<u8> = (0..DEFAULT_CHUNK_SIZE * 3 + 17).map(|index| (index % 251) as u8).collect();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&RequestContext::system(), "/stream.bin", &data, Some("application/octet-stream")).unwrap();
  let baseline = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();

  let record = operations.get_metadata("/stream.bin").unwrap().unwrap();
  let mut stream = EngineByteRangeStream::new(
    record.chunk_hashes,
    Arc::clone(&engine),
    false,
    HttpByteRange { start: 0, end: record.total_size - 1 },
    record.total_size,
  )
  .unwrap();

  let current_stage = engine.writer_read_lock().unwrap().file_header().kv_block_stage as usize;
  engine.expand_kv_block_online(current_stage + 1).unwrap();

  let mut actual = Vec::new();
  for chunk in &mut stream {
    let chunk = chunk.unwrap();
    actual.extend_from_slice(chunk.as_ref());
  }
  assert_eq!(actual, data, "a planned streaming response must follow live chunk rows across KV relocation");
  drop(stream);
  let released = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
  assert_eq!(released.active_reservations, baseline.active_reservations, "layout replan leaked a streaming reservation");
  assert_eq!(released.reserved_bytes, baseline.reserved_bytes, "layout replan leaked reserved streaming bytes");
}

#[test]
fn coalesced_file_stream_survives_kv_expansion_between_response_frames() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("stream-layout-expansion-between-frames.aeordb");
  let engine = Arc::new(StorageEngine::create(database.to_str().unwrap()).unwrap());
  let data: Vec<u8> = (0..DEFAULT_CHUNK_SIZE * 12 + 17).map(|index| (index % 251) as u8).collect();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&RequestContext::system(), "/stream.bin", &data, Some("application/octet-stream")).unwrap();

  let record = operations.get_metadata("/stream.bin").unwrap().unwrap();
  let mut stream = EngineByteRangeStream::new(
    record.chunk_hashes,
    Arc::clone(&engine),
    false,
    HttpByteRange { start: 0, end: record.total_size - 1 },
    record.total_size,
  )
  .unwrap();
  let first = stream.next().unwrap().unwrap();
  assert!(first.len() < data.len(), "test data must require more than one response frame");
  let mut actual = first.as_ref().to_vec();
  drop(first);

  let current_stage = engine.writer_read_lock().unwrap().file_header().kv_block_stage as usize;
  engine.expand_kv_block_online(current_stage + 1).unwrap();

  for chunk in &mut stream {
    let chunk = chunk.unwrap();
    actual.extend_from_slice(chunk.as_ref());
  }
  assert_eq!(actual, data, "every later response frame must refresh a relocated KV layout");
}

#[test]
fn coalesced_layout_refresh_rejects_missing_compressed_and_length_changed_chunks() {
  let directory = tempfile::tempdir().unwrap();
  let database = directory.path().join("invalid-stream-layout-refresh.aeordb");
  let engine = Arc::new(StorageEngine::create(database.to_str().unwrap()).unwrap());
  let data: Vec<u8> = (0..DEFAULT_CHUNK_SIZE).map(|index| (index % 251) as u8).collect();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&RequestContext::system(), "/stream.bin", &data, Some("application/octet-stream")).unwrap();
  let record = operations.get_metadata("/stream.bin").unwrap().unwrap();
  let build_stream = || {
    EngineByteRangeStream::new(
      record.chunk_hashes.clone(),
      Arc::clone(&engine),
      false,
      HttpByteRange { start: 0, end: record.total_size - 1 },
      record.total_size,
    )
    .unwrap()
  };
  let mut length_changed = build_stream();
  let mut compressed = build_stream();
  let mut missing = build_stream();

  let EngineByteRangeStream::Coalesced(length_changed) = &mut length_changed else {
    panic!("uncompressed test chunk must use the coalesced stream");
  };
  length_changed.chunks[0].file_end += 1;
  let error = length_changed.refresh_remaining_layout().expect_err("a changed decoded length must fail closed");
  assert!(matches!(error, crate::engine::EngineError::CorruptEntry { reason, .. } if reason.contains("Chunk length changed")));

  let chunk_hash = record.chunk_hashes[0].clone();
  let encoded = compress(&data, CompressionAlgorithm::Zstd).unwrap();
  engine.store_entry_compressed(EntryType::Chunk, &chunk_hash, &encoded, CompressionAlgorithm::Zstd).unwrap();
  let EngineByteRangeStream::Coalesced(compressed) = &mut compressed else {
    panic!("stream was planned before the chunk representation changed");
  };
  let error = compressed.refresh_remaining_layout().expect_err("a newly compressed chunk must fail closed");
  assert!(matches!(error, crate::engine::EngineError::CorruptEntry { reason, .. } if reason.contains("became compressed")));

  engine.remove_kv_entry(&chunk_hash).unwrap();
  let EngineByteRangeStream::Coalesced(missing) = &mut missing else {
    panic!("stream was planned before the chunk disappeared");
  };
  let error = missing.refresh_remaining_layout().expect_err("a missing chunk must fail closed");
  assert!(matches!(error, crate::engine::EngineError::NotFound(_)));
}
