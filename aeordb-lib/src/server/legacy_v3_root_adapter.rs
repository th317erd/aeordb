//! Legacy-v3 compatibility reader for one exact namespace root.
//!
//! This is the sole P7 compatibility owner for legacy directory-tree walking.
//! It captures HEAD once or resolves one supplied selector, then performs every
//! namespace lookup against that selected root. It does not authorize paths,
//! activate v4 storage, mutate state, or fall back to mutable path locators.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::engine::btree::{BTreeWalkMode, btree_list_from_node_with_mode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::directory_ops::EngineFileStream;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::symlink_resolver::MAX_SYMLINK_DEPTH;
use crate::engine::v4::read_view::{ReadViewRootMetadataV1, ReadableRootStateV1};
use crate::engine::version_access::{resolve_directory_at_version, resolve_file_at_version, resolve_symlink_at_version};
use crate::engine::version_manager::VersionManager;

use super::root_api::{RequestedRootSelectorV1, RootApiErrorV1, RootResponseV1, root_response_v1};

#[derive(Debug)]
pub struct LegacyV3RootAdapterErrorV1 {
  public_error: RootApiErrorV1,
  context: &'static str,
  source: Option<EngineError>,
}

impl LegacyV3RootAdapterErrorV1 {
  fn public(public_error: RootApiErrorV1, context: &'static str) -> Self {
    Self { public_error, context, source: None }
  }

  fn storage(public_error: RootApiErrorV1, context: &'static str, source: EngineError) -> Self {
    Self { public_error, context, source: Some(source) }
  }

  pub const fn public_error(&self) -> RootApiErrorV1 {
    self.public_error
  }

  pub const fn context(&self) -> &'static str {
    self.context
  }

  pub const fn engine_source(&self) -> Option<&EngineError> {
    self.source.as_ref()
  }
}

impl fmt::Display for LegacyV3RootAdapterErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.public_error.code(), self.context)
  }
}

impl Error for LegacyV3RootAdapterErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self.source.as_ref() {
      Some(source) => Some(source),
      None => None,
    }
  }
}

impl From<RootApiErrorV1> for LegacyV3RootAdapterErrorV1 {
  fn from(public_error: RootApiErrorV1) -> Self {
    Self::public(public_error, "selected root metadata is invalid")
  }
}

impl PartialEq<RootApiErrorV1> for LegacyV3RootAdapterErrorV1 {
  fn eq(&self, other: &RootApiErrorV1) -> bool {
    self.public_error == *other
  }
}

pub struct LegacyV3SelectedRootAdapterV1<'engine> {
  engine: &'engine StorageEngine,
  root_metadata: ReadViewRootMetadataV1,
  root: RootResponseV1,
}

impl fmt::Debug for LegacyV3SelectedRootAdapterV1<'_> {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("LegacyV3SelectedRootAdapterV1").field("root", &self.root).finish_non_exhaustive()
  }
}

#[derive(Debug)]
pub struct LegacyV3SelectedFileV1 {
  pub record_hash: Vec<u8>,
  pub record: FileRecord,
}

#[derive(Debug)]
pub struct LegacyV3SelectedSymlinkV1 {
  pub record_hash: Vec<u8>,
  pub record: SymlinkRecord,
}

#[derive(Debug)]
pub struct LegacyV3SelectedDirectoryEntryV1 {
  pub path: String,
  pub name: String,
  pub entry_type: u8,
  pub record_hash: Vec<u8>,
  pub total_size: u64,
  pub created_at: i64,
  pub updated_at: i64,
  pub content_type: Option<String>,
  pub symlink_target: Option<String>,
}

#[derive(Debug)]
pub struct LegacyV3SelectedDirectoryV1 {
  pub path: String,
  pub record_hash: Vec<u8>,
  pub entries: Vec<LegacyV3SelectedDirectoryEntryV1>,
}

#[derive(Debug)]
pub enum LegacyV3SelectedPathV1 {
  File(LegacyV3SelectedFileV1),
  Directory(LegacyV3SelectedDirectoryV1),
  Symlink(LegacyV3SelectedSymlinkV1),
}

#[derive(Debug)]
pub enum LegacyV3ResolvedPathV1 {
  File(LegacyV3SelectedFileV1),
  Directory(LegacyV3SelectedDirectoryV1),
}

impl<'engine> LegacyV3SelectedRootAdapterV1<'engine> {
  pub fn resolve(engine: &'engine StorageEngine, selector: &RequestedRootSelectorV1) -> Result<Self, LegacyV3RootAdapterErrorV1> {
    let current_head = match engine.head_hash() {
      Ok(current_head) => current_head,
      Err(error) => {
        return Err(LegacyV3RootAdapterErrorV1::storage(RootApiErrorV1::DatabaseCorruption, "failed to capture current HEAD", error));
      }
    };
    let selected_root = match selector {
      RequestedRootSelectorV1::CurrentHead => current_head.clone(),
      RequestedRootSelectorV1::ExplicitRoot(root) | RequestedRootSelectorV1::VersionRoot(root) => root.clone(),
      RequestedRootSelectorV1::Snapshot(name) => match VersionManager::new(engine).get_snapshot_hash(name) {
        Ok(root) => root,
        Err(error @ EngineError::NotFound(_)) => {
          return Err(LegacyV3RootAdapterErrorV1::storage(RootApiErrorV1::InvalidNamespaceRoot, "selected snapshot does not exist", error));
        }
        Err(error) => {
          return Err(LegacyV3RootAdapterErrorV1::storage(
            RootApiErrorV1::DatabaseCorruption,
            "failed to resolve selected snapshot",
            error,
          ));
        }
      },
    };

    let hash_length = engine.hash_algo().hash_length();
    if selected_root.len() != hash_length || selected_root.iter().all(|byte| *byte == 0) {
      return Err(LegacyV3RootAdapterErrorV1::public(RootApiErrorV1::InvalidRootHash, "selected root hash is invalid"));
    }
    if current_head.len() != hash_length || current_head.iter().all(|byte| *byte == 0) {
      return Err(LegacyV3RootAdapterErrorV1::public(RootApiErrorV1::DatabaseCorruption, "captured current HEAD hash is invalid"));
    }

    let root_header = match engine.get_entry_header_including_deleted(&selected_root) {
      Ok(Some(root_header)) => root_header,
      Ok(None) => {
        let public_error = match selector {
          RequestedRootSelectorV1::CurrentHead => RootApiErrorV1::DatabaseCorruption,
          _ => RootApiErrorV1::HistoricalViewUnavailable,
        };
        return Err(LegacyV3RootAdapterErrorV1::public(public_error, "selected namespace root is unavailable"));
      }
      Err(error) => {
        return Err(LegacyV3RootAdapterErrorV1::storage(
          RootApiErrorV1::DatabaseCorruption,
          "failed to inspect selected namespace root",
          error,
        ));
      }
    };
    if root_header.entry_type != EntryType::DirectoryIndex || root_header.is_system_entry() {
      return Err(LegacyV3RootAdapterErrorV1::public(RootApiErrorV1::InvalidNamespaceRoot, "selected hash is not a public namespace root"));
    }

    let state = if selected_root == current_head { ReadableRootStateV1::Live } else { ReadableRootStateV1::Retained };
    let root_metadata = ReadViewRootMetadataV1 { hash: selected_root, state, expires_at_ms: None };
    let root = root_response_v1(&root_metadata, engine.hash_algo())?;
    Ok(Self { engine, root_metadata, root })
  }

  pub fn selected_root(&self) -> &[u8] {
    &self.root_metadata.hash
  }

  pub const fn root_metadata(&self) -> &ReadViewRootMetadataV1 {
    &self.root_metadata
  }

  pub const fn root(&self) -> &RootResponseV1 {
    &self.root
  }

  pub fn file(&self, path: &str) -> EngineResult<LegacyV3SelectedFileV1> {
    let (record_hash, record) = resolve_file_at_version(self.engine, self.selected_root(), path)?;
    Ok(LegacyV3SelectedFileV1 { record_hash, record })
  }

  pub fn read_file_body(&self, path: &str) -> EngineResult<Vec<u8>> {
    let selected_file = self.file(path)?;
    EngineFileStream::from_chunk_hashes_including_deleted(selected_file.record.chunk_hashes, self.engine)?.collect_to_vec()
  }

  pub fn symlink(&self, path: &str) -> EngineResult<LegacyV3SelectedSymlinkV1> {
    let (record_hash, record) = resolve_symlink_at_version(self.engine, self.selected_root(), path)?;
    Ok(LegacyV3SelectedSymlinkV1 { record_hash, record })
  }

  pub fn directory(&self, path: &str) -> EngineResult<LegacyV3SelectedDirectoryV1> {
    let normalized_path = normalize_path(path);
    let resolved = resolve_directory_at_version(self.engine, self.selected_root(), &normalized_path)?;
    let children = self.decode_directory_children(&resolved.hash, &resolved.header, &resolved.value)?;
    let mut names = HashSet::with_capacity(children.len());
    let mut entries = Vec::with_capacity(children.len());
    for child in children {
      if !names.insert(child.name.clone()) {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("selected directory '{normalized_path}' contains duplicate child name '{}'", child.name),
        });
      }
      entries.push(self.selected_directory_entry(&normalized_path, child)?);
    }
    Ok(LegacyV3SelectedDirectoryV1 { path: normalized_path, record_hash: resolved.hash, entries })
  }

  pub fn list_directory(&self, path: &str) -> EngineResult<Vec<LegacyV3SelectedDirectoryEntryV1>> {
    Ok(self.directory(path)?.entries)
  }

  pub fn path(&self, path: &str) -> EngineResult<LegacyV3SelectedPathV1> {
    let normalized_path = normalize_path(path);
    if normalized_path == "/" {
      return self.directory(&normalized_path).map(LegacyV3SelectedPathV1::Directory);
    }

    match crate::engine::version_access::resolve_entry_type_at_version(self.engine, self.selected_root(), &normalized_path)? {
      EntryType::FileRecord => self.file(&normalized_path).map(LegacyV3SelectedPathV1::File),
      EntryType::DirectoryIndex => self.directory(&normalized_path).map(LegacyV3SelectedPathV1::Directory),
      EntryType::Symlink => self.symlink(&normalized_path).map(LegacyV3SelectedPathV1::Symlink),
      entry_type => Err(EngineError::NotFound(format!("Path '{normalized_path}' is not a public namespace entry ({entry_type:?})"))),
    }
  }

  pub fn follow_path(&self, path: &str) -> EngineResult<LegacyV3ResolvedPathV1> {
    let mut visited = HashSet::new();
    let mut current_path = normalize_path(path);
    let mut chain = Vec::new();
    let mut depth = 0usize;

    loop {
      if !visited.insert(current_path.clone()) {
        chain.push(current_path);
        return Err(EngineError::CyclicSymlink(chain.join(" -> ")));
      }
      if depth >= MAX_SYMLINK_DEPTH {
        return Err(EngineError::SymlinkDepthExceeded(format!(
          "Exceeded maximum symlink depth of {MAX_SYMLINK_DEPTH} following '{}'",
          normalize_path(path)
        )));
      }
      chain.push(current_path.clone());

      match self.path(&current_path)? {
        LegacyV3SelectedPathV1::File(file) => return Ok(LegacyV3ResolvedPathV1::File(file)),
        LegacyV3SelectedPathV1::Directory(directory) => return Ok(LegacyV3ResolvedPathV1::Directory(directory)),
        LegacyV3SelectedPathV1::Symlink(symlink) => {
          current_path = normalize_path(&symlink.record.target);
          depth = depth.checked_add(1).ok_or_else(|| EngineError::SymlinkDepthExceeded("symlink depth overflow".to_string()))?;
        }
      }
    }
  }

  pub fn file_by_hash(&self, record_hash: &[u8]) -> EngineResult<LegacyV3SelectedFileV1> {
    if record_hash.len() != self.engine.hash_algo().hash_length() || record_hash.iter().all(|byte| *byte == 0) {
      return Err(EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()));
    }
    let header = self
      .engine
      .get_entry_header_including_deleted(record_hash)?
      .ok_or_else(|| EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()))?;
    if header.entry_type != EntryType::FileRecord || header.is_system_entry() {
      return Err(EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()));
    }
    let (stored_header, stored_key, value) = self
      .engine
      .get_entry_verified_including_deleted(record_hash)?
      .ok_or_else(|| EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()))?;
    if stored_key != record_hash || stored_header.entry_type != EntryType::FileRecord || stored_header.entry_version != header.entry_version
    {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "selected FileRecord hash changed while reading".to_string() });
    }
    let candidate = FileRecord::deserialize(&value, self.engine.hash_algo().hash_length(), stored_header.entry_version)?;
    let selected = self.file(&candidate.path)?;
    if selected.record_hash != record_hash {
      return Err(EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()));
    }
    Ok(selected)
  }

  fn decode_directory_children(
    &self,
    directory_hash: &[u8],
    header: &crate::engine::entry_header::EntryHeader,
    value: &[u8],
  ) -> EngineResult<Vec<ChildEntry>> {
    if value.is_empty() {
      return Ok(Vec::new());
    }
    if is_btree_format(value) {
      return Ok(
        btree_list_from_node_with_mode(value, self.engine, self.engine.hash_algo().hash_length(), true, BTreeWalkMode::Strict)?.entries,
      );
    }
    let children = deserialize_child_entries(value, self.engine.hash_algo().hash_length(), header.entry_version)?;
    if directory_hash.len() != self.engine.hash_algo().hash_length() {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "selected directory hash width is invalid".to_string() });
    }
    Ok(children)
  }

  fn selected_directory_entry(&self, directory_path: &str, child: ChildEntry) -> EngineResult<LegacyV3SelectedDirectoryEntryV1> {
    let entry_type = EntryType::from_u8(child.entry_type)?;
    let path = if directory_path == "/" { format!("/{}", child.name) } else { format!("{directory_path}/{}", child.name) };
    if child.name.is_empty() || child.name.contains('/') || child.name.contains('\0') || normalize_path(&path) != path {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("selected directory '{directory_path}' contains non-canonical child name '{}'", child.name),
      });
    }
    let child_header = self.engine.get_entry_header_including_deleted(&child.hash)?.ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("selected directory entry '{path}' points to a missing record"),
    })?;
    if child_header.entry_type != entry_type {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("selected directory entry '{path}' declares {entry_type:?} but points to {:?}", child_header.entry_type),
      });
    }
    let symlink_target = match entry_type {
      EntryType::FileRecord | EntryType::Symlink => {
        let (stored_header, stored_key, value) = self.engine.get_entry_verified_including_deleted(&child.hash)?.ok_or_else(|| {
          EngineError::CorruptEntry { offset: 0, reason: format!("selected directory entry '{path}' disappeared while reading") }
        })?;
        if stored_key != child.hash
          || stored_header.entry_type != child_header.entry_type
          || stored_header.entry_version != child_header.entry_version
        {
          return Err(EngineError::CorruptEntry { offset: 0, reason: format!("selected directory entry '{path}' changed while reading") });
        }
        if entry_type == EntryType::FileRecord {
          let record = FileRecord::deserialize(&value, self.engine.hash_algo().hash_length(), stored_header.entry_version)?;
          if record.path != path {
            return Err(EngineError::CorruptEntry {
              offset: 0,
              reason: format!("selected FileRecord path '{}' does not match directory path '{path}'", record.path),
            });
          }
          None
        } else {
          let record = SymlinkRecord::deserialize(&value, stored_header.entry_version)?;
          if record.path != path {
            return Err(EngineError::CorruptEntry {
              offset: 0,
              reason: format!("selected symlink path '{}' does not match directory path '{path}'", record.path),
            });
          }
          Some(record.target)
        }
      }
      EntryType::DirectoryIndex => None,
      _ => {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("selected directory entry '{path}' has non-namespace type {entry_type:?}"),
        });
      }
    };
    Ok(LegacyV3SelectedDirectoryEntryV1 {
      path,
      name: child.name,
      entry_type: child.entry_type,
      record_hash: child.hash,
      total_size: child.total_size,
      created_at: child.created_at,
      updated_at: child.updated_at,
      content_type: child.content_type,
      symlink_target,
    })
  }
}
