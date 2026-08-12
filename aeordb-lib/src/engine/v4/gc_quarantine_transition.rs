//! Constant-memory physical-quarantine state transitions.
//!
//! This unit emits candidate mutations and non-authoritative sweep intents. It
//! does not publish quarantine authority or remove a physical incarnation.

use std::cmp::Ordering;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::{PhysicalIncarnationV1, compare_physical_incarnations_v1};
use super::gc_quarantine::{
  CandidateDeltaOperationV1, CandidateDeltaRecordWriteV1, PhysicalQuarantineCandidateClassV1, PhysicalQuarantineCandidateV1,
  PhysicalQuarantineCandidateWriteV1, QuarantineManifestV1, encode_candidate_delta_record_v1, encode_physical_quarantine_candidate_v1,
  extend_candidate_mutation_digest_v1, initial_candidate_mutation_digest_v1,
};
use super::hash::digest_parts;
use super::reader::FormatError;
use crate::engine::HashAlgorithm;

pub const REQUIRED_PHYSICAL_QUARANTINE_COMPLETE_MARKS_V1: u8 = 2;

#[derive(Clone, Copy, Debug)]
pub struct PhysicalQuarantineTransitionContextV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub prior_manifest: &'a QuarantineManifestV1<'a>,
  pub mark_generation: u64,
  pub completed_at_ms: u64,
  pub current_configured_grace_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  pub semantic_state_digest: &'a [u8],
  pub kv_layout_fingerprint: &'a [u8],
  pub mark_result_digest: &'a [u8],
  pub captured_root_lifecycle_manifest: &'a [u8],
  pub maximum_incarnations: u64,
  pub maximum_candidates: u64,
  pub mark_complete: bool,
  pub destructive_gc_enabled: bool,
  pub mark_authority_healthy: bool,
  pub physical_inventory_healthy: bool,
  pub root_lifecycle_healthy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalQuarantineReachabilityV1 {
  Reachable,
  ConfirmedUnreachable { class: PhysicalQuarantineCandidateClassV1 },
  Indeterminate,
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicalQuarantineObservationV1<'a> {
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub prior_candidate: Option<&'a PhysicalQuarantineCandidateV1<'a>>,
  pub reachability: PhysicalQuarantineReachabilityV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalIncarnationStateV1 {
  pub logical_key: Vec<u8>,
  pub integrity_or_legacy_digest: Vec<u8>,
  pub wal_offset: u64,
  pub write_sequence: u64,
  pub entity_length: u32,
  pub entry_type: u8,
  pub entity_version: u8,
}

impl PhysicalIncarnationStateV1 {
  pub fn as_borrowed(&self) -> PhysicalIncarnationV1<'_> {
    PhysicalIncarnationV1 {
      logical_key: &self.logical_key,
      integrity_or_legacy_digest: &self.integrity_or_legacy_digest,
      wal_offset: self.wal_offset,
      write_sequence: self.write_sequence,
      entity_length: self.entity_length,
      entry_type: self.entry_type,
      entity_version: self.entity_version,
    }
  }

  fn try_from_borrowed(value: &PhysicalIncarnationV1<'_>) -> Result<Self, PhysicalQuarantineTransitionErrorV1> {
    Ok(Self {
      logical_key: try_copy_bytes(value.logical_key)?,
      integrity_or_legacy_digest: try_copy_bytes(value.integrity_or_legacy_digest)?,
      wal_offset: value.wal_offset,
      write_sequence: value.write_sequence,
      entity_length: value.entity_length,
      entry_type: value.entry_type,
      entity_version: value.entity_version,
    })
  }

  fn overwrite_from_borrowed(&mut self, value: &PhysicalIncarnationV1<'_>) -> Result<(), PhysicalQuarantineTransitionErrorV1> {
    self.logical_key.clear();
    self.logical_key.try_reserve_exact(value.logical_key.len()).map_err(PhysicalQuarantineTransitionErrorV1::Allocation)?;
    self.logical_key.extend_from_slice(value.logical_key);
    self.integrity_or_legacy_digest.clear();
    self
      .integrity_or_legacy_digest
      .try_reserve_exact(value.integrity_or_legacy_digest.len())
      .map_err(PhysicalQuarantineTransitionErrorV1::Allocation)?;
    self.integrity_or_legacy_digest.extend_from_slice(value.integrity_or_legacy_digest);
    self.wal_offset = value.wal_offset;
    self.write_sequence = value.write_sequence;
    self.entity_length = value.entity_length;
    self.entry_type = value.entry_type;
    self.entity_version = value.entity_version;
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalQuarantineCandidateStateV1 {
  pub hash_algorithm: HashAlgorithm,
  pub incarnation: PhysicalIncarnationStateV1,
  pub class: PhysicalQuarantineCandidateClassV1,
  pub pending_since_ms: u64,
  pub first_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
}

impl PhysicalQuarantineCandidateStateV1 {
  pub fn as_write_request(&self) -> PhysicalQuarantineCandidateWriteV1<'_> {
    PhysicalQuarantineCandidateWriteV1 {
      hash_algorithm: self.hash_algorithm,
      incarnation: self.incarnation.as_borrowed(),
      class: self.class,
      pending_since_ms: self.pending_since_ms,
      first_unreachable_generation: self.first_unreachable_generation,
      grace_at_pending_ms: self.grace_at_pending_ms,
    }
  }

  pub fn as_delta_write_request(&self) -> CandidateDeltaRecordWriteV1<'_> {
    CandidateDeltaRecordWriteV1 { operation: CandidateDeltaOperationV1::Set, candidate: self.as_write_request() }
  }

  fn try_from_prior(value: &PhysicalQuarantineCandidateV1<'_>) -> Result<Self, PhysicalQuarantineTransitionErrorV1> {
    Ok(Self {
      hash_algorithm: value.hash_algorithm,
      incarnation: PhysicalIncarnationStateV1::try_from_borrowed(&value.incarnation)?,
      class: value.class,
      pending_since_ms: value.pending_since_ms,
      first_unreachable_generation: value.first_unreachable_generation,
      grace_at_pending_ms: value.grace_at_pending_ms,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalQuarantineCandidateClearV1 {
  pub hash_algorithm: HashAlgorithm,
  pub incarnation: PhysicalIncarnationStateV1,
  pub class: PhysicalQuarantineCandidateClassV1,
}

impl PhysicalQuarantineCandidateClearV1 {
  pub fn as_delta_write_request(&self) -> CandidateDeltaRecordWriteV1<'_> {
    CandidateDeltaRecordWriteV1 {
      operation: CandidateDeltaOperationV1::Clear,
      candidate: PhysicalQuarantineCandidateWriteV1 {
        hash_algorithm: self.hash_algorithm,
        incarnation: self.incarnation.as_borrowed(),
        class: self.class,
        pending_since_ms: 0,
        first_unreachable_generation: 0,
        grace_at_pending_ms: 0,
      },
    }
  }
}

/// A bounded, non-authoritative request for P4-6 to revalidate before sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalSweepIntentV1 {
  pub candidate: PhysicalQuarantineCandidateStateV1,
  pub confirming_mark_generation: u64,
  pub confirmed_at_ms: u64,
  pub effective_grace_ms: u64,
  pub eligible_at_ms: u64,
  pub prior_quarantine_manifest_hash: Vec<u8>,
  pub authority_root_set_digest: Vec<u8>,
  pub semantic_state_digest: Vec<u8>,
  pub kv_layout_fingerprint: Vec<u8>,
  pub mark_result_digest: Vec<u8>,
  pub captured_root_lifecycle_manifest: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhysicalQuarantineTransitionV1 {
  Retained,
  CandidateStarted(PhysicalQuarantineCandidateStateV1),
  CandidateRestarted(PhysicalQuarantineCandidateStateV1),
  CandidateConfirmed(PhysicalQuarantineCandidateStateV1),
  CandidateCleared(PhysicalQuarantineCandidateClearV1),
  SweepEligible(PhysicalSweepIntentV1),
  CapacityDeferred,
  IndeterminateRetained { had_candidate: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalQuarantineTransitionSummaryV1 {
  pub observed_incarnation_count: u64,
  pub observed_prior_candidate_count: u64,
  pub resulting_candidate_count: u64,
  pub resulting_candidate_bytes: u64,
  pub started_count: u64,
  pub restarted_count: u64,
  pub confirmed_count: u64,
  pub cleared_count: u64,
  pub eligible_count: u64,
  pub capacity_deferred_count: u64,
  pub indeterminate_count: u64,
  pub capacity_blocked: bool,
}

#[derive(Debug)]
pub struct PhysicalQuarantineTransitionPublicationPermitV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  prior_manifest_hash: Vec<u8>,
  mark_generation: u64,
  completed_at_ms: u64,
  authority_root_set_digest: Vec<u8>,
  semantic_state_digest: Vec<u8>,
  kv_layout_fingerprint: Vec<u8>,
  mark_result_digest: Vec<u8>,
  captured_root_lifecycle_manifest: Vec<u8>,
  mutation_count: u64,
  mutation_digest: Vec<u8>,
  eligible_intent_digest: Vec<u8>,
  summary: PhysicalQuarantineTransitionSummaryV1,
}

impl PhysicalQuarantineTransitionPublicationPermitV1 {
  pub fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub fn database_id(&self) -> &[u8; 16] {
    &self.database_id
  }

  pub fn prior_manifest_hash(&self) -> &[u8] {
    &self.prior_manifest_hash
  }

  pub fn mark_generation(&self) -> u64 {
    self.mark_generation
  }

  pub fn completed_at_ms(&self) -> u64 {
    self.completed_at_ms
  }

  pub fn authority_root_set_digest(&self) -> &[u8] {
    &self.authority_root_set_digest
  }

  pub fn semantic_state_digest(&self) -> &[u8] {
    &self.semantic_state_digest
  }

  pub fn kv_layout_fingerprint(&self) -> &[u8] {
    &self.kv_layout_fingerprint
  }

  pub fn mark_result_digest(&self) -> &[u8] {
    &self.mark_result_digest
  }

  pub fn captured_root_lifecycle_manifest(&self) -> &[u8] {
    &self.captured_root_lifecycle_manifest
  }

  pub fn mutation_count(&self) -> u64 {
    self.mutation_count
  }

  pub fn mutation_digest(&self) -> &[u8] {
    &self.mutation_digest
  }

  pub fn eligible_intent_digest(&self) -> &[u8] {
    &self.eligible_intent_digest
  }

  pub fn summary(&self) -> PhysicalQuarantineTransitionSummaryV1 {
    self.summary
  }
}

#[derive(Debug, Error)]
pub enum PhysicalQuarantineTransitionErrorV1 {
  #[error("invalid physical-quarantine transition configuration: {0}")]
  InvalidConfiguration(&'static str),
  #[error("physical-quarantine transition requires a complete healthy destructive mark")]
  Unavailable,
  #[error("physical-quarantine transition was canceled")]
  Canceled,
  #[error("physical-quarantine transition has a stale or invalid generation")]
  StaleGeneration,
  #[error("physical-quarantine transition received an invalid incarnation")]
  InvalidIncarnation,
  #[error("physical-quarantine observations are not strictly ordered")]
  RecordOrder,
  #[error("physical-quarantine observation does not match its prior candidate")]
  CandidateIdentityMismatch,
  #[error("physical-quarantine candidate state is invalid")]
  CandidateStateMismatch,
  #[error("physical-quarantine candidate generations do not close against the prior complete mark")]
  CandidateGenerationMismatch,
  #[error("physical-quarantine grace timestamp overflowed")]
  TimeOverflow,
  #[error("physical-quarantine transition exceeded its incarnation limit")]
  RecordLimit,
  #[error("physical-quarantine transition accounting overflowed or underflowed")]
  Arithmetic,
  #[error("physical-quarantine transition integer conversion failed: {0}")]
  ArithmeticConversion(std::num::TryFromIntError),
  #[error("physical-quarantine transition allocation failed: {0}")]
  Allocation(std::collections::TryReserveError),
  #[error("physical-quarantine transition mutation encoding failed: {0}")]
  Format(#[from] FormatError),
  #[error("physical-quarantine transition does not close against its prior manifest")]
  ManifestAggregate,
  #[error("physical-quarantine transition model has already failed")]
  Failed,
}

impl PhysicalQuarantineTransitionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidConfiguration(_) => "physical_quarantine_transition_configuration",
      Self::Unavailable => "physical_quarantine_transition_unavailable",
      Self::Canceled => "physical_quarantine_transition_canceled",
      Self::StaleGeneration | Self::CandidateGenerationMismatch => "physical_quarantine_transition_generation",
      Self::InvalidIncarnation => "physical_quarantine_transition_incarnation",
      Self::RecordOrder => "physical_quarantine_transition_order",
      Self::CandidateIdentityMismatch => "physical_quarantine_transition_candidate_identity",
      Self::CandidateStateMismatch => "physical_quarantine_transition_candidate_state",
      Self::TimeOverflow => "physical_quarantine_transition_time",
      Self::RecordLimit => "physical_quarantine_transition_limit",
      Self::Arithmetic | Self::ArithmeticConversion(_) => "physical_quarantine_transition_arithmetic",
      Self::Allocation(_) => "physical_quarantine_transition_allocation",
      Self::Format(source) => source.code(),
      Self::ManifestAggregate => "physical_quarantine_transition_manifest",
      Self::Failed => "physical_quarantine_transition_failed",
    }
  }
}

#[derive(Debug)]
pub struct PhysicalQuarantineTransitionModelV1<'a> {
  context: PhysicalQuarantineTransitionContextV1<'a>,
  cancellation: &'a CancellationToken,
  candidate_record_bytes: u64,
  observed_incarnation_count: u64,
  observed_prior_candidate_count: u64,
  resulting_candidate_count: u64,
  resulting_candidate_bytes: u64,
  started_count: u64,
  restarted_count: u64,
  confirmed_count: u64,
  cleared_count: u64,
  eligible_count: u64,
  capacity_deferred_count: u64,
  indeterminate_count: u64,
  capacity_blocked: bool,
  previous_incarnation: Option<PhysicalIncarnationStateV1>,
  mutation_count: u64,
  mutation_digest: Vec<u8>,
  eligible_intent_digest: Vec<u8>,
  failed: bool,
}

impl<'a> PhysicalQuarantineTransitionModelV1<'a> {
  pub fn new(
    context: PhysicalQuarantineTransitionContextV1<'a>,
    cancellation: &'a CancellationToken,
  ) -> Result<Self, PhysicalQuarantineTransitionErrorV1> {
    if cancellation.is_cancelled() {
      return Err(PhysicalQuarantineTransitionErrorV1::Canceled);
    }
    if !context.mark_complete
      || !context.destructive_gc_enabled
      || !context.mark_authority_healthy
      || !context.physical_inventory_healthy
      || !context.root_lifecycle_healthy
    {
      return Err(PhysicalQuarantineTransitionErrorV1::Unavailable);
    }
    if context.maximum_incarnations == 0 {
      return Err(PhysicalQuarantineTransitionErrorV1::InvalidConfiguration("maximum incarnations must be nonzero"));
    }
    if context.maximum_candidates == 0 {
      return Err(PhysicalQuarantineTransitionErrorV1::InvalidConfiguration("maximum candidates must be nonzero"));
    }
    if context.completed_at_ms == 0 {
      return Err(PhysicalQuarantineTransitionErrorV1::InvalidConfiguration("complete mark timestamp must be nonzero"));
    }
    if context.hash_algorithm != context.prior_manifest.hash_algorithm {
      return Err(PhysicalQuarantineTransitionErrorV1::ManifestAggregate);
    }
    if context.mark_generation <= context.prior_manifest.mark_generation {
      return Err(PhysicalQuarantineTransitionErrorV1::StaleGeneration);
    }

    let hash_width = context.hash_algorithm.hash_length();
    require_manifest_width(context.prior_manifest.database_id, 16)?;
    require_manifest_width(context.prior_manifest.key.as_slice(), hash_width)?;
    require_manifest_width(context.prior_manifest.authority_root_set_digest, hash_width)?;
    require_manifest_width(context.prior_manifest.semantic_state_digest, hash_width)?;
    require_manifest_width(context.prior_manifest.kv_layout_fingerprint, hash_width)?;
    require_manifest_width(context.prior_manifest.mark_result_digest, hash_width)?;
    require_manifest_width(context.prior_manifest.captured_root_lifecycle_manifest, hash_width)?;
    require_manifest_width(context.authority_root_set_digest, hash_width)?;
    require_manifest_width(context.semantic_state_digest, hash_width)?;
    require_manifest_width(context.kv_layout_fingerprint, hash_width)?;
    require_manifest_width(context.mark_result_digest, hash_width)?;
    require_manifest_width(context.captured_root_lifecycle_manifest, hash_width)?;
    if let Some(candidate_directory_root) = context.prior_manifest.candidate_directory_root {
      require_manifest_width(candidate_directory_root, hash_width)?;
    }
    if context.prior_manifest.candidate_count > context.maximum_incarnations {
      return Err(PhysicalQuarantineTransitionErrorV1::RecordLimit);
    }

    let candidate_record_bytes =
      u64::try_from(52usize + 2 * hash_width).map_err(PhysicalQuarantineTransitionErrorV1::ArithmeticConversion)?;
    let expected_candidate_bytes =
      context.prior_manifest.candidate_count.checked_mul(candidate_record_bytes).ok_or(PhysicalQuarantineTransitionErrorV1::Arithmetic)?;
    if expected_candidate_bytes != context.prior_manifest.candidate_bytes
      || (context.prior_manifest.candidate_count != 0
        && context.prior_manifest.candidate_directory_root.is_none()
        && context.prior_manifest.delta_count == 0)
      || context.prior_manifest.eligible_count_hint > context.prior_manifest.candidate_count
      || context.prior_manifest.eligible_bytes_hint > context.prior_manifest.candidate_bytes
    {
      return Err(PhysicalQuarantineTransitionErrorV1::ManifestAggregate);
    }

    Ok(Self {
      context,
      cancellation,
      candidate_record_bytes,
      observed_incarnation_count: 0,
      observed_prior_candidate_count: 0,
      resulting_candidate_count: context.prior_manifest.candidate_count,
      resulting_candidate_bytes: expected_candidate_bytes,
      started_count: 0,
      restarted_count: 0,
      confirmed_count: 0,
      cleared_count: 0,
      eligible_count: 0,
      capacity_deferred_count: 0,
      indeterminate_count: 0,
      capacity_blocked: false,
      previous_incarnation: None,
      mutation_count: 0,
      mutation_digest: initial_candidate_mutation_digest_v1(context.hash_algorithm),
      eligible_intent_digest: initial_sweep_intent_digest_v1(context.hash_algorithm),
      failed: false,
    })
  }

  pub fn observe(
    &mut self,
    observation: PhysicalQuarantineObservationV1<'_>,
  ) -> Result<PhysicalQuarantineTransitionV1, PhysicalQuarantineTransitionErrorV1> {
    if self.failed {
      return Err(PhysicalQuarantineTransitionErrorV1::Failed);
    }
    match self.observe_inner(observation).and_then(|transition| {
      self.record_mutation(&transition)?;
      Ok(transition)
    }) {
      Ok(transition) => Ok(transition),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<PhysicalQuarantineTransitionSummaryV1, PhysicalQuarantineTransitionErrorV1> {
    self.completion_summary()
  }

  pub fn finish_for_publication(self) -> Result<PhysicalQuarantineTransitionPublicationPermitV1, PhysicalQuarantineTransitionErrorV1> {
    let summary = self.completion_summary()?;
    let mut database_id = [0u8; 16];
    if self.context.prior_manifest.database_id.len() != database_id.len() {
      return Err(PhysicalQuarantineTransitionErrorV1::ManifestAggregate);
    }
    database_id.copy_from_slice(self.context.prior_manifest.database_id);
    Ok(PhysicalQuarantineTransitionPublicationPermitV1 {
      hash_algorithm: self.context.hash_algorithm,
      database_id,
      prior_manifest_hash: try_copy_bytes(&self.context.prior_manifest.key)?,
      mark_generation: self.context.mark_generation,
      completed_at_ms: self.context.completed_at_ms,
      authority_root_set_digest: try_copy_bytes(self.context.authority_root_set_digest)?,
      semantic_state_digest: try_copy_bytes(self.context.semantic_state_digest)?,
      kv_layout_fingerprint: try_copy_bytes(self.context.kv_layout_fingerprint)?,
      mark_result_digest: try_copy_bytes(self.context.mark_result_digest)?,
      captured_root_lifecycle_manifest: try_copy_bytes(self.context.captured_root_lifecycle_manifest)?,
      mutation_count: self.mutation_count,
      mutation_digest: self.mutation_digest,
      eligible_intent_digest: self.eligible_intent_digest,
      summary,
    })
  }

  fn completion_summary(&self) -> Result<PhysicalQuarantineTransitionSummaryV1, PhysicalQuarantineTransitionErrorV1> {
    if self.failed {
      return Err(PhysicalQuarantineTransitionErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(PhysicalQuarantineTransitionErrorV1::Canceled);
    }
    if self.observed_prior_candidate_count != self.context.prior_manifest.candidate_count {
      return Err(PhysicalQuarantineTransitionErrorV1::ManifestAggregate);
    }
    let expected_candidate_bytes =
      self.resulting_candidate_count.checked_mul(self.candidate_record_bytes).ok_or(PhysicalQuarantineTransitionErrorV1::Arithmetic)?;
    if expected_candidate_bytes != self.resulting_candidate_bytes {
      return Err(PhysicalQuarantineTransitionErrorV1::ManifestAggregate);
    }
    Ok(PhysicalQuarantineTransitionSummaryV1 {
      observed_incarnation_count: self.observed_incarnation_count,
      observed_prior_candidate_count: self.observed_prior_candidate_count,
      resulting_candidate_count: self.resulting_candidate_count,
      resulting_candidate_bytes: self.resulting_candidate_bytes,
      started_count: self.started_count,
      restarted_count: self.restarted_count,
      confirmed_count: self.confirmed_count,
      cleared_count: self.cleared_count,
      eligible_count: self.eligible_count,
      capacity_deferred_count: self.capacity_deferred_count,
      indeterminate_count: self.indeterminate_count,
      capacity_blocked: self.capacity_blocked,
    })
  }

  fn observe_inner(
    &mut self,
    observation: PhysicalQuarantineObservationV1<'_>,
  ) -> Result<PhysicalQuarantineTransitionV1, PhysicalQuarantineTransitionErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(PhysicalQuarantineTransitionErrorV1::Canceled);
    }
    if self.observed_incarnation_count >= self.context.maximum_incarnations {
      return Err(PhysicalQuarantineTransitionErrorV1::RecordLimit);
    }
    validate_incarnation(&observation.incarnation, self.context.hash_algorithm.hash_length())?;
    if self
      .previous_incarnation
      .as_ref()
      .is_some_and(|previous| compare_physical_incarnations_v1(&previous.as_borrowed(), &observation.incarnation) != Ordering::Less)
    {
      return Err(PhysicalQuarantineTransitionErrorV1::RecordOrder);
    }
    if let Some(candidate) = observation.prior_candidate {
      if candidate.hash_algorithm != self.context.hash_algorithm
        || compare_physical_incarnations_v1(&candidate.incarnation, &observation.incarnation) != Ordering::Equal
      {
        return Err(PhysicalQuarantineTransitionErrorV1::CandidateIdentityMismatch);
      }
      if candidate.pending_since_ms == 0 || candidate.first_unreachable_generation == 0 {
        return Err(PhysicalQuarantineTransitionErrorV1::CandidateStateMismatch);
      }
      if candidate.first_unreachable_generation > self.context.prior_manifest.mark_generation {
        return Err(PhysicalQuarantineTransitionErrorV1::CandidateGenerationMismatch);
      }
      self.observed_prior_candidate_count = checked_increment(self.observed_prior_candidate_count)?;
    }
    self.observed_incarnation_count = checked_increment(self.observed_incarnation_count)?;
    match &mut self.previous_incarnation {
      Some(previous) => previous.overwrite_from_borrowed(&observation.incarnation)?,
      None => self.previous_incarnation = Some(PhysicalIncarnationStateV1::try_from_borrowed(&observation.incarnation)?),
    }

    match observation.reachability {
      PhysicalQuarantineReachabilityV1::Reachable => self.clear_or_retain(observation.prior_candidate),
      PhysicalQuarantineReachabilityV1::Indeterminate => {
        self.indeterminate_count = checked_increment(self.indeterminate_count)?;
        Ok(PhysicalQuarantineTransitionV1::IndeterminateRetained { had_candidate: observation.prior_candidate.is_some() })
      }
      PhysicalQuarantineReachabilityV1::ConfirmedUnreachable { class } => {
        self.confirm_unreachable(&observation.incarnation, observation.prior_candidate, class)
      }
    }
  }

  fn record_mutation(&mut self, transition: &PhysicalQuarantineTransitionV1) -> Result<(), PhysicalQuarantineTransitionErrorV1> {
    if let PhysicalQuarantineTransitionV1::SweepEligible(intent) = transition {
      self.eligible_intent_digest = extend_sweep_intent_digest_v1(self.context.hash_algorithm, &self.eligible_intent_digest, intent)?;
    }
    let record = match transition {
      PhysicalQuarantineTransitionV1::CandidateStarted(candidate) | PhysicalQuarantineTransitionV1::CandidateRestarted(candidate) => {
        Some(candidate.as_delta_write_request())
      }
      PhysicalQuarantineTransitionV1::CandidateCleared(candidate) => Some(candidate.as_delta_write_request()),
      PhysicalQuarantineTransitionV1::Retained
      | PhysicalQuarantineTransitionV1::CandidateConfirmed(_)
      | PhysicalQuarantineTransitionV1::SweepEligible(_)
      | PhysicalQuarantineTransitionV1::CapacityDeferred
      | PhysicalQuarantineTransitionV1::IndeterminateRetained { .. } => None,
    };
    let Some(record) = record else {
      return Ok(());
    };
    let encoded = encode_candidate_delta_record_v1(&record, self.context.hash_algorithm)?;
    self.mutation_digest = extend_candidate_mutation_digest_v1(self.context.hash_algorithm, &self.mutation_digest, &encoded);
    self.mutation_count = checked_increment(self.mutation_count)?;
    Ok(())
  }

  fn clear_or_retain(
    &mut self,
    candidate: Option<&PhysicalQuarantineCandidateV1<'_>>,
  ) -> Result<PhysicalQuarantineTransitionV1, PhysicalQuarantineTransitionErrorV1> {
    let Some(candidate) = candidate else {
      return Ok(PhysicalQuarantineTransitionV1::Retained);
    };
    self.resulting_candidate_count =
      self.resulting_candidate_count.checked_sub(1).ok_or(PhysicalQuarantineTransitionErrorV1::Arithmetic)?;
    self.resulting_candidate_bytes =
      self.resulting_candidate_bytes.checked_sub(self.candidate_record_bytes).ok_or(PhysicalQuarantineTransitionErrorV1::Arithmetic)?;
    self.cleared_count = checked_increment(self.cleared_count)?;
    Ok(PhysicalQuarantineTransitionV1::CandidateCleared(PhysicalQuarantineCandidateClearV1 {
      hash_algorithm: candidate.hash_algorithm,
      incarnation: PhysicalIncarnationStateV1::try_from_borrowed(&candidate.incarnation)?,
      class: candidate.class,
    }))
  }

  fn confirm_unreachable(
    &mut self,
    incarnation: &PhysicalIncarnationV1<'_>,
    prior_candidate: Option<&PhysicalQuarantineCandidateV1<'_>>,
    class: PhysicalQuarantineCandidateClassV1,
  ) -> Result<PhysicalQuarantineTransitionV1, PhysicalQuarantineTransitionErrorV1> {
    let Some(candidate) = prior_candidate else {
      return self.start_candidate(incarnation, class);
    };
    if candidate.class != class {
      self.restarted_count = checked_increment(self.restarted_count)?;
      return Ok(PhysicalQuarantineTransitionV1::CandidateRestarted(self.new_candidate_state(incarnation, class)?));
    }

    let candidate_state = PhysicalQuarantineCandidateStateV1::try_from_prior(candidate)?;
    let effective_grace_ms = candidate.grace_at_pending_ms.max(self.context.current_configured_grace_ms);
    let eligible_at_ms =
      candidate.pending_since_ms.checked_add(effective_grace_ms).ok_or(PhysicalQuarantineTransitionErrorV1::TimeOverflow)?;
    if self.context.completed_at_ms < eligible_at_ms {
      self.confirmed_count = checked_increment(self.confirmed_count)?;
      return Ok(PhysicalQuarantineTransitionV1::CandidateConfirmed(candidate_state));
    }

    self.eligible_count = checked_increment(self.eligible_count)?;
    Ok(PhysicalQuarantineTransitionV1::SweepEligible(PhysicalSweepIntentV1 {
      candidate: candidate_state,
      confirming_mark_generation: self.context.mark_generation,
      confirmed_at_ms: self.context.completed_at_ms,
      effective_grace_ms,
      eligible_at_ms,
      prior_quarantine_manifest_hash: try_copy_bytes(self.context.prior_manifest.key.as_slice())?,
      authority_root_set_digest: try_copy_bytes(self.context.authority_root_set_digest)?,
      semantic_state_digest: try_copy_bytes(self.context.semantic_state_digest)?,
      kv_layout_fingerprint: try_copy_bytes(self.context.kv_layout_fingerprint)?,
      mark_result_digest: try_copy_bytes(self.context.mark_result_digest)?,
      captured_root_lifecycle_manifest: try_copy_bytes(self.context.captured_root_lifecycle_manifest)?,
    }))
  }

  fn start_candidate(
    &mut self,
    incarnation: &PhysicalIncarnationV1<'_>,
    class: PhysicalQuarantineCandidateClassV1,
  ) -> Result<PhysicalQuarantineTransitionV1, PhysicalQuarantineTransitionErrorV1> {
    if self.capacity_blocked || self.resulting_candidate_count >= self.context.maximum_candidates {
      self.capacity_blocked = true;
      self.capacity_deferred_count = checked_increment(self.capacity_deferred_count)?;
      return Ok(PhysicalQuarantineTransitionV1::CapacityDeferred);
    }
    self.resulting_candidate_count = checked_increment(self.resulting_candidate_count)?;
    self.resulting_candidate_bytes =
      self.resulting_candidate_bytes.checked_add(self.candidate_record_bytes).ok_or(PhysicalQuarantineTransitionErrorV1::Arithmetic)?;
    self.started_count = checked_increment(self.started_count)?;
    Ok(PhysicalQuarantineTransitionV1::CandidateStarted(self.new_candidate_state(incarnation, class)?))
  }

  fn new_candidate_state(
    &self,
    incarnation: &PhysicalIncarnationV1<'_>,
    class: PhysicalQuarantineCandidateClassV1,
  ) -> Result<PhysicalQuarantineCandidateStateV1, PhysicalQuarantineTransitionErrorV1> {
    Ok(PhysicalQuarantineCandidateStateV1 {
      hash_algorithm: self.context.hash_algorithm,
      incarnation: PhysicalIncarnationStateV1::try_from_borrowed(incarnation)?,
      class,
      pending_since_ms: self.context.completed_at_ms,
      first_unreachable_generation: self.context.mark_generation,
      grace_at_pending_ms: self.context.current_configured_grace_ms,
    })
  }
}

pub fn initial_sweep_intent_digest_v1(algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[b"aeordb.physical-sweep-intents.v1\0"])
}

pub fn extend_sweep_intent_digest_v1(
  algorithm: HashAlgorithm,
  previous_digest: &[u8],
  intent: &PhysicalSweepIntentV1,
) -> Result<Vec<u8>, FormatError> {
  let candidate = encode_physical_quarantine_candidate_v1(&intent.candidate.as_write_request())?;
  Ok(digest_parts(
    algorithm,
    &[
      b"aeordb.physical-sweep-intent.v1\0",
      previous_digest,
      &candidate,
      &intent.confirming_mark_generation.to_le_bytes(),
      &intent.confirmed_at_ms.to_le_bytes(),
      &intent.effective_grace_ms.to_le_bytes(),
      &intent.eligible_at_ms.to_le_bytes(),
      &intent.prior_quarantine_manifest_hash,
      &intent.authority_root_set_digest,
      &intent.semantic_state_digest,
      &intent.kv_layout_fingerprint,
      &intent.mark_result_digest,
      &intent.captured_root_lifecycle_manifest,
    ],
  ))
}

fn validate_incarnation(value: &PhysicalIncarnationV1<'_>, hash_width: usize) -> Result<(), PhysicalQuarantineTransitionErrorV1> {
  if !width_is_valid(value.logical_key, hash_width)
    || !width_is_valid(value.integrity_or_legacy_digest, hash_width)
    || value.wal_offset == 0
    || value.entity_length == 0
    || !(1..=0x0a).contains(&value.entry_type)
    || (value.entity_version == 0) != (value.write_sequence == 0)
    || value.wal_offset.checked_add(u64::from(value.entity_length)).is_none()
  {
    return Err(PhysicalQuarantineTransitionErrorV1::InvalidIncarnation);
  }
  Ok(())
}

fn checked_increment(value: u64) -> Result<u64, PhysicalQuarantineTransitionErrorV1> {
  value.checked_add(1).ok_or(PhysicalQuarantineTransitionErrorV1::Arithmetic)
}

fn width_is_valid(value: &[u8], expected_width: usize) -> bool {
  value.len() == expected_width && value.iter().any(|byte| *byte != 0)
}

fn require_manifest_width(value: &[u8], expected_width: usize) -> Result<(), PhysicalQuarantineTransitionErrorV1> {
  if !width_is_valid(value, expected_width) {
    return Err(PhysicalQuarantineTransitionErrorV1::ManifestAggregate);
  }
  Ok(())
}

fn try_copy_bytes(source: &[u8]) -> Result<Vec<u8>, PhysicalQuarantineTransitionErrorV1> {
  let mut destination = Vec::new();
  destination.try_reserve_exact(source.len()).map_err(PhysicalQuarantineTransitionErrorV1::Allocation)?;
  destination.extend_from_slice(source);
  Ok(destination)
}
