use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::engine::content_type::detect_content_type;
use crate::engine::directory_ops::{
  chunk_content_hash, v0_system_entry_flags, validate_existing_chunk_locator, whole_file_content_hash, BatchFilePublicationInput,
  DirectoryOps, FileRecordPublishInput, DEFAULT_CHUNK_SIZE,
};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::namespace_mutation::NamespaceMutationKind;
use crate::engine::path_utils::normalize_path;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::GenericDataPathSelection;
use crate::engine::SystemFamilyPolicyResolver;

/// A file to commit as part of a batch, with pre-uploaded chunk hashes.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitFile {
  pub path: String,
  /// Hex-encoded chunk hashes (matching hashes already in the KV store).
  #[serde(alias = "chunk_hashes")]
  pub chunks: Vec<String>,
  #[serde(default)]
  pub content_type: Option<String>,
  /// Optional caller-asserted raw whole-file hash (`BLAKE3(file bytes)`).
  ///
  /// When present with `size` and all referenced chunks are stored raw, commit
  /// can avoid a full chunk body read pass. If it must read chunk bodies
  /// anyway, the supplied hash is verified against the computed value.
  #[serde(default)]
  pub content_hash: Option<String>,
  /// Optional caller-asserted total file size in bytes.
  #[serde(default)]
  pub size: Option<u64>,
}

/// A small, fully-buffered file to commit through the embedded SDK batch path.
///
/// This intentionally accepts raw bytes, not HTTP pre-uploaded chunk hashes.
/// It is meant for trusted in-process callers that already hold file contents
/// in memory, such as sync dirty-bucket flushes and small JSON/config writes.
#[derive(Debug, Clone)]
pub struct BufferedFile {
  pub path: String,
  pub data: Vec<u8>,
  pub content_type: Option<String>,
}

/// Result of a successful batch commit.
#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
  pub committed: usize,
  pub files: Vec<CommittedFile>,
}

/// Metadata for a single committed file.
#[derive(Debug, Clone, Serialize)]
pub struct CommittedFile {
  pub path: String,
  pub size: u64,
}

struct PreparedCommitFile {
  chunks: Vec<(Vec<u8>, u64)>,
  content_hash: Vec<u8>,
  fast_path_status: &'static str,
  chunk_metadata_lookup_us: u128,
  chunk_body_read_us: u128,
  chunk_body_read_bytes: u64,
}

/// Atomically commit multiple files from pre-uploaded chunks.
///
/// 1. Validates all chunk hashes exist in the KV store
/// 2. Creates FileRecords from chunk hash lists (preserving created_at on overwrite)
/// 3. Updates directories in a single pass (each directory updated once)
/// 4. Updates HEAD once
/// 5. Emits a single `entries_created` event
pub fn commit_files(engine: &StorageEngine, ctx: &RequestContext, files: Vec<CommitFile>) -> EngineResult<CommitResult> {
  let total_start = std::time::Instant::now();
  if files.is_empty() {
    return Err(EngineError::InvalidInput("No files provided for commit".to_string()));
  }

  let file_count = files.len();
  let total_logical_file_bytes: u64 = files.iter().filter_map(|file| file.size).sum();
  let supplied_content_hash_files = files.iter().filter(|file| file.content_hash.is_some()).count();
  let supplied_size_files = files.iter().filter(|file| file.size.is_some()).count();

  let algo = engine.hash_algo();
  let family_policy = SystemFamilyPolicyResolver::new(algo)?;

  // Generic blob commit may only write registry-selected ordinary data.
  // Dedicated owners remain the sole mutation path for concealed families.
  for file in &files {
    let normalized = normalize_path(&file.path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }
    match family_policy.generic_data_path_selection(&normalized)? {
      GenericDataPathSelection::Include => {}
      GenericDataPathSelection::Conceal | GenericDataPathSelection::StructuralContainer => {
        return Err(EngineError::InvalidInput(format!(
          "Path '{}' is reserved for owner-specific data and cannot be written through this endpoint",
          file.path
        )));
      }
    }
  }

  // --- Phase 1: Validate all chunk hashes exist ---
  let validation_start = std::time::Instant::now();
  let mut missing_chunks: Vec<String> = Vec::new();
  // Decode all hex chunk hashes upfront and validate existence.
  // file_chunks[i] = Vec of (raw_hash_bytes, chunk_byte_size) for files[i].
  let mut file_chunks: Vec<Vec<(Vec<u8>, u64)>> = Vec::with_capacity(files.len());
  let mut file_content_hashes: Vec<Vec<u8>> = Vec::with_capacity(files.len());
  let mut asserted_hash_fast_path_files = 0usize;
  let mut fast_path_missing_content_hash_files = 0usize;
  let mut fast_path_missing_size_files = 0usize;
  let mut fast_path_metadata_incomplete_files = 0usize;
  let mut chunk_metadata_lookup_us = 0u128;
  let mut chunk_body_read_us = 0u128;
  let mut chunk_body_read_bytes = 0u64;
  let mut total_chunk_refs = 0usize;

  for file in &files {
    total_chunk_refs += file.chunks.len();
    match prepare_commit_file(engine, file, algo.hash_length())? {
      Ok(prepared) => {
        match prepared.fast_path_status {
          "used" => asserted_hash_fast_path_files += 1,
          "missing_content_hash" => fast_path_missing_content_hash_files += 1,
          "missing_size" => fast_path_missing_size_files += 1,
          "chunk_metadata_incomplete" => fast_path_metadata_incomplete_files += 1,
          _ => {}
        }
        chunk_metadata_lookup_us += prepared.chunk_metadata_lookup_us;
        chunk_body_read_us += prepared.chunk_body_read_us;
        chunk_body_read_bytes = chunk_body_read_bytes.saturating_add(prepared.chunk_body_read_bytes);
        file_content_hashes.push(prepared.content_hash);
        file_chunks.push(prepared.chunks);
      }
      Err(missing) => {
        missing_chunks.extend(missing);
      }
    }
  }

  if !missing_chunks.is_empty() {
    return Err(EngineError::InvalidInput(format!("Missing {} chunk(s): {}", missing_chunks.len(), missing_chunks.join(", "))));
  }

  let validation_elapsed = validation_start.elapsed();

  // --- Phase 2: Prepare one coordinator-owned namespace publication ---
  let prepare_publication_start = std::time::Instant::now();
  let mut publications = Vec::with_capacity(files.len());
  let mut first_chunk_sniff_reads = 0usize;
  let mut first_chunk_sniff_bytes = 0u64;
  let mut first_chunk_sniff_us = 0u128;

  for (i, file) in files.iter().enumerate() {
    let normalized = normalize_path(&file.path);
    let chunk_hashes: Vec<Vec<u8>> = file_chunks[i].iter().map(|(h, _)| h.clone()).collect();

    // Compute total size from chunk data sizes
    let total_size: u64 = file_chunks[i].iter().map(|(_, sz)| *sz).sum();

    // Match DirectoryOps' MIME contract: trust specific caller-provided
    // types, but treat empty/octet-stream as unknown and sniff bytes.
    let first_chunk_bytes = if content_type_needs_sniffing(file.content_type.as_deref()) {
      if let Some(first_hash) = chunk_hashes.first() {
        let sniff_start = std::time::Instant::now();
        let bytes = read_chunk_data(engine, first_hash)?.ok_or_else(|| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("blob commit first chunk disappeared after validation: {}", hex::encode(first_hash)),
        })?;
        first_chunk_sniff_us += sniff_start.elapsed().as_micros();
        first_chunk_sniff_reads += 1;
        first_chunk_sniff_bytes = first_chunk_sniff_bytes.saturating_add(bytes.len() as u64);
        bytes
      } else {
        Vec::new()
      }
    } else {
      Vec::new()
    };
    let detected_content_type = detect_content_type(&first_chunk_bytes, file.content_type.as_deref());

    publications.push(BatchFilePublicationInput {
      publication: FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        chunk_hashes,
        content_hash: file_content_hashes[i].clone(),
        flags: 0,
        created_at_override: None,
        updated_at_override: None,
        prefer_existing_created_at: true,
      },
      throughput_bytes: 0,
    });
  }
  let prepare_publication_ms = prepare_publication_start.elapsed().as_millis();

  let namespace_publication_start = std::time::Instant::now();
  let published = DirectoryOps::new(engine).execute_file_publications(ctx, publications, NamespaceMutationKind::BatchWrite)?;
  let namespace_publication_ms = namespace_publication_start.elapsed().as_millis();
  let result = CommitResult {
    committed: published.len(),
    files: published.into_iter().map(|file| CommittedFile { path: file.normalized_path, size: file.file_record.total_size }).collect(),
  };

  tracing::info!(
    files = file_count,
    total_chunk_refs,
    total_logical_file_bytes,
    supplied_content_hash_files,
    supplied_size_files,
    asserted_hash_fast_path_files,
    fast_path_missing_content_hash_files,
    fast_path_missing_size_files,
    fast_path_metadata_incomplete_files,
    chunk_metadata_lookup_us,
    chunk_body_read_us,
    chunk_body_read_bytes,
    chunk_validation_ms = validation_elapsed.as_millis(),
    prepare_publication_ms,
    first_chunk_sniff_reads,
    first_chunk_sniff_bytes,
    first_chunk_sniff_us,
    namespace_publication_ms,
    total_ms = total_start.elapsed().as_millis(),
    "blob commit completed"
  );

  Ok(result)
}

/// Atomically commit multiple small files from raw in-memory buffers.
///
/// This is the embedded-library companion to [`commit_files`]. It avoids the
/// HTTP chunk pre-upload contract, validates all paths before writing any
/// entries, supports trusted/system paths the same way `DirectoryOps` does,
/// and performs directory propagation in one batch.
pub fn commit_buffered_files(engine: &StorageEngine, ctx: &RequestContext, files: Vec<BufferedFile>) -> EngineResult<CommitResult> {
  commit_buffered_files_with_kind(engine, ctx, files, NamespaceMutationKind::BatchWrite)
}

pub(crate) fn commit_buffered_files_with_kind(
  engine: &StorageEngine,
  ctx: &RequestContext,
  files: Vec<BufferedFile>,
  kind: NamespaceMutationKind,
) -> EngineResult<CommitResult> {
  if files.is_empty() {
    return Err(EngineError::InvalidInput("No files provided for buffered batch commit".to_string()));
  }

  let mut seen_paths = HashSet::with_capacity(files.len());
  let mut normalized_paths = Vec::with_capacity(files.len());
  for file in &files {
    let normalized = normalize_path(&file.path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }
    if !seen_paths.insert(normalized.clone()) {
      return Err(EngineError::InvalidInput(format!("Duplicate batch path: {}", normalized)));
    }
    normalized_paths.push(normalized);
  }

  let algo = engine.hash_algo();
  let mut publications = Vec::with_capacity(files.len());

  for (file, normalized) in files.iter().zip(normalized_paths) {
    let sys_flags = v0_system_entry_flags(&normalized);
    let detected_content_type = detect_content_type(&file.data, file.content_type.as_deref());
    let total_size = file.data.len() as u64;
    let mut chunk_hashes = Vec::new();

    let mut offset = 0usize;
    while offset < file.data.len() {
      let end = (offset + DEFAULT_CHUNK_SIZE).min(file.data.len());
      let chunk_data = &file.data[offset..end];
      let chunk_key = store_buffered_chunk(engine, chunk_data, sys_flags)?;
      chunk_hashes.push(chunk_key);
      offset = end;
    }

    publications.push(BatchFilePublicationInput {
      publication: FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        chunk_hashes,
        content_hash: whole_file_content_hash(&file.data, &algo)?,
        flags: sys_flags,
        created_at_override: None,
        updated_at_override: None,
        prefer_existing_created_at: true,
      },
      throughput_bytes: total_size,
    });
  }

  let published = DirectoryOps::new(engine).execute_file_publications(ctx, publications, kind)?;
  Ok(CommitResult {
    committed: published.len(),
    files: published.into_iter().map(|file| CommittedFile { path: file.normalized_path, size: file.file_record.total_size }).collect(),
  })
}

fn content_type_needs_sniffing(content_type: Option<&str>) -> bool {
  match content_type {
    Some(content_type) => content_type.is_empty() || content_type == "application/octet-stream",
    None => true,
  }
}

fn prepare_commit_file(
  engine: &StorageEngine,
  file: &CommitFile,
  hash_length: usize,
) -> EngineResult<Result<PreparedCommitFile, Vec<String>>> {
  let decoded_hashes = decode_commit_chunk_hashes(file)?;
  let supplied_content_hash = decode_commit_content_hash(file.content_hash.as_deref(), hash_length)?;
  let mut chunk_metadata_lookup_us = 0u128;
  let mut chunk_body_read_us = 0u128;
  let mut chunk_body_read_bytes = 0u64;
  let mut metadata_fallback_status = "chunk_metadata_incomplete";

  if let Some(asserted_hash) = supplied_content_hash.as_ref().filter(|_| file.size.is_some()) {
    let mut chunks_for_file: Vec<(Vec<u8>, u64)> = Vec::with_capacity(decoded_hashes.len());
    let mut missing_chunks = Vec::new();
    let mut total_size = 0u64;
    let mut metadata_complete = true;

    for (hex_hash, raw_hash) in &decoded_hashes {
      let metadata_start = std::time::Instant::now();
      match engine.get_chunk_metadata(raw_hash)? {
        Some(metadata) => match metadata.raw_value_length {
          Some(raw_len) => {
            chunk_metadata_lookup_us += metadata_start.elapsed().as_micros();
            total_size = total_size
              .checked_add(raw_len)
              .ok_or_else(|| EngineError::InvalidInput(format!("Commit size overflow while preparing '{}'", file.path)))?;
            chunks_for_file.push((raw_hash.clone(), raw_len));
          }
          None => {
            chunk_metadata_lookup_us += metadata_start.elapsed().as_micros();
            metadata_complete = false;
            break;
          }
        },
        None => {
          chunk_metadata_lookup_us += metadata_start.elapsed().as_micros();
          missing_chunks.push(hex_hash.clone());
        }
      }
    }

    if !missing_chunks.is_empty() {
      return Ok(Err(missing_chunks));
    }

    if metadata_complete {
      match validate_commit_size(file, total_size) {
        Ok(()) => {
          return Ok(Ok(PreparedCommitFile {
            chunks: chunks_for_file,
            content_hash: asserted_hash.clone(),
            fast_path_status: "used",
            chunk_metadata_lookup_us,
            chunk_body_read_us,
            chunk_body_read_bytes,
          }));
        }
        Err(error) => {
          metadata_fallback_status = "chunk_metadata_size_mismatch";
          tracing::debug!(
            path = %file.path,
            metadata_size = total_size,
            asserted_size = file.size,
            %error,
            "blob commit chunk metadata did not match asserted size; falling back to chunk body verification"
          );
        }
      }
    }
  }

  let fast_path_status = if supplied_content_hash.is_none() {
    "missing_content_hash"
  } else if file.size.is_none() {
    "missing_size"
  } else {
    metadata_fallback_status
  };

  let algo = engine.hash_algo();
  let mut chunks_for_file: Vec<(Vec<u8>, u64)> = Vec::with_capacity(decoded_hashes.len());
  let mut missing_chunks = Vec::new();
  let mut content_hasher = algo.incremental_hasher()?;
  let mut total_size = 0u64;

  for (hex_hash, raw_hash) in decoded_hashes {
    let read_start = std::time::Instant::now();
    match read_chunk_data(engine, &raw_hash)? {
      Some(value) => {
        chunk_body_read_us += read_start.elapsed().as_micros();
        chunk_body_read_bytes = chunk_body_read_bytes.saturating_add(value.len() as u64);
        content_hasher.update(&value);
        let chunk_len = value.len() as u64;
        total_size = total_size
          .checked_add(chunk_len)
          .ok_or_else(|| EngineError::InvalidInput(format!("Commit size overflow while preparing '{}'", file.path)))?;
        chunks_for_file.push((raw_hash, chunk_len));
      }
      None => {
        chunk_body_read_us += read_start.elapsed().as_micros();
        missing_chunks.push(hex_hash);
      }
    }
  }

  if !missing_chunks.is_empty() {
    return Ok(Err(missing_chunks));
  }

  validate_commit_size(file, total_size)?;
  let computed_hash = content_hasher.finalize();
  if let Some(asserted_hash) = supplied_content_hash {
    if asserted_hash != computed_hash {
      return Err(EngineError::InvalidInput(format!(
        "Content hash mismatch for '{}': expected {}, computed {}",
        file.path,
        hex::encode(asserted_hash),
        hex::encode(&computed_hash),
      )));
    }
  }

  Ok(Ok(PreparedCommitFile {
    chunks: chunks_for_file,
    content_hash: computed_hash,
    fast_path_status,
    chunk_metadata_lookup_us,
    chunk_body_read_us,
    chunk_body_read_bytes,
  }))
}

fn decode_commit_chunk_hashes(file: &CommitFile) -> EngineResult<Vec<(String, Vec<u8>)>> {
  let mut decoded = Vec::with_capacity(file.chunks.len());
  for hex_hash in &file.chunks {
    let raw_hash = hex::decode(hex_hash)
      .map_err(|error| EngineError::InvalidInput(format!("Invalid hex chunk hash '{}' for '{}': {}", hex_hash, file.path, error)))?;
    decoded.push((hex_hash.clone(), raw_hash));
  }
  Ok(decoded)
}

fn decode_commit_content_hash(content_hash: Option<&str>, hash_length: usize) -> EngineResult<Option<Vec<u8>>> {
  let Some(content_hash) = content_hash else {
    return Ok(None);
  };

  let decoded =
    hex::decode(content_hash).map_err(|error| EngineError::InvalidInput(format!("Invalid content_hash '{}': {}", content_hash, error)))?;
  if decoded.len() != hash_length {
    return Err(EngineError::InvalidInput(format!("Invalid content_hash length {} bytes; expected {} bytes", decoded.len(), hash_length,)));
  }
  Ok(Some(decoded))
}

fn validate_commit_size(file: &CommitFile, actual_size: u64) -> EngineResult<()> {
  if let Some(expected_size) = file.size {
    if expected_size != actual_size {
      return Err(EngineError::InvalidInput(format!(
        "Size mismatch for '{}': expected {}, computed {}",
        file.path, expected_size, actual_size,
      )));
    }
  }
  Ok(())
}

fn read_chunk_data(engine: &StorageEngine, hash: &[u8]) -> EngineResult<Option<Vec<u8>>> {
  engine.read_chunk(hash)
}

fn store_buffered_chunk(engine: &StorageEngine, data: &[u8], flags: u8) -> EngineResult<Vec<u8>> {
  let algo = engine.hash_algo();
  let chunk_key = chunk_content_hash(data, &algo)?;

  if validate_existing_chunk_locator(engine, "buffered chunk staging", &chunk_key)? {
    engine.counters().record_chunk_deduped();
    return Ok(chunk_key);
  }

  if flags != 0 {
    engine.store_entry_with_flags(EntryType::Chunk, &chunk_key, data, flags)?;
  } else {
    engine.store_entry(EntryType::Chunk, &chunk_key, data)?;
  }
  engine.counters().record_chunk_stored(data.len() as u64);

  Ok(chunk_key)
}
