//! Receipt publication after guarded sweep removal or restart reconciliation.
//!
//! A receipt is immutable evidence, not allocator authority. The caller-owned
//! Void authority must prove that the exact catalog is selected and allocator
//! admission remains blocked until this boundary returns an exact receipt.

use std::fmt::{self, Debug, Formatter};
use std::array::TryFromSliceError;
use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::first_authority::FirstAuthorityPublicationErrorV1;
use super::gc::{EncodedImmutableGcArtifactV1, GcArtifactKindV1, checked_immutable_gc_artifact_encoded_length};
use super::gc_sweep_removal::{
  SweepLocatorRemovalCompletionPermitV1, SweepLocatorRemovalErrorV1, SweepLocatorRemovalOutcomeV1,
  validate_sweep_locator_removal_outcomes_v1,
};
use super::gc_void::{SweepProposalV1, SweepReceiptOutcomeWriteV1, SweepReceiptV1, SweepReceiptWriteV1, encode_sweep_receipt_v1};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const SWEEP_RECEIPT_RECONCILIATION_BASE_BYTES_V1: u64 = 512;

#[derive(Clone, Copy, Debug)]
pub struct SweepReceiptRecoveryIdentityV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub proposal_hash: &'a [u8],
  pub proposal_write_sequence: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum SweepReceiptReconciliationSourceV1<'a> {
  Completion(&'a SweepLocatorRemovalCompletionPermitV1),
  Recovery(SweepReceiptRecoveryIdentityV1<'a>),
}

#[derive(Clone, Copy, Debug)]
pub struct SweepReceiptVoidAuthorityRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub batch_id: &'a [u8; 16],
  pub generation: u64,
  pub proposal_hash: &'a [u8],
  pub proposal_write_sequence: u64,
  pub proposal: &'a SweepProposalV1<'a>,
  pub recovery: bool,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingSweepReceiptAuthorityV1 {
  pub receipt_hash: Vec<u8>,
  pub receipt_write_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepReceiptVoidAuthoritySnapshotV1 {
  pub selected_void_catalog_hash: Vec<u8>,
  pub selected_void_catalog_generation: u64,
  pub reclaim_committed_at_ms: i64,
  pub selected_void_catalog_current: bool,
  pub proposal_catalog_closure_complete: bool,
  pub reclaimed_extents_exact: bool,
  pub nonreclaimed_extents_absent: bool,
  pub locator_removals_durable: bool,
  pub replacement_lineage_complete: bool,
  pub memory_coordinator_current: bool,
  pub allocator_admission_blocked: bool,
  pub receipt_search_complete: bool,
  pub conflicting_receipt_count: u32,
  pub existing_receipt: Option<ExistingSweepReceiptAuthorityV1>,
  pub repair_latch_clear: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct SweepReceiptVoidAuthorityErrorV1 {
  code: String,
  message: String,
}

impl SweepReceiptVoidAuthorityErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "sweep_receipt_void_authority"
    } else {
      self.code.as_str()
    }
  }
}

pub trait SweepReceiptVoidAuthorityV1 {
  /// Prove the exact selected catalog and receipt-search state while the
  /// caller holds first authority. Implementations must not reenter that
  /// guard or permit allocator admission during an unreceipted prefix.
  fn recheck_sweep_receipt_void_authority(
    &mut self,
    request: SweepReceiptVoidAuthorityRequestV1<'_>,
  ) -> Result<SweepReceiptVoidAuthoritySnapshotV1, SweepReceiptVoidAuthorityErrorV1>;

  /// Reconstruct one complete proposal-ordered outcome set after restart.
  /// This is read-only reconciliation; implementations must return an error
  /// rather than guess when locator/catalog evidence is ambiguous.
  fn recover_sweep_receipt_outcomes(
    &mut self,
    request: SweepReceiptVoidAuthorityRequestV1<'_>,
  ) -> Result<Vec<SweepLocatorRemovalOutcomeV1>, SweepReceiptVoidAuthorityErrorV1>;
}

#[must_use = "a prepared sweep receipt must be hard-published or discarded without granting allocator authority"]
pub(crate) struct PreparedSweepReceiptReconciliationV1 {
  pub(crate) artifact: EncodedImmutableGcArtifactV1,
  pub(crate) recovered: bool,
  pub(crate) void_catalog_hash: Vec<u8>,
  pub(crate) void_catalog_generation: u64,
  pub(crate) reclaim_committed_at_ms: i64,
  _memory: MemoryReservation,
}

impl Debug for PreparedSweepReceiptReconciliationV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("PreparedSweepReceiptReconciliationV1")
      .field("receipt_hash", &hex::encode(&self.artifact.key))
      .field("recovered", &self.recovered)
      .field("void_catalog_hash", &hex::encode(&self.void_catalog_hash))
      .field("void_catalog_generation", &self.void_catalog_generation)
      .field("reclaim_committed_at_ms", &self.reclaim_committed_at_ms)
      .finish()
  }
}

#[derive(Debug, Error)]
pub enum SweepReceiptReconciliationErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("sweep receipt Void-authority recheck failed: {0}")]
  VoidAuthority(#[from] SweepReceiptVoidAuthorityErrorV1),
  #[error("sweep receipt first-authority publication failed: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("sweep receipt outcome validation failed: {0}")]
  Removal(#[from] SweepLocatorRemovalErrorV1),
  #[error("sweep receipt format failure: {0}")]
  Format(#[from] FormatError),
  #[error("sweep receipt memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("sweep receipt integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
  #[error("sweep receipt fixed-width identity conversion failed: {0}")]
  SliceConversion(#[from] TryFromSliceError),
}

impl SweepReceiptReconciliationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::VoidAuthority(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Removal(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Memory(_) => "sweep_receipt_memory",
      Self::IntegerConversion(_) => "sweep_receipt_integer_conversion",
      Self::SliceConversion(_) => "sweep_receipt_slice_conversion",
    }
  }

  pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

pub(crate) fn validate_sweep_receipt_void_authority_v1(
  request: SweepReceiptVoidAuthorityRequestV1<'_>,
  snapshot: &SweepReceiptVoidAuthoritySnapshotV1,
) -> Result<(), SweepReceiptReconciliationErrorV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if snapshot.selected_void_catalog_hash.len() != hash_width
    || snapshot.selected_void_catalog_hash.iter().all(|byte| *byte == 0)
    || snapshot.selected_void_catalog_generation == 0
    || snapshot.reclaim_committed_at_ms <= 0
  {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_void_identity",
      "selected Void catalog identity, generation, or durable commit time is invalid",
    ));
  }
  if !snapshot.selected_void_catalog_current
    || !snapshot.proposal_catalog_closure_complete
    || !snapshot.reclaimed_extents_exact
    || !snapshot.nonreclaimed_extents_absent
    || !snapshot.locator_removals_durable
    || !snapshot.replacement_lineage_complete
    || !snapshot.memory_coordinator_current
    || !snapshot.receipt_search_complete
    || snapshot.conflicting_receipt_count != 0
    || !snapshot.repair_latch_clear
  {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_void_authority_changed",
      "selected Void, closure, locator, lineage, receipt-search, or repair authority is incomplete",
    ));
  }
  if !snapshot.allocator_admission_blocked && snapshot.existing_receipt.is_none() {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_allocator_unblocked",
      "allocator admission became available before an exact receipt was proven",
    ));
  }
  if snapshot.existing_receipt.as_ref().is_some_and(|receipt| {
    receipt.receipt_hash.len() != hash_width || receipt.receipt_hash.iter().all(|byte| *byte == 0) || receipt.receipt_write_sequence == 0
  }) {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_existing_identity",
      "existing sweep receipt authority has an invalid hash or write sequence",
    ));
  }
  Ok(())
}

pub(crate) fn reserve_sweep_receipt_reconciliation_v1(
  hash_algorithm: HashAlgorithm,
  outcome_count: u32,
  memory: &MemoryCoordinator,
) -> Result<MemoryReservation, SweepReceiptReconciliationErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  let outcome_count = usize::try_from(outcome_count)?;
  let record_length = 48usize
    .checked_add(
      hash_width.checked_mul(2).ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "hash width overflowed"))?,
    )
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "outcome record length overflowed"))?;
  let records_length = outcome_count
    .checked_mul(record_length)
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "outcome records length overflowed"))?;
  let body_length = 64usize
    .checked_add(
      hash_width
        .checked_mul(2)
        .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "receipt hash fields overflowed"))?,
    )
    .and_then(|length| length.checked_add(records_length))
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "receipt body length overflowed"))?;
  let artifact_length = checked_immutable_gc_artifact_encoded_length(GcArtifactKindV1::RecoveredSweepReceipt, 32, body_length)?;
  let write_rows_length = outcome_count
    .checked_mul(std::mem::size_of::<SweepReceiptOutcomeWriteV1<'_>>())
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "receipt write rows overflowed"))?;
  let recovered_outcomes_length = outcome_count
    .checked_mul(std::mem::size_of::<SweepLocatorRemovalOutcomeV1>())
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "recovered outcome rows overflowed"))?;
  let accounted_bytes = u64::try_from(
    artifact_length
      .checked_mul(4)
      .and_then(|length| length.checked_add(write_rows_length))
      .and_then(|length| length.checked_add(recovered_outcomes_length))
      .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "receipt memory estimate overflowed"))?,
  )?
  .checked_add(SWEEP_RECEIPT_RECONCILIATION_BASE_BYTES_V1)
  .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_size", "receipt memory estimate overflowed"))?;
  memory.reserve(MemoryOwner::GarbageCollection, accounted_bytes, AdmissionClass::Maintenance).map_err(Into::into)
}

pub(crate) fn prepare_sweep_receipt_reconciliation_v1<'a>(
  request: SweepReceiptVoidAuthorityRequestV1<'a>,
  snapshot: &SweepReceiptVoidAuthoritySnapshotV1,
  outcomes: &[SweepLocatorRemovalOutcomeV1],
  memory_reservation: MemoryReservation,
) -> Result<PreparedSweepReceiptReconciliationV1, SweepReceiptReconciliationErrorV1> {
  validate_sweep_receipt_void_authority_v1(request, snapshot)?;
  validate_sweep_locator_removal_outcomes_v1(request.hash_algorithm, request.proposal, outcomes)?;

  let outcome_count = usize::try_from(request.proposal.candidate_count)?;
  let mut write_outcomes = Vec::with_capacity(outcome_count);
  let mut candidates = request.proposal.candidate_records(request.hash_algorithm)?;
  for outcome in outcomes {
    let candidate = candidates.next().ok_or_else(|| {
      SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_outcome_count", "proposal ended before receipt outcomes")
    })??;
    write_outcomes.push(SweepReceiptOutcomeWriteV1 {
      incarnation: candidate,
      outcome: outcome.outcome,
      stable_reason_detail: outcome.stable_reason_detail,
      resulting_void_offset: outcome.resulting_void_offset,
      resulting_void_length: outcome.resulting_void_length,
    });
  }
  if candidates.next().is_some() {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_outcome_count",
      "proposal contains candidates beyond the receipt outcomes",
    ));
  }
  let artifact = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
    hash_algorithm: request.hash_algorithm,
    recovered: request.recovery,
    database_id: request.database_id,
    batch_id: request.batch_id,
    generation: request.generation,
    reclaim_committed_at_ms: snapshot.reclaim_committed_at_ms,
    proposal_hash: request.proposal_hash,
    void_catalog_hash: &snapshot.selected_void_catalog_hash,
    outcomes: &write_outcomes,
  })?;

  Ok(PreparedSweepReceiptReconciliationV1 {
    artifact,
    recovered: request.recovery,
    void_catalog_hash: snapshot.selected_void_catalog_hash.clone(),
    void_catalog_generation: snapshot.selected_void_catalog_generation,
    reclaim_committed_at_ms: snapshot.reclaim_committed_at_ms,
    _memory: memory_reservation,
  })
}

pub(crate) fn validate_existing_sweep_receipt_v1(
  request: SweepReceiptVoidAuthorityRequestV1<'_>,
  snapshot: &SweepReceiptVoidAuthoritySnapshotV1,
  receipt: &SweepReceiptV1<'_>,
  expected_outcomes: Option<&[SweepLocatorRemovalOutcomeV1]>,
) -> Result<(), SweepReceiptReconciliationErrorV1> {
  if receipt.database_id != request.database_id
    || receipt.batch_id != request.batch_id
    || receipt.generation != request.generation
    || receipt.reclaim_committed_at_ms != snapshot.reclaim_committed_at_ms
    || receipt.proposal_hash != request.proposal_hash
    || receipt.void_catalog_hash != snapshot.selected_void_catalog_hash
    || receipt.outcome_count != request.proposal.candidate_count
  {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_existing_conflict",
      "existing receipt identity, selected catalog, commit time, or outcome count differs from the reconciled batch",
    ));
  }
  let hash_width = request.hash_algorithm.hash_length();
  let candidate_length = 24usize
    .checked_add(
      hash_width
        .checked_mul(2)
        .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_size", "candidate hash widths overflowed"))?,
    )
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_size", "candidate record length overflowed"))?;
  let outcome_length = 48usize
    .checked_add(
      hash_width
        .checked_mul(2)
        .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_size", "outcome hash widths overflowed"))?,
    )
    .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_size", "outcome record length overflowed"))?;
  let candidates_match = request
    .proposal
    .candidates
    .chunks_exact(candidate_length)
    .zip(receipt.outcomes.chunks_exact(outcome_length))
    .all(|(candidate, outcome)| candidate == &outcome[..candidate_length]);
  if !candidates_match {
    return Err(SweepReceiptReconciliationErrorV1::invalid(
      "sweep_receipt_existing_conflict",
      "existing receipt physical incarnations differ from the durable proposal",
    ));
  }
  if let Some(expected_outcomes) = expected_outcomes {
    validate_sweep_locator_removal_outcomes_v1(request.hash_algorithm, request.proposal, expected_outcomes)?;
    let mut records = receipt.outcome_records(request.hash_algorithm)?;
    for expected in expected_outcomes {
      let actual = records.next().ok_or_else(|| {
        SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_conflict", "existing receipt outcomes ended early")
      })??;
      if actual.outcome != expected.outcome
        || actual.stable_reason_detail != expected.stable_reason_detail
        || actual.resulting_void_offset != expected.resulting_void_offset
        || actual.resulting_void_length != expected.resulting_void_length
      {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_existing_conflict",
          "existing receipt outcomes differ from the live removal completion",
        ));
      }
    }
    if records.next().is_some() {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_existing_conflict",
        "existing receipt contains outcomes beyond the live removal completion",
      ));
    }
  }
  Ok(())
}
