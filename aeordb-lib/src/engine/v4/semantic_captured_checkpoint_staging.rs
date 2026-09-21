//! Initial immutable checkpoint dependencies, not selected task authority.
use super::*;
use crate::engine::v4::hash::try_digest_parts;
use crate::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
use crate::engine::v4::semantic_mutation_control::{
  SemanticMutationCheckpointV1, SemanticMutationCursorV1, SemanticMutationPhaseV1, encode_semantic_mutation_checkpoint,
};
use crate::engine::v4::semantic_source_capture::{
  SemanticSourceCaptureV1, decode_semantic_source_capture_binding_v1, encode_semantic_source_capture_v1,
};
use crate::engine::v4::system_family::embedded_system_family_registry;

#[derive(Clone, Copy, Debug)]
pub struct NativeCapturedSemanticCheckpointRequestV1 {
  pub task_id: [u8; 16],
  pub mutation_count: u64,
  pub captured_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub maximum_workspace_bytes: usize,
}

pub(super) struct PreparedInitialSemanticCheckpointV1 {
  pub(super) encoded_checkpoint: Vec<u8>,
  pub(super) encoded_companion: Vec<u8>,
  pub(super) memory: MemoryReservation,
}

impl NativeStagedSemanticSourceUnionV1<'_> {
  /// Stage the initial Captured dependency pair under the union's protection.
  /// This never selects a task or grants retention after the guard is dropped.
  /// Capture time is the caller's observation time, not the later write time.
  pub fn stage_initial_checkpoint(
    &self,
    request: NativeCapturedSemanticCheckpointRequestV1,
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    self.stage_initial_checkpoint_observed(request, || {}, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  pub(in crate::engine::v4::first_authority) fn stage_initial_checkpoint_observed(
    &self,
    request: NativeCapturedSemanticCheckpointRequestV1,
    before_lock: impl FnOnce(),
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
    let PreparedInitialSemanticCheckpointV1 { encoded_checkpoint, encoded_companion, memory } = self.prepare_initial_checkpoint(request)?;
    let capture = self.union.captured_inventory();
    let mut identity = [0; 24];
    identity[..16].copy_from_slice(&request.task_id);
    identity[16..].copy_from_slice(&1u64.to_le_bytes());
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
    capture.stage_captured_source_controls(&controls, request.publication_timestamp_ms, &memory, before_lock, observer)
  }

  pub(super) fn prepare_initial_checkpoint(
    &self,
    request: NativeCapturedSemanticCheckpointRequestV1,
  ) -> Result<PreparedInitialSemanticCheckpointV1, SemanticMutationObservationErrorV1> {
    let capture = self.union.captured_inventory();
    check_cancelled(&capture.cancellation)?;
    capture._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let header = self.union.captured_header();
    if request.task_id.iter().all(|byte| *byte == 0) || request.mutation_count == 0 {
      return Err(invalid("semantic_captured_checkpoint_identity", "initial checkpoint requires a task ID and accepted mutation count"));
    }
    if request.captured_at_ms < 0
      || (request.captured_at_ms as u64) < header.updated_at_ms
      || request.publication_timestamp_ms == 0
      || request.publication_timestamp_ms > i64::MAX as u64
      || request.publication_timestamp_ms < request.captured_at_ms as u64
    {
      return Err(invalid(
        "semantic_captured_checkpoint_time",
        "capture/publication times must be ordered within the signed persistent range",
      ));
    }
    let algorithm = header.hash_algorithm;
    // Fixed Captured envelopes plus the existing source publisher's simultaneous
    // wrapper/decode/transaction scratch. No reservation scales with sources or
    // claimed logical mutations. Charge before either encoder allocates output.
    let body_bytes = 2 * 36 + 168 + 112 + 15 * algorithm.hash_length();
    let workspace = body_bytes
      .checked_add(2 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP)
      .and_then(|bytes| bytes.checked_mul(8))
      .and_then(|bytes| bytes.checked_add((128 << 10) + 2 * body_bytes))
      .filter(|bytes| *bytes <= request.maximum_workspace_bytes)
      .ok_or(SemanticMutationObservationErrorV1::Resource {
        code: "semantic_captured_checkpoint_workspace",
        message: "initial checkpoint staging exceeds its admitted workspace",
      })?;
    let memory = capture
      .memory
      .reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let registry = embedded_system_family_registry(algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    let checkpoint = SemanticMutationCheckpointV1 {
      database_id: &header.database_id,
      task_id: &request.task_id,
      checkpoint_sequence: 1,
      physical_instance_id: &header.physical_instance_id,
      writer_fence_epoch: header.writer_fence_epoch,
      semantic_generation: self.union.generation_selection().control_sequence,
      header_sequence: header.slot_sequence,
      captured_at_ms: request.captured_at_ms,
      phase: SemanticMutationPhaseV1::Captured,
      cursor: SemanticMutationCursorV1::None,
      expected_configuration_count: self.union.requested_configuration_count(),
      configuration_count: 0,
      record_count: 0,
      node_count: 0,
      dependency_count: 0,
      mutation_count: request.mutation_count,
      activation_generation: 0,
      pruning_record_count: 0,
      pruning_node_count: 0,
      base_namespace_root: &self.union.base_authority().root_hash,
      staged_directory_root: self.union.requested_directory_root(),
      catalog_root: None,
      pruning_catalog_root: None,
      semantic_state: None,
      candidate_namespace_root: None,
      compiler_fingerprint: semantic_compiler_fingerprint_v1(algorithm),
      semantic_registry_fingerprint: &registry.semantic_projection_fingerprint,
      source_identity_fingerprint: self.union.fingerprint().digest(),
    };
    let encoded_checkpoint =
      encode_semantic_mutation_checkpoint(&checkpoint, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
    let digest = try_digest_parts(algorithm, &[&encoded_checkpoint]).map_err(|source| SemanticMutationObservationErrorV1::Allocation {
      code: "semantic_captured_checkpoint_digest_allocation",
      source,
    })?;
    let encoded_companion = encode_semantic_source_capture_v1(
      &SemanticSourceCaptureV1 {
        database_id: checkpoint.database_id,
        task_id: checkpoint.task_id,
        checkpoint_sequence: checkpoint.checkpoint_sequence,
        physical_instance_id: checkpoint.physical_instance_id,
        writer_fence_epoch: checkpoint.writer_fence_epoch,
        semantic_generation: checkpoint.semantic_generation,
        header_sequence: checkpoint.header_sequence,
        captured_at_ms: checkpoint.captured_at_ms,
        protected_path_count: self.union.catalogs().path_count(),
        base_catalog_node_count: self.union.catalogs().node_count(),
        requested_catalog_node_count: self.union.catalogs().node_count(),
        base_namespace_root: checkpoint.base_namespace_root,
        staged_directory_root: checkpoint.staged_directory_root,
        base_source_catalog: self.union.catalogs().base_root(),
        requested_source_catalog: self.union.catalogs().requested_root(),
        source_identity_fingerprint: checkpoint.source_identity_fingerprint,
        checkpoint_payload_hash: &digest,
      },
      algorithm,
    )
    .map_err(SemanticMutationObservationErrorV1::from)?;
    decode_semantic_source_capture_binding_v1(&encoded_companion, &encoded_checkpoint, algorithm)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(PreparedInitialSemanticCheckpointV1 { encoded_checkpoint, encoded_companion, memory })
  }
}
