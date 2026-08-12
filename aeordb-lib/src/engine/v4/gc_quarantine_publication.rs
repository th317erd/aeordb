//! Qualification for selector-last physical-quarantine publication.
//!
//! This unit binds a completed transition to one exact incremental candidate
//! delta and one exact validated support closure. It does not publish authority
//! or remove physical incarnations.

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc_quarantine::{
  CandidateDeltaRecordWriteV1, QuarantineClosureSummaryV1, QuarantineManifestV1, decode_candidate_delta_v1,
  encode_candidate_delta_record_v1, extend_candidate_mutation_digest_v1, initial_candidate_mutation_digest_v1,
};
use super::gc_quarantine_transition::{PhysicalQuarantineTransitionPublicationPermitV1, PhysicalQuarantineTransitionSummaryV1};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;

#[derive(Clone, Copy)]
pub struct PhysicalQuarantinePublicationQualificationRequestV1<'a> {
  pub prior_manifest: &'a QuarantineManifestV1<'a>,
  pub next_manifest: &'a QuarantineManifestV1<'a>,
  pub support_closure: &'a QuarantineClosureSummaryV1,
  pub transition: &'a PhysicalQuarantineTransitionPublicationPermitV1,
  pub appended_delta: Option<&'a [u8]>,
  pub cancellation: &'a CancellationToken,
}

#[derive(Debug)]
pub struct PhysicalQuarantinePublicationPermitV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  prior_manifest_hash: Vec<u8>,
  next_manifest_hash: Vec<u8>,
  mark_generation: u64,
  mutation_count: u64,
  resulting_candidate_count: u64,
  resulting_candidate_bytes: u64,
  eligible_count: u64,
  eligible_intent_digest: Vec<u8>,
  support_closure: QuarantineClosureSummaryV1,
}

impl PhysicalQuarantinePublicationPermitV1 {
  pub fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub fn database_id(&self) -> &[u8; 16] {
    &self.database_id
  }

  pub fn prior_manifest_hash(&self) -> &[u8] {
    &self.prior_manifest_hash
  }

  pub fn next_manifest_hash(&self) -> &[u8] {
    &self.next_manifest_hash
  }

  pub fn mark_generation(&self) -> u64 {
    self.mark_generation
  }

  pub fn mutation_count(&self) -> u64 {
    self.mutation_count
  }

  pub fn resulting_candidate_count(&self) -> u64 {
    self.resulting_candidate_count
  }

  pub fn resulting_candidate_bytes(&self) -> u64 {
    self.resulting_candidate_bytes
  }

  pub fn eligible_count(&self) -> u64 {
    self.eligible_count
  }

  pub fn eligible_intent_digest(&self) -> &[u8] {
    &self.eligible_intent_digest
  }

  pub fn support_closure(&self) -> &QuarantineClosureSummaryV1 {
    &self.support_closure
  }
}

#[derive(Debug, Error)]
pub enum PhysicalQuarantinePublicationQualificationErrorV1 {
  #[error("physical-quarantine publication qualification was canceled")]
  Canceled,
  #[error("physical-quarantine publication identity or generation differs from the completed transition")]
  Identity,
  #[error("physical-quarantine publication changed the compacted base or prior delta chain")]
  NonIncremental,
  #[error("physical-quarantine publication delta does not exactly match emitted transition mutations")]
  MutationMismatch,
  #[error("physical-quarantine publication manifest totals differ from the completed transition")]
  AggregateMismatch,
  #[error("physical-quarantine publication support closure differs from its manifest")]
  ClosureMismatch,
  #[error("physical-quarantine publication accounting overflowed")]
  Arithmetic,
  #[error("physical-quarantine publication allocation failed: {0}")]
  Allocation(#[from] std::collections::TryReserveError),
  #[error(transparent)]
  Format(#[from] FormatError),
}

impl PhysicalQuarantinePublicationQualificationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "quarantine_publication_canceled",
      Self::Identity => "quarantine_publication_identity",
      Self::NonIncremental => "quarantine_publication_nonincremental",
      Self::MutationMismatch => "quarantine_publication_mutations",
      Self::AggregateMismatch => "quarantine_publication_aggregates",
      Self::ClosureMismatch => "quarantine_publication_closure",
      Self::Arithmetic => "quarantine_publication_arithmetic",
      Self::Allocation(_) => "quarantine_publication_allocation",
      Self::Format(source) => source.code(),
    }
  }
}

pub fn qualify_physical_quarantine_publication_v1(
  request: PhysicalQuarantinePublicationQualificationRequestV1<'_>,
) -> Result<PhysicalQuarantinePublicationPermitV1, PhysicalQuarantinePublicationQualificationErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::Canceled);
  }
  validate_identity(&request)?;
  validate_incremental_support(&request)?;
  let summary = request.transition.summary();
  validate_aggregates(&request, summary)?;
  validate_mutations(&request)?;
  if request.cancellation.is_cancelled() {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::Canceled);
  }
  Ok(PhysicalQuarantinePublicationPermitV1 {
    hash_algorithm: request.transition.hash_algorithm(),
    database_id: *request.transition.database_id(),
    prior_manifest_hash: try_copy_bytes(request.transition.prior_manifest_hash())?,
    next_manifest_hash: try_copy_bytes(&request.next_manifest.key)?,
    mark_generation: request.transition.mark_generation(),
    mutation_count: request.transition.mutation_count(),
    resulting_candidate_count: summary.resulting_candidate_count,
    resulting_candidate_bytes: summary.resulting_candidate_bytes,
    eligible_count: summary.eligible_count,
    eligible_intent_digest: try_copy_bytes(request.transition.eligible_intent_digest())?,
    support_closure: request.support_closure.clone(),
  })
}

fn validate_identity(
  request: &PhysicalQuarantinePublicationQualificationRequestV1<'_>,
) -> Result<(), PhysicalQuarantinePublicationQualificationErrorV1> {
  let transition = request.transition;
  let next = request.next_manifest;
  if transition.hash_algorithm() != request.prior_manifest.hash_algorithm
    || transition.hash_algorithm() != next.hash_algorithm
    || transition.database_id().as_slice() != request.prior_manifest.database_id
    || transition.database_id().as_slice() != next.database_id
    || transition.prior_manifest_hash() != request.prior_manifest.key
    || transition.mark_generation() != next.mark_generation
    || transition.completed_at_ms() != next.completed_at_ms
    || transition.authority_root_set_digest() != next.authority_root_set_digest
    || transition.semantic_state_digest() != next.semantic_state_digest
    || transition.kv_layout_fingerprint() != next.kv_layout_fingerprint
    || transition.mark_result_digest() != next.mark_result_digest
    || transition.captured_root_lifecycle_manifest() != next.captured_root_lifecycle_manifest
  {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::Identity);
  }
  Ok(())
}

fn validate_incremental_support(
  request: &PhysicalQuarantinePublicationQualificationRequestV1<'_>,
) -> Result<(), PhysicalQuarantinePublicationQualificationErrorV1> {
  let prior = request.prior_manifest;
  let next = request.next_manifest;
  if next.candidate_directory_root != prior.candidate_directory_root
    || next.next_candidate_page_id != prior.next_candidate_page_id
    || !next.delta_hashes.starts_with(prior.delta_hashes)
  {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::NonIncremental);
  }
  let hash_width = next.hash_algorithm.hash_length();
  let appended_hashes = &next.delta_hashes[prior.delta_hashes.len()..];
  let expected_appended_hash_count = usize::from(request.transition.mutation_count() != 0);
  if appended_hashes.len() != expected_appended_hash_count * hash_width {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::NonIncremental);
  }
  if request.support_closure.manifest_key() != next.key
    || request.support_closure.declared_candidate_count != next.candidate_count
    || request.support_closure.declared_candidate_bytes != next.candidate_bytes
    || request.support_closure.delta_count != u64::from(next.delta_count)
  {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::ClosureMismatch);
  }
  Ok(())
}

fn validate_aggregates(
  request: &PhysicalQuarantinePublicationQualificationRequestV1<'_>,
  summary: PhysicalQuarantineTransitionSummaryV1,
) -> Result<(), PhysicalQuarantinePublicationQualificationErrorV1> {
  let record_bytes = match request.next_manifest.hash_algorithm {
    HashAlgorithm::Blake3_256 | HashAlgorithm::Sha256 | HashAlgorithm::Sha3_256 => 116,
    HashAlgorithm::Sha512 | HashAlgorithm::Sha3_512 => 180,
  };
  let eligible_bytes =
    summary.eligible_count.checked_mul(record_bytes).ok_or(PhysicalQuarantinePublicationQualificationErrorV1::Arithmetic)?;
  if request.next_manifest.candidate_count != summary.resulting_candidate_count
    || request.next_manifest.candidate_bytes != summary.resulting_candidate_bytes
    || request.next_manifest.eligible_count_hint != summary.eligible_count
    || request.next_manifest.eligible_bytes_hint != eligible_bytes
  {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::AggregateMismatch);
  }
  Ok(())
}

fn validate_mutations(
  request: &PhysicalQuarantinePublicationQualificationRequestV1<'_>,
) -> Result<(), PhysicalQuarantinePublicationQualificationErrorV1> {
  if request.transition.mutation_count() == 0 {
    if request.appended_delta.is_some() {
      return Err(PhysicalQuarantinePublicationQualificationErrorV1::MutationMismatch);
    }
    return Ok(());
  }
  let appended_delta = request.appended_delta.ok_or(PhysicalQuarantinePublicationQualificationErrorV1::MutationMismatch)?;
  let decoded = decode_candidate_delta_v1(appended_delta, request.transition.hash_algorithm())?;
  let hash_width = request.transition.hash_algorithm().hash_length();
  let appended_hash = request
    .next_manifest
    .delta_hashes
    .get(request.prior_manifest.delta_hashes.len()..request.prior_manifest.delta_hashes.len() + hash_width)
    .ok_or(PhysicalQuarantinePublicationQualificationErrorV1::MutationMismatch)?;
  if decoded.key != appended_hash
    || decoded.mark_generation != request.transition.mark_generation()
    || u64::from(decoded.record_count) != request.transition.mutation_count()
  {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::MutationMismatch);
  }
  let mut digest = initial_candidate_mutation_digest_v1(request.transition.hash_algorithm());
  for record in decoded.records()? {
    if request.cancellation.is_cancelled() {
      return Err(PhysicalQuarantinePublicationQualificationErrorV1::Canceled);
    }
    let record = record?;
    let encoded = encode_candidate_delta_record_v1(&CandidateDeltaRecordWriteV1::from(&record), request.transition.hash_algorithm())?;
    digest = extend_candidate_mutation_digest_v1(request.transition.hash_algorithm(), &digest, &encoded);
  }
  if digest != request.transition.mutation_digest() {
    return Err(PhysicalQuarantinePublicationQualificationErrorV1::MutationMismatch);
  }
  Ok(())
}

fn try_copy_bytes(source: &[u8]) -> Result<Vec<u8>, PhysicalQuarantinePublicationQualificationErrorV1> {
  let mut destination = Vec::new();
  destination.try_reserve_exact(source.len())?;
  destination.extend_from_slice(source);
  Ok(destination)
}
