use crate::engine::compression::{CompressionAlgorithm, compress, decompress, should_compress};
use crate::engine::deletion_record::DeletionRecord;
use crate::engine::directory_entry::{
  ChildEntry, deserialize_child_entries, serialize_child_entries,
};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::index_store::IndexManager;
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::storage_engine::StorageEngine;

/// Default chunk size for splitting file data (256 KB).
const DEFAULT_CHUNK_SIZE: usize = 262_144;

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

/// Compute the domain-prefixed hash for a chunk.
fn chunk_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
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

/// An iterator that yields chunk data by reading chunk hashes from the engine.
pub struct EngineFileStream {
  chunk_hashes: Vec<Vec<u8>>,
  current_index: usize,
  engine: *const StorageEngine,
}

// SAFETY: StorageEngine uses RwLock internally and is thread-safe.
// We store a raw pointer only because we can't store a reference with
// a lifetime that satisfies Iterator. The caller must ensure the engine
// outlives this stream.
unsafe impl Send for EngineFileStream {}
unsafe impl Sync for EngineFileStream {}

impl EngineFileStream {
  fn new(chunk_hashes: Vec<Vec<u8>>, engine: &StorageEngine) -> Self {
    EngineFileStream {
      chunk_hashes,
      current_index: 0,
      engine: engine as *const StorageEngine,
    }
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
    if self.current_index >= self.chunk_hashes.len() {
      return None;
    }

    let hash = &self.chunk_hashes[self.current_index];
    self.current_index += 1;

    // SAFETY: caller ensures engine outlives this stream
    let engine = unsafe { &*self.engine };

    match engine.get_entry(hash) {
      Ok(Some((header, _key, value))) => {
        // Decompress if the chunk was stored compressed
        if header.compression_algo != CompressionAlgorithm::None {
          match decompress(&value, header.compression_algo) {
            Ok(decompressed) => Some(Ok(decompressed)),
            Err(error) => Some(Err(error)),
          }
        } else {
          Some(Ok(value))
        }
      }
      Ok(None) => Some(Err(EngineError::NotFound(
        format!("Chunk not found: {}", hex::encode(hash)),
      ))),
      Err(error) => Some(Err(error)),
    }
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
  pub fn new(engine: &'a StorageEngine) -> Self {
    DirectoryOps { engine }
  }

  /// Store a file at the given path, splitting data into chunks.
  /// Creates intermediate directories as needed and updates HEAD.
  pub fn store_file(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> EngineResult<FileRecord> {
    self.store_file_internal(path, data, content_type, CompressionAlgorithm::None)
  }

  /// Store a file with compression at the given path, splitting data into chunks.
  /// Creates intermediate directories as needed and updates HEAD.
  /// Chunks are compressed individually using the specified algorithm.
  pub fn store_file_compressed(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    self.store_file_internal(path, data, content_type, compression_algo)
  }

  /// Internal file storage with optional compression.
  fn store_file_internal(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<FileRecord> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
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
      Some((_header, _key, value)) => {
        let existing = FileRecord::deserialize(&value, hash_length)?;
        Some(existing.created_at)
      }
      None => None,
    };

    // Create the FileRecord
    let mut file_record = FileRecord::new(
      normalized.clone(),
      content_type.map(|s| s.to_string()),
      data.len() as u64,
      chunk_hashes,
    );

    // Preserve original created_at on overwrite
    if let Some(original_created_at) = existing_created_at {
      file_record.created_at = original_created_at;
    }

    let file_value = file_record.serialize(hash_length);
    self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

    // Build child entry for directory update
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: file_key.clone(),
      total_size: data.len() as u64,
      created_at: file_record.created_at,
      updated_at: file_record.updated_at,
      name: file_name(&normalized).unwrap_or("").to_string(),
      content_type: content_type.map(|s| s.to_string()),
    };

    self.update_parent_directories(&normalized, child)?;

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

    let (_header, _key, value) = entry;
    let file_record = FileRecord::deserialize(&value, hash_length)?;

    Ok(EngineFileStream::new(file_record.chunk_hashes, self.engine))
  }

  /// Read a file's full content into memory.
  pub fn read_file(&self, path: &str) -> EngineResult<Vec<u8>> {
    self.read_file_streaming(path)?.collect_to_vec()
  }

  /// Delete a file, storing a DeletionRecord and updating parent directories.
  pub fn delete_file(&self, path: &str) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();

    // Verify the file exists
    let file_key = file_path_hash(&normalized, &algo)?;
    if !self.engine.has_entry(&file_key)? {
      return Err(EngineError::NotFound(normalized));
    }

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

    Ok(())
  }

  /// List the children of a directory.
  pub fn list_directory(&self, path: &str) -> EngineResult<Vec<ChildEntry>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let dir_key = directory_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&dir_key)? {
      Some((_header, _key, value)) => {
        if value.is_empty() {
          return Ok(Vec::new());
        }
        deserialize_child_entries(&value, hash_length)
      }
      None => Err(EngineError::NotFound(normalized)),
    }
  }

  /// Create an empty directory at the given path.
  pub fn create_directory(&self, path: &str) -> EngineResult<()> {
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
    if normalized != "/" {
      let child = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,  // content hash for tree walker
        total_size: 0,
        created_at: chrono::Utc::now().timestamp_millis(),
        updated_at: chrono::Utc::now().timestamp_millis(),
        name: file_name(&normalized).unwrap_or("").to_string(),
        content_type: None,
      };
      self.update_parent_directories(&normalized, child)?;
    }

    Ok(())
  }

  /// Get the FileRecord metadata for a file path.
  pub fn get_metadata(&self, path: &str) -> EngineResult<Option<FileRecord>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let file_key = file_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&file_key)? {
      Some((_header, _key, value)) => {
        let record = FileRecord::deserialize(&value, hash_length)?;
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
  pub fn ensure_root_directory(&self) -> EngineResult<()> {
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

  /// Store a file with automatic index updates and optional compression.
  /// After storing the file, checks for index config at `.config/indexes.json`
  /// under the parent path and updates relevant indexes.
  /// Compression is determined by config or auto-detection via `should_compress`.
  pub fn store_file_with_indexing(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> EngineResult<FileRecord> {
    let normalized_for_config = normalize_path(path);
    let parent_for_config = parent_path(&normalized_for_config).unwrap_or_else(|| "/".to_string());

    // Check for compression configuration
    let config_path_for_compression = if parent_for_config.ends_with('/') {
      format!("{}.config/indexes.json", parent_for_config)
    } else {
      format!("{}/.config/indexes.json", parent_for_config)
    };

    let compression_algo = match self.read_file(&config_path_for_compression) {
      Ok(config_data) => {
        match PathIndexConfig::deserialize_with_compression(&config_data) {
          Ok(Some(algo_str)) if algo_str == "zstd" => {
            if should_compress(content_type, data.len()) {
              CompressionAlgorithm::Zstd
            } else {
              CompressionAlgorithm::None
            }
          }
          _ => CompressionAlgorithm::None,
        }
      }
      Err(EngineError::NotFound(_)) => CompressionAlgorithm::None,
      Err(_) => CompressionAlgorithm::None,
    };

    let file_record = self.store_file_internal(path, data, content_type, compression_algo)?;

    // Guard: skip indexing for system directories
    if is_system_path(path) {
      return Ok(file_record);
    }

    // Delegate to indexing pipeline
    let pipeline = crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine);
    let _ = pipeline.run(path, data, content_type);

    Ok(file_record)
  }

  /// Store a file with the full indexing pipeline including parser plugin support.
  pub fn store_file_with_full_pipeline(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    plugin_manager: Option<&crate::plugins::PluginManager>,
  ) -> EngineResult<FileRecord> {
    // Compression detection (same as store_file_with_indexing)
    let normalized_for_config = normalize_path(path);
    let parent_for_config = parent_path(&normalized_for_config).unwrap_or_else(|| "/".to_string());
    let config_path_for_compression = if parent_for_config.ends_with('/') {
      format!("{}.config/indexes.json", parent_for_config)
    } else {
      format!("{}/.config/indexes.json", parent_for_config)
    };
    let compression_algo = match self.read_file(&config_path_for_compression) {
      Ok(config_data) => {
        match PathIndexConfig::deserialize_with_compression(&config_data) {
          Ok(Some(algo_str)) if algo_str == "zstd" => {
            if should_compress(content_type, data.len()) {
              CompressionAlgorithm::Zstd
            } else {
              CompressionAlgorithm::None
            }
          }
          _ => CompressionAlgorithm::None,
        }
      }
      Err(_) => CompressionAlgorithm::None,
    };

    let file_record = self.store_file_internal(path, data, content_type, compression_algo)?;

    if is_system_path(path) {
      return Ok(file_record);
    }

    // Use full pipeline with plugin manager
    let pipeline = match plugin_manager {
      Some(pm) => crate::engine::indexing_pipeline::IndexingPipeline::with_plugin_manager(self.engine, pm),
      None => crate::engine::indexing_pipeline::IndexingPipeline::new(self.engine),
    };
    let _ = pipeline.run(path, data, content_type);

    Ok(file_record)
  }

  /// Delete a file and remove its entries from all indexes at that path.
  pub fn delete_file_with_indexing(&self, path: &str) -> EngineResult<()> {
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
    self.delete_file(path)
  }

  /// Update parent directories after a child is added or modified.
  /// Propagates from the immediate parent up to root, updating HEAD at the end.
  fn update_parent_directories(
    &self,
    child_path: &str,
    child_entry: ChildEntry,
  ) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let parent = match parent_path(child_path) {
      Some(parent) => parent,
      None => return Ok(()), // root has no parent
    };

    let dir_key = directory_path_hash(&parent, &algo)?;

    // Read existing directory or create empty
    let mut children = match self.engine.get_entry(&dir_key)? {
      Some((_header, _key, value)) => {
        if value.is_empty() {
          Vec::new()
        } else {
          deserialize_child_entries(&value, hash_length)?
        }
      }
      None => Vec::new(),
    };

    // Add or update the child entry
    let child_name = &child_entry.name;
    if let Some(existing) = children.iter_mut().find(|c| c.name == *child_name) {
      *existing = child_entry;
    } else {
      children.push(child_entry);
    }

    // Serialize and store the updated directory at path-based key
    let dir_value = serialize_child_entries(&children, hash_length);
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &dir_key,
      &dir_value,
    )?;

    // Also store at content-addressed key for immutable versioning
    let content_key = directory_content_hash(&dir_value, &algo)?;
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &content_key,
      &dir_value,
    )?;

    // If this is root "/", update HEAD to content hash
    if parent == "/" {
      self.engine.update_head(&content_key)?;
      return Ok(());
    }

    // Recurse: update grandparent with this directory as child (using content hash)
    let parent_child = ChildEntry {
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: content_key,  // content hash for tree walker
      total_size: dir_value.len() as u64,
      created_at: chrono::Utc::now().timestamp_millis(),
      updated_at: chrono::Utc::now().timestamp_millis(),
      name: file_name(&parent).unwrap_or("").to_string(),
      content_type: None,
    };

    self.update_parent_directories(&parent, parent_child)
  }

  /// Remove a child entry from its parent directory and propagate up.
  fn remove_from_parent_directory(&self, child_path: &str) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let parent = match parent_path(child_path) {
      Some(parent) => parent,
      None => return Ok(()),
    };

    let dir_key = directory_path_hash(&parent, &algo)?;
    let child_name = file_name(child_path).unwrap_or("").to_string();

    let mut children = match self.engine.get_entry(&dir_key)? {
      Some((_header, _key, value)) => {
        if value.is_empty() {
          Vec::new()
        } else {
          deserialize_child_entries(&value, hash_length)?
        }
      }
      None => Vec::new(),
    };

    // Remove the child
    children.retain(|c| c.name != child_name);

    // Re-store directory at path-based key
    let dir_value = serialize_child_entries(&children, hash_length);
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &dir_key,
      &dir_value,
    )?;

    // Also store at content-addressed key for immutable versioning
    let content_key = directory_content_hash(&dir_value, &algo)?;
    self.engine.store_entry(
      EntryType::DirectoryIndex,
      &content_key,
      &dir_value,
    )?;

    // Propagate up
    if parent == "/" {
      self.engine.update_head(&content_key)?;
      return Ok(());
    }

    let parent_child = ChildEntry {
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: content_key,  // content hash for tree walker
      total_size: dir_value.len() as u64,
      created_at: chrono::Utc::now().timestamp_millis(),
      updated_at: chrono::Utc::now().timestamp_millis(),
      name: file_name(&parent).unwrap_or("").to_string(),
      content_type: None,
    };

    self.update_parent_directories(&parent, parent_child)
  }
}
