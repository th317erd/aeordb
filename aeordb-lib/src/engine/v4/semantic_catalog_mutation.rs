//! Bounded, storage-neutral COW of an admitted immutable semantic catalog.
//!
//! This planner changes bindings only. The compiler owns definition closure and
//! distinct-definition counts; the existing publication owner durably stores
//! dependencies and rechecks captured authority before selecting a new root.

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::hash::digest_parts;
use super::namespace::{
  EncodedSemanticObjectV1, SemanticCatalogChildV1, SemanticCatalogInternalV1, SemanticCatalogLeafV1, SemanticCatalogNodeV1,
  SemanticCatalogRecordV1, decode_semantic_catalog_node, encode_semantic_catalog_internal, encode_semantic_catalog_leaf,
  validate_catalog_owner_key,
};
use super::reader::{FormatError, MalformedInputClass};
use super::semantic_catalog::{SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1};

type Result<T> = std::result::Result<T, SemanticCatalogReadErrorV1>;
const INTERNAL_CAP: usize = 65_536;
const LEAF_CAP: usize = 1_048_576;

#[derive(Clone, Copy, Debug)]
pub struct SemanticCatalogSnapshotV1<'a> {
  pub root_object_id: Option<&'a [u8]>,
  pub record_count: u64,
  pub node_count: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum SemanticCatalogMutationV1<'a> {
  Upsert(SemanticCatalogRecordV1<'a>),
  Remove { record_kind: u16, owner_key: &'a [u8] },
}

impl<'a> SemanticCatalogMutationV1<'a> {
  fn key(self) -> (u16, &'a [u8]) {
    match self {
      Self::Upsert(record) => (record.record_kind, record.owner_key),
      Self::Remove { record_kind, owner_key } => (record_kind, owner_key),
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct SemanticCatalogMutationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub snapshot: SemanticCatalogSnapshotV1<'a>,
  pub mutation: SemanticCatalogMutationV1<'a>,
  pub maximum_workspace_bytes: usize,
}

/// Owns its admitted output memory until the caller finishes publication.
/// Borrow the objects; detaching them from their reservation is not supported.
pub struct SemanticCatalogMutationPlanV1 {
  source_root_object_id: Option<Vec<u8>>,
  root_object_id: Option<Vec<u8>>,
  record_count: u64,
  node_count: u64,
  objects: Vec<EncodedSemanticObjectV1>,
  _memory: MemoryReservation,
}

impl SemanticCatalogMutationPlanV1 {
  pub fn source_root_object_id(&self) -> Option<&[u8]> {
    self.source_root_object_id.as_deref()
  }

  pub fn root_object_id(&self) -> Option<&[u8]> {
    self.root_object_id.as_deref()
  }

  pub const fn record_count(&self) -> u64 {
    self.record_count
  }

  pub const fn node_count(&self) -> u64 {
    self.node_count
  }

  /// New catalog nodes in child-before-parent order. Definitions are external
  /// prerequisites and are never loaded or synthesized by this planner.
  pub fn objects(&self) -> &[EncodedSemanticObjectV1] {
    &self.objects
  }

  pub fn is_unchanged(&self) -> bool {
    self.objects.is_empty() && self.root_object_id == self.source_root_object_id
  }
}

/// Plan one binding update against an already-admitted, captured catalog.
/// The source must enforce each kind's preallocation bound and retain the same
/// captured read authority throughout this operation. Only the affected path
/// (and at most one collapsing sibling) is read; untouched subtree closure and
/// the supplied global node count are prerequisites, not reverified by a scan.
pub fn plan_semantic_catalog_mutation_v1(
  request: SemanticCatalogMutationRequestV1<'_>,
  source: &dyn SemanticCatalogObjectSourceV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<SemanticCatalogMutationPlanV1> {
  check_cancellation(is_cancelled)?;
  validate_request(request)?;
  let width = request.hash_algorithm.hash_length();
  let workspace = semantic_catalog_mutation_workspace_bytes_v1(request.hash_algorithm)?;
  if workspace > request.maximum_workspace_bytes {
    return Err(resource("catalog_mutation_workspace", "bounded path workspace exceeds the caller's limit"));
  }
  let admitted_bytes = u64::try_from(workspace).map_err(|source| resource("catalog_mutation_workspace", source.to_string()))?;
  let reservation = memory
    .reserve(MemoryOwner::Task, admitted_bytes, AdmissionClass::Workload)
    .map_err(|source| resource("catalog_mutation_memory", source.to_string()))?;
  let mut planner = Planner { request, source, is_cancelled, reservation, objects: allocate(width + 3)?, visited: 0, node_delta: 0 };
  let (root_object_id, record_count, node_count) = planner.execute()?;
  planner.check()?;
  Ok(SemanticCatalogMutationPlanV1 {
    source_root_object_id: request.snapshot.root_object_id.map(copy_bytes).transpose()?,
    root_object_id,
    record_count,
    node_count,
    objects: planner.objects,
    _memory: planner.reservation,
  })
}

pub(super) fn semantic_catalog_mutation_workspace_bytes_v1(algorithm: HashAlgorithm) -> Result<usize> {
  // Two full H-bounded internal paths, fixed metadata, and six leaf-sized
  // buffers cover reads, decoding and replacement. Independent of catalog size.
  algorithm
    .hash_length()
    .checked_mul(2)
    .and_then(|count| count.checked_add(8))
    .and_then(|count| count.checked_mul(INTERNAL_CAP))
    .and_then(|bytes| bytes.checked_add(6 * LEAF_CAP))
    .ok_or_else(|| resource("catalog_mutation_workspace", "workspace size overflow"))
}

struct Frame {
  value: Vec<u8>,
  edge: u8,
}

struct Subtree {
  object_id: Vec<u8>,
  records: u64,
}

enum PathChange {
  Unchanged,
  Replace(Option<Subtree>),
}

struct Planner<'a> {
  request: SemanticCatalogMutationRequestV1<'a>,
  source: &'a dyn SemanticCatalogObjectSourceV1,
  is_cancelled: &'a dyn Fn() -> bool,
  reservation: MemoryReservation,
  objects: Vec<EncodedSemanticObjectV1>,
  visited: u64,
  node_delta: i64,
}

impl Planner<'_> {
  fn check(&self) -> Result<()> {
    check_cancellation(self.is_cancelled)?;
    self.reservation.check_admission().map_err(|source| resource("catalog_mutation_memory", source.to_string()))
  }

  fn execute(&mut self) -> Result<(Option<Vec<u8>>, u64, u64)> {
    let Some(root) = self.request.snapshot.root_object_id else {
      return match self.request.mutation {
        SemanticCatalogMutationV1::Remove { .. } => Ok((None, 0, 0)),
        SemanticCatalogMutationV1::Upsert(record) => {
          let leaf = self.leaf(&[record])?;
          Ok((Some(leaf.object_id), 1, 1))
        }
      };
    };
    let algorithm = self.request.hash_algorithm;
    let width = algorithm.hash_length();
    let (kind, owner) = self.request.mutation.key();
    let lookup = digest_parts(algorithm, &[b"aeordb.semantic-catalog-key.v1\0", &kind.to_le_bytes(), owner]);
    let mut frames = allocate(width)?;
    let mut prefix = allocate(width)?;
    let mut identity = copy_bytes(root)?;
    let mut expected_records = self.request.snapshot.record_count;
    let change = loop {
      let bytes = self.load_node(&identity)?;
      let node = decode_node(&bytes, algorithm)?;
      validate_node_closure(&node, &prefix, expected_records)?;
      match node {
        SemanticCatalogNodeV1::Leaf(leaf) => break self.change_leaf(&leaf, &lookup, prefix.len())?,
        SemanticCatalogNodeV1::Internal(internal) => {
          let depth = usize::from(internal.depth());
          let end = depth + internal.prefix().len();
          if let Some(difference) = internal.prefix().iter().zip(&lookup[depth..end]).position(|(left, right)| left != right) {
            break self.split_internal(&internal, &lookup, difference)?;
          }
          let edge = lookup[end];
          let children = collect_children(&internal)?;
          let selected = children.iter().find(|child| child.edge == edge).copied();
          let Some(child) = selected else {
            break self.add_missing_child(&internal, &children, edge)?;
          };
          if frames.len() >= width {
            return Err(corrupt("catalog_mutation_depth", "catalog path exceeds the hash width"));
          }
          identity = copy_bytes(child.object_id)?;
          expected_records = child.record_count;
          prefix.extend_from_slice(internal.prefix());
          prefix.push(edge);
          drop(children);
          frames.push(Frame { value: bytes, edge });
        }
      }
    };
    let PathChange::Replace(mut replacement) = change else {
      return Ok((Some(copy_bytes(root)?), self.request.snapshot.record_count, self.request.snapshot.node_count));
    };
    while let Some(frame) = frames.pop() {
      self.check()?;
      replacement = self.rebuild_parent(&frame, &lookup, replacement)?;
    }
    let node_count = self
      .request
      .snapshot
      .node_count
      .checked_add_signed(self.node_delta)
      .ok_or_else(|| corrupt("catalog_mutation_count", "catalog node count overflow or underflow"))?;
    let (identity, records) = match replacement {
      Some(subtree) => (Some(subtree.object_id), subtree.records),
      None => (None, 0),
    };
    validate_snapshot(SemanticCatalogSnapshotV1 { root_object_id: identity.as_deref(), record_count: records, node_count }, width)?;
    Ok((identity, records, node_count))
  }

  fn load_node(&mut self, identity: &[u8]) -> Result<Vec<u8>> {
    self.check()?;
    if self.visited >= self.request.snapshot.node_count || self.visited >= self.request.hash_algorithm.hash_length() as u64 + 2 {
      return Err(corrupt("catalog_mutation_depth", "path reads exceed captured node count or bounded depth"));
    }
    self.visited += 1;
    let leaf = self.source.load_semantic_object(2, identity)?;
    self.check()?;
    let internal = self.source.load_semantic_object(3, identity)?;
    self.check()?;
    let (kind, bytes) = match (leaf, internal) {
      (Some(bytes), None) => (2, bytes),
      (None, Some(bytes)) => (3, bytes),
      (None, None) => return Err(corrupt("catalog_mutation_missing", "captured catalog node is absent")),
      (Some(_), Some(_)) => return Err(corrupt("catalog_mutation_ambiguous", "catalog node appears under both kinds")),
    };
    if bytes.len() > if kind == 2 { LEAF_CAP } else { INTERNAL_CAP } {
      return Err(corrupt("catalog_mutation_node_size", "stored node exceeds its kind bound"));
    }
    let node = decode_node(&bytes, self.request.hash_algorithm)?;
    let actual_kind = match node {
      SemanticCatalogNodeV1::Leaf(_) => 2,
      SemanticCatalogNodeV1::Internal(_) => 3,
    };
    if kind != actual_kind || node.object_id() != identity {
      return Err(corrupt("catalog_mutation_identity", "stored node kind or identity disagrees with its requested locator"));
    }
    Ok(bytes)
  }

  fn append_object(&mut self, object: EncodedSemanticObjectV1, records: u64) -> Result<Subtree> {
    self.check()?;
    if self.objects.len() >= self.request.hash_algorithm.hash_length() + 3 {
      return Err(corrupt("catalog_mutation_output_bound", "new node count exceeds one bounded path"));
    }
    let object_id = copy_bytes(&object.object_id)?;
    self.objects.push(object);
    Ok(Subtree { object_id, records })
  }

  fn leaf(&mut self, records: &[SemanticCatalogRecordV1<'_>]) -> Result<Subtree> {
    self.check()?;
    let object = encode_semantic_catalog_leaf(records, self.request.hash_algorithm).map_err(encoding_error)?;
    self.append_object(object, records.len() as u64)
  }

  fn internal(&mut self, depth: u16, prefix: &[u8], children: &[SemanticCatalogChildV1<'_>]) -> Result<Subtree> {
    self.check()?;
    let records = children.iter().try_fold(0u64, |count, child| {
      count.checked_add(child.record_count).ok_or_else(|| corrupt("catalog_mutation_count", "subtree record count overflow"))
    })?;
    let object = encode_semantic_catalog_internal(depth, prefix, children, self.request.hash_algorithm).map_err(encoding_error)?;
    self.append_object(object, records)
  }

  fn change_leaf(&mut self, leaf: &SemanticCatalogLeafV1<'_>, lookup: &[u8], depth: usize) -> Result<PathChange> {
    if leaf.lookup_digest() != lookup {
      let SemanticCatalogMutationV1::Upsert(record) = self.request.mutation else {
        return Ok(PathChange::Unchanged);
      };
      let branch = (depth..lookup.len())
        .find(|position| leaf.lookup_digest()[*position] != lookup[*position])
        .ok_or_else(|| corrupt("catalog_mutation_digest", "distinct lookup digests have no differing byte"))?;
      let inserted = self.leaf(&[record])?;
      let mut children = [
        SemanticCatalogChildV1 {
          edge: leaf.lookup_digest()[branch],
          record_count: u64::from(leaf.record_count()),
          object_id: leaf.object_id(),
        },
        SemanticCatalogChildV1 { edge: lookup[branch], record_count: 1, object_id: &inserted.object_id },
      ];
      children.sort_by_key(|child| child.edge);
      let parent = self.internal(depth as u16, &lookup[depth..branch], &children)?;
      self.node_delta += 2;
      return Ok(PathChange::Replace(Some(parent)));
    }
    let key = self.request.mutation.key();
    let mut records = allocate(leaf.record_count() as usize + 1)?;
    let mut changed = false;
    let mut inserted = false;
    for (index, existing) in leaf.records().enumerate() {
      if index % 128 == 0 {
        self.check()?;
      }
      let existing = existing.map_err(stored_error)?;
      let existing_key = (existing.record_kind, existing.owner_key);
      if existing_key == key {
        match self.request.mutation {
          SemanticCatalogMutationV1::Remove { .. } => changed = true,
          SemanticCatalogMutationV1::Upsert(record) => {
            changed = record != existing;
            inserted = true;
            records.push(record);
          }
        }
        continue;
      }
      if !inserted && existing_key > key {
        if let SemanticCatalogMutationV1::Upsert(record) = self.request.mutation {
          records.push(record);
          inserted = true;
          changed = true;
        }
      }
      records.push(existing);
    }
    if !inserted {
      if let SemanticCatalogMutationV1::Upsert(record) = self.request.mutation {
        records.push(record);
        changed = true;
      }
    }
    if !changed {
      return Ok(PathChange::Unchanged);
    }
    if records.is_empty() {
      self.node_delta -= 1;
      return Ok(PathChange::Replace(None));
    }
    Ok(PathChange::Replace(Some(self.leaf(&records)?)))
  }

  fn split_internal(&mut self, node: &SemanticCatalogInternalV1<'_>, lookup: &[u8], difference: usize) -> Result<PathChange> {
    let SemanticCatalogMutationV1::Upsert(record) = self.request.mutation else {
      return Ok(PathChange::Unchanged);
    };
    let depth = usize::from(node.depth());
    let branch = depth + difference;
    let old_children = collect_children(node)?;
    let old = self.internal((branch + 1) as u16, &node.prefix()[difference + 1..], &old_children)?;
    let inserted = self.leaf(&[record])?;
    let mut children = [
      SemanticCatalogChildV1 { edge: node.prefix()[difference], record_count: old.records, object_id: &old.object_id },
      SemanticCatalogChildV1 { edge: lookup[branch], record_count: 1, object_id: &inserted.object_id },
    ];
    children.sort_by_key(|child| child.edge);
    let parent = self.internal(node.depth(), &node.prefix()[..difference], &children)?;
    self.node_delta += 2;
    Ok(PathChange::Replace(Some(parent)))
  }

  fn add_missing_child(
    &mut self,
    node: &SemanticCatalogInternalV1<'_>,
    children: &[SemanticCatalogChildV1<'_>],
    edge: u8,
  ) -> Result<PathChange> {
    let SemanticCatalogMutationV1::Upsert(record) = self.request.mutation else {
      return Ok(PathChange::Unchanged);
    };
    let inserted = self.leaf(&[record])?;
    let mut updated = allocate(children.len() + 1)?;
    updated.extend_from_slice(children);
    let position = updated.partition_point(|child| child.edge < edge);
    updated.insert(position, SemanticCatalogChildV1 { edge, record_count: 1, object_id: &inserted.object_id });
    let parent = self.internal(node.depth(), node.prefix(), &updated)?;
    self.node_delta += 1;
    Ok(PathChange::Replace(Some(parent)))
  }

  fn rebuild_parent(&mut self, frame: &Frame, lookup: &[u8], replacement: Option<Subtree>) -> Result<Option<Subtree>> {
    let SemanticCatalogNodeV1::Internal(node) = decode_node(&frame.value, self.request.hash_algorithm)? else {
      return Err(corrupt("catalog_mutation_frame", "retained path frame is not internal"));
    };
    let mut children = collect_children(&node)?;
    let position = children
      .iter()
      .position(|child| child.edge == frame.edge)
      .ok_or_else(|| corrupt("catalog_mutation_frame", "retained path frame lost its selected edge"))?;
    if let Some(subtree) = &replacement {
      children[position] = SemanticCatalogChildV1 { edge: frame.edge, record_count: subtree.records, object_id: &subtree.object_id };
    } else {
      children.remove(position);
    }
    if children.len() >= 2 {
      return Ok(Some(self.internal(node.depth(), node.prefix(), &children)?));
    }
    let child = children.first().ok_or_else(|| corrupt("catalog_mutation_frame", "internal collapse has no surviving child"))?;
    let bytes = self.load_node(child.object_id)?;
    let sibling = decode_node(&bytes, self.request.hash_algorithm)?;
    let depth = usize::from(node.depth());
    let mut prefix = allocate(self.request.hash_algorithm.hash_length())?;
    prefix.extend_from_slice(&lookup[..depth]);
    prefix.extend_from_slice(node.prefix());
    prefix.push(child.edge);
    validate_node_closure(&sibling, &prefix, child.record_count)?;
    self.node_delta -= 1;
    match sibling {
      SemanticCatalogNodeV1::Leaf(leaf) => Ok(Some(Subtree { object_id: copy_bytes(leaf.object_id())?, records: child.record_count })),
      SemanticCatalogNodeV1::Internal(internal) => {
        let mut compressed = allocate(self.request.hash_algorithm.hash_length())?;
        compressed.extend_from_slice(node.prefix());
        compressed.push(child.edge);
        compressed.extend_from_slice(internal.prefix());
        let grandchildren = collect_children(&internal)?;
        Ok(Some(self.internal(node.depth(), &compressed, &grandchildren)?))
      }
    }
  }
}

fn validate_request(request: SemanticCatalogMutationRequestV1<'_>) -> Result<()> {
  let width = request.hash_algorithm.hash_length();
  validate_snapshot(request.snapshot, width)?;
  let (kind, owner) = request.mutation.key();
  validate_catalog_owner_key(kind, owner, width).map_err(stored_error)?;
  if matches!(kind, 3..=7) && owner.iter().all(|byte| *byte == 0) {
    return Err(corrupt("catalog_mutation_binding_owner", "definition owner identity must be nonzero for both upsert and removal"));
  }
  if let SemanticCatalogMutationV1::Upsert(record) = request.mutation {
    for identity in [record.semantic_id, record.definition_object_id] {
      if identity.len() != width || identity.iter().all(|byte| *byte == 0) {
        return Err(corrupt("catalog_mutation_binding_identity", "binding identities must be nonzero and exactly database-width"));
      }
    }
    if matches!(kind, 3..=7) && record.owner_key != record.semantic_id {
      return Err(corrupt("catalog_mutation_binding_owner", "definition owner key must equal the complete semantic definition ID"));
    }
  }
  Ok(())
}

fn validate_snapshot(snapshot: SemanticCatalogSnapshotV1<'_>, width: usize) -> Result<()> {
  match snapshot.root_object_id {
    None if snapshot.record_count == 0 && snapshot.node_count == 0 => Ok(()),
    Some(identity)
      if identity.len() == width
        && identity.iter().any(|byte| *byte != 0)
        && snapshot.record_count > 0
        && snapshot.node_count > 0
        && snapshot.node_count <= snapshot.record_count.saturating_mul(2).saturating_sub(1) =>
    {
      Ok(())
    }
    _ => Err(corrupt("catalog_mutation_snapshot", "catalog root presence, width or exact counts are inconsistent")),
  }
}

fn validate_node_closure(node: &SemanticCatalogNodeV1<'_>, prefix: &[u8], expected_records: u64) -> Result<()> {
  let valid = match node {
    SemanticCatalogNodeV1::Leaf(leaf) => leaf.lookup_digest().starts_with(prefix) && u64::from(leaf.record_count()) == expected_records,
    SemanticCatalogNodeV1::Internal(internal) => {
      usize::from(internal.depth()) == prefix.len() && internal.subtree_record_count() == expected_records
    }
  };
  if !valid {
    return Err(corrupt("catalog_mutation_closure", "node disagrees with its parent's depth, prefix or exact record count"));
  }
  Ok(())
}

fn collect_children<'a>(node: &SemanticCatalogInternalV1<'a>) -> Result<Vec<SemanticCatalogChildV1<'a>>> {
  let mut children = allocate(usize::from(node.child_count()))?;
  for child in node.children() {
    children.push(child.map_err(stored_error)?);
  }
  Ok(children)
}

fn decode_node(bytes: &[u8], algorithm: HashAlgorithm) -> Result<SemanticCatalogNodeV1<'_>> {
  decode_semantic_catalog_node(bytes, algorithm).map_err(stored_error)
}

fn check_cancellation(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  if is_cancelled() {
    return Err(SemanticCatalogReadErrorV1::cancelled("catalog_mutation_cancelled", "catalog mutation was cancelled"));
  }
  Ok(())
}

fn allocate<T>(capacity: usize) -> Result<Vec<T>> {
  let mut output = Vec::new();
  output.try_reserve_exact(capacity).map_err(|source| resource("catalog_mutation_allocation", source.to_string()))?;
  Ok(output)
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
  let mut output = allocate(bytes.len())?;
  output.extend_from_slice(bytes);
  Ok(output)
}

fn stored_error(error: FormatError) -> SemanticCatalogReadErrorV1 {
  corrupt(error.code(), error.context())
}

fn encoding_error(error: FormatError) -> SemanticCatalogReadErrorV1 {
  if error.class() == MalformedInputClass::AllocationAmplification {
    return resource(error.code(), error.context());
  }
  stored_error(error)
}

fn corrupt(code: &'static str, context: impl Into<String>) -> SemanticCatalogReadErrorV1 {
  SemanticCatalogReadErrorV1::corrupt(code, context)
}

fn resource(code: &'static str, context: impl Into<String>) -> SemanticCatalogReadErrorV1 {
  SemanticCatalogReadErrorV1::resource(code, context)
}
