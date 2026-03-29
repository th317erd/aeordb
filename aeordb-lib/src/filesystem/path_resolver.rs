use crate::filesystem::directory_entry::{DirectoryEntry, EntryType};
use crate::filesystem::redb_directory::RedbDirectory;
use crate::storage::{ChunkHash, ChunkStorage, ChunkStore};
use redb::Database;
use std::fmt;
use std::sync::Arc;

#[derive(Debug)]
pub enum PathError {
  NotFound(String),
  NotAFile(String),
  NotADirectory(String),
  InvalidPath(String),
  AlreadyExists(String),
  StorageError(String),
}

impl fmt::Display for PathError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      PathError::NotFound(path) => write!(formatter, "not found: {path}"),
      PathError::NotAFile(path) => write!(formatter, "not a file: {path}"),
      PathError::NotADirectory(path) => write!(formatter, "not a directory: {path}"),
      PathError::InvalidPath(path) => write!(formatter, "invalid path: {path}"),
      PathError::AlreadyExists(path) => write!(formatter, "already exists: {path}"),
      PathError::StorageError(message) => write!(formatter, "storage error: {message}"),
    }
  }
}

impl std::error::Error for PathError {}

impl From<crate::filesystem::redb_directory::DirectoryError> for PathError {
  fn from(error: crate::filesystem::redb_directory::DirectoryError) -> Self {
    PathError::StorageError(error.to_string())
  }
}

impl From<crate::storage::ChunkStoreError> for PathError {
  fn from(error: crate::storage::ChunkStoreError) -> Self {
    PathError::StorageError(error.to_string())
  }
}

pub struct PathResolver {
  directory: RedbDirectory,
  chunk_store: ChunkStore,
}

pub struct FileStream {
  chunk_hashes: Vec<ChunkHash>,
  current_index: usize,
  storage: Arc<dyn ChunkStorage>,
}

impl fmt::Debug for FileStream {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("FileStream")
      .field("chunk_count", &self.chunk_hashes.len())
      .field("current_index", &self.current_index)
      .finish()
  }
}

impl Iterator for FileStream {
  type Item = Result<Vec<u8>, PathError>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.current_index >= self.chunk_hashes.len() {
      return None;
    }

    let hash = &self.chunk_hashes[self.current_index];
    self.current_index += 1;

    let result = self
      .storage
      .get_chunk(hash)
      .map_err(|error| PathError::StorageError(error.to_string()))
      .and_then(|maybe_chunk| {
        maybe_chunk
          .map(|chunk| chunk.data)
          .ok_or_else(|| PathError::StorageError(format!(
            "chunk not found: {}",
            hex::encode(hash),
          )))
      });

    Some(result)
  }
}

impl FileStream {
  /// Collect all chunks into a single Vec<u8>. FOR TESTING ONLY.
  /// Do NOT use this in production -- use the iterator for streaming.
  pub fn collect_to_vec(self) -> Result<Vec<u8>, PathError> {
    let mut result = Vec::new();
    for chunk_result in self {
      result.extend(chunk_result?);
    }
    Ok(result)
  }
}

impl PathResolver {
  pub fn new(database: Arc<Database>, chunk_store: ChunkStore) -> Self {
    let directory = RedbDirectory::new(database);
    Self {
      directory,
      chunk_store,
    }
  }

  /// Parse a path string into segments. Handles leading/trailing slashes.
  /// "/myapp/users/abc123" -> ["myapp", "users", "abc123"]
  fn parse_path(path: &str) -> Vec<&str> {
    path
      .split('/')
      .filter(|segment| !segment.is_empty())
      .collect()
  }

  /// Build the directory table path from segments.
  /// ["myapp", "users"] -> "/myapp/users"
  /// [] -> "/"
  fn build_directory_path(segments: &[&str]) -> String {
    if segments.is_empty() {
      return "/".to_string();
    }
    format!("/{}", segments.join("/"))
  }

  /// Store a file at the given path. Creates intermediate directories (mkdir -p).
  /// Returns the DirectoryEntry that was created.
  pub fn store_file(
    &self,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> Result<DirectoryEntry, PathError> {
    let segments = Self::parse_path(path);
    if segments.is_empty() {
      return Err(PathError::InvalidPath(
        "path must have at least one segment".to_string(),
      ));
    }

    let file_name = segments[segments.len() - 1];
    let parent_segments = &segments[..segments.len() - 1];

    // Ensure root exists.
    self.ensure_root()?;

    // Create intermediate directories (mkdir -p).
    self.ensure_directories(parent_segments)?;

    let parent_path = Self::build_directory_path(parent_segments);

    // Check if something already exists at this path.
    if let Some(existing) = self.directory.get_entry(&parent_path, file_name)? {
      if existing.entry_type == EntryType::Directory {
        return Err(PathError::NotAFile(format!(
          "a directory already exists at '{path}'",
        )));
      }
    }

    // Store data as chunks.
    let content_hash_map = self.chunk_store.store(data)?;

    let entry = DirectoryEntry::new_file(
      file_name.to_string(),
      content_hash_map.chunk_hashes,
      content_type.map(|content_type| content_type.to_string()),
      data.len() as u64,
    );

    self.directory.insert_entry(&parent_path, &entry)?;

    Ok(entry)
  }

  /// Read a file at the given path. Returns a streaming iterator over chunks.
  /// NEVER loads the entire file into memory.
  pub fn read_file_streaming(
    &self,
    path: &str,
  ) -> Result<FileStream, PathError> {
    let segments = Self::parse_path(path);
    if segments.is_empty() {
      return Err(PathError::InvalidPath(
        "path must have at least one segment".to_string(),
      ));
    }

    let file_name = segments[segments.len() - 1];
    let parent_segments = &segments[..segments.len() - 1];
    let parent_path = Self::build_directory_path(parent_segments);

    let entry = self
      .directory
      .get_entry(&parent_path, file_name)?
      .ok_or_else(|| PathError::NotFound(path.to_string()))?;

    if entry.entry_type != EntryType::File {
      return Err(PathError::NotAFile(path.to_string()));
    }

    Ok(FileStream {
      chunk_hashes: entry.chunk_hashes,
      current_index: 0,
      storage: self.chunk_store.storage().clone(),
    })
  }

  /// Get the metadata for a file or directory at the given path, without reading content.
  pub fn get_metadata(
    &self,
    path: &str,
  ) -> Result<Option<DirectoryEntry>, PathError> {
    let segments = Self::parse_path(path);

    // Root path metadata.
    if segments.is_empty() {
      let exists = self.directory.directory_exists("/")?;
      if exists {
        return Ok(Some(DirectoryEntry::new_directory("/")));
      }
      return Ok(None);
    }

    let entry_name = segments[segments.len() - 1];
    let parent_segments = &segments[..segments.len() - 1];
    let parent_path = Self::build_directory_path(parent_segments);

    let entry = self.directory.get_entry(&parent_path, entry_name)?;
    Ok(entry)
  }

  /// Delete a file at the given path. Returns the removed entry.
  /// Does NOT delete the chunks -- garbage collection handles that.
  pub fn delete_file(
    &self,
    path: &str,
  ) -> Result<DirectoryEntry, PathError> {
    let segments = Self::parse_path(path);
    if segments.is_empty() {
      return Err(PathError::InvalidPath(
        "cannot delete root".to_string(),
      ));
    }

    let file_name = segments[segments.len() - 1];
    let parent_segments = &segments[..segments.len() - 1];
    let parent_path = Self::build_directory_path(parent_segments);

    let entry = self
      .directory
      .get_entry(&parent_path, file_name)?
      .ok_or_else(|| PathError::NotFound(path.to_string()))?;

    if entry.entry_type != EntryType::File {
      return Err(PathError::NotAFile(format!(
        "cannot delete_file on a directory: '{path}'",
      )));
    }

    self
      .directory
      .remove_entry(&parent_path, file_name)?
      .ok_or_else(|| PathError::NotFound(path.to_string()))
  }

  /// List entries in a directory at the given path.
  pub fn list_directory(
    &self,
    path: &str,
  ) -> Result<Vec<DirectoryEntry>, PathError> {
    let segments = Self::parse_path(path);
    let directory_path = Self::build_directory_path(&segments);

    // Verify the directory exists.
    if !segments.is_empty() {
      let entry_name = segments[segments.len() - 1];
      let parent_segments = &segments[..segments.len() - 1];
      let parent_path = Self::build_directory_path(parent_segments);

      let entry = self
        .directory
        .get_entry(&parent_path, entry_name)?;

      match entry {
        Some(entry) if entry.entry_type == EntryType::File => {
          return Err(PathError::NotADirectory(path.to_string()));
        }
        None => {
          // Check if the table exists even without a parent entry (e.g., root).
          if !self.directory.directory_exists(&directory_path)? {
            return Err(PathError::NotFound(path.to_string()));
          }
        }
        _ => {}
      }
    } else {
      // Root -- just check the table exists.
      if !self.directory.directory_exists("/")? {
        return Err(PathError::NotFound("/".to_string()));
      }
    }

    let entries = self.directory.list_entries(&directory_path)?;
    Ok(entries)
  }

  /// Create a directory at the given path. Creates intermediate directories.
  pub fn create_directory(
    &self,
    path: &str,
  ) -> Result<(), PathError> {
    let segments = Self::parse_path(path);
    if segments.is_empty() {
      // Creating root is just ensure_root.
      return self.ensure_root();
    }

    self.ensure_root()?;
    self.ensure_directories(&segments)?;

    Ok(())
  }

  /// Check if a path exists (file or directory).
  pub fn exists(&self, path: &str) -> Result<bool, PathError> {
    let segments = Self::parse_path(path);
    if segments.is_empty() {
      return self.directory.directory_exists("/").map_err(PathError::from);
    }

    let entry_name = segments[segments.len() - 1];
    let parent_segments = &segments[..segments.len() - 1];
    let parent_path = Self::build_directory_path(parent_segments);

    let entry = self.directory.get_entry(&parent_path, entry_name)?;
    Ok(entry.is_some())
  }

  /// Ensure root directory exists. Call on startup.
  pub fn ensure_root(&self) -> Result<(), PathError> {
    self.directory.create_directory("/")?;
    Ok(())
  }

  /// Internal: ensure all directories in the given segments exist, creating them if needed.
  /// For segments ["myapp", "deep", "nested"], ensures:
  ///   - "myapp" entry in root + table "/myapp"
  ///   - "deep" entry in /myapp + table "/myapp/deep"
  ///   - "nested" entry in /myapp/deep + table "/myapp/deep/nested"
  fn ensure_directories(&self, segments: &[&str]) -> Result<(), PathError> {
    for index in 0..segments.len() {
      let parent_path = Self::build_directory_path(&segments[..index]);
      let segment_name = segments[index];
      let child_path = Self::build_directory_path(&segments[..=index]);

      // Check if an entry already exists at this name.
      if let Some(existing) = self.directory.get_entry(&parent_path, segment_name)? {
        if existing.entry_type != EntryType::Directory {
          return Err(PathError::NotADirectory(format!(
            "'{segment_name}' in path is not a directory",
          )));
        }
        // Directory entry exists; ensure the table also exists.
        self.directory.create_directory(&child_path)?;
        continue;
      }

      // Create the directory entry in the parent.
      let directory_entry = DirectoryEntry::new_directory(segment_name);
      self.directory.insert_entry(&parent_path, &directory_entry)?;

      // Create the table for this directory.
      self.directory.create_directory(&child_path)?;
    }

    Ok(())
  }
}
