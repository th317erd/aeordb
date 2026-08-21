//! Production source adapters for the native v4 index runtime.

use std::mem::size_of;

use crate::engine::btree::{BTREE_CONVERSION_THRESHOLD, BTREE_MAX_INTERNAL_KEYS, BTREE_MAX_LEAF_ENTRIES, BTreeNode, is_btree_format};
use crate::engine::directory_entry::ChildEntry;
use crate::engine::entry_header::EntryHeader;
use crate::engine::errors::EngineError;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::{CompressionAlgorithm, EntryType};

use super::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionReadV1, IndexFileRevisionSourceV1, LoadedIndexFileRevisionV1,
};

const NATIVE_REVISION_ALLOCATION_MULTIPLIER: u64 = 4;
const NATIVE_REVISION_FIXED_BYTES: u64 = 1_024;

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

  fn resolve_file_reference(&self, namespace_root: &[u8], path: &str) -> Result<Option<Vec<u8>>, IndexFileRevisionReadErrorV1> {
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
        return Ok((entry_type == EntryType::FileRecord).then_some(child_hash));
      }
      if entry_type != EntryType::DirectoryIndex {
        return Ok(None);
      }
      directory_hash = child_hash;
    }
    Ok(None)
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
      let node =
        BTreeNode::deserialize(&value, self.engine.hash_algo().hash_length(), _header.entry_version).map_err(map_revision_error)?;
      validate_btree_node(&node, self.engine.hash_algo().hash_length())?;
      if node.serialize(self.engine.hash_algo().hash_length()).map_err(map_revision_error)? != value {
        return Err(IndexFileRevisionReadErrorV1::corrupt(
          "native_revision_btree_noncanonical",
          format!("B-tree node {} for '{path}' has trailing or noncanonical bytes", hex::encode(&node_hash)),
        ));
      }
      match node {
        BTreeNode::Leaf(leaf) => return Ok(leaf.find(name).cloned()),
        BTreeNode::Internal(internal) => node_hash = internal.children[internal.find_child_index(name)].clone(),
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

    let Some(revision_hash) = self.resolve_file_reference(namespace_root, path)? else {
      return Ok(None);
    };
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
    IndexFileRevisionReadV1::new(LoadedIndexFileRevisionV1 { revision_hash, file_record }, reservation).map(Some)
  }
}

fn find_flat_child(
  value: &[u8],
  entry_version: u8,
  hash_width: usize,
  name: &str,
) -> Result<Option<ChildEntry>, IndexFileRevisionReadErrorV1> {
  let mut offset = 0usize;
  let mut count = 0usize;
  let mut previous_name: Option<String> = None;
  while offset < value.len() {
    if count >= BTREE_CONVERSION_THRESHOLD {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_flat_count",
        "flat directory exceeds the B-tree conversion threshold",
      ));
    }
    let (entry, consumed) = ChildEntry::deserialize(&value[offset..], hash_width, entry_version).map_err(map_revision_error)?;
    if consumed == 0 {
      return Err(IndexFileRevisionReadErrorV1::corrupt("native_revision_flat_progress", "flat directory entry consumed zero bytes"));
    }
    offset = offset
      .checked_add(consumed)
      .ok_or_else(|| IndexFileRevisionReadErrorV1::corrupt("native_revision_flat_offset", "flat directory offset overflowed"))?;
    count += 1;
    if entry.hash.len() != hash_width || previous_name.as_deref().is_some_and(|previous| previous >= entry.name.as_str()) {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "native_revision_flat_order",
        "flat directory has a wrong-width child identity or non-strict name order",
      ));
    }
    if entry.name == name {
      return Ok(Some(entry));
    }
    if entry.name.as_str() > name {
      return Ok(None);
    }
    previous_name = Some(entry.name);
  }
  Ok(None)
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

fn validate_btree_node(node: &BTreeNode, hash_width: usize) -> Result<(), IndexFileRevisionReadErrorV1> {
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
        if entry.hash.len() != hash_width
          || entry.hash.iter().all(|byte| *byte == 0)
          || previous.is_some_and(|prior| prior >= entry.name.as_str())
        {
          return Err(IndexFileRevisionReadErrorV1::corrupt(
            "native_revision_btree_leaf_order",
            "B-tree leaf has an invalid child identity or non-strict name order",
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
        || internal.children.iter().any(|hash| hash.len() != hash_width || hash.iter().all(|byte| *byte == 0))
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
