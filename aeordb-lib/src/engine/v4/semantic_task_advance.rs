//! Bounded native compiler continuation; no activation or pin-release authority.
use super::*;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticTaskCompilerAdvanceRequestV1 {
  pub maximum_configuration_steps: u64,
  pub maximum_pruning_steps: u64,
  pub compiler_bounds: NativeSemanticCompilerProgressBoundsV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub maximum_workspace_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct NativeSemanticTaskCompilerAdvanceReceiptV1 {
  pub publication: MutableSystemControlPublicationReceiptV1,
  pub phase: SemanticMutationPhaseV1,
  pub configuration_steps: u64,
  pub pruning_steps: u64,
}

impl NativeSemanticTaskWorkV1<'_> {
  pub fn advance_compilation(
    self,
    request: NativeSemanticTaskCompilerAdvanceRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<NativeSemanticTaskCompilerAdvanceReceiptV1, NativeSemanticTaskWorkErrorV1> {
    self.advance_compilation_observed(
      request,
      retirement_owner,
      (|| {}, || {}, || {}),
      (
        &mut NoopFirstAuthorityDependencyObserverV1,
        &mut NoopFirstAuthorityDependencyObserverV1,
        &mut NoopFirstAuthorityDependencyObserverV1,
      ),
    )
  }

  pub(in crate::engine::v4::first_authority) fn advance_compilation_observed(
    self,
    request: NativeSemanticTaskCompilerAdvanceRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    hooks: (impl FnOnce(), impl FnOnce(), impl FnOnce()),
    observers: (
      &mut dyn FirstAuthorityDependencyObserverV1,
      &mut dyn FirstAuthorityDependencyObserverV1,
      &mut dyn FirstAuthorityDependencyObserverV1,
    ),
  ) -> Result<NativeSemanticTaskCompilerAdvanceReceiptV1, NativeSemanticTaskWorkErrorV1> {
    let (before_candidate, before_pair, before_selection) = hooks;
    let (candidate_observer, pair_observer, selection_observer) = observers;
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let header = &self._observed.header.selected.header;
    let task =
      decode_semantic_mutation_task(&self._encoded_task, header.hash_algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    let previous = self
      ._observed
      .checkpoint()
      .map_err(SemanticMutationObservationErrorV1::from)?
      .ok_or_else(|| invalid("semantic_task_work_checkpoint", "continuation has no selected checkpoint"))?;
    if task.pins_released
      || task.state != SemanticMutationTaskStateV1::Compiling
      || !matches!(previous.phase, SemanticMutationPhaseV1::Compiling | SemanticMutationPhaseV1::Pruning)
    {
      return Err(invalid("semantic_task_work_phase", "advance requires held Compiling or Pruning work").into());
    }
    if request.maximum_configuration_steps == 0 || request.maximum_pruning_steps == 0 {
      return Err(invalid("semantic_task_advance_steps", "configuration and pruning step ceilings must be positive").into());
    }
    if request.publication_timestamp_ms == 0
      || request.publication_timestamp_ms > i64::MAX as u64
      || request.publication_timestamp_ms < self.request.publication_timestamp_ms
      || request.monotonic_now_ms == 0
      || request.monotonic_now_ms < self.request.monotonic_now_ms
    {
      return Err(invalid("semantic_task_work_time", "compiler publication requires ordered, bounded clocks").into());
    }
    if retirement_owner.hash_algorithm() != header.hash_algorithm || retirement_owner.database_id() != header.database_id {
      return Err(invalid("semantic_task_work_retirement_owner", "task and retirement owner belong to different databases").into());
    }
    retirement_owner.preflight_operation(request.monotonic_now_ms).map_err(MutableSystemControlPublicationErrorV1::from)?;
    let memory = self.reserve_compiler_publication_workspace(request.maximum_workspace_bytes)?;
    let publisher = self._protection.publisher();
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
    let mut task_id = [0; 16];
    task_id.copy_from_slice(task.task_id);
    let capture = self._protection.capture_semantic_mutation_inventory(self.request.inventory_bounds, &self.memory, &self.cancellation)?;
    validate_initial_task_owner(header, &capture.header.selected.header)?;
    capture
      .visit_captured_semantic_checkpoint_metadata_entries(&task_id, task.checkpoint_sequence, self.request.graph_bounds, |_| Ok(()))?;
    let old_checkpoint =
      self._observed.checkpoint.as_ref().ok_or_else(|| invalid("semantic_task_work_checkpoint", "work lost its original checkpoint"))?;
    let batch = capture.advance_captured_semantic_compiler(
      (&task_id, task.checkpoint_sequence, self.reserved_checkpoint_sequence, &old_checkpoint.bytes),
      request,
      || {
        let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
          publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
        self.validate_work_protection(&authority)?;
        let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
        if observation.region != capture.header.region {
          return Err(invalid("semantic_task_work_frontier", "authority changed during compiler prefix validation").into());
        }
        self.validate_selected_work(publisher, &observation)?;
        memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
        Ok(())
      },
    )?;
    drop(capture);
    if let Some(candidate) = &batch.candidate {
      self.stage_compiler_candidate(candidate, request.publication_timestamp_ms, &memory, before_candidate, candidate_observer)?;
    }
    let checkpoint =
      crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_checkpoint(&batch.checkpoint, header.hash_algorithm)
        .map_err(SemanticMutationObservationErrorV1::from)?;
    let companion = crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1(&batch.companion, header.hash_algorithm)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let prepared = self.prepare_compiler_checkpoint(&checkpoint, &companion, request.publication_timestamp_ms)?;
    let (phase, configuration_steps, pruning_steps) = (batch.phase, batch.configuration_steps, batch.pruning_steps);
    drop(batch);
    let publication = self.select_compiler_checkpoint(
      prepared,
      TaskCompilerPublicationContextV1 {
        request: NativeSemanticTaskCompilerStartRequestV1 {
          compiler_bounds: request.compiler_bounds,
          publication_timestamp_ms: request.publication_timestamp_ms,
          monotonic_now_ms: request.monotonic_now_ms,
          maximum_workspace_bytes: request.maximum_workspace_bytes,
        },
        memory: &memory,
        prior_hard_publication_sequence,
      },
      retirement_owner,
      (before_pair, before_selection, pair_observer, selection_observer),
    )?;
    Ok(NativeSemanticTaskCompilerAdvanceReceiptV1 { publication, phase, configuration_steps, pruning_steps })
  }

  fn stage_compiler_candidate(
    &self,
    candidate: &EncodedNamespaceRootV1,
    publication_timestamp_ms: u64,
    memory: &MemoryReservation,
    before_lock: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<ImmutableEntityBatchPublicationReceiptV1, NativeSemanticTaskWorkErrorV1> {
    let publisher = self._protection.publisher();
    let captured = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    let decoded =
      decode_namespace_root(&candidate.value, captured.selected.header.hash_algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    if decoded.root_hash != candidate.root_hash {
      return Err(invalid("semantic_task_candidate_identity", "derived root does not match its canonical identity").into());
    }
    before_lock();
    let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
      publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
    self.validate_work_protection(&authority)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    if observation.region != captured.region {
      return Err(invalid("semantic_task_work_frontier", "authority changed before candidate staging").into());
    }
    self.validate_selected_work(publisher, &observation)?;
    let entities = [ImmutableEntityWriteV1 {
      entity_version: 1,
      entry_type: EntryTypeV4::DirectoryIndex,
      flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
      key: &candidate.root_hash,
      stored_value: &candidate.value,
    }];
    let request = ImmutableEntityBatchPublicationRequestV1 {
      database_id: &observation.selected.header.database_id,
      entities: &entities,
      publication_timestamp_ms,
    };
    validate_immutable_entity_batch_request(&request)?;
    // The existing physical immutable publisher remains the only writer. This
    // narrow derived-root path grants no generic v1-root publication surface.
    let receipt = publisher.publish_immutable_entity_batch_with_validation_locked(
      request,
      observer,
      ImmutableEntityValidationV1::PrevalidatedSemanticTaskCandidate,
    )?;
    drop(authority);
    Ok(receipt)
  }
}
