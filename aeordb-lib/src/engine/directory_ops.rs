use crate::engine::compression::{CompressionAlgorithm, compress, decompress, should_compress};
use crate::engine::deletion_record::DeletionRecord;
use crate::engine::directory_entry::{
  ChildEntry, deserialize_child_entries, serialize_child_entries,
};
use crate::engine::entry_header::FLAG_SYSTEM;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::symlink_record::{SymlinkRecord, symlink_path_hash, symlink_content_hash};
use crate::engine::index_config::PathIndexConfig;
use crate::engine::index_store::IndexManager;
use crate::engine::engine_event::{EntryEventData, EVENT_ENTRIES_CREATED, EVENT_ENTRIES_DELETED};
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::{StorageEngine, WriteBatch};

/// Default chunk size for splitting file data (256 KB).
pub const DEFAULT_CHUNK_SIZE: usize = 262_144;

/// Compute the domain-prefixed hash for a file path.
pub fn file_path_hash(path: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("file:{}", path).as_bytes())
}

/// Check if a path targets an internal directory that should not trigger indexing.
/// Returns true for paths containing .logs/, .indexes/, or .config/ segments.
pub fn is_internal_path(path: &str) -> bool {
  let normalized = normalize_path(path);
  let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
  segments.iter().any(|s| *s == ".logs" || *s == ".aeordb-indexes" || *s == ".aeordb-config")
}

/// Compute the domain-prefixed hash for a directory path.
pub fn directory_path_hash(path: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("dir:{}", path).as_bytes())
}

/// Compute a content-addressed hash for directory data.
/// Uses the "dirc:" domain prefix + the actual serialized content,
/// distinct from the path-based "dir:" prefix to avoid collisions.
pub fn directory_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(5 + data.len());
  input.extend_from_slice(b"dirc:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Compute a content-addressed hash for a serialized FileRecord.
/// Uses the "filec:" domain prefix, distinct from the path-based "file:" prefix.
pub fn file_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"filec:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Identity hash for a file — based on content-defining fields only.
/// Excludes timestamps, metadata, and total_size.
/// Two identical files stored at different times produce the SAME identity hash.
pub fn file_identity_hash(
    path: &str,
    content_type: Option<&str>,
    chunk_hashes: &[Vec<u8>],
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    let mut input = Vec::new();
    input.extend_from_slice(b"fileid:");
    input.extend_from_slice(path.as_bytes());
    input.push(0); // separator
    input.extend_from_slice(content_type.unwrap_or("").as_bytes());
    input.push(0); // separator
    for hash in chunk_hashes {
        input.extend_from_slice(hash);
    }
    algo.compute_hash(&input)
}

/// Identity hash for a symlink — based on path and target only.
/// Excludes timestamps.
pub fn symlink_identity_hash(
    path: &str,
    target: &str,
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    let mut input = Vec::new();
    input.extend_from_slice(b"symlinkid:");
    input.extend_from_slice(path.as_bytes());
    input.push(0); // separator
    input.extend_from_slice(target.as_bytes());
    algo.compute_hash(&input)
}

/// Compute the domain-prefixed hash for a chunk.
pub fn chunk_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"chunk:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}

/// Compute the hash for a system chunk (/.aeordb-system/ data).
/// Uses "system::" domain prefix — cryptographically separated from user "chunk:" domain.
pub fn system_chunk_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    let mut input = Vec::with_capacity(8 + data.len());
    input.extend_from_slice(b"system::");
    input.extend_from_slice(data);
    algo.compute_hash(&input)
}

/// Compute the identity hash for a system file.
/// Uses "sysfileid:" domain prefix.
pub fn system_file_identity_hash(
    path: &str,
    content_type: Option<&str>,
    chunk_hashes: &[Vec<u8>],
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    let mut input = Vec::new();
    input.extend_from_slice(b"sysfileid:");
    input.extend_from_slice(path.as_bytes());
    input.push(0);
    input.extend_from_slice(content_type.unwrap_or("").as_bytes());
    input.push(0);
    for hash in chunk_hashes {
        input.extend_from_slice(hash);
    }
    algo.compute_hash(&input)
}

/// Check if a path is under the /.aeordb-system/ directory.
pub fn is_system_path(path: &str) -> bool {
    let normalized = crate::engine::path_utils::normalize_path(path);
    normalized.starts_with("/.aeordb-") || normalized == "/.aeordb-system"
}

/// Compute the domain-prefixed hash for a deletion record.
fn deletion_record_hash(
  path: &str,
  timestamp: i64,
  algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("del:{}:{}", path, timestamp).as_bytes())
}

/// An iterator that yields chunk data pre-read from the engine.
///
/// All chunks are eagerly loaded upfront to avoid storing a raw pointer
/// or reference to the StorageEngine. Each chunk is yielded one at a time,
/// which still allows the HTTP layer to stream chunk-by-chunk from the Vec.
pub struct EngineFileStream {
  chunks: Vec<Result<Vec<u8>, EngineError>>,
  current_index: usize,
}

impl EngineFileStream {
  /// Build a stream from an explicit list of chunk hashes (public entry point
  /// for hash-based retrieval where we already have the FileRecord).
  pub fn from_chunk_hashes(chunk_hashes: Vec<Vec<u8>>, engine: &StorageEngine) -> EngineResult<Self> {
    Self::new(chunk_hashes, engine, false)
  }

  /// Like `from_chunk_hashes` but reads chunks even if they are marked deleted.
  /// Used for streaming files from historical snapshots.
  pub fn from_chunk_hashes_including_deleted(chunk_hashes: Vec<Vec<u8>>, engine: &StorageEngine) -> EngineResult<Self> {
    Self::new(chunk_hashes, engine, true)
  }

  fn new(chunk_hashes: Vec<Vec<u8>>, engine: &StorageEngine, include_deleted: bool) -> EngineResult<Self> {
    let mut chunks = Vec::with_capacity(chunk_hashes.len());

    for hash in &chunk_hashes {
      // Chunks are user-facing data — verify integrity on read
      let result = if include_deleted {
        engine.get_entry_verified_including_deleted(hash)
      } else {
        engine.get_entry_verified(hash)
      };
      match result {
        Ok(Some((header, _key, value))) => {
          // Decompress if the chunk was stored compressed
          if header.compression_algo != CompressionAlgorithm::None {
            match decompress(&value, header.compression_algo) {
              Ok(decompressed) => chunks.push(Ok(decompressed)),
              Err(error) => chunks.push(Err(error)),
            }
          } else {
            chunks.push(Ok(value));
          }
        }
        Ok(None) => {
          chunks.push(Err(EngineError::NotFound(
            format!("Chunk not found: {}", hex::encode(hash)),
          )));
        }
        Err(error) => {
          chunks.push(Err(error));
        }
      }
    }

    Ok(EngineFileStream {
      chunks,
      current_index: 0,
    })
  }

  /// Collect all chunks into a single Vec<u8>.
  pub fn collect_to_vec(self) -> EngineResult<Vec<u8>> {
    let mut result = Vec::new();
    for item in self {
      result.extend_from_slice(&item?);
    }
    Ok(result)
  }
}

impl Iterator for EngineFileStream {
  type Item = EngineResult<Vec<u8>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.current_index >= self.chunks.len() {
      return None;
    }

    let index = self.current_index;
    self.current_index += 1;

    // Take the pre-read result, replacing with a placeholder error
    // (the index will never be visited again since current_index only moves forward)
    let chunk = std::mem::replace(
      &mut self.chunks[index],
      Err(EngineError::NotFound("already consumed".to_string())),
    );

    Some(chunk)
  }
}

/// Directory operations built on top of the StorageEngine.
///
/// Provides file storage, retrieval, deletion, directory listing,
/// and path-based navigation with automatic parent directory management.
pub struct DirectoryOps<'a> {
  engine: &'a StorageEngine,
}

impl<'a> DirectoryOps<'a> {
  /// Create a new `DirectoryOps` handle wrapping the given storage engine.
  pub fn new(engine: &'a StorageEngine) -> Self {
    DirectoryOps { engine }
  }

  /// Store a file at the given path, splitting data into chunks.
  /// Creates intermediate directories as needed and updates HEAD.
  pub fn store_file(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> EngineResult<FileRecord> {
    self.store_file_internal(ctx, path, data, content_type, CompressionAlgorithm::None)
  }

  /// Store a single data chunk and return its hash. Deduplicates automatically.
  /// Used by streaming upload to store chunks as they arrive without buffering.
  pub fn store_chunk(&self, data: &[u8]) -> EngineResult<Vec<u8>> {
    let algo = self.engine.hash_algo();
    let chunk_key = chunk_content_hash(data, &algo)?;
    if !self.engine.has_entry(&chunk_key)? {
      self.engine.store_entry(EntryType::Chunk, &chunk_key, data)?;
      self.engine.counters().increment_chunks();
      self.engine.counters().add_chunk_data_size(data.len() as u64);
    } else {
      self.engine.counters().increment_chunks_deduped();
    }
    Ok(chunk_key)
  }

  /// Finalize a file from pre-stored chunk hashes.
  /// Chunks must already be stored via `store_chunk()`. This method creates
  /// the FileRecord, updates directory indexes, and emits events.
  /// `first_bytes` is the first ≤8KB for content-type detection.
  pub fn finalize_file(
    &self,
    ctx: &RequestContext,
    path: &str,
    chunk_hashes: Vec<Vec<u8>>,
    total_size: u64,
    content_type: Option<&str>,
    first_bytes: &[u8],
  ) -> EngineResult<FileRecord> {
    let timer_start = std::time::Instant::now();
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);

    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = if is_system_path(&normalized) { FLAG_SYSTEM } else { 0 };
    let detected_content_type = crate::engine::content_type::detect_content_type(first_bytes, content_type);
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    let (existing_created_at, existing_total_size) = match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        let existing = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        (Some(existing.created_at), Some(existing.total_size))
      }
      None => (None, None),
    };

    let mut file_record = FileRecord::new(
      normalized.clone(),
      Some(detected_content_type.clone()),
      total_size,
      chunk_hashes,
    );

    if let Some(original_created_at) = existing_created_at {
      file_record.created_at = original_created_at;
    }

    let file_value = file_record.serialize(hash_length)?;
    let file_content_key = file_content_hash(&file_value, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &file_content_key, &file_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;
    }

    let identity_key = file_identity_hash(&normalized, Some(detected_content_type.as_str()), &file_record.chunk_hashes, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &identity_key, &file_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &identity_key, &file_value)?;
    }

    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &file_key, &file_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;
    }

    let now_vt = chrono::Utc::now().timestamp_millis() as u64;
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: identity_key,
      total_size,
      created_at: file_record.created_at,
      updated_at: file_record.updated_at,
      name: file_name(&normalized).unwrap_or("").to_string(),
      content_type: Some(detected_content_type.clone()),
      virtual_time: now_vt,
      node_id: 0,
    };

    self.update_parent_directories(&normalized, child)?;

    let counters = self.engine.counters();
    counters.increment_writes();
    counters.add_bytes_written(total_size);
    if existing_created_at.is_none() {
      counters.increment_files();
      counters.add_logical_data_size(total_size);
    } else if let Some(old_size) = existing_total_size {
      if total_size > old_size {
        counters.add_logical_data_size(total_size - old_size);
      }
    }

    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "file".to_string(),
      content_type: file_record.content_type.clone(),
      size: file_record.total_size,
      hash: hex::encode(file_record.chunk_hashes.first().unwrap_or(&vec![])),
      created_at: file_record.created_at,
      updated_at: file_record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [entry_data]}));

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_STORE_DURATION).record(elapsed);

    Ok(file_record)
  }

  /// Store a file with compression at the given path, splitting data into chunks.
  /// Creates intermediate directories as needed and updates HEAD.
  /// Chunks are compressed individually using the specified algorithm.
  pub fn store_file_compressed(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    self.store_file_internal(ctx, path, data, content_type, compression_algo)
  }

  /// Internal file storage with optional compression.
  ///
  /// **Atomicity (M15)**: This method stores chunks, a FileRecord, and
  /// updated directory entries as separate append-writer operations. If the
  /// process crashes mid-way, some chunks or the FileRecord may be written
  /// to disk without the directory tree pointing to them. These orphaned
  /// entries are harmless — they consume space but are unreachable — and
  /// will be reclaimed by the next GC sweep. The hot-file mechanism
  /// ensures the KV index is recovered on restart, and since the directory
  /// tree is only updated atomically at the end (single entry write),
  /// readers will never see a partially-stored file.
  fn store_file_internal(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    let timer_start = std::time::Instant::now();
    let result = self.store_file_internal_inner(ctx, path, data, content_type, compression_algo);
    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_STORE_DURATION).record(elapsed);
    result
  }

  /// Inner implementation of store_file_internal, separated for timing.
  fn store_file_internal_inner(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);

    // M15: Reject storing at root path — it would create a ghost entry.
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = if is_system_path(&normalized) { FLAG_SYSTEM } else { 0 };

    // Detect content type from magic bytes when not explicitly provided
    let detected_content_type = crate::engine::content_type::detect_content_type(data, content_type);
    let hash_length = algo.hash_length();

    // Split data into chunks and store each one
    let mut chunk_hashes = Vec::new();
    let chunk_size = DEFAULT_CHUNK_SIZE;

    if data.is_empty() {
      // Even empty files get zero chunks — that's fine
    } else {
      let mut offset = 0;
      while offset < data.len() {
        let end = (offset + chunk_size).min(data.len());
        let chunk_data = &data[offset..end];

        // Hash is ALWAYS on uncompressed data (for dedup)
        let chunk_key = chunk_content_hash(chunk_data, &algo)?;

        // Dedup: only store if not already present
        if !self.engine.has_entry(&chunk_key)? {
          if compression_algo != CompressionAlgorithm::None {
            let compressed_data = compress(chunk_data, compression_algo)?;
            if sys_flags != 0 {
              self.engine.store_entry_compressed_with_flags(
                EntryType::Chunk, &chunk_key, &compressed_data, sys_flags, compression_algo,
              )?;
            } else {
              self.engine.store_entry_compressed(
                EntryType::Chunk, &chunk_key, &compressed_data, compression_algo,
              )?;
            }
          } else if sys_flags != 0 {
            self.engine.store_entry_with_flags(
              EntryType::Chunk, &chunk_key, chunk_data, sys_flags,
            )?;
          } else {
            self.engine.store_entry(
              EntryType::Chunk, &chunk_key, chunk_data,
            )?;
          }
          self.engine.counters().increment_chunks();
          self.engine.counters().add_chunk_data_size(chunk_data.len() as u64);
        } else {
          self.engine.counters().increment_chunks_deduped();
        }

        chunk_hashes.push(chunk_key);
        offset = end;
      }
    }

    // Check if file already exists (for preserving created_at on overwrite)
    let file_key = file_path_hash(&normalized, &algo)?;
    let (existing_created_at, existing_total_size) = match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        let existing = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        (Some(existing.created_at), Some(existing.total_size))
      }
      None => (None, None),
    };

    // Create the FileRecord with detected content type
    let mut file_record = FileRecord::new(
      normalized.clone(),
      Some(detected_content_type.clone()),
      data.len() as u64,
      chunk_hashes,
    );

    // Preserve original created_at on overwrite
    if let Some(original_created_at) = existing_created_at {
      file_record.created_at = original_created_at;
    }

    let file_value = file_record.serialize(hash_length)?;

    // Content-addressed key (immutable — for KV store entry)
    let file_content_key = file_content_hash(&file_value, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &file_content_key, &file_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;
    }

    // Identity hash (for ChildEntry.hash — excludes timestamps)
    let identity_key = file_identity_hash(&normalized, Some(detected_content_type.as_str()), &file_record.chunk_hashes, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &identity_key, &file_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &identity_key, &file_value)?;
    }

    // Path-based key (mutable — for reads, indexing, deletion)
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &file_key, &file_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;
    }

    // Build child entry with identity hash (not content hash)
    let now_vt = chrono::Utc::now().timestamp_millis() as u64;
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: identity_key,
      total_size: data.len() as u64,
      created_at: file_record.created_at,
      updated_at: file_record.updated_at,
      name: file_name(&normalized).unwrap_or("").to_string(),
      content_type: Some(detected_content_type.clone()),
      virtual_time: now_vt,
      node_id: 0,
    };

    self.update_parent_directories(&normalized, child)?;

    // Update counters
    let counters = self.engine.counters();
    counters.increment_writes();
    counters.add_bytes_written(data.len() as u64);
    if existing_created_at.is_none() {
      // New file
      counters.increment_files();
      counters.add_logical_data_size(data.len() as u64);
    } else {
      // Overwrite — adjust logical_data_size by delta
      let old_size = existing_total_size.unwrap_or(0);
      let new_size = data.len() as u64;
      if new_size >= old_size {
        counters.add_logical_data_size(new_size - old_size);
      } else {
        counters.sub_logical_data_size(old_size - new_size);
      }
    }

    // Emit entry event after successful store
    let event_type = EVENT_ENTRIES_CREATED;
    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "file".to_string(),
      content_type: file_record.content_type.clone(),
      size: file_record.total_size,
      hash: hex::encode(file_record.chunk_hashes.first().unwrap_or(&vec![])),
      created_at: file_record.created_at,
      updated_at: file_record.updated_at,
      previous_hash: None,
    };
    ctx.emit(event_type, serde_json::json!({"entries": [entry_data]}));

    Ok(file_record)
  }

  /// Restore a file from an existing FileRecord without re-reading chunk data.
  /// The chunks must already exist in the database (e.g., from a historical snapshot).
  /// This avoids loading the entire file into memory for large file restores.
  pub fn restore_file_from_record(
    &self,
    ctx: &RequestContext,
    path: &str,
    source_record: &FileRecord,
  ) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let content_type = source_record.content_type.as_deref()
      .unwrap_or("application/octet-stream");

    // Preserve created_at if file already exists
    let file_key = file_path_hash(&normalized, &algo)?;
    let existing_created_at = match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        let existing = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        Some(existing.created_at)
      }
      None => None,
    };

    // Create new FileRecord pointing to the same chunks
    let mut file_record = FileRecord::new(
      normalized.clone(),
      Some(content_type.to_string()),
      source_record.total_size,
      source_record.chunk_hashes.clone(),
    );
    if let Some(original_created_at) = existing_created_at {
      file_record.created_at = original_created_at;
    }

    let file_value = file_record.serialize(hash_length)?;

    // Store at all three keys (content, identity, path)
    let file_content_key = file_content_hash(&file_value, &algo)?;
    self.engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;

    let identity_key = file_identity_hash(&normalized, Some(content_type), &file_record.chunk_hashes, &algo)?;
    self.engine.store_entry(EntryType::FileRecord, &identity_key, &file_value)?;
    self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

    // Update parent directories
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: identity_key,
      total_size: source_record.total_size,
      created_at: file_record.created_at,
      updated_at: file_record.updated_at,
      name: file_name(&normalized).unwrap_or("").to_string(),
      content_type: Some(content_type.to_string()),
      virtual_time: chrono::Utc::now().timestamp_millis() as u64,
      node_id: 0,
    };
    self.update_parent_directories(&normalized, child)?;

    ctx.emit(
      crate::engine::engine_event::EVENT_ENTRIES_CREATED,
      serde_json::json!({"entries": [{
        "path": normalized,
        "entry_type": "file",
        "content_type": content_type,
        "size": source_record.total_size,
      }]}),
    );

    Ok(())
  }

  /// Read a file as a streaming iterator of chunk data.
  pub fn read_file_streaming(&self, path: &str) -> EngineResult<EngineFileStream> {
    let timer_start = std::time::Instant::now();
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    // User-facing read — verify hash integrity
    let entry = self.engine.get_entry_verified(&file_key)?
      .ok_or_else(|| EngineError::NotFound(normalized.clone()))?;

    let (header, _key, value) = entry;
    let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;

    let counters = self.engine.counters();
    counters.increment_reads();
    counters.add_bytes_read(file_record.total_size);

    let elapsed = timer_start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::FILE_READ_DURATION).record(elapsed);

    EngineFileStream::new(file_record.chunk_hashes, self.engine, false)
  }

  /// Read a file's full content into memory.
  pub fn read_file(&self, path: &str) -> EngineResult<Vec<u8>> {
    let result = self.read_file_streaming(path)?.collect_to_vec()?;
    Ok(result)
  }

  /// Delete a file, storing a DeletionRecord and updating parent directories.
  /// Takes an auto-snapshot before delete (throttled to once per minute).
  pub fn delete_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);

    // Auto-snapshot before delete (at most once per minute)
    if !is_system_path(&normalized) {
      self.auto_snapshot_before_delete(ctx);
    }

    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let sys_flags = if is_system_path(&normalized) { FLAG_SYSTEM } else { 0 };

    // Verify the file exists and capture metadata for event
    let file_key = file_path_hash(&normalized, &algo)?;
    let file_record_opt = match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        Some(FileRecord::deserialize(&value, hash_length, header.entry_version)?)
      }
      None => {
        return Err(EngineError::NotFound(normalized));
      }
    };

    // Store a DeletionRecord
    let deletion = DeletionRecord::new(normalized.clone(), None);
    let deletion_key = deletion_record_hash(
      &normalized,
      deletion.deleted_at,
      &algo,
    )?;
    let deletion_value = deletion.serialize();
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }

    // Mark the FileRecord as deleted in the KV store
    self.engine.mark_entry_deleted(&file_key)?;

    // Remove child from parent directory
    self.remove_from_parent_directory(&normalized)?;

    // Update counters
    if let Some(ref record) = file_record_opt {
      let counters = self.engine.counters();
      counters.decrement_files();
      counters.sub_logical_data_size(record.total_size);
    }

    // Emit deletion event with captured metadata
    if let Some(record) = file_record_opt {
      let entry_data = EntryEventData {
        path: normalized,
        entry_type: "file".to_string(),
        content_type: record.content_type,
        size: record.total_size,
        hash: hex::encode(record.chunk_hashes.first().unwrap_or(&vec![])),
        created_at: record.created_at,
        updated_at: record.updated_at,
        previous_hash: None,
      };
      ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [entry_data]}));
    }

    Ok(())
  }

  /// Delete an empty directory. Returns an error if the directory has children.
  ///
  /// **TOCTOU note**: The emptiness check is not fully atomic with the deletion.
  /// A TransactionGuard documents the atomicity boundary. After mark_entry_deleted
  /// and remove_from_parent_directory, we re-check the raw directory data for
  /// children. If a concurrent write sneaked in between the initial check and
  /// the deletion, those children are now orphaned -- but we log a warning so
  /// the condition is observable (and GC will eventually reclaim them).
  pub fn delete_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let algo = self.engine.hash_algo();

    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot delete root directory".to_string()));
    }

    // Verify the directory exists and is empty
    let children = self.list_directory(&normalized)?;
    if !children.is_empty() {
      return Err(EngineError::InvalidInput(
        format!("Directory '{}' is not empty ({} children)", normalized, children.len()),
      ));
    }

    // Mark the directory index entry as deleted
    let dir_key = directory_path_hash(&normalized, &algo)?;
    self.engine.mark_entry_deleted(&dir_key)?;

    // Remove from parent listing
    self.remove_from_parent_directory(&normalized)?;

    // TOCTOU re-check: verify no children were added between our emptiness
    // check and the deletion. The directory entry is already marked deleted,
    // so use get_entry_including_deleted to read the raw data at that offset.
    if let Ok(Some((_header, _key, value))) = self.engine.get_entry_including_deleted(&dir_key) {
      if !value.is_empty() {
        let hash_length = algo.hash_length();
        let recheck_children = if crate::engine::btree::is_btree_format(&value) {
          crate::engine::btree::btree_list_from_node(&value, self.engine, hash_length, false)
            .unwrap_or_default()
        } else {
          deserialize_child_entries(&value, hash_length, 0).unwrap_or_default()
        };
        if !recheck_children.is_empty() {
          tracing::warn!(
            path = %normalized,
            orphaned_children = recheck_children.len(),
            "TOCTOU race in delete_directory: children were added concurrently and are now orphaned"
          );
        }
      }
    }

    // Update counters
    self.engine.counters().decrement_directories();

    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [{
      "path": normalized,
      "entry_type": "directory",
    }]}));

    Ok(())
  }

  /// List the children of a directory.
  ///
  /// If the directory index or child entries are corrupt, logs a warning
  /// and returns an empty listing instead of failing the entire operation.
  /// `NotFound` is still returned as an error (directory genuinely doesn't exist).
  pub fn list_directory(&self, path: &str) -> EngineResult<Vec<ChildEntry>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let dir_key = directory_path_hash(&normalized, &algo)?;
    if normalized == "/" {
      let snapshot = self.engine.kv_snapshot.load();
      if let Some(kv_entry) = snapshot.get(&dir_key) {
        tracing::debug!(
          kv_offset = kv_entry.offset,
          kv_type = kv_entry.type_flags,
          "list_directory: root KV entry"
        );
      }
    }
    match self.engine.get_entry(&dir_key) {
      Ok(Some((header, _key, value))) => {
        if normalized == "/" {
          tracing::debug!(
            value_len = value.len(),
            is_btree = if value.is_empty() { false } else { crate::engine::btree::is_btree_format(&value) },
            first_bytes = %if value.is_empty() { "empty".to_string() } else { hex::encode(&value[..value.len().min(16)]) },
            "list_directory: root entry"
          );
        }
        if value.is_empty() {
          return Ok(Vec::new());
        }
        if crate::engine::btree::is_btree_format(&value) {
          // B-tree format: value is the root node data
          match crate::engine::btree::btree_list_from_node(&value, self.engine, hash_length, false) {
            Ok(children) => Ok(children),
            Err(e) => {
              tracing::warn!(
                "Corrupt B-tree directory index at '{}': {}. Returning empty listing.",
                normalized, e
              );
              Ok(Vec::new())
            }
          }
        } else {
          // Flat format
          match deserialize_child_entries(&value, hash_length, header.entry_version) {
            Ok(children) => Ok(children),
            Err(e) => {
              tracing::warn!(
                "Corrupt directory index at '{}': {}. Returning empty listing.",
                normalized, e
              );
              Ok(Vec::new())
            }
          }
        }
      }
      Ok(None) => Err(EngineError::NotFound(normalized)),
      Err(e) => {
        tracing::warn!(
          "Error reading directory '{}': {}",
          normalized, e
        );
        Err(e)
      }
    }
  }

  /// Create an empty directory at the given path.
  pub fn create_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let algo = self.engine.hash_algo();

    let dir_key = directory_path_hash(&normalized, &algo)?;

    // Store empty directory index at path-based key
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &dir_key,
      &[],
    )?;

    // Also store at content-addressed key for immutable versioning
    let content_key = directory_content_hash(&[], &algo)?;
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &content_key,
      &[],
    )?;

    // Update parent directory if this isn't root
    let now = chrono::Utc::now().timestamp_millis();
    if normalized != "/" {
      let child = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,  // content hash for tree walker
        total_size: 0,
        created_at: now,
        updated_at: now,
        name: file_name(&normalized).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: now as u64,
        node_id: 0,
      };
      self.update_parent_directories(&normalized, child)?;
    }

    // Emit directory creation event
    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "directory".to_string(),
      content_type: None,
      size: 0,
      hash: String::new(),
      created_at: now,
      updated_at: now,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [entry_data]}));

    self.engine.counters().increment_directories();

    Ok(())
  }

  /// Get the FileRecord metadata for a file path.
  pub fn get_metadata(&self, path: &str) -> EngineResult<Option<FileRecord>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        let record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Check if a file or directory exists at the given path.
  pub fn exists(&self, path: &str) -> EngineResult<bool> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

    let file_key = file_path_hash(&normalized, &algo)?;
    if self.engine.has_entry(&file_key)? {
      return Ok(true);
    }

    let dir_key = directory_path_hash(&normalized, &algo)?;
    self.engine.has_entry(&dir_key)
  }

  /// List deleted files whose paths are under the given directory.
  /// Returns a list of (path, deleted_at) tuples.
  pub fn list_deleted(&self, dir_path: &str) -> EngineResult<Vec<crate::engine::deletion_record::DeletionRecord>> {
    let normalized = normalize_path(dir_path);
    let prefix = if normalized == "/" { "/".to_string() } else { format!("{}/", normalized.trim_end_matches('/')) };

    let deletion_entries = self.engine.entries_by_type(
      crate::engine::kv_store::KV_TYPE_DELETION,
    )?;

    let mut results = Vec::new();
    for (_hash, value) in &deletion_entries {
      if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(value) {
        if record.path.starts_with("/.aeordb-") { continue; }
        // Check if this deletion is a direct child of the requested directory
        if record.path.starts_with(&prefix) || (normalized == "/" && record.path.starts_with('/')) {
          let remainder = if normalized == "/" {
            &record.path[1..]
          } else {
            &record.path[prefix.len()..]
          };
          // Direct child: no further slashes in the remainder
          if !remainder.contains('/') && !remainder.is_empty() {
            results.push(record);
          }
        }
      }
    }

    // Sort by deleted_at descending (most recent first)
    results.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    // Deduplicate by path (keep most recent deletion)
    let mut seen = std::collections::HashSet::new();
    results.retain(|r| seen.insert(r.path.clone()));

    Ok(results)
  }

  /// Take an auto-snapshot before a destructive operation.
  /// Uses a per-lane AtomicI64 so delete/restore/manual snapshots
  /// don't block each other. Each lane throttles independently.
  fn auto_snapshot_throttled(
    &self,
    ctx: &RequestContext,
    lane: &std::sync::atomic::AtomicI64,
    throttle_ms: i64,
    prefix: &str,
  ) {
    use std::sync::atomic::Ordering;
    let now = chrono::Utc::now().timestamp_millis();
    let last = lane.load(Ordering::Relaxed);
    let elapsed = now - last;

    if elapsed < throttle_ms && last > 0 {
      return;
    }

    // Try to claim the slot (CAS prevents races)
    if lane
      .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed)
      .is_err()
    {
      return; // another thread beat us
    }

    let vm = crate::engine::version_manager::VersionManager::new(self.engine);
    let dt = chrono::Utc::now();
    let name = format!(
      "{} {}-{}-{} {}:{}:{}.{:03}",
      prefix,
      dt.format("%Y"), dt.format("%m"), dt.format("%d"),
      dt.format("%H"), dt.format("%M"), dt.format("%S"),
      dt.timestamp_subsec_millis(),
    );

    match vm.create_snapshot(ctx, &name, std::collections::HashMap::new()) {
      Ok(_) => {
        tracing::info!(snapshot = %name, "Auto-snapshot ({})", prefix);
      }
      Err(e) => {
        tracing::warn!("Auto-snapshot ({}) failed: {}", prefix, e);
        lane.store(last, Ordering::Relaxed);
      }
    }
  }

  /// Auto-snapshot before delete — own lane, 60s throttle.
  fn auto_snapshot_before_delete(&self, ctx: &RequestContext) {
    self.auto_snapshot_throttled(
      ctx,
      &self.engine.last_auto_snapshot_delete,
      60_000,
      "auto-pre-delete",
    );
  }

  /// Auto-snapshot before restore — own lane, 60s throttle.
  pub fn auto_snapshot_before_restore(&self, ctx: &RequestContext) {
    self.auto_snapshot_throttled(
      ctx,
      &self.engine.last_auto_snapshot_restore,
      60_000,
      "auto-pre-restore",
    );
  }

  /// Restore a deleted file by un-marking it in the KV and re-adding
  /// it to its parent directory.
  pub fn restore_deleted_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;

    // Try to read the file record even though it's marked deleted.
    // get_raw bypasses the deleted flag check.
    let file_record = {
      let snapshot = self.engine.kv_snapshot.load();
      let kv_entry = snapshot.get_raw(&file_key)
        .ok_or_else(|| EngineError::NotFound(format!("No record found for deleted file: {}", normalized)))?;

      let writer = self.engine.writer_read_lock()?;
      let (header, _key, value) = writer.read_entry_at_shared(kv_entry.offset)?;
      FileRecord::deserialize(&value, hash_length, header.entry_version)?
    };

    // Re-store the file record at the path key (this creates a new WAL
    // entry and un-marks the KV entry)
    let file_value = file_record.serialize(hash_length)?;
    self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

    // Also store at the identity key (immutable, used by ChildEntry.hash
    // for version tree walks and GC marking — mirrors store_file_internal)
    let identity_key = file_identity_hash(
      &normalized,
      file_record.content_type.as_deref(),
      &file_record.chunk_hashes,
      &algo,
    )?;
    self.engine.store_entry(EntryType::FileRecord, &identity_key, &file_value)?;

    // Also store at the content key (immutable content-addressed entry)
    let content_key = file_content_hash(&file_value, &algo)?;
    self.engine.store_entry(EntryType::FileRecord, &content_key, &file_value)?;

    // Re-add to parent directory using identity_key (not file_key)
    let child = ChildEntry {
      name: crate::engine::path_utils::file_name(&normalized).unwrap_or("").to_string(),
      entry_type: EntryType::FileRecord.to_u8(),
      hash: identity_key,
      total_size: file_record.total_size,
      content_type: file_record.content_type.clone(),
      created_at: file_record.created_at,
      updated_at: chrono::Utc::now().timestamp_millis(),
      virtual_time: 0,
      node_id: 0,
    };
    self.update_parent_directories(&normalized, child)?;

    self.engine.counters().increment_files();

    ctx.emit(
      crate::engine::engine_event::EVENT_ENTRIES_CREATED,
      serde_json::json!({"entries": [{
        "path": normalized,
        "entry_type": "file",
        "content_type": file_record.content_type,
        "size": file_record.total_size,
      }]}),
    );

    Ok(())
  }

  /// Ensure the root directory exists. Called during database creation.
  pub fn ensure_root_directory(&self, _ctx: &RequestContext) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash("/", &algo)?;

    // If the root directory exists and has children, leave it alone.
    if self.engine.has_entry(&dir_key)? {
      match self.list_directory("/") {
        Ok(children) if !children.is_empty() => return Ok(()),
        _ => {
          // Root entry exists but is empty or unreadable — continue to
          // create a fresh one. This self-heals after a repair where the
          // root directory's children list was overwritten by a previous
          // startup on a corrupt database.
          tracing::warn!("Root directory exists but is empty, will recreate");
        }
      }
    }

    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &dir_key,
      &[],
    )?;

    // Also store at content-addressed key for immutable versioning
    let content_key = directory_content_hash(&[], &algo)?;
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &content_key,
      &[],
    )?;

    // Update HEAD to point to content hash (immutable) instead of path hash
    self.engine.update_head(&content_key)?;

    Ok(())
  }

  /// Rebuild the directory tree by scanning all file records in the KV and
  /// re-propagating their parent directories up to root. Used by `verify --repair`
  /// when the root directory is empty but files exist (e.g., after a KV rebuild
  /// where the root directory entry was overwritten by a prior corrupt session).
  pub fn rebuild_directory_tree(&self, ctx: &RequestContext) -> EngineResult<usize> {
    let hash_length = self.engine.hash_algo().hash_length();
    let snapshot = self.engine.kv_snapshot.load();
    let all_entries = snapshot.iter_all()?;

    let mut paths_propagated = 0;
    let mut file_records_found = 0;
    let mut skipped_system = 0;
    let mut skipped_error = 0;
    for entry in &all_entries {
      let kv_type = entry.type_flags & 0x0F;
      if kv_type != crate::engine::kv_store::KV_TYPE_FILE_RECORD { continue; }
      file_records_found += 1;

      // Read the file record to get its path
      match self.engine.get_entry(&entry.hash) {
        Ok(Some((header, _key, value))) => {
          match crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) {
            Ok(record) => {
              let path = &record.path;
              if path.is_empty() || path.starts_with("/.aeordb-") { skipped_system += 1; continue; }

              // Build child entry and propagate to parents
              let child = ChildEntry {
                name: crate::engine::path_utils::file_name(path).unwrap_or("").to_string(),
                entry_type: crate::engine::entry_type::EntryType::FileRecord.to_u8(),
                hash: entry.hash.clone(),
                total_size: record.total_size,
                content_type: record.content_type.clone(),
                created_at: record.created_at,
                updated_at: record.updated_at,
                virtual_time: 0,
                node_id: 0,
              };
              if let Err(e) = self.update_parent_directories(path, child) {
                tracing::debug!("Skipping path '{}' during rebuild: {}", path, e);
                continue;
              }
              paths_propagated += 1;
            }
            Err(_) => { skipped_error += 1; continue; }
          }
        }
        _ => { skipped_error += 1; continue; }
      }
    }

    // Also discover directories from KV entries and propagate them up
    // to root. This rebuilds the root directory even when file records
    // are at corrupt offsets.
    let mut dirs_propagated = 0;
    self.rebuild_dirs_from_kv(&mut dirs_propagated);

    tracing::debug!(
      file_records_found, paths_propagated, skipped_system, skipped_error, dirs_propagated,
      "rebuild_directory_tree complete"
    );

    Ok(paths_propagated + dirs_propagated)
  }

  /// Discover directories by scanning all directory entries in the KV,
  /// reading their children, and building a path map by brute-force
  /// trying candidate paths. Then propagate each to its parent.
  fn rebuild_dirs_from_kv(&self, count: &mut usize) {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let snapshot = self.engine.kv_snapshot.load();
    let all = match snapshot.iter_all() {
      Ok(e) => e,
      Err(_) => return,
    };

    // Collect all directory entries that have children
    let mut dir_hashes: std::collections::HashMap<Vec<u8>, Vec<ChildEntry>> = std::collections::HashMap::new();
    let mut dir_count = 0;
    let mut dir_empty = 0;
    let mut dir_read_err = 0;
    let mut dir_parse_err = 0;
    for entry in &all {
      let kv_type = entry.type_flags & 0x0F;
      if kv_type != crate::engine::kv_store::KV_TYPE_DIRECTORY { continue; }
      dir_count += 1;

      match self.engine.get_entry(&entry.hash) {
        Ok(Some((header, _key, value))) => {
          if value.is_empty() { dir_empty += 1; continue; }
          let children = if crate::engine::btree::is_btree_format(&value) {
            crate::engine::btree::btree_list_from_node(&value, self.engine, hash_length, false).ok()
          } else {
            crate::engine::directory_entry::deserialize_child_entries(
              &value, hash_length, header.entry_version
            ).ok()
          };
          match children {
            Some(c) if !c.is_empty() => { dir_hashes.insert(entry.hash.clone(), c); }
            Some(_) => { dir_empty += 1; }
            None => { dir_parse_err += 1; }
          }
        }
        Ok(None) => { dir_read_err += 1; }
        Err(_) => { dir_read_err += 1; }
      }
    }

    tracing::debug!(
      dir_count, dir_empty, dir_read_err, dir_parse_err,
      dir_with_children = dir_hashes.len(),
      "rebuild_dirs_from_kv: scanned directory entries"
    );
    if dir_hashes.is_empty() { return; }

    // Discover paths: start with "/" and walk children.
    let mut known: Vec<(String, Vec<ChildEntry>)> = Vec::new();

    // Seed: try root "/"
    if let Ok(root_hash) = directory_path_hash("/", &algo) {
      if let Some(children) = dir_hashes.get(&root_hash) {
        known.push(("/".to_string(), children.clone()));
      }
    }

    tracing::debug!(root_found = !known.is_empty(), "rebuild_dirs_from_kv: root check");

    // If root itself is empty/missing, try to discover top-level dirs
    // by checking ALL child names from ALL directory entries as potential
    // top-level paths.
    if known.is_empty() {
      let mut candidates_tried = 0;
      let mut candidates_found = 0;
      for (_hash, children) in &dir_hashes {
        for child in children {
          if child.entry_type != crate::engine::entry_type::EntryType::DirectoryIndex.to_u8() { continue; }
          let candidate = format!("/{}", child.name);
          candidates_tried += 1;
          if let Ok(candidate_hash) = directory_path_hash(&candidate, &algo) {
            if dir_hashes.contains_key(&candidate_hash) {
              candidates_found += 1;
              tracing::debug!(path = %candidate, "rebuild_dirs_from_kv: discovered top-level dir");
              if !known.iter().any(|(p, _)| *p == "/") {
                // We found a top-level dir — seed with a synthetic root
                known.push(("/".to_string(), Vec::new()));
              }
              if let Some(dir_children) = dir_hashes.get(&candidate_hash) {
                if !known.iter().any(|(p, _)| *p == candidate) {
                  known.push((candidate, dir_children.clone()));
                }
              }
            }
          }
        }
      }
      tracing::debug!(candidates_tried, candidates_found, "rebuild_dirs_from_kv: top-level discovery done");
    }

    tracing::debug!(known_paths = known.len(), "rebuild_dirs_from_kv: before deep walk");

    // Walk deeper
    let mut depth = 0;
    loop {
      let mut found_new = false;
      depth += 1;
      if depth > 50 { break; }
      let snapshot: Vec<(String, Vec<ChildEntry>)> = known.clone();
      for (parent, children) in &snapshot {
        for child in children {
          if child.entry_type != crate::engine::entry_type::EntryType::DirectoryIndex.to_u8() { continue; }
          let child_path = if *parent == "/" {
            format!("/{}", child.name)
          } else {
            format!("{}/{}", parent.trim_end_matches('/'), child.name)
          };
          if known.iter().any(|(p, _)| *p == child_path) { continue; }
          if let Ok(child_hash) = directory_path_hash(&child_path, &algo) {
            if let Some(dir_children) = dir_hashes.get(&child_hash) {
              known.push((child_path, dir_children.clone()));
              found_new = true;
            }
          }
        }
      }
      if !found_new { break; }
    }

    tracing::debug!(total_known = known.len(), "rebuild_dirs_from_kv: propagating directories");
    // Propagate each discovered directory to its parent
    for (dir_path, _children) in &known {
      if *dir_path == "/" { continue; }
      tracing::debug!(dir_path = %dir_path, "rebuild_dirs_from_kv: propagating");
      // Read the actual directory content and propagate as a child entry
      if let Ok(dir_children) = self.list_directory(dir_path) {
        if dir_children.is_empty() { continue; }
      }
      let now_ms = chrono::Utc::now().timestamp_millis();
      if let Ok(content_hash) = directory_path_hash(dir_path, &algo) {
        let dir_name = crate::engine::path_utils::file_name(dir_path).unwrap_or("").to_string();
        let child = ChildEntry {
          name: dir_name,
          entry_type: crate::engine::entry_type::EntryType::DirectoryIndex.to_u8(),
          hash: content_hash,
          total_size: 0,
          content_type: None,
          created_at: now_ms,
          updated_at: now_ms,
          virtual_time: 0,
          node_id: 0,
        };
        if self.update_parent_directories(dir_path, child).is_ok() {
          *count += 1;
        }
      }
    }
  }

  /// Detect the compression algorithm for a file based on its parent's index config.
  /// Reads `.config/indexes.json` under the parent path; returns Zstd if configured
  /// and the content type/size pass the `should_compress` heuristic, else None.
  fn detect_compression(
    &self,
    path: &str,
    content_type: Option<&str>,
    data_length: usize,
  ) -> CompressionAlgorithm {
    let normalized = normalize_path(path);
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
    let config_path = if parent.ends_with('/') {
      format!("{}.config/indexes.json", parent)
    } else {
      format!("{}/.aeordb-config/indexes.json", parent)
    };

    match self.read_file(&config_path) {
      Ok(config_data) => {
        match PathIndexConfig::deserialize_with_compression(&config_data) {
          Ok(Some(algo_str)) if algo_str == "zstd" => {
            if should_compress(content_type, data_length) {
              CompressionAlgorithm::Zstd
            } else {
              CompressionAlgorithm::None
            }
          }
          _ => CompressionAlgorithm::None,
        }
      }
      Err(_) => CompressionAlgorithm::None,
    }
  }

  /// Store a file with automatic index updates and optional compression.
  /// After storing the file, checks for index config at `.config/indexes.json`
  /// under the parent path and updates relevant indexes.
  /// Compression is determined by config or auto-detection via `should_compress`.
  pub fn store_file_with_indexing(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> EngineResult<FileRecord> {
    let compression_algo = self.detect_compression(path, content_type, data.len());
    let file_record = self.store_file_internal(ctx, path, data, content_type, compression_algo)?;

    // Guard: skip indexing for system directories
    if is_internal_path(path) {
      return Ok(file_record);
    }

    // Delegate to indexing pipeline using the detected content type from the file record
    let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
    let detected_ct = file_record.content_type.as_deref();
    if let Err(e) = pipeline.run(ctx, path, data, detected_ct) {
      tracing::warn!("Indexing pipeline failed for '{}': {}", path, e);
    }

    Ok(file_record)
  }

  /// Store a file with the full indexing pipeline including parser plugin support.
  pub fn store_file_with_full_pipeline(
    &self,
    ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    plugin_manager: Option<&crate::plugins::PluginManager>,
  ) -> EngineResult<FileRecord> {
    let compression_algo = self.detect_compression(path, content_type, data.len());

    let file_record = self.store_file_internal(ctx, path, data, content_type, compression_algo)?;

    if is_internal_path(path) {
      return Ok(file_record);
    }

    // Use full pipeline with plugin manager, passing detected content type
    let pipeline = match plugin_manager {
      Some(pm) => crate::engine::indexing_pipeline::IndexingPipeline::with_plugin_manager(self.engine, pm),
      None => crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine),
    };
    let detected_ct = file_record.content_type.as_deref();
    if let Err(e) = pipeline.run(ctx, path, data, detected_ct) {
      tracing::warn!("Indexing pipeline failed for '{}': {}", path, e);
    }

    Ok(file_record)
  }

  /// Delete a file and remove its entries from all indexes at that path.
  pub fn delete_file_with_indexing(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let algo = self.engine.hash_algo();
    let file_key = file_path_hash(&normalized, &algo)?;
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());

    // Remove from indexes before deleting the file
    let index_manager = IndexManager::new(self.engine);
    let index_names = index_manager.list_indexes(&parent)?;

    for field_name in &index_names {
      if let Some(mut index) = index_manager.load_index(&parent, field_name)? {
        index.remove(&file_key);
        index_manager.save_index(&parent, &index)?;
      }
    }

    // Also check ancestor directories for glob-based configs
    let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
    if let Ok(Some((_config, config_dir))) = pipeline.find_config_for_path(&normalized) {
      if config_dir != parent {
        let ancestor_index_names = index_manager.list_indexes(&config_dir)?;
        for field_name in &ancestor_index_names {
          if let Some(mut index) = index_manager.load_index(&config_dir, field_name)? {
            index.remove(&file_key);
            index_manager.save_index(&config_dir, &index)?;
          }
        }
      }
    }

    // Now delete the file itself
    self.delete_file(ctx, path)
  }

  /// Read directory data by path key, following hard links and checking the
  /// content cache. Returns the entry header and directory value bytes.
  ///
  /// Hard link detection: if the value at dir_key is exactly hash_length bytes,
  /// it's a hard link (content hash pointer). Follow it to get the actual data.
  /// Backward compatible: values >hash_length are inline data (pre-optimization).
  pub(crate) fn read_directory_data(&self, dir_key: &[u8]) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    let hash_length = self.engine.hash_algo().hash_length();

    let entry = match self.engine.get_entry(dir_key)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let (header, _key, value) = entry;

    // Check if this is a hard link (value == hash_length bytes)
    if value.len() == hash_length {
      let content_key = &value;

      // Check cache first
      if let Some(cached) = self.engine.get_cached_dir_content(content_key) {
        return Ok(Some((header, cached)));
      }

      // Cache miss — read from WAL
      match self.engine.get_entry(content_key)? {
        Some((_h, _k, content_value)) => {
          // Cache for future reads
          self.engine.cache_dir_content(content_key.to_vec(), content_value.clone());
          Ok(Some((header, content_value)))
        }
        None => {
          tracing::warn!("Hard link target not found for directory entry");
          Ok(None)
        }
      }
    } else {
      // Inline data (backward compatible or empty directory)
      Ok(Some((header, value)))
    }
  }

  /// Maximum directory depth for update_parent_directories iteration.
  /// Prevents unbounded looping on pathologically deep paths.
  const MAX_DIRECTORY_DEPTH: usize = 1000;

  /// Update parent directories after a child is added or modified.
  /// Propagates from the immediate parent up to root, updating HEAD at the end.
  /// For directories with >= BTREE_CONVERSION_THRESHOLD children, uses B-tree
  /// storage for O(log N) insertions instead of rewriting the entire flat list.
  ///
  /// Iterative implementation: walks from the child's parent up to root,
  /// bounded by MAX_DIRECTORY_DEPTH as a safety measure.
  fn update_parent_directories(
    &self,
    child_path: &str,
    child_entry: ChildEntry,
  ) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let mut current_child_path = child_path.to_string();
    let mut current_child_entry = child_entry;
    let mut batch = WriteBatch::new();

    for _depth in 0..Self::MAX_DIRECTORY_DEPTH {
      let parent = match parent_path(&current_child_path) {
        Some(parent) => parent,
        None => {
          // root has no parent
          if !batch.is_empty() { self.engine.flush_batch(batch)?; }
          return Ok(());
        }
      };

      // Don't propagate system paths (/.aeordb-*) to root — they're accessed
      // directly and listing root would filter them anyway. This prevents
      // system path operations from clobbering a recovered root directory.
      if parent == "/" && is_system_path(&current_child_path) {
        if !batch.is_empty() { self.engine.flush_batch(batch)?; }
        return Ok(());
      }

      let dir_key = directory_path_hash(&parent, &algo)?;

      // Read existing directory via cache-aware, hard-link-following reader
      let existing = self.read_directory_data(&dir_key)?;

      let (dir_value, content_key) = match existing {
        Some((_header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
          // === B-TREE FORMAT ===
          // B-tree nodes are stored synchronously by btree_insert_batched
          let (new_root_hash, new_root_data) = crate::engine::btree::btree_insert_batched(
            self.engine, &value, current_child_entry, hash_length, &algo
          )?;

          // Cache the B-tree root data for subsequent reads in this propagation
          self.engine.cache_dir_content(new_root_hash.clone(), new_root_data.clone());
          (new_root_data, new_root_hash)
        }
        Some((header, value)) => {
          // === FLAT FORMAT ===
          let mut children = if value.is_empty() {
            Vec::new()
          } else {
            deserialize_child_entries(&value, hash_length, header.entry_version)?
          };

          // Add or update the child
          let child_name = &current_child_entry.name;
          if let Some(existing) = children.iter_mut().find(|c| c.name == *child_name) {
            *existing = current_child_entry;
          } else {
            children.push(current_child_entry);
          }

          // Check if we should convert to B-tree
          if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
            // Convert flat -> B-tree (nodes stored synchronously)
            let root_hash = crate::engine::btree::btree_from_entries(
              self.engine, children, hash_length, &algo
            )?;
            let root_entry = self.engine.get_entry(&root_hash)?
              .ok_or_else(|| EngineError::NotFound("B-tree root not found after conversion".to_string()))?;
            self.engine.cache_dir_content(root_hash.clone(), root_entry.2.clone());
            (root_entry.2, root_hash)
          } else {
            // Stay flat — batch the content write
            let dir_value = serialize_child_entries(&children, hash_length)?;
            let content_key = directory_content_hash(&dir_value, &algo)?;
            batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
            self.engine.cache_dir_content(content_key.clone(), dir_value.clone());
            (dir_value, content_key)
          }
        }
        None => {
          // New directory (implicitly created for an intermediate parent)
          self.engine.counters().increment_directories();
          let children = vec![current_child_entry];
          let dir_value = serialize_child_entries(&children, hash_length)?;
          let content_key = directory_content_hash(&dir_value, &algo)?;
          batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
          self.engine.cache_dir_content(content_key.clone(), dir_value.clone());
          (dir_value, content_key)
        }
      };

      // Hard link at path-based key: store content hash instead of full data
      batch.add(EntryType::DirectoryIndex, dir_key, content_key.clone());

      // If this is root "/", flush the entire batch and update HEAD atomically
      if parent == "/" {
        self.engine.flush_batch_and_update_head(batch, &content_key)?;
        return Ok(());
      }

      // Set up next iteration: update grandparent with this directory as child
      let now_ms = chrono::Utc::now().timestamp_millis();
      current_child_entry = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,  // content hash for tree walker
        total_size: dir_value.len() as u64,
        created_at: now_ms,
        updated_at: now_ms,
        name: file_name(&parent).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: now_ms as u64,
        node_id: 0,
      };
      current_child_path = parent;
    }

    Err(EngineError::InvalidInput(
      format!("Directory depth exceeds maximum of {} levels", Self::MAX_DIRECTORY_DEPTH),
    ))
  }

  /// Remove a child entry from its parent directory and propagate up.
  /// Handles both flat and B-tree directory formats.
  fn remove_from_parent_directory(&self, child_path: &str) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let parent = match parent_path(child_path) {
      Some(parent) => parent,
      None => return Ok(()),
    };

    let dir_key = directory_path_hash(&parent, &algo)?;
    let child_name = file_name(child_path).unwrap_or("").to_string();

    let existing = self.engine.get_entry(&dir_key)?;

    let (dir_value, content_key) = match existing {
      Some((_header, _key, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
        // B-tree format: delete from tree
        let root_node = crate::engine::btree::BTreeNode::deserialize(&value, hash_length)?;
        let root_hash = root_node.content_hash(hash_length, &algo)?;

        match crate::engine::btree::btree_delete(self.engine, &root_hash, &child_name, hash_length, &algo)? {
          Some(new_root_hash) => {
            let new_root_entry = self.engine.get_entry(&new_root_hash)?
              .ok_or_else(|| EngineError::NotFound("B-tree root not found after delete".to_string()))?;
            (new_root_entry.2, new_root_hash)
          }
          None => {
            // Tree is empty -- store empty flat directory
            let dir_value = Vec::new();
            let content_key = directory_content_hash(&dir_value, &algo)?;
            self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
            (dir_value, content_key)
          }
        }
      }
      Some((header, _key, value)) => {
        // Flat format
        let mut children = if value.is_empty() {
          Vec::new()
        } else {
          deserialize_child_entries(&value, hash_length, header.entry_version)?
        };

        children.retain(|c| c.name != child_name);

        let dir_value = serialize_child_entries(&children, hash_length)?;
        let content_key = directory_content_hash(&dir_value, &algo)?;
        self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
        (dir_value, content_key)
      }
      None => {
        let dir_value = Vec::new();
        let content_key = directory_content_hash(&dir_value, &algo)?;
        self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
        (dir_value, content_key)
      }
    };

    // Store at path-based key
    self.engine.store_entry(EntryType::DirectoryIndex, &dir_key, &dir_value)?;

    // Propagate up
    if parent == "/" {
      self.engine.update_head(&content_key)?;
      return Ok(());
    }

    let del_now = chrono::Utc::now().timestamp_millis();
    let parent_child = ChildEntry {
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: content_key,  // content hash for tree walker
      total_size: dir_value.len() as u64,
      created_at: del_now,
      updated_at: del_now,
      name: file_name(&parent).unwrap_or("").to_string(),
      content_type: None,
      virtual_time: del_now as u64,
      node_id: 0,
    };

    self.update_parent_directories(&parent, parent_child)
  }

  /// Store a symlink at the given path pointing to the target path.
  /// If a symlink already exists at the path, updates its target (preserving created_at).
  /// Does NOT validate that the target exists.
  pub fn store_symlink(
    &self,
    ctx: &RequestContext,
    path: &str,
    target: &str,
  ) -> EngineResult<SymlinkRecord> {
    // SECURITY: Reject control characters in both path and target BEFORE
    // normalization. JSON deserializes \r\n into actual CR+LF bytes (0x0D, 0x0A)
    // which normalize_path does NOT strip. This prevents CRLF injection and
    // other control character attacks in symlink paths and targets.
    if path.bytes().any(|b| (b < 0x20 && b != 0) || b == 0x7F) {
      return Err(EngineError::InvalidInput(
        "Symlink path contains control characters".to_string()
      ));
    }
    if target.bytes().any(|b| (b < 0x20 && b != 0) || b == 0x7F) {
      return Err(EngineError::InvalidInput(
        "Symlink target contains control characters".to_string()
      ));
    }

    let normalized = normalize_path(path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let normalized_target = normalize_path(target);

    // M15: Reject storing at root path — it would create a ghost entry.
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }

    // M16: Reject self-referencing symlinks at creation time.
    if normalized == normalized_target {
      return Err(EngineError::InvalidInput(
        format!("Symlink cannot point to itself: {}", normalized)
      ));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = if is_system_path(&normalized) { FLAG_SYSTEM } else { 0 };

    // Check if symlink already exists (preserve created_at on update)
    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    let existing_created_at = match self.engine.get_entry(&symlink_key)? {
      Some((header, _key, value)) => {
        let existing = SymlinkRecord::deserialize(&value, header.entry_version)?;
        Some(existing.created_at)
      }
      None => None,
    };

    let mut record = SymlinkRecord::new(normalized.clone(), normalized_target);

    // Preserve original created_at on update
    if let Some(original_created_at) = existing_created_at {
      record.created_at = original_created_at;
    }

    let serialized = record.serialize()?;

    // Content-addressed key (immutable — for KV store entry)
    let content_key = symlink_content_hash(&serialized, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &content_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &content_key, &serialized)?;
    }

    // Identity hash (for ChildEntry.hash — excludes timestamps)
    let identity_key = symlink_identity_hash(&normalized, &record.target, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &identity_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &identity_key, &serialized)?;
    }

    // Path-based key (mutable — for reads/deletion)
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &symlink_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &symlink_key, &serialized)?;
    }

    // Build child entry for parent directory
    let child = ChildEntry {
      entry_type: EntryType::Symlink.to_u8(),
      hash: identity_key,
      total_size: 0,
      created_at: record.created_at,
      updated_at: record.updated_at,
      name: file_name(&normalized).unwrap_or("").to_string(),
      content_type: None,
      virtual_time: chrono::Utc::now().timestamp_millis() as u64,
      node_id: 0,
    };

    self.update_parent_directories(&normalized, child)?;

    // Update counters: only increment for new symlinks, not updates
    if existing_created_at.is_none() {
      self.engine.counters().increment_symlinks();
    }

    // Emit event
    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&record.target),
      created_at: record.created_at,
      updated_at: record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [entry_data]}));

    Ok(record)
  }

  /// Read a SymlinkRecord at the given path, or None if not found.
  pub fn get_symlink(&self, path: &str) -> EngineResult<Option<SymlinkRecord>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&symlink_key)? {
      Some((header, _key, value)) => {
        let record = SymlinkRecord::deserialize(&value, header.entry_version)?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// Delete a symlink at the given path.
  pub fn delete_symlink(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let sys_flags = if is_system_path(&normalized) { FLAG_SYSTEM } else { 0 };

    // Verify symlink exists
    let symlink_key = symlink_path_hash(&normalized, &algo)?;
    let record = match self.engine.get_entry(&symlink_key)? {
      Some((header, _key, value)) => SymlinkRecord::deserialize(&value, header.entry_version)?,
      None => return Err(EngineError::NotFound(normalized)),
    };

    // Store a DeletionRecord
    let deletion = DeletionRecord::new(normalized.clone(), None);
    let deletion_key = deletion_record_hash(&normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }

    // Mark as deleted in KV store
    self.engine.mark_entry_deleted(&symlink_key)?;

    // Remove from parent directory
    self.remove_from_parent_directory(&normalized)?;

    // Update counters
    self.engine.counters().decrement_symlinks();

    // Emit deletion event
    let entry_data = EntryEventData {
      path: normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&record.target),
      created_at: record.created_at,
      updated_at: record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [entry_data]}));

    Ok(())
  }

  /// Rename (move) a file from one path to another.
  ///
  /// This is a metadata-only operation — no chunk data is copied.
  /// The file's content (chunk_hashes), content_type, total_size, and
  /// created_at are preserved. Only the path and updated_at change.
  pub fn rename_file(
    &self,
    ctx: &RequestContext,
    old_path: &str,
    new_path: &str,
  ) -> EngineResult<FileRecord> {
    let old_normalized = normalize_path(old_path);
    let new_normalized = normalize_path(new_path);

    // Reject root paths
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot rename root path".to_string()));
    }

    // Reject same source/destination
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput(
        "Source and destination paths are the same".to_string(),
      ));
    }

    // Reject cross-system-boundary renames
    let old_is_system = is_system_path(&old_normalized);
    let new_is_system = is_system_path(&new_normalized);
    if old_is_system != new_is_system {
      return Err(EngineError::InvalidInput(
        "Cannot rename across system boundary".to_string(),
      ));
    }

    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();
    let sys_flags = if is_system_path(&new_normalized) { FLAG_SYSTEM } else { 0 };

    // Read the source FileRecord
    let old_file_key = file_path_hash(&old_normalized, &algo)?;
    let old_record = match self.engine.get_entry(&old_file_key)? {
      Some((header, _key, value)) => {
        FileRecord::deserialize(&value, hash_length, header.entry_version)?
      }
      None => return Err(EngineError::NotFound(old_normalized)),
    };

    // Check destination doesn't already exist (file or symlink)
    let new_file_key = file_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_file_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }
    let new_symlink_key = symlink_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_symlink_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }

    // Create a new FileRecord at the new path, preserving content fields
    let mut new_record = FileRecord::new(
      new_normalized.clone(),
      old_record.content_type.clone(),
      old_record.total_size,
      old_record.chunk_hashes.clone(),
    );
    new_record.created_at = old_record.created_at;

    let new_value = new_record.serialize(hash_length)?;

    // Store at content-addressed key
    let content_key = file_content_hash(&new_value, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &content_key, &new_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &content_key, &new_value)?;
    }

    // Store at identity hash
    let identity_key = file_identity_hash(
      &new_normalized,
      new_record.content_type.as_deref(),
      &new_record.chunk_hashes,
      &algo,
    )?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &identity_key, &new_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &identity_key, &new_value)?;
    }

    // Store at path-based key
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::FileRecord, &new_file_key, &new_value, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::FileRecord, &new_file_key, &new_value)?;
    }

    // Build child entry and update parent directories for the new path
    let now_vt = chrono::Utc::now().timestamp_millis() as u64;
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: identity_key,
      total_size: new_record.total_size,
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      name: file_name(&new_normalized).unwrap_or("").to_string(),
      content_type: new_record.content_type.clone(),
      virtual_time: now_vt,
      node_id: 0,
    };
    self.update_parent_directories(&new_normalized, child)?;

    // Delete old path: DeletionRecord + mark deleted + remove from parent
    let deletion = DeletionRecord::new(old_normalized.clone(), None);
    let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    let old_sys_flags = if is_system_path(&old_normalized) { FLAG_SYSTEM } else { 0 };
    if old_sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, old_sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }
    self.engine.mark_entry_deleted(&old_file_key)?;
    self.remove_from_parent_directory(&old_normalized)?;

    // Emit events: deleted from old path, created at new path
    let deleted_event = EntryEventData {
      path: old_normalized,
      entry_type: "file".to_string(),
      content_type: old_record.content_type,
      size: old_record.total_size,
      hash: hex::encode(old_record.chunk_hashes.first().unwrap_or(&vec![])),
      created_at: old_record.created_at,
      updated_at: old_record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [deleted_event]}));

    let created_event = EntryEventData {
      path: new_normalized,
      entry_type: "file".to_string(),
      content_type: new_record.content_type.clone(),
      size: new_record.total_size,
      hash: hex::encode(new_record.chunk_hashes.first().unwrap_or(&vec![])),
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [created_event]}));

    Ok(new_record)
  }

  /// Copy a file to a new path. Reuses existing chunk hashes (no data duplication).
  pub fn copy_file(
    &self,
    ctx: &RequestContext,
    from_path: &str,
    to_path: &str,
  ) -> EngineResult<FileRecord> {
    let from_normalized = normalize_path(from_path);
    let to_normalized = normalize_path(to_path);

    if from_normalized == "/" || to_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot copy root path".to_string()));
    }
    if from_normalized == to_normalized {
      return Err(EngineError::InvalidInput("Source and destination are the same".to_string()));
    }
    if is_system_path(&from_normalized) || is_system_path(&to_normalized) {
      return Err(EngineError::InvalidInput("Cannot copy system paths".to_string()));
    }

    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    // Read the source FileRecord
    let from_key = file_path_hash(&from_normalized, &algo)?;
    let source_record = match self.engine.get_entry(&from_key)? {
      Some((header, _key, value)) => FileRecord::deserialize(&value, hash_length, header.entry_version)?,
      None => return Err(EngineError::NotFound(from_normalized)),
    };

    // Use restore_file_from_record which handles all 3 keys + parent dirs
    self.restore_file_from_record(ctx, &to_normalized, &source_record)?;

    // Read back the new record
    let to_key = file_path_hash(&to_normalized, &algo)?;
    match self.engine.get_entry(&to_key)? {
      Some((header, _key, value)) => Ok(FileRecord::deserialize(&value, hash_length, header.entry_version)?),
      None => Err(EngineError::NotFound(to_normalized)),
    }
  }

  /// Recursively copy a path (file or directory) to a new location.
  pub fn copy_path(
    &self,
    ctx: &RequestContext,
    from_path: &str,
    to_path: &str,
  ) -> EngineResult<Vec<String>> {
    let from_normalized = normalize_path(from_path);
    let to_normalized = normalize_path(to_path);
    let mut copied = Vec::new();

    // Check if source is a directory
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash(&from_normalized, &algo)?;
    if self.engine.has_entry(&dir_key)? {
      // Directory — create destination dir and recurse
      let _ = self.create_directory(ctx, &to_normalized);
      let children = self.list_directory(&from_normalized)?;
      for child in &children {
        let child_from = format!("{}/{}", from_normalized.trim_end_matches('/'), child.name);
        let child_to = format!("{}/{}", to_normalized.trim_end_matches('/'), child.name);
        let sub_copied = self.copy_path(ctx, &child_from, &child_to)?;
        copied.extend(sub_copied);
      }
      return Ok(copied);
    }

    // File
    self.copy_file(ctx, &from_normalized, &to_normalized)?;
    copied.push(to_normalized);
    Ok(copied)
  }

  /// Rename (move) a symlink from one path to another.
  ///
  /// This is a metadata-only operation — the symlink's target does NOT change,
  /// only its path. created_at is preserved.
  pub fn rename_symlink(
    &self,
    ctx: &RequestContext,
    old_path: &str,
    new_path: &str,
  ) -> EngineResult<SymlinkRecord> {
    let old_normalized = normalize_path(old_path);
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
    let new_normalized = normalize_path(new_path);

    // Reject root paths
    if old_normalized == "/" || new_normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot rename root path".to_string()));
    }

    // Reject same source/destination
    if old_normalized == new_normalized {
      return Err(EngineError::InvalidInput(
        "Source and destination paths are the same".to_string(),
      ));
    }

    // Reject cross-system-boundary renames
    let old_is_system = is_system_path(&old_normalized);
    let new_is_system = is_system_path(&new_normalized);
    if old_is_system != new_is_system {
      return Err(EngineError::InvalidInput(
        "Cannot rename across system boundary".to_string(),
      ));
    }

    let algo = self.engine.hash_algo();
    let sys_flags = if is_system_path(&new_normalized) { FLAG_SYSTEM } else { 0 };

    // Read the source SymlinkRecord
    let old_symlink_key = symlink_path_hash(&old_normalized, &algo)?;
    let old_record = match self.engine.get_entry(&old_symlink_key)? {
      Some((header, _key, value)) => SymlinkRecord::deserialize(&value, header.entry_version)?,
      None => return Err(EngineError::NotFound(old_normalized)),
    };

    // Check destination doesn't already exist (file or symlink)
    let new_file_key = file_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_file_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }
    let new_symlink_key = symlink_path_hash(&new_normalized, &algo)?;
    if self.engine.has_entry(&new_symlink_key)? {
      return Err(EngineError::AlreadyExists(new_normalized));
    }

    // Create new SymlinkRecord at new path with same target, preserving created_at
    let mut new_record = SymlinkRecord::new(new_normalized.clone(), old_record.target.clone());
    new_record.created_at = old_record.created_at;

    let serialized = new_record.serialize()?;

    // Store at content-addressed key
    let content_key = symlink_content_hash(&serialized, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &content_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &content_key, &serialized)?;
    }

    // Store at identity hash
    let identity_key = symlink_identity_hash(&new_normalized, &new_record.target, &algo)?;
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &identity_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &identity_key, &serialized)?;
    }

    // Store at path-based key
    if sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::Symlink, &new_symlink_key, &serialized, sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::Symlink, &new_symlink_key, &serialized)?;
    }

    // Build child entry and update parent directories
    let child = ChildEntry {
      entry_type: EntryType::Symlink.to_u8(),
      hash: identity_key,
      total_size: 0,
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      name: file_name(&new_normalized).unwrap_or("").to_string(),
      content_type: None,
      virtual_time: chrono::Utc::now().timestamp_millis() as u64,
      node_id: 0,
    };
    self.update_parent_directories(&new_normalized, child)?;

    // Delete old path: DeletionRecord + mark deleted + remove from parent
    let deletion = DeletionRecord::new(old_normalized.clone(), None);
    let deletion_key = deletion_record_hash(&old_normalized, deletion.deleted_at, &algo)?;
    let deletion_value = deletion.serialize();
    let old_sys_flags = if is_system_path(&old_normalized) { FLAG_SYSTEM } else { 0 };
    if old_sys_flags != 0 {
      self.engine.store_entry_with_flags(EntryType::DeletionRecord, &deletion_key, &deletion_value, old_sys_flags)?;
    } else {
      self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;
    }
    self.engine.mark_entry_deleted(&old_symlink_key)?;
    self.remove_from_parent_directory(&old_normalized)?;

    // Emit events
    let deleted_event = EntryEventData {
      path: old_normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&old_record.target),
      created_at: old_record.created_at,
      updated_at: old_record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_DELETED, serde_json::json!({"entries": [deleted_event]}));

    let created_event = EntryEventData {
      path: new_normalized,
      entry_type: "symlink".to_string(),
      content_type: None,
      size: 0,
      hash: hex::encode(&new_record.target),
      created_at: new_record.created_at,
      updated_at: new_record.updated_at,
      previous_hash: None,
    };
    ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({"entries": [created_event]}));

    Ok(new_record)
  }
}
