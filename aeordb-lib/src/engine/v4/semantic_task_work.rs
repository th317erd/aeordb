//! Fenced native compiler work and guarded checkpoint selection.
#[path = "semantic_task_advance.rs"]
mod compiler_advance;
#[path = "semantic_task_compiler_publication.rs"]
mod compiler_publication;
use compiler_publication::TaskCompilerPublicationContextV1;
pub use compiler_advance::{NativeSemanticTaskCompilerAdvanceReceiptV1, NativeSemanticTaskCompilerAdvanceRequestV1};
use super::*;
use crate::engine::v4::semantic_mutation_control::{SemanticMutationPhaseV1, SemanticMutationTaskStateV1, encode_semantic_mutation_task};
use source_capture_staging::validate_initial_task_owner;
use crate::engine::v4::hash::try_digest_parts;
use crate::engine::v4::semantic_catalog_compiler::SemanticCatalogContinuationV1;
use crate::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;
use crate::engine::v4::semantic_mutation_control::{SemanticMutationCursorV1, encode_semantic_mutation_checkpoint};
use crate::engine::v4::semantic_source_capture::{
  SemanticSourceCaptureV1, decode_semantic_source_capture_binding_v1, encode_semantic_source_capture_v1,
};

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskWorkRequestV1 {
  pub holder_boot_id: [u8; 16],
  pub acquired_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub inventory_bounds: NativeSemanticMutationInventoryBoundsV1,
  pub graph_bounds: NativeSemanticTaskGraphBoundsV1,
  pub maximum_workspace_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskCompilerStartRequestV1 {
  pub compiler_bounds: NativeSemanticCompilerProgressBoundsV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub maximum_workspace_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeSemanticTaskWorkErrorV1 {
  #[error(transparent)]
  Observation(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Graph(#[from] SemanticTaskGraphErrorV1),
  #[error(transparent)]
  Publication(#[from] MutableSystemControlPublicationErrorV1),
  #[error(transparent)]
  SourceUnion(#[from] NativeSemanticSourceUnionErrorV1),
  #[error(transparent)]
  Checkpoint(#[from] NativeSemanticSourceControlPublicationErrorV1),
  #[error(transparent)]
  Candidate(#[from] ImmutableEntityBatchPublicationErrorV1),
}

impl NativeSemanticTaskWorkErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Observation(source) => source.code(),
      Self::Graph(source) => source.code(),
      Self::Publication(source) => source.code(),
      Self::SourceUnion(_) => "semantic_task_work_source_union",
      Self::Checkpoint(source) => source.code(),
      Self::Candidate(source) => source.code(),
    }
  }

  pub fn committed_receipt(&self) -> Option<&MutableSystemControlPublicationReceiptV1> {
    match self {
      Self::Publication(source) => source.committed_receipt(),
      Self::Observation(_) | Self::Graph(_) | Self::SourceUnion(_) | Self::Checkpoint(_) | Self::Candidate(_) => None,
    }
  }

  /// Immutable staging commitment is never a committed task selection.
  pub fn committed_checkpoint_receipt(&self) -> Option<&ImmutableSystemControlBatchPublicationReceiptV1> {
    match self {
      Self::Checkpoint(source) => source.committed_receipt(),
      _ => None,
    }
  }

  /// A staged candidate is not a selected task or an admitted namespace root.
  pub fn committed_candidate_receipt(&self) -> Option<&ImmutableEntityBatchPublicationReceiptV1> {
    match self {
      Self::Candidate(source) => source.committed_receipt(),
      _ => None,
    }
  }
}

pub struct NativeSemanticTaskWorkV1<'a> {
  _protection: &'a NativeStagingProtectionV1<'a>,
  _observed: &'a SemanticMutationObservationV1,
  _encoded_task: Vec<u8>,
  _memory: MemoryReservation,
  receipt: MutableSystemControlPublicationReceiptV1,
  reserved_checkpoint_sequence: u64,
  memory: MemoryCoordinator,
  cancellation: CancellationToken,
  request: NativeSemanticTaskWorkRequestV1,
}

impl fmt::Debug for NativeSemanticTaskWorkV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("NativeSemanticTaskWorkV1")
      .field("control_sequence", &self.receipt.control_sequence)
      .field("reserved_checkpoint_sequence", &self.reserved_checkpoint_sequence)
      .finish_non_exhaustive()
  }
}

impl NativeSemanticTaskWorkV1<'_> {
  pub const fn receipt(&self) -> &MutableSystemControlPublicationReceiptV1 {
    &self.receipt
  }

  pub const fn reserved_checkpoint_sequence(&self) -> u64 {
    self.reserved_checkpoint_sequence
  }

  pub fn start_compilation(
    self,
    request: NativeSemanticTaskCompilerStartRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MutableSystemControlPublicationReceiptV1, NativeSemanticTaskWorkErrorV1> {
    self.start_compilation_observed(request, retirement_owner, || {}, || {}, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  pub(in crate::engine::v4::first_authority) fn start_compilation_observed(
    self,
    request: NativeSemanticTaskCompilerStartRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    before_pair: impl FnOnce(),
    before_selection: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<MutableSystemControlPublicationReceiptV1, NativeSemanticTaskWorkErrorV1> {
    self.start_compilation_with_observers(
      request,
      retirement_owner,
      before_pair,
      before_selection,
      (&mut NoopFirstAuthorityDependencyObserverV1, observer),
    )
  }

  pub(in crate::engine::v4::first_authority) fn start_compilation_with_observers(
    self,
    request: NativeSemanticTaskCompilerStartRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    before_pair: impl FnOnce(),
    before_selection: impl FnOnce(),
    observers: (&mut dyn FirstAuthorityDependencyObserverV1, &mut dyn FirstAuthorityDependencyObserverV1),
  ) -> Result<MutableSystemControlPublicationReceiptV1, NativeSemanticTaskWorkErrorV1> {
    let (pair_observer, observer) = observers;
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let header = &self._observed.header.selected.header;
    let algorithm = header.hash_algorithm;
    let task = decode_semantic_mutation_task(&self._encoded_task, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    if !matches!(task.state, SemanticMutationTaskStateV1::Queued | SemanticMutationTaskStateV1::Capturing)
      || self
        ._observed
        .checkpoint()
        .map_err(SemanticMutationObservationErrorV1::from)?
        .is_none_or(|checkpoint| checkpoint.phase != SemanticMutationPhaseV1::Captured)
    {
      return Err(invalid("semantic_task_work_phase", "compiler start requires Captured work, not an existing continuation").into());
    }
    if request.publication_timestamp_ms == 0
      || request.publication_timestamp_ms > i64::MAX as u64
      || request.publication_timestamp_ms < self.request.publication_timestamp_ms
      || request.monotonic_now_ms == 0
    {
      return Err(invalid("semantic_task_work_time", "compiler publication requires ordered, bounded clocks").into());
    }
    if retirement_owner.hash_algorithm() != algorithm || retirement_owner.database_id() != header.database_id {
      return Err(invalid("semantic_task_work_retirement_owner", "task and retirement owner belong to different databases").into());
    }
    let memory = self.reserve_compiler_publication_workspace(request.maximum_workspace_bytes)?;
    let publisher = self._protection.publisher();
    let mut task_id = [0; 16];
    task_id.copy_from_slice(task.task_id);
    // Refuse stale work before flushing retirement or staging compiler objects.
    {
      let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
        publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
      self.validate_work_protection(&authority)?;
      self.validate_selected_work(publisher, &publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?)?;
    }
    retirement_owner
      .flush(&mut SharedFirstAuthorityRetirementSinkV1 { publisher })
      .map_err(MutableSystemControlPublicationErrorV1::from)?;
    let prior_hard_publication_sequence = retirement_owner.status().last_hard_publication_sequence;
    let capture = self._protection.capture_semantic_mutation_inventory(self.request.inventory_bounds, &self.memory, &self.cancellation)?;
    validate_initial_task_owner(header, &capture.header.selected.header)?;
    capture
      .visit_captured_semantic_checkpoint_metadata_entries(&task_id, task.checkpoint_sequence, self.request.graph_bounds, |_| Ok(()))?;
    let old_checkpoint =
      self._observed.checkpoint.as_ref().ok_or_else(|| invalid("semantic_task_work_checkpoint", "work lost its original checkpoint"))?;
    let (inputs, companion_bytes) = capture.prepare_captured_semantic_compiler_inputs(
      &task_id,
      task.checkpoint_sequence,
      &old_checkpoint.bytes,
      request.compiler_bounds,
    )?;
    {
      let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
        publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
      self.validate_work_protection(&authority)?;
      let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
      if observation.region != capture.header.region {
        return Err(invalid("semantic_task_work_frontier", "authority changed during compiler input validation").into());
      }
      self.validate_selected_work(publisher, &observation)?;
      memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    }
    drop(capture);
    let mut store =
      NativeSemanticCatalogStagingStoreV1::new(self._protection, header.database_id, request.publication_timestamp_ms, &self.cancellation)
        .map_err(NativeSemanticSourceUnionErrorV1::from)?;
    let cancelled = || self.cancellation.is_cancelled();
    let continuation = match inputs.mode {
      SemanticCompilerConstructionModeV1::Fresh => {
        SemanticCatalogContinuationV1::start(inputs.request, &inputs.registry, &mut store, &self.memory, &cancelled)
      }
      SemanticCompilerConstructionModeV1::Incremental => {
        let base = inputs
          .base_admission
          .as_ref()
          .ok_or_else(|| invalid("semantic_task_work_base", "incremental construction requires admitted Complete input"))?;
        SemanticCatalogContinuationV1::from_complete(inputs.request, base, &inputs.registry, &store, &self.memory, &cancelled)
      }
    }
    .map_err(NativeSemanticSourceUnionErrorV1::from)?;
    let (old_companion, old_checkpoint) = decode_semantic_source_capture_binding_v1(&companion_bytes, &old_checkpoint.bytes, algorithm)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let catalog = continuation.catalog();
    let pruning = continuation.pruning_candidates();
    let checkpoint = SemanticMutationCheckpointV1 {
      checkpoint_sequence: self.reserved_checkpoint_sequence,
      phase: continuation.phase(),
      cursor: SemanticMutationCursorV1::None,
      configuration_count: continuation.configuration_count(),
      record_count: catalog.record_count,
      node_count: catalog.node_count,
      dependency_count: continuation.dependency_count(),
      pruning_record_count: pruning.record_count,
      pruning_node_count: pruning.node_count,
      catalog_root: catalog.root_object_id,
      pruning_catalog_root: pruning.root_object_id,
      ..old_checkpoint
    };
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let prepared = self.prepare_compiler_checkpoint(&checkpoint, &old_companion, request.publication_timestamp_ms)?;
    drop(continuation);
    drop(inputs);
    self.select_compiler_checkpoint(
      prepared,
      TaskCompilerPublicationContextV1 { request, memory: &memory, prior_hard_publication_sequence },
      retirement_owner,
      (before_pair, before_selection, pair_observer, observer),
    )
  }

  fn validate_work_protection(&self, authority: &FirstAuthorityRootStateV1) -> Result<(), SemanticMutationObservationErrorV1> {
    if authority.staging_accounting_failed || authority.active_staging_protections == 0 {
      return Err(invalid("semantic_task_work_protection", "work lost healthy staging protection"));
    }
    Ok(())
  }

  // Call only while the publisher's existing root guard is held.
  pub(in crate::engine::v4::first_authority) fn validate_selected_work(
    &self,
    publisher: &V4FirstAuthorityPublisher,
    observation: &DatabaseHeaderObservationV4,
  ) -> Result<LoadedMutableSystemControlV1, SemanticMutationObservationErrorV1> {
    if !std::ptr::eq(self._protection.publisher(), publisher) {
      return Err(invalid("semantic_task_work_publisher", "work and the held publication guard belong to different publishers"));
    }
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    self._observed._memory.check_admission()?;
    validate_initial_task_owner(&self._observed.header.selected.header, &observation.selected.header)?;
    if observation.selected.redundancy_degraded {
      return Err(invalid("semantic_task_work_frontier", "work requires healthy selected authority"));
    }
    let kv = publisher.lock_kv()?;
    validate_kv_header_alignment(&kv, &observation.selected.header)?;
    let generation = load_mutable_system_control_pair(
      &publisher.file,
      &kv,
      &observation.selected.header,
      SystemControlKindV1::SemanticMutationGeneration,
      &[],
    )?
    .selected;
    if generation.as_ref() != self._observed.generation_selection() {
      return Err(invalid("semantic_task_work_generation", "semantic generation changed after acquisition"));
    }
    let task = decode_semantic_mutation_task(&self._encoded_task, observation.selected.header.hash_algorithm)?;
    let current = load_mutable_system_control_pair(
      &publisher.file,
      &kv,
      &observation.selected.header,
      SystemControlKindV1::SemanticMutationTask,
      task.task_id,
    )?
    .selected
    .ok_or_else(|| invalid("semantic_task_work_conflict", "reserved task is no longer selected"))?;
    if current.bytes != self._encoded_task
      || current.selected_slot != self.receipt.selected_slot
      || current.control_sequence != self.receipt.control_sequence
      || current.control_digest != self.receipt.control_digest
    {
      return Err(invalid("semantic_task_work_conflict", "selected task no longer belongs to this work reservation"));
    }
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    self._observed._memory.check_admission()?;
    Ok(current)
  }
}

impl NativeStagingProtectionV1<'_> {
  pub fn begin_semantic_task_work<'a>(
    &'a self,
    observed: &'a SemanticMutationObservationV1,
    request: NativeSemanticTaskWorkRequestV1,
    memory: &MemoryCoordinator,
    cancellation: &CancellationToken,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<NativeSemanticTaskWorkV1<'a>, NativeSemanticTaskWorkErrorV1> {
    self.begin_semantic_task_work_observed(
      observed,
      request,
      memory,
      cancellation,
      retirement_owner,
      (|| {}, &mut NoopFirstAuthorityDependencyObserverV1),
    )
  }

  pub(in crate::engine::v4::first_authority) fn begin_semantic_task_work_observed<'a>(
    &'a self,
    observed: &'a SemanticMutationObservationV1,
    request: NativeSemanticTaskWorkRequestV1,
    memory: &MemoryCoordinator,
    cancellation: &CancellationToken,
    retirement_owner: &mut RetirementJournalOwnerV1,
    hooks: (impl FnOnce(), &mut dyn FirstAuthorityDependencyObserverV1),
  ) -> Result<NativeSemanticTaskWorkV1<'a>, NativeSemanticTaskWorkErrorV1> {
    let (before_lock, observer) = hooks;
    check_cancelled(cancellation)?;
    observed._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let header = &observed.header.selected.header;
    let selected = observed.task_selection().ok_or_else(|| invalid("semantic_task_work_absent", "work requires a selected task"))?;
    let task = observed
      .task()
      .map_err(SemanticMutationObservationErrorV1::from)?
      .ok_or_else(|| invalid("semantic_task_work_absent", "work requires a selected task"))?;
    let checkpoint = observed
      .checkpoint()
      .map_err(SemanticMutationObservationErrorV1::from)?
      .ok_or_else(|| invalid("semantic_task_work_checkpoint", "work requires a held checkpoint"))?;
    let phase_matches = match task.state {
      SemanticMutationTaskStateV1::Queued | SemanticMutationTaskStateV1::Capturing => checkpoint.phase == SemanticMutationPhaseV1::Captured,
      SemanticMutationTaskStateV1::Compiling => {
        matches!(checkpoint.phase, SemanticMutationPhaseV1::Compiling | SemanticMutationPhaseV1::Pruning)
      }
      _ => false,
    };
    if task.pins_released || !phase_matches {
      return Err(invalid("semantic_task_work_phase", "work requires a held Captured or unfinished compiler checkpoint").into());
    }
    if task.database_id != header.database_id
      || task.physical_instance_id != header.physical_instance_id
      || task.writer_fence_epoch > header.writer_fence_epoch
    {
      return Err(invalid("semantic_task_work_owner", "selected task cannot cross physical ownership or a future writer fence").into());
    }
    if observed.generation() != Some(checkpoint.semantic_generation) {
      return Err(invalid("semantic_task_work_generation", "selected checkpoint requires its exact captured generation").into());
    }
    if request.holder_boot_id.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_task_work_holder", "work acquisition requires a nonzero boot identity").into());
    }
    if request.acquired_at_ms < 0
      || request.acquired_at_ms < task.updated_at_ms
      || request.publication_timestamp_ms == 0
      || request.publication_timestamp_ms > i64::MAX as u64
      || request.publication_timestamp_ms < request.acquired_at_ms as u64
      || request.monotonic_now_ms == 0
    {
      return Err(invalid("semantic_task_work_time", "work acquisition requires ordered, nonzero bounded clocks").into());
    }
    let reserved_checkpoint_sequence = task
      .fencing_token
      .max(checkpoint.checkpoint_sequence)
      .checked_add(1)
      .ok_or_else(|| invalid("semantic_task_work_fence_exhausted", "no fresh task fence remains for an immutable checkpoint"))?;
    task
      .control_sequence
      .checked_add(2)
      .ok_or_else(|| invalid("semantic_task_work_control_exhausted", "work requires room for reservation and checkpoint selection"))?;
    if retirement_owner.hash_algorithm() != header.hash_algorithm || retirement_owner.database_id() != header.database_id {
      return Err(invalid("semantic_task_work_retirement_owner", "task and retirement owner belong to different databases").into());
    }
    let workspace = (36usize + 112 + header.hash_algorithm.hash_length())
      .checked_mul(16)
      .and_then(|bytes| bytes.checked_add(8 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP + (128 << 10)))
      .filter(|bytes| *bytes <= request.maximum_workspace_bytes)
      .ok_or(SemanticMutationObservationErrorV1::Resource {
        code: "semantic_task_work_workspace",
        message: "work exceeds its admitted publication workspace",
      })?;
    let reservation =
      memory.reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Maintenance).map_err(SemanticMutationObservationErrorV1::from)?;
    let encoded_task = encode_semantic_mutation_task(
      &SemanticMutationTaskV1 {
        control_sequence: task.control_sequence + 1,
        holder_boot_id: &request.holder_boot_id,
        fencing_token: reserved_checkpoint_sequence,
        writer_fence_epoch: header.writer_fence_epoch,
        updated_at_ms: request.acquired_at_ms,
        ..task
      },
      header.hash_algorithm,
    )
    .map_err(SemanticMutationObservationErrorV1::from)?;
    let mut task_id = [0; 16];
    task_id.copy_from_slice(task.task_id);
    let publisher = self.publisher();
    retirement_owner
      .flush(&mut SharedFirstAuthorityRetirementSinkV1 { publisher })
      .map_err(MutableSystemControlPublicationErrorV1::from)?;
    let prior_hard_publication_sequence = retirement_owner.status().last_hard_publication_sequence;
    let fresh = self.capture_semantic_mutation_inventory(request.inventory_bounds, memory, cancellation)?;
    validate_initial_task_owner(header, &fresh.header.selected.header)?;
    fresh.visit_captured_semantic_checkpoint_metadata_entries(&task_id, task.checkpoint_sequence, request.graph_bounds, |_| Ok(()))?;
    before_lock();
    let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
      publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
    check_cancelled(cancellation)?;
    reservation.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    observed._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if authority.staging_accounting_failed || authority.active_staging_protections == 0 {
      return Err(invalid("semantic_task_work_protection", "work acquisition lost healthy staging protection").into());
    }
    let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    if observation.selected.redundancy_degraded || observation.region != fresh.header.region {
      return Err(invalid("semantic_task_work_frontier", "authority changed after work graph validation; capture again").into());
    }
    validate_initial_task_owner(header, &observation.selected.header)?;
    let (retry, expected) = {
      let kv = publisher.lock_kv().map_err(SemanticMutationObservationErrorV1::from)?;
      validate_kv_header_alignment(&kv, &observation.selected.header).map_err(SemanticMutationObservationErrorV1::from)?;
      let generation = load_mutable_system_control_pair(
        &publisher.file,
        &kv,
        &observation.selected.header,
        SystemControlKindV1::SemanticMutationGeneration,
        &[],
      )
      .map_err(SemanticMutationObservationErrorV1::from)?
      .selected;
      if generation.as_ref() != observed.generation_selection() {
        return Err(invalid("semantic_task_work_generation", "semantic generation changed after observation").into());
      }
      let current = load_mutable_system_control_pair(
        &publisher.file,
        &kv,
        &observation.selected.header,
        SystemControlKindV1::SemanticMutationTask,
        &task_id,
      )
      .map_err(SemanticMutationObservationErrorV1::from)?
      .selected;
      match current {
        Some(current) if current.bytes == encoded_task => (Some(current), None),
        Some(current) if &current == selected => {
          let expectation = MutableSystemControlExpectationV1 {
            selected_slot: current.selected_slot,
            control_sequence: current.control_sequence,
            control_digest: current.control_digest,
          };
          (None, Some(expectation))
        }
        _ => return Err(invalid("semantic_task_work_conflict", "selected task changed after observation").into()),
      }
    };
    check_cancelled(cancellation)?;
    reservation.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    drop(fresh);
    let receipt = if let Some(current) = retry {
      // Retain the guard through retry projection; never enter the KV flush.
      let receipt = idempotent_mutable_system_control_receipt(&current, observation);
      drop(authority);
      receipt
    } else {
      publisher.publish_admitted_mutable_system_control_with_observer(
        AdmittedMutableSystemControlPublicationV1 {
          publisher,
          authority,
          observation,
          request: MutableSystemControlPublicationRequestV1 {
            database_id: &header.database_id,
            kind: SystemControlKindV1::SemanticMutationTask,
            identity: &task_id,
            expected,
            guards: &[],
            encoded_control: &encoded_task,
            publication_timestamp_ms: request.publication_timestamp_ms,
            monotonic_now_ms: request.monotonic_now_ms,
          },
          timestamp: request.publication_timestamp_ms as i64,
          prior_hard_publication_sequence,
        },
        retirement_owner,
        observer,
      )?
    };
    // Do not recheck cancellation after a committed task publication.
    Ok(NativeSemanticTaskWorkV1 {
      _protection: self,
      _observed: observed,
      _encoded_task: encoded_task,
      _memory: reservation,
      receipt,
      reserved_checkpoint_sequence,
      memory: memory.clone(),
      cancellation: cancellation.clone(),
      request,
    })
  }
}
