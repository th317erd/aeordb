//! Fenced owner for the disconnected v3-to-v4 migration controls.
//!
//! The owner consumes one opaque preflight permit and publishes only through
//! the physical v4 first authority. It deliberately has no live service,
//! namespace, source-file, or garbage-collection authority.

use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::sync::Arc;

use super::first_authority::{
  FirstAuthorityPublicationErrorV1, LoadedMutableSystemControlV1, MutableSystemControlAuthorityExpectationV1,
  MutableSystemControlExpectationV1, MutableSystemControlGuardV1, MutableSystemControlPublicationErrorV1,
  MutableSystemControlPublicationRequestV1, MutableSystemControlPublicationReceiptV1, V4FirstAuthorityPublisher,
};
use super::gc_retirement::{RetirementJournalOwnerErrorV1, RetirementJournalOwnerV1};
use super::header_publication::DatabaseHeaderObservationV4;
use super::migration_control::{
  MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE,
  MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED, MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, MigrationLeaseBodyV1,
  MigrationLeaseControlV1, MigrationLeaseStateV1, MigrationPhaseV1, MigrationProgressBodyV1, MigrationProgressControlV1,
  MigrationProgressStateV1, decode_migration_lease_control, decode_migration_progress_control, encode_migration_lease_control,
  encode_migration_progress_control,
};
use super::migration_final_authority_reconciliation::MigrationFinalAuthorityReconciliationProofV1;
use super::migration_preflight::MigrationPreflightPermitV1;
use super::reader::FormatError;
use super::system_control::SystemControlKindV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationAcquisitionRequestV1 {
  pub holder_boot_id: [u8; 16],
  pub acquired_at_ms: i64,
  pub lease_duration_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationAcquisitionReceiptV1 {
  pub lease_control_sequence: u64,
  pub progress_control_sequence: u64,
  pub fencing_token: u64,
  pub resumed_partial: bool,
  pub idempotent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationLeaseRenewalRequestV1 {
  pub renewed_at_ms: i64,
  pub lease_duration_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationLeaseReleaseRequestV1 {
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationTakeoverRequestV1 {
  pub new_holder_boot_id: [u8; 16],
  pub expected_fencing_token: u64,
  pub takeover_at_ms: i64,
  pub lease_duration_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgressTransitionRequestV1 {
  pub phase: MigrationPhaseV1,
  pub state: MigrationProgressStateV1,
  pub flags: u32,
  pub destination_header_sequence: u64,
  pub copied_through_write_sequence: u64,
  pub reconciled_through_publication_sequence: u64,
  pub namespace_count: u64,
  pub entity_count: u64,
  pub copied_bytes: u64,
  pub updated_at_ms: i64,
  pub legacy_root_map_control_payload_hash: Vec<u8>,
  pub last_error_evidence: Vec<u8>,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureCheckpointPublicationRequestV1 {
  pub captured_through_publication_sequence: u64,
  pub checkpoint_artifact: Vec<u8>,
  pub updated_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationFullReconciliationLatchRequestV1 {
  pub last_error_evidence: Vec<u8>,
  pub updated_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationReplayCheckpointPublicationRequestV1 {
  pub reconciled_through_publication_sequence: u64,
  pub destination_header_sequence: u64,
  pub updated_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Copy)]
pub struct MigrationFinalFreezeCompletionRequestV1<'proof, 'freeze, 'source> {
  pub proof: &'proof MigrationFinalAuthorityReconciliationProofV1<'freeze, 'source>,
  pub updated_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationLeaseRenewalReceiptV1 {
  pub control_sequence: u64,
  pub fencing_token: u64,
  pub expires_at_ms: i64,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationLeaseReleaseReceiptV1 {
  pub control_sequence: u64,
  pub fencing_token: u64,
  pub resumed_releasing: bool,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationTakeoverReceiptV1 {
  pub lease_control_sequence: u64,
  pub progress_control_sequence: u64,
  pub fencing_token: u64,
  pub resumed_rebind: bool,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgressTransitionReceiptV1 {
  pub control_sequence: u64,
  pub fencing_token: u64,
  pub phase: MigrationPhaseV1,
  pub state: MigrationProgressStateV1,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureStateObservationV1 {
  pub control_sequence: u64,
  pub captured_through_publication_sequence: u64,
  pub reconciled_through_publication_sequence: u64,
  pub destination_header_sequence: u64,
  pub checkpoint_artifact: Vec<u8>,
  pub needs_full_reconciliation: bool,
  pub last_error_evidence: Vec<u8>,
}

struct MigrationProgressPublicationContextV1 {
  loaded_lease: LoadedMutableSystemControlV1,
  loaded_progress: LoadedMutableSystemControlV1,
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
}

pub struct MigrationStateOwnerV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
  permit: MigrationPreflightPermitV1,
  holder_boot_id: [u8; 16],
  fencing_token: u64,
}

impl Debug for MigrationStateOwnerV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("MigrationStateOwnerV1")
      .field("database_id", &self.permit.database_id())
      .field("migration_id", &self.permit.migration_id())
      .field("holder_boot_id", &self.holder_boot_id)
      .field("fencing_token", &self.fencing_token)
      .finish_non_exhaustive()
  }
}

impl MigrationStateOwnerV1 {
  pub fn acquire(
    publisher: Arc<V4FirstAuthorityPublisher>,
    permit: MigrationPreflightPermitV1,
    request: MigrationAcquisitionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(Self, MigrationAcquisitionReceiptV1), MigrationStateOwnerErrorV1> {
    validate_acquisition_request(request)?;
    validate_destination_authority(&publisher, &permit)?;

    let (initial_lease, initial_progress) = load_migration_controls(&publisher, &permit)?;
    if initial_lease.is_none() && initial_progress.is_some() {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_progress_without_lease",
        "migration progress exists without a selected migration lease",
      ));
    }

    let lease_was_present = initial_lease.is_some();
    let progress_was_present = initial_progress.is_some();
    let lease = match initial_lease {
      Some((_, lease)) => {
        validate_held_lease(&lease, &permit, request)?;
        lease
      }
      None => {
        let expires_at_ms = request
          .acquired_at_ms
          .checked_add(request.lease_duration_ms)
          .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_time_overflow", "migration lease expiry overflowed"))?;
        let body = MigrationLeaseBodyV1 {
          database_id: permit.database_id(),
          migration_id: permit.migration_id(),
          source_physical_instance_id: permit.source_physical_instance_id(),
          destination_physical_instance_id: permit.destination_physical_instance_id(),
          holder_boot_id: request.holder_boot_id,
          fencing_token: 1,
          acquired_at_ms: request.acquired_at_ms,
          renewed_at_ms: request.acquired_at_ms,
          expires_at_ms,
          source_header_sequence: permit.source_header_sequence(),
          state: MigrationLeaseStateV1::Held,
        };
        let encoded = encode_migration_lease_control(1, &body, permit.hash_algorithm())?;
        let receipt = match publish_control(
          &publisher,
          &permit,
          SystemControlKindV1::MigrationLease,
          None,
          &[],
          &encoded,
          request.publication_timestamp_ms,
          request.monotonic_now_ms,
          retirement_owner,
        ) {
          Ok(receipt) => receipt,
          Err(source) if source.committed_receipt().is_some() => {
            return Err(MigrationStateOwnerErrorV1::LeaseCommitted { source: Box::new(source) });
          }
          Err(source) => return Err(MigrationStateOwnerErrorV1::Publication(source)),
        };
        MigrationLeaseControlV1 { sequence: receipt.control_sequence, body }
      }
    };

    let progress_publication_timestamp = request.publication_timestamp_ms.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress publication timestamp overflowed")
    })?;
    let progress_monotonic_now = request.monotonic_now_ms.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress monotonic timestamp overflowed")
    })?;
    let progress = match initial_progress {
      Some((_, progress)) => {
        validate_bound_progress(&progress, &lease, &permit)?;
        progress
      }
      None => {
        let hash_width = permit.hash_algorithm().hash_length();
        let body = MigrationProgressBodyV1 {
          database_id: permit.database_id(),
          migration_id: permit.migration_id(),
          source_physical_instance_id: permit.source_physical_instance_id(),
          destination_physical_instance_id: permit.destination_physical_instance_id(),
          fencing_token: lease.body.fencing_token,
          phase: MigrationPhaseV1::Preflight,
          state: MigrationProgressStateV1::Pending,
          flags: 0,
          source_header_sequence: permit.source_header_sequence(),
          destination_header_sequence: 0,
          copied_through_write_sequence: 0,
          captured_through_publication_sequence: 0,
          reconciled_through_publication_sequence: 0,
          namespace_count: 0,
          entity_count: 0,
          copied_bytes: 0,
          updated_at_ms: lease.body.acquired_at_ms,
          source_capture_head: permit.source_capture_head().to_vec(),
          checkpoint_artifact: vec![0; hash_width],
          legacy_root_map_control_payload_hash: vec![0; hash_width],
          effective_config_fingerprint: permit.effective_configuration_fingerprint().to_vec(),
          system_family_registry_fingerprint: permit.system_family_registry_fingerprint().to_vec(),
          last_error_evidence: vec![0; hash_width],
        };
        let encoded = encode_migration_progress_control(1, &body, permit.hash_algorithm())?;
        let receipt = match publish_control(
          &publisher,
          &permit,
          SystemControlKindV1::MigrationProgress,
          None,
          &[],
          &encoded,
          progress_publication_timestamp,
          progress_monotonic_now,
          retirement_owner,
        ) {
          Ok(receipt) => receipt,
          Err(source) if source.committed_receipt().is_some() => {
            return Err(MigrationStateOwnerErrorV1::AcquisitionCommitted {
              lease_control_sequence: lease.sequence,
              source: Box::new(source),
            });
          }
          Err(source) => {
            return Err(MigrationStateOwnerErrorV1::PartialAcquisition {
              lease_control_sequence: lease.sequence,
              source: Box::new(source),
            });
          }
        };
        MigrationProgressControlV1 { sequence: receipt.control_sequence, body }
      }
    };

    let owner = Self { publisher, permit, holder_boot_id: lease.body.holder_boot_id, fencing_token: lease.body.fencing_token };
    let receipt = MigrationAcquisitionReceiptV1 {
      lease_control_sequence: lease.sequence,
      progress_control_sequence: progress.sequence,
      fencing_token: lease.body.fencing_token,
      resumed_partial: lease_was_present && !progress_was_present,
      idempotent: lease_was_present && progress_was_present,
    };
    Ok((owner, receipt))
  }

  pub fn renew(
    &self,
    request: MigrationLeaseRenewalRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationLeaseRenewalReceiptV1, MigrationStateOwnerErrorV1> {
    if request.renewed_at_ms < 0 || request.lease_duration_ms <= 0 {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_renewal_times",
        "migration renewal time must be nonnegative and lease duration must be positive",
      ));
    }
    let expires_at_ms = request
      .renewed_at_ms
      .checked_add(request.lease_duration_ms)
      .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_time_overflow", "migration renewal expiry overflowed"))?;
    let publication_time_ms = validate_publication_clock(request.publication_timestamp_ms, request.monotonic_now_ms, i64::MAX as u64)?;
    validate_transition_publication_time(publication_time_ms, request.renewed_at_ms)?;
    validate_destination_authority(&self.publisher, &self.permit)?;
    let ((loaded_lease, lease), (loaded_progress, progress)) = require_migration_controls(&self.publisher, &self.permit)?;
    validate_owned_held_controls(self, &lease, &progress, publication_time_ms)?;
    if request.renewed_at_ms < lease.body.renewed_at_ms {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_renewal_time_regression",
        "migration renewal time cannot precede the selected lease renewal",
      ));
    }

    let mut body = lease.body.clone();
    body.renewed_at_ms = request.renewed_at_ms;
    body.expires_at_ms = expires_at_ms;
    if body == lease.body {
      return Ok(MigrationLeaseRenewalReceiptV1 {
        control_sequence: lease.sequence,
        fencing_token: lease.body.fencing_token,
        expires_at_ms: lease.body.expires_at_ms,
        idempotent: true,
      });
    }
    if expires_at_ms <= lease.body.expires_at_ms {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_renewal_not_extended",
        "migration renewal must strictly extend the selected lease expiry",
      ));
    }
    let sequence = lease
      .sequence
      .checked_add(1)
      .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_sequence_exhausted", "migration lease sequence is exhausted"))?;
    let encoded = encode_migration_lease_control(sequence, &body, self.permit.hash_algorithm())?;
    let guard_identity = self.permit.migration_id();
    let guards = [MutableSystemControlGuardV1 {
      kind: SystemControlKindV1::MigrationProgress,
      identity: &guard_identity,
      expected: control_expectation(&loaded_progress),
    }];
    let receipt = publish_owned_control(
      &self.publisher,
      &self.permit,
      SystemControlKindV1::MigrationLease,
      Some(control_expectation(&loaded_lease)),
      &guards,
      &encoded,
      request.publication_timestamp_ms,
      request.monotonic_now_ms,
      retirement_owner,
      MigrationCommittedControlV1::Lease,
    )?;
    Ok(MigrationLeaseRenewalReceiptV1 {
      control_sequence: receipt.control_sequence,
      fencing_token: body.fencing_token,
      expires_at_ms: body.expires_at_ms,
      idempotent: receipt.idempotent,
    })
  }

  pub fn transition_progress(
    &self,
    request: MigrationProgressTransitionRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    if request.phase as u16 > MigrationPhaseV1::FinalFreeze as u16
      || (request.phase == MigrationPhaseV1::FinalFreeze && request.state == MigrationProgressStateV1::Complete)
    {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_progress_specialized_authority",
        "final-freeze completion and every later phase require their specialized live-proof authority",
      ));
    }
    let (context, progress) =
      self.prepare_progress_publication(request.updated_at_ms, request.publication_timestamp_ms, request.monotonic_now_ms)?;

    let body = MigrationProgressBodyV1 {
      database_id: progress.body.database_id,
      migration_id: progress.body.migration_id,
      source_physical_instance_id: progress.body.source_physical_instance_id,
      destination_physical_instance_id: progress.body.destination_physical_instance_id,
      fencing_token: progress.body.fencing_token,
      phase: request.phase,
      state: request.state,
      flags: request.flags,
      source_header_sequence: progress.body.source_header_sequence,
      destination_header_sequence: request.destination_header_sequence,
      copied_through_write_sequence: request.copied_through_write_sequence,
      captured_through_publication_sequence: progress.body.captured_through_publication_sequence,
      reconciled_through_publication_sequence: request.reconciled_through_publication_sequence,
      namespace_count: request.namespace_count,
      entity_count: request.entity_count,
      copied_bytes: request.copied_bytes,
      updated_at_ms: request.updated_at_ms,
      source_capture_head: progress.body.source_capture_head.clone(),
      checkpoint_artifact: progress.body.checkpoint_artifact.clone(),
      legacy_root_map_control_payload_hash: request.legacy_root_map_control_payload_hash,
      effective_config_fingerprint: progress.body.effective_config_fingerprint.clone(),
      system_family_registry_fingerprint: progress.body.system_family_registry_fingerprint.clone(),
      last_error_evidence: request.last_error_evidence,
    };
    if body == progress.body {
      return Ok(MigrationProgressTransitionReceiptV1 {
        control_sequence: progress.sequence,
        fencing_token: progress.body.fencing_token,
        phase: progress.body.phase,
        state: progress.body.state,
        idempotent: true,
      });
    }
    self.publish_progress_body(context, &progress, body, 0, false, retirement_owner)
  }

  pub fn publish_capture_checkpoint(
    &self,
    request: MigrationCaptureCheckpointPublicationRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    let hash_width = self.permit.hash_algorithm().hash_length();
    if request.checkpoint_artifact.len() != hash_width || all_zero(&request.checkpoint_artifact) {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_capture_checkpoint_identity",
        "selected capture checkpoint identity must be a nonzero database-profile hash",
      ));
    }
    let (context, progress) =
      self.prepare_progress_publication(request.updated_at_ms, request.publication_timestamp_ms, request.monotonic_now_ms)?;
    if progress.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE != 0 {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_capture_already_inexact",
        "optional capture checkpoints cannot advance after full reconciliation became mandatory",
      ));
    }
    let mut body = progress.body.clone();
    body.captured_through_publication_sequence = request.captured_through_publication_sequence;
    body.checkpoint_artifact = request.checkpoint_artifact;
    body.updated_at_ms = request.updated_at_ms;
    if body == progress.body {
      return Ok(progress_receipt(&progress, true));
    }
    self.publish_progress_body(context, &progress, body, 0, true, retirement_owner)
  }

  pub fn latch_needs_full_reconciliation(
    &self,
    request: MigrationFullReconciliationLatchRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    let hash_width = self.permit.hash_algorithm().hash_length();
    if request.last_error_evidence.len() != hash_width || all_zero(&request.last_error_evidence) {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_capture_failure_evidence",
        "full-reconciliation latching requires a nonzero database-profile evidence hash",
      ));
    }
    let (context, progress) =
      self.prepare_progress_publication(request.updated_at_ms, request.publication_timestamp_ms, request.monotonic_now_ms)?;
    if progress.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE != 0 {
      return Ok(progress_receipt(&progress, true));
    }
    let mut body = progress.body.clone();
    body.flags |= MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE;
    body.last_error_evidence = request.last_error_evidence;
    body.updated_at_ms = request.updated_at_ms;
    self.publish_progress_body(context, &progress, body, MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE, true, retirement_owner)
  }

  pub fn publish_replay_checkpoint(
    &self,
    request: MigrationReplayCheckpointPublicationRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    if request.reconciled_through_publication_sequence == 0 || request.destination_header_sequence == 0 {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_replay_destination_sequence",
        "capture replay checkpoint requires nonzero source and destination sequences",
      ));
    }
    let (context, progress) =
      self.prepare_progress_publication(request.updated_at_ms, request.publication_timestamp_ms, request.monotonic_now_ms)?;
    if request.reconciled_through_publication_sequence > progress.body.captured_through_publication_sequence {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_replay_beyond_capture",
        "capture replay cannot reconcile beyond the selected capture watermark",
      ));
    }
    if request.reconciled_through_publication_sequence == progress.body.reconciled_through_publication_sequence {
      if request.destination_header_sequence != progress.body.destination_header_sequence {
        return Err(MigrationStateOwnerErrorV1::invalid(
          "migration_replay_checkpoint_conflict",
          "an existing replay watermark is bound to a different destination header sequence",
        ));
      }
      return Ok(progress_receipt(&progress, true));
    }
    let mut body = progress.body.clone();
    body.reconciled_through_publication_sequence = request.reconciled_through_publication_sequence;
    body.destination_header_sequence = request.destination_header_sequence;
    body.updated_at_ms = request.updated_at_ms;
    if body == progress.body {
      return Ok(progress_receipt(&progress, true));
    }
    self.publish_progress_body(context, &progress, body, 0, true, retirement_owner)
  }

  pub fn complete_final_freeze(
    &self,
    request: MigrationFinalFreezeCompletionRequestV1<'_, '_, '_>,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    request
      .proof
      .validate_for_completion(&self.permit, &self.publisher)
      .map_err(|error| MigrationStateOwnerErrorV1::invalid(error.code(), error.to_string()))?;
    let closure = request.proof.closure();
    let (context, progress) =
      self.prepare_progress_publication(request.updated_at_ms, request.publication_timestamp_ms, request.monotonic_now_ms)?;
    if progress.body.phase != MigrationPhaseV1::FinalFreeze
      || !matches!(progress.body.state, MigrationProgressStateV1::Running | MigrationProgressStateV1::Complete)
    {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_final_freeze_phase",
        "specialized final-freeze completion requires running or already complete FinalFreeze progress",
      ));
    }
    if progress.body.flags & MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED == 0 {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_progress_gc_suspension_required",
        "final-freeze completion requires durable source GC suspension",
      ));
    }
    if closure.frozen_source_publication_sequence < progress.body.reconciled_through_publication_sequence
      || closure.destination_header_sequence < progress.body.destination_header_sequence
    {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_final_freeze_proof_regression",
        "live final-freeze proof is older than selected migration progress",
      ));
    }

    let mut body = progress.body.clone();
    body.phase = MigrationPhaseV1::FinalFreeze;
    body.state = MigrationProgressStateV1::Complete;
    body.flags |= MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD;
    body.destination_header_sequence = closure.destination_header_sequence;
    body.reconciled_through_publication_sequence = closure.frozen_source_publication_sequence;
    body.updated_at_ms = request.updated_at_ms;
    if body == progress.body {
      return Ok(progress_receipt(&progress, true));
    }
    request
      .proof
      .validate_for_completion(&self.permit, &self.publisher)
      .map_err(|error| MigrationStateOwnerErrorV1::invalid(error.code(), error.to_string()))?;
    self.publish_progress_body_with_authority_expectation(
      context,
      &progress,
      body,
      MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD,
      true,
      MutableSystemControlAuthorityExpectationV1 {
        selected_header_sequence: closure.destination_header_sequence,
        head_hash: &closure.destination_namespace_root,
      },
      retirement_owner,
    )
  }

  pub fn observe_capture_state(
    &self,
    updated_at_ms: i64,
    publication_timestamp_ms: u64,
    monotonic_now_ms: u64,
  ) -> Result<MigrationCaptureStateObservationV1, MigrationStateOwnerErrorV1> {
    let (_, progress) = self.prepare_progress_publication(updated_at_ms, publication_timestamp_ms, monotonic_now_ms)?;
    Ok(MigrationCaptureStateObservationV1 {
      control_sequence: progress.sequence,
      captured_through_publication_sequence: progress.body.captured_through_publication_sequence,
      reconciled_through_publication_sequence: progress.body.reconciled_through_publication_sequence,
      destination_header_sequence: progress.body.destination_header_sequence,
      checkpoint_artifact: progress.body.checkpoint_artifact,
      needs_full_reconciliation: progress.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE != 0,
      last_error_evidence: progress.body.last_error_evidence,
    })
  }

  pub(crate) fn claim_source_gc_suspension(
    &self,
    suspended_at_ms: i64,
    publication_timestamp_ms: u64,
    monotonic_now_ms: u64,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    let (loaded_lease, loaded_progress, progress) =
      self.prepare_source_gc_suspension_claim(suspended_at_ms, publication_timestamp_ms, monotonic_now_ms)?;
    if progress.body.flags & MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED != 0 {
      return Ok(MigrationProgressTransitionReceiptV1 {
        control_sequence: progress.sequence,
        fencing_token: progress.body.fencing_token,
        phase: progress.body.phase,
        state: progress.body.state,
        idempotent: true,
      });
    }
    if progress.body.phase != MigrationPhaseV1::Preflight
      || !matches!(progress.body.state, MigrationProgressStateV1::Pending | MigrationProgressStateV1::Running)
    {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_source_gc_phase",
        "source GC suspension must be established during active preflight before copy begins",
      ));
    }
    let mut body = progress.body.clone();
    body.flags |= MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED;
    body.updated_at_ms = body.updated_at_ms.max(suspended_at_ms);
    self.publish_progress_body(
      MigrationProgressPublicationContextV1 { loaded_lease, loaded_progress, publication_timestamp_ms, monotonic_now_ms },
      &progress,
      body,
      MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
      false,
      retirement_owner,
    )
  }

  pub(crate) fn validate_source_gc_suspension_claim(
    &self,
    suspended_at_ms: i64,
    publication_timestamp_ms: u64,
    monotonic_now_ms: u64,
  ) -> Result<(), MigrationStateOwnerErrorV1> {
    self.prepare_source_gc_suspension_claim(suspended_at_ms, publication_timestamp_ms, monotonic_now_ms).map(|_| ())
  }

  fn prepare_source_gc_suspension_claim(
    &self,
    suspended_at_ms: i64,
    publication_timestamp_ms: u64,
    monotonic_now_ms: u64,
  ) -> Result<(LoadedMutableSystemControlV1, LoadedMutableSystemControlV1, MigrationProgressControlV1), MigrationStateOwnerErrorV1> {
    let (context, progress) = self.prepare_progress_publication(suspended_at_ms, publication_timestamp_ms, monotonic_now_ms)?;
    if progress.body.flags & MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED == 0
      && (progress.body.phase != MigrationPhaseV1::Preflight
        || !matches!(progress.body.state, MigrationProgressStateV1::Pending | MigrationProgressStateV1::Running))
    {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_source_gc_phase",
        "source GC suspension must be established during active preflight before copy begins",
      ));
    }
    Ok((context.loaded_lease, context.loaded_progress, progress))
  }

  fn prepare_progress_publication(
    &self,
    updated_at_ms: i64,
    publication_timestamp_ms: u64,
    monotonic_now_ms: u64,
  ) -> Result<(MigrationProgressPublicationContextV1, MigrationProgressControlV1), MigrationStateOwnerErrorV1> {
    if updated_at_ms < 0 {
      return Err(MigrationStateOwnerErrorV1::invalid("migration_progress_time", "migration progress update time must be nonnegative"));
    }
    let publication_time_ms = validate_publication_clock(publication_timestamp_ms, monotonic_now_ms, i64::MAX as u64)?;
    validate_transition_publication_time(publication_time_ms, updated_at_ms)?;
    validate_destination_authority(&self.publisher, &self.permit)?;
    let ((loaded_lease, lease), (loaded_progress, progress)) = require_migration_controls(&self.publisher, &self.permit)?;
    validate_owned_held_controls(self, &lease, &progress, publication_time_ms)?;
    Ok((MigrationProgressPublicationContextV1 { loaded_lease, loaded_progress, publication_timestamp_ms, monotonic_now_ms }, progress))
  }

  fn publish_progress_body(
    &self,
    context: MigrationProgressPublicationContextV1,
    progress: &MigrationProgressControlV1,
    body: MigrationProgressBodyV1,
    authorized_new_flags: u32,
    allow_same_state_metadata_update: bool,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    self.publish_progress_body_inner(
      context,
      progress,
      body,
      authorized_new_flags,
      allow_same_state_metadata_update,
      None,
      retirement_owner,
    )
  }

  #[allow(clippy::too_many_arguments)]
  fn publish_progress_body_with_authority_expectation(
    &self,
    context: MigrationProgressPublicationContextV1,
    progress: &MigrationProgressControlV1,
    body: MigrationProgressBodyV1,
    authorized_new_flags: u32,
    allow_same_state_metadata_update: bool,
    authority_expectation: MutableSystemControlAuthorityExpectationV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    self.publish_progress_body_inner(
      context,
      progress,
      body,
      authorized_new_flags,
      allow_same_state_metadata_update,
      Some(authority_expectation),
      retirement_owner,
    )
  }

  #[allow(clippy::too_many_arguments)]
  fn publish_progress_body_inner(
    &self,
    context: MigrationProgressPublicationContextV1,
    progress: &MigrationProgressControlV1,
    body: MigrationProgressBodyV1,
    authorized_new_flags: u32,
    allow_same_state_metadata_update: bool,
    authority_expectation: Option<MutableSystemControlAuthorityExpectationV1<'_>>,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationProgressTransitionReceiptV1, MigrationStateOwnerErrorV1> {
    validate_progress_transition(&progress.body, &body, authorized_new_flags, allow_same_state_metadata_update)?;
    let sequence = progress.sequence.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_progress_sequence_exhausted", "migration progress sequence is exhausted")
    })?;
    let encoded = encode_migration_progress_control(sequence, &body, self.permit.hash_algorithm())?;
    let guard_identity = self.permit.migration_id();
    let guards = [MutableSystemControlGuardV1 {
      kind: SystemControlKindV1::MigrationLease,
      identity: &guard_identity,
      expected: control_expectation(&context.loaded_lease),
    }];
    let receipt = publish_owned_control_inner(
      &self.publisher,
      &self.permit,
      SystemControlKindV1::MigrationProgress,
      Some(control_expectation(&context.loaded_progress)),
      &guards,
      &encoded,
      context.publication_timestamp_ms,
      context.monotonic_now_ms,
      retirement_owner,
      MigrationCommittedControlV1::Progress,
      authority_expectation,
    )?;
    Ok(MigrationProgressTransitionReceiptV1 {
      control_sequence: receipt.control_sequence,
      fencing_token: body.fencing_token,
      phase: body.phase,
      state: body.state,
      idempotent: receipt.idempotent,
    })
  }

  pub fn release(
    &self,
    request: MigrationLeaseReleaseRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<MigrationLeaseReleaseReceiptV1, MigrationStateOwnerErrorV1> {
    let publication_time_ms = validate_publication_clock(request.publication_timestamp_ms, request.monotonic_now_ms, i64::MAX as u64 - 1)?;
    let next_publication_timestamp_ms = request
      .publication_timestamp_ms
      .checked_add(1)
      .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_release_time_overflow", "migration release timestamp overflowed"))?;
    let next_monotonic_now_ms = request.monotonic_now_ms.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_release_time_overflow", "migration release monotonic time overflowed")
    })?;
    validate_publication_clock(next_publication_timestamp_ms, next_monotonic_now_ms, i64::MAX as u64)?;
    validate_destination_authority(&self.publisher, &self.permit)?;
    let ((loaded_lease, lease), (loaded_progress, progress)) = require_migration_controls(&self.publisher, &self.permit)?;
    validate_lease_binding(&lease, &self.permit)?;
    validate_bound_progress(&progress, &lease, &self.permit)?;
    validate_owner_fence(self, &lease)?;
    validate_early_release_progress(&progress.body)?;

    if lease.body.state == MigrationLeaseStateV1::Released {
      return Ok(MigrationLeaseReleaseReceiptV1 {
        control_sequence: lease.sequence,
        fencing_token: lease.body.fencing_token,
        resumed_releasing: false,
        idempotent: true,
      });
    }
    if lease.body.state == MigrationLeaseStateV1::Expired {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_lease_expired_state",
        "an explicitly expired migration lease cannot enter release",
      ));
    }
    if lease.body.state == MigrationLeaseStateV1::Held && publication_time_ms >= lease.body.expires_at_ms {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_lease_expired",
        "a held migration lease must be taken over after expiry before it can be released",
      ));
    }

    let required_sequence_steps = if lease.body.state == MigrationLeaseStateV1::Held { 2 } else { 1 };
    lease
      .sequence
      .checked_add(required_sequence_steps)
      .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_sequence_exhausted", "migration lease sequence is exhausted"))?;
    let progress_guard_identity = self.permit.migration_id();
    let progress_guards = [MutableSystemControlGuardV1 {
      kind: SystemControlKindV1::MigrationProgress,
      identity: &progress_guard_identity,
      expected: control_expectation(&loaded_progress),
    }];
    let releasing_sequence = if lease.body.state == MigrationLeaseStateV1::Held { lease.sequence + 1 } else { lease.sequence };
    let encoded_releasing = if lease.body.state == MigrationLeaseStateV1::Held {
      let mut releasing_body = lease.body.clone();
      releasing_body.state = MigrationLeaseStateV1::Releasing;
      Some(encode_migration_lease_control(releasing_sequence, &releasing_body, self.permit.hash_algorithm())?)
    } else {
      None
    };
    let mut released_body = lease.body.clone();
    released_body.state = MigrationLeaseStateV1::Released;
    let released_sequence = releasing_sequence + 1;
    let encoded_released = encode_migration_lease_control(released_sequence, &released_body, self.permit.hash_algorithm())?;

    let (releasing_expectation, resumed_releasing) = if let Some(encoded_releasing) = encoded_releasing.as_ref() {
      let receipt = match publish_control(
        &self.publisher,
        &self.permit,
        SystemControlKindV1::MigrationLease,
        Some(control_expectation(&loaded_lease)),
        &progress_guards,
        encoded_releasing,
        request.publication_timestamp_ms,
        request.monotonic_now_ms,
        retirement_owner,
      ) {
        Ok(receipt) => receipt,
        Err(source) if source.committed_receipt().is_some() => {
          return Err(MigrationStateOwnerErrorV1::ReleaseTransitionCommitted {
            target_state: MigrationLeaseStateV1::Releasing,
            source: Box::new(source),
          });
        }
        Err(source) => return Err(MigrationStateOwnerErrorV1::Publication(source)),
      };
      (control_expectation_from_receipt(receipt), false)
    } else {
      (control_expectation(&loaded_lease), true)
    };

    let (released_publication_timestamp_ms, released_monotonic_now_ms) = if resumed_releasing {
      (request.publication_timestamp_ms, request.monotonic_now_ms)
    } else {
      (next_publication_timestamp_ms, next_monotonic_now_ms)
    };
    let receipt = match publish_control(
      &self.publisher,
      &self.permit,
      SystemControlKindV1::MigrationLease,
      Some(releasing_expectation),
      &progress_guards,
      &encoded_released,
      released_publication_timestamp_ms,
      released_monotonic_now_ms,
      retirement_owner,
    ) {
      Ok(receipt) => receipt,
      Err(source) if source.committed_receipt().is_some() => {
        return Err(MigrationStateOwnerErrorV1::ReleaseTransitionCommitted {
          target_state: MigrationLeaseStateV1::Released,
          source: Box::new(source),
        });
      }
      Err(source) => {
        return Err(MigrationStateOwnerErrorV1::ReleasePartial { releasing_control_sequence: releasing_sequence, source });
      }
    };
    Ok(MigrationLeaseReleaseReceiptV1 {
      control_sequence: receipt.control_sequence,
      fencing_token: lease.body.fencing_token,
      resumed_releasing,
      idempotent: false,
    })
  }

  pub fn takeover(
    publisher: Arc<V4FirstAuthorityPublisher>,
    permit: MigrationPreflightPermitV1,
    request: MigrationTakeoverRequestV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(Self, MigrationTakeoverReceiptV1), MigrationStateOwnerErrorV1> {
    let clocks = validate_takeover_request(request)?;
    validate_destination_authority(&publisher, &permit)?;
    let ((loaded_lease, lease), (loaded_progress, progress)) = require_migration_controls(&publisher, &permit)?;
    validate_lease_binding(&lease, &permit)?;
    validate_progress_policy_binding(&progress, &permit)?;
    if !matches!(lease.body.state, MigrationLeaseStateV1::Held | MigrationLeaseStateV1::Expired) {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_takeover_lease_state",
        format!("migration takeover requires a held or explicitly expired lease, selected state is {:?}", lease.body.state),
      ));
    }
    if progress.body.fencing_token > lease.body.fencing_token {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_progress_token_ahead",
        "migration progress fencing token cannot be ahead of the selected lease",
      ));
    }

    let target_fencing_token = request
      .expected_fencing_token
      .checked_add(1)
      .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_takeover_fencing_exhausted", "migration fencing token is exhausted"))?;
    let target_expires_at_ms = clocks.target_expires_at_ms;

    let mut target_lease_body = lease.body.clone();
    target_lease_body.holder_boot_id = request.new_holder_boot_id;
    target_lease_body.fencing_token = target_fencing_token;
    target_lease_body.acquired_at_ms = request.takeover_at_ms;
    target_lease_body.renewed_at_ms = request.takeover_at_ms;
    target_lease_body.expires_at_ms = target_expires_at_ms;
    target_lease_body.state = MigrationLeaseStateV1::Held;

    let selected_is_target = lease.body.fencing_token == target_fencing_token;
    if lease.body.fencing_token != request.expected_fencing_token && !selected_is_target {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_takeover_fenced",
        "selected migration lease no longer matches the expected takeover token",
      ));
    }
    if selected_is_target && lease.body != target_lease_body {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_takeover_fenced",
        "selected migration lease token belongs to a different takeover request",
      ));
    }
    if !selected_is_target && request.takeover_at_ms < lease.body.expires_at_ms {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_takeover_lease_active",
        "migration takeover cannot replace an unexpired held lease",
      ));
    }
    let target_lease_sequence = if selected_is_target {
      lease.sequence
    } else {
      lease
        .sequence
        .checked_add(1)
        .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_sequence_exhausted", "migration lease sequence is exhausted"))?
    };
    if selected_is_target && progress.body.fencing_token == target_fencing_token {
      let owner = Self { publisher, permit, holder_boot_id: target_lease_body.holder_boot_id, fencing_token: target_fencing_token };
      return Ok((
        owner,
        MigrationTakeoverReceiptV1 {
          lease_control_sequence: lease.sequence,
          progress_control_sequence: progress.sequence,
          fencing_token: target_fencing_token,
          resumed_rebind: false,
          idempotent: true,
        },
      ));
    }
    let target_progress_sequence = progress.sequence.checked_add(1).ok_or_else(|| {
      MigrationStateOwnerErrorV1::invalid("migration_progress_sequence_exhausted", "migration progress sequence is exhausted")
    })?;
    let encoded_target_lease = if selected_is_target {
      None
    } else {
      Some(encode_migration_lease_control(target_lease_sequence, &target_lease_body, permit.hash_algorithm())?)
    };
    let mut target_progress_body = progress.body.clone();
    target_progress_body.fencing_token = target_fencing_token;
    target_progress_body.updated_at_ms = target_progress_body.updated_at_ms.max(request.takeover_at_ms);
    let encoded_target_progress =
      encode_migration_progress_control(target_progress_sequence, &target_progress_body, permit.hash_algorithm())?;

    let lease_expectation = if let Some(encoded_target_lease) = encoded_target_lease.as_ref() {
      let progress_guard_identity = permit.migration_id();
      let progress_guards = [MutableSystemControlGuardV1 {
        kind: SystemControlKindV1::MigrationProgress,
        identity: &progress_guard_identity,
        expected: control_expectation(&loaded_progress),
      }];
      let receipt = match publish_control(
        &publisher,
        &permit,
        SystemControlKindV1::MigrationLease,
        Some(control_expectation(&loaded_lease)),
        &progress_guards,
        encoded_target_lease,
        request.publication_timestamp_ms,
        request.monotonic_now_ms,
        retirement_owner,
      ) {
        Ok(receipt) => receipt,
        Err(source) if source.committed_receipt().is_some() => {
          return Err(MigrationStateOwnerErrorV1::TakeoverLeaseCommitted { fencing_token: target_fencing_token, source: Box::new(source) });
        }
        Err(source) if mutable_control_was_fenced(&source) => {
          return Err(MigrationStateOwnerErrorV1::invalid(
            "migration_takeover_fenced",
            "migration takeover lost its exact lease/progress compare-and-swap",
          ));
        }
        Err(source) => return Err(MigrationStateOwnerErrorV1::Publication(source)),
      };
      control_expectation_from_receipt(receipt)
    } else {
      control_expectation(&loaded_lease)
    };

    let lease_guard_identity = permit.migration_id();
    let lease_guards = [MutableSystemControlGuardV1 {
      kind: SystemControlKindV1::MigrationLease,
      identity: &lease_guard_identity,
      expected: lease_expectation,
    }];
    let receipt = match publish_control(
      &publisher,
      &permit,
      SystemControlKindV1::MigrationProgress,
      Some(control_expectation(&loaded_progress)),
      &lease_guards,
      &encoded_target_progress,
      clocks.progress_publication_timestamp_ms,
      clocks.progress_monotonic_now_ms,
      retirement_owner,
    ) {
      Ok(receipt) => receipt,
      Err(source) if source.committed_receipt().is_some() => {
        return Err(MigrationStateOwnerErrorV1::TakeoverCommitted {
          lease_control_sequence: target_lease_sequence,
          fencing_token: target_fencing_token,
          source: Box::new(source),
        });
      }
      Err(source) if mutable_control_was_fenced(&source) => {
        return Err(MigrationStateOwnerErrorV1::invalid(
          "migration_takeover_fenced",
          "migration takeover lost its exact progress/lease compare-and-swap",
        ));
      }
      Err(source) => {
        return Err(MigrationStateOwnerErrorV1::TakeoverPartial {
          lease_control_sequence: target_lease_sequence,
          fencing_token: target_fencing_token,
          source,
        });
      }
    };
    let owner = Self { publisher, permit, holder_boot_id: target_lease_body.holder_boot_id, fencing_token: target_fencing_token };
    Ok((
      owner,
      MigrationTakeoverReceiptV1 {
        lease_control_sequence: target_lease_sequence,
        progress_control_sequence: receipt.control_sequence,
        fencing_token: target_fencing_token,
        resumed_rebind: selected_is_target,
        idempotent: false,
      },
    ))
  }

  pub const fn fencing_token(&self) -> u64 {
    self.fencing_token
  }

  pub const fn holder_boot_id(&self) -> [u8; 16] {
    self.holder_boot_id
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.permit.database_id()
  }

  pub const fn source_physical_instance_id(&self) -> [u8; 16] {
    self.permit.source_physical_instance_id()
  }

  pub const fn destination_physical_instance_id(&self) -> [u8; 16] {
    self.permit.destination_physical_instance_id()
  }

  pub const fn source_file_identity(&self) -> crate::engine::native_durability::PlatformFileIdentityDescriptorV1 {
    self.permit.source_file_identity()
  }

  pub const fn hash_algorithm(&self) -> crate::engine::HashAlgorithm {
    self.permit.hash_algorithm()
  }

  pub const fn migration_id(&self) -> [u8; 16] {
    self.permit.migration_id()
  }

  pub const fn preflight_evidence_fingerprint(&self) -> [u8; 32] {
    self.permit.evidence_fingerprint()
  }

  pub fn effective_configuration_fingerprint(&self) -> &[u8] {
    self.permit.effective_configuration_fingerprint()
  }

  pub fn system_family_registry_fingerprint(&self) -> &[u8] {
    self.permit.system_family_registry_fingerprint()
  }

  pub const fn source_authority_digest(&self) -> [u8; 32] {
    self.permit.source_authority_digest()
  }

  pub fn destination_observation(&self) -> Result<DatabaseHeaderObservationV4, MigrationStateOwnerErrorV1> {
    self.publisher.observe().map_err(Into::into)
  }
}

#[derive(Debug)]
pub enum MigrationStateOwnerErrorV1 {
  Invalid { code: &'static str, message: String },
  LeaseCommitted { source: Box<MutableSystemControlPublicationErrorV1> },
  PartialAcquisition { lease_control_sequence: u64, source: Box<MutableSystemControlPublicationErrorV1> },
  AcquisitionCommitted { lease_control_sequence: u64, source: Box<MutableSystemControlPublicationErrorV1> },
  LeaseTransitionCommitted { source: Box<MutableSystemControlPublicationErrorV1> },
  ProgressTransitionCommitted { source: Box<MutableSystemControlPublicationErrorV1> },
  ReleaseTransitionCommitted { target_state: MigrationLeaseStateV1, source: Box<MutableSystemControlPublicationErrorV1> },
  ReleasePartial { releasing_control_sequence: u64, source: MutableSystemControlPublicationErrorV1 },
  TakeoverLeaseCommitted { fencing_token: u64, source: Box<MutableSystemControlPublicationErrorV1> },
  TakeoverPartial { lease_control_sequence: u64, fencing_token: u64, source: MutableSystemControlPublicationErrorV1 },
  TakeoverCommitted { lease_control_sequence: u64, fencing_token: u64, source: Box<MutableSystemControlPublicationErrorV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  Publication(MutableSystemControlPublicationErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl MigrationStateOwnerErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::LeaseCommitted { .. } => "migration_lease_committed",
      Self::PartialAcquisition { .. } => "migration_acquisition_partial",
      Self::AcquisitionCommitted { .. } => "migration_acquisition_committed",
      Self::LeaseTransitionCommitted { .. } => "migration_lease_transition_committed",
      Self::ProgressTransitionCommitted { .. } => "migration_progress_transition_committed",
      Self::ReleaseTransitionCommitted { .. } => "migration_release_transition_committed",
      Self::ReleasePartial { .. } => "migration_release_partial",
      Self::TakeoverLeaseCommitted { .. } => "migration_takeover_lease_committed",
      Self::TakeoverPartial { .. } => "migration_takeover_partial",
      Self::TakeoverCommitted { .. } => "migration_takeover_committed",
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Publication(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationStateOwnerErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    let code = self.code();
    match self {
      Self::Invalid { message, .. } => write!(formatter, "{code}: {message}"),
      Self::LeaseCommitted { source } => write!(formatter, "{code}: lease committed but acquisition did not continue: {source}"),
      Self::PartialAcquisition { lease_control_sequence, source } => {
        write!(formatter, "{code}: lease control {lease_control_sequence} is durable but initial progress did not commit: {source}")
      }
      Self::AcquisitionCommitted { lease_control_sequence, source } => {
        write!(formatter, "{code}: lease control {lease_control_sequence} and progress committed but post-commit handling failed: {source}")
      }
      Self::LeaseTransitionCommitted { source } => {
        write!(formatter, "{code}: migration lease transition committed but post-commit handling failed: {source}")
      }
      Self::ProgressTransitionCommitted { source } => {
        write!(formatter, "{code}: migration progress transition committed but post-commit handling failed: {source}")
      }
      Self::ReleaseTransitionCommitted { target_state, source } => {
        write!(formatter, "{code}: migration release transition to {target_state:?} committed but post-commit handling failed: {source}")
      }
      Self::ReleasePartial { releasing_control_sequence, source } => {
        write!(formatter, "{code}: migration lease is durably releasing at control {releasing_control_sequence}: {source}")
      }
      Self::TakeoverLeaseCommitted { fencing_token, source } => {
        write!(formatter, "{code}: migration takeover lease token {fencing_token} committed but post-commit handling failed: {source}")
      }
      Self::TakeoverPartial { lease_control_sequence, fencing_token, source } => {
        write!(formatter, "{code}: migration takeover lease {lease_control_sequence} token {fencing_token} is durable but progress is not rebound: {source}")
      }
      Self::TakeoverCommitted { lease_control_sequence, fencing_token, source } => {
        write!(formatter, "{code}: migration takeover lease {lease_control_sequence} and progress token {fencing_token} committed but post-commit handling failed: {source}")
      }
      Self::Format(source) => write!(formatter, "{code}: migration control format error: {source}"),
      Self::Authority(source) => write!(formatter, "{code}: migration authority error: {source}"),
      Self::Publication(source) => write!(formatter, "{code}: migration control publication error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "{code}: migration retirement owner error: {source}"),
    }
  }
}

impl Error for MigrationStateOwnerErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::LeaseCommitted { source }
      | Self::PartialAcquisition { source, .. }
      | Self::AcquisitionCommitted { source, .. }
      | Self::LeaseTransitionCommitted { source }
      | Self::ProgressTransitionCommitted { source }
      | Self::ReleaseTransitionCommitted { source, .. }
      | Self::TakeoverLeaseCommitted { source, .. }
      | Self::TakeoverCommitted { source, .. } => Some(source.as_ref()),
      Self::ReleasePartial { source, .. } | Self::TakeoverPartial { source, .. } => Some(source),
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::Publication(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<FormatError> for MigrationStateOwnerErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for MigrationStateOwnerErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for MigrationStateOwnerErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
}

fn validate_acquisition_request(request: MigrationAcquisitionRequestV1) -> Result<(), MigrationStateOwnerErrorV1> {
  if request.holder_boot_id.iter().all(|byte| *byte == 0) {
    return Err(MigrationStateOwnerErrorV1::invalid("migration_holder_boot_identity", "migration holder boot identity must be nonzero"));
  }
  if request.acquired_at_ms < 0 || request.lease_duration_ms <= 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_times",
      "migration acquisition time must be nonnegative and lease duration must be positive",
    ));
  }
  request
    .acquired_at_ms
    .checked_add(request.lease_duration_ms)
    .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_time_overflow", "migration lease expiry overflowed"))?;
  let publication_time_ms = validate_publication_clock(request.publication_timestamp_ms, request.monotonic_now_ms, i64::MAX as u64 - 1)?;
  validate_transition_publication_time(publication_time_ms, request.acquired_at_ms)?;
  request.publication_timestamp_ms.checked_add(1).ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress publication timestamp overflowed")
  })?;
  request.monotonic_now_ms.checked_add(1).ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_progress_time_overflow", "migration progress monotonic timestamp overflowed")
  })?;
  Ok(())
}

fn validate_destination_authority(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let observation = publisher.observe()?;
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_destination_authority",
      "migration acquisition requires initialized non-degraded destination first authority",
    ));
  }
  if header.database_id != permit.database_id()
    || header.physical_instance_id != permit.destination_physical_instance_id()
    || header.hash_algorithm != permit.hash_algorithm()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_destination_identity",
      "preflight permit and destination database authority disagree",
    ));
  }
  if header.system_family_registry_fingerprint != permit.system_family_registry_fingerprint() {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_destination_system_family",
      "preflight permit and destination SystemFamily registry disagree",
    ));
  }
  Ok(())
}

fn validate_held_lease(
  lease: &MigrationLeaseControlV1,
  permit: &MigrationPreflightPermitV1,
  request: MigrationAcquisitionRequestV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  validate_lease_binding(lease, permit)?;
  let body = &lease.body;
  if body.state != MigrationLeaseStateV1::Held {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_not_held",
      format!("selected migration lease is {:?}, not held", body.state),
    ));
  }
  if body.holder_boot_id != request.holder_boot_id {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_held_by_other_boot",
      "selected migration lease belongs to another boot identity",
    ));
  }
  if request.acquired_at_ms >= body.expires_at_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_expired",
      "selected migration lease has expired and requires explicit takeover recovery",
    ));
  }
  Ok(())
}

fn validate_lease_binding(lease: &MigrationLeaseControlV1, permit: &MigrationPreflightPermitV1) -> Result<(), MigrationStateOwnerErrorV1> {
  let body = &lease.body;
  if body.database_id != permit.database_id()
    || body.migration_id != permit.migration_id()
    || body.source_physical_instance_id != permit.source_physical_instance_id()
    || body.destination_physical_instance_id != permit.destination_physical_instance_id()
    || body.source_header_sequence != permit.source_header_sequence()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_binding",
      "selected migration lease does not match the exact preflight permit",
    ));
  }
  Ok(())
}

fn validate_bound_progress(
  progress: &MigrationProgressControlV1,
  lease: &MigrationLeaseControlV1,
  permit: &MigrationPreflightPermitV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  validate_progress_policy_binding(progress, permit)?;
  if progress.body.fencing_token < lease.body.fencing_token {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_rebind_required",
      "selected migration lease token is ahead of progress and requires explicit takeover rebind",
    ));
  }
  if progress.body.fencing_token > lease.body.fencing_token {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_token_ahead",
      "selected migration progress token is ahead of the lease",
    ));
  }
  Ok(())
}

fn validate_progress_policy_binding(
  progress: &MigrationProgressControlV1,
  permit: &MigrationPreflightPermitV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let body = &progress.body;
  if body.database_id != permit.database_id()
    || body.migration_id != permit.migration_id()
    || body.source_physical_instance_id != permit.source_physical_instance_id()
    || body.destination_physical_instance_id != permit.destination_physical_instance_id()
    || body.source_header_sequence != permit.source_header_sequence()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_binding",
      "selected migration progress does not match the exact preflight permit",
    ));
  }
  if body.source_capture_head != permit.source_capture_head()
    || body.effective_config_fingerprint != permit.effective_configuration_fingerprint()
    || body.system_family_registry_fingerprint != permit.system_family_registry_fingerprint()
  {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_policy_binding",
      "selected migration progress does not match preflight source, configuration, or SystemFamily evidence",
    ));
  }
  Ok(())
}

fn validate_owned_held_controls(
  owner: &MigrationStateOwnerV1,
  lease: &MigrationLeaseControlV1,
  progress: &MigrationProgressControlV1,
  now_ms: i64,
) -> Result<(), MigrationStateOwnerErrorV1> {
  validate_lease_binding(lease, &owner.permit)?;
  validate_bound_progress(progress, lease, &owner.permit)?;
  if lease.body.state != MigrationLeaseStateV1::Held {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_not_held",
      format!("selected migration lease is {:?}, not held", lease.body.state),
    ));
  }
  validate_owner_fence(owner, lease)?;
  if now_ms >= lease.body.expires_at_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_lease_expired",
      "selected migration lease has expired and requires explicit takeover recovery",
    ));
  }
  Ok(())
}

fn validate_owner_fence(owner: &MigrationStateOwnerV1, lease: &MigrationLeaseControlV1) -> Result<(), MigrationStateOwnerErrorV1> {
  if lease.body.holder_boot_id != owner.holder_boot_id || lease.body.fencing_token != owner.fencing_token {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_owner_fenced",
      "selected migration lease no longer belongs to this holder and fencing token",
    ));
  }
  Ok(())
}

fn validate_early_release_progress(progress: &MigrationProgressBodyV1) -> Result<(), MigrationStateOwnerErrorV1> {
  if !matches!(progress.state, MigrationProgressStateV1::Failed | MigrationProgressStateV1::Canceled) {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_release_progress_not_terminal",
      "early migration release requires failed or canceled progress",
    ));
  }
  if progress.phase as u16 >= MigrationPhaseV1::FinalFreeze as u16 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_release_after_final_freeze",
      "migration release at or after final freeze requires later verified rollback or cutover evidence",
    ));
  }
  Ok(())
}

#[derive(Clone, Copy)]
struct MigrationTakeoverClocksV1 {
  progress_publication_timestamp_ms: u64,
  progress_monotonic_now_ms: u64,
  target_expires_at_ms: i64,
}

fn validate_takeover_request(request: MigrationTakeoverRequestV1) -> Result<MigrationTakeoverClocksV1, MigrationStateOwnerErrorV1> {
  if request.new_holder_boot_id.iter().all(|byte| *byte == 0) {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_holder_boot_identity",
      "migration takeover holder boot identity must be nonzero",
    ));
  }
  if request.expected_fencing_token == 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_takeover_fencing",
      "migration takeover expected fencing token must be nonzero",
    ));
  }
  request
    .expected_fencing_token
    .checked_add(1)
    .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_takeover_fencing_exhausted", "migration fencing token is exhausted"))?;
  if request.takeover_at_ms < 0 || request.lease_duration_ms <= 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_takeover_times",
      "migration takeover time must be nonnegative and lease duration must be positive",
    ));
  }
  let target_expires_at_ms = request
    .takeover_at_ms
    .checked_add(request.lease_duration_ms)
    .ok_or_else(|| MigrationStateOwnerErrorV1::invalid("migration_lease_time_overflow", "migration takeover expiry overflowed"))?;
  let lease_publication_time_ms =
    validate_publication_clock(request.publication_timestamp_ms, request.monotonic_now_ms, i64::MAX as u64 - 1)?;
  validate_transition_publication_time(lease_publication_time_ms, request.takeover_at_ms)?;
  let progress_publication_timestamp_ms = request.publication_timestamp_ms.checked_add(1).ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_takeover_time_overflow", "migration takeover progress timestamp overflowed")
  })?;
  let progress_monotonic_now_ms = request.monotonic_now_ms.checked_add(1).ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_takeover_time_overflow", "migration takeover progress monotonic time overflowed")
  })?;
  let progress_publication_time_ms =
    validate_publication_clock(progress_publication_timestamp_ms, progress_monotonic_now_ms, i64::MAX as u64)?;
  validate_transition_publication_time(progress_publication_time_ms, request.takeover_at_ms)?;
  if lease_publication_time_ms >= target_expires_at_ms || progress_publication_time_ms >= target_expires_at_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_takeover_expired_target",
      "migration takeover cannot publish an already expired replacement lease",
    ));
  }
  Ok(MigrationTakeoverClocksV1 { progress_publication_timestamp_ms, progress_monotonic_now_ms, target_expires_at_ms })
}

fn validate_publication_clock(
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
  maximum_publication_timestamp_ms: u64,
) -> Result<i64, MigrationStateOwnerErrorV1> {
  if publication_timestamp_ms == 0 || monotonic_now_ms == 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_publication_times",
      "migration publication and monotonic timestamps must be nonzero",
    ));
  }
  if publication_timestamp_ms > maximum_publication_timestamp_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_publication_time_range",
      "migration publication timestamp exceeds the durable FileRecord time range",
    ));
  }
  i64::try_from(publication_timestamp_ms).map_err(|error| {
    MigrationStateOwnerErrorV1::invalid(
      "migration_publication_time_range",
      format!("migration publication timestamp exceeds the durable FileRecord time range: {error}"),
    )
  })
}

fn validate_transition_publication_time(publication_time_ms: i64, transition_time_ms: i64) -> Result<(), MigrationStateOwnerErrorV1> {
  if publication_time_ms < transition_time_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_publication_before_transition",
      "migration publication timestamp cannot precede its semantic transition time",
    ));
  }
  Ok(())
}

fn validate_progress_transition(
  current: &MigrationProgressBodyV1,
  target: &MigrationProgressBodyV1,
  authorized_new_flags: u32,
  allow_same_state_metadata_update: bool,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let current_phase = current.phase as u16;
  let target_phase = target.phase as u16;
  if target_phase < current_phase || target_phase > current_phase + 1 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_phase_sequence",
      "migration progress phase must remain current or advance exactly one phase",
    ));
  }
  if target_phase == current_phase {
    if !(allow_same_state_metadata_update && target.state == current.state) {
      validate_same_phase_state_transition(current.state, target.state)?;
    }
  } else if current.state != MigrationProgressStateV1::Complete || target.state != MigrationProgressStateV1::Pending {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_phase_boundary",
      "the current phase must complete before the next phase enters pending state",
    ));
  }
  if target.flags & current.flags != current.flags {
    return Err(MigrationStateOwnerErrorV1::invalid("migration_progress_flag_regression", "migration progress flags cannot be cleared"));
  }
  let new_flags = target.flags & !current.flags;
  if new_flags & !authorized_new_flags != 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_flag_authority",
      "this transition owner cannot claim a new external migration condition",
    ));
  }
  for (target_value, current_value, name) in [
    (target.destination_header_sequence, current.destination_header_sequence, "destination header sequence"),
    (target.copied_through_write_sequence, current.copied_through_write_sequence, "copied write sequence"),
    (target.captured_through_publication_sequence, current.captured_through_publication_sequence, "captured publication sequence"),
    (target.reconciled_through_publication_sequence, current.reconciled_through_publication_sequence, "reconciled publication sequence"),
    (target.namespace_count, current.namespace_count, "namespace count"),
    (target.entity_count, current.entity_count, "entity count"),
    (target.copied_bytes, current.copied_bytes, "copied bytes"),
  ] {
    if target_value < current_value {
      return Err(MigrationStateOwnerErrorV1::invalid("migration_progress_scalar_regression", format!("migration {name} cannot decrease")));
    }
  }
  if target.updated_at_ms < current.updated_at_ms {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_time_regression",
      "migration progress update time cannot decrease",
    ));
  }
  for (target_hash, current_hash, name) in [
    (&target.checkpoint_artifact, &current.checkpoint_artifact, "checkpoint artifact"),
    (&target.legacy_root_map_control_payload_hash, &current.legacy_root_map_control_payload_hash, "legacy root-map control payload hash"),
    (&target.last_error_evidence, &current.last_error_evidence, "last error evidence"),
  ] {
    if !all_zero(current_hash) && all_zero(target_hash) {
      return Err(MigrationStateOwnerErrorV1::invalid(
        "migration_progress_evidence_regression",
        format!("migration {name} cannot be cleared once established"),
      ));
    }
  }
  if target_phase >= MigrationPhaseV1::Copy as u16 && target.flags & MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED == 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_gc_suspension_required",
      "copy and later phases require durable source GC suspension",
    ));
  }
  if target_phase > MigrationPhaseV1::FinalFreeze as u16 && target.flags & MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD == 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_write_freeze_required",
      "destination verification and later phases require the source write freeze",
    ));
  }
  if target_phase > MigrationPhaseV1::DestinationVerify as u16 && target.flags & MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED == 0 {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_destination_verification_required",
      "cutover and later phases require full destination verification",
    ));
  }
  if target.state == MigrationProgressStateV1::Failed && all_zero(&target.last_error_evidence) {
    return Err(MigrationStateOwnerErrorV1::invalid(
      "migration_progress_failure_evidence",
      "failed migration progress requires durable error evidence",
    ));
  }
  Ok(())
}

fn progress_receipt(progress: &MigrationProgressControlV1, idempotent: bool) -> MigrationProgressTransitionReceiptV1 {
  MigrationProgressTransitionReceiptV1 {
    control_sequence: progress.sequence,
    fencing_token: progress.body.fencing_token,
    phase: progress.body.phase,
    state: progress.body.state,
    idempotent,
  }
}

fn validate_same_phase_state_transition(
  current: MigrationProgressStateV1,
  target: MigrationProgressStateV1,
) -> Result<(), MigrationStateOwnerErrorV1> {
  let allowed = match current {
    MigrationProgressStateV1::Pending => matches!(
      target,
      MigrationProgressStateV1::Pending
        | MigrationProgressStateV1::Running
        | MigrationProgressStateV1::Failed
        | MigrationProgressStateV1::Canceled
    ),
    MigrationProgressStateV1::Running => matches!(
      target,
      MigrationProgressStateV1::Running
        | MigrationProgressStateV1::Paused
        | MigrationProgressStateV1::Complete
        | MigrationProgressStateV1::Failed
        | MigrationProgressStateV1::Canceled
    ),
    MigrationProgressStateV1::Paused => matches!(
      target,
      MigrationProgressStateV1::Paused
        | MigrationProgressStateV1::Running
        | MigrationProgressStateV1::Failed
        | MigrationProgressStateV1::Canceled
    ),
    MigrationProgressStateV1::Complete | MigrationProgressStateV1::Failed | MigrationProgressStateV1::Canceled => false,
  };
  if !allowed {
    let code = if matches!(current, MigrationProgressStateV1::Failed | MigrationProgressStateV1::Canceled) {
      "migration_progress_terminal"
    } else {
      "migration_progress_state_transition"
    };
    return Err(MigrationStateOwnerErrorV1::invalid(
      code,
      format!("migration progress cannot transition from {current:?} to {target:?} within one phase"),
    ));
  }
  Ok(())
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn mutable_control_was_fenced(error: &MutableSystemControlPublicationErrorV1) -> bool {
  matches!(error.code(), "mutable_control_selector_conflict" | "mutable_control_guard_conflict")
}

type LoadedMigrationLeaseV1 = (LoadedMutableSystemControlV1, MigrationLeaseControlV1);
type LoadedMigrationProgressV1 = (LoadedMutableSystemControlV1, MigrationProgressControlV1);

fn load_migration_controls(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
) -> Result<(Option<LoadedMigrationLeaseV1>, Option<LoadedMigrationProgressV1>), MigrationStateOwnerErrorV1> {
  let identity = permit.migration_id();
  let (lease, progress) = publisher.load_mutable_system_control_selected_pair(
    SystemControlKindV1::MigrationLease,
    &identity,
    SystemControlKindV1::MigrationProgress,
    &identity,
    &permit.database_id(),
  )?;
  let lease = match lease {
    Some(loaded) => {
      let decoded = decode_migration_lease_control(&loaded.bytes, permit.hash_algorithm())?;
      Some((loaded, decoded))
    }
    None => None,
  };
  let progress = match progress {
    Some(loaded) => {
      let decoded = decode_migration_progress_control(&loaded.bytes, permit.hash_algorithm())?;
      Some((loaded, decoded))
    }
    None => None,
  };
  Ok((lease, progress))
}

fn require_migration_controls(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
) -> Result<(LoadedMigrationLeaseV1, LoadedMigrationProgressV1), MigrationStateOwnerErrorV1> {
  let (lease, progress) = load_migration_controls(publisher, permit)?;
  let lease = lease.ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_lease_missing", "selected migration lease is required for this operation")
  })?;
  let progress = progress.ok_or_else(|| {
    MigrationStateOwnerErrorV1::invalid("migration_progress_missing", "selected migration progress is required for this operation")
  })?;
  Ok((lease, progress))
}

fn control_expectation(loaded: &LoadedMutableSystemControlV1) -> MutableSystemControlExpectationV1 {
  MutableSystemControlExpectationV1 {
    selected_slot: loaded.selected_slot,
    control_sequence: loaded.control_sequence,
    control_digest: loaded.control_digest.clone(),
  }
}

fn control_expectation_from_receipt(receipt: MutableSystemControlPublicationReceiptV1) -> MutableSystemControlExpectationV1 {
  MutableSystemControlExpectationV1 {
    selected_slot: receipt.selected_slot,
    control_sequence: receipt.control_sequence,
    control_digest: receipt.control_digest,
  }
}

#[derive(Clone, Copy)]
enum MigrationCommittedControlV1 {
  Lease,
  Progress,
}

fn publish_owned_control(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
  kind: SystemControlKindV1,
  expected: Option<MutableSystemControlExpectationV1>,
  guards: &[MutableSystemControlGuardV1<'_>],
  encoded_control: &[u8],
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
  retirement_owner: &mut RetirementJournalOwnerV1,
  committed_control: MigrationCommittedControlV1,
) -> Result<MutableSystemControlPublicationReceiptV1, MigrationStateOwnerErrorV1> {
  publish_owned_control_inner(
    publisher,
    permit,
    kind,
    expected,
    guards,
    encoded_control,
    publication_timestamp_ms,
    monotonic_now_ms,
    retirement_owner,
    committed_control,
    None,
  )
}

#[allow(clippy::too_many_arguments)]
fn publish_owned_control_inner(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
  kind: SystemControlKindV1,
  expected: Option<MutableSystemControlExpectationV1>,
  guards: &[MutableSystemControlGuardV1<'_>],
  encoded_control: &[u8],
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
  retirement_owner: &mut RetirementJournalOwnerV1,
  committed_control: MigrationCommittedControlV1,
  authority_expectation: Option<MutableSystemControlAuthorityExpectationV1<'_>>,
) -> Result<MutableSystemControlPublicationReceiptV1, MigrationStateOwnerErrorV1> {
  match publish_control_inner(
    publisher,
    permit,
    kind,
    expected,
    guards,
    encoded_control,
    publication_timestamp_ms,
    monotonic_now_ms,
    retirement_owner,
    authority_expectation,
  ) {
    Ok(receipt) => Ok(receipt),
    Err(source) if source.committed_receipt().is_some() => match committed_control {
      MigrationCommittedControlV1::Lease => Err(MigrationStateOwnerErrorV1::LeaseTransitionCommitted { source: Box::new(source) }),
      MigrationCommittedControlV1::Progress => Err(MigrationStateOwnerErrorV1::ProgressTransitionCommitted { source: Box::new(source) }),
    },
    Err(source) => Err(MigrationStateOwnerErrorV1::Publication(source)),
  }
}

fn publish_control(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
  kind: SystemControlKindV1,
  expected: Option<MutableSystemControlExpectationV1>,
  guards: &[MutableSystemControlGuardV1<'_>],
  encoded_control: &[u8],
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
  retirement_owner: &mut RetirementJournalOwnerV1,
) -> Result<MutableSystemControlPublicationReceiptV1, MutableSystemControlPublicationErrorV1> {
  publish_control_inner(
    publisher,
    permit,
    kind,
    expected,
    guards,
    encoded_control,
    publication_timestamp_ms,
    monotonic_now_ms,
    retirement_owner,
    None,
  )
}

#[allow(clippy::too_many_arguments)]
fn publish_control_inner(
  publisher: &V4FirstAuthorityPublisher,
  permit: &MigrationPreflightPermitV1,
  kind: SystemControlKindV1,
  expected: Option<MutableSystemControlExpectationV1>,
  guards: &[MutableSystemControlGuardV1<'_>],
  encoded_control: &[u8],
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
  retirement_owner: &mut RetirementJournalOwnerV1,
  authority_expectation: Option<MutableSystemControlAuthorityExpectationV1<'_>>,
) -> Result<MutableSystemControlPublicationReceiptV1, MutableSystemControlPublicationErrorV1> {
  let request = MutableSystemControlPublicationRequestV1 {
    database_id: &permit.database_id(),
    kind,
    identity: &permit.migration_id(),
    expected,
    guards,
    encoded_control,
    publication_timestamp_ms,
    monotonic_now_ms,
  };
  match authority_expectation {
    Some(expectation) => publisher.publish_mutable_system_control_with_authority_expectation(request, expectation, retirement_owner),
    None => publisher.publish_mutable_system_control(request, retirement_owner),
  }
}
