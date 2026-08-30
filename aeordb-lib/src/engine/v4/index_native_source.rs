//! Production source adapters for the native v4 index runtime.

use std::cmp::Ordering;
use std::mem::size_of;

use crate::engine::btree::{BTREE_CONVERSION_THRESHOLD, BTREE_MAX_INTERNAL_KEYS, BTREE_MAX_LEAF_ENTRIES, BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::entry_header::EntryHeader;
use crate::engine::errors::EngineError;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::{CompressionAlgorithm, EntryType};

use super::index_producer_source::{
  IndexFileRevisionReadErrorClassV1, IndexFileRevisionReadErrorV1, IndexFileRevisionReadV1, IndexFileRevisionSourceV1,
  LoadedIndexFileRevisionV1,
};
use super::index_maintenance_scan::{
  IndexMaintenanceScanDocumentV1, IndexMaintenanceScanErrorV1, IndexMaintenanceScanPageV1, IndexMaintenanceScanReadErrorV1,
  IndexMaintenanceScanReadV1, IndexMaintenanceScanRequestV1, IndexMaintenanceScanSourceV1, index_maintenance_scan_page_retained_bytes_v1,
  validate_index_maintenance_scan_request_v1,
};

const NATIVE_REVISION_ALLOCATION_MULTIPLIER: u64 = 4;
const NATIVE_REVISION_FIXED_BYTES: u64 = 1_024;
const NATIVE_SCAN_FIXED_WORKSPACE_BYTES: u64 = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeIndexSourceLimitsV1 {
  maximum_file_record_bytes: u32,
  maximum_directory_entity_bytes: u32,
  maximum_btree_depth: u16,
}

impl NativeIndexSourceLimitsV1 {
  pub fn new(
    maximum_file_record_bytes: u32,
    maximum_directory_entity_bytes: u32,
    maximum_btree_depth: u16,
  ) -> Result<Self, IndexFileRevisionReadErrorV1> {
    if maximum_file_record_bytes == 0 || maximum_directory_entity_bytes == 0 || maximum_btree_depth == 0 {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_options",
        "file-record, directory-entity, and B-tree-depth limits must be nonzero",
      ));
    }
    Ok(Self { maximum_file_record_bytes, maximum_directory_entity_bytes, maximum_btree_depth })
  }
}

pub struct NativeIndexFileRevisionSourceV1<'engine> {
  engine: &'engine StorageEngine,
  limits: NativeIndexSourceLimitsV1,
}

impl<'engine> NativeIndexFileRevisionSourceV1<'engine> {
  pub const fn new(engine: &'engine StorageEngine, limits: NativeIndexSourceLimitsV1) -> Self {
    Self { engine, limits }
  }

  fn resolve_entry_reference(
    &self,
    namespace_root: &[u8],
    path: &str,
  ) -> Result<Option<(Vec<u8>, EntryType)>, IndexFileRevisionReadErrorV1> {
    let hash_width = self.engine.hash_algo().hash_length();
    let ancestry_item_bytes = size_of::<Vec<u8>>()
      .checked_add(hash_width)
      .ok_or_else(|| IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", "B-tree ancestry item charge overflowed"))?;
    let ancestry_bytes = usize::from(self.limits.maximum_btree_depth)
      .checked_mul(ancestry_item_bytes)
      .ok_or_else(|| IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", "B-tree ancestry charge overflowed"))?;
    let ancestry_bytes = u64::try_from(ancestry_bytes).map_err(|error| {
      IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", format!("B-tree ancestry charge does not fit u64: {error}"))
    })?;
    let path_bytes = u64::try_from(path.len()).map_err(|error| {
      IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", format!("path charge does not fit u64: {error}"))
    })?;
    let hash_bytes = u64::try_from(hash_width)
      .map_err(|error| {
        IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", format!("hash width does not fit u64: {error}"))
      })?
      .checked_mul(3)
      .ok_or_else(|| IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", "hash traversal charge overflowed"))?;
    let transient_bytes = path_bytes
      .checked_add(hash_bytes)
      .and_then(|bytes| bytes.checked_add(ancestry_bytes))
      .and_then(|bytes| bytes.checked_add(NATIVE_REVISION_FIXED_BYTES))
      .ok_or_else(|| IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", "path traversal charge overflowed"))?;
    let _transient = self
      .engine
      .memory_coordinator()
      .reserve(MemoryOwner::Task, transient_bytes, AdmissionClass::Maintenance)
      .map_err(map_memory_error)?;

    let mut directory_hash = namespace_root.to_vec();
    let mut segments = path.split('/').filter(|segment| !segment.is_empty()).peekable();
    while let Some(segment) = segments.next() {
      let child = self.load_directory_child(&directory_hash, segment, path)?;
      let Some((child_hash, entry_type)) = child else {
        return Ok(None);
      };
      if segments.peek().is_none() {
        return Ok(Some((child_hash, entry_type)));
      }
      if entry_type != EntryType::DirectoryIndex {
        return Ok(None);
      }
      directory_hash = child_hash;
    }
    Ok(None)
  }

  fn load_file_reference(&self, revision_hash: Vec<u8>, path: &str) -> Result<IndexFileRevisionReadV1, IndexFileRevisionReadErrorV1> {
    let hash_width = self.engine.hash_algo().hash_length();
    let header = self.required_header(&revision_hash, EntryType::FileRecord, self.limits.maximum_file_record_bytes, path)?;
    let reservation = self.reserve_decoded(header.value_length, "file revision")?;
    let (actual, key, value) = self.required_bounded_entry(&revision_hash, &header, path)?;
    if key != revision_hash || !same_header(&header, &actual) {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_file_changed",
        format!("file revision {} changed during the bounded read for '{path}'", hex::encode(&revision_hash)),
      ));
    }
    let file_record = FileRecord::deserialize(&value, hash_width, actual.entry_version).map_err(map_revision_error)?;
    if file_record.path != path {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_path_mismatch",
        format!("FileRecord path '{}' does not match requested path '{path}'", file_record.path),
      ));
    }
    IndexFileRevisionReadV1::new(LoadedIndexFileRevisionV1 { revision_hash, file_record }, reservation)
  }

  fn load_directory_child(
    &self,
    directory_hash: &[u8],
    name: &str,
    path: &str,
  ) -> Result<Option<(Vec<u8>, EntryType)>, IndexFileRevisionReadErrorV1> {
    let (header, value, _reservation) = self.load_directory_entity(directory_hash, path)?;
    let child = if !is_btree_format(&value) {
      find_flat_child(&value, header.entry_version, self.engine.hash_algo().hash_length(), name)?
    } else {
      self.find_btree_child(directory_hash.to_vec(), Some((header, value, _reservation)), name, path)?
    };
    child
      .map(|child| EntryType::from_u8(child.entry_type).map(|entry_type| (child.hash, entry_type)).map_err(map_revision_error))
      .transpose()
  }

  fn find_btree_child(
    &self,
    mut node_hash: Vec<u8>,
    mut loaded_root: Option<(EntryHeader, Vec<u8>, MemoryReservation)>,
    name: &str,
    path: &str,
  ) -> Result<Option<ChildEntry>, IndexFileRevisionReadErrorV1> {
    let mut ancestry = Vec::new();
    let mut lower_bound = None;
    let mut upper_bound = None;
    ancestry.try_reserve_exact(usize::from(self.limits.maximum_btree_depth)).map_err(|error| {
      IndexFileRevisionReadErrorV1::retryable("native_revision_allocation", format!("B-tree ancestry allocation failed: {error}"))
    })?;
    for _ in 0..self.limits.maximum_btree_depth {
      if ancestry.iter().any(|ancestor| ancestor == &node_hash) {
        return Err(IndexFileRevisionReadErrorV1::corrupt(
          "native_revision_btree_cycle",
          format!("B-tree ancestry for '{path}' repeats node {}", hex::encode(&node_hash)),
        ));
      }
      ancestry.push(node_hash.clone());
      let (_header, value, _reservation) = match loaded_root.take() {
        Some(loaded) => loaded,
        None => self.load_directory_entity(&node_hash, path)?,
      };
      let node = decode_canonical_btree_node(
        &node_hash,
        &_header,
        &value,
        self.engine.hash_algo().hash_length(),
        lower_bound.as_deref(),
        upper_bound.as_deref(),
      )?;
      match node {
        BTreeNode::Leaf(leaf) => return Ok(leaf.find(name).cloned()),
        BTreeNode::Internal(internal) => {
          let child_index = internal.find_child_index(name);
          let (child_lower, child_upper) = btree_child_bounds(&internal, child_index, lower_bound, upper_bound)?;
          lower_bound = child_lower;
          upper_bound = child_upper;
          node_hash = internal.children[child_index].clone();
        }
      }
    }
    Err(IndexFileRevisionReadErrorV1::corrupt(
      "native_revision_btree_depth",
      format!("B-tree ancestry for '{path}' exceeds the configured depth limit"),
    ))
  }

  fn load_directory_entity(
    &self,
    hash: &[u8],
    path: &str,
  ) -> Result<(EntryHeader, Vec<u8>, MemoryReservation), IndexFileRevisionReadErrorV1> {
    let expected = self.required_header(hash, EntryType::DirectoryIndex, self.limits.maximum_directory_entity_bytes, path)?;
    let reservation = self.reserve_decoded(expected.value_length, "directory entity")?;
    let (header, key, value) = self.required_bounded_entry(hash, &expected, path)?;
    if key != hash || !same_header(&expected, &header) {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_directory_changed",
        format!("directory entity {} changed during the bounded read for '{path}'", hex::encode(hash)),
      ));
    }
    Ok((header, value, reservation))
  }

  fn required_header(
    &self,
    hash: &[u8],
    expected_type: EntryType,
    maximum_value_bytes: u32,
    path: &str,
  ) -> Result<EntryHeader, IndexFileRevisionReadErrorV1> {
    let header = self.engine.get_entry_header_including_deleted(hash).map_err(map_revision_error)?.ok_or_else(|| {
      IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_required_entity_missing",
        format!("required {:?} entity {} is missing for '{path}'", expected_type, hex::encode(hash)),
      )
    })?;
    if header.entry_type != expected_type
      || header.hash_algo != self.engine.hash_algo()
      || header.compression_algo != CompressionAlgorithm::None
      || header.encryption_algo != 0
      || header.key_length as usize != self.engine.hash_algo().hash_length()
    {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_entity_header",
        format!("required entity {} has a noncanonical header for '{path}'", hex::encode(hash)),
      ));
    }
    if header.value_length > maximum_value_bytes {
      return Err(IndexFileRevisionReadErrorV1::retryable(
        "native_revision_entity_limit",
        format!("required entity {} is {} bytes, exceeding the {maximum_value_bytes}-byte limit", hex::encode(hash), header.value_length),
      ));
    }
    Ok(header)
  }

  fn required_bounded_entry(
    &self,
    hash: &[u8],
    expected: &EntryHeader,
    path: &str,
  ) -> Result<(EntryHeader, Vec<u8>, Vec<u8>), IndexFileRevisionReadErrorV1> {
    self.engine.get_entry_including_deleted_verified_bounded(hash, expected.value_length).map_err(map_revision_error)?.ok_or_else(|| {
      IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_required_entity_missing",
        format!("required entity {} disappeared during the bounded read for '{path}'", hex::encode(hash)),
      )
    })
  }

  fn reserve_decoded(&self, encoded_bytes: u32, resource: &'static str) -> Result<MemoryReservation, IndexFileRevisionReadErrorV1> {
    let retained_bytes = u64::from(encoded_bytes)
      .checked_mul(NATIVE_REVISION_ALLOCATION_MULTIPLIER)
      .and_then(|bytes| bytes.checked_add(NATIVE_REVISION_FIXED_BYTES))
      .ok_or_else(|| {
        IndexFileRevisionReadErrorV1::corrupt("native_revision_memory_overflow", format!("{resource} reservation overflowed"))
      })?;
    self.engine.memory_coordinator().reserve(MemoryOwner::Task, retained_bytes, AdmissionClass::Maintenance).map_err(map_memory_error)
  }
}

impl IndexFileRevisionSourceV1 for NativeIndexFileRevisionSourceV1<'_> {
  fn load_file_revision(&self, namespace_root: &[u8], path: &str) -> Result<Option<IndexFileRevisionReadV1>, IndexFileRevisionReadErrorV1> {
    let hash_width = self.engine.hash_algo().hash_length();
    if namespace_root.len() != hash_width
      || namespace_root.iter().all(|byte| *byte == 0)
      || path == "/"
      || !path.starts_with('/')
      || normalize_path(path) != path
    {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_request",
        "namespace root or canonical absolute file path is invalid",
      ));
    }

    let root_header = self.engine.get_entry_header_including_deleted(namespace_root).map_err(map_revision_error)?;
    let Some(root_header) = root_header else {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_root_missing",
        format!("retained namespace root {} is missing", hex::encode(namespace_root)),
      ));
    };
    if root_header.entry_type != EntryType::DirectoryIndex || root_header.hash_algo != self.engine.hash_algo() {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_root_invalid",
        format!("retained namespace root {} has a non-directory or foreign-hash header", hex::encode(namespace_root)),
      ));
    }

    let Some((revision_hash, entry_type)) = self.resolve_entry_reference(namespace_root, path)? else {
      return Ok(None);
    };
    if entry_type != EntryType::FileRecord {
      return Ok(None);
    }
    self.load_file_reference(revision_hash, path).map(Some)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeIndexScanTraversalLimitsV1 {
  maximum_path_depth: u16,
  maximum_work_steps: u32,
}

impl NativeIndexScanTraversalLimitsV1 {
  pub fn new(maximum_path_depth: u16, maximum_work_steps: u32) -> Result<Self, IndexMaintenanceScanErrorV1> {
    if maximum_path_depth == 0 || maximum_work_steps == 0 {
      return Err(IndexMaintenanceScanErrorV1::InvalidOptions("native path-depth and traversal-work limits must be nonzero".to_string()));
    }
    Ok(Self { maximum_path_depth, maximum_work_steps })
  }
}

pub struct NativeIndexMaintenanceScanSourceV1<'engine> {
  revisions: NativeIndexFileRevisionSourceV1<'engine>,
  traversal_limits: NativeIndexScanTraversalLimitsV1,
}

impl<'engine> NativeIndexMaintenanceScanSourceV1<'engine> {
  pub const fn new(
    engine: &'engine StorageEngine,
    source_limits: NativeIndexSourceLimitsV1,
    traversal_limits: NativeIndexScanTraversalLimitsV1,
  ) -> Self {
    Self { revisions: NativeIndexFileRevisionSourceV1::new(engine, source_limits), traversal_limits }
  }

  fn resolve_scope(
    &self,
    request: &IndexMaintenanceScanRequestV1<'_>,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<NativeScanReferenceV1>, IndexMaintenanceScanReadErrorV1> {
    work.step()?;
    let header =
      self.revisions.engine.get_entry_header_including_deleted(request.namespace_root).map_err(map_engine_scan_error)?.ok_or_else(
        || {
          IndexMaintenanceScanReadErrorV1::corrupt(
            "native_scan_root_missing",
            format!("retained namespace root {} is missing", hex::encode(request.namespace_root)),
          )
        },
      )?;
    if header.entry_type != EntryType::DirectoryIndex || header.hash_algo != self.revisions.engine.hash_algo() {
      return Err(IndexMaintenanceScanReadErrorV1::corrupt(
        "native_scan_root_invalid",
        format!("retained namespace root {} has a non-directory or foreign-hash header", hex::encode(request.namespace_root)),
      ));
    }
    if request.scope == "/" {
      return Ok(Some(NativeScanReferenceV1 {
        hash: request.namespace_root.to_vec(),
        entry_type: EntryType::DirectoryIndex,
        path: "/".to_string(),
        depth: 0,
      }));
    }
    let mut directory_hash = request.namespace_root.to_vec();
    let mut current_path = String::new();
    let mut segments = request.scope.split('/').filter(|segment| !segment.is_empty()).peekable();
    let mut depth = 0u16;
    while let Some(segment) = segments.next() {
      depth = depth
        .checked_add(1)
        .ok_or_else(|| IndexMaintenanceScanReadErrorV1::retryable("native_scan_path_depth", "namespace path depth overflowed"))?;
      if depth > self.traversal_limits.maximum_path_depth {
        return Err(IndexMaintenanceScanReadErrorV1::retryable(
          "native_scan_path_depth",
          "namespace scope exceeds the configured path-depth limit",
        ));
      }
      let child = self.scan_exact_child(&directory_hash, segment, request.scope, work)?;
      let Some(child) = child else {
        return Ok(None);
      };
      let entry_type = EntryType::from_u8(child.entry_type).map_err(map_engine_scan_error)?;
      current_path = join_scan_path(&current_path, segment, request.limits.maximum_path_bytes())?;
      if segments.peek().is_none() {
        return Ok(Some(NativeScanReferenceV1 { hash: child.hash, entry_type, path: current_path, depth }));
      }
      if entry_type != EntryType::DirectoryIndex {
        return Ok(None);
      }
      directory_hash = child.hash;
    }
    Ok(None)
  }

  fn next_reference(
    &self,
    scope: &NativeScanReferenceV1,
    lower_path: Option<&str>,
    maximum_path_bytes: u32,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<NativeScanReferenceV1>, IndexMaintenanceScanReadErrorV1> {
    match scope.entry_type {
      EntryType::FileRecord => Ok(lower_path.is_none_or(|lower| scope.path.as_str() > lower).then(|| scope.clone())),
      EntryType::DirectoryIndex => self.next_file_in_directory(&scope.hash, &scope.path, lower_path, maximum_path_bytes, scope.depth, work),
      _ => Ok(None),
    }
  }

  fn next_file_in_directory(
    &self,
    directory_hash: &[u8],
    directory_path: &str,
    lower_path: Option<&str>,
    maximum_path_bytes: u32,
    depth: u16,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<NativeScanReferenceV1>, IndexMaintenanceScanReadErrorV1> {
    if depth > self.traversal_limits.maximum_path_depth {
      return Err(IndexMaintenanceScanReadErrorV1::retryable(
        "native_scan_path_depth",
        "namespace traversal exceeds the configured path-depth limit",
      ));
    }
    work.check_cancelled()?;
    let relative_lower = lower_path.and_then(|lower| relative_scan_path(directory_path, lower));
    let context_path = match lower_path {
      Some(path) => path,
      None => directory_path,
    };

    if let Some(relative) = relative_lower {
      if let Some((component, _)) = relative.split_once('/') {
        if !component.is_empty() {
          if let Some(child) = self.scan_exact_child(directory_hash, component, context_path, work)? {
            if child.entry_type == EntryType::DirectoryIndex.to_u8() {
              let child_path = join_scan_path(directory_path, component, maximum_path_bytes)?;
              let next_depth = next_scan_depth(depth)?;
              if let Some(found) =
                self.next_file_in_directory(&child.hash, &child_path, lower_path, maximum_path_bytes, next_depth, work)?
              {
                return Ok(Some(found));
              }
            }
          }
        }
      }
    }

    let mut child_lower = relative_lower.map(str::to_string);
    loop {
      let Some((scan_key, child)) = self.next_child_by_scan_key(directory_hash, child_lower.as_deref(), context_path, work)? else {
        return Ok(None);
      };
      let entry_type = EntryType::from_u8(child.entry_type)
        .map_err(|error| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_child_type", error.to_string()))?;
      let child_path = join_scan_path(directory_path, &child.name, maximum_path_bytes)?;
      let child_depth = next_scan_depth(depth)?;
      if child_depth > self.traversal_limits.maximum_path_depth {
        return Err(IndexMaintenanceScanReadErrorV1::retryable(
          "native_scan_path_depth",
          "namespace traversal exceeds the configured path-depth limit",
        ));
      }
      match entry_type {
        EntryType::FileRecord => {
          return Ok(Some(NativeScanReferenceV1 { hash: child.hash, entry_type, path: child_path, depth: child_depth }));
        }
        EntryType::DirectoryIndex => {
          if let Some(found) = self.next_file_in_directory(&child.hash, &child_path, None, maximum_path_bytes, child_depth, work)? {
            return Ok(Some(found));
          }
        }
        _ => {}
      }
      child_lower = Some(scan_key);
    }
  }

  fn next_child_by_scan_key(
    &self,
    directory_hash: &[u8],
    lower: Option<&str>,
    context_path: &str,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<(String, ChildEntry)>, IndexMaintenanceScanReadErrorV1> {
    let mut best: Option<(String, ChildEntry)> = None;
    if let Some(lower) = lower {
      let component = lower.split('/').next().map_or("", |component| component);
      for (end, _) in component.char_indices().skip(1).chain(std::iter::once((component.len(), '\0'))) {
        let prefix = &component[..end];
        if prefix.is_empty() {
          continue;
        }
        let child = self.scan_exact_child(directory_hash, prefix, context_path, work)?;
        if let Some(entry) = child.filter(|entry| entry.entry_type == EntryType::DirectoryIndex.to_u8()) {
          let key = child_scan_key(&entry)?;
          if key.as_str() > lower && best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
            best = Some((key, entry));
          }
        }
      }
    }

    let mut raw_lower = match lower {
      Some(lower) => lower.to_string(),
      None => String::new(),
    };
    let mut inclusive = lower.is_none();
    loop {
      let child = self.seek_raw_child(directory_hash, &raw_lower, inclusive, context_path, work)?;
      let Some(child) = child else {
        break;
      };
      if best.as_ref().is_some_and(|(best_key, _)| child.name.as_str() >= best_key.as_str()) {
        break;
      }
      let key = child_scan_key(&child)?;
      if lower.is_none_or(|lower| key.as_str() > lower) && best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
        best = Some((key, child.clone()));
      }
      if child.entry_type != EntryType::DirectoryIndex.to_u8() {
        break;
      }
      raw_lower = child.name;
      inclusive = false;
    }
    Ok(best)
  }

  fn scan_exact_child(
    &self,
    directory_hash: &[u8],
    name: &str,
    context_path: &str,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<ChildEntry>, IndexMaintenanceScanReadErrorV1> {
    Ok(self.seek_raw_child(directory_hash, name, true, context_path, work)?.filter(|entry| entry.name == name))
  }

  fn seek_raw_child(
    &self,
    directory_hash: &[u8],
    lower: &str,
    inclusive: bool,
    context_path: &str,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<ChildEntry>, IndexMaintenanceScanReadErrorV1> {
    work.step()?;
    let (header, value, reservation) =
      self.revisions.load_directory_entity(directory_hash, context_path).map_err(map_revision_scan_error)?;
    if !is_btree_format(&value) {
      return seek_flat_child(&value, header.entry_version, self.revisions.engine.hash_algo().hash_length(), lower, inclusive, work);
    }
    self.seek_btree_child(directory_hash, (header, value, reservation), lower, inclusive, context_path, work)
  }

  fn seek_btree_child(
    &self,
    root_hash: &[u8],
    loaded_root: (EntryHeader, Vec<u8>, MemoryReservation),
    lower: &str,
    inclusive: bool,
    context_path: &str,
    work: &mut NativeScanWorkV1<'_>,
  ) -> Result<Option<ChildEntry>, IndexMaintenanceScanReadErrorV1> {
    let hash_width = self.revisions.engine.hash_algo().hash_length();
    let mut stack: Vec<NativeBtreeSeekFrameV1> = Vec::new();
    stack.try_reserve_exact(usize::from(self.revisions.limits.maximum_btree_depth)).map_err(|error| {
      IndexMaintenanceScanReadErrorV1::retryable("native_scan_allocation", format!("B-tree seek stack allocation failed: {error}"))
    })?;
    let mut node_hash = root_hash.to_vec();
    let mut loaded = Some(loaded_root);
    let mut lower_bound = None;
    let mut upper_bound = None;
    loop {
      if stack.len() >= usize::from(self.revisions.limits.maximum_btree_depth) {
        return Err(IndexMaintenanceScanReadErrorV1::corrupt(
          "native_scan_btree_depth",
          "B-tree seek exceeds the configured structural depth",
        ));
      }
      if stack.iter().any(|frame| frame.node_hash == node_hash) {
        return Err(IndexMaintenanceScanReadErrorV1::corrupt(
          "native_scan_btree_cycle",
          format!("B-tree seek repeats node {}", hex::encode(&node_hash)),
        ));
      }
      let (header, value, reservation) = match loaded.take() {
        Some(root) => root,
        None => {
          work.step()?;
          self.revisions.load_directory_entity(&node_hash, context_path).map_err(map_revision_scan_error)?
        }
      };
      let node = decode_canonical_btree_node(&node_hash, &header, &value, hash_width, lower_bound.as_deref(), upper_bound.as_deref())
        .map_err(map_revision_scan_error)?;
      drop(reservation);
      match node {
        BTreeNode::Leaf(leaf) => {
          work.step()?;
          let index = leaf.entries.partition_point(|entry| match entry.name.as_str().cmp(lower) {
            Ordering::Less => true,
            Ordering::Equal => !inclusive,
            Ordering::Greater => false,
          });
          if let Some(entry) = leaf.entries.get(index) {
            return Ok(Some(entry.clone()));
          }
          break;
        }
        BTreeNode::Internal(internal) => {
          let child_index = internal.find_child_index(lower);
          let (child_lower, child_upper) =
            btree_child_bounds(&internal, child_index, lower_bound.clone(), upper_bound.clone()).map_err(map_revision_scan_error)?;
          stack.push(NativeBtreeSeekFrameV1 { node_hash, child_index, lower_bound, upper_bound });
          lower_bound = child_lower;
          upper_bound = child_upper;
          node_hash = internal.children[child_index].clone();
        }
      }
    }

    'ascend: while let Some(frame) = stack.pop() {
      work.step()?;
      let (header, value, reservation) =
        self.revisions.load_directory_entity(&frame.node_hash, context_path).map_err(map_revision_scan_error)?;
      let node = decode_canonical_btree_node(
        &frame.node_hash,
        &header,
        &value,
        hash_width,
        frame.lower_bound.as_deref(),
        frame.upper_bound.as_deref(),
      )
      .map_err(map_revision_scan_error)?;
      drop(reservation);
      let BTreeNode::Internal(parent) = node else {
        return Err(IndexMaintenanceScanReadErrorV1::corrupt(
          "native_scan_btree_parent",
          "B-tree seek parent changed node shape during traversal",
        ));
      };
      let next_child_index = frame
        .child_index
        .checked_add(1)
        .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_btree_child", "B-tree child index overflowed"))?;
      if next_child_index >= parent.children.len() {
        continue;
      }
      let (child_lower, child_upper) = btree_child_bounds(&parent, next_child_index, frame.lower_bound.clone(), frame.upper_bound.clone())
        .map_err(map_revision_scan_error)?;
      stack.push(NativeBtreeSeekFrameV1 {
        node_hash: frame.node_hash,
        child_index: next_child_index,
        lower_bound: frame.lower_bound,
        upper_bound: frame.upper_bound,
      });
      node_hash = parent.children[next_child_index].clone();
      lower_bound = child_lower;
      upper_bound = child_upper;
      loop {
        if stack.len() >= usize::from(self.revisions.limits.maximum_btree_depth) {
          return Err(IndexMaintenanceScanReadErrorV1::corrupt(
            "native_scan_btree_depth",
            "B-tree successor descent exceeds the configured structural depth",
          ));
        }
        if stack.iter().any(|frame| frame.node_hash == node_hash) {
          return Err(IndexMaintenanceScanReadErrorV1::corrupt(
            "native_scan_btree_cycle",
            format!("B-tree successor repeats node {}", hex::encode(&node_hash)),
          ));
        }
        work.step()?;
        let (header, value, reservation) =
          self.revisions.load_directory_entity(&node_hash, context_path).map_err(map_revision_scan_error)?;
        let node = decode_canonical_btree_node(&node_hash, &header, &value, hash_width, lower_bound.as_deref(), upper_bound.as_deref())
          .map_err(map_revision_scan_error)?;
        drop(reservation);
        match node {
          BTreeNode::Leaf(leaf) => match leaf.entries.first() {
            Some(entry) => return Ok(Some(entry.clone())),
            None => continue 'ascend,
          },
          BTreeNode::Internal(internal) => {
            let (child_lower, child_upper) =
              btree_child_bounds(&internal, 0, lower_bound.clone(), upper_bound.clone()).map_err(map_revision_scan_error)?;
            stack.push(NativeBtreeSeekFrameV1 { node_hash, child_index: 0, lower_bound, upper_bound });
            lower_bound = child_lower;
            upper_bound = child_upper;
            node_hash = internal.children[0].clone();
          }
        }
      }
    }
    Ok(None)
  }
}

impl IndexMaintenanceScanSourceV1 for NativeIndexMaintenanceScanSourceV1<'_> {
  fn scan(&self, request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    validate_index_maintenance_scan_request_v1(self.revisions.engine.hash_algo(), &request).map_err(map_scan_contract_error)?;
    let maximum_documents = usize::try_from(request.limits.maximum_documents()).map_err(|error| {
      IndexMaintenanceScanReadErrorV1::corrupt("native_scan_document_count", format!("document count does not fit usize: {error}"))
    })?;
    let minimum_page_bytes = size_of::<IndexMaintenanceScanPageV1>()
      .checked_add(
        maximum_documents
          .checked_mul(size_of::<IndexMaintenanceScanDocumentV1>())
          .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "document page slot bytes overflowed"))?,
      )
      .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "minimum page bytes overflowed"))?;
    if u64::try_from(minimum_page_bytes).map_err(|error| {
      IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", format!("minimum page bytes do not fit u64: {error}"))
    })?
      > request.limits.maximum_retained_bytes()
    {
      return Err(IndexMaintenanceScanReadErrorV1::retryable(
        "native_scan_page_limit",
        "document page slots cannot fit within the authoritative scan retained-byte limit",
      ));
    }
    let mut page_reservation = self
      .revisions
      .engine
      .memory_coordinator()
      .reserve(MemoryOwner::Task, request.limits.maximum_retained_bytes(), AdmissionClass::Maintenance)
      .map_err(map_memory_scan_error)?;
    let workspace_bytes = native_scan_workspace_bytes(
      request.limits.maximum_path_bytes(),
      self.traversal_limits.maximum_path_depth,
      self.revisions.limits.maximum_btree_depth,
      self.revisions.engine.hash_algo().hash_length(),
    )?;
    let transient_reservation = self
      .revisions
      .engine
      .memory_coordinator()
      .reserve(MemoryOwner::Task, workspace_bytes, AdmissionClass::Maintenance)
      .map_err(map_memory_scan_error)?;
    let mut documents = Vec::new();
    documents.try_reserve_exact(maximum_documents).map_err(|error| {
      IndexMaintenanceScanReadErrorV1::retryable("native_scan_allocation", format!("document page allocation failed: {error}"))
    })?;
    let mut work = NativeScanWorkV1::new(self.traversal_limits.maximum_work_steps, request.is_cancelled);
    let scope = self.resolve_scope(&request, &mut work)?;
    let mut complete = false;
    let mut next_resume_after = None;

    if let Some(scope) = scope {
      loop {
        let lower =
          documents.last().map(|document: &IndexMaintenanceScanDocumentV1| document.file_record.path.as_str()).or(request.resume_after);
        let Some(reference) = self.next_reference(&scope, lower, request.limits.maximum_path_bytes(), &mut work)? else {
          complete = true;
          next_resume_after = None;
          break;
        };
        if documents.len() >= maximum_documents {
          next_resume_after = documents.last().map(|document| document.file_record.path.clone());
          break;
        }
        work.step()?;
        let read = self.revisions.load_file_reference(reference.hash, &reference.path).map_err(map_revision_scan_error)?;
        let (revision, revision_reservation) = read.into_parts();
        documents.push(IndexMaintenanceScanDocumentV1 { revision_hash: revision.revision_hash, file_record: revision.file_record });
        drop(revision_reservation);
        let provisional_resume = documents.last().map(|document| document.file_record.path.clone());
        let provisional =
          IndexMaintenanceScanPageV1 { documents, next_resume_after: provisional_resume, complete: false, retained_bytes: 0 };
        let retained = index_maintenance_scan_page_retained_bytes_v1(&provisional).map_err(map_scan_contract_error)?;
        documents = provisional.documents;
        if retained > request.limits.maximum_retained_bytes() {
          documents.pop();
          if documents.is_empty() {
            return Err(IndexMaintenanceScanReadErrorV1::retryable(
              "native_scan_document_limit",
              "the first document cannot fit within the authoritative scan page byte limit",
            ));
          }
          next_resume_after = documents.last().map(|document| document.file_record.path.clone());
          break;
        }
      }
    } else {
      complete = true;
    }

    let mut page = IndexMaintenanceScanPageV1 { documents, next_resume_after, complete, retained_bytes: 0 };
    page.retained_bytes = index_maintenance_scan_page_retained_bytes_v1(&page).map_err(map_scan_contract_error)?;
    if page.retained_bytes > request.limits.maximum_retained_bytes() {
      return Err(IndexMaintenanceScanReadErrorV1::retryable(
        "native_scan_page_limit",
        "the authoritative scan page allocation exceeds its retained-byte limit",
      ));
    }
    let release_bytes = page_reservation.bytes().checked_sub(page.retained_bytes).ok_or_else(|| {
      IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_accounting", "page reservation is smaller than retained bytes")
    })?;
    page_reservation.shrink(release_bytes).map_err(map_memory_scan_error)?;
    drop(transient_reservation);
    IndexMaintenanceScanReadV1::new(self.revisions.engine.hash_algo(), &request, page, page_reservation).map_err(map_scan_contract_error)
  }
}

#[derive(Debug, Clone)]
struct NativeScanReferenceV1 {
  hash: Vec<u8>,
  entry_type: EntryType,
  path: String,
  depth: u16,
}

struct NativeBtreeSeekFrameV1 {
  node_hash: Vec<u8>,
  child_index: usize,
  lower_bound: Option<String>,
  upper_bound: Option<String>,
}

struct NativeScanWorkV1<'request> {
  steps: u32,
  maximum_steps: u32,
  is_cancelled: &'request dyn Fn() -> bool,
}

impl<'request> NativeScanWorkV1<'request> {
  const fn new(maximum_steps: u32, is_cancelled: &'request dyn Fn() -> bool) -> Self {
    Self { steps: 0, maximum_steps, is_cancelled }
  }

  fn check_cancelled(&self) -> Result<(), IndexMaintenanceScanReadErrorV1> {
    if (self.is_cancelled)() {
      return Err(IndexMaintenanceScanReadErrorV1::cancelled("native_scan_cancelled", "authoritative scan was cancelled"));
    }
    Ok(())
  }

  fn step(&mut self) -> Result<(), IndexMaintenanceScanReadErrorV1> {
    self.check_cancelled()?;
    self.steps = self
      .steps
      .checked_add(1)
      .ok_or_else(|| IndexMaintenanceScanReadErrorV1::retryable("native_scan_work_limit", "authoritative scan work counter overflowed"))?;
    if self.steps > self.maximum_steps {
      return Err(IndexMaintenanceScanReadErrorV1::retryable(
        "native_scan_work_limit",
        "authoritative scan exceeded the configured traversal-work limit",
      ));
    }
    Ok(())
  }
}

fn find_flat_child(
  value: &[u8],
  entry_version: u8,
  hash_width: usize,
  name: &str,
) -> Result<Option<ChildEntry>, IndexFileRevisionReadErrorV1> {
  let entries = decode_valid_flat_children(value, entry_version, hash_width)?;
  let index = entries.partition_point(|entry| entry.name.as_str() < name);
  Ok(entries.get(index).filter(|entry| entry.name == name).cloned())
}

fn seek_flat_child(
  value: &[u8],
  entry_version: u8,
  hash_width: usize,
  lower: &str,
  inclusive: bool,
  work: &mut NativeScanWorkV1<'_>,
) -> Result<Option<ChildEntry>, IndexMaintenanceScanReadErrorV1> {
  let entries = decode_valid_flat_children(value, entry_version, hash_width).map_err(map_revision_scan_error)?;
  for _ in &entries {
    work.step()?;
  }
  let index = entries.partition_point(|entry| match entry.name.as_str().cmp(lower) {
    Ordering::Less => true,
    Ordering::Equal => !inclusive,
    Ordering::Greater => false,
  });
  Ok(entries.get(index).cloned())
}

fn decode_valid_flat_children(value: &[u8], entry_version: u8, hash_width: usize) -> Result<Vec<ChildEntry>, IndexFileRevisionReadErrorV1> {
  let mut entries = deserialize_child_entries(value, hash_width, entry_version).map_err(map_revision_error)?;
  if entries.len() >= BTREE_CONVERSION_THRESHOLD {
    return Err(IndexFileRevisionReadErrorV1::corrupt(
      "native_revision_flat_count",
      "flat directory reaches or exceeds the B-tree conversion threshold",
    ));
  }
  for entry in &entries {
    EntryType::from_u8(entry.entry_type).map_err(map_revision_error)?;
    if !canonical_scan_name(&entry.name) || entry.hash.len() != hash_width || entry.hash.iter().all(|byte| *byte == 0) {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_flat_child",
        "flat directory has an invalid child name, type, or identity",
      ));
    }
  }
  // V0 flat writers do not all preserve physical name order.
  entries.sort_by(|left, right| left.name.cmp(&right.name));
  if entries.windows(2).any(|pair| pair[0].name == pair[1].name) {
    return Err(IndexFileRevisionReadErrorV1::corrupt("native_revision_flat_duplicate", "flat directory contains duplicate child names"));
  }
  Ok(entries)
}

fn decode_canonical_btree_node(
  node_hash: &[u8],
  header: &EntryHeader,
  value: &[u8],
  hash_width: usize,
  lower_bound: Option<&str>,
  upper_bound: Option<&str>,
) -> Result<BTreeNode, IndexFileRevisionReadErrorV1> {
  let node = BTreeNode::deserialize(value, hash_width, header.entry_version).map_err(map_revision_error)?;
  validate_btree_node(&node, hash_width, lower_bound, upper_bound)?;
  if node.serialize(hash_width).map_err(map_revision_error)? != value {
    return Err(IndexFileRevisionReadErrorV1::corrupt(
      "native_revision_btree_noncanonical",
      format!("B-tree node {} has trailing or noncanonical bytes", hex::encode(node_hash)),
    ));
  }
  Ok(node)
}

fn child_scan_key(entry: &ChildEntry) -> Result<String, IndexMaintenanceScanReadErrorV1> {
  let entry_type = EntryType::from_u8(entry.entry_type).map_err(map_engine_scan_error)?;
  let suffix = usize::from(entry_type == EntryType::DirectoryIndex);
  let capacity = entry
    .name
    .len()
    .checked_add(suffix)
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_path_overflow", "child scan-key length overflowed"))?;
  let mut key = String::new();
  key.try_reserve_exact(capacity).map_err(|error| {
    IndexMaintenanceScanReadErrorV1::retryable("native_scan_allocation", format!("child scan-key allocation failed: {error}"))
  })?;
  key.push_str(&entry.name);
  if entry_type == EntryType::DirectoryIndex {
    key.push('/');
  }
  Ok(key)
}

fn relative_scan_path<'path>(directory_path: &str, path: &'path str) -> Option<&'path str> {
  if path == directory_path {
    return None;
  }
  if directory_path == "/" {
    return path.strip_prefix('/');
  }
  path.strip_prefix(directory_path).and_then(|suffix| suffix.strip_prefix('/'))
}

fn next_scan_depth(depth: u16) -> Result<u16, IndexMaintenanceScanReadErrorV1> {
  depth
    .checked_add(1)
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::retryable("native_scan_path_depth", "namespace traversal depth overflowed"))
}

fn join_scan_path(directory_path: &str, name: &str, maximum_path_bytes: u32) -> Result<String, IndexMaintenanceScanReadErrorV1> {
  if name.is_empty() || name.contains('/') {
    return Err(IndexMaintenanceScanReadErrorV1::corrupt(
      "native_scan_child_name",
      "directory child name is empty or contains a path separator",
    ));
  }
  let separator_bytes = usize::from(!directory_path.is_empty() && directory_path != "/");
  let path_bytes = 1usize
    .checked_add(directory_path.trim_matches('/').len())
    .and_then(|bytes| bytes.checked_add(separator_bytes))
    .and_then(|bytes| bytes.checked_add(name.len()))
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_path_overflow", "document path length overflowed"))?;
  if path_bytes > maximum_path_bytes as usize {
    return Err(IndexMaintenanceScanReadErrorV1::retryable(
      "native_scan_path_limit",
      "document path exceeds the authoritative scan path-byte limit",
    ));
  }
  let mut path = String::new();
  path.try_reserve_exact(path_bytes).map_err(|error| {
    IndexMaintenanceScanReadErrorV1::retryable("native_scan_allocation", format!("document path allocation failed: {error}"))
  })?;
  path.push('/');
  if !directory_path.is_empty() && directory_path != "/" {
    path.push_str(directory_path.trim_matches('/'));
    path.push('/');
  }
  path.push_str(name);
  if normalize_path(&path) != path {
    return Err(IndexMaintenanceScanReadErrorV1::corrupt(
      "native_scan_child_path",
      format!("directory child resolves to noncanonical path '{path}'"),
    ));
  }
  Ok(path)
}

fn native_scan_workspace_bytes(
  maximum_path_bytes: u32,
  maximum_path_depth: u16,
  maximum_btree_depth: u16,
  hash_width: usize,
) -> Result<u64, IndexMaintenanceScanReadErrorV1> {
  let path_copies = u64::from(maximum_path_depth)
    .checked_add(4)
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "path workspace multiplier overflowed"))?;
  let path_bytes = u64::from(maximum_path_bytes)
    .checked_mul(path_copies)
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "path workspace bytes overflowed"))?;
  let hash_width = u64::try_from(hash_width).map_err(|error| {
    IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", format!("hash width does not fit u64: {error}"))
  })?;
  let btree_item = u64::try_from(size_of::<NativeBtreeSeekFrameV1>())
    .map_err(|error| {
      IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", format!("B-tree item size does not fit u64: {error}"))
    })?
    .checked_add(hash_width)
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "B-tree item bytes overflowed"))?;
  path_bytes
    .checked_add(
      u64::from(maximum_btree_depth)
        .checked_mul(btree_item)
        .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "B-tree workspace bytes overflowed"))?,
    )
    .and_then(|bytes| bytes.checked_add(NATIVE_SCAN_FIXED_WORKSPACE_BYTES))
    .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("native_scan_memory_overflow", "native scan workspace bytes overflowed"))
}

fn map_scan_contract_error(error: IndexMaintenanceScanErrorV1) -> IndexMaintenanceScanReadErrorV1 {
  match error {
    IndexMaintenanceScanErrorV1::Cancelled => {
      IndexMaintenanceScanReadErrorV1::cancelled("native_scan_cancelled", "authoritative scan was cancelled")
    }
    IndexMaintenanceScanErrorV1::InvalidOptions(context) | IndexMaintenanceScanErrorV1::InvalidRequest(context) => {
      IndexMaintenanceScanReadErrorV1::corrupt("native_scan_request", context)
    }
    IndexMaintenanceScanErrorV1::InvalidPage(context) => IndexMaintenanceScanReadErrorV1::corrupt("native_scan_page", context),
  }
}

fn map_memory_scan_error(error: MemoryCoordinatorError) -> IndexMaintenanceScanReadErrorV1 {
  IndexMaintenanceScanReadErrorV1::retryable("native_scan_memory_pressure", error.to_string())
}

fn map_revision_scan_error(error: IndexFileRevisionReadErrorV1) -> IndexMaintenanceScanReadErrorV1 {
  match error.class() {
    IndexFileRevisionReadErrorClassV1::Cancelled => IndexMaintenanceScanReadErrorV1::cancelled("native_scan_cancelled", error.context()),
    IndexFileRevisionReadErrorClassV1::Retryable => IndexMaintenanceScanReadErrorV1::retryable(error.code(), error.context()),
    IndexFileRevisionReadErrorClassV1::Corrupt => IndexMaintenanceScanReadErrorV1::corrupt(error.code(), error.context()),
  }
}

fn map_engine_scan_error(error: EngineError) -> IndexMaintenanceScanReadErrorV1 {
  match error {
    EngineError::Cancelled(context) => IndexMaintenanceScanReadErrorV1::cancelled("native_scan_cancelled", context),
    error @ (EngineError::IoError(_)
    | EngineError::ResourceExhausted(_)
    | EngineError::DurabilityFailure(_)
    | EngineError::PostMutationDurabilityFailure(_)
    | EngineError::ShuttingDown) => IndexMaintenanceScanReadErrorV1::retryable("native_scan_unavailable", error.to_string()),
    error => IndexMaintenanceScanReadErrorV1::corrupt("native_scan_corrupt", error.to_string()),
  }
}

fn same_header(left: &EntryHeader, right: &EntryHeader) -> bool {
  left.entry_version == right.entry_version
    && left.entry_type == right.entry_type
    && left.flags == right.flags
    && left.hash_algo == right.hash_algo
    && left.compression_algo == right.compression_algo
    && left.encryption_algo == right.encryption_algo
    && left.key_length == right.key_length
    && left.value_length == right.value_length
    && left.timestamp == right.timestamp
    && left.total_length == right.total_length
    && left.hash == right.hash
}

fn validate_btree_node(
  node: &BTreeNode,
  hash_width: usize,
  lower_bound: Option<&str>,
  upper_bound: Option<&str>,
) -> Result<(), IndexFileRevisionReadErrorV1> {
  if lower_bound.zip(upper_bound).is_some_and(|(lower, upper)| lower >= upper) {
    return Err(IndexFileRevisionReadErrorV1::corrupt(
      "native_revision_btree_range",
      "B-tree inherited separator range is empty or reversed",
    ));
  }
  match node {
    BTreeNode::Leaf(leaf) => {
      if leaf.entries.len() > BTREE_MAX_LEAF_ENTRIES {
        return Err(IndexFileRevisionReadErrorV1::corrupt(
          "native_revision_btree_leaf_count",
          "B-tree leaf exceeds the structural entry bound",
        ));
      }
      let mut previous: Option<&str> = None;
      for entry in &leaf.entries {
        EntryType::from_u8(entry.entry_type).map_err(map_revision_error)?;
        if !canonical_scan_name(&entry.name)
          || entry.hash.len() != hash_width
          || entry.hash.iter().all(|byte| *byte == 0)
          || previous.is_some_and(|prior| prior >= entry.name.as_str())
        {
          return Err(IndexFileRevisionReadErrorV1::corrupt(
            "native_revision_btree_leaf_order",
            "B-tree leaf has an invalid child identity or non-strict name order",
          ));
        }
        if !within_btree_range(&entry.name, lower_bound, upper_bound) {
          return Err(IndexFileRevisionReadErrorV1::corrupt(
            "native_revision_btree_range",
            "B-tree leaf entry is outside its inherited separator range",
          ));
        }
        previous = Some(&entry.name);
      }
    }
    BTreeNode::Internal(internal) => {
      if internal.keys.is_empty()
        || internal.keys.len() > BTREE_MAX_INTERNAL_KEYS
        || internal.children.len() != internal.keys.len() + 1
        || internal.keys.windows(2).any(|pair| pair[0] >= pair[1])
        || internal.keys.iter().any(|key| !canonical_scan_name(key) || !within_btree_range(key, lower_bound, upper_bound))
        || internal.children.iter().enumerate().any(|(index, hash)| {
          hash.len() != hash_width
            || hash.iter().all(|byte| *byte == 0)
            || internal.children[index + 1..].iter().any(|candidate| candidate == hash)
        })
      {
        return Err(IndexFileRevisionReadErrorV1::corrupt(
          "native_revision_btree_internal",
          "B-tree internal node has invalid keys, children, order, or identity width",
        ));
      }
    }
  }
  Ok(())
}

fn btree_child_bounds(
  internal: &crate::engine::btree::InternalNode,
  child_index: usize,
  inherited_lower: Option<String>,
  inherited_upper: Option<String>,
) -> Result<(Option<String>, Option<String>), IndexFileRevisionReadErrorV1> {
  if child_index >= internal.children.len() {
    return Err(IndexFileRevisionReadErrorV1::corrupt("native_revision_btree_child", "B-tree child index exceeds the internal node"));
  }
  let lower = if child_index == 0 { inherited_lower } else { Some(internal.keys[child_index - 1].clone()) };
  let upper = if child_index == internal.keys.len() { inherited_upper } else { Some(internal.keys[child_index].clone()) };
  Ok((lower, upper))
}

fn canonical_scan_name(name: &str) -> bool {
  !name.is_empty() && !matches!(name, "." | "..") && !name.contains('/') && !name.contains('\0')
}

fn within_btree_range(value: &str, lower: Option<&str>, upper: Option<&str>) -> bool {
  lower.is_none_or(|bound| value >= bound) && upper.is_none_or(|bound| value < bound)
}

fn map_memory_error(error: MemoryCoordinatorError) -> IndexFileRevisionReadErrorV1 {
  IndexFileRevisionReadErrorV1::retryable("native_revision_memory_pressure", error.to_string())
}

fn map_revision_error(error: EngineError) -> IndexFileRevisionReadErrorV1 {
  match error {
    EngineError::Cancelled(context) => IndexFileRevisionReadErrorV1::cancelled("native_revision_cancelled", context),
    error @ (EngineError::IoError(_)
    | EngineError::ResourceExhausted(_)
    | EngineError::DurabilityFailure(_)
    | EngineError::PostMutationDurabilityFailure(_)
    | EngineError::ShuttingDown) => IndexFileRevisionReadErrorV1::retryable("native_revision_unavailable", error.to_string()),
    error => IndexFileRevisionReadErrorV1::corrupt("native_revision_corrupt", error.to_string()),
  }
}

const _: () = assert!(size_of::<NativeIndexSourceLimitsV1>() <= 16);
