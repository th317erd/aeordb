//! Qualification for bounded, non-authoritative sweep proposals.
//!
//! A qualified proposal proves that its exact physical incarnations were
//! emitted by the transition that produced one selected-quarantine candidate
//! set. It does not authorize locator removal or reusable Void publication.

use std::cmp::Ordering;
use std::num::TryFromIntError;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::{EncodedImmutableGcArtifactV1, compare_physical_incarnations_v1};
use super::gc_quarantine::QuarantineManifestV1;
use super::gc_quarantine_publication::PhysicalQuarantinePublicationPermitV1;
use super::gc_quarantine_transition::{PhysicalSweepIntentV1, extend_sweep_intent_digest_v1, initial_sweep_intent_digest_v1};
use super::gc_void::{SweepProposalWriteV1, encode_sweep_proposal_v1};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;

pub const MAXIMUM_SWEEP_PROPOSAL_CANDIDATES_V1: usize = 4_096;

#[derive(Clone, Copy)]
pub struct SweepProposalQualificationRequestV1<'a> {
  pub quarantine_publication: &'a PhysicalQuarantinePublicationPermitV1,
  pub quarantine_manifest: &'a QuarantineManifestV1<'a>,
  pub batch_id: &'a [u8; 16],
  pub created_at_ms: i64,
  pub intents: &'a [PhysicalSweepIntentV1],
  pub cancellation: &'a CancellationToken,
}

#[derive(Debug)]
pub struct SweepProposalPublicationPermitV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  batch_id: [u8; 16],
  quarantine_manifest_hash: Vec<u8>,
  generation: u64,
  candidate_count: u32,
  eligible_intent_digest: Vec<u8>,
  proposal: EncodedImmutableGcArtifactV1,
}

impl SweepProposalPublicationPermitV1 {
  pub fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub fn database_id(&self) -> &[u8; 16] {
    &self.database_id
  }

  pub fn batch_id(&self) -> &[u8; 16] {
    &self.batch_id
  }

  pub fn quarantine_manifest_hash(&self) -> &[u8] {
    &self.quarantine_manifest_hash
  }

  pub fn generation(&self) -> u64 {
    self.generation
  }

  pub fn candidate_count(&self) -> u32 {
    self.candidate_count
  }

  pub fn eligible_intent_digest(&self) -> &[u8] {
    &self.eligible_intent_digest
  }

  pub fn proposal(&self) -> &EncodedImmutableGcArtifactV1 {
    &self.proposal
  }
}

#[derive(Debug, Error)]
pub enum SweepProposalQualificationErrorV1 {
  #[error("sweep proposal qualification was canceled")]
  Canceled,
  #[error("sweep proposal identity differs from selected quarantine authority")]
  Identity,
  #[error("sweep proposal candidate aggregates differ from selected quarantine authority")]
  Aggregate,
  #[error("sweep proposal candidate input exceeds its bounded limit")]
  RecordLimit,
  #[error("sweep proposal candidate count is outside its persisted range")]
  RecordCount(#[source] TryFromIntError),
  #[error("sweep proposal candidates are duplicate or out of physical order")]
  RecordOrder,
  #[error("sweep proposal contains an invalid or stale eligible intent")]
  Intent,
  #[error("sweep proposal eligible-intent digest differs from the completed transition")]
  IntentDigest,
  #[error("sweep proposal time arithmetic overflowed")]
  Time,
  #[error("sweep proposal creation time is outside its persisted range")]
  CreationTime(#[source] TryFromIntError),
  #[error("sweep proposal candidate width is outside its aggregate range")]
  RecordWidth(#[source] TryFromIntError),
  #[error("sweep proposal allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
  #[error(transparent)]
  Format(#[from] FormatError),
}

impl SweepProposalQualificationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "sweep_proposal_canceled",
      Self::Identity => "sweep_proposal_identity",
      Self::Aggregate => "sweep_proposal_aggregate",
      Self::RecordLimit => "sweep_proposal_limit",
      Self::RecordCount(_) => "sweep_proposal_limit",
      Self::RecordOrder => "sweep_proposal_order",
      Self::Intent => "sweep_proposal_intent",
      Self::IntentDigest => "sweep_proposal_intent_digest",
      Self::Time => "sweep_proposal_time",
      Self::CreationTime(_) => "sweep_proposal_time",
      Self::RecordWidth(_) => "sweep_proposal_aggregate",
      Self::Allocation(_) => "sweep_proposal_allocation",
      Self::Format(source) => source.code(),
    }
  }
}

pub fn qualify_sweep_proposal_v1(
  request: SweepProposalQualificationRequestV1<'_>,
) -> Result<SweepProposalPublicationPermitV1, SweepProposalQualificationErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(SweepProposalQualificationErrorV1::Canceled);
  }
  validate_authority(&request)?;
  if request.intents.is_empty() || request.intents.len() > MAXIMUM_SWEEP_PROPOSAL_CANDIDATES_V1 {
    return Err(SweepProposalQualificationErrorV1::RecordLimit);
  }
  let candidate_count = match u32::try_from(request.intents.len()) {
    Ok(candidate_count) => candidate_count,
    Err(source) => return Err(SweepProposalQualificationErrorV1::RecordCount(source)),
  };
  let intent_count = u64::from(candidate_count);
  if intent_count != request.quarantine_publication.eligible_count() || intent_count != request.quarantine_manifest.eligible_count_hint {
    return Err(SweepProposalQualificationErrorV1::Aggregate);
  }

  let mut candidates = Vec::new();
  candidates.try_reserve_exact(request.intents.len())?;
  let mut previous = None;
  let mut intent_digest = initial_sweep_intent_digest_v1(request.quarantine_manifest.hash_algorithm);
  for intent in request.intents {
    if request.cancellation.is_cancelled() {
      return Err(SweepProposalQualificationErrorV1::Canceled);
    }
    validate_intent(&request, intent)?;
    let incarnation = intent.candidate.incarnation.as_borrowed();
    if previous.is_some_and(|prior| compare_physical_incarnations_v1(&prior, &incarnation) != Ordering::Less) {
      return Err(SweepProposalQualificationErrorV1::RecordOrder);
    }
    intent_digest = extend_sweep_intent_digest_v1(request.quarantine_manifest.hash_algorithm, &intent_digest, intent)?;
    previous = Some(incarnation);
    candidates.push(incarnation);
  }
  if intent_digest != request.quarantine_publication.eligible_intent_digest() {
    return Err(SweepProposalQualificationErrorV1::IntentDigest);
  }
  if request.cancellation.is_cancelled() {
    return Err(SweepProposalQualificationErrorV1::Canceled);
  }

  let proposal = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
    hash_algorithm: request.quarantine_manifest.hash_algorithm,
    database_id: request.quarantine_publication.database_id(),
    batch_id: request.batch_id,
    generation: request.quarantine_manifest.mark_generation,
    created_at_ms: request.created_at_ms,
    quarantine_manifest_hash: &request.quarantine_manifest.key,
    candidates: &candidates,
  })?;
  Ok(SweepProposalPublicationPermitV1 {
    hash_algorithm: request.quarantine_manifest.hash_algorithm,
    database_id: *request.quarantine_publication.database_id(),
    batch_id: *request.batch_id,
    quarantine_manifest_hash: request.quarantine_manifest.key.clone(),
    generation: request.quarantine_manifest.mark_generation,
    candidate_count,
    eligible_intent_digest: intent_digest,
    proposal,
  })
}

fn validate_authority(request: &SweepProposalQualificationRequestV1<'_>) -> Result<(), SweepProposalQualificationErrorV1> {
  let manifest = request.quarantine_manifest;
  let publication = request.quarantine_publication;
  let created_at_ms = match u64::try_from(request.created_at_ms) {
    Ok(created_at_ms) => created_at_ms,
    Err(source) => return Err(SweepProposalQualificationErrorV1::CreationTime(source)),
  };
  if request.batch_id.iter().all(|byte| *byte == 0)
    || publication.hash_algorithm() != manifest.hash_algorithm
    || publication.database_id().as_slice() != manifest.database_id
    || publication.next_manifest_hash() != manifest.key
    || publication.mark_generation() != manifest.mark_generation
    || created_at_ms < manifest.completed_at_ms
  {
    return Err(SweepProposalQualificationErrorV1::Identity);
  }
  let hash_width = manifest.hash_algorithm.hash_length();
  let record_bytes = match u64::try_from(52usize + 2 * hash_width) {
    Ok(record_bytes) => record_bytes,
    Err(source) => return Err(SweepProposalQualificationErrorV1::RecordWidth(source)),
  };
  if manifest.eligible_count_hint.checked_mul(record_bytes) != Some(manifest.eligible_bytes_hint)
    || publication.eligible_intent_digest().len() != hash_width
    || publication.eligible_intent_digest().iter().all(|byte| *byte == 0)
  {
    return Err(SweepProposalQualificationErrorV1::Aggregate);
  }
  Ok(())
}

fn validate_intent(
  request: &SweepProposalQualificationRequestV1<'_>,
  intent: &PhysicalSweepIntentV1,
) -> Result<(), SweepProposalQualificationErrorV1> {
  let manifest = request.quarantine_manifest;
  let publication = request.quarantine_publication;
  let eligible_at_ms =
    intent.candidate.pending_since_ms.checked_add(intent.effective_grace_ms).ok_or(SweepProposalQualificationErrorV1::Time)?;
  if intent.candidate.hash_algorithm != manifest.hash_algorithm
    || intent.candidate.pending_since_ms == 0
    || intent.candidate.first_unreachable_generation == 0
    || intent.candidate.first_unreachable_generation >= intent.confirming_mark_generation
    || intent.effective_grace_ms < intent.candidate.grace_at_pending_ms
    || intent.eligible_at_ms != eligible_at_ms
    || intent.confirmed_at_ms < intent.eligible_at_ms
    || intent.confirming_mark_generation != manifest.mark_generation
    || intent.confirmed_at_ms != manifest.completed_at_ms
    || intent.prior_quarantine_manifest_hash != publication.prior_manifest_hash()
    || intent.authority_root_set_digest != manifest.authority_root_set_digest
    || intent.semantic_state_digest != manifest.semantic_state_digest
    || intent.kv_layout_fingerprint != manifest.kv_layout_fingerprint
    || intent.mark_result_digest != manifest.mark_result_digest
    || intent.captured_root_lifecycle_manifest != manifest.captured_root_lifecycle_manifest
  {
    return Err(SweepProposalQualificationErrorV1::Intent);
  }
  Ok(())
}
