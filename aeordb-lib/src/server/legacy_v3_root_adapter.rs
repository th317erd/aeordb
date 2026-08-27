//! Legacy-v3 compatibility reader for one exact namespace root.
//!
//! This is the sole P7 compatibility owner for legacy directory-tree walking.
//! It captures HEAD once or resolves one supplied selector, then performs every
//! namespace lookup against that selected root. It does not authorize paths,
//! activate v4 storage, mutate state, or fall back to mutable path locators.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;

use crate::engine::btree::{BTreeWalkMode, btree_list_from_node_with_mode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::directory_ops::{EngineFileStream, file_content_hash, file_identity_hash, file_path_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_config::{IndexFieldConfig, PathIndexConfig, create_converter_from_config};
use crate::engine::index_config_resolver::glob_matches;
use crate::engine::index_store::{FieldIndex, INDEX_LOAD_SERIALIZED_AMPLIFICATION, IndexLoadMemoryAccount};
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::permission_resolver::{CrudlifyOp, evaluate_ordered_path_permissions};
use crate::engine::permissions::{PathPermissions, PermissionLink};
use crate::engine::query_engine::{QueryMemoryBudget, QueryReadSourceV1, QuerySourceFileV1};
use crate::engine::range_extract::{ExtractedRange, RangeExtractionRequest, extract_range_from_record_including_deleted};
use crate::engine::source_resolver::resolve_sources;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::symlink_resolver::MAX_SYMLINK_DEPTH;
use crate::engine::system_family_policy::SystemFamilyPolicyResolver;
use crate::engine::v4::read_view::{ReadViewRootMetadataV1, ReadableRootStateV1};
use crate::engine::v4::system_family::{IndexPolicyV1, SystemFamilyPolicyDecisionV1};
use crate::engine::version_access::{resolve_directory_at_version, resolve_file_at_version, resolve_symlink_at_version};
use crate::engine::version_manager::VersionManager;

use super::root_api::{RequestedRootSelectorV1, RootApiErrorV1, RootResponseV1, root_response_v1};

const LEGACY_PERMISSION_DOCUMENT_MAX_BYTES_V1: u64 = 1024 * 1024;
const LEGACY_DESCENDANT_MAXIMUM_DEPTH_V1: usize = 10;
const LEGACY_DESCENDANT_MAXIMUM_DIRECTORIES_V1: usize = 100_000;
const LEGACY_DESCENDANT_MAXIMUM_PERMISSION_FILES_V1: usize = 1_000;
const LEGACY_SELECTED_INDEX_CONFIG_MAX_BYTES_V1: u64 = 16 * 1024 * 1024;
const LEGACY_SELECTED_PARSER_REGISTRY_MAX_BYTES_V1: u64 = 1024 * 1024;
const LEGACY_SELECTED_PARSER_REGISTRY_PATH_V1: &str = "/.aeordb-config/parsers.json";
const LEGACY_SELECTED_INDEX_BUILD_FIXED_OVERHEAD_BYTES_V1: u64 = 64 * 1024;

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

struct SelectedDirectoryTraversal<'path> {
  base_path: &'path str,
  recursive_mode: bool,
  glob_pattern: Option<&'path str>,
  family_policy: SystemFamilyPolicyResolver,
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
    let (normalized_path, record_hash, children) = self.selected_directory_children(path)?;
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
    Ok(LegacyV3SelectedDirectoryV1 { path: normalized_path, record_hash, entries })
  }

  pub fn list_directory(&self, path: &str) -> EngineResult<Vec<LegacyV3SelectedDirectoryEntryV1>> {
    Ok(self.directory(path)?.entries)
  }

  /// Preserve the legacy recursive-listing contract while keeping every tree
  /// walk bound to this adapter's captured root.
  pub fn list_directory_recursive_strict(
    &self,
    path: &str,
    depth: i32,
    glob_pattern: Option<&str>,
  ) -> EngineResult<Vec<LegacyV3SelectedDirectoryEntryV1>> {
    let normalized_path = normalize_path(path);
    let family_policy = SystemFamilyPolicyResolver::new(self.engine.hash_algo())?;
    family_policy.policy_for_path(&normalized_path, "strict selected-root recursive traversal")?;
    let traversal = SelectedDirectoryTraversal { base_path: &normalized_path, recursive_mode: depth != 0, glob_pattern, family_policy };
    let mut entries = Vec::new();
    let mut active_directory_hashes = HashSet::new();
    self.collect_directory_entries(&traversal, &normalized_path, depth, &mut active_directory_hashes, &mut entries)?;
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
    let selected_header = self
      .engine
      .get_entry_header_including_deleted(&selected.record_hash)?
      .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "selected FileRecord revision disappeared".to_string() })?;
    let selected_value = selected.record.serialize_for_version(self.engine.hash_algo().hash_length(), selected_header.entry_version)?;
    let selected_alias = record_hash == selected.record_hash
      || record_hash == file_path_hash(&selected.record.path, &algorithm)?
      || record_hash == file_content_hash(&selected_value, &algorithm)?
      || record_hash
        == file_identity_hash(&selected.record.path, selected.record.content_type.as_deref(), &selected.record.chunk_hashes, &algorithm)?;
    if !selected_alias {
      return Err(EngineError::NotFound("FileRecord hash is not reachable from the selected root".to_string()));
    }
    Ok(selected)
  }

  fn query_indexes_directory_path(path: &str) -> String {
    let normalized_path = normalize_path(path);
    if normalized_path == "/" {
      "/.aeordb-indexes".to_string()
    } else {
      format!("{normalized_path}/.aeordb-indexes")
    }
  }

  fn query_index_config_file_path(path: &str) -> String {
    let normalized_path = normalize_path(path);
    if normalized_path == "/" {
      "/.aeordb-config/indexes.json".to_string()
    } else {
      format!("{normalized_path}/.aeordb-config/indexes.json")
    }
  }

  fn load_selected_file_body_bounded(
    &self,
    path: &str,
    maximum_bytes: u64,
    budget: &mut QueryMemoryBudget,
    context: &str,
  ) -> EngineResult<Option<Vec<u8>>> {
    let selected_file = match self.file(path) {
      Ok(selected_file) => selected_file,
      Err(EngineError::NotFound(_)) => return Ok(None),
      Err(error) => return Err(error),
    };
    if selected_file.record.total_size > maximum_bytes {
      return Err(EngineError::ResourceExhausted(format!(
        "{context} at '{path}' is {} bytes, exceeding the {maximum_bytes}-byte bound",
        selected_file.record.total_size
      )));
    }
    let reserved_bytes = selected_file
      .record
      .total_size
      .checked_mul(INDEX_LOAD_SERIALIZED_AMPLIFICATION)
      .and_then(|bytes| bytes.checked_add(4 * 1024))
      .ok_or_else(|| EngineError::ResourceExhausted(format!("{context} memory estimate overflow")))?;
    budget.grow_index_load(reserved_bytes, "selected query configuration admission failed")?;
    let data = self.file_stream(&selected_file)?.collect_to_vec()?;
    let actual_length =
      u64::try_from(data.len()).map_err(|error| EngineError::ResourceExhausted(format!("{context} length does not fit u64: {error}")))?;
    if actual_length != selected_file.record.total_size {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("{context} at '{path}' declared {} bytes but yielded {actual_length}", selected_file.record.total_size),
      });
    }
    Ok(Some(data))
  }

  fn load_selected_index_config(&self, owner_path: &str, budget: &mut QueryMemoryBudget) -> EngineResult<Option<PathIndexConfig>> {
    let config_path = Self::query_index_config_file_path(owner_path);
    let Some(data) = self.load_selected_file_body_bounded(
      &config_path,
      LEGACY_SELECTED_INDEX_CONFIG_MAX_BYTES_V1,
      budget,
      "selected index configuration",
    )?
    else {
      return Ok(None);
    };
    PathIndexConfig::deserialize(&data).map(Some).map_err(|error| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("selected index configuration at '{config_path}' is malformed: {error}"),
    })
  }

  fn load_selected_index_config_cached<'cache>(
    &self,
    owner_path: &str,
    budget: &mut QueryMemoryBudget,
    configurations: &'cache mut HashMap<String, Option<PathIndexConfig>>,
  ) -> EngineResult<Option<&'cache PathIndexConfig>> {
    let normalized_owner = normalize_path(owner_path);
    if !configurations.contains_key(&normalized_owner) {
      let configuration = self.load_selected_index_config(&normalized_owner, budget)?;
      configurations.insert(normalized_owner.clone(), configuration);
    }
    Ok(configurations.get(&normalized_owner).and_then(Option::as_ref))
  }

  fn selected_configuration_owner_matches(
    &self,
    file_path: &str,
    target_owner: &str,
    budget: &mut QueryMemoryBudget,
    configurations: &mut HashMap<String, Option<PathIndexConfig>>,
  ) -> EngineResult<bool> {
    let normalized_path = normalize_path(file_path);
    let immediate_parent = match parent_path(&normalized_path) {
      Some(parent) => parent,
      None => "/".to_string(),
    };
    if let Some(configuration) = self.load_selected_index_config_cached(&immediate_parent, budget, configurations)? {
      let matches = match configuration.glob.as_deref() {
        None => true,
        Some(pattern) => glob_matches(pattern, selected_file_name(&normalized_path)),
      };
      if matches {
        return Ok(immediate_parent == target_owner);
      }
    }

    let mut ancestor = parent_path(&immediate_parent);
    while let Some(directory) = ancestor {
      if let Some(configuration) = self.load_selected_index_config_cached(&directory, budget, configurations)? {
        if let Some(pattern) = configuration.glob.as_deref() {
          let prefix = if directory == "/" { "/".to_string() } else { format!("{directory}/") };
          if let Some(relative_path) = normalized_path.strip_prefix(&prefix) {
            if glob_matches(pattern, relative_path) {
              return Ok(directory == target_owner);
            }
          }
        }
      }
      if directory == "/" {
        break;
      }
      ancestor = parent_path(&directory);
    }
    Ok(false)
  }

  fn load_selected_parser_registry(&self, budget: &mut QueryMemoryBudget) -> EngineResult<Option<BTreeMap<String, String>>> {
    let Some(data) = self.load_selected_file_body_bounded(
      LEGACY_SELECTED_PARSER_REGISTRY_PATH_V1,
      LEGACY_SELECTED_PARSER_REGISTRY_MAX_BYTES_V1,
      budget,
      "selected parser registry",
    )?
    else {
      return Ok(None);
    };
    serde_json::from_slice(&data)
      .map(Some)
      .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("selected parser registry is malformed: {error}") })
  }

  fn selected_index_values(
    &self,
    configuration: &PathIndexConfig,
    field: &IndexFieldConfig,
    file: &QuerySourceFileV1,
    parser_registry: &mut Option<BTreeMap<String, String>>,
    parser_registry_loaded: &mut bool,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<Option<(Vec<Vec<u8>>, u64)>> {
    if field.name.starts_with('@') {
      return Ok(selected_metadata_field_value(&field.name, &file.record).map(|value| (vec![value], 0)));
    }

    if let Some(parser) = configuration.parser.as_deref() {
      return Err(EngineError::HistoricalViewUnavailable(format!(
        "exact selected-root fallback requires parser plugin '{parser}' for '{}'",
        file.record.path
      )));
    }
    if field
      .source
      .as_ref()
      .and_then(serde_json::Value::as_object)
      .and_then(|source| source.get("plugin"))
      .and_then(serde_json::Value::as_str)
      .is_some()
    {
      return Err(EngineError::HistoricalViewUnavailable(format!(
        "exact selected-root fallback requires mapper plugin semantics for field '{}'",
        field.name
      )));
    }

    let mut content_type = "application/octet-stream";
    if let Some(selected_content_type) = file.record.content_type.as_deref() {
      content_type = selected_content_type;
    }
    if content_type != "application/json" {
      if !*parser_registry_loaded {
        *parser_registry = self.load_selected_parser_registry(budget)?;
        *parser_registry_loaded = true;
      }
      if parser_registry.is_none() {
        return Err(EngineError::HistoricalViewUnavailable(format!(
          "exact selected-root fallback cannot prove parser-registry absence for non-JSON file '{}'",
          file.record.path
        )));
      }
      if let Some(parser) = parser_registry.as_ref().and_then(|registry| registry.get(content_type)) {
        return Err(EngineError::HistoricalViewUnavailable(format!(
          "exact selected-root fallback requires registered parser plugin '{parser}' for '{}'",
          file.record.path
        )));
      }
    }

    let direct_top_level_field = field.source.is_none();
    let mut transient_bytes = if direct_top_level_field {
      file.record.total_size.checked_mul(4).and_then(|bytes| bytes.checked_add(64 * 1024))
    } else {
      file.record.total_size.checked_mul(INDEX_LOAD_SERIALIZED_AMPLIFICATION).and_then(|bytes| bytes.checked_add(1024 * 1024))
    }
    .ok_or_else(|| EngineError::ResourceExhausted("selected authoritative document estimate overflow".to_string()))?;
    budget.grow_index_load(transient_bytes, "selected authoritative document admission failed")?;
    let data = <Self as QueryReadSourceV1>::read_file_body(self, file)?;
    let actual_length = u64::try_from(data.len())
      .map_err(|error| EngineError::ResourceExhausted(format!("selected authoritative document length does not fit u64: {error}")))?;
    if actual_length != file.record.total_size {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "selected authoritative document '{}' declared {} bytes but yielded {actual_length}",
          file.record.path, file.record.total_size
        ),
      });
    }

    let values = match field.source.as_ref() {
      None => match crate::engine::json_parser::parse_json_top_level_field(&data, &field.name) {
        Ok(Some(value)) => vec![value],
        Ok(None) => Vec::new(),
        Err(EngineError::JsonParseError(_)) => {
          let full_parse_bytes = file
            .record
            .total_size
            .checked_mul(INDEX_LOAD_SERIALIZED_AMPLIFICATION)
            .and_then(|bytes| bytes.checked_add(1024 * 1024))
            .ok_or_else(|| EngineError::ResourceExhausted("selected authoritative native document estimate overflow".to_string()))?;
          if full_parse_bytes > transient_bytes {
            budget.grow_index_load(full_parse_bytes - transient_bytes, "selected authoritative native document admission failed")?;
            transient_bytes = full_parse_bytes;
          }
          match selected_native_document(&data, content_type, &file.record.path, file.record.total_size) {
            Some(document) => resolve_sources(&document, &[serde_json::Value::String(field.name.clone())]),
            None => Vec::new(),
          }
        }
        Err(error) => return Err(error),
      },
      Some(serde_json::Value::Array(source)) => {
        let document = match serde_json::from_slice::<serde_json::Value>(&data) {
          Ok(document) => Some(document),
          Err(error) => {
            tracing::debug!(path = %file.record.path, %error, "selected authoritative JSON parse rejected input; trying the native parser");
            selected_native_document(&data, content_type, &file.record.path, file.record.total_size)
          }
        };
        document.as_ref().map_or_else(Vec::new, |document| resolve_sources(document, source))
      }
      Some(serde_json::Value::Object(_)) | Some(_) => Vec::new(),
    };
    if values.is_empty() {
      budget.shrink_index_load(transient_bytes, "selected authoritative document accounting failed")?;
      return Ok(None);
    }
    Ok(Some((values, transient_bytes)))
  }

  fn build_selected_index(
    &self,
    path: &str,
    field_name: &str,
    strategy: &str,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<Option<FieldIndex>> {
    let normalized_scope = normalize_path(path);
    let Some(configuration) = self.load_selected_index_config(&normalized_scope, budget)? else {
      return Ok(None);
    };
    let Some(field) = configuration
      .indexes
      .iter()
      .find(|field| canonical_selected_index_field_name(&field.name) == Some(field_name) && field.index_type == strategy)
      .cloned()
    else {
      return Ok(None);
    };

    budget.grow_index_load(LEGACY_SELECTED_INDEX_BUILD_FIXED_OVERHEAD_BYTES_V1, "selected authoritative index admission failed")?;
    let converter = create_converter_from_config(&field)?;
    let mut index = FieldIndex::new(field_name.to_string(), converter);
    let mut admitted_index_bytes = LEGACY_SELECTED_INDEX_BUILD_FIXED_OVERHEAD_BYTES_V1;
    let mut configurations = HashMap::new();
    configurations.insert(normalized_scope.clone(), Some(configuration.clone()));
    let mut parser_registry = None;
    let mut parser_registry_loaded = false;
    let family_policy = SystemFamilyPolicyResolver::new(self.engine.hash_algo())?;
    let files = <Self as QueryReadSourceV1>::list_file_records(self, &normalized_scope, budget)?;
    for file in files {
      budget.record_work(1)?;
      let indexable = match family_policy.index_policy_for_path(&file.record.path)? {
        SystemFamilyPolicyDecisionV1::Ordinary => true,
        SystemFamilyPolicyDecisionV1::StructuralContainer => false,
        SystemFamilyPolicyDecisionV1::Known { policy: IndexPolicyV1::IncludeUnderOrdinaryScope, .. } => true,
        SystemFamilyPolicyDecisionV1::Known {
          policy: IndexPolicyV1::NotApplicable | IndexPolicyV1::ExcludeFromAllIndexes | IndexPolicyV1::CanonicalProjectionOnly,
          ..
        } => false,
      };
      if !indexable || !self.selected_configuration_owner_matches(&file.record.path, &normalized_scope, budget, &mut configurations)? {
        continue;
      }

      let Some((values, transient_bytes)) =
        self.selected_index_values(&configuration, &field, &file, &mut parser_registry, &mut parser_registry_loaded, budget)?
      else {
        continue;
      };
      let file_key = file_path_hash(&file.record.path, &self.engine.hash_algo())?;
      for value in values {
        let insertion_bytes = selected_index_insert_upper_bound(&index, value.len(), file_key.len())?;
        budget.grow_index_load(insertion_bytes, "selected authoritative index growth admission failed")?;
        admitted_index_bytes = admitted_index_bytes
          .checked_add(insertion_bytes)
          .ok_or_else(|| EngineError::ResourceExhausted("selected authoritative index accounting overflow".to_string()))?;
        index.insert_expanded(&value, file_key.clone());
      }
      budget.shrink_index_load(transient_bytes, "selected authoritative document accounting failed")?;
    }

    let retained_bytes = index.estimated_memory_bytes();
    let unused_bytes = admitted_index_bytes
      .checked_sub(retained_bytes)
      .ok_or_else(|| EngineError::ResourceExhausted("selected authoritative index exceeded its admitted memory upper bound".to_string()))?;
    budget.shrink_index_load(unused_bytes, "selected authoritative index accounting failed")?;
    Ok(Some(index))
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

  fn selected_directory_children(&self, path: &str) -> EngineResult<(String, Vec<u8>, Vec<ChildEntry>)> {
    let normalized_path = normalize_path(path);
    let resolved = resolve_directory_at_version(self.engine, self.selected_root(), &normalized_path)?;
    let children = self.decode_directory_children(&resolved.hash, &resolved.header, &resolved.value)?;
    Ok((normalized_path, resolved.hash, children))
  }

  fn selected_child_path(directory_path: &str, child_name: &str) -> EngineResult<String> {
    let path = if directory_path == "/" { format!("/{child_name}") } else { format!("{directory_path}/{child_name}") };
    if child_name.is_empty() || child_name.contains('/') || child_name.contains('\0') || normalize_path(&path) != path {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("selected directory '{directory_path}' contains non-canonical child name '{child_name}'"),
      });
    }
    Ok(path)
  }

  fn selected_directory_entry(&self, directory_path: &str, child: ChildEntry) -> EngineResult<LegacyV3SelectedDirectoryEntryV1> {
    let entry_type = EntryType::from_u8(child.entry_type)?;
    let path = Self::selected_child_path(directory_path, &child.name)?;
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
    traversal: &SelectedDirectoryTraversal<'_>,
    current_path: &str,
    remaining_depth: i32,
    active_directory_hashes: &mut HashSet<Vec<u8>>,
    output: &mut Vec<LegacyV3SelectedDirectoryEntryV1>,
  ) -> EngineResult<()> {
    let (normalized_path, record_hash, children) = self.selected_directory_children(current_path)?;
    if !active_directory_hashes.insert(record_hash.clone()) {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("selected directory cycle at '{normalized_path}' ({})", hex::encode(record_hash)),
      });
    }
    let mut names = HashSet::with_capacity(children.len());
    for child in children {
      if !names.insert(child.name.clone()) {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("selected directory '{normalized_path}' contains duplicate child name '{}'", child.name),
        });
      }
      let child_path = Self::selected_child_path(&normalized_path, &child.name)?;
      traversal.family_policy.policy_for_path(&child_path, "strict selected-root recursive traversal")?;
      let entry = self.selected_directory_entry(&normalized_path, child)?;
      let entry_type = EntryType::from_u8(entry.entry_type)?;
      match entry_type {
        EntryType::FileRecord | EntryType::Symlink => {
          if traversal.glob_pattern.is_none_or(|pattern| listing_glob_matches(pattern, traversal.base_path, &entry.path, &entry.name)) {
            output.push(entry);
          }
        }
        EntryType::DirectoryIndex if !traversal.recursive_mode => {
          if traversal.glob_pattern.is_none_or(|pattern| listing_glob_matches(pattern, traversal.base_path, &entry.path, &entry.name)) {
            output.push(entry);
          }
        }
        EntryType::DirectoryIndex if remaining_depth > 0 || remaining_depth == -1 => {
          let next_depth = if remaining_depth == -1 { -1 } else { remaining_depth - 1 };
          self.collect_directory_entries(traversal, &entry.path, next_depth, active_directory_hashes, output)?;
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
    active_directory_hashes.remove(&record_hash);
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

impl QueryReadSourceV1 for LegacyV3SelectedRootAdapterV1<'_> {
  fn selected_root(&self) -> &[u8] {
    LegacyV3SelectedRootAdapterV1::selected_root(self)
  }

  fn list_indexes(&self, path: &str, budget: &mut QueryMemoryBudget) -> EngineResult<Vec<String>> {
    let indexes_path = Self::query_indexes_directory_path(path);
    let mut names = BTreeSet::new();
    match self.directory(&indexes_path) {
      Ok(directory) => {
        budget.reserve_listing(directory.entries.len() as u64)?;
        for entry in directory.entries {
          budget.record_work(1)?;
          if let Some(name) = entry.name.strip_suffix(".idx") {
            names.insert(name.to_string());
          }
        }
      }
      Err(EngineError::NotFound(_)) => {}
      Err(error) => return Err(error),
    }

    if let Some(configuration) = self.load_selected_index_config(path, budget)? {
      for field in configuration.indexes {
        budget.record_work(1)?;
        let Some(field_name) = canonical_selected_index_field_name(&field.name) else {
          continue;
        };
        if field.index_type == "string" && names.contains(field_name) {
          continue;
        }
        names.insert(format!("{field_name}.{}", field.index_type));
      }
    }
    Ok(names.into_iter().collect())
  }

  fn load_index(&self, path: &str, field_name: &str, budget: &mut QueryMemoryBudget) -> EngineResult<Option<FieldIndex>> {
    for index_name in <Self as QueryReadSourceV1>::list_indexes(self, path, budget)? {
      let prefix = format!("{field_name}.");
      if !index_name.starts_with(&prefix) {
        continue;
      }
      let Some((_field_name, strategy)) = index_name.rsplit_once('.') else {
        continue;
      };
      return <Self as QueryReadSourceV1>::load_index_by_strategy(self, path, field_name, strategy, budget);
    }
    Ok(None)
  }

  fn load_index_by_strategy(
    &self,
    path: &str,
    field_name: &str,
    strategy: &str,
    budget: &mut QueryMemoryBudget,
  ) -> EngineResult<Option<FieldIndex>> {
    self.build_selected_index(path, field_name, strategy, budget)
  }

  fn load_indexes_for_field(&self, path: &str, field_name: &str, budget: &mut QueryMemoryBudget) -> EngineResult<Vec<FieldIndex>> {
    let mut indexes = Vec::new();
    for index_name in <Self as QueryReadSourceV1>::list_indexes(self, path, budget)? {
      let is_matching_field = index_name == field_name || index_name.starts_with(&format!("{field_name}."));
      if !is_matching_field {
        continue;
      }

      let strategy = match index_name.split_once('.') {
        Some((_field_name, strategy)) => strategy,
        None => "string",
      };
      if let Some(index) = <Self as QueryReadSourceV1>::load_index_by_strategy(self, path, field_name, strategy, budget)? {
        indexes.push(index);
      } else if strategy == "string" {
        if let Some(index) = <Self as QueryReadSourceV1>::load_index(self, path, field_name, budget)? {
          indexes.push(index);
        }
      }
    }
    Ok(indexes)
  }

  fn discover_indexed_directories(&self, base_path: &str, budget: &mut QueryMemoryBudget) -> EngineResult<Vec<String>> {
    let normalized_base = normalize_path(base_path);
    let mut indexed_directories = BTreeSet::new();
    if !<Self as QueryReadSourceV1>::list_indexes(self, &normalized_base, budget)?.is_empty() {
      indexed_directories.insert(normalized_base.clone());
    }

    let entries = self.list_directory_recursive_strict(&normalized_base, -1, None)?;
    budget.reserve_listing(entries.len() as u64)?;
    for entry in entries {
      budget.record_work(1)?;
      let Some(indexes_offset) = entry.path.find("/.aeordb-indexes/") else {
        if let Some(config_owner) = entry.path.strip_suffix("/.aeordb-config/indexes.json") {
          indexed_directories.insert(if config_owner.is_empty() { "/".to_string() } else { config_owner.to_string() });
        }
        continue;
      };
      let parent = &entry.path[..indexes_offset];
      if parent.is_empty() {
        indexed_directories.insert("/".to_string());
      } else {
        indexed_directories.insert(parent.to_string());
      }
    }
    Ok(indexed_directories.into_iter().collect())
  }

  fn file_by_hash(&self, record_hash: &[u8], budget: &mut QueryMemoryBudget) -> EngineResult<Option<QuerySourceFileV1>> {
    let header = match self.engine.get_entry_header_including_deleted(record_hash)? {
      Some(header) if header.entry_type == EntryType::FileRecord && !header.is_system_entry() => header,
      Some(_) | None => return Ok(None),
    };
    budget.reserve_file_record_load(header.value_length)?;
    match LegacyV3SelectedRootAdapterV1::file_by_hash(self, record_hash) {
      Ok(file) => {
        let selected_header = self
          .engine
          .get_entry_header_including_deleted(&file.record_hash)?
          .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "selected query FileRecord disappeared".to_string() })?;
        if selected_header.entry_type != EntryType::FileRecord || selected_header.is_system_entry() {
          return Err(EngineError::CorruptEntry { offset: 0, reason: "selected query revision is not a public FileRecord".to_string() });
        }
        if selected_header.value_length > header.value_length {
          budget.reserve_file_record_load(selected_header.value_length - header.value_length)?;
        }
        Ok(Some(QuerySourceFileV1 {
          record_hash: file.record_hash,
          record_value_length: selected_header.value_length,
          record: file.record,
        }))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  fn list_file_records(&self, path: &str, budget: &mut QueryMemoryBudget) -> EngineResult<Vec<QuerySourceFileV1>> {
    let entries = match self.list_directory_recursive_strict(path, -1, None) {
      Ok(entries) => entries,
      Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
      Err(error) => return Err(error),
    };
    budget.reserve_listing(entries.len() as u64)?;
    let mut files = Vec::new();
    for entry in entries {
      budget.record_work(1)?;
      if entry.entry_type != EntryType::FileRecord.to_u8() {
        continue;
      }
      if let Some(file) = <Self as QueryReadSourceV1>::file_by_hash(self, &entry.record_hash, budget)? {
        files.push(file);
      }
    }
    Ok(files)
  }

  fn read_file_body(&self, file: &QuerySourceFileV1) -> EngineResult<Vec<u8>> {
    EngineFileStream::from_chunk_hashes_including_deleted(file.record.chunk_hashes.clone(), self.engine)?.collect_to_vec()
  }
}

fn canonical_selected_index_field_name(field_name: &str) -> Option<&str> {
  match field_name {
    "@path" => Some("@path"),
    "@filename" | "@file_name" => Some("@filename"),
    "@extension" => Some("@extension"),
    "@content_type" => Some("@content_type"),
    "@size" => Some("@size"),
    "@created_at" => Some("@created_at"),
    "@updated_at" => Some("@updated_at"),
    "@hash" => Some("@hash"),
    field_name if !field_name.starts_with('@') => Some(field_name),
    _ => None,
  }
}

fn selected_metadata_field_value(field_name: &str, record: &FileRecord) -> Option<Vec<u8>> {
  let canonical_name = canonical_selected_index_field_name(field_name)?;
  match canonical_name {
    "@path" => Some(record.path.as_bytes().to_vec()),
    "@filename" => Some(selected_file_name(&record.path).as_bytes().to_vec()),
    "@extension" => {
      let filename = selected_file_name(&record.path);
      let extension = match filename.rsplit_once('.') {
        Some((_, extension)) => extension,
        None => "",
      };
      Some(if extension == filename { Vec::new() } else { extension.as_bytes().to_vec() })
    }
    "@content_type" => {
      let mut content_type = "";
      if let Some(selected_content_type) = record.content_type.as_deref() {
        content_type = selected_content_type;
      }
      Some(content_type.as_bytes().to_vec())
    }
    "@size" => Some(record.total_size.to_be_bytes().to_vec()),
    "@created_at" => Some(record.created_at.to_be_bytes().to_vec()),
    "@updated_at" => Some(record.updated_at.to_be_bytes().to_vec()),
    "@hash" => Some(record.content_hash_hex().into_bytes()),
    _ => None,
  }
}

fn selected_index_insert_upper_bound(index: &FieldIndex, value_length: usize, file_key_length: usize) -> EngineResult<u64> {
  let expanded_entries = match index.converter.type_tag() {
    crate::engine::scalar_converter::CONVERTER_TYPE_TRIGRAM => value_length
      .checked_mul(8)
      .and_then(|entries| entries.checked_add(8))
      .ok_or_else(|| EngineError::ResourceExhausted("selected trigram expansion estimate overflow".to_string()))?,
    crate::engine::scalar_converter::CONVERTER_TYPE_PHONETIC => value_length
      .checked_mul(2)
      .and_then(|entries| entries.checked_add(2))
      .ok_or_else(|| EngineError::ResourceExhausted("selected phonetic expansion estimate overflow".to_string()))?,
    _ => 1,
  };
  let per_entry_bytes = std::mem::size_of::<crate::engine::index_store::IndexEntry>()
    .checked_add(file_key_length.saturating_mul(4))
    .and_then(|bytes| bytes.checked_add(1024))
    .ok_or_else(|| EngineError::ResourceExhausted("selected index entry estimate overflow".to_string()))?;
  let bytes = expanded_entries
    .checked_mul(per_entry_bytes)
    .and_then(|bytes| bytes.checked_add(value_length.saturating_mul(16)))
    .and_then(|bytes| bytes.checked_add(64 * 1024))
    .ok_or_else(|| EngineError::ResourceExhausted("selected index insertion estimate overflow".to_string()))?;
  u64::try_from(bytes)
    .map_err(|error| EngineError::ResourceExhausted(format!("selected index insertion estimate does not fit u64: {error}")))
}

fn selected_file_name(path: &str) -> &str {
  if let Some(filename) = file_name(path) {
    return filename;
  }
  ""
}

fn selected_native_document(data: &[u8], content_type: &str, path: &str, size: u64) -> Option<serde_json::Value> {
  let filename = selected_file_name(path);
  match crate::engine::native_parsers::parse_native(data, content_type, filename, path, size) {
    Some(Ok(document)) => Some(document),
    Some(Err(error)) => {
      tracing::debug!(%path, %error, "selected authoritative native parser classified the document as unindexable");
      None
    }
    None => None,
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
