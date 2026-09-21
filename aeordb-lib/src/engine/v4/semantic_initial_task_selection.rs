//! Initial task selection from an opaque staged source union and exact graph.
use super::*;
use crate::engine::v4::contract_generated::capability_bit;
use crate::engine::v4::hash::try_digest_parts;
use crate::engine::v4::semantic_mutation_control::{SemanticMutationTaskStateV1, SemanticMutationTaskV1, encode_semantic_mutation_task};

#[derive(Clone, Copy, Debug)]
pub struct NativeInitialSemanticTaskSelectionRequestV1 {
  pub checkpoint: NativeCapturedSemanticCheckpointRequestV1,
  pub holder_boot_id: [u8; 16],
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub inventory_bounds: NativeSemanticMutationInventoryBoundsV1,
  pub graph_bounds: NativeSemanticTaskGraphBoundsV1,
  pub maximum_workspace_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeInitialSemanticTaskSelectionErrorV1 {
  #[error(transparent)]
  Observation(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Graph(#[from] SemanticTaskGraphErrorV1),
  #[error(transparent)]
  Publication(#[from] MutableSystemControlPublicationErrorV1),
}

impl NativeInitialSemanticTaskSelectionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Observation(source) => source.code(),
      Self::Graph(source) => source.code(),
      Self::Publication(source) => source.code(),
    }
  }

  pub fn committed_receipt(&self) -> Option<&MutableSystemControlPublicationReceiptV1> {
    match self {
      Self::Publication(source) => source.committed_receipt(),
      Self::Observation(_) | Self::Graph(_) => None,
    }
  }
}

impl NativeStagedSemanticSourceUnionV1<'_> {
  /// Select only this union's already-staged initial checkpoint. The operation
  /// retains the union's staging protection through durable ASMT publication.
  /// It neither activates a namespace nor grants resume/takeover authority.
  pub fn select_initial_task(
    &self,
    request: NativeInitialSemanticTaskSelectionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MutableSystemControlPublicationReceiptV1, NativeInitialSemanticTaskSelectionErrorV1> {
    self.select_initial_task_observed(request, retirement_owner, || {}, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  pub(in crate::engine::v4::first_authority) fn select_initial_task_observed(
    &self,
    request: NativeInitialSemanticTaskSelectionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    before_lock: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<MutableSystemControlPublicationReceiptV1, NativeInitialSemanticTaskSelectionErrorV1> {
    let capture = self.union.captured_inventory();
    check_cancelled(&capture.cancellation)?;
    capture._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if request.holder_boot_id.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_task_initial_holder", "initial task selection requires a nonzero boot identity").into());
    }
    if request.publication_timestamp_ms == 0
      || request.publication_timestamp_ms > i64::MAX as u64
      || request.checkpoint.captured_at_ms < 0
      || request.publication_timestamp_ms < request.checkpoint.captured_at_ms as u64
      || request.monotonic_now_ms == 0
    {
      return Err(invalid("semantic_task_initial_time", "initial task publication requires ordered, nonzero bounded clocks").into());
    }
    let header = self.union.captured_header();
    if retirement_owner.hash_algorithm() != header.hash_algorithm || retirement_owner.database_id() != header.database_id {
      return Err(invalid("semantic_task_initial_retirement_owner", "task and retirement owner belong to different databases").into());
    }
    let workspace = (36usize + 112 + header.hash_algorithm.hash_length())
      .checked_mul(16)
      .and_then(|bytes| bytes.checked_add(8 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP + (128 << 10)))
      .filter(|bytes| *bytes <= request.maximum_workspace_bytes)
      .ok_or(SemanticMutationObservationErrorV1::Resource {
        code: "semantic_task_initial_workspace",
        message: "initial task selection exceeds its admitted publication workspace",
      })?;
    let memory = capture
      .memory
      .reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let pair = self.prepare_initial_checkpoint(request.checkpoint)?;
    let digest = try_digest_parts(header.hash_algorithm, &[&pair.encoded_checkpoint])
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_task_initial_digest_allocation", source })?;
    let encoded_task = encode_semantic_mutation_task(
      &SemanticMutationTaskV1 {
        control_sequence: 1,
        database_id: &header.database_id,
        task_id: &request.checkpoint.task_id,
        physical_instance_id: &header.physical_instance_id,
        holder_boot_id: &request.holder_boot_id,
        fencing_token: 1,
        writer_fence_epoch: header.writer_fence_epoch,
        created_at_ms: request.checkpoint.captured_at_ms,
        updated_at_ms: request.checkpoint.captured_at_ms,
        state: SemanticMutationTaskStateV1::Queued,
        pins_released: false,
        checkpoint_sequence: 1,
        checkpoint_payload_hash: &digest,
      },
      header.hash_algorithm,
    )
    .map_err(SemanticMutationObservationErrorV1::from)?;
    let publisher = capture._protection.publisher();
    // Settle lineage before qualification. The common write body must not
    // perform another pre-publication flush that would silently stale evidence.
    retirement_owner
      .flush(&mut SharedFirstAuthorityRetirementSinkV1 { publisher })
      .map_err(MutableSystemControlPublicationErrorV1::from)?;
    let prior_hard_publication_sequence = retirement_owner.status().last_hard_publication_sequence;
    let fresh =
      capture._protection.capture_semantic_mutation_inventory(request.inventory_bounds, &capture.memory, &capture.cancellation)?;
    validate_initial_task_owner(header, &fresh.header.selected.header)?;
    fresh.visit_captured_semantic_checkpoint_metadata_entries_expected(
      &request.checkpoint.task_id,
      1,
      request.graph_bounds,
      Some((&pair.encoded_checkpoint, &pair.encoded_companion)),
      |_| Ok(()),
    )?;
    pair.memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    before_lock();
    let SelectedSemanticAuthorityGuardV1 { _authority: authority, .. } =
      publisher.selected_semantic_authority_guard().map_err(SemanticMutationObservationErrorV1::from)?;
    check_cancelled(&capture.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    capture._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if authority.staging_accounting_failed || authority.active_staging_protections == 0 {
      return Err(invalid("semantic_task_initial_protection", "initial task selection lost healthy staging protection").into());
    }
    let observation = publisher.observe().map_err(SemanticMutationObservationErrorV1::from)?;
    if observation.selected.redundancy_degraded || observation.region != fresh.header.region {
      return Err(invalid("semantic_task_initial_frontier", "authority changed after checkpoint validation; capture again").into());
    }
    validate_initial_task_owner(header, &observation.selected.header)?;
    let current_task = {
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
      if generation.as_ref() != Some(self.union.generation_selection()) {
        return Err(invalid("semantic_task_initial_generation", "semantic generation changed after source capture").into());
      }
      let current = load_mutable_system_control_pair(
        &publisher.file,
        &kv,
        &observation.selected.header,
        SystemControlKindV1::SemanticMutationTask,
        &request.checkpoint.task_id,
      )
      .map_err(SemanticMutationObservationErrorV1::from)?
      .selected;
      if current.as_ref().is_some_and(|task| task.bytes != encoded_task) {
        validate_mutable_system_control_expectation(current.as_ref(), None, header.hash_algorithm)?;
      }
      current
    };
    check_cancelled(&capture.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    // The guarded frontier now replaces the transient graph capture; the source
    // union's original process protection remains alive through publication.
    drop(fresh);
    drop(pair);
    // An exact retry is read-only. Do not enter the physical writer's legacy
    // KV flush, which can settle buffered pages even when no task is selected.
    // The same root guard still covers every final check and this projection.
    if let Some(current) = current_task {
      return Ok(idempotent_mutable_system_control_receipt(&current, observation));
    }
    Ok(publisher.publish_admitted_mutable_system_control_with_observer(
      AdmittedMutableSystemControlPublicationV1 {
        publisher,
        authority,
        observation,
        request: MutableSystemControlPublicationRequestV1 {
          database_id: &header.database_id,
          kind: SystemControlKindV1::SemanticMutationTask,
          identity: &request.checkpoint.task_id,
          expected: None,
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

fn validate_initial_task_owner(captured: &DatabaseHeaderV4, current: &DatabaseHeaderV4) -> Result<(), SemanticMutationObservationErrorV1> {
  if current.database_id != captured.database_id
    || current.hash_algorithm != captured.hash_algorithm
    || current.physical_instance_id != captured.physical_instance_id
    || current.writer_fence_epoch != captured.writer_fence_epoch
  {
    return Err(invalid("semantic_task_initial_owner", "initial task selection cannot cross physical ownership or writer fences"));
  }
  if current.slot_sequence < captured.slot_sequence || current.write_sequence_high_water < captured.write_sequence_high_water {
    return Err(invalid("semantic_task_initial_regression", "current authority regressed behind its captured source owner"));
  }
  for bit in [capability_bit::SEMANTIC_MUTATION_TASK_V1, capability_bit::SEMANTIC_SOURCE_CAPTURE_V1] {
    let index = usize::from(bit / 8);
    let mask = 1u8 << (bit % 8);
    if current.required_reader_capabilities[index] & mask == 0
      || current.required_writer_capabilities[index] & mask == 0
      || captured.required_reader_capabilities[index] & mask == 0
      || captured.required_writer_capabilities[index] & mask == 0
    {
      return Err(invalid(
        "semantic_task_initial_capability",
        "initial task selection requires captured and current task/source capabilities",
      ));
    }
  }
  Ok(())
}
