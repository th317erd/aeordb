use crate::engine::compression::{CompressionAlgorithm, compress, decompress, should_compress};
use crate::engine::deletion_record::DeletionRecord;
use crate::engine::directory_entry::{
  ChildEntry, deserialize_child_entries, serialize_child_entries,
};
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
use crate::engine::storage_engine::StorageEngine;

/// Default chunk size for splitting file data (256 KB).
pub const DEFAULT_CHUNK_SIZE: usize = 262_144;

/// Compute the domain-prefixed hash for a file path.
pub fn file_path_hash(path: &str, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  algo.compute_hash(format!("file:{}", path).as_bytes())
}

/// Check if a path targets a system directory that should not trigger indexing.
/// Returns true for paths containing .logs/, .indexes/, or .config/ segments.
fn is_system_path(path: &str) -> bool {
  let normalized = normalize_path(path);
  let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
  segments.iter().any(|s| *s == ".logs" || *s == ".indexes" || *s == ".config")
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
    Self::new(chunk_hashes, engine)
  }

  fn new(chunk_hashes: Vec<Vec<u8>>, engine: &StorageEngine) -> EngineResult<Self> {
    let mut chunks = Vec::with_capacity(chunk_hashes.len());

    for hash in &chunk_hashes {
      match engine.get_entry(hash) {
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
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

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
            self.engine.store_entry_compressed(
              EntryType::Chunk,
              &chunk_key,
              &compressed_data,
              compression_algo,
            )?;
          } else {
            self.engine.store_entry(
              EntryType::Chunk,
              &chunk_key,
              chunk_data,
            )?;
          }
        }

        chunk_hashes.push(chunk_key);
        offset = end;
      }
    }

    // Check if file already exists (for preserving created_at on overwrite)
    let file_key = file_path_hash(&normalized, &algo)?;
    let existing_created_at = match self.engine.get_entry(&file_key)? {
      Some((header, _key, value)) => {
        let existing = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
        Some(existing.created_at)
      }
      None => None,
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

    let file_value = file_record.serialize(hash_length);

    // Content-addressed key (immutable — for KV store entry)
    let file_content_key = file_content_hash(&file_value, &algo)?;
    self.engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;

    // Identity hash (for ChildEntry.hash — excludes timestamps)
    let identity_key = file_identity_hash(&normalized, Some(detected_content_type.as_str()), &file_record.chunk_hashes, &algo)?;
    // Store at identity key so tree walker can look up entries by ChildEntry.hash
    self.engine.store_entry(EntryType::FileRecord, &identity_key, &file_value)?;

    // Path-based key (mutable — for reads, indexing, deletion)
    self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

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

  /// Read a file as a streaming iterator of chunk data.
  pub fn read_file_streaming(&self, path: &str) -> EngineResult<EngineFileStream> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    let entry = self.engine.get_entry(&file_key)?
      .ok_or_else(|| EngineError::NotFound(normalized.clone()))?;

    let (header, _key, value) = entry;
    let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;

    EngineFileStream::new(file_record.chunk_hashes, self.engine)
  }

  /// Read a file's full content into memory.
  pub fn read_file(&self, path: &str) -> EngineResult<Vec<u8>> {
    self.read_file_streaming(path)?.collect_to_vec()
  }

  /// Delete a file, storing a DeletionRecord and updating parent directories.
  pub fn delete_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

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
    self.engine.store_entry(
      EntryType::DeletionRecord,
      &deletion_key,
      &deletion_value,
    )?;

    // Mark the FileRecord as deleted in the KV store
    self.engine.mark_entry_deleted(&file_key)?;

    // Remove child from parent directory
    self.remove_from_parent_directory(&normalized)?;

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

  /// List the children of a directory.
  pub fn list_directory(&self, path: &str) -> EngineResult<Vec<ChildEntry>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let dir_key = directory_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&dir_key)? {
      Some((header, _key, value)) => {
        if value.is_empty() {
          return Ok(Vec::new());
        }
        if crate::engine::btree::is_btree_format(&value) {
          // B-tree format: value is the root node data
          crate::engine::btree::btree_list_from_node(&value, self.engine, hash_length)
        } else {
          // Flat format
          deserialize_child_entries(&value, hash_length, header.entry_version)
        }
      }
      None => Err(EngineError::NotFound(normalized)),
    }
  }

  /// Create an empty directory at the given path.
  pub fn create_directory(&self, ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
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

  /// Ensure the root directory exists. Called during database creation.
  pub fn ensure_root_directory(&self, _ctx: &RequestContext) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let dir_key = directory_path_hash("/", &algo)?;

    if self.engine.has_entry(&dir_key)? {
      return Ok(());
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
      format!("{}/.config/indexes.json", parent)
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
    if is_system_path(path) {
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

    if is_system_path(path) {
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

    // Now delete the file itself
    self.delete_file(ctx, path)
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

    for _depth in 0..Self::MAX_DIRECTORY_DEPTH {
      let parent = match parent_path(&current_child_path) {
        Some(parent) => parent,
        None => return Ok(()), // root has no parent
      };

      let dir_key = directory_path_hash(&parent, &algo)?;

      // Read existing directory
      let existing = self.engine.get_entry(&dir_key)?;

      let (dir_value, content_key) = match existing {
        Some((_header, _key, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
          // === B-TREE FORMAT ===
          let (new_root_hash, new_root_data) = crate::engine::btree::btree_insert_batched(
            self.engine, &value, current_child_entry, hash_length, &algo
          )?;

          (new_root_data, new_root_hash)
        }
        Some((header, _key, value)) => {
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
            // Convert flat -> B-tree
            let root_hash = crate::engine::btree::btree_from_entries(
              self.engine, children, hash_length, &algo
            )?;
            let root_entry = self.engine.get_entry(&root_hash)?
              .ok_or_else(|| EngineError::NotFound("B-tree root not found after conversion".to_string()))?;
            (root_entry.2, root_hash)
          } else {
            // Stay flat
            let dir_value = serialize_child_entries(&children, hash_length);
            let content_key = directory_content_hash(&dir_value, &algo)?;
            self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
            (dir_value, content_key)
          }
        }
        None => {
          // New directory
          let children = vec![current_child_entry];
          let dir_value = serialize_child_entries(&children, hash_length);
          let content_key = directory_content_hash(&dir_value, &algo)?;
          self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
          (dir_value, content_key)
        }
      };

      // Store at path-based key
      self.engine.store_entry(EntryType::DirectoryIndex, &dir_key, &dir_value)?;

      // If this is root "/", update HEAD to content hash and we're done
      if parent == "/" {
        self.engine.update_head(&content_key)?;
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

        let dir_value = serialize_child_entries(&children, hash_length);
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
    let normalized = normalize_path(path);
    let normalized_target = normalize_path(target);
    let algo = self.engine.hash_algo();

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

    let serialized = record.serialize();

    // Content-addressed key (immutable — for KV store entry)
    let content_key = symlink_content_hash(&serialized, &algo)?;
    self.engine.store_entry(EntryType::Symlink, &content_key, &serialized)?;

    // Identity hash (for ChildEntry.hash — excludes timestamps)
    let identity_key = symlink_identity_hash(&normalized, &record.target, &algo)?;
    // Store at identity key so tree walker can look up entries by ChildEntry.hash
    self.engine.store_entry(EntryType::Symlink, &identity_key, &serialized)?;

    // Path-based key (mutable — for reads/deletion)
    self.engine.store_entry(EntryType::Symlink, &symlink_key, &serialized)?;

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
    self.engine.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_value)?;

    // Mark as deleted in KV store
    self.engine.mark_entry_deleted(&symlink_key)?;

    // Remove from parent directory
    self.remove_from_parent_directory(&normalized)?;

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
}
