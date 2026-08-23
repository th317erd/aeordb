use std::collections::BTreeSet;
use std::mem::size_of;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::engine::btree::{BTREE_MAX_INTERNAL_KEYS, BTREE_MAX_LEAF_ENTRIES, BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::permission_resolver::{evaluate_ordered_path_permissions, normalize_permission_path};
use crate::engine::permissions::{PathPermissions, PermissionLink};
use crate::engine::{CompressionAlgorithm, EntryType};

use super::database_header::SelectedDatabaseHeaderV4;
use super::entity::EntryTypeV4;
use super::first_authority::{
  FirstAuthorityPublicationErrorV1, LoadedImmutableEntityV1, RootLifecyclePointReadErrorV1, V4FirstAuthorityPublisher,
};
use super::hash::digest_parts;
use super::namespace::{NamespaceTreeEdgeV0, SemanticAvailabilityV1};
use super::read_view::{
  LoadedReadAuthorityV1, ReadViewAuthoritySourceV1, ReadViewAuthorizationFailureV1, ReadViewLifecycleErrorV1, ReadViewSourceErrorV1,
  RootLifecycleObservationV1,
};
use super::read_view_authorization::{PathAuthorizationDecisionV1, SelectedRootPermissionRequestV1, SelectedRootPermissionSourceV1};
use super::root_authority::ImmutableNamespaceAuthorityV1;

// The frozen namespace authority permits a 48 MiB tree entity. Reserve enough
// for that entity, its decoded form, transient validation copies, and the
// smaller admission/control entities before any authority allocation occurs.
const AUTHORITY_PEAK_RESERVATION_BYTES: u64 = 128 * 1024 * 1024;
const AUTHORITY_RETAINED_BASE_BYTES: u64 = 16 * 1024;
const PERMISSION_WORKSPACE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DIRECTORY_ENTITY_BYTES: usize = 48 * 1024 * 1024;
const MAX_FILE_RECORD_ENTITY_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHUNK_ENTITY_BYTES: usize = 2 * 1024 * 1024;
const MAX_PERMISSION_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_PERMISSION_DOCUMENT_CHUNKS: usize = 64;
const MAX_FLAT_DIRECTORY_ENTRIES: usize = 256;
const MAX_BTREE_DEPTH: usize = 128;
const MAX_BTREE_SCAN_NODES: usize = 100_000;
const MAX_DESCENDANT_DEPTH: usize = 10;
const MAX_DESCENDANT_PERMISSION_FILES: usize = 1_000;
const MAX_DESCENDANT_DIRECTORIES: usize = 100_000;

/// One production source for captured v4 authority, lifecycle, and selected
/// permission reads. Callers must use the same process memory coordinator for
/// this source and its `RootReadPinCoordinatorV1`.
#[derive(Clone)]
pub struct NativeReadViewSourceV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
  memory: Arc<MemoryCoordinator>,
  current_configured_grace_ms: u64,
}

struct AccountedLoadedImmutableEntityV1 {
  entity: LoadedImmutableEntityV1,
  _memory: MemoryReservation,
}

impl std::ops::Deref for AccountedLoadedImmutableEntityV1 {
  type Target = LoadedImmutableEntityV1;

  fn deref(&self) -> &Self::Target {
    &self.entity
  }
}

impl NativeReadViewSourceV1 {
  pub const fn new(publisher: Arc<V4FirstAuthorityPublisher>, memory: Arc<MemoryCoordinator>, current_configured_grace_ms: u64) -> Self {
    Self { publisher, memory, current_configured_grace_ms }
  }

  pub fn publisher(&self) -> &Arc<V4FirstAuthorityPublisher> {
    &self.publisher
  }

  pub fn memory_coordinator(&self) -> &Arc<MemoryCoordinator> {
    &self.memory
  }
}

impl ReadViewAuthoritySourceV1 for NativeReadViewSourceV1 {
  fn capture_header(&self, cancellation: &CancellationToken) -> Result<SelectedDatabaseHeaderV4, ReadViewSourceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    let selected = self.publisher.observe().map_err(map_header_error)?.selected;
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    Ok(selected)
  }

  fn load_verified_authority(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<LoadedReadAuthorityV1, ReadViewSourceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    let mut memory = self
      .memory
      .reserve(MemoryOwner::Query, AUTHORITY_PEAK_RESERVATION_BYTES, AdmissionClass::Workload)
      .map_err(|error| ReadViewSourceErrorV1::Memory(error.to_string()))?;
    let authority = self
      .publisher
      .load_namespace_authority_at_captured_header(header, root_hash, cancellation)
      .map_err(map_authority_error)?
      .ok_or(ReadViewSourceErrorV1::RootNotAdmitted)?;
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    let retained = authority_retained_bytes(&authority)?;
    memory.shrink(memory.bytes().saturating_sub(retained)).map_err(|error| ReadViewSourceErrorV1::Memory(error.to_string()))?;
    Ok(LoadedReadAuthorityV1::new_accounted(authority, None, memory))
  }

  fn observe_lifecycle(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<RootLifecycleObservationV1, ReadViewLifecycleErrorV1> {
    self
      .publisher
      .observe_root_lifecycle_at_captured_header(header, root_hash, self.current_configured_grace_ms, cancellation, &self.memory)
      .map_err(map_lifecycle_error)
  }
}

impl SelectedRootPermissionSourceV1 for NativeReadViewSourceV1 {
  fn authorize_selected_root(
    &self,
    header: &SelectedDatabaseHeaderV4,
    authority: &LoadedReadAuthorityV1,
    request: SelectedRootPermissionRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewAuthorizationFailureV1::Canceled);
    }
    let _workspace = self
      .memory
      .reserve(MemoryOwner::Query, PERMISSION_WORKSPACE_BYTES, AdmissionClass::Workload)
      .map_err(|error| ReadViewAuthorizationFailureV1::Unavailable(format!("selected permission memory admission failed: {error}")))?;
    let tree_root = authority.authority.namespace_tree.root_hash.as_slice();
    let direct = evaluate_ordered_path_permissions(request.current_groups(), request.path(), request.operation(), |level| {
      let path = permission_document_path(level);
      self.load_permission_document(header, tree_root, &path, cancellation)
    })?;
    if direct {
      return Ok(Some(PathAuthorizationDecisionV1::direct()));
    }
    if !matches!(
      request.operation(),
      crate::engine::permission_resolver::CrudlifyOp::Read | crate::engine::permission_resolver::CrudlifyOp::List
    ) {
      return Ok(None);
    }
    let children = self.descendant_grant_children(header, tree_root, request.path(), request.current_groups(), cancellation)?;
    Ok(PathAuthorizationDecisionV1::ancestor_navigation(children))
  }
}

impl NativeReadViewSourceV1 {
  fn load_permission_document(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    path: &str,
    cancellation: &CancellationToken,
  ) -> Result<Option<PathPermissions>, ReadViewAuthorizationFailureV1> {
    let Some(entry) = self.resolve_path(header, tree_root, path, cancellation)? else {
      return Ok(None);
    };
    let entry_type = EntryType::from_u8(entry.entry_type).map_err(|error| selected_corrupt(path, error))?;
    if entry_type != EntryType::FileRecord {
      return Err(selected_corrupt(path, "permission path resolves to a non-file entity"));
    }
    let bytes = self.load_file_bytes(header, &entry, path, cancellation)?;
    PathPermissions::deserialize_stored(&bytes, path).map(Some).map_err(|error| selected_corrupt(path, error))
  }

  fn resolve_path(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    path: &str,
    cancellation: &CancellationToken,
  ) -> Result<Option<ChildEntry>, ReadViewAuthorizationFailureV1> {
    let normalized = normalize_permission_path(path);
    if normalized != path || !path.starts_with('/') || path.split('/').any(|segment| segment == "." || segment == "..") {
      return Err(selected_corrupt(path, "selected permission path is not canonical"));
    }
    if path == "/" {
      return Ok(Some(directory_child(tree_root.to_vec(), String::new())));
    }
    let mut directory_hash = tree_root.to_vec();
    let segments = path.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
    if segments.len() > MAX_BTREE_DEPTH {
      return Err(selected_corrupt(path, "selected permission path exceeds the traversal depth bound"));
    }
    for (index, segment) in segments.iter().enumerate() {
      ensure_selected_not_cancelled(cancellation)?;
      let child = self.lookup_directory_child(header, &directory_hash, segment, cancellation)?;
      let Some(child) = child else {
        return Ok(None);
      };
      if index + 1 == segments.len() {
        return Ok(Some(child));
      }
      if EntryType::from_u8(child.entry_type).map_err(|error| selected_corrupt(path, error))? != EntryType::DirectoryIndex {
        return Ok(None);
      }
      directory_hash = child.hash;
    }
    Ok(None)
  }

  fn lookup_directory_child(
    &self,
    header: &SelectedDatabaseHeaderV4,
    directory_hash: &[u8],
    name: &str,
    cancellation: &CancellationToken,
  ) -> Result<Option<ChildEntry>, ReadViewAuthorizationFailureV1> {
    let mut current_hash = directory_hash.to_vec();
    let mut ancestors = BTreeSet::new();
    let mut btree_child = false;
    for _ in 0..MAX_BTREE_DEPTH {
      ensure_selected_not_cancelled(cancellation)?;
      if !ancestors.insert(current_hash.clone()) {
        return Err(selected_corrupt(name, "selected directory B-tree contains a cycle"));
      }
      let entity = self.load_directory_entity(header, &current_hash, cancellation)?;
      if !is_btree_format(&entity.stored_value) {
        if btree_child {
          return Err(selected_corrupt(name, "selected B-tree child uses the flat-directory format"));
        }
        let children = deserialize_child_entries(&entity.stored_value, header.header.hash_algorithm.hash_length(), entity.entity_version)
          .map_err(|error| selected_corrupt(name, error))?;
        if children.len() > MAX_FLAT_DIRECTORY_ENTRIES {
          return Err(selected_corrupt(name, "selected flat directory exceeds its entry bound"));
        }
        validate_sorted_children(&children, name)?;
        return Ok(children.into_iter().find(|child| child.name == name));
      }
      match decode_canonical_btree_node(&entity, header.header.hash_algorithm.hash_length(), name)? {
        BTreeNode::Leaf(leaf) => {
          return Ok(leaf.entries.into_iter().find(|entry| entry.name == name));
        }
        BTreeNode::Internal(internal) => {
          current_hash = internal.children[internal.find_child_index(name)].clone();
          btree_child = true;
        }
      }
    }
    Err(selected_corrupt(name, "selected directory B-tree exceeds the traversal depth bound"))
  }

  fn load_directory_entity(
    &self,
    header: &SelectedDatabaseHeaderV4,
    hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<AccountedLoadedImmutableEntityV1, ReadViewAuthorizationFailureV1> {
    let entity = self
      .load_entity_at_header(header, hash, MAX_DIRECTORY_ENTITY_BYTES, cancellation)?
      .ok_or_else(|| selected_corrupt(&hex::encode(hash), "selected directory entity is missing"))?;
    if entity.entry_type != EntryTypeV4::DirectoryIndex
      || entity.entity_version != 0
      || entity.flags != 0
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != hash
    {
      return Err(selected_corrupt(&hex::encode(hash), "selected directory entity representation is noncanonical"));
    }
    let domain = if is_btree_format(&entity.stored_value) { b"btree:".as_slice() } else { b"dirc:".as_slice() };
    if digest_parts(header.header.hash_algorithm, &[domain, &entity.stored_value]) != hash {
      return Err(selected_corrupt(&hex::encode(hash), "selected directory content identity is invalid"));
    }
    Ok(entity)
  }

  fn load_file_bytes(
    &self,
    header: &SelectedDatabaseHeaderV4,
    entry: &ChildEntry,
    expected_path: &str,
    cancellation: &CancellationToken,
  ) -> Result<Vec<u8>, ReadViewAuthorizationFailureV1> {
    let entity = self
      .load_entity_at_header(header, &entry.hash, MAX_FILE_RECORD_ENTITY_BYTES, cancellation)?
      .ok_or_else(|| selected_corrupt(expected_path, "selected FileRecord is missing"))?;
    if entity.entry_type != EntryTypeV4::FileRecord
      || !matches!(entity.entity_version, 0 | 1)
      || entity.flags != 0
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != entry.hash
      || digest_parts(header.header.hash_algorithm, &[b"filec:", &entity.stored_value]) != entry.hash
    {
      return Err(selected_corrupt(expected_path, "selected FileRecord representation or identity is invalid"));
    }
    let record = FileRecord::deserialize(&entity.stored_value, header.header.hash_algorithm.hash_length(), entity.entity_version)
      .map_err(|error| selected_corrupt(expected_path, error))?;
    if record.path != expected_path
      || record.total_size != entry.total_size
      || record.content_type != entry.content_type
      || record.total_size > MAX_PERMISSION_DOCUMENT_BYTES as u64
    {
      return Err(selected_corrupt(expected_path, "selected FileRecord metadata does not match its directory entry"));
    }
    if record.chunk_hashes.len() > MAX_PERMISSION_DOCUMENT_CHUNKS {
      return Err(selected_corrupt(expected_path, "selected permission FileRecord exceeds its chunk-count bound"));
    }
    let output_length = usize::try_from(record.total_size).map_err(|error| selected_corrupt(expected_path, error))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(output_length).map_err(|error| selected_unavailable(expected_path, error))?;
    for chunk_hash in &record.chunk_hashes {
      ensure_selected_not_cancelled(cancellation)?;
      let chunk = self
        .load_entity_at_header(header, chunk_hash, MAX_CHUNK_ENTITY_BYTES, cancellation)?
        .ok_or_else(|| selected_corrupt(expected_path, format!("selected chunk {} is missing", hex::encode(chunk_hash))))?;
      if chunk.entry_type != EntryTypeV4::Chunk || chunk.entity_version != 0 || chunk.flags != 0 || chunk.key != *chunk_hash {
        return Err(selected_corrupt(expected_path, "selected chunk representation is noncanonical"));
      }
      let remaining = output_length.saturating_sub(bytes.len());
      let decoded = crate::engine::compression::decompress_bounded(&chunk.stored_value, chunk.compression_algorithm, remaining)
        .map_err(|error| selected_corrupt(expected_path, error))?;
      if digest_parts(header.header.hash_algorithm, &[b"chunk:", &decoded]) != *chunk_hash {
        return Err(selected_corrupt(expected_path, "selected chunk content identity is invalid"));
      }
      bytes.extend_from_slice(&decoded);
    }
    if bytes.len() != output_length {
      return Err(selected_corrupt(expected_path, "selected file chunks do not match the declared length"));
    }
    if entity.entity_version == 1 && digest_parts(header.header.hash_algorithm, &[&bytes]) != record.content_hash {
      return Err(selected_corrupt(expected_path, "selected file content hash is invalid"));
    }
    Ok(bytes)
  }

  fn load_entity_at_header(
    &self,
    header: &SelectedDatabaseHeaderV4,
    key: &[u8],
    maximum_total_length: usize,
    cancellation: &CancellationToken,
  ) -> Result<Option<AccountedLoadedImmutableEntityV1>, ReadViewAuthorizationFailureV1> {
    ensure_selected_not_cancelled(cancellation)?;
    let locator = self.publisher.locator(key).map_err(map_selected_authority_error)?;
    let length = locator.as_ref().map_or(0, |locator| locator.total_length as u64);
    if length > maximum_total_length as u64 {
      return Err(selected_corrupt(&hex::encode(key), "selected entity exceeds its role bound"));
    }
    let charge = length
      .checked_mul(2)
      .and_then(|bytes| bytes.checked_add(4096))
      .ok_or_else(|| selected_unavailable(&hex::encode(key), "selected entity memory charge overflow"))?;
    let memory = self
      .memory
      .reserve(MemoryOwner::Query, charge, AdmissionClass::Workload)
      .map_err(|error| selected_unavailable(&hex::encode(key), error))?;
    self
      .publisher
      .load_immutable_entity_at_captured_header(header, key, maximum_total_length, cancellation)
      .map_err(map_selected_authority_error)
      .map(|entity| entity.map(|entity| AccountedLoadedImmutableEntityV1 { entity, _memory: memory }))
  }

  fn descendant_grant_children(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    parent_path: &str,
    current_groups: &[String],
    cancellation: &CancellationToken,
  ) -> Result<BTreeSet<String>, ReadViewAuthorizationFailureV1> {
    let normalized_parent = normalize_navigation_path(parent_path);
    let Some(parent) = self.resolve_path(header, tree_root, &normalized_parent, cancellation)? else {
      return Ok(BTreeSet::new());
    };
    if EntryType::from_u8(parent.entry_type).map_err(|error| selected_corrupt(parent_path, error))? != EntryType::DirectoryIndex {
      return Ok(BTreeSet::new());
    }
    let mut visited_directories = 0usize;
    let mut permission_files = 0usize;
    let mut allowed_children = BTreeSet::new();
    self.scan_descendant_directory(
      header,
      tree_root,
      &normalized_parent,
      &normalized_parent,
      &parent.hash,
      path_depth(&normalized_parent),
      current_groups,
      cancellation,
      &mut visited_directories,
      &mut permission_files,
      &mut allowed_children,
    )?;
    Ok(allowed_children)
  }

  #[allow(clippy::too_many_arguments)]
  fn scan_descendant_directory(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    parent_path: &str,
    directory_path: &str,
    directory_hash: &[u8],
    depth: usize,
    current_groups: &[String],
    cancellation: &CancellationToken,
    visited_directories: &mut usize,
    permission_files: &mut usize,
    allowed_children: &mut BTreeSet<String>,
  ) -> Result<(), ReadViewAuthorizationFailureV1> {
    ensure_selected_not_cancelled(cancellation)?;
    *visited_directories = visited_directories.saturating_add(1);
    if *visited_directories > MAX_DESCENDANT_DIRECTORIES {
      return Err(selected_unavailable(parent_path, "selected descendant permission scan exceeded its directory bound"));
    }
    self.visit_directory_children(header, directory_hash, cancellation, |child| {
      if child.name == ".aeordb-permissions" {
        *permission_files = permission_files.saturating_add(1);
        if *permission_files > MAX_DESCENDANT_PERMISSION_FILES {
          return Err(selected_unavailable(parent_path, "selected descendant permission scan exceeded its permission-file bound"));
        }
        let permission_path = join_path(directory_path, &child.name);
        let Some(document) = self.load_permission_document(header, tree_root, &permission_path, cancellation)? else {
          return Err(selected_corrupt(&permission_path, "listed permission authority disappeared"));
        };
        collect_descendant_children(&document.links, current_groups, parent_path, directory_path, allowed_children);
      } else if EntryType::from_u8(child.entry_type).map_err(|error| selected_corrupt(directory_path, error))? == EntryType::DirectoryIndex
        && depth < MAX_DESCENDANT_DEPTH
      {
        let child_path = join_path(directory_path, &child.name);
        self.scan_descendant_directory(
          header,
          tree_root,
          parent_path,
          &child_path,
          &child.hash,
          depth + 1,
          current_groups,
          cancellation,
          visited_directories,
          permission_files,
          allowed_children,
        )?;
      }
      Ok(())
    })
  }

  fn visit_directory_children(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
    mut visitor: impl FnMut(ChildEntry) -> Result<(), ReadViewAuthorizationFailureV1>,
  ) -> Result<(), ReadViewAuthorizationFailureV1> {
    let mut stack = vec![(root_hash.to_vec(), 0usize, false)];
    let mut visited_nodes = 0usize;
    let mut previous = None;
    while let Some((hash, depth, btree_child)) = stack.pop() {
      ensure_selected_not_cancelled(cancellation)?;
      visited_nodes = visited_nodes.saturating_add(1);
      if depth > MAX_BTREE_DEPTH || visited_nodes > MAX_BTREE_SCAN_NODES {
        return Err(selected_corrupt(&hex::encode(root_hash), "selected directory B-tree exceeds its depth or node bound"));
      }
      let entity = self.load_directory_entity(header, &hash, cancellation)?;
      if !is_btree_format(&entity.stored_value) {
        if btree_child {
          return Err(selected_corrupt(&hex::encode(root_hash), "selected B-tree child uses the flat-directory format"));
        }
        let children = deserialize_child_entries(&entity.stored_value, header.header.hash_algorithm.hash_length(), entity.entity_version)
          .map_err(|error| selected_corrupt(&hex::encode(root_hash), error))?;
        if children.len() > MAX_FLAT_DIRECTORY_ENTRIES {
          return Err(selected_corrupt(&hex::encode(root_hash), "selected flat directory exceeds its entry bound"));
        }
        validate_sorted_children(&children, &hex::encode(root_hash))?;
        for child in children {
          validate_child_order(previous.as_deref(), &child.name)?;
          previous = Some(child.name.clone());
          visitor(child)?;
        }
        continue;
      }
      match decode_canonical_btree_node(&entity, header.header.hash_algorithm.hash_length(), &hex::encode(root_hash))? {
        BTreeNode::Leaf(leaf) => {
          for child in leaf.entries {
            validate_child_order(previous.as_deref(), &child.name)?;
            previous = Some(child.name.clone());
            visitor(child)?;
          }
        }
        BTreeNode::Internal(internal) => {
          for child in internal.children.into_iter().rev() {
            stack.push((child, depth + 1, true));
          }
        }
      }
    }
    Ok(())
  }
}

fn permission_document_path(level: &str) -> String {
  join_path(level, ".aeordb-permissions")
}

fn normalize_navigation_path(path: &str) -> String {
  let normalized = normalize_permission_path(path);
  if normalized == "/" {
    normalized
  } else {
    normalized.trim_end_matches('/').to_string()
  }
}

fn join_path(parent: &str, child: &str) -> String {
  if parent == "/" {
    format!("/{child}")
  } else {
    format!("{}/{child}", parent.trim_end_matches('/'))
  }
}

fn path_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

fn directory_child(hash: Vec<u8>, name: String) -> ChildEntry {
  ChildEntry {
    entry_type: EntryType::DirectoryIndex.to_u8(),
    hash,
    total_size: 0,
    created_at: 0,
    updated_at: 0,
    name,
    content_type: None,
    virtual_time: 0,
    node_id: 0,
  }
}

fn decode_canonical_btree_node(
  entity: &LoadedImmutableEntityV1,
  hash_width: usize,
  path: &str,
) -> Result<BTreeNode, ReadViewAuthorizationFailureV1> {
  let node =
    BTreeNode::deserialize(&entity.stored_value, hash_width, entity.entity_version).map_err(|error| selected_corrupt(path, error))?;
  let canonical = node.serialize(hash_width).map_err(|error| selected_corrupt(path, error))?;
  if canonical != entity.stored_value {
    return Err(selected_corrupt(path, "selected B-tree node is not canonically encoded"));
  }
  match &node {
    BTreeNode::Leaf(leaf) => {
      if leaf.entries.len() > BTREE_MAX_LEAF_ENTRIES {
        return Err(selected_corrupt(path, "selected B-tree leaf exceeds its canonical fanout"));
      }
      validate_sorted_children(&leaf.entries, path)?;
    }
    BTreeNode::Internal(internal) => {
      if internal.keys.is_empty() || internal.keys.len() > BTREE_MAX_INTERNAL_KEYS {
        return Err(selected_corrupt(path, "selected B-tree internal node has noncanonical fanout"));
      }
      for pair in internal.keys.windows(2) {
        if pair[0] >= pair[1] {
          return Err(selected_corrupt(path, "selected B-tree separator keys are not strictly increasing"));
        }
      }
      if internal.children.iter().any(|child| child.iter().all(|byte| *byte == 0)) {
        return Err(selected_corrupt(path, "selected B-tree contains a zero child identity"));
      }
    }
  }
  Ok(node)
}

fn validate_sorted_children(children: &[ChildEntry], path: &str) -> Result<(), ReadViewAuthorizationFailureV1> {
  for pair in children.windows(2) {
    validate_child_order(Some(&pair[0].name), &pair[1].name).map_err(|error| selected_corrupt(path, error))?;
  }
  Ok(())
}

fn validate_child_order(previous: Option<&str>, current: &str) -> Result<(), ReadViewAuthorizationFailureV1> {
  if previous.is_some_and(|previous| previous >= current) {
    Err(selected_corrupt(current, "selected directory child names are not strictly increasing"))
  } else {
    Ok(())
  }
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
    let target = link.path_pattern.as_ref().map_or_else(|| document_directory.to_string(), |name| join_path(document_directory, name));
    if let Some(child) = next_segment_below(parent_path, &target) {
      output.insert(child.to_string());
    }
  }
}

fn next_segment_below<'a>(parent: &str, target: &'a str) -> Option<&'a str> {
  let parent = if parent == "/" { "" } else { parent.trim_end_matches('/') };
  let suffix = target.strip_prefix(parent)?;
  if !suffix.starts_with('/') {
    return None;
  }
  let remainder = &suffix[1..];
  (!remainder.is_empty()).then(|| remainder.split('/').next()).flatten()
}

fn ensure_selected_not_cancelled(cancellation: &CancellationToken) -> Result<(), ReadViewAuthorizationFailureV1> {
  if cancellation.is_cancelled() {
    Err(ReadViewAuthorizationFailureV1::Canceled)
  } else {
    Ok(())
  }
}

fn map_selected_authority_error(error: FirstAuthorityPublicationErrorV1) -> ReadViewAuthorizationFailureV1 {
  if error.code() == "captured_authority_cancelled" {
    ReadViewAuthorizationFailureV1::Canceled
  } else if authority_error_is_unavailable(&error) {
    ReadViewAuthorizationFailureV1::Unavailable(error.to_string())
  } else {
    ReadViewAuthorizationFailureV1::Corrupt(error.to_string())
  }
}

fn selected_corrupt(path: &str, error: impl std::fmt::Display) -> ReadViewAuthorizationFailureV1 {
  ReadViewAuthorizationFailureV1::Corrupt(format!("selected permission authority at {path}: {error}"))
}

fn selected_unavailable(path: &str, error: impl std::fmt::Display) -> ReadViewAuthorizationFailureV1 {
  ReadViewAuthorizationFailureV1::Unavailable(format!("selected permission authority at {path}: {error}"))
}

fn authority_retained_bytes(authority: &ImmutableNamespaceAuthorityV1) -> Result<u64, ReadViewSourceErrorV1> {
  let mut bytes = AUTHORITY_RETAINED_BASE_BYTES
    .checked_add(size_of::<ImmutableNamespaceAuthorityV1>() as u64)
    .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  for value in [
    &authority.root.root_hash,
    &authority.root.namespace_tree_root,
    &authority.root.semantic_state_root,
    &authority.namespace_tree.root_hash,
    &authority.semantic_state.object_id,
    &authority.admission.namespace_root,
    &authority.admission.authority_identity_digest,
    &authority.admission.authority_after,
    &authority.admission.prepare_payload_hash,
  ] {
    bytes = bytes
      .checked_add(value.capacity() as u64)
      .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  }
  bytes = bytes
    .checked_add((authority.namespace_tree.edges.capacity() * size_of::<NamespaceTreeEdgeV0>()) as u64)
    .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  for edge in &authority.namespace_tree.edges {
    let edge_bytes = match edge {
      NamespaceTreeEdgeV0::Entry { name, identity, .. } => name.capacity().saturating_add(identity.capacity()),
      NamespaceTreeEdgeV0::BTreeNode { identity } => identity.capacity(),
    };
    bytes =
      bytes.checked_add(edge_bytes as u64).ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  }
  if let SemanticAvailabilityV1::Complete { compiler_fingerprint, semantic_registry_fingerprint, catalog_root, .. } =
    &authority.semantic_state.availability
  {
    for value in [compiler_fingerprint, semantic_registry_fingerprint, catalog_root] {
      bytes = bytes
        .checked_add(value.capacity() as u64)
        .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
    }
  }
  if bytes > AUTHORITY_PEAK_RESERVATION_BYTES {
    return Err(ReadViewSourceErrorV1::Memory(format!(
      "retained authority requires {bytes} bytes, exceeding its {AUTHORITY_PEAK_RESERVATION_BYTES}-byte admitted peak",
    )));
  }
  Ok(bytes)
}

fn map_header_error(error: FirstAuthorityPublicationErrorV1) -> ReadViewSourceErrorV1 {
  if authority_error_is_unavailable(&error) {
    ReadViewSourceErrorV1::HeaderUnavailable(error.to_string())
  } else {
    ReadViewSourceErrorV1::HeaderCorrupt(error.to_string())
  }
}

fn map_authority_error(error: FirstAuthorityPublicationErrorV1) -> ReadViewSourceErrorV1 {
  if error.code() == "captured_authority_cancelled" {
    ReadViewSourceErrorV1::Canceled
  } else if authority_error_is_unavailable(&error) {
    ReadViewSourceErrorV1::AuthorityUnavailable(error.to_string())
  } else {
    ReadViewSourceErrorV1::AuthorityCorrupt(error.to_string())
  }
}

fn authority_error_is_unavailable(error: &FirstAuthorityPublicationErrorV1) -> bool {
  matches!(error.code(), "engine_failure" | "native_io_failure" | "durability_failure")
}

fn map_lifecycle_error(error: RootLifecyclePointReadErrorV1) -> ReadViewLifecycleErrorV1 {
  match error {
    RootLifecyclePointReadErrorV1::Canceled => ReadViewLifecycleErrorV1::Canceled,
    RootLifecyclePointReadErrorV1::Memory(source) => ReadViewLifecycleErrorV1::Memory(source.to_string()),
    RootLifecyclePointReadErrorV1::Authority(source) if authority_error_is_unavailable(&source) => {
      ReadViewLifecycleErrorV1::Unavailable(source.to_string())
    }
    RootLifecyclePointReadErrorV1::Authority(source) => ReadViewLifecycleErrorV1::Corrupt(source.to_string()),
    RootLifecyclePointReadErrorV1::Invalid { code, message } => ReadViewLifecycleErrorV1::Corrupt(format!("{code}: {message}")),
    RootLifecyclePointReadErrorV1::Format(source) => ReadViewLifecycleErrorV1::Corrupt(source.to_string()),
  }
}
