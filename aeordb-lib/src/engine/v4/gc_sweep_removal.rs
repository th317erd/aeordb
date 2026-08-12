//! Guarded, proposal-bound physical locator removal contracts.
//!
//! The concrete mutation authority remains caller-owned. This module binds its
//! complete per-incarnation result to one durable proposal without publishing
//! a sweep receipt or making any physical extent reusable.

use std::fmt::{self, Debug, Formatter};
use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::first_authority::{FirstAuthorityPublicationErrorV1, PhysicalQuarantinePublicationErrorV1};
use super::gc_void::{SweepOutcomeClassV1, SweepProposalV1};
use super::read_view::RootPinCoordinatorErrorV1;
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

const SWEEP_REMOVAL_RESULT_ACCOUNTED_BASE_BYTES_V1: u64 = 256;

#[derive(Clone, Copy, Debug)]
pub struct SweepLocatorRemovalAuthorityRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub batch_id: &'a [u8; 16],
  pub generation: u64,
  pub proposal_hash: &'a [u8],
  pub proposal_write_sequence: u64,
  pub quarantine_manifest_hash: &'a [u8],
  pub proposal: &'a SweepProposalV1<'a>,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepLocatorRemovalAuthoritySnapshotV1 {
  pub selected_quarantine_manifest_hash: Vec<u8>,
  pub selected_mark_generation: u64,
  pub lifecycle_current: bool,
  pub all_candidates_still_grace_eligible: bool,
  pub all_candidate_incarnations_exact_and_unreachable: bool,
  pub all_locator_and_replacement_states_match: bool,
  pub replacement_lineage_complete: bool,
  pub all_physical_ranges_valid: bool,
  pub request_pin_coordinator_current: bool,
  pub task_and_audit_pins_absent: bool,
  pub protected_family_policy_allows: bool,
  pub repair_latch_clear: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepLocatorRemovalOutcomeV1 {
  pub ordinal: u32,
  pub outcome: SweepOutcomeClassV1,
  pub stable_reason_detail: u16,
  pub resulting_void_offset: u64,
  pub resulting_void_length: u32,
}

#[must_use = "the durable locator-removal sequence and every outcome must be bound into Void publication or retained for recovery"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepLocatorRemovalBatchOutcomeV1 {
  pub reclaim_commit_sequence: u64,
  pub outcomes: Vec<SweepLocatorRemovalOutcomeV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct SweepLocatorRemovalAuthorityErrorV1 {
  code: String,
  message: String,
}

impl SweepLocatorRemovalAuthorityErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    if self.code.is_empty() {
      "sweep_removal_authority"
    } else {
      self.code.as_str()
    }
  }
}

pub trait SweepLocatorRemovalAuthorityV1 {
  /// Recheck every caller-owned authority input before any locator mutation.
  /// Implementations must not mutate storage or reenter either guard here.
  fn recheck_sweep_locator_removal_authority(
    &mut self,
    request: SweepLocatorRemovalAuthorityRequestV1<'_>,
  ) -> Result<SweepLocatorRemovalAuthoritySnapshotV1, SweepLocatorRemovalAuthorityErrorV1>;

  /// Remove/classify the complete bounded proposal in one caller-owned path.
  ///
  /// This operation deliberately has no batch-level error return. Every I/O,
  /// corruption, policy, pin, or cancellation result after mutation begins
  /// must occupy its proposal ordinal using one frozen outcome class. A process
  /// crash is reconciled later from the durable proposal. Implementations must
  /// not reenter the first-authority or request-pin guards held by the caller.
  /// `reclaim_commit_sequence` must be one durable, globally monotonic v4 write
  /// sequence assigned after every returned locator outcome is durable. It must
  /// strictly advance `request.proposal_write_sequence`; gaps are valid, but
  /// sequence reuse and wrap are forbidden.
  fn remove_sweep_locators(&mut self, request: SweepLocatorRemovalAuthorityRequestV1<'_>) -> SweepLocatorRemovalBatchOutcomeV1;
}

#[must_use = "locator-removal outcomes must be consumed by receipt/Void reconciliation or retained for retry"]
pub struct SweepLocatorRemovalCompletionPermitV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  batch_id: [u8; 16],
  generation: u64,
  proposal_hash: Vec<u8>,
  proposal_write_sequence: u64,
  reclaim_commit_sequence: u64,
  quarantine_manifest_hash: Vec<u8>,
  outcomes: Box<[SweepLocatorRemovalOutcomeV1]>,
  _memory: MemoryReservation,
}

impl Debug for SweepLocatorRemovalCompletionPermitV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("SweepLocatorRemovalCompletionPermitV1")
      .field("hash_algorithm", &self.hash_algorithm)
      .field("database_id", &hex::encode(self.database_id))
      .field("batch_id", &hex::encode(self.batch_id))
      .field("generation", &self.generation)
      .field("proposal_hash", &hex::encode(&self.proposal_hash))
      .field("proposal_write_sequence", &self.proposal_write_sequence)
      .field("reclaim_commit_sequence", &self.reclaim_commit_sequence)
      .field("quarantine_manifest_hash", &hex::encode(&self.quarantine_manifest_hash))
      .field("outcome_count", &self.outcomes.len())
      .finish()
  }
}

impl SweepLocatorRemovalCompletionPermitV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn batch_id(&self) -> [u8; 16] {
    self.batch_id
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub fn proposal_hash(&self) -> &[u8] {
    &self.proposal_hash
  }

  pub const fn proposal_write_sequence(&self) -> u64 {
    self.proposal_write_sequence
  }

  pub const fn reclaim_commit_sequence(&self) -> u64 {
    self.reclaim_commit_sequence
  }

  pub fn quarantine_manifest_hash(&self) -> &[u8] {
    &self.quarantine_manifest_hash
  }

  pub fn outcomes(&self) -> &[SweepLocatorRemovalOutcomeV1] {
    &self.outcomes
  }
}

#[derive(Debug, Error)]
pub enum SweepLocatorRemovalErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("sweep locator removal authority recheck failed: {0}")]
  AuthorityRecheck(#[from] SweepLocatorRemovalAuthorityErrorV1),
  #[error("sweep locator removal first-authority failure: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("sweep locator removal quarantine-authority failure: {0}")]
  Quarantine(#[from] PhysicalQuarantinePublicationErrorV1),
  #[error("sweep locator removal request-pin failure: {0}")]
  Pin(#[from] RootPinCoordinatorErrorV1),
  #[error("sweep locator removal format failure: {0}")]
  Format(#[from] FormatError),
  #[error("sweep locator removal memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("sweep locator removal integer conversion failed: {0}")]
  IntegerConversion(#[from] TryFromIntError),
}

impl SweepLocatorRemovalErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::AuthorityRecheck(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Quarantine(source) => source.code(),
      Self::Pin(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Memory(_) => "sweep_removal_memory",
      Self::IntegerConversion(_) => "sweep_removal_integer_conversion",
    }
  }

  pub(crate) fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

pub(crate) fn reserve_sweep_locator_removal_results_v1(
  memory: &MemoryCoordinator,
  candidate_count: u32,
) -> Result<MemoryReservation, SweepLocatorRemovalErrorV1> {
  let result_bytes = usize::try_from(candidate_count)?
    .checked_mul(std::mem::size_of::<SweepLocatorRemovalOutcomeV1>())
    .ok_or_else(|| SweepLocatorRemovalErrorV1::invalid("sweep_removal_result_size", "result memory estimate overflowed"))?;
  let accounted_bytes = SWEEP_REMOVAL_RESULT_ACCOUNTED_BASE_BYTES_V1
    .checked_add(u64::try_from(result_bytes)?)
    .ok_or_else(|| SweepLocatorRemovalErrorV1::invalid("sweep_removal_result_size", "result memory estimate overflowed"))?;
  memory.reserve(MemoryOwner::GarbageCollection, accounted_bytes, AdmissionClass::Maintenance).map_err(Into::into)
}

pub(crate) fn validate_sweep_locator_removal_snapshot_v1(
  request: SweepLocatorRemovalAuthorityRequestV1<'_>,
  snapshot: &SweepLocatorRemovalAuthoritySnapshotV1,
) -> Result<(), SweepLocatorRemovalErrorV1> {
  if snapshot.selected_quarantine_manifest_hash != request.quarantine_manifest_hash
    || snapshot.selected_mark_generation != request.generation
  {
    return Err(SweepLocatorRemovalErrorV1::invalid(
      "sweep_removal_authority_changed",
      "caller-owned selected quarantine authority differs from the durable proposal",
    ));
  }
  if !snapshot.lifecycle_current
    || !snapshot.all_candidates_still_grace_eligible
    || !snapshot.all_candidate_incarnations_exact_and_unreachable
    || !snapshot.all_locator_and_replacement_states_match
    || !snapshot.replacement_lineage_complete
    || !snapshot.all_physical_ranges_valid
    || !snapshot.request_pin_coordinator_current
    || !snapshot.task_and_audit_pins_absent
    || !snapshot.protected_family_policy_allows
    || !snapshot.repair_latch_clear
  {
    return Err(SweepLocatorRemovalErrorV1::invalid(
      "sweep_removal_authority_changed",
      "caller-owned lifecycle, incarnation, locator, lineage, range, request-pin coordinator, pin, policy, or repair authority changed",
    ));
  }
  Ok(())
}

pub(crate) fn complete_sweep_locator_removal_v1(
  request: SweepLocatorRemovalAuthorityRequestV1<'_>,
  batch_outcome: SweepLocatorRemovalBatchOutcomeV1,
  memory: MemoryReservation,
) -> Result<SweepLocatorRemovalCompletionPermitV1, SweepLocatorRemovalErrorV1> {
  if batch_outcome.reclaim_commit_sequence <= request.proposal_write_sequence {
    return Err(SweepLocatorRemovalErrorV1::invalid(
      "sweep_removal_commit_sequence",
      "locator-removal commit sequence must advance the durable sweep proposal sequence",
    ));
  }
  validate_sweep_locator_removal_outcomes_v1(request.hash_algorithm, request.proposal, &batch_outcome.outcomes)?;
  Ok(SweepLocatorRemovalCompletionPermitV1 {
    hash_algorithm: request.hash_algorithm,
    database_id: *request.database_id,
    batch_id: *request.batch_id,
    generation: request.generation,
    proposal_hash: request.proposal_hash.to_vec(),
    proposal_write_sequence: request.proposal_write_sequence,
    reclaim_commit_sequence: batch_outcome.reclaim_commit_sequence,
    quarantine_manifest_hash: request.quarantine_manifest_hash.to_vec(),
    outcomes: batch_outcome.outcomes.into_boxed_slice(),
    _memory: memory,
  })
}

pub(crate) fn validate_sweep_locator_removal_outcomes_v1(
  hash_algorithm: HashAlgorithm,
  proposal: &SweepProposalV1<'_>,
  outcomes: &[SweepLocatorRemovalOutcomeV1],
) -> Result<(), SweepLocatorRemovalErrorV1> {
  let candidate_count = usize::try_from(proposal.candidate_count)?;
  if candidate_count == 0 || outcomes.len() != candidate_count {
    return Err(SweepLocatorRemovalErrorV1::invalid(
      "sweep_removal_outcome_count",
      "caller-owned removal authority did not return one outcome per proposal candidate",
    ));
  }
  let mut candidates = proposal.candidate_records(hash_algorithm)?;
  for (index, outcome) in outcomes.iter().enumerate() {
    let candidate = candidates.next().ok_or_else(|| {
      SweepLocatorRemovalErrorV1::invalid("sweep_removal_outcome_count", "proposal candidates ended before removal outcomes")
    })??;
    let ordinal = u32::try_from(index)?;
    if outcome.ordinal != ordinal {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "sweep_removal_outcome_order",
        "caller-owned removal outcomes are not in exact proposal order",
      ));
    }
    let reclaimed = outcome.outcome == SweepOutcomeClassV1::Reclaimed;
    if (reclaimed
      && (outcome.stable_reason_detail != 0
        || outcome.resulting_void_offset != candidate.wal_offset
        || outcome.resulting_void_length != candidate.entity_length))
      || (!reclaimed && (outcome.stable_reason_detail == 0 || outcome.resulting_void_offset != 0 || outcome.resulting_void_length != 0))
    {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "sweep_removal_outcome_shape",
        "caller-owned removal outcome class, reason, or extent differs from its proposal candidate",
      ));
    }
  }
  if candidates.next().is_some() {
    return Err(SweepLocatorRemovalErrorV1::invalid(
      "sweep_removal_outcome_count",
      "proposal contains candidates beyond the returned removal outcomes",
    ));
  }
  Ok(())
}
