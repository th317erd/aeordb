use std::collections::{HashMap, HashSet};

use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::directory_ops::{directory_content_hash, directory_path_hash, file_content_hash, file_path_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::{SystemFamilyPolicyResolver, TransferPathSelection};
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::v4::system_family::{SystemFamilyClassificationV1, SystemFamilyTransferOperationV1};

const COLLECTION_ENTRY_OVERHEAD: u64 = 64;
const TREE_SCRATCH_OVERHEAD: u64 = 1024;

pub(crate) trait HistoricalEntrySource {
  fn hash_algo(&self) -> HashAlgorithm;

  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<crate::engine::entry_header::EntryHeader>>;

  fn historical_entry_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<HistoricalEntry>>;
}

pub(crate) type HistoricalEntry = (crate::engine::entry_header::EntryHeader, Vec<u8>, Vec<u8>);
type DirectoryStateMap = HashMap<String, (Vec<u8>, Vec<u8>)>;

impl HistoricalEntrySource for StorageEngine {
  fn hash_algo(&self) -> HashAlgorithm {
    StorageEngine::hash_algo(self)
  }

  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<crate::engine::entry_header::EntryHeader>> {
    self.get_entry_header_including_deleted(hash)
  }

  fn historical_entry_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<HistoricalEntry>> {
    self.get_entry_including_deleted_verified_bounded(hash, maximum_value_length)
  }
}

/// The complete tree state at a version: all files, directories, symlinks, and chunk hashes.
#[derive(Debug, Clone)]
pub struct VersionTree {
  /// All files: path -> (file_hash, FileRecord)
  pub files: HashMap<String, (Vec<u8>, FileRecord)>,
  /// All directory entries: path -> (dir_hash, raw_data)
  pub directories: HashMap<String, (Vec<u8>, Vec<u8>)>,
  /// Content-addressed B-tree branch and leaf nodes required by directories.
  pub btree_nodes: HashMap<Vec<u8>, Vec<u8>>,
  /// All chunk hashes referenced by any file in the tree
  pub chunks: HashSet<Vec<u8>>,
  /// All symlinks: path -> (symlink_hash, SymlinkRecord)
  pub symlinks: HashMap<String, (Vec<u8>, SymlinkRecord)>,
}

impl Default for VersionTree {
  fn default() -> Self {
    Self::new()
  }
}

impl VersionTree {
  pub fn new() -> Self {
    VersionTree {
      files: HashMap::new(),
      directories: HashMap::new(),
      btree_nodes: HashMap::new(),
      chunks: HashSet::new(),
      symlinks: HashMap::new(),
    }
  }
}

/// Walk a version's directory tree starting from a root hash.
/// Collects all files, directories, and chunk hashes reachable from the root.
///
/// Uses a visited set for cycle detection: if corrupted data creates a
/// circular reference (directory A contains directory B which contains A),
/// the walk terminates that branch instead of recursing infinitely.
pub fn walk_version_tree(engine: &StorageEngine, root_hash: &[u8]) -> EngineResult<VersionTree> {
  let mut control = TreeWalkControl::unbounded();
  walk_version_tree_controlled(engine, root_hash, &mut control, &mut |_| Ok(TransferPathSelection::Include))
}

pub(crate) fn walk_version_tree_for_transfer_with_budget(
  engine: &StorageEngine,
  root_hash: &[u8],
  operation: SystemFamilyTransferOperationV1,
  include_detached_current_state: bool,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<VersionTree> {
  walk_version_tree_for_transfer_from_source_with_budget(engine, root_hash, operation, include_detached_current_state, budget)
}

pub(crate) fn walk_version_tree_for_transfer_from_source_with_budget<S: HistoricalEntrySource + ?Sized>(
  source: &S,
  root_hash: &[u8],
  operation: SystemFamilyTransferOperationV1,
  include_detached_current_state: bool,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<VersionTree> {
  let resolver = SystemFamilyPolicyResolver::new(source.hash_algo())?;
  let mut control = TreeWalkControl::accounted(budget, true);
  walk_version_tree_for_transfer_controlled(source, root_hash, resolver, operation, include_detached_current_state, &mut control)
}

fn walk_version_tree_for_transfer_controlled<S: HistoricalEntrySource + ?Sized>(
  source: &S,
  root_hash: &[u8],
  resolver: SystemFamilyPolicyResolver,
  operation: SystemFamilyTransferOperationV1,
  include_detached_current_state: bool,
  control: &mut TreeWalkControl<'_>,
) -> EngineResult<VersionTree> {
  let mut path_filter = |path: &str| resolver.transfer_path_selection(path, operation);
  let mut tree = walk_version_tree_controlled(source, root_hash, control, &mut path_filter)?;
  if include_detached_current_state {
    collect_detached_transfer_paths(source, &mut tree, resolver, operation, control, &mut path_filter)?;
  }
  Ok(tree)
}

fn walk_version_tree_controlled<S, F>(
  source: &S,
  root_hash: &[u8],
  control: &mut TreeWalkControl<'_>,
  path_filter: &mut F,
) -> EngineResult<VersionTree>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  let mut tree = VersionTree::new();
  let hash_length = source.hash_algo().hash_length();
  walk_directory(source, root_hash, "/", hash_length, &mut tree, control, path_filter)?;
  Ok(tree)
}

/// Walk a subtree rooted at a given path and add it to an existing tree.
pub fn walk_subtree(engine: &StorageEngine, start_path: &str, start_dir_hash: &[u8], tree: &mut VersionTree) -> EngineResult<()> {
  let hash_length = engine.hash_algo().hash_length();
  let mut control = TreeWalkControl::unbounded();
  walk_directory(engine, start_dir_hash, start_path, hash_length, tree, &mut control, &mut |_| Ok(TransferPathSelection::Include))
}

/// Add registry-selected state that is deliberately detached from HEAD.
///
/// Candidate roots come from the immutable registry. Structural ancestors are
/// inspected so path listings remain coherent, and every descendant is
/// classified before its FileRecord, symlink, or directory body is read.
fn collect_detached_transfer_paths<S, F>(
  source: &S,
  tree: &mut VersionTree,
  resolver: SystemFamilyPolicyResolver,
  operation: SystemFamilyTransferOperationV1,
  control: &mut TreeWalkControl<'_>,
  path_filter: &mut F,
) -> EngineResult<()>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  let mut candidates = std::collections::BTreeMap::<String, bool>::new();
  for candidate in resolver.included_absolute_paths(operation)? {
    candidates.entry(candidate.path.clone()).and_modify(|is_prefix| *is_prefix |= candidate.is_prefix).or_insert(candidate.is_prefix);
    let mut ancestor = candidate.path.as_str();
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
      if parent.is_empty() {
        break;
      }
      if matches!(resolver.classify_path(parent)?, SystemFamilyClassificationV1::StructuralContainer) {
        candidates.entry(parent.to_string()).or_insert(true);
      }
      ancestor = parent;
    }
  }

  let algorithm = source.hash_algo();
  let hash_length = algorithm.hash_length();
  for (path, is_prefix) in candidates {
    control.record_work(1)?;
    if !resolver.transfer_path_is_included(&path, operation)? {
      continue;
    }

    if !is_prefix && !tree.files.contains_key(&path) {
      let file_key = file_path_hash(&path, &algorithm)?;
      if let Some(((header, _key, raw), loaded_charge)) = load_historical_entry(source, &file_key, control)? {
        if header.entry_type != EntryType::FileRecord {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("transfer path '{}' resolved to {:?} instead of FileRecord", path, header.entry_type),
          });
        }
        let record = FileRecord::deserialize(&raw, hash_length, header.entry_version).map_err(|error| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("transfer FileRecord '{}' is malformed: {error}", path),
        })?;
        let content_hash = file_content_hash(&raw, &algorithm)?;
        let retained_bytes = path
          .len()
          .checked_add(content_hash.len())
          .and_then(|bytes| bytes.checked_add(raw.len()))
          .ok_or_else(|| EngineError::ResourceExhausted("detached transfer file estimate overflow".to_string()))?;
        control.reserve(collection_charge(retained_bytes, 0)?, "detached transfer file admission failed")?;
        for chunk_hash in &record.chunk_hashes {
          if tree.chunks.insert(chunk_hash.clone()) {
            control.reserve(collection_charge(0, chunk_hash.len())?, "detached transfer chunk set admission failed")?;
          }
        }
        tree.files.insert(path.clone(), (content_hash, record));
        control.release(loaded_charge, "detached transfer FileRecord buffer release failed")?;
      }
    }

    if tree.directories.contains_key(&path) {
      continue;
    }
    let directory_key = directory_path_hash(&path, &algorithm)?;
    let Some(((header, _key, raw), loaded_charge)) = load_historical_entry(source, &directory_key, control)? else {
      continue;
    };
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("transfer directory '{}' resolved to {:?} instead of DirectoryIndex", path, header.entry_type),
      });
    }
    let directory_hash = if raw.len() == hash_length { raw.clone() } else { directory_content_hash(&raw, &algorithm)? };
    control.release(loaded_charge, "detached transfer directory path-entry buffer release failed")?;
    walk_directory(source, &directory_hash, &path, hash_length, tree, control, path_filter)?;
  }
  Ok(())
}

/// Recursively walk a directory and its children.
///
/// The `visited` set tracks directory hashes already traversed to prevent
/// infinite recursion on corrupted data that contains cycles.
struct TreeWalkControl<'a> {
  budget: Option<&'a mut OperationMemoryBudget>,
  strict_missing: bool,
  strict_root: bool,
}

impl<'a> TreeWalkControl<'a> {
  fn unbounded() -> Self {
    Self { budget: None, strict_missing: false, strict_root: false }
  }

  fn accounted(budget: &'a mut OperationMemoryBudget, strict_root: bool) -> Self {
    Self { budget: Some(budget), strict_missing: true, strict_root }
  }

  fn reserve(&mut self, bytes: u64, context: &'static str) -> EngineResult<()> {
    if let Some(budget) = self.budget.as_deref_mut() {
      budget.reserve(bytes, context)?;
    }
    Ok(())
  }

  fn release(&mut self, bytes: u64, context: &'static str) -> EngineResult<()> {
    if let Some(budget) = self.budget.as_deref_mut() {
      budget.release(bytes, context)?;
    }
    Ok(())
  }

  fn record_work(&mut self, units: usize) -> EngineResult<()> {
    if let Some(budget) = self.budget.as_deref_mut() {
      budget.record_work(units)?;
    }
    Ok(())
  }
}

struct PendingDirectory {
  hash: Vec<u8>,
  path: String,
  retained_charge: u64,
}

enum PendingDirectoryWork {
  Enter(PendingDirectory),
  Exit { hash: Vec<u8>, active_charge: u64 },
}

struct DirectoryVisitState<'state, 'budget> {
  hash_length: usize,
  tree: &'state mut VersionTree,
  pending: &'state mut Vec<PendingDirectoryWork>,
  control: &'state mut TreeWalkControl<'budget>,
}

fn walk_directory<S, F>(
  source: &S,
  dir_hash: &[u8],
  current_path: &str,
  hash_length: usize,
  tree: &mut VersionTree,
  control: &mut TreeWalkControl<'_>,
  path_filter: &mut F,
) -> EngineResult<()>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  let initial_charge = collection_charge(current_path.len(), dir_hash.len())?;
  control.reserve(initial_charge, "directory frontier admission failed")?;
  let mut pending = vec![PendingDirectoryWork::Enter(PendingDirectory {
    hash: dir_hash.to_vec(),
    path: current_path.to_string(),
    retained_charge: initial_charge,
  })];
  let mut active = HashSet::new();

  while let Some(work) = pending.pop() {
    let directory = match work {
      PendingDirectoryWork::Enter(directory) => directory,
      PendingDirectoryWork::Exit { hash, active_charge } => {
        if !active.remove(&hash) {
          return Err(EngineError::IoError(std::io::Error::other("directory traversal active-set exit was not owned")));
        }
        control.release(active_charge, "directory ancestry release failed")?;
        continue;
      }
    };
    control.record_work(1)?;
    if active.contains(&directory.hash) {
      control.release(directory.retained_charge, "directory frontier release failed")?;
      if control.strict_missing {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("directory cycle at '{}' ({})", directory.path, hex::encode(&directory.hash)),
        });
      }
      continue;
    }
    let active_charge = collection_charge(0, directory.hash.len())?;
    control.reserve(active_charge, "directory ancestry admission failed")?;
    active.insert(directory.hash.clone());

    let Some(((header, _key, dir_data), loaded_charge)) = load_historical_entry(source, &directory.hash, control)? else {
      active.remove(&directory.hash);
      control.release(active_charge, "missing directory ancestry release failed")?;
      control.release(directory.retained_charge, "missing directory frontier release failed")?;
      if control.strict_missing && (control.strict_root || directory.path != current_path) {
        return Err(missing_tree_entry("directory", &directory.path, &directory.hash));
      }
      continue;
    };
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("directory '{}' resolved to {:?} instead of DirectoryIndex", directory.path, header.entry_type),
      });
    }

    pending.push(PendingDirectoryWork::Exit { hash: directory.hash.clone(), active_charge });
    if !dir_data.is_empty() {
      visit_directory_children(
        source,
        &directory.path,
        &dir_data,
        &mut DirectoryVisitState { hash_length, tree, pending: &mut pending, control },
        path_filter,
      )?;
    }
    tree.directories.insert(directory.path, (directory.hash, dir_data));
    let _retained_charge = directory.retained_charge.saturating_add(loaded_charge);
  }
  Ok(())
}

fn visit_directory_children<S, F>(
  source: &S,
  current_path: &str,
  dir_data: &[u8],
  state: &mut DirectoryVisitState<'_, '_>,
  path_filter: &mut F,
) -> EngineResult<()>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  if crate::engine::btree::is_btree_format(dir_data) {
    visit_btree_children(source, current_path, dir_data, state, path_filter)
  } else {
    let scratch = scratch_charge(dir_data.len())?;
    state.control.reserve(scratch, "flat directory parse admission failed")?;
    let children = deserialize_child_entries(dir_data, state.hash_length, 0)?;
    for child in &children {
      process_child(source, current_path, child, state, path_filter)?;
    }
    state.control.release(scratch, "flat directory parse release failed")
  }
}

fn visit_btree_children<S, F>(
  source: &S,
  current_path: &str,
  root_data: &[u8],
  state: &mut DirectoryVisitState<'_, '_>,
  path_filter: &mut F,
) -> EngineResult<()>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  let mut node_hashes: Vec<(Vec<u8>, u64)> = Vec::new();
  let mut visited_node_hashes = HashSet::new();
  let mut visited_node_charge = 0u64;
  process_btree_node(
    source,
    current_path,
    root_data,
    0,
    state.hash_length,
    state.tree,
    state.pending,
    &mut node_hashes,
    state.control,
    path_filter,
  )?;
  while let Some((node_hash, frontier_charge)) = node_hashes.pop() {
    state.control.record_work(1)?;
    state.control.release(frontier_charge, "B-tree frontier release failed")?;
    if visited_node_hashes.contains(&node_hash) {
      if state.control.strict_missing {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("B-tree cycle or duplicate node {} under '{}'", hex::encode(&node_hash), current_path),
        });
      }
      continue;
    }
    let seen_charge = collection_charge(0, node_hash.len())?;
    state.control.reserve(seen_charge, "B-tree visited-set admission failed")?;
    visited_node_charge = visited_node_charge
      .checked_add(seen_charge)
      .ok_or_else(|| EngineError::ResourceExhausted("B-tree visited-set accounting overflow".to_string()))?;
    visited_node_hashes.insert(node_hash.clone());
    let Some(((header, _key, node_data), loaded_charge)) = load_historical_entry(source, &node_hash, state.control)? else {
      return Err(missing_tree_entry("B-tree node", current_path, &node_hash));
    };
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("B-tree node {} resolved to {:?}", hex::encode(&node_hash), header.entry_type),
      });
    }
    process_btree_node(
      source,
      current_path,
      &node_data,
      header.entry_version,
      state.hash_length,
      state.tree,
      state.pending,
      &mut node_hashes,
      state.control,
      path_filter,
    )?;
    if let std::collections::hash_map::Entry::Vacant(e) = state.tree.btree_nodes.entry(node_hash) {
      e.insert(node_data);
      let _retained_charge = loaded_charge;
    } else {
      state.control.release(loaded_charge, "shared B-tree node buffer release failed")?;
    }
  }
  state.control.release(visited_node_charge, "B-tree visited-set release failed")
}

#[allow(clippy::too_many_arguments)]
fn process_btree_node<S, F>(
  source: &S,
  current_path: &str,
  node_data: &[u8],
  entry_version: u8,
  hash_length: usize,
  tree: &mut VersionTree,
  pending: &mut Vec<PendingDirectoryWork>,
  node_hashes: &mut Vec<(Vec<u8>, u64)>,
  control: &mut TreeWalkControl<'_>,
  path_filter: &mut F,
) -> EngineResult<()>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  let scratch = scratch_charge(node_data.len())?;
  control.reserve(scratch, "B-tree parse admission failed")?;
  match crate::engine::btree::BTreeNode::deserialize(node_data, hash_length, entry_version)? {
    crate::engine::btree::BTreeNode::Leaf(leaf) => {
      for child in &leaf.entries {
        process_child(source, current_path, child, &mut DirectoryVisitState { hash_length, tree, pending, control }, path_filter)?;
      }
    }
    crate::engine::btree::BTreeNode::Internal(internal) => {
      for child_hash in internal.children.iter().rev() {
        let charge = collection_charge(0, child_hash.len())?;
        control.reserve(charge, "B-tree frontier admission failed")?;
        node_hashes.push((child_hash.clone(), charge));
      }
    }
  }
  control.release(scratch, "B-tree parse release failed")
}

fn process_child<S, F>(
  source: &S,
  current_path: &str,
  child: &crate::engine::directory_entry::ChildEntry,
  state: &mut DirectoryVisitState<'_, '_>,
  path_filter: &mut F,
) -> EngineResult<()>
where
  S: HistoricalEntrySource + ?Sized,
  F: FnMut(&str) -> EngineResult<TransferPathSelection>,
{
  state.control.record_work(1)?;
  let path_len = joined_path_len(current_path, child.name.len())?;
  let child_path = join_child_path(current_path, &child.name);
  let selection = path_filter(&child_path)?;
  if selection == TransferPathSelection::Omit {
    return Ok(());
  }
  let child_entry_type = EntryType::from_u8(child.entry_type)?;
  if selection == TransferPathSelection::StructuralContainer && child_entry_type != EntryType::DirectoryIndex {
    return Err(EngineError::SystemFamilyPolicy {
      code: "system_family_structural_leaf",
      reason: format!("transfer traversal uses structural container '{}' as {:?}", child_path, child_entry_type),
    });
  }
  match child_entry_type {
    EntryType::DirectoryIndex => {
      let charge = collection_charge(path_len, child.hash.len())?;
      state.control.reserve(charge, "directory frontier admission failed")?;
      state.pending.push(PendingDirectoryWork::Enter(PendingDirectory {
        hash: child.hash.clone(),
        path: child_path,
        retained_charge: charge,
      }));
    }
    EntryType::FileRecord => {
      let map_charge = collection_charge(path_len, child.hash.len())?;
      state.control.reserve(map_charge, "file tree admission failed")?;
      let Some(((header, _key, value), _loaded_charge)) = load_historical_entry(source, &child.hash, state.control)? else {
        state.control.release(map_charge, "missing file tree release failed")?;
        if state.control.strict_missing {
          return Err(missing_tree_entry("file", &child_path, &child.hash));
        }
        return Ok(());
      };
      if header.entry_type != EntryType::FileRecord {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("file '{}' resolved to {:?} instead of FileRecord", child_path, header.entry_type),
        });
      }
      let file_record = FileRecord::deserialize(&value, state.hash_length, header.entry_version)
        .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("FileRecord '{}' is malformed: {error}", child_path) })?;
      if state.control.strict_missing && file_record.path != child_path {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("FileRecord path '{}' does not match traversed path '{}'", file_record.path, child_path),
        });
      }
      for chunk_hash in &file_record.chunk_hashes {
        if !state.tree.chunks.contains(chunk_hash) {
          let charge = collection_charge(0, chunk_hash.len())?;
          state.control.reserve(charge, "chunk set admission failed")?;
          state.tree.chunks.insert(chunk_hash.clone());
        }
      }
      state.tree.files.insert(child_path, (child.hash.clone(), file_record));
    }
    EntryType::Symlink => {
      let map_charge = collection_charge(path_len, child.hash.len())?;
      state.control.reserve(map_charge, "symlink tree admission failed")?;
      let Some(((header, _key, value), _loaded_charge)) = load_historical_entry(source, &child.hash, state.control)? else {
        state.control.release(map_charge, "missing symlink tree release failed")?;
        if state.control.strict_missing {
          return Err(missing_tree_entry("symlink", &child_path, &child.hash));
        }
        return Ok(());
      };
      if header.entry_type != EntryType::Symlink {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("symlink '{}' resolved to {:?} instead of Symlink", child_path, header.entry_type),
        });
      }
      let symlink_record = SymlinkRecord::deserialize(&value, header.entry_version)
        .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("symlink '{}' is malformed: {error}", child_path) })?;
      if state.control.strict_missing && symlink_record.path != child_path {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("Symlink path '{}' does not match traversed path '{}'", symlink_record.path, child_path),
        });
      }
      state.tree.symlinks.insert(child_path, (child.hash.clone(), symlink_record));
    }
    _ => {}
  }
  Ok(())
}

type AccountedEntry = ((crate::engine::entry_header::EntryHeader, Vec<u8>, Vec<u8>), u64);

fn load_historical_entry<S: HistoricalEntrySource + ?Sized>(
  source: &S,
  hash: &[u8],
  control: &mut TreeWalkControl<'_>,
) -> EngineResult<Option<AccountedEntry>> {
  let Some(header) = source.historical_entry_header(hash)? else {
    return Ok(None);
  };
  let charge = u64::from(header.key_length)
    .checked_add(u64::from(header.value_length))
    .and_then(|bytes| bytes.checked_add(header.header_size() as u64))
    .and_then(|bytes| bytes.checked_add(COLLECTION_ENTRY_OVERHEAD))
    .ok_or_else(|| EngineError::ResourceExhausted("tree entry allocation estimate overflow".to_string()))?;
  control.reserve(charge, "tree entry buffer admission failed")?;
  match source.historical_entry_bounded(hash, header.value_length) {
    Ok(Some(entry)) => Ok(Some((entry, charge))),
    Ok(None) => {
      control.release(charge, "missing tree entry buffer release failed")?;
      Ok(None)
    }
    Err(error) => {
      control.release(charge, "failed tree entry buffer release failed")?;
      Err(error)
    }
  }
}

fn collection_charge(first_bytes: usize, second_bytes: usize) -> EngineResult<u64> {
  u64::try_from(first_bytes)
    .ok()
    .and_then(|first| u64::try_from(second_bytes).ok().and_then(|second| first.checked_add(second)))
    .and_then(|bytes| bytes.checked_add(COLLECTION_ENTRY_OVERHEAD))
    .ok_or_else(|| EngineError::ResourceExhausted("tree collection allocation estimate overflow".to_string()))
}

fn scratch_charge(bytes: usize) -> EngineResult<u64> {
  u64::try_from(bytes)
    .ok()
    .and_then(|bytes| bytes.checked_mul(4))
    .and_then(|bytes| bytes.checked_add(TREE_SCRATCH_OVERHEAD))
    .ok_or_else(|| EngineError::ResourceExhausted("tree parse scratch estimate overflow".to_string()))
}

fn joined_path_len(parent: &str, child_len: usize) -> EngineResult<usize> {
  parent
    .len()
    .checked_add(child_len)
    .and_then(|length| length.checked_add(if parent == "/" { 0 } else { 1 }))
    .ok_or_else(|| EngineError::ResourceExhausted("tree path length overflow".to_string()))
}

fn join_child_path(parent: &str, child: &str) -> String {
  if parent == "/" {
    format!("/{child}")
  } else {
    format!("{parent}/{child}")
  }
}

fn missing_tree_entry(kind: &str, path: &str, hash: &[u8]) -> EngineError {
  EngineError::CorruptEntry { offset: 0, reason: format!("{kind} '{path}' references missing entry {}", hex::encode(hash)) }
}

/// The result of comparing two version trees.
#[derive(Debug, Clone)]
pub struct TreeDiff {
  /// Files added (path -> (file_hash, FileRecord))
  pub added: HashMap<String, (Vec<u8>, FileRecord)>,
  /// Files modified (path -> (new_file_hash, new FileRecord))
  pub modified: HashMap<String, (Vec<u8>, FileRecord)>,
  /// Files deleted (paths)
  pub deleted: Vec<String>,
  /// Chunks that exist in target but not in base
  pub new_chunks: HashSet<Vec<u8>>,
  /// Directories that were added or changed
  pub changed_directories: HashMap<String, (Vec<u8>, Vec<u8>)>,
  /// Symlinks added
  pub symlinks_added: HashMap<String, (Vec<u8>, SymlinkRecord)>,
  /// Symlinks modified (target changed)
  pub symlinks_modified: HashMap<String, (Vec<u8>, SymlinkRecord)>,
  /// Symlinks deleted
  pub symlinks_deleted: Vec<String>,
}

impl TreeDiff {
  pub fn is_empty(&self) -> bool {
    self.added.is_empty()
      && self.modified.is_empty()
      && self.deleted.is_empty()
      && self.symlinks_added.is_empty()
      && self.symlinks_modified.is_empty()
      && self.symlinks_deleted.is_empty()
  }
}

/// Compute the diff between two version trees.
/// Returns a TreeDiff with added, modified, deleted files and new chunks.
pub fn diff_trees(base: &VersionTree, target: &VersionTree) -> TreeDiff {
  let mut control = TreeWalkControl::unbounded();
  diff_trees_controlled(base, target, &mut control).expect("unbounded tree diff cannot fail memory admission")
}

pub(crate) fn diff_trees_with_budget(
  base: &VersionTree,
  target: &VersionTree,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<TreeDiff> {
  let mut control = TreeWalkControl::accounted(budget, false);
  diff_trees_controlled(base, target, &mut control)
}

fn diff_trees_controlled(base: &VersionTree, target: &VersionTree, control: &mut TreeWalkControl<'_>) -> EngineResult<TreeDiff> {
  let mut added = HashMap::new();
  let mut modified = HashMap::new();
  let mut deleted = Vec::new();

  // Files in target but not base -> added
  // Files in both but different content -> modified
  // Note: file hashes are path-based (deterministic per path), so we compare
  // chunk_hashes to detect actual content changes.
  for (path, (target_hash, target_record)) in &target.files {
    control.record_work(1)?;
    match base.files.get(path) {
      None => {
        control.reserve(file_diff_charge(path, target_hash, target_record)?, "added-file diff admission failed")?;
        added.insert(path.clone(), (target_hash.clone(), target_record.clone()));
      }
      Some((_, base_record)) => {
        if base_record.chunk_hashes != target_record.chunk_hashes {
          control.reserve(file_diff_charge(path, target_hash, target_record)?, "modified-file diff admission failed")?;
          modified.insert(path.clone(), (target_hash.clone(), target_record.clone()));
        }
      }
    }
  }

  // Files in base but not target -> deleted
  for path in base.files.keys() {
    control.record_work(1)?;
    if !target.files.contains_key(path) {
      control.reserve(collection_charge(path.len(), 0)?, "deleted-file diff admission failed")?;
      deleted.push(path.clone());
    }
  }

  // New chunks: chunks in target tree but not in base tree
  let mut new_chunks = HashSet::new();
  for chunk in target.chunks.difference(&base.chunks) {
    control.record_work(1)?;
    control.reserve(collection_charge(0, chunk.len())?, "new-chunk diff admission failed")?;
    new_chunks.insert(chunk.clone());
  }

  // Changed directories
  let changed_directories = diff_directories(&base.directories, &target.directories, control)?;

  // Symlink diffs
  let mut symlinks_added = HashMap::new();
  let mut symlinks_modified = HashMap::new();
  let mut symlinks_deleted = Vec::new();

  for (path, (target_hash, target_record)) in &target.symlinks {
    control.record_work(1)?;
    match base.symlinks.get(path) {
      None => {
        control.reserve(symlink_diff_charge(path, target_hash, target_record)?, "added-symlink diff admission failed")?;
        symlinks_added.insert(path.clone(), (target_hash.clone(), target_record.clone()));
      }
      Some((_, base_record)) => {
        if base_record.target != target_record.target {
          control.reserve(symlink_diff_charge(path, target_hash, target_record)?, "modified-symlink diff admission failed")?;
          symlinks_modified.insert(path.clone(), (target_hash.clone(), target_record.clone()));
        }
      }
    }
  }

  for path in base.symlinks.keys() {
    control.record_work(1)?;
    if !target.symlinks.contains_key(path) {
      control.reserve(collection_charge(path.len(), 0)?, "deleted-symlink diff admission failed")?;
      symlinks_deleted.push(path.clone());
    }
  }

  Ok(TreeDiff { added, modified, deleted, new_chunks, changed_directories, symlinks_added, symlinks_modified, symlinks_deleted })
}

/// Find directories that changed between base and target.
/// Compares raw data (not hash) because directory hashes are path-based.
fn diff_directories(
  base: &DirectoryStateMap,
  target: &DirectoryStateMap,
  control: &mut TreeWalkControl<'_>,
) -> EngineResult<DirectoryStateMap> {
  let mut changed = HashMap::new();
  for (path, (target_hash, target_data)) in target {
    control.record_work(1)?;
    match base.get(path) {
      None => {
        control.reserve(directory_diff_charge(path, target_hash, target_data)?, "added-directory diff admission failed")?;
        changed.insert(path.clone(), (target_hash.clone(), target_data.clone()));
      }
      Some((_, base_data)) => {
        if base_data != target_data {
          control.reserve(directory_diff_charge(path, target_hash, target_data)?, "changed-directory diff admission failed")?;
          changed.insert(path.clone(), (target_hash.clone(), target_data.clone()));
        }
      }
    }
  }
  Ok(changed)
}

fn file_diff_charge(path: &str, hash: &[u8], record: &FileRecord) -> EngineResult<u64> {
  let chunk_bytes = record
    .chunk_hashes
    .iter()
    .try_fold(0usize, |total, chunk| total.checked_add(chunk.len()).and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<u8>>())))
    .ok_or_else(|| EngineError::ResourceExhausted("file diff chunk estimate overflow".to_string()))?;
  let bytes = path
    .len()
    .checked_add(hash.len())
    .and_then(|bytes| bytes.checked_add(record.path.len()))
    .and_then(|bytes| bytes.checked_add(record.content_type.as_ref().map_or(0, String::len)))
    .and_then(|bytes| bytes.checked_add(record.metadata.len()))
    .and_then(|bytes| bytes.checked_add(record.content_hash.len()))
    .and_then(|bytes| bytes.checked_add(chunk_bytes))
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<FileRecord>()))
    .ok_or_else(|| EngineError::ResourceExhausted("file diff allocation estimate overflow".to_string()))?;
  collection_charge(bytes, 0)
}

fn symlink_diff_charge(path: &str, hash: &[u8], record: &SymlinkRecord) -> EngineResult<u64> {
  let bytes = path
    .len()
    .checked_add(hash.len())
    .and_then(|bytes| bytes.checked_add(record.path.len()))
    .and_then(|bytes| bytes.checked_add(record.target.len()))
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<SymlinkRecord>()))
    .ok_or_else(|| EngineError::ResourceExhausted("symlink diff allocation estimate overflow".to_string()))?;
  collection_charge(bytes, 0)
}

fn directory_diff_charge(path: &str, hash: &[u8], data: &[u8]) -> EngineResult<u64> {
  let bytes = path
    .len()
    .checked_add(hash.len())
    .and_then(|bytes| bytes.checked_add(data.len()))
    .ok_or_else(|| EngineError::ResourceExhausted("directory diff allocation estimate overflow".to_string()))?;
  collection_charge(bytes, 0)
}
