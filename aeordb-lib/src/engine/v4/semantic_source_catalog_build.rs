//! Bounded ordered construction, not source discovery or publication authority.
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use super::super::parser_registry_compiler::SemanticCompilationErrorV1;
use super::super::reader::FormatError;
use super::super::scope::validate_canonical_absolute_path;
use super::super::system_control::decode_system_control;
use super::{
  encode_semantic_source_internal_v1, encode_semantic_source_leaf_v1, SemanticSourceChildV1, SemanticSourceLeafEntryV1, NODE_BODY_CAP,
  PATH_CAP,
};

type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;
type NodePairSink<'a> = dyn FnMut(&[u8], &[u8]) -> Result<()> + 'a;
const ERROR_PATH: &str = "<semantic-source-catalog-build>";

#[derive(Clone, Copy, Debug)]
pub struct SemanticSourceCatalogBuildRequestV1 {
  pub database_id: [u8; 16],
  pub hash_algorithm: HashAlgorithm,
  pub expected_path_count: u64,
  pub maximum_path_bytes: usize,
  pub maximum_workspace_bytes: usize,
  pub maximum_node_pairs: u64,
  pub maximum_output_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct SemanticSourceCatalogPairRowV1 {
  pub path: String,
  pub base_file_record_id: Option<Vec<u8>>,
  pub requested_file_record_id: Option<Vec<u8>>,
}

pub struct SemanticSourceCatalogPairV1 {
  base_root: Vec<u8>,
  requested_root: Vec<u8>,
  path_count: u64,
  node_count: u64,
  _memory: MemoryReservation,
}

impl SemanticSourceCatalogPairV1 {
  pub fn base_root(&self) -> &[u8] {
    &self.base_root
  }
  pub fn requested_root(&self) -> &[u8] {
    &self.requested_root
  }
  pub const fn path_count(&self) -> u64 {
    self.path_count
  }
  pub const fn node_count(&self) -> u64 {
    self.node_count
  }
}

/// The caller owns iterator and sink storage admission. Each callback receives
/// one aligned pair of encoded nodes, children before parents. Partial emitted
/// bytes are not a completed result or a durable retention/resume permit.
pub fn build_semantic_source_catalog_pair_v1(
  request: SemanticSourceCatalogBuildRequestV1,
  rows: impl IntoIterator<Item = Result<SemanticSourceCatalogPairRowV1>>,
  emit: &mut NodePairSink<'_>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<SemanticSourceCatalogPairV1> {
  let mut builder = Builder::new(request, emit, memory, is_cancelled)?;
  let mut rows = rows.into_iter();
  loop {
    builder.check()?;
    let next = rows.next();
    builder.check()?;
    let Some(row) = next else { break };
    builder.row(row?)?;
  }
  drop(rows);
  if builder.path_count != request.expected_path_count {
    return Err(invalid("source stream ended before its expected path count"));
  }
  builder.finish()
}

struct Subtree {
  minimum: String,
  base: Vec<u8>,
  requested: Vec<u8>,
  paths: u64,
  nodes: u64,
}

struct Builder<'a, 'sink> {
  request: SemanticSourceCatalogBuildRequestV1,
  emit: &'a mut NodePairSink<'sink>,
  is_cancelled: &'a dyn Fn() -> bool,
  memory: MemoryReservation,
  workspace: u64,
  rows: Vec<SemanticSourceCatalogPairRowV1>,
  leaf_bytes: usize,
  previous: Option<String>,
  forest: Vec<Option<Subtree>>,
  path_count: u64,
  node_count: u64,
  output_bytes: u64,
}

impl<'a, 'sink> Builder<'a, 'sink> {
  fn new(
    request: SemanticSourceCatalogBuildRequestV1,
    emit: &'a mut NodePairSink<'sink>,
    memory: &MemoryCoordinator,
    is_cancelled: &'a dyn Fn() -> bool,
  ) -> Result<Self> {
    cancelled(is_cancelled)?;
    if request.database_id == [0; 16] || request.expected_path_count == 0 || request.expected_path_count > u64::MAX / 2 {
      return Err(invalid("database identity and bounded nonempty source count are required"));
    }
    if !(1..=PATH_CAP).contains(&request.maximum_path_bytes) || request.maximum_node_pairs == 0 || request.maximum_output_bytes == 0 {
      return Err(resource("positive bounded path, node and output limits are required"));
    }
    let leaf_capacity = request.expected_path_count.min(256) as usize;
    let levels = (u64::BITS - request.expected_path_count.leading_zeros()) as usize;
    let width = request.hash_algorithm.hash_length();
    // Charge full admitted owned capacities, not merely encoded path lengths.
    // Include one current row, the previous path, two merging minima/roots,
    // borrowed projections, both encoded nodes, hash state and result scratch.
    let workspace = (leaf_capacity + levels + 4)
      .checked_mul(request.maximum_path_bytes)
      .and_then(|bytes| bytes.checked_add((leaf_capacity + levels + 4) * 2 * width))
      .and_then(|bytes| {
        bytes.checked_add(
          leaf_capacity * (std::mem::size_of::<SemanticSourceCatalogPairRowV1>() + std::mem::size_of::<SemanticSourceLeafEntryV1<'_>>()),
        )
      })
      .and_then(|bytes| bytes.checked_add(levels * std::mem::size_of::<Option<Subtree>>()))
      .and_then(|bytes| bytes.checked_add(2 * NODE_BODY_CAP + (64 << 10)))
      .filter(|bytes| *bytes <= request.maximum_workspace_bytes)
      .ok_or_else(|| resource("source catalog workspace exceeds the caller's limit"))? as u64;
    let reservation =
      memory.reserve(MemoryOwner::Task, workspace, AdmissionClass::Workload).map_err(|source| resource(source.to_string()))?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(leaf_capacity).map_err(|source| resource(source.to_string()))?;
    let mut forest = Vec::new();
    forest.try_reserve_exact(levels).map_err(|source| resource(source.to_string()))?;
    forest.resize_with(levels, || None);
    let builder = Self {
      request,
      emit,
      is_cancelled,
      memory: reservation,
      workspace,
      rows,
      leaf_bytes: 32,
      previous: None,
      forest,
      path_count: 0,
      node_count: 0,
      output_bytes: 0,
    };
    builder.check()?;
    Ok(builder)
  }

  fn check(&self) -> Result<()> {
    cancelled(self.is_cancelled)?;
    self.memory.check_admission().map_err(|source| resource(source.to_string()))
  }

  fn row(&mut self, row: SemanticSourceCatalogPairRowV1) -> Result<()> {
    self.check()?;
    if self.path_count == self.request.expected_path_count {
      return Err(invalid("source stream exceeds its expected path count"));
    }
    if row.path.len() > self.request.maximum_path_bytes || row.path.capacity() > self.request.maximum_path_bytes {
      return Err(resource("source path exceeds its admitted owned capacity"));
    }
    validate_canonical_absolute_path(&row.path).map_err(format_error)?;
    if self.previous.as_ref().is_some_and(|previous| previous.as_bytes() >= row.path.as_bytes()) {
      return Err(invalid("source paths must be strictly ordered and unique"));
    }
    let width = self.request.hash_algorithm.hash_length();
    for identity in [&row.base_file_record_id, &row.requested_file_record_id].into_iter().flatten() {
      if identity.len() != width || identity.iter().all(|byte| *byte == 0) {
        return Err(invalid("present source identity must be nonzero and selected-hash width"));
      }
      if identity.capacity() > width {
        return Err(resource("source identity exceeds its admitted owned capacity"));
      }
    }
    let encoded_bytes = 4 + row.path.len() + width;
    if self.rows.len() == 256 || self.leaf_bytes + encoded_bytes > NODE_BODY_CAP {
      self.flush_leaf()?;
    }
    let mut previous = String::new();
    previous.try_reserve_exact(row.path.len()).map_err(|source| resource(source.to_string()))?;
    previous.push_str(&row.path);
    self.previous = Some(previous);
    self.leaf_bytes += encoded_bytes;
    self.rows.push(row);
    self.path_count += 1;
    self.check()
  }

  fn emit_pair(&mut self, base: &[u8], requested: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    self.check()?;
    let count = self
      .node_count
      .checked_add(1)
      .filter(|count| *count <= self.request.maximum_node_pairs)
      .ok_or_else(|| resource("source catalog exceeds its node-pair work limit"))?;
    let bytes = self
      .output_bytes
      .checked_add(base.len() as u64)
      .and_then(|bytes| bytes.checked_add(requested.len() as u64))
      .filter(|bytes| *bytes <= self.request.maximum_output_bytes)
      .ok_or_else(|| resource("source catalog exceeds its encoded-output byte limit"))?;
    let base_id = decode_system_control(base, self.request.hash_algorithm).map_err(format_error)?.identity;
    let requested_id = decode_system_control(requested, self.request.hash_algorithm).map_err(format_error)?.identity;
    self.check()?;
    (self.emit)(base, requested)?;
    self.check()?;
    self.node_count = count;
    self.output_bytes = bytes;
    Ok((base_id, requested_id))
  }

  fn flush_leaf(&mut self) -> Result<()> {
    self.check()?;
    if self.rows.is_empty() {
      return Ok(());
    }
    let mut projected = Vec::new();
    projected.try_reserve_exact(self.rows.len()).map_err(|source| resource(source.to_string()))?;
    for row in &self.rows {
      projected.push(SemanticSourceLeafEntryV1 { path: &row.path, file_record_id: row.base_file_record_id.as_deref() });
    }
    let base = encode_semantic_source_leaf_v1(&self.request.database_id, &projected, self.request.hash_algorithm).map_err(format_error)?;
    for (projected, row) in projected.iter_mut().zip(&self.rows) {
      projected.file_record_id = row.requested_file_record_id.as_deref();
    }
    let requested =
      encode_semantic_source_leaf_v1(&self.request.database_id, &projected, self.request.hash_algorithm).map_err(format_error)?;
    drop(projected);
    let (base_id, requested_id) = self.emit_pair(&base, &requested)?;
    drop(base);
    drop(requested);
    let subtree = Subtree {
      minimum: std::mem::take(&mut self.rows[0].path),
      base: base_id,
      requested: requested_id,
      paths: self.rows.len() as u64,
      nodes: 1,
    };
    self.rows.clear();
    self.leaf_bytes = 32;
    self.carry(subtree)
  }

  fn carry(&mut self, mut subtree: Subtree) -> Result<()> {
    for level in 0..self.forest.len() {
      self.check()?;
      match self.forest[level].take() {
        None => {
          self.forest[level] = Some(subtree);
          return Ok(());
        }
        Some(left) => subtree = self.merge(left, subtree)?,
      }
    }
    Err(invalid("source catalog exceeded its count-bounded carry forest"))
  }

  fn merge(&mut self, left: Subtree, right: Subtree) -> Result<Subtree> {
    self.check()?;
    if left.minimum.as_bytes() >= right.minimum.as_bytes() {
      return Err(invalid("source subtree minima are not ordered"));
    }
    let paths = left.paths.checked_add(right.paths).ok_or_else(|| invalid("source subtree path count overflow"))?;
    let nodes = left
      .nodes
      .checked_add(right.nodes)
      .and_then(|nodes| nodes.checked_add(1))
      .ok_or_else(|| invalid("source subtree node count overflow"))?;
    let children = [
      SemanticSourceChildV1 { separator: None, node_id: &left.base },
      SemanticSourceChildV1 { separator: Some(&right.minimum), node_id: &right.base },
    ];
    let base =
      encode_semantic_source_internal_v1(&self.request.database_id, &children, self.request.hash_algorithm).map_err(format_error)?;
    let children = [
      SemanticSourceChildV1 { separator: None, node_id: &left.requested },
      SemanticSourceChildV1 { separator: Some(&right.minimum), node_id: &right.requested },
    ];
    let requested =
      encode_semantic_source_internal_v1(&self.request.database_id, &children, self.request.hash_algorithm).map_err(format_error)?;
    let (base, requested) = self.emit_pair(&base, &requested)?;
    Ok(Subtree { minimum: left.minimum, base, requested, paths, nodes })
  }

  fn finish(mut self) -> Result<SemanticSourceCatalogPairV1> {
    self.flush_leaf()?;
    let mut root = None;
    // Low levels are the newest/rightmost blocks. Fold towards the oldest.
    for level in 0..self.forest.len() {
      self.check()?;
      if let Some(left) = self.forest[level].take() {
        root = Some(match root {
          Some(right) => self.merge(left, right)?,
          None => left,
        });
      }
    }
    let root = root.ok_or_else(|| invalid("nonempty source catalog produced no root"))?;
    if root.paths != self.path_count || root.nodes != self.node_count {
      return Err(invalid("source catalog final counts disagree with emitted nodes"));
    }
    drop(self.rows);
    drop(self.forest);
    drop(self.previous);
    drop(root.minimum);
    let retained = (root.base.capacity() + root.requested.capacity() + std::mem::size_of::<SemanticSourceCatalogPairV1>()) as u64;
    self.memory.shrink(self.workspace - retained).map_err(|source| resource(source.to_string()))?;
    cancelled(self.is_cancelled)?;
    self.memory.check_admission().map_err(|source| resource(source.to_string()))?;
    Ok(SemanticSourceCatalogPairV1 {
      base_root: root.base,
      requested_root: root.requested,
      path_count: self.path_count,
      node_count: self.node_count,
      _memory: self.memory,
    })
  }
}

fn cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  if is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  Ok(())
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: ERROR_PATH, message: message.into() }
}

fn invalid(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: ERROR_PATH, message: message.into() }
}

fn format_error(source: FormatError) -> SemanticCompilationErrorV1 {
  if source.is_allocation_failure() {
    return resource(source.to_string());
  }
  invalid(source.to_string())
}
