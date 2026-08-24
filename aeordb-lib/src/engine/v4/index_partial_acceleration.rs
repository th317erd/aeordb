//! Exact, storage-neutral execution contract for partial index acceleration.
//!
//! An older immutable generation is never result authority by itself. This
//! module returns an exact result only after a bounded candidate scan, an exact
//! changed-document complement, selected-root rechecks, and FileKey/revision
//! deduplication all complete. Every other nonfatal path returns an explicit
//! authoritative-only fallback without exposing partial matches.

use std::error::Error;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::hash::IncrementalDigestV1;
use super::index_coverage_planner::{IndexCoverageGenerationV1, IndexCoveragePlanV1};

const CHANGED_SET_DOMAIN_V1: &[u8] = b"aeordb:index-exact-changed-document-set:v1\0";
const EXECUTION_BASE_ALLOWANCE: u64 = 4 * 1_024;
const OWNED_ROW_ALLOWANCE: u64 = 128;
const OUTPUT_ROW_ALLOWANCE: u64 = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexPartialAccelerationLimitsV1 {
  maximum_changed_documents: u64,
  maximum_accelerator_candidates: u64,
  maximum_matches: u64,
  maximum_retained_bytes: u64,
}

impl IndexPartialAccelerationLimitsV1 {
  pub fn new(
    maximum_changed_documents: u64,
    maximum_accelerator_candidates: u64,
    maximum_matches: u64,
    maximum_retained_bytes: u64,
  ) -> Result<Self, IndexPartialAccelerationErrorV1> {
    if maximum_changed_documents == 0 || maximum_accelerator_candidates == 0 || maximum_matches == 0 || maximum_retained_bytes == 0 {
      return Err(IndexPartialAccelerationErrorV1::invalid(
        "index_partial_limits",
        "partial-acceleration document, candidate, match, and retained-byte limits must be nonzero",
      ));
    }
    Ok(Self { maximum_changed_documents, maximum_accelerator_candidates, maximum_matches, maximum_retained_bytes })
  }

  pub const fn maximum_changed_documents(self) -> u64 {
    self.maximum_changed_documents
  }

  pub const fn maximum_accelerator_candidates(self) -> u64 {
    self.maximum_accelerator_candidates
  }

  pub const fn maximum_matches(self) -> u64 {
    self.maximum_matches
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexPartialSourceErrorClassV1 {
  Unavailable,
  ResourceLimit,
  Corrupt,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPartialSourceErrorV1 {
  class: IndexPartialSourceErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexPartialSourceErrorV1 {
  pub fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialSourceErrorClassV1::Unavailable, code, context: context.into() }
  }

  pub fn resource_limit(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialSourceErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialSourceErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialSourceErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn internal(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialSourceErrorClassV1::Internal, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexPartialSourceErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for IndexPartialSourceErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for IndexPartialSourceErrorV1 {}

#[derive(Debug)]
pub enum IndexPartialScanErrorV1 {
  Source(IndexPartialSourceErrorV1),
  Visitor(IndexPartialAccelerationErrorV1),
}

impl From<IndexPartialSourceErrorV1> for IndexPartialScanErrorV1 {
  fn from(source: IndexPartialSourceErrorV1) -> Self {
    Self::Source(source)
  }
}

impl From<IndexPartialAccelerationErrorV1> for IndexPartialScanErrorV1 {
  fn from(source: IndexPartialAccelerationErrorV1) -> Self {
    Self::Visitor(source)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexAcceleratorCandidateV1<'a> {
  pub file_key: &'a [u8],
  pub indexed_revision_hash: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexChangedDocumentV1<'a> {
  pub file_key: &'a [u8],
  pub basis_revision_hash: Option<&'a [u8]>,
  pub target_revision_hash: Option<&'a [u8]>,
}

pub trait IndexAcceleratorCandidateVisitorV1 {
  fn visit(&mut self, candidate: IndexAcceleratorCandidateV1<'_>) -> Result<(), IndexPartialAccelerationErrorV1>;
}

pub trait IndexChangedDocumentVisitorV1 {
  fn visit(&mut self, document: IndexChangedDocumentV1<'_>) -> Result<(), IndexPartialAccelerationErrorV1>;
}

#[derive(Clone, Copy, Debug)]
pub struct IndexAcceleratorCandidateScanRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub generation_manifest_hash: &'a [u8],
  pub source_namespace_root: &'a [u8],
  pub query_fingerprint: &'a [u8],
  pub maximum_candidates: u64,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexAcceleratorCandidateScanReceiptV1 {
  pub generation: u64,
  pub generation_manifest_hash: Vec<u8>,
  pub source_namespace_root: Vec<u8>,
  pub query_fingerprint: Vec<u8>,
  pub candidate_count: u64,
  pub complete: bool,
}

pub trait IndexAcceleratorCandidateSourceV1 {
  /// Stream every exact Posting-directory candidate for the selected
  /// generation and query without retaining an unaccounted aggregate. An NVT
  /// lookup may supply a validated starting hint, but it is never authority:
  /// absent, stale, corrupt, or resource-limited NVT state must fall back to
  /// exact Posting traversal before this source reports `complete`.
  /// The source must stop at its admitted bound and return `ResourceLimit`;
  /// emitting past that bound is treated as corrupt disposable acceleration.
  ///
  /// Any source-owned page or receipt allocation remains the source's memory
  /// responsibility until this call returns.
  fn scan_candidates(
    &mut self,
    request: IndexAcceleratorCandidateScanRequestV1<'_>,
    visitor: &mut dyn IndexAcceleratorCandidateVisitorV1,
  ) -> Result<IndexAcceleratorCandidateScanReceiptV1, IndexPartialScanErrorV1>;
}

#[derive(Clone, Copy, Debug)]
pub struct IndexChangedDocumentScanRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub generation_manifest_hash: &'a [u8],
  pub source_namespace_root: &'a [u8],
  pub target_namespace_root: &'a [u8],
  pub covered_through_publication_sequence: u64,
  pub target_publication_sequence: u64,
  pub maximum_changed_documents: u64,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexChangedDocumentScanReceiptV1 {
  pub source_namespace_root: Vec<u8>,
  pub target_namespace_root: Vec<u8>,
  pub covered_through_publication_sequence: u64,
  pub target_publication_sequence: u64,
  pub changed_document_count: u64,
  pub complete: bool,
}

pub trait IndexChangedDocumentSourceV1 {
  /// Stream the exact immutable-root diff in strict FileKey order. The source
  /// must stop at its admitted bound and return `ResourceLimit`; emitting past
  /// that bound is treated as corrupt authority.
  fn scan_changed_documents(
    &mut self,
    request: IndexChangedDocumentScanRequestV1<'_>,
    visitor: &mut dyn IndexChangedDocumentVisitorV1,
  ) -> Result<IndexChangedDocumentScanReceiptV1, IndexPartialScanErrorV1>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexPartialRecheckOriginV1 {
  AcceleratorCandidate,
  ChangedDocumentComplement,
}

#[derive(Clone, Copy, Debug)]
pub struct IndexPartialRecheckRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub target_namespace_root: &'a [u8],
  pub target_publication_sequence: u64,
  pub query_fingerprint: &'a [u8],
  pub file_key: &'a [u8],
  pub basis_revision_hash: Option<&'a [u8]>,
  pub expected_target_revision_hash: Option<&'a [u8]>,
  pub origin: IndexPartialRecheckOriginV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexPartialRecheckOutcomeV1 {
  Absent,
  Present { record_revision_hash: Vec<u8>, matches: bool },
}

pub trait IndexPartialCandidateRecheckerV1 {
  fn recheck(&mut self, request: IndexPartialRecheckRequestV1<'_>) -> Result<IndexPartialRecheckOutcomeV1, IndexPartialSourceErrorV1>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexPartialAccelerationStageV1 {
  Local,
  CandidateSource,
  ComplementSource,
  Recheck,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexPartialAccelerationFallbackReasonV1 {
  LocalResourceLimit,
  CandidateUnavailable,
  CandidateResourceLimit,
  CandidateCorrupt,
  ComplementUnavailable,
  ComplementResourceLimit,
  RecheckUnavailable,
  RecheckResourceLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPartialAccelerationDiagnosticV1 {
  pub stage: IndexPartialAccelerationStageV1,
  pub class: IndexPartialSourceErrorClassV1,
  pub code: &'static str,
  context: String,
}

impl IndexPartialAccelerationDiagnosticV1 {
  pub fn context(&self) -> &str {
    &self.context
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexPartialAccelerationErrorClassV1 {
  InvalidRequest,
  LocalResourceLimit,
  CorruptAuthoritativeComplement,
  CorruptAuthoritativeRecheck,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPartialAccelerationErrorV1 {
  class: IndexPartialAccelerationErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexPartialAccelerationErrorV1 {
  pub const fn class(&self) -> IndexPartialAccelerationErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }

  pub(crate) fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialAccelerationErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  fn local_resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialAccelerationErrorClassV1::LocalResourceLimit, code, context: context.into() }
  }

  fn corrupt_complement(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeComplement, code, context: context.into() }
  }

  fn corrupt_recheck(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeRecheck, code, context: context.into() }
  }

  fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialAccelerationErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub(crate) fn internal(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexPartialAccelerationErrorClassV1::Internal, code, context: context.into() }
  }
}

impl fmt::Display for IndexPartialAccelerationErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for IndexPartialAccelerationErrorV1 {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexMatchedDocumentIdentityV1 {
  file_key: Vec<u8>,
  record_revision_hash: Vec<u8>,
}

impl IndexMatchedDocumentIdentityV1 {
  pub fn file_key(&self) -> &[u8] {
    &self.file_key
  }

  pub fn record_revision_hash(&self) -> &[u8] {
    &self.record_revision_hash
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactIndexComplementProofV1 {
  hash_algorithm: HashAlgorithm,
  hashes: Vec<u8>,
  coverage_epoch_id: [u8; 16],
  covered_through_publication_sequence: u64,
  target_publication_sequence: u64,
  changed_document_count: u64,
}

impl ExactIndexComplementProofV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub fn generation_manifest_hash(&self) -> &[u8] {
    self.hash(0)
  }

  pub fn source_namespace_root(&self) -> &[u8] {
    self.hash(1)
  }

  pub fn target_namespace_root(&self) -> &[u8] {
    self.hash(2)
  }

  pub fn query_fingerprint(&self) -> &[u8] {
    self.hash(3)
  }

  pub const fn coverage_epoch_id(&self) -> &[u8; 16] {
    &self.coverage_epoch_id
  }

  pub const fn covered_through_publication_sequence(&self) -> u64 {
    self.covered_through_publication_sequence
  }

  pub const fn target_publication_sequence(&self) -> u64 {
    self.target_publication_sequence
  }

  pub fn changed_document_set_hash(&self) -> &[u8] {
    self.hash(4)
  }

  pub const fn changed_document_count(&self) -> u64 {
    self.changed_document_count
  }

  fn hash(&self, slot: usize) -> &[u8] {
    let width = self.hash_algorithm.hash_length();
    let start = slot * width;
    &self.hashes[start..start + width]
  }
}

pub struct ExactPartialIndexAccelerationV1 {
  proof: ExactIndexComplementProofV1,
  matches: Vec<IndexMatchedDocumentIdentityV1>,
  observed_candidate_count: u64,
  unique_candidate_count: u64,
  rechecked_candidate_count: u64,
  overlap_deduplicated_count: u64,
  retained_bytes: u64,
  _reservation: MemoryReservation,
}

impl fmt::Debug for ExactPartialIndexAccelerationV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ExactPartialIndexAccelerationV1")
      .field("proof", &self.proof)
      .field("matches", &self.matches)
      .field("observed_candidate_count", &self.observed_candidate_count)
      .field("unique_candidate_count", &self.unique_candidate_count)
      .field("rechecked_candidate_count", &self.rechecked_candidate_count)
      .field("overlap_deduplicated_count", &self.overlap_deduplicated_count)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl ExactPartialIndexAccelerationV1 {
  pub const fn proof(&self) -> &ExactIndexComplementProofV1 {
    &self.proof
  }

  pub fn matches(&self) -> &[IndexMatchedDocumentIdentityV1] {
    &self.matches
  }

  pub const fn observed_candidate_count(&self) -> u64 {
    self.observed_candidate_count
  }

  pub const fn unique_candidate_count(&self) -> u64 {
    self.unique_candidate_count
  }

  pub const fn rechecked_candidate_count(&self) -> u64 {
    self.rechecked_candidate_count
  }

  pub const fn overlap_deduplicated_count(&self) -> u64 {
    self.overlap_deduplicated_count
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

#[derive(Debug)]
pub enum IndexPartialAccelerationOutcomeV1 {
  Exact(ExactPartialIndexAccelerationV1),
  AuthoritativeOnly { reason: IndexPartialAccelerationFallbackReasonV1, diagnostic: IndexPartialAccelerationDiagnosticV1 },
}

pub struct IndexPartialAccelerationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub plan: &'a IndexCoveragePlanV1<'a>,
  pub query_fingerprint: &'a [u8],
  pub candidates: &'a mut dyn IndexAcceleratorCandidateSourceV1,
  pub complement: &'a mut dyn IndexChangedDocumentSourceV1,
  pub rechecker: &'a mut dyn IndexPartialCandidateRecheckerV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub limits: IndexPartialAccelerationLimitsV1,
}

#[derive(Debug)]
struct OwnedCandidateV1 {
  file_key: Vec<u8>,
  indexed_revision_hash: Vec<u8>,
}

#[derive(Debug)]
struct OwnedChangedDocumentV1 {
  file_key: Vec<u8>,
  basis_revision_hash: Option<Vec<u8>>,
  target_revision_hash: Option<Vec<u8>>,
  target_matches: bool,
}

pub fn execute_partial_index_acceleration_v1(
  request: IndexPartialAccelerationRequestV1<'_>,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  normalize_execution_outcome_v1(execute_partial_index_acceleration_inner_v1(request))
}

fn normalize_execution_outcome_v1(
  outcome: Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1>,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  match outcome {
    Err(error) if error.class == IndexPartialAccelerationErrorClassV1::LocalResourceLimit => {
      Ok(local_resource_fallback(error.code, error.context))
    }
    outcome => outcome,
  }
}

fn execute_partial_index_acceleration_inner_v1(
  request: IndexPartialAccelerationRequestV1<'_>,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  require_not_cancelled(request.cancellation)?;
  let (generation, target_namespace_root, target_publication_sequence) = match request.plan {
    IndexCoveragePlanV1::PartialCandidate { generation, target_namespace_root, target_publication_sequence } => {
      (*generation, *target_namespace_root, *target_publication_sequence)
    }
    _ => {
      return Err(IndexPartialAccelerationErrorV1::invalid(
        "index_partial_plan",
        "partial acceleration requires a planner-produced partial candidate",
      ));
    }
  };
  validate_execution_identity(
    request.hash_algorithm,
    generation,
    target_namespace_root,
    target_publication_sequence,
    request.query_fingerprint,
  )?;

  let bounds = match execution_bounds(request.hash_algorithm, request.limits) {
    Ok(bounds) => bounds,
    Err(error) => return Ok(local_resource_fallback(error.code(), error.context)),
  };
  let mut reservation = match request.memory.reserve(MemoryOwner::Query, bounds.total, AdmissionClass::Workload) {
    Ok(reservation) => reservation,
    Err(
      error @ (MemoryCoordinatorError::HardLimitExceeded { .. }
      | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
      | MemoryCoordinatorError::SoftPressureDeferred { .. }),
    ) => {
      return Ok(local_resource_fallback(memory_refusal_code(&error), error.to_string()));
    }
    Err(error) => {
      return Err(IndexPartialAccelerationErrorV1::internal(
        "index_partial_memory_authority",
        format!("query memory authority failed: {error}"),
      ));
    }
  };

  let mut candidates = Vec::new();
  if let Err(error) = candidates.try_reserve_exact(bounds.candidate_capacity) {
    return Ok(local_resource_fallback("index_partial_candidate_allocation", error.to_string()));
  }
  let mut changed = Vec::new();
  if let Err(error) = changed.try_reserve_exact(bounds.changed_capacity) {
    return Ok(local_resource_fallback("index_partial_complement_allocation", error.to_string()));
  }
  let mut matches = Vec::new();
  if let Err(error) = matches.try_reserve_exact(bounds.match_capacity) {
    return Ok(local_resource_fallback("index_partial_result_allocation", error.to_string()));
  }

  let candidate_receipt = {
    let mut visitor = CandidateCollectorV1 {
      hash_algorithm: request.hash_algorithm,
      maximum: request.limits.maximum_accelerator_candidates,
      cancellation: request.cancellation,
      candidates: &mut candidates,
    };
    request.candidates.scan_candidates(
      IndexAcceleratorCandidateScanRequestV1 {
        hash_algorithm: request.hash_algorithm,
        generation: generation.generation,
        generation_manifest_hash: generation.manifest_hash,
        source_namespace_root: generation.source_namespace_root,
        query_fingerprint: request.query_fingerprint,
        maximum_candidates: request.limits.maximum_accelerator_candidates,
        cancellation: request.cancellation,
      },
      &mut visitor,
    )
  };
  let candidate_receipt = match candidate_receipt {
    Ok(receipt) => receipt,
    Err(error) => return handle_scan_error(IndexPartialAccelerationStageV1::CandidateSource, error, true),
  };
  require_not_cancelled(request.cancellation)?;
  if let Err(error) = validate_candidate_receipt(generation, request.query_fingerprint, candidates.len(), &candidate_receipt) {
    return Ok(derived_fallback(
      IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt,
      IndexPartialAccelerationStageV1::CandidateSource,
      IndexPartialSourceErrorClassV1::Corrupt,
      error.code(),
      error.context,
    ));
  }
  if let Some(fallback) = normalize_candidates(&mut candidates) {
    return Ok(fallback);
  }

  let complement_receipt = {
    let mut visitor = ChangedDocumentCollectorV1 {
      hash_algorithm: request.hash_algorithm,
      maximum: request.limits.maximum_changed_documents,
      cancellation: request.cancellation,
      changed: &mut changed,
    };
    request.complement.scan_changed_documents(
      IndexChangedDocumentScanRequestV1 {
        hash_algorithm: request.hash_algorithm,
        generation_manifest_hash: generation.manifest_hash,
        source_namespace_root: generation.source_namespace_root,
        target_namespace_root,
        covered_through_publication_sequence: generation.coverage_publication_sequence,
        target_publication_sequence,
        maximum_changed_documents: request.limits.maximum_changed_documents,
        cancellation: request.cancellation,
      },
      &mut visitor,
    )
  };
  let complement_receipt = match complement_receipt {
    Ok(receipt) => receipt,
    Err(error) => return handle_scan_error(IndexPartialAccelerationStageV1::ComplementSource, error, false),
  };
  require_not_cancelled(request.cancellation)?;
  validate_complement_receipt(generation, target_namespace_root, target_publication_sequence, changed.len(), &complement_receipt)?;
  let changed_document_count = u64::try_from(changed.len())
    .map_err(|error| IndexPartialAccelerationErrorV1::internal("index_partial_changed_count", error.to_string()))?;
  let changed_document_set_hash = changed_document_set_hash(ChangedDocumentSetHashInputV1 {
    algorithm: request.hash_algorithm,
    generation_manifest_hash: generation.manifest_hash,
    source_namespace_root: generation.source_namespace_root,
    target_namespace_root,
    source_sequence: generation.coverage_publication_sequence,
    target_sequence: target_publication_sequence,
    changed_document_count,
    changed: &changed,
  });

  for document in &mut changed {
    require_not_cancelled(request.cancellation)?;
    let outcome = match request.rechecker.recheck(IndexPartialRecheckRequestV1 {
      hash_algorithm: request.hash_algorithm,
      target_namespace_root,
      target_publication_sequence,
      query_fingerprint: request.query_fingerprint,
      file_key: &document.file_key,
      basis_revision_hash: document.basis_revision_hash.as_deref(),
      expected_target_revision_hash: document.target_revision_hash.as_deref(),
      origin: IndexPartialRecheckOriginV1::ChangedDocumentComplement,
      cancellation: request.cancellation,
    }) {
      Ok(outcome) => outcome,
      Err(error) => return handle_recheck_error(error),
    };
    document.target_matches = validate_recheck_outcome(request.hash_algorithm, document.target_revision_hash.as_deref(), &outcome)?;
    if document.target_matches {
      let revision = document.target_revision_hash.as_deref().ok_or_else(|| {
        IndexPartialAccelerationErrorV1::corrupt_recheck("index_partial_absent_match", "an absent target document cannot satisfy the query")
      })?;
      if matches.len() >= bounds.match_capacity {
        return Ok(local_resource_fallback("index_partial_match_limit", "exact complement matches exceed the admitted result limit"));
      }
      matches.push(copy_match(&document.file_key, revision)?);
    }
  }

  let observed_candidate_count = candidate_receipt.candidate_count;
  let unique_candidate_count = u64::try_from(candidates.len())
    .map_err(|error| IndexPartialAccelerationErrorV1::internal("index_partial_candidate_count", error.to_string()))?;
  let mut overlap_deduplicated_count = 0u64;
  for candidate in &candidates {
    require_not_cancelled(request.cancellation)?;
    let changed_index = changed.partition_point(|document| document.file_key < candidate.file_key);
    if changed.get(changed_index).is_some_and(|document| document.file_key == candidate.file_key) {
      let index = changed_index;
      let document = &changed[index];
      if document.basis_revision_hash.as_deref() != Some(candidate.indexed_revision_hash.as_slice()) {
        return Ok(derived_fallback(
          IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt,
          IndexPartialAccelerationStageV1::CandidateSource,
          IndexPartialSourceErrorClassV1::Corrupt,
          "index_partial_candidate_basis_mismatch",
          "accelerator candidate revision disagrees with the exact changed-document basis",
        ));
      }
      overlap_deduplicated_count = overlap_deduplicated_count.checked_add(1).ok_or_else(|| {
        IndexPartialAccelerationErrorV1::internal("index_partial_overlap_count", "candidate/complement overlap count overflowed")
      })?;
    } else {
      let outcome = match request.rechecker.recheck(IndexPartialRecheckRequestV1 {
        hash_algorithm: request.hash_algorithm,
        target_namespace_root,
        target_publication_sequence,
        query_fingerprint: request.query_fingerprint,
        file_key: &candidate.file_key,
        basis_revision_hash: Some(&candidate.indexed_revision_hash),
        expected_target_revision_hash: Some(&candidate.indexed_revision_hash),
        origin: IndexPartialRecheckOriginV1::AcceleratorCandidate,
        cancellation: request.cancellation,
      }) {
        Ok(outcome) => outcome,
        Err(error) => return handle_recheck_error(error),
      };
      if validate_recheck_outcome(request.hash_algorithm, Some(&candidate.indexed_revision_hash), &outcome)? {
        if matches.len() >= bounds.match_capacity {
          return Ok(local_resource_fallback("index_partial_match_limit", "exact candidate matches exceed the admitted result limit"));
        }
        matches.push(copy_match(&candidate.file_key, &candidate.indexed_revision_hash)?);
      }
    }
  }

  matches.sort_unstable_by(|left, right| {
    left.file_key.cmp(&right.file_key).then_with(|| left.record_revision_hash.cmp(&right.record_revision_hash))
  });
  for pair in matches.windows(2) {
    if pair[0].file_key == pair[1].file_key && pair[0].record_revision_hash != pair[1].record_revision_hash {
      return Err(IndexPartialAccelerationErrorV1::corrupt_recheck(
        "index_partial_duplicate_target_revision",
        "one target FileKey resolved to multiple result revisions",
      ));
    }
  }
  matches.dedup_by(|later, earlier| later.file_key == earlier.file_key && later.record_revision_hash == earlier.record_revision_hash);
  require_not_cancelled(request.cancellation)?;

  let mut coverage_epoch_id = [0u8; 16];
  coverage_epoch_id.copy_from_slice(generation.coverage_epoch_id);
  let proof_hashes = copy_proof_hashes(
    request.hash_algorithm,
    [
      generation.manifest_hash,
      generation.source_namespace_root,
      target_namespace_root,
      request.query_fingerprint,
      &changed_document_set_hash,
    ],
  )?;
  let proof = ExactIndexComplementProofV1 {
    hash_algorithm: request.hash_algorithm,
    hashes: proof_hashes,
    coverage_epoch_id,
    covered_through_publication_sequence: generation.coverage_publication_sequence,
    target_publication_sequence,
    changed_document_count,
  };

  drop(candidates);
  drop(changed);
  let workspace_bytes = bounds.total.checked_sub(bounds.output).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::internal("index_partial_memory_accounting", "output reservation exceeds total reservation")
  })?;
  reservation.shrink(workspace_bytes).map_err(|error| {
    IndexPartialAccelerationErrorV1::internal("index_partial_memory_release", format!("failed to release execution workspace: {error}"))
  })?;
  Ok(IndexPartialAccelerationOutcomeV1::Exact(ExactPartialIndexAccelerationV1 {
    proof,
    matches,
    observed_candidate_count,
    unique_candidate_count,
    rechecked_candidate_count: unique_candidate_count,
    overlap_deduplicated_count,
    retained_bytes: bounds.output,
    _reservation: reservation,
  }))
}

struct CandidateCollectorV1<'a> {
  hash_algorithm: HashAlgorithm,
  maximum: u64,
  cancellation: &'a CancellationToken,
  candidates: &'a mut Vec<OwnedCandidateV1>,
}

impl IndexAcceleratorCandidateVisitorV1 for CandidateCollectorV1<'_> {
  fn visit(&mut self, candidate: IndexAcceleratorCandidateV1<'_>) -> Result<(), IndexPartialAccelerationErrorV1> {
    require_not_cancelled(self.cancellation)?;
    validate_hash(candidate.file_key, self.hash_algorithm, "candidate FileKey", false)?;
    validate_hash(candidate.indexed_revision_hash, self.hash_algorithm, "candidate revision", false)?;
    let retained_count = u64::try_from(self.candidates.len())
      .map_err(|error| IndexPartialAccelerationErrorV1::internal("index_partial_candidate_count", error.to_string()))?;
    if retained_count >= self.maximum {
      return Err(IndexPartialAccelerationErrorV1::invalid(
        "index_partial_candidate_limit",
        "accelerator emitted more candidates than admitted",
      ));
    }
    self.candidates.push(OwnedCandidateV1 {
      file_key: copy_bytes(candidate.file_key, "candidate FileKey")?,
      indexed_revision_hash: copy_bytes(candidate.indexed_revision_hash, "candidate revision")?,
    });
    Ok(())
  }
}

struct ChangedDocumentCollectorV1<'a> {
  hash_algorithm: HashAlgorithm,
  maximum: u64,
  cancellation: &'a CancellationToken,
  changed: &'a mut Vec<OwnedChangedDocumentV1>,
}

impl IndexChangedDocumentVisitorV1 for ChangedDocumentCollectorV1<'_> {
  fn visit(&mut self, document: IndexChangedDocumentV1<'_>) -> Result<(), IndexPartialAccelerationErrorV1> {
    require_not_cancelled(self.cancellation)?;
    validate_hash(document.file_key, self.hash_algorithm, "changed FileKey", true)?;
    validate_optional_hash(document.basis_revision_hash, self.hash_algorithm, "changed basis revision")?;
    validate_optional_hash(document.target_revision_hash, self.hash_algorithm, "changed target revision")?;
    if document.basis_revision_hash == document.target_revision_hash {
      return Err(IndexPartialAccelerationErrorV1::corrupt_complement(
        "index_partial_unchanged_complement_row",
        "changed-document evidence has identical basis and target revisions",
      ));
    }
    if self.changed.last().is_some_and(|prior| prior.file_key.as_slice() >= document.file_key) {
      return Err(IndexPartialAccelerationErrorV1::corrupt_complement(
        "index_partial_complement_order",
        "changed-document evidence is not strictly ordered by FileKey",
      ));
    }
    let retained_count = u64::try_from(self.changed.len())
      .map_err(|error| IndexPartialAccelerationErrorV1::internal("index_partial_changed_count", error.to_string()))?;
    if retained_count >= self.maximum {
      return Err(IndexPartialAccelerationErrorV1::corrupt_complement(
        "index_partial_complement_limit",
        "changed-document source exceeded the admitted document count",
      ));
    }
    self.changed.push(OwnedChangedDocumentV1 {
      file_key: copy_bytes(document.file_key, "changed FileKey")?,
      basis_revision_hash: copy_optional_bytes(document.basis_revision_hash, "changed basis revision")?,
      target_revision_hash: copy_optional_bytes(document.target_revision_hash, "changed target revision")?,
      target_matches: false,
    });
    Ok(())
  }
}

#[derive(Clone, Copy)]
struct ExecutionBoundsV1 {
  total: u64,
  output: u64,
  candidate_capacity: usize,
  changed_capacity: usize,
  match_capacity: usize,
}

fn execution_bounds(
  algorithm: HashAlgorithm,
  limits: IndexPartialAccelerationLimitsV1,
) -> Result<ExecutionBoundsV1, IndexPartialAccelerationErrorV1> {
  let hash_width = algorithm.hash_length() as u64;
  let candidate_capacity = usize::try_from(limits.maximum_accelerator_candidates)
    .map_err(|error| IndexPartialAccelerationErrorV1::invalid("index_partial_candidate_capacity", error.to_string()))?;
  let changed_capacity = usize::try_from(limits.maximum_changed_documents)
    .map_err(|error| IndexPartialAccelerationErrorV1::invalid("index_partial_changed_capacity", error.to_string()))?;
  let match_capacity = usize::try_from(limits.maximum_matches)
    .map_err(|error| IndexPartialAccelerationErrorV1::invalid("index_partial_match_capacity", error.to_string()))?;
  let candidate_row = (size_of::<OwnedCandidateV1>() as u64)
    .checked_add(2 * hash_width)
    .and_then(|value| value.checked_add(OWNED_ROW_ALLOWANCE))
    .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "candidate row bound overflowed"))?;
  let changed_row = (size_of::<OwnedChangedDocumentV1>() as u64)
    .checked_add(3 * hash_width)
    .and_then(|value| value.checked_add(OWNED_ROW_ALLOWANCE))
    .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "changed row bound overflowed"))?;
  let output_row = (size_of::<IndexMatchedDocumentIdentityV1>() as u64)
    .checked_add(2 * hash_width)
    .and_then(|value| value.checked_add(OUTPUT_ROW_ALLOWANCE))
    .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "output row bound overflowed"))?;
  let output = EXECUTION_BASE_ALLOWANCE
    .checked_add(
      limits
        .maximum_matches
        .checked_mul(output_row)
        .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "output memory bound overflowed"))?,
    )
    .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "output memory bound overflowed"))?;
  let total = output
    .checked_add(
      limits
        .maximum_accelerator_candidates
        .checked_mul(candidate_row)
        .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "candidate memory bound overflowed"))?,
    )
    .and_then(|value| value.checked_add(limits.maximum_changed_documents.checked_mul(changed_row)?))
    .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("index_partial_memory_bound", "execution memory bound overflowed"))?;
  if total > limits.maximum_retained_bytes {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "index_partial_memory_bound",
      format!("partial acceleration requires {total} retained bytes but its bound is {}", limits.maximum_retained_bytes),
    ));
  }
  Ok(ExecutionBoundsV1 { total, output, candidate_capacity, changed_capacity, match_capacity })
}

fn validate_execution_identity(
  algorithm: HashAlgorithm,
  generation: IndexCoverageGenerationV1<'_>,
  target_namespace_root: &[u8],
  target_publication_sequence: u64,
  query_fingerprint: &[u8],
) -> Result<(), IndexPartialAccelerationErrorV1> {
  validate_hash(generation.manifest_hash, algorithm, "generation manifest", false)?;
  validate_hash(generation.source_namespace_root, algorithm, "generation source root", false)?;
  validate_hash(target_namespace_root, algorithm, "target namespace root", false)?;
  validate_hash(query_fingerprint, algorithm, "query fingerprint", false)?;
  if generation.generation == 0
    || generation.coverage_publication_sequence == 0
    || generation.coverage_publication_sequence >= target_publication_sequence
    || generation.coverage_epoch_id.len() != 16
    || generation.coverage_epoch_id.iter().all(|byte| *byte == 0)
  {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "index_partial_generation",
      "partial generation identity, epoch, or publication interval is invalid",
    ));
  }
  Ok(())
}

fn validate_candidate_receipt(
  generation: IndexCoverageGenerationV1<'_>,
  query_fingerprint: &[u8],
  observed: usize,
  receipt: &IndexAcceleratorCandidateScanReceiptV1,
) -> Result<(), IndexPartialAccelerationErrorV1> {
  let observed = u64::try_from(observed).map_err(|error| {
    IndexPartialAccelerationErrorV1::invalid("index_partial_candidate_receipt", format!("candidate count exceeds u64: {error}"))
  })?;
  if !receipt.complete
    || receipt.generation != generation.generation
    || receipt.generation_manifest_hash != generation.manifest_hash
    || receipt.source_namespace_root != generation.source_namespace_root
    || receipt.query_fingerprint != query_fingerprint
    || receipt.candidate_count != observed
  {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "index_partial_candidate_receipt",
      "candidate receipt does not bind the complete observed generation/query stream",
    ));
  }
  Ok(())
}

fn validate_complement_receipt(
  generation: IndexCoverageGenerationV1<'_>,
  target_namespace_root: &[u8],
  target_publication_sequence: u64,
  observed: usize,
  receipt: &IndexChangedDocumentScanReceiptV1,
) -> Result<(), IndexPartialAccelerationErrorV1> {
  let observed = u64::try_from(observed).map_err(|error| {
    IndexPartialAccelerationErrorV1::corrupt_complement(
      "index_partial_complement_receipt",
      format!("changed-document count exceeds u64: {error}"),
    )
  })?;
  if !receipt.complete
    || receipt.source_namespace_root != generation.source_namespace_root
    || receipt.target_namespace_root != target_namespace_root
    || receipt.covered_through_publication_sequence != generation.coverage_publication_sequence
    || receipt.target_publication_sequence != target_publication_sequence
    || receipt.changed_document_count != observed
  {
    return Err(IndexPartialAccelerationErrorV1::corrupt_complement(
      "index_partial_complement_receipt",
      "changed-document receipt does not bind the complete observed root interval",
    ));
  }
  Ok(())
}

fn normalize_candidates(candidates: &mut Vec<OwnedCandidateV1>) -> Option<IndexPartialAccelerationOutcomeV1> {
  candidates.sort_unstable_by(|left, right| {
    left.file_key.cmp(&right.file_key).then_with(|| left.indexed_revision_hash.cmp(&right.indexed_revision_hash))
  });
  for pair in candidates.windows(2) {
    if pair[0].file_key == pair[1].file_key && pair[0].indexed_revision_hash != pair[1].indexed_revision_hash {
      return Some(derived_fallback(
        IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt,
        IndexPartialAccelerationStageV1::CandidateSource,
        IndexPartialSourceErrorClassV1::Corrupt,
        "index_partial_candidate_revision_conflict",
        "one immutable generation returned multiple revisions for the same FileKey",
      ));
    }
  }
  candidates.dedup_by(|later, earlier| later.file_key == earlier.file_key && later.indexed_revision_hash == earlier.indexed_revision_hash);
  None
}

fn validate_recheck_outcome(
  algorithm: HashAlgorithm,
  expected_revision: Option<&[u8]>,
  outcome: &IndexPartialRecheckOutcomeV1,
) -> Result<bool, IndexPartialAccelerationErrorV1> {
  match (expected_revision, outcome) {
    (None, IndexPartialRecheckOutcomeV1::Absent) => Ok(false),
    (Some(expected), IndexPartialRecheckOutcomeV1::Present { record_revision_hash, matches }) => {
      if record_revision_hash.len() != algorithm.hash_length() || record_revision_hash.iter().all(|byte| *byte == 0) {
        return Err(IndexPartialAccelerationErrorV1::corrupt_recheck(
          "index_partial_recheck_revision_hash",
          "selected-root recheck returned an all-zero or wrong-width revision",
        ));
      }
      if record_revision_hash != expected {
        return Err(IndexPartialAccelerationErrorV1::corrupt_complement(
          "index_partial_recheck_revision_mismatch",
          "selected-root recheck disagrees with the exact complement revision",
        ));
      }
      Ok(*matches)
    }
    _ => Err(IndexPartialAccelerationErrorV1::corrupt_complement(
      "index_partial_recheck_presence_mismatch",
      "selected-root recheck disagrees with the exact complement presence state",
    )),
  }
}

struct ChangedDocumentSetHashInputV1<'a> {
  algorithm: HashAlgorithm,
  generation_manifest_hash: &'a [u8],
  source_namespace_root: &'a [u8],
  target_namespace_root: &'a [u8],
  source_sequence: u64,
  target_sequence: u64,
  changed_document_count: u64,
  changed: &'a [OwnedChangedDocumentV1],
}

fn changed_document_set_hash(input: ChangedDocumentSetHashInputV1<'_>) -> Vec<u8> {
  let mut digest = IncrementalDigestV1::new(input.algorithm);
  digest.update(CHANGED_SET_DOMAIN_V1);
  digest.update(&input.algorithm.to_u16().to_le_bytes());
  digest.update(input.generation_manifest_hash);
  digest.update(input.source_namespace_root);
  digest.update(input.target_namespace_root);
  digest.update(&input.source_sequence.to_le_bytes());
  digest.update(&input.target_sequence.to_le_bytes());
  digest.update(&input.changed_document_count.to_le_bytes());
  for document in input.changed {
    digest.update(&document.file_key);
    digest.update(&[u8::from(document.basis_revision_hash.is_some()), u8::from(document.target_revision_hash.is_some())]);
    if let Some(revision) = document.basis_revision_hash.as_deref() {
      digest.update(revision);
    }
    if let Some(revision) = document.target_revision_hash.as_deref() {
      digest.update(revision);
    }
  }
  digest.finalize()
}

fn handle_scan_error(
  stage: IndexPartialAccelerationStageV1,
  error: IndexPartialScanErrorV1,
  derived: bool,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  match error {
    IndexPartialScanErrorV1::Source(source) => handle_source_error(stage, source, derived),
    IndexPartialScanErrorV1::Visitor(error) => {
      if matches!(error.class, IndexPartialAccelerationErrorClassV1::Cancelled | IndexPartialAccelerationErrorClassV1::LocalResourceLimit) {
        Err(error)
      } else if derived {
        Ok(derived_fallback(
          IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt,
          stage,
          IndexPartialSourceErrorClassV1::Corrupt,
          error.code,
          error.context,
        ))
      } else {
        Err(error)
      }
    }
  }
}

fn handle_source_error(
  stage: IndexPartialAccelerationStageV1,
  error: IndexPartialSourceErrorV1,
  derived: bool,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  if error.class == IndexPartialSourceErrorClassV1::Cancelled {
    return Err(IndexPartialAccelerationErrorV1::cancelled(error.code, error.context));
  }
  if error.class == IndexPartialSourceErrorClassV1::Internal {
    return Err(IndexPartialAccelerationErrorV1::internal(error.code, error.context));
  }
  let reason = match (stage, error.class) {
    (IndexPartialAccelerationStageV1::CandidateSource, IndexPartialSourceErrorClassV1::Unavailable) => {
      IndexPartialAccelerationFallbackReasonV1::CandidateUnavailable
    }
    (IndexPartialAccelerationStageV1::CandidateSource, IndexPartialSourceErrorClassV1::ResourceLimit) => {
      IndexPartialAccelerationFallbackReasonV1::CandidateResourceLimit
    }
    (IndexPartialAccelerationStageV1::CandidateSource, IndexPartialSourceErrorClassV1::Corrupt) if derived => {
      IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt
    }
    (IndexPartialAccelerationStageV1::ComplementSource, IndexPartialSourceErrorClassV1::Unavailable) => {
      IndexPartialAccelerationFallbackReasonV1::ComplementUnavailable
    }
    (IndexPartialAccelerationStageV1::ComplementSource, IndexPartialSourceErrorClassV1::ResourceLimit) => {
      IndexPartialAccelerationFallbackReasonV1::ComplementResourceLimit
    }
    (_, IndexPartialSourceErrorClassV1::Corrupt) => {
      return Err(IndexPartialAccelerationErrorV1::corrupt_complement(error.code, error.context));
    }
    _ => return Err(IndexPartialAccelerationErrorV1::internal(error.code, error.context)),
  };
  Ok(derived_fallback(reason, stage, error.class, error.code, error.context))
}

fn handle_recheck_error(error: IndexPartialSourceErrorV1) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  match error.class {
    IndexPartialSourceErrorClassV1::Unavailable => Ok(derived_fallback(
      IndexPartialAccelerationFallbackReasonV1::RecheckUnavailable,
      IndexPartialAccelerationStageV1::Recheck,
      error.class,
      error.code,
      error.context,
    )),
    IndexPartialSourceErrorClassV1::ResourceLimit => Ok(derived_fallback(
      IndexPartialAccelerationFallbackReasonV1::RecheckResourceLimit,
      IndexPartialAccelerationStageV1::Recheck,
      error.class,
      error.code,
      error.context,
    )),
    IndexPartialSourceErrorClassV1::Corrupt => Err(IndexPartialAccelerationErrorV1::corrupt_recheck(error.code, error.context)),
    IndexPartialSourceErrorClassV1::Cancelled => Err(IndexPartialAccelerationErrorV1::cancelled(error.code, error.context)),
    IndexPartialSourceErrorClassV1::Internal => Err(IndexPartialAccelerationErrorV1::internal(error.code, error.context)),
  }
}

fn local_resource_fallback(code: &'static str, context: impl Into<String>) -> IndexPartialAccelerationOutcomeV1 {
  derived_fallback(
    IndexPartialAccelerationFallbackReasonV1::LocalResourceLimit,
    IndexPartialAccelerationStageV1::Local,
    IndexPartialSourceErrorClassV1::ResourceLimit,
    code,
    context,
  )
}

fn memory_refusal_code(error: &MemoryCoordinatorError) -> &'static str {
  match error {
    MemoryCoordinatorError::HardLimitExceeded { .. } => "index_partial_memory_hard_limit",
    MemoryCoordinatorError::EmergencyReserveExceeded { .. } => "index_partial_memory_emergency_limit",
    MemoryCoordinatorError::SoftPressureDeferred { .. } => "index_partial_memory_soft_pressure",
    MemoryCoordinatorError::InvalidPolicy(_)
    | MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::InvalidObservation { .. }
    | MemoryCoordinatorError::InvalidCriticalOwner { .. }
    | MemoryCoordinatorError::AccountingOverflow { .. }
    | MemoryCoordinatorError::InvalidShrink { .. }
    | MemoryCoordinatorError::AccountingInvariant { .. }
    | MemoryCoordinatorError::Poisoned
    | MemoryCoordinatorError::ObservationFailed { .. } => "index_partial_memory_authority",
  }
}

fn derived_fallback(
  reason: IndexPartialAccelerationFallbackReasonV1,
  stage: IndexPartialAccelerationStageV1,
  class: IndexPartialSourceErrorClassV1,
  code: &'static str,
  context: impl Into<String>,
) -> IndexPartialAccelerationOutcomeV1 {
  IndexPartialAccelerationOutcomeV1::AuthoritativeOnly {
    reason,
    diagnostic: IndexPartialAccelerationDiagnosticV1 { stage, class, code, context: context.into() },
  }
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), IndexPartialAccelerationErrorV1> {
  if cancellation.is_cancelled() {
    return Err(IndexPartialAccelerationErrorV1::cancelled("index_partial_cancelled", "partial index acceleration was cancelled"));
  }
  Ok(())
}

fn validate_optional_hash(
  value: Option<&[u8]>,
  algorithm: HashAlgorithm,
  label: &'static str,
) -> Result<(), IndexPartialAccelerationErrorV1> {
  if let Some(value) = value {
    validate_hash(value, algorithm, label, true)?;
  }
  Ok(())
}

fn validate_hash(
  value: &[u8],
  algorithm: HashAlgorithm,
  label: &'static str,
  authoritative: bool,
) -> Result<(), IndexPartialAccelerationErrorV1> {
  if value.len() == algorithm.hash_length() && value.iter().any(|byte| *byte != 0) {
    return Ok(());
  }
  if authoritative {
    Err(IndexPartialAccelerationErrorV1::corrupt_complement(
      "index_partial_authority_hash",
      format!("{label} is all zero or has the wrong hash width"),
    ))
  } else {
    Err(IndexPartialAccelerationErrorV1::invalid("index_partial_identity_hash", format!("{label} is all zero or has the wrong hash width")))
  }
}

fn copy_bytes(value: &[u8], label: &'static str) -> Result<Vec<u8>, IndexPartialAccelerationErrorV1> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(value.len()).map_err(|error| {
    IndexPartialAccelerationErrorV1::local_resource("index_partial_allocation", format!("failed to retain {label}: {error}"))
  })?;
  copy.extend_from_slice(value);
  Ok(copy)
}

fn copy_proof_hashes(algorithm: HashAlgorithm, values: [&[u8]; 5]) -> Result<Vec<u8>, IndexPartialAccelerationErrorV1> {
  let length = algorithm
    .hash_length()
    .checked_mul(values.len())
    .ok_or_else(|| IndexPartialAccelerationErrorV1::internal("index_partial_proof_length", "proof hash length overflowed"))?;
  let mut hashes = Vec::new();
  hashes.try_reserve_exact(length).map_err(|error| {
    IndexPartialAccelerationErrorV1::local_resource(
      "index_partial_allocation",
      format!("failed to retain exact complement proof hashes: {error}"),
    )
  })?;
  for value in values {
    hashes.extend_from_slice(value);
  }
  Ok(hashes)
}

fn copy_optional_bytes(value: Option<&[u8]>, label: &'static str) -> Result<Option<Vec<u8>>, IndexPartialAccelerationErrorV1> {
  value.map(|value| copy_bytes(value, label)).transpose()
}

fn copy_match(file_key: &[u8], revision: &[u8]) -> Result<IndexMatchedDocumentIdentityV1, IndexPartialAccelerationErrorV1> {
  Ok(IndexMatchedDocumentIdentityV1 {
    file_key: copy_bytes(file_key, "result FileKey")?,
    record_revision_hash: copy_bytes(revision, "result revision")?,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn candidate_visitor_allocation_pressure_is_a_local_fallback_not_candidate_corruption() {
    let visitor_error = IndexPartialScanErrorV1::Visitor(IndexPartialAccelerationErrorV1::local_resource(
      "allocation_refused",
      "allocator refused a retained candidate row",
    ));
    let outcome =
      normalize_execution_outcome_v1(handle_scan_error(IndexPartialAccelerationStageV1::CandidateSource, visitor_error, true)).unwrap();
    let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, diagnostic } = outcome else {
      panic!("local allocation pressure produced an exact result");
    };
    assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::LocalResourceLimit);
    assert_eq!(diagnostic.stage, IndexPartialAccelerationStageV1::Local);
    assert_eq!(diagnostic.class, IndexPartialSourceErrorClassV1::ResourceLimit);
    assert_eq!(diagnostic.code, "allocation_refused");
    assert_eq!(diagnostic.context(), "allocator refused a retained candidate row");
  }
}
