//! Provisional task/checkpoint physical reads, never GC or resume authority.
use super::*;
use super::source_catalog::{CatalogPhysicalEntryObserverV1, CatalogReadOperationV1};
use crate::engine::v4::read_view_native::NativeSelectedNamespaceReadErrorV1;
use crate::engine::v4::semantic_catalog::SemanticCatalogReadErrorV1;
use crate::engine::v4::semantic_mutation_control::SemanticMutationCheckpointV1;
use std::cell::RefCell;
#[path = "semantic_task_catalog_graph.rs"]
mod catalog;
pub(super) use catalog::load_captured_semantic_object;
#[path = "semantic_task_namespace_graph.rs"]
mod namespace;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskGraphBoundsV1 {
  /// All physical reads and namespace/catalog element work, including repeats.
  pub maximum_work: u64,
  pub maximum_read_bytes: u64,
  pub maximum_namespace_workspace_bytes: u64,
  pub maximum_depth: usize,
  pub maximum_path_bytes: usize,
  pub maximum_decoded_chunk_bytes: usize,
  /// Additional logical limits for the paired source branch.
  pub sources: NativeSemanticSourceCatalogBoundsV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticTaskGraphSummaryV1 {
  pub disposition: SemanticMutationObservationDispositionV1,
  pub checkpoint_sequence: Option<u64>,
  pub physical_reads: u64,
  pub read_bytes: u64,
  /// Ordinary chunk locators visited without reading their entities. These are
  /// references, not verified payloads; both counters are zero for deep reads.
  pub opaque_chunk_references: u64,
  pub opaque_chunk_bytes: u64,
  pub work: u64,
  pub namespace_directories: u64,
  pub namespace_files: u64,
  pub namespace_symlinks: u64,
  pub namespace_chunks: u64,
  pub source_paths: u64,
}

/// Structural checkpoint observation, never selected task disposition or a
/// publication, resume, retention-release or global-mark permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticCheckpointGraphSummaryV1 {
  pub checkpoint_sequence: u64,
  pub physical_reads: u64,
  pub read_bytes: u64,
  /// Ordinary chunk locators, not payload reads or content integrity proof.
  pub opaque_chunk_references: u64,
  pub opaque_chunk_bytes: u64,
  pub work: u64,
  pub namespace_directories: u64,
  pub namespace_files: u64,
  pub namespace_symlinks: u64,
  pub namespace_chunks: u64,
  pub source_paths: u64,
}

#[derive(Default)]
struct CheckpointGraphStatistics {
  opaque_chunk_references: u64,
  opaque_chunk_bytes: u64,
  namespace_directories: u64,
  namespace_files: u64,
  namespace_symlinks: u64,
  namespace_chunks: u64,
  source_paths: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticTaskGraphErrorV1 {
  #[error(transparent)]
  Source(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Namespace(#[from] NativeSelectedNamespaceReadErrorV1),
  #[error(transparent)]
  Catalog(#[from] SemanticCatalogReadErrorV1),
}

impl SemanticTaskGraphErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Source(error) => error.code(),
      Self::Namespace(error) => error.code(),
      Self::Catalog(error) => error.code(),
    }
  }
}

impl From<FirstAuthorityPublicationErrorV1> for SemanticTaskGraphErrorV1 {
  fn from(error: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Source(error.into())
  }
}
impl From<FormatError> for SemanticTaskGraphErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Source(error.into())
  }
}
impl From<EngineError> for SemanticTaskGraphErrorV1 {
  fn from(error: EngineError) -> Self {
    Self::Source(FirstAuthorityPublicationErrorV1::from(error).into())
  }
}
impl From<MemoryCoordinatorError> for SemanticTaskGraphErrorV1 {
  fn from(error: MemoryCoordinatorError) -> Self {
    Self::Source(error.into())
  }
}

type Result<T> = std::result::Result<T, SemanticTaskGraphErrorV1>;
type PhysicalVisitor<'a> = dyn FnMut(&KVEntry) -> std::result::Result<(), SemanticMutationObservationErrorV1> + 'a;

impl NativeSemanticMutationInventoryV1<'_> {
  /// Validate an immutable checkpoint graph without selecting or resuming a
  /// task. Entries are provisional until complete success, not GC authority.
  pub fn visit_captured_semantic_checkpoint_metadata_entries(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticTaskGraphBoundsV1,
    mut visitor: impl FnMut(&KVEntry) -> std::result::Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticCheckpointGraphSummaryV1> {
    check_cancelled(&self.cancellation)?;
    let operation = GraphOperation::new(self, bounds, &mut visitor, false, None)?;
    let result = operation.read_checkpoint(task_id, checkpoint_sequence);
    operation.complete_result(result)
  }

  /// Traverse metadata and retain ordinary chunk references without verifying
  /// their payloads. Source/control/catalog bodies still use their exact bounded
  /// readers. This is not content verification, cheap commit admission, a
  /// durable frontier or a complete GC run. Callbacks remain provisional.
  pub fn visit_captured_semantic_task_metadata_entries(
    &self,
    task_id: &[u8; 16],
    bounds: NativeSemanticTaskGraphBoundsV1,
    visitor: impl FnMut(&KVEntry) -> std::result::Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticTaskGraphSummaryV1> {
    self.visit_captured_task_entries(task_id, bounds, visitor, false, None)
  }

  /// Entries are provisional and may repeat. Only successful return reports
  /// the requested task's checked graph; it is not a global mark or permit.
  pub fn visit_captured_semantic_task_physical_entries(
    &self,
    task_id: &[u8; 16],
    bounds: NativeSemanticTaskGraphBoundsV1,
    visitor: impl FnMut(&KVEntry) -> std::result::Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticTaskGraphSummaryV1> {
    self.visit_captured_task_entries(task_id, bounds, visitor, true, None)
  }

  pub(super) fn visit_captured_task_entries(
    &self,
    task_id: &[u8; 16],
    bounds: NativeSemanticTaskGraphBoundsV1,
    mut visitor: impl FnMut(&KVEntry) -> std::result::Result<(), SemanticMutationObservationErrorV1>,
    inspect_ordinary_payloads: bool,
    admission: Option<&dyn TaskRetentionAdmissionV1>,
  ) -> Result<SemanticTaskGraphSummaryV1> {
    check_cancelled(&self.cancellation)?;
    if task_id.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_task_graph_identity", "task identity must be nonzero").into());
    }
    let operation = GraphOperation::new(self, bounds, &mut visitor, inspect_ordinary_payloads, admission)?;
    let result = operation.read(task_id);
    operation.complete_result(result)
  }
}

struct GraphLookup<'a, 'visitor> {
  captured: CapturedEntityLookupV1<'a>,
  admission: Option<&'a dyn TaskRetentionAdmissionV1>,
  visitor: RefCell<&'visitor mut PhysicalVisitor<'visitor>>,
  failure: RefCell<Option<SemanticMutationObservationErrorV1>>,
  work: Cell<u64>,
  maximum_work: u64,
  reads: Cell<u64>,
  memory: MemoryReservation,
}

impl GraphLookup<'_, '_> {
  fn check(&self) -> std::result::Result<(), SemanticMutationObservationErrorV1> {
    check_cancelled(self.captured.cancellation)?;
    self.memory.check_admission()?;
    Ok(())
  }

  fn step(&self, count: u64) -> std::result::Result<(), SemanticMutationObservationErrorV1> {
    self.check()?;
    let next =
      self.work.get().checked_add(count).filter(|next| *next <= self.maximum_work).ok_or(SemanticMutationObservationErrorV1::Resource {
        code: "semantic_task_graph_work_bound",
        message: "selected task graph exhausted its cumulative work limit",
      })?;
    if let Some(admission) = self.admission {
      admission.admit_work(count)?;
    }
    self.work.set(next);
    Ok(())
  }
}

impl FirstAuthorityEntityLookupV1 for GraphLookup<'_, '_> {
  fn get(&self, key: &[u8]) -> std::result::Result<Option<KVEntry>, EngineError> {
    self.captured.get(key)
  }
  fn hash_algo(&self) -> HashAlgorithm {
    self.captured.hash_algo()
  }
  fn admit_read(&self, locator: &KVEntry) -> std::result::Result<(), FirstAuthorityPublicationErrorV1> {
    let result: std::result::Result<(), SemanticMutationObservationErrorV1> = (|| {
      self.step(1)?;
      self.captured.admit_read(locator)?;
      if let Some(admission) = self.admission {
        admission.admit_read_bytes(u64::from(locator.total_length))?;
      }
      let next = self.reads.get().checked_add(1).ok_or_else(|| invalid("semantic_task_graph_counts", "physical read count overflowed"))?;
      self.reads.set(next);
      self.visitor.borrow_mut()(locator)?;
      self.check()
    })();
    if let Err(original) = result {
      if self.failure.borrow().is_none() {
        *self.failure.borrow_mut() = Some(original);
      }
      return Err(FirstAuthorityPublicationErrorV1::invalid("semantic_task_graph_read_refused", "selected task physical read refused"));
    }
    Ok(())
  }
}

impl CatalogPhysicalEntryObserverV1 for GraphLookup<'_, '_> {
  fn observe(&self, locator: &KVEntry) -> std::result::Result<(), FirstAuthorityPublicationErrorV1> {
    self.admit_read(locator)
  }
}

struct GraphOperation<'a, 'visitor> {
  capture: &'a NativeSemanticMutationInventoryV1<'a>,
  lookup: GraphLookup<'a, 'visitor>,
  bounds: NativeSemanticTaskGraphBoundsV1,
  namespace_bytes: Cell<u64>,
  inspect_ordinary_payloads: bool,
}

impl<'a, 'visitor> GraphOperation<'a, 'visitor> {
  fn new(
    capture: &'a NativeSemanticMutationInventoryV1<'a>,
    bounds: NativeSemanticTaskGraphBoundsV1,
    visitor: &'visitor mut PhysicalVisitor<'visitor>,
    inspect_ordinary_payloads: bool,
    admission: Option<&'a dyn TaskRetentionAdmissionV1>,
  ) -> Result<Self> {
    capture._memory.check_admission()?;
    if bounds.maximum_work == 0
      || bounds.maximum_read_bytes == 0
      || bounds.maximum_namespace_workspace_bytes == 0
      || !(1..=256).contains(&bounds.maximum_depth)
      || !(1..=65_535).contains(&bounds.maximum_path_bytes)
      || !(1..=64 << 20).contains(&bounds.maximum_decoded_chunk_bytes)
    {
      return Err(invalid("semantic_task_graph_bounds", "selected task graph requires positive bounded work, depth and workspace").into());
    }
    // Common control/path scratch stays live for the entire operation. The
    // larger namespace/catalog decode reservation starts only AFTER the paired
    // source traversal drops its independently accounted decode workspace.
    let scratch = (2u64 << 20)
      .checked_add((bounds.maximum_depth as u64) * (3 * bounds.maximum_path_bytes as u64 + 4096))
      .ok_or_else(|| invalid("semantic_task_graph_memory_bound", "graph scratch estimate overflowed"))?;
    let memory = capture.memory.reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)?;
    let maximum_read_bytes = bounds.maximum_read_bytes.min(capture.bounds.maximum_read_bytes);
    let lookup = GraphLookup {
      admission,
      captured: CapturedEntityLookupV1 {
        snapshot: &capture.snapshot,
        header: &capture.header.selected.header,
        bounds: NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes, ..capture.bounds },
        cancellation: &capture.cancellation,
        remaining_read_bytes: Cell::new(maximum_read_bytes),
      },
      visitor: RefCell::new(visitor),
      failure: RefCell::new(None),
      work: Cell::new(0),
      maximum_work: bounds.maximum_work.min(capture.bounds.maximum_work),
      reads: Cell::new(0),
      memory,
    };
    Ok(Self { capture, lookup, bounds, namespace_bytes: Cell::new(0), inspect_ordinary_payloads })
  }

  fn check(&self) -> Result<()> {
    self.lookup.check()?;
    self.capture._memory.check_admission()?;
    Ok(())
  }
  fn header(&self) -> &DatabaseHeaderV4 {
    &self.capture.header.selected.header
  }
  fn algorithm(&self) -> HashAlgorithm {
    self.header().hash_algorithm
  }
  fn file(&self) -> &File {
    &self.capture._protection.publisher().file
  }

  fn complete_result<T>(&self, result: Result<T>) -> Result<T> {
    match self.lookup.failure.borrow_mut().take() {
      Some(original) => Err(original.into()),
      None => result,
    }
  }

  fn read_checkpoint(&self, task_id: &[u8; 16], sequence: u64) -> Result<SemanticCheckpointGraphSummaryV1> {
    self.check()?;
    let sources = CatalogReadOperationV1::new(self.capture, self.bounds.sources, Some(&self.lookup))?;
    let (companion, checkpoint_bytes) = sources.load_companion_and_checkpoint(task_id, sequence)?;
    let (manifest, checkpoint) = crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_binding_v1(
      &companion.bytes,
      &checkpoint_bytes,
      self.algorithm(),
    )?;
    let source_summary = sources.visit_pairs(&manifest, |_, _, _| Ok::<_, SemanticMutationObservationErrorV1>(true))?;
    if !source_summary.complete {
      return Err(invalid("semantic_task_graph_source_incomplete", "source branch did not complete").into());
    }
    sources.check()?;
    // Release catalog traversal workspace before reserving graph decode space.
    // The companion retains the reservation for both immutable control bodies.
    drop(sources);
    let mut summary = CheckpointGraphStatistics { source_paths: source_summary.paths, ..Default::default() };
    self.walk_checkpoint(&checkpoint, &mut summary)?;
    self.check()?;
    Ok(SemanticCheckpointGraphSummaryV1 {
      checkpoint_sequence: checkpoint.checkpoint_sequence,
      physical_reads: self.lookup.reads.get(),
      read_bytes: self.lookup.captured.bounds.maximum_read_bytes - self.lookup.captured.remaining_read_bytes.get(),
      opaque_chunk_references: summary.opaque_chunk_references,
      opaque_chunk_bytes: summary.opaque_chunk_bytes,
      work: self.lookup.work.get(),
      namespace_directories: summary.namespace_directories,
      namespace_files: summary.namespace_files,
      namespace_symlinks: summary.namespace_symlinks,
      namespace_chunks: summary.namespace_chunks,
      source_paths: summary.source_paths,
    })
  }

  fn read(&self, task_id: &[u8; 16]) -> Result<SemanticTaskGraphSummaryV1> {
    self.check()?;
    let task =
      load_mutable_system_control_pair(self.file(), &self.lookup, self.header(), SystemControlKindV1::SemanticMutationTask, task_id)?
        .selected;
    let observation = complete_semantic_mutation_observation(
      self.file(),
      &self.lookup,
      self.capture.header.clone(),
      task,
      SemanticMutationObservationRequestV1 {
        database_id: &self.header().database_id,
        task_id,
        memory: &self.capture.memory,
        cancellation: &self.capture.cancellation,
      },
      reserve_observation_memory(&self.capture.memory)?,
    )?;
    let mut summary = CheckpointGraphStatistics::default();
    let mut checkpoint_sequence = None;
    if let Some(checkpoint) = observation.checkpoint()? {
      checkpoint_sequence = Some(checkpoint.checkpoint_sequence);
      // Same capture, with an additional enclosing read admission on every
      // branch callback. The source reader retains its own logical work bound.
      let sources = self.capture.visit_captured_source_physical_entries_admitted(
        task_id,
        checkpoint.checkpoint_sequence,
        self.bounds.sources,
        self.lookup.admission,
        |entry| self.lookup.admit_read(entry).map_err(SemanticMutationObservationErrorV1::from),
      )?;
      if !sources.complete {
        return Err(invalid("semantic_task_graph_source_incomplete", "source branch did not complete").into());
      }
      summary.source_paths = sources.paths;
      self.walk_checkpoint(&checkpoint, &mut summary)?;
    }
    self.check()?;
    Ok(SemanticTaskGraphSummaryV1 {
      disposition: observation.disposition(),
      checkpoint_sequence,
      physical_reads: self.lookup.reads.get(),
      read_bytes: self.lookup.captured.bounds.maximum_read_bytes - self.lookup.captured.remaining_read_bytes.get(),
      opaque_chunk_references: summary.opaque_chunk_references,
      opaque_chunk_bytes: summary.opaque_chunk_bytes,
      work: self.lookup.work.get(),
      namespace_directories: summary.namespace_directories,
      namespace_files: summary.namespace_files,
      namespace_symlinks: summary.namespace_symlinks,
      namespace_chunks: summary.namespace_chunks,
      source_paths: summary.source_paths,
    })
  }

  fn walk_checkpoint(&self, checkpoint: &SemanticMutationCheckpointV1<'_>, summary: &mut CheckpointGraphStatistics) -> Result<()> {
    let _graph_decode_memory =
      self.capture.memory.reserve(MemoryOwner::Task, 8 * self.capture.bounds.maximum_entity_bytes as u64, AdmissionClass::Maintenance)?;
    let base = load_namespace_authority_from_lookup(
      self.file(),
      &self.lookup,
      &self.capture.header.selected,
      checkpoint.base_namespace_root,
      &self.capture.cancellation,
    )?
    .ok_or_else(|| invalid("semantic_task_graph_base_missing", "selected task admitted base is absent"))?;
    self.walk_namespace(&base.root.namespace_tree_root, summary)?;
    self.walk_state(&base.semantic_state)?;
    drop(base);
    self.walk_namespace(checkpoint.staged_directory_root, summary)?;
    if let Some(root) = checkpoint.catalog_root {
      let counts = self.walk_catalog(root, checkpoint.record_count, checkpoint.node_count)?;
      if counts.class_counts[1] != checkpoint.configuration_count
        || counts.class_counts[6].checked_add(counts.class_counts[7]) != Some(checkpoint.dependency_count)
      {
        return Err(invalid("semantic_task_graph_catalog_counts", "selected task catalog member counts disagree").into());
      }
    }
    if let Some(root) = checkpoint.pruning_catalog_root {
      self.walk_catalog(root, checkpoint.pruning_record_count, checkpoint.pruning_node_count)?;
    }
    if let Some(state_id) = checkpoint.semantic_state {
      let bytes =
        self.semantic_object(1, state_id)?.ok_or_else(|| invalid("semantic_task_graph_state_missing", "task output state is absent"))?;
      let decoded = decode_semantic_object(&bytes, self.algorithm())?;
      let state =
        decoded.semantic_state.ok_or_else(|| invalid("semantic_task_graph_state_kind", "task output has another semantic kind"))?;
      self.validate_output(checkpoint, &state)?;
      self.walk_state(&state)?;
    }
    if let Some(root_id) = checkpoint.candidate_namespace_root {
      let bytes = self.raw(root_id, kv_tag::DIRECTORY, FIRST_AUTHORITY_NAMESPACE_ROOT_ENTITY_CAP)?;
      let root =
        crate::engine::v4::namespace::decode_namespace_root_entity(&bytes, self.algorithm(), self.header().write_sequence_high_water)?;
      if root.root_hash != root_id || Some(root.semantic_state_root.as_slice()) != checkpoint.semantic_state {
        return Err(invalid("semantic_task_graph_candidate_closure", "staged candidate differs from its checkpoint output").into());
      }
      // STAGED candidate bytes are retained. Deliberately do not look for or
      // manufacture a RootAdmissionCommit for this unactivated candidate.
      // Ordinary-change rebase can give the candidate another tree. Retain
      // that additional branch instead of assuming it equals the request.
      if root.namespace_tree_root != checkpoint.staged_directory_root {
        self.walk_namespace(&root.namespace_tree_root, summary)?;
      }
    }
    Ok(())
  }

  fn raw(&self, key: &[u8], role: u8, cap: usize) -> Result<Vec<u8>> {
    self.check()?;
    let locator =
      self.lookup.get(key)?.ok_or_else(|| invalid("semantic_task_graph_entity_missing", "selected task physical dependency is absent"))?;
    if locator.type_flags != role {
      return Err(invalid("semantic_task_graph_entity_role", "selected task dependency resolves to another KV role").into());
    }
    let bytes = read_entity_bounded(
      self.file(),
      &self.lookup,
      key,
      cap.min(self.capture.bounds.maximum_entity_bytes),
      self.header().write_sequence_high_water,
    )?
    .ok_or_else(|| invalid("semantic_task_graph_entity_missing", "selected task dependency disappeared from its capture"))?;
    self.check()?;
    Ok(bytes)
  }
}

fn graph_allocation(source: std::collections::TryReserveError) -> SemanticTaskGraphErrorV1 {
  SemanticMutationObservationErrorV1::Allocation { code: "semantic_task_graph_allocation", source }.into()
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
  let mut result = Vec::new();
  result.try_reserve_exact(bytes.len()).map_err(graph_allocation)?;
  result.extend_from_slice(bytes);
  Ok(result)
}
