//! Shared derived checkpoint pair and guarded task selection for start/advance.
use super::*;

pub(super) struct PreparedTaskCompilerCheckpointV1 {
  checkpoint: Vec<u8>,
  companion: Vec<u8>,
  task: Vec<u8>,
  phase: SemanticMutationPhaseV1,
}

pub(super) struct TaskCompilerPublicationContextV1<'a> {
  pub(super) request: NativeSemanticTaskCompilerStartRequestV1,
  pub(super) memory: &'a MemoryReservation,
  pub(super) prior_hard_publication_sequence: u64,
}

impl NativeSemanticTaskWorkV1<'_> {
  pub(super) fn reserve_compiler_publication_workspace(
    &self,
    maximum_workspace_bytes: usize,
  ) -> Result<MemoryReservation, NativeSemanticTaskWorkErrorV1> {
    let algorithm = self._observed.header.selected.header.hash_algorithm;
    let body_bytes = 3 * 36 + 168 + 112 + 112 + 16 * algorithm.hash_length();
    let workspace = body_bytes
      .checked_add(4 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP)
      .and_then(|bytes| bytes.checked_mul(16))
      .and_then(|bytes| bytes.checked_add(256 << 10))
      .filter(|bytes| *bytes <= maximum_workspace_bytes)
      .ok_or(SemanticMutationObservationErrorV1::Resource {
        code: "semantic_task_work_workspace",
        message: "compiler selection exceeds its admitted publication workspace",
      })?;
    self
      .memory
      .reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)
      .map_err(Into::into)
  }
  pub(super) fn prepare_compiler_checkpoint(
    &self,
    checkpoint: &SemanticMutationCheckpointV1<'_>,
    old_companion: &SemanticSourceCaptureV1<'_>,
    publication_timestamp_ms: u64,
  ) -> Result<PreparedTaskCompilerCheckpointV1, NativeSemanticTaskWorkErrorV1> {
    let algorithm = self._observed.header.selected.header.hash_algorithm;
    let task = decode_semantic_mutation_task(&self._encoded_task, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    let state = match checkpoint.phase {
      SemanticMutationPhaseV1::Compiling | SemanticMutationPhaseV1::Pruning => SemanticMutationTaskStateV1::Compiling,
      SemanticMutationPhaseV1::Ready => SemanticMutationTaskStateV1::ReadyToActivate,
      _ => return Err(invalid("semantic_task_work_phase", "compiler output is neither unfinished nor Ready").into()),
    };
    let encoded_checkpoint =
      encode_semantic_mutation_checkpoint(checkpoint, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    let digest = try_digest_parts(algorithm, &[&encoded_checkpoint])
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_task_work_digest_allocation", source })?;
    let encoded_companion = encode_semantic_source_capture_v1(
      &SemanticSourceCaptureV1 {
        checkpoint_sequence: self.reserved_checkpoint_sequence,
        checkpoint_payload_hash: &digest,
        ..*old_companion
      },
      algorithm,
    )
    .map_err(SemanticMutationObservationErrorV1::from)?;
    decode_semantic_source_capture_binding_v1(&encoded_companion, &encoded_checkpoint, algorithm)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let encoded_task = encode_semantic_mutation_task(
      &SemanticMutationTaskV1 {
        control_sequence: task.control_sequence + 1,
        state,
        checkpoint_sequence: self.reserved_checkpoint_sequence,
        checkpoint_payload_hash: &digest,
        updated_at_ms: publication_timestamp_ms as i64,
        ..task
      },
      algorithm,
    )
    .map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(PreparedTaskCompilerCheckpointV1 {
      checkpoint: encoded_checkpoint,
      companion: encoded_companion,
      task: encoded_task,
      phase: checkpoint.phase,
    })
  }

  pub(super) fn select_compiler_checkpoint(
    &self,
    prepared: PreparedTaskCompilerCheckpointV1,
    context: TaskCompilerPublicationContextV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1,
    hooks: (impl FnOnce(), impl FnOnce(), &mut dyn FirstAuthorityDependencyObserverV1, &mut dyn FirstAuthorityDependencyObserverV1),
  ) -> Result<MutableSystemControlPublicationReceiptV1, NativeSemanticTaskWorkErrorV1> {
    let PreparedTaskCompilerCheckpointV1 { checkpoint: encoded_checkpoint, companion: encoded_companion, task: encoded_task, phase } =
      prepared;
    let TaskCompilerPublicationContextV1 { request, memory, prior_hard_publication_sequence } = context;
    let (before_pair, before_selection, pair_observer, observer) = hooks;
    let header = &self._observed.header.selected.header;
    let publisher = self._protection.publisher();
    let task =
      decode_semantic_mutation_task(&self._encoded_task, header.hash_algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    let mut task_id = [0; 16];
    task_id.copy_from_slice(task.task_id);
    let mut identity = [0; 24];
    identity[..16].copy_from_slice(&task_id);
    identity[16..].copy_from_slice(&self.reserved_checkpoint_sequence.to_le_bytes());
    let controls = [
      ImmutableSystemControlWriteV1 {
        kind: SystemControlKindV1::SemanticMutationCheckpoint,
        identity: &identity,
        encoded_control: &encoded_checkpoint,
      },
      ImmutableSystemControlWriteV1 {
        kind: SystemControlKindV1::SemanticSourceCapture,
        identity: &identity,
        encoded_control: &encoded_companion,
      },
    ];
    let capture = self._protection.capture_semantic_mutation_inventory(self.request.inventory_bounds, &self.memory, &self.cancellation)?;
    capture.stage_captured_work_controls(&controls, request.publication_timestamp_ms, memory, self, (before_pair, pair_observer))?;
    drop(capture);
    let fresh = self._protection.capture_semantic_mutation_inventory(self.request.inventory_bounds, &self.memory, &self.cancellation)?;
    fresh.visit_captured_semantic_checkpoint_metadata_entries_expected(
      &task_id,
      self.reserved_checkpoint_sequence,
      self.request.graph_bounds,
      Some((&encoded_checkpoint, &encoded_companion)),
      |_| Ok(()),
    )?;
    if phase == SemanticMutationPhaseV1::Ready {
      drop(fresh.admit_captured_semantic_compiler_output(&task_id, self.reserved_checkpoint_sequence, request.compiler_bounds)?);
    } else {
      drop(fresh.admit_captured_semantic_compiler_progress(&task_id, self.reserved_checkpoint_sequence, request.compiler_bounds)?);
    }
    before_selection();
    let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
      publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
    self.validate_work_protection(&authority)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    if observation.region != fresh.header.region {
      return Err(invalid("semantic_task_work_frontier", "authority changed after compiler graph and prefix validation").into());
    }
    let current = self.validate_selected_work(publisher, &observation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let expected = Some(MutableSystemControlExpectationV1 {
      selected_slot: current.selected_slot,
      control_sequence: current.control_sequence,
      control_digest: current.control_digest,
    });
    drop(fresh);
    Ok(publisher.publish_admitted_mutable_system_control_with_observer(
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
    )?)
  }
}
