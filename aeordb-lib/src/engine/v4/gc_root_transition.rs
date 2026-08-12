use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::contract_generated::root_retirement_reason_v1;
use super::gc_lifecycle::{RootCandidateRecordWriteV1, RootExpiryManifestV1, RootLifecycleManifestV1, validate_root_lifecycle_expiry_manifest};
use super::gc_state::RootCandidateRecordV1;
use super::reader::FormatError;
use crate::engine::HashAlgorithm;

pub const REQUIRED_ROOT_LIFECYCLE_COMPLETE_MARKS_V1: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub struct RootLifecycleTransitionContextV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub prior_lifecycle: &'a RootLifecycleManifestV1<'a>,
  pub prior_expiry: Option<&'a RootExpiryManifestV1<'a>>,
  pub lifecycle_generation: u64,
  pub complete_mark_generation: u64,
  pub completed_at_ms: i64,
  pub current_configured_grace_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  /// Bounds candidate rows plus mandatory logically-retired expiry rows.
  /// Optional post-reclaim evidence has its own retention budget.
  pub lifecycle_hard_max_bytes: u64,
  pub maximum_roots: u64,
  pub mark_complete: bool,
  pub destructive_gc_enabled: bool,
  pub lifecycle_authority_healthy: bool,
  pub physical_authority_healthy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootLifecycleReachabilityV1<'a> {
  Reachable,
  ConfirmedUnreachable { reason: u16, admission_commit_payload_hash: &'a [u8] },
  Indeterminate,
}

#[derive(Debug, Clone, Copy)]
pub struct RootLifecycleRootObservationV1<'a> {
  pub namespace_root_hash: &'a [u8],
  pub prior_candidate: Option<&'a RootCandidateRecordV1<'a>>,
  pub reachability: RootLifecycleReachabilityV1<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCandidateStateV1 {
  pub namespace_root_hash: Vec<u8>,
  pub reason: u16,
  pub pending_since_ms: i64,
  pub first_unreachable_generation: u64,
  pub last_confirmed_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
  pub authority_root_set_digest: Vec<u8>,
  pub admission_commit_payload_hash: Vec<u8>,
}

impl RootCandidateStateV1 {
  pub fn as_write_request(&self, hash_algorithm: HashAlgorithm) -> RootCandidateRecordWriteV1<'_> {
    RootCandidateRecordWriteV1 {
      hash_algorithm,
      namespace_root_hash: &self.namespace_root_hash,
      reason: self.reason,
      pending_since_ms: self.pending_since_ms,
      first_unreachable_generation: self.first_unreachable_generation,
      last_confirmed_unreachable_generation: self.last_confirmed_unreachable_generation,
      grace_at_pending_ms: self.grace_at_pending_ms,
      authority_root_set_digest: &self.authority_root_set_digest,
      admission_commit_payload_hash: &self.admission_commit_payload_hash,
    }
  }

  fn from_prior(candidate: &RootCandidateRecordV1<'_>, last_confirmed_unreachable_generation: u64) -> Self {
    Self {
      namespace_root_hash: candidate.namespace_root_hash.to_vec(),
      reason: candidate.reason,
      pending_since_ms: candidate.pending_since_ms,
      first_unreachable_generation: candidate.first_unreachable_generation,
      last_confirmed_unreachable_generation,
      grace_at_pending_ms: candidate.grace_at_pending_ms,
      authority_root_set_digest: candidate.authority_root_set_digest.to_vec(),
      admission_commit_payload_hash: candidate.admission_commit_payload_hash.to_vec(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRetirementIntentV1 {
  pub namespace_root_hash: Vec<u8>,
  pub committed_at_ms: i64,
  pub pending_since_ms: i64,
  pub grace_at_pending_ms: u64,
  pub final_mark_generation: u64,
  pub reason: u16,
  pub prior_lifecycle_manifest_hash: Vec<u8>,
  pub authority_root_set_digest: Vec<u8>,
  pub admission_commit_payload_hash: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootLifecycleTransitionV1 {
  Retained,
  CandidateStarted(RootCandidateStateV1),
  CandidateConfirmed(RootCandidateStateV1),
  CandidateCleared,
  RetirementEligible(RootRetirementIntentV1),
  CapacityDeferred { candidate: Option<RootCandidateStateV1> },
  IndeterminateRetained { had_candidate: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootLifecycleTransitionSummaryV1 {
  pub observed_root_count: u64,
  pub observed_prior_candidate_count: u64,
  pub resulting_candidate_count: u64,
  pub resulting_candidate_bytes: u64,
  pub resulting_mandatory_count: u64,
  pub resulting_mandatory_bytes: u64,
  pub resulting_lifecycle_bytes: u64,
  pub started_count: u64,
  pub confirmed_count: u64,
  pub cleared_count: u64,
  pub retirement_count: u64,
  pub capacity_deferred_count: u64,
  pub indeterminate_count: u64,
  pub capacity_blocked: bool,
}

#[derive(Debug, Error)]
pub enum RootLifecycleTransitionErrorV1 {
  #[error("invalid root-lifecycle transition configuration: {0}")]
  InvalidConfiguration(&'static str),
  #[error("root-lifecycle transition requires a complete healthy destructive mark")]
  Unavailable,
  #[error("root-lifecycle transition was canceled")]
  Canceled,
  #[error("root-lifecycle transition has a stale or invalid generation")]
  StaleGeneration,
  #[error("root-lifecycle transition received an invalid root hash")]
  InvalidRoot,
  #[error("root-lifecycle observations are not strictly ordered")]
  RecordOrder,
  #[error("root-lifecycle observation does not match its prior candidate")]
  CandidateRootMismatch,
  #[error("root-lifecycle candidate evidence changed while pending")]
  CandidateEvidenceMismatch,
  #[error("root-lifecycle candidate generations do not close against the prior complete mark")]
  CandidateGenerationMismatch,
  #[error("root-lifecycle grace timestamp is invalid or overflowed")]
  TimeOverflow,
  #[error("root-lifecycle transition exceeded its root limit")]
  RecordLimit,
  #[error("root-lifecycle transition accounting overflowed or underflowed")]
  Arithmetic,
  #[error("root-lifecycle transition integer conversion failed: {0}")]
  ArithmeticConversion(std::num::TryFromIntError),
  #[error("root-lifecycle grace conversion failed: {0}")]
  TimeConversion(std::num::TryFromIntError),
  #[error("root-lifecycle transition does not close against its prior manifest")]
  ManifestAggregate,
  #[error(transparent)]
  Format(Box<FormatError>),
  #[error("root-lifecycle transition model has already failed")]
  Failed,
}

impl RootLifecycleTransitionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidConfiguration(_) => "root_lifecycle_transition_configuration",
      Self::Unavailable => "root_lifecycle_transition_unavailable",
      Self::Canceled => "root_lifecycle_transition_canceled",
      Self::StaleGeneration | Self::CandidateGenerationMismatch => "root_lifecycle_transition_generation",
      Self::InvalidRoot => "root_lifecycle_transition_root",
      Self::RecordOrder => "root_lifecycle_transition_order",
      Self::CandidateRootMismatch => "root_lifecycle_transition_candidate_root",
      Self::CandidateEvidenceMismatch => "root_lifecycle_transition_evidence",
      Self::TimeOverflow | Self::TimeConversion(_) => "root_lifecycle_transition_time",
      Self::RecordLimit => "root_lifecycle_transition_limit",
      Self::Arithmetic | Self::ArithmeticConversion(_) => "root_lifecycle_transition_arithmetic",
      Self::ManifestAggregate => "root_lifecycle_transition_manifest",
      Self::Format(error) => error.code(),
      Self::Failed => "root_lifecycle_transition_failed",
    }
  }
}

impl From<FormatError> for RootLifecycleTransitionErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Format(Box::new(error))
  }
}

#[derive(Debug)]
pub struct RootLifecycleTransitionModelV1<'a> {
  context: RootLifecycleTransitionContextV1<'a>,
  cancellation: &'a CancellationToken,
  candidate_record_bytes: u64,
  expiry_record_bytes: u64,
  observed_root_count: u64,
  observed_prior_candidate_count: u64,
  resulting_candidate_count: u64,
  resulting_candidate_bytes: u64,
  resulting_mandatory_count: u64,
  resulting_mandatory_bytes: u64,
  started_count: u64,
  confirmed_count: u64,
  cleared_count: u64,
  retirement_count: u64,
  capacity_deferred_count: u64,
  indeterminate_count: u64,
  capacity_blocked: bool,
  previous_root_hash: Vec<u8>,
  failed: bool,
}

impl<'a> RootLifecycleTransitionModelV1<'a> {
  pub fn new(
    context: RootLifecycleTransitionContextV1<'a>,
    cancellation: &'a CancellationToken,
  ) -> Result<Self, RootLifecycleTransitionErrorV1> {
    if cancellation.is_cancelled() {
      return Err(RootLifecycleTransitionErrorV1::Canceled);
    }
    if !context.mark_complete
      || !context.destructive_gc_enabled
      || !context.lifecycle_authority_healthy
      || !context.physical_authority_healthy
    {
      return Err(RootLifecycleTransitionErrorV1::Unavailable);
    }
    if context.maximum_roots == 0 {
      return Err(RootLifecycleTransitionErrorV1::InvalidConfiguration("maximum roots must be nonzero"));
    }
    if context.lifecycle_hard_max_bytes == 0 {
      return Err(RootLifecycleTransitionErrorV1::InvalidConfiguration("lifecycle hard limit must be nonzero"));
    }
    if context.completed_at_ms <= 0 {
      return Err(RootLifecycleTransitionErrorV1::InvalidConfiguration("complete mark timestamp must be positive"));
    }
    if context.lifecycle_generation <= context.prior_lifecycle.generation
      || context.complete_mark_generation <= context.prior_lifecycle.source_complete_mark_generation
    {
      return Err(RootLifecycleTransitionErrorV1::StaleGeneration);
    }

    let hash_width = context.hash_algorithm.hash_length();
    require_manifest_width(context.prior_lifecycle.database_id, 16)?;
    require_manifest_width(context.prior_lifecycle.key.as_slice(), hash_width)?;
    require_manifest_width(context.prior_lifecycle.authority_root_set_digest, hash_width)?;
    require_manifest_width(context.authority_root_set_digest, hash_width)?;
    if let Some(candidate_directory_hash) = context.prior_lifecycle.candidate_directory_hash {
      require_manifest_width(candidate_directory_hash, hash_width)?;
    }
    if let Some(expiry_manifest_hash) = context.prior_lifecycle.root_expiry_manifest_hash {
      require_manifest_width(expiry_manifest_hash, hash_width)?;
    }
    let candidate_record_bytes = u64::try_from(36usize + 3 * hash_width).map_err(RootLifecycleTransitionErrorV1::ArithmeticConversion)?;
    let expiry_record_bytes = u64::try_from(40usize + 3 * hash_width).map_err(RootLifecycleTransitionErrorV1::ArithmeticConversion)?;
    let expected_candidate_bytes =
      context.prior_lifecycle.candidate_count.checked_mul(candidate_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    if context.prior_lifecycle.candidate_count > context.maximum_roots {
      return Err(RootLifecycleTransitionErrorV1::RecordLimit);
    }
    if expected_candidate_bytes != context.prior_lifecycle.candidate_bytes
      || (context.prior_lifecycle.candidate_directory_hash.is_some() != (context.prior_lifecycle.candidate_count != 0))
      || context.prior_lifecycle.pending_count != context.prior_lifecycle.candidate_count
    {
      return Err(RootLifecycleTransitionErrorV1::ManifestAggregate);
    }

    let (mandatory_count, mandatory_bytes) = match context.prior_expiry {
      Some(expiry) => {
        validate_root_lifecycle_expiry_manifest(context.prior_lifecycle, expiry)?;
        let expected_mandatory_bytes =
          expiry.mandatory_count.checked_mul(expiry_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
        let expected_optional_bytes =
          expiry.optional_count.checked_mul(expiry_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
        let expected_record_count =
          expiry.mandatory_count.checked_add(expiry.optional_count).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
        let expected_logical_bytes =
          expected_mandatory_bytes.checked_add(expected_optional_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
        if expected_mandatory_bytes != expiry.mandatory_bytes
          || expected_optional_bytes != expiry.optional_bytes
          || expected_record_count != expiry.record_count
          || expected_logical_bytes != expiry.logical_bytes
          || (expiry.directory_root_hash.is_some() != (expiry.record_count != 0))
        {
          return Err(RootLifecycleTransitionErrorV1::ManifestAggregate);
        }
        (expiry.mandatory_count, expiry.mandatory_bytes)
      }
      None => {
        if context.prior_lifecycle.root_expiry_manifest_hash.is_some()
          || context.prior_lifecycle.retired_evidence_count != 0
          || context.prior_lifecycle.expiry_bytes != 0
        {
          return Err(RootLifecycleTransitionErrorV1::ManifestAggregate);
        }
        (0, 0)
      }
    };
    let current_lifecycle_bytes =
      expected_candidate_bytes.checked_add(mandatory_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;

    Ok(Self {
      context,
      cancellation,
      candidate_record_bytes,
      expiry_record_bytes,
      observed_root_count: 0,
      observed_prior_candidate_count: 0,
      resulting_candidate_count: context.prior_lifecycle.candidate_count,
      resulting_candidate_bytes: expected_candidate_bytes,
      resulting_mandatory_count: mandatory_count,
      resulting_mandatory_bytes: mandatory_bytes,
      started_count: 0,
      confirmed_count: 0,
      cleared_count: 0,
      retirement_count: 0,
      capacity_deferred_count: 0,
      indeterminate_count: 0,
      capacity_blocked: current_lifecycle_bytes > context.lifecycle_hard_max_bytes,
      previous_root_hash: Vec::with_capacity(hash_width),
      failed: false,
    })
  }

  pub fn observe(
    &mut self,
    observation: RootLifecycleRootObservationV1<'_>,
  ) -> Result<RootLifecycleTransitionV1, RootLifecycleTransitionErrorV1> {
    if self.failed {
      return Err(RootLifecycleTransitionErrorV1::Failed);
    }
    match self.observe_inner(observation) {
      Ok(transition) => Ok(transition),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<RootLifecycleTransitionSummaryV1, RootLifecycleTransitionErrorV1> {
    if self.failed {
      return Err(RootLifecycleTransitionErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleTransitionErrorV1::Canceled);
    }
    if self.observed_prior_candidate_count != self.context.prior_lifecycle.candidate_count {
      return Err(RootLifecycleTransitionErrorV1::ManifestAggregate);
    }
    let resulting_lifecycle_bytes =
      self.resulting_candidate_bytes.checked_add(self.resulting_mandatory_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    Ok(RootLifecycleTransitionSummaryV1 {
      observed_root_count: self.observed_root_count,
      observed_prior_candidate_count: self.observed_prior_candidate_count,
      resulting_candidate_count: self.resulting_candidate_count,
      resulting_candidate_bytes: self.resulting_candidate_bytes,
      resulting_mandatory_count: self.resulting_mandatory_count,
      resulting_mandatory_bytes: self.resulting_mandatory_bytes,
      resulting_lifecycle_bytes,
      started_count: self.started_count,
      confirmed_count: self.confirmed_count,
      cleared_count: self.cleared_count,
      retirement_count: self.retirement_count,
      capacity_deferred_count: self.capacity_deferred_count,
      indeterminate_count: self.indeterminate_count,
      capacity_blocked: self.capacity_blocked,
    })
  }

  fn observe_inner(
    &mut self,
    observation: RootLifecycleRootObservationV1<'_>,
  ) -> Result<RootLifecycleTransitionV1, RootLifecycleTransitionErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(RootLifecycleTransitionErrorV1::Canceled);
    }
    if self.observed_root_count >= self.context.maximum_roots {
      return Err(RootLifecycleTransitionErrorV1::RecordLimit);
    }
    let hash_width = self.context.hash_algorithm.hash_length();
    require_root_width(observation.namespace_root_hash, hash_width)?;
    if !self.previous_root_hash.is_empty() && self.previous_root_hash.as_slice() >= observation.namespace_root_hash {
      return Err(RootLifecycleTransitionErrorV1::RecordOrder);
    }
    if let Some(candidate) = observation.prior_candidate {
      if candidate.namespace_root_hash != observation.namespace_root_hash {
        return Err(RootLifecycleTransitionErrorV1::CandidateRootMismatch);
      }
      if candidate.last_confirmed_unreachable_generation > self.context.prior_lifecycle.source_complete_mark_generation
        || candidate.first_unreachable_generation > candidate.last_confirmed_unreachable_generation
      {
        return Err(RootLifecycleTransitionErrorV1::CandidateGenerationMismatch);
      }
      self.observed_prior_candidate_count = checked_increment(self.observed_prior_candidate_count)?;
    }
    self.observed_root_count = checked_increment(self.observed_root_count)?;
    self.previous_root_hash.clear();
    self.previous_root_hash.extend_from_slice(observation.namespace_root_hash);

    match observation.reachability {
      RootLifecycleReachabilityV1::Reachable => self.clear_or_retain(observation.prior_candidate),
      RootLifecycleReachabilityV1::Indeterminate => {
        self.indeterminate_count = checked_increment(self.indeterminate_count)?;
        Ok(RootLifecycleTransitionV1::IndeterminateRetained { had_candidate: observation.prior_candidate.is_some() })
      }
      RootLifecycleReachabilityV1::ConfirmedUnreachable { reason, admission_commit_payload_hash } => {
        self.confirm_unreachable(observation.namespace_root_hash, observation.prior_candidate, reason, admission_commit_payload_hash)
      }
    }
  }

  fn clear_or_retain(
    &mut self,
    candidate: Option<&RootCandidateRecordV1<'_>>,
  ) -> Result<RootLifecycleTransitionV1, RootLifecycleTransitionErrorV1> {
    if candidate.is_none() {
      return Ok(RootLifecycleTransitionV1::Retained);
    }
    self.resulting_candidate_count = self.resulting_candidate_count.checked_sub(1).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    self.resulting_candidate_bytes =
      self.resulting_candidate_bytes.checked_sub(self.candidate_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    self.cleared_count = checked_increment(self.cleared_count)?;
    Ok(RootLifecycleTransitionV1::CandidateCleared)
  }

  fn confirm_unreachable(
    &mut self,
    root_hash: &[u8],
    prior_candidate: Option<&RootCandidateRecordV1<'_>>,
    reason: u16,
    admission_commit_payload_hash: &[u8],
  ) -> Result<RootLifecycleTransitionV1, RootLifecycleTransitionErrorV1> {
    require_reason(reason)?;
    require_evidence_width(admission_commit_payload_hash, self.context.hash_algorithm.hash_length())?;
    let Some(candidate) = prior_candidate else {
      return self.start_candidate(root_hash, reason, admission_commit_payload_hash);
    };
    if candidate.reason != reason || candidate.admission_commit_payload_hash != admission_commit_payload_hash {
      return Err(RootLifecycleTransitionErrorV1::CandidateEvidenceMismatch);
    }
    require_evidence_width(candidate.authority_root_set_digest, self.context.hash_algorithm.hash_length())?;
    let confirmed = RootCandidateStateV1::from_prior(candidate, self.context.complete_mark_generation);
    let effective_grace_ms = candidate.grace_at_pending_ms.max(self.context.current_configured_grace_ms);
    let effective_grace_ms = i64::try_from(effective_grace_ms).map_err(RootLifecycleTransitionErrorV1::TimeConversion)?;
    let eligible_at_ms = candidate.pending_since_ms.checked_add(effective_grace_ms).ok_or(RootLifecycleTransitionErrorV1::TimeOverflow)?;
    if self.context.completed_at_ms < eligible_at_ms {
      self.confirmed_count = checked_increment(self.confirmed_count)?;
      return Ok(RootLifecycleTransitionV1::CandidateConfirmed(confirmed));
    }

    if !self.reserve_capacity(self.candidate_record_bytes, self.expiry_record_bytes)? {
      self.confirmed_count = checked_increment(self.confirmed_count)?;
      self.capacity_deferred_count = checked_increment(self.capacity_deferred_count)?;
      return Ok(RootLifecycleTransitionV1::CapacityDeferred { candidate: Some(confirmed) });
    }
    self.resulting_candidate_count = self.resulting_candidate_count.checked_sub(1).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    self.resulting_candidate_bytes =
      self.resulting_candidate_bytes.checked_sub(self.candidate_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    self.resulting_mandatory_count = checked_increment(self.resulting_mandatory_count)?;
    self.resulting_mandatory_bytes =
      self.resulting_mandatory_bytes.checked_add(self.expiry_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    self.retirement_count = checked_increment(self.retirement_count)?;
    Ok(RootLifecycleTransitionV1::RetirementEligible(RootRetirementIntentV1 {
      namespace_root_hash: root_hash.to_vec(),
      committed_at_ms: self.context.completed_at_ms,
      pending_since_ms: candidate.pending_since_ms,
      grace_at_pending_ms: candidate.grace_at_pending_ms,
      final_mark_generation: self.context.complete_mark_generation,
      reason: candidate.reason,
      prior_lifecycle_manifest_hash: self.context.prior_lifecycle.key.clone(),
      authority_root_set_digest: candidate.authority_root_set_digest.to_vec(),
      admission_commit_payload_hash: candidate.admission_commit_payload_hash.to_vec(),
    }))
  }

  fn start_candidate(
    &mut self,
    root_hash: &[u8],
    reason: u16,
    admission_commit_payload_hash: &[u8],
  ) -> Result<RootLifecycleTransitionV1, RootLifecycleTransitionErrorV1> {
    if !self.reserve_capacity(0, self.candidate_record_bytes)? {
      self.capacity_deferred_count = checked_increment(self.capacity_deferred_count)?;
      return Ok(RootLifecycleTransitionV1::CapacityDeferred { candidate: None });
    }
    self.resulting_candidate_count = checked_increment(self.resulting_candidate_count)?;
    self.resulting_candidate_bytes =
      self.resulting_candidate_bytes.checked_add(self.candidate_record_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    self.started_count = checked_increment(self.started_count)?;
    Ok(RootLifecycleTransitionV1::CandidateStarted(RootCandidateStateV1 {
      namespace_root_hash: root_hash.to_vec(),
      reason,
      pending_since_ms: self.context.completed_at_ms,
      first_unreachable_generation: self.context.complete_mark_generation,
      last_confirmed_unreachable_generation: self.context.complete_mark_generation,
      grace_at_pending_ms: self.context.current_configured_grace_ms,
      authority_root_set_digest: self.context.authority_root_set_digest.to_vec(),
      admission_commit_payload_hash: admission_commit_payload_hash.to_vec(),
    }))
  }

  fn reserve_capacity(&mut self, removed_bytes: u64, added_bytes: u64) -> Result<bool, RootLifecycleTransitionErrorV1> {
    if self.capacity_blocked {
      return Ok(false);
    }
    let current_bytes =
      self.resulting_candidate_bytes.checked_add(self.resulting_mandatory_bytes).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    let next_bytes = current_bytes
      .checked_sub(removed_bytes)
      .and_then(|value| value.checked_add(added_bytes))
      .ok_or(RootLifecycleTransitionErrorV1::Arithmetic)?;
    if next_bytes > self.context.lifecycle_hard_max_bytes {
      self.capacity_blocked = true;
      return Ok(false);
    }
    Ok(true)
  }
}

fn checked_increment(value: u64) -> Result<u64, RootLifecycleTransitionErrorV1> {
  value.checked_add(1).ok_or(RootLifecycleTransitionErrorV1::Arithmetic)
}

fn width_is_valid(value: &[u8], expected_width: usize) -> bool {
  value.len() == expected_width && value.iter().any(|byte| *byte != 0)
}

fn require_manifest_width(value: &[u8], expected_width: usize) -> Result<(), RootLifecycleTransitionErrorV1> {
  if !width_is_valid(value, expected_width) {
    return Err(RootLifecycleTransitionErrorV1::ManifestAggregate);
  }
  Ok(())
}

fn require_root_width(value: &[u8], expected_width: usize) -> Result<(), RootLifecycleTransitionErrorV1> {
  if !width_is_valid(value, expected_width) {
    return Err(RootLifecycleTransitionErrorV1::InvalidRoot);
  }
  Ok(())
}

fn require_evidence_width(value: &[u8], expected_width: usize) -> Result<(), RootLifecycleTransitionErrorV1> {
  if !width_is_valid(value, expected_width) {
    return Err(RootLifecycleTransitionErrorV1::CandidateEvidenceMismatch);
  }
  Ok(())
}

fn require_reason(reason: u16) -> Result<(), RootLifecycleTransitionErrorV1> {
  if !matches!(reason, root_retirement_reason_v1::ORDINARY_GC_UNREACHABLE | root_retirement_reason_v1::EXPLICIT_OPERATOR_RETIREMENT) {
    return Err(RootLifecycleTransitionErrorV1::CandidateEvidenceMismatch);
  }
  Ok(())
}
