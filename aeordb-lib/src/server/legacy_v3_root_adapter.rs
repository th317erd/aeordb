//! Legacy-v3 compatibility reader for one exact namespace root.
//!
//! This is the sole P7 compatibility owner for legacy directory-tree walking.
//! It captures HEAD once or resolves one supplied selector, then performs every
//! namespace lookup against that selected root. It does not authorize paths,
//! activate v4 storage, mutate state, or fall back to mutable path locators.

use std::collections::{BTreeSet, HashSet};
use std::error::Error;
use std::fmt;

use crate::engine::btree::{BTreeWalkMode, btree_list_from_node_with_mode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::directory_ops::{EngineFileStream, file_content_hash, file_identity_hash, file_path_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::normalize_path;
use crate::engine::permission_resolver::{CrudlifyOp, evaluate_ordered_path_permissions};
use crate::engine::permissions::{PathPermissions, PermissionLink};
use crate::engine::range_extract::{ExtractedRange, RangeExtractionRequest, extract_range_from_record_including_deleted};
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::symlink_resolver::MAX_SYMLINK_DEPTH;
use crate::engine::v4::read_view::{ReadViewRootMetadataV1, ReadableRootStateV1};
use crate::engine::version_access::{resolve_directory_at_version, resolve_file_at_version, resolve_symlink_at_version};
use crate::engine::version_manager::VersionManager;

use super::root_api::{RequestedRootSelectorV1, RootApiErrorV1, RootResponseV1, root_response_v1};

const LEGACY_PERMISSION_DOCUMENT_MAX_BYTES_V1: u64 = 1024 * 1024;
const LEGACY_DESCENDANT_MAXIMUM_DEPTH_V1: usize = 10;
const LEGACY_DESCENDANT_MAXIMUM_DIRECTORIES_V1: usize = 100_000;
const LEGACY_DESCENDANT_MAXIMUM_PERMISSION_FILES_V1: usize = 1_000;

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

  pub fn engine_hash_algorithm(&self) -> crate::engine::HashAlgorithm {
    self.engine.hash_algo()
  }

  pub fn file(&self, path: &str) -> EngineResult<LegacyV3SelectedFileV1> {
    let (record_hash, record) = resolve_file_at_version(self.engine, self.selected_root(), path)?;
    Ok(LegacyV3SelectedFileV1 { record_hash, record })
  }

  pub fn read_file_body(&self, path: &str) -> EngineResult<Vec<u8>> {
    let selected_file = self.file(path)?;
    EngineFileStream::from_chunk_hashes_including_deleted(selected_file.record.chunk_hashes, self.engine)?.collect_to_vec()
  }

  pub fn file_stream(&self, selected_file: &LegacyV3SelectedFileV1) -> EngineResult<EngineFileStream<'engine>> {
    EngineFileStream::from_chunk_hashes_including_deleted(selected_file.record.chunk_hashes.clone(), self.engine)
  }

  pub fn extract_range(&self, selected_file: &LegacyV3SelectedFileV1, request: &RangeExtractionRequest) -> EngineResult<ExtractedRange> {
    extract_range_from_record_including_deleted(self.engine, &selected_file.record, request)
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

  /// Preserve the legacy recursive-listing contract while keeping every tree
  /// walk bound to this adapter's captured root.
  pub fn list_directory_recursive(
    &self,
    path: &str,
    depth: i32,
    glob_pattern: Option<&str>,
  ) -> EngineResult<Vec<LegacyV3SelectedDirectoryEntryV1>> {
    let normalized_path = normalize_path(path);
    let recursive_mode = depth != 0;
    let mut entries = Vec::new();
    self.collect_directory_entries(&normalized_path, &normalized_path, depth, recursive_mode, glob_pattern, &mut entries)?;
    Ok(entries)
  }

  /// Evaluate the selected root's immutable permission documents using the
  /// same ordered algebra as current permission resolution. The returned
  /// decision is a restriction only; callers own intersection with the
  /// already-completed current decision.
  pub fn authorize_path(
    &self,
    path: &str,
    operation: CrudlifyOp,
    current_groups: &[String],
  ) -> EngineResult<Option<crate::engine::v4::read_view_authorization::PathAuthorizationDecisionV1>> {
    use crate::engine::v4::read_view_authorization::PathAuthorizationDecisionV1;

    let direct = evaluate_ordered_path_permissions(current_groups, path, operation, |level| {
      self.load_permission_document(&permission_document_path(level))
    })?;
    let direct = if direct || path.ends_with('/') {
      direct
    } else {
      evaluate_ordered_path_permissions(current_groups, &format!("{path}/"), operation, |level| {
        self.load_permission_document(&permission_document_path(level))
      })?
    };
    if direct {
      return Ok(Some(PathAuthorizationDecisionV1::direct()));
    }
    if !matches!(operation, CrudlifyOp::Read | CrudlifyOp::List) {
      return Ok(None);
    }
    let children = self.descendant_grant_children(path, current_groups)?;
    Ok(PathAuthorizationDecisionV1::ancestor_navigation(children))
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
    self.follow_path_authorized(path, |_| Ok(true))
  }

  /// Follow a selected-root symlink chain while requiring the caller to
  /// authorize each target before the adapter reads that target's entity.
  pub fn follow_path_authorized(
    &self,
    path: &str,
    mut authorize_target: impl FnMut(&str) -> EngineResult<bool>,
  ) -> EngineResult<LegacyV3ResolvedPathV1> {
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
          let target_path = normalize_path(&symlink.record.target);
          if !authorize_target(&target_path)? {
            return Err(EngineError::NotFound(format!("Symlink target is not available for '{}'", normalize_path(path))));
          }
          current_path = target_path;
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
    let algorithm = self.engine.hash_algo();
    let canonical_alias = record_hash == file_path_hash(&candidate.path, &algorithm)?
      || record_hash == file_identity_hash(&candidate.path, candidate.content_type.as_deref(), &candidate.chunk_hashes, &algorithm)?
      || record_hash == file_content_hash(&value, &algorithm)?;
    if !canonical_alias {
      return Err(EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()));
    }
    let selected = self.file(&candidate.path)?;
    if selected.record != candidate {
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

  fn collect_directory_entries(
    &self,
    base_path: &str,
    current_path: &str,
    remaining_depth: i32,
    recursive_mode: bool,
    glob_pattern: Option<&str>,
    output: &mut Vec<LegacyV3SelectedDirectoryEntryV1>,
  ) -> EngineResult<()> {
    for entry in self.list_directory(current_path)? {
      let entry_type = EntryType::from_u8(entry.entry_type)?;
      match entry_type {
        EntryType::FileRecord | EntryType::Symlink => {
          if glob_pattern.is_none_or(|pattern| listing_glob_matches(pattern, base_path, &entry.path, &entry.name)) {
            output.push(entry);
          }
        }
        EntryType::DirectoryIndex if !recursive_mode => {
          if glob_pattern.is_none_or(|pattern| listing_glob_matches(pattern, base_path, &entry.path, &entry.name)) {
            output.push(entry);
          }
        }
        EntryType::DirectoryIndex if remaining_depth > 0 || remaining_depth == -1 => {
          let next_depth = if remaining_depth == -1 { -1 } else { remaining_depth - 1 };
          self.collect_directory_entries(base_path, &entry.path, next_depth, true, glob_pattern, output)?;
        }
        EntryType::DirectoryIndex => {}
        _ => {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("selected recursive listing found non-namespace entry type {entry_type:?} at '{}'", entry.path),
          });
        }
      }
    }
    Ok(())
  }

  fn load_permission_document(&self, path: &str) -> EngineResult<Option<PathPermissions>> {
    let selected_file = match self.file(path) {
      Ok(selected_file) => selected_file,
      Err(EngineError::NotFound(_)) => return Ok(None),
      Err(error) => return Err(error),
    };
    if selected_file.record.total_size > LEGACY_PERMISSION_DOCUMENT_MAX_BYTES_V1 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("stored selected permission authority at {path} exceeds the {LEGACY_PERMISSION_DOCUMENT_MAX_BYTES_V1}-byte limit"),
      });
    }
    let bytes = EngineFileStream::from_chunk_hashes_including_deleted(selected_file.record.chunk_hashes, self.engine)?.collect_to_vec()?;
    if bytes.len() as u64 > LEGACY_PERMISSION_DOCUMENT_MAX_BYTES_V1 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("stored selected permission authority at {path} exceeds the {LEGACY_PERMISSION_DOCUMENT_MAX_BYTES_V1}-byte limit"),
      });
    }
    PathPermissions::deserialize_stored(&bytes, path).map(Some)
  }

  fn descendant_grant_children(&self, parent_path: &str, current_groups: &[String]) -> EngineResult<BTreeSet<String>> {
    let normalized_parent = normalize_path(parent_path);
    let mut allowed_children = BTreeSet::new();
    let mut visited_directories = 0usize;
    let mut permission_files = 0usize;
    self.scan_descendant_permissions(
      &normalized_parent,
      &normalized_parent,
      0,
      current_groups,
      &mut visited_directories,
      &mut permission_files,
      &mut allowed_children,
    )?;
    Ok(allowed_children)
  }

  #[allow(clippy::too_many_arguments)]
  fn scan_descendant_permissions(
    &self,
    parent_path: &str,
    directory_path: &str,
    relative_depth: usize,
    current_groups: &[String],
    visited_directories: &mut usize,
    permission_files: &mut usize,
    allowed_children: &mut BTreeSet<String>,
  ) -> EngineResult<()> {
    *visited_directories = visited_directories.saturating_add(1);
    if *visited_directories > LEGACY_DESCENDANT_MAXIMUM_DIRECTORIES_V1 {
      return Err(EngineError::ResourceExhausted(format!(
        "selected descendant permission scan exceeded its {LEGACY_DESCENDANT_MAXIMUM_DIRECTORIES_V1}-directory bound under '{parent_path}'"
      )));
    }
    let directory = match self.directory(directory_path) {
      Ok(directory) => directory,
      Err(EngineError::NotFound(_)) => return Ok(()),
      Err(error) => return Err(error),
    };
    for entry in directory.entries {
      let entry_type = EntryType::from_u8(entry.entry_type)?;
      if entry.name == ".aeordb-permissions" {
        if entry_type != EntryType::FileRecord {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("selected permission authority at '{}' is not a file", entry.path),
          });
        }
        *permission_files = permission_files.saturating_add(1);
        if *permission_files > LEGACY_DESCENDANT_MAXIMUM_PERMISSION_FILES_V1 {
          return Err(EngineError::ResourceExhausted(format!(
            "selected descendant permission scan exceeded its {LEGACY_DESCENDANT_MAXIMUM_PERMISSION_FILES_V1}-file bound under '{parent_path}'"
          )));
        }
        let document = self.load_permission_document(&entry.path)?.ok_or_else(|| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("selected permission authority at '{}' disappeared", entry.path),
        })?;
        collect_descendant_children(&document.links, current_groups, parent_path, directory_path, allowed_children);
      } else if entry_type == EntryType::DirectoryIndex && relative_depth < LEGACY_DESCENDANT_MAXIMUM_DEPTH_V1 {
        self.scan_descendant_permissions(
          parent_path,
          &entry.path,
          relative_depth + 1,
          current_groups,
          visited_directories,
          permission_files,
          allowed_children,
        )?;
      }
    }
    Ok(())
  }
}

fn permission_document_path(level: &str) -> String {
  if level == "/" {
    "/.aeordb-permissions".to_string()
  } else {
    format!("{}/.aeordb-permissions", level.trim_end_matches('/'))
  }
}

fn listing_glob_matches(pattern: &str, base_path: &str, child_path: &str, child_name: &str) -> bool {
  if glob_match::glob_match(pattern, child_name) {
    return true;
  }
  let relative = if base_path == "/" {
    child_path.trim_start_matches('/').to_string()
  } else {
    child_path.strip_prefix(base_path.trim_end_matches('/')).unwrap_or(child_path).trim_start_matches('/').to_string()
  };
  crate::engine::indexing_pipeline::glob_matches(pattern, &relative) || crate::engine::indexing_pipeline::glob_matches(pattern, child_path)
}

fn collect_descendant_children(
  links: &[PermissionLink],
  current_groups: &[String],
  parent_path: &str,
  document_directory: &str,
  output: &mut BTreeSet<String>,
) {
  for link in links {
    if !current_groups.contains(&link.group) {
      continue;
    }
    let target = link.path_pattern.as_ref().map_or_else(
      || document_directory.to_string(),
      |name| if document_directory == "/" { format!("/{name}") } else { format!("{document_directory}/{name}") },
    );
    if let Some(child) = next_segment_below(parent_path, &target) {
      output.insert(child.to_string());
    }
  }
}

fn next_segment_below<'a>(parent_path: &str, target: &'a str) -> Option<&'a str> {
  let parent = if parent_path == "/" { "" } else { parent_path.trim_end_matches('/') };
  let suffix = target.strip_prefix(parent)?;
  if !suffix.starts_with('/') {
    return None;
  }
  suffix.trim_start_matches('/').split('/').next().filter(|segment| !segment.is_empty())
}
