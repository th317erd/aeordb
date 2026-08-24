use std::collections::BTreeMap;
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_coverage_planner::{
  IndexCoverageGenerationHealthV1, IndexCoverageGenerationV1, IndexCoveragePlanV1, IndexCoveragePlanningRequestV1,
  IndexSemanticQueryAvailabilityV1, plan_selected_index_coverage_v1,
};
use aeordb::engine::v4::index_partial_acceleration::{
  ExactPartialIndexAccelerationV1, IndexAcceleratorCandidateScanReceiptV1, IndexAcceleratorCandidateScanRequestV1,
  IndexAcceleratorCandidateSourceV1, IndexAcceleratorCandidateV1, IndexAcceleratorCandidateVisitorV1, IndexChangedDocumentScanReceiptV1,
  IndexChangedDocumentScanRequestV1, IndexChangedDocumentSourceV1, IndexChangedDocumentV1, IndexChangedDocumentVisitorV1,
  IndexPartialAccelerationErrorClassV1, IndexPartialAccelerationFallbackReasonV1, IndexPartialAccelerationLimitsV1,
  IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationRequestV1, IndexPartialAccelerationStageV1, IndexPartialCandidateRecheckerV1,
  IndexPartialRecheckOriginV1, IndexPartialRecheckOutcomeV1, IndexPartialRecheckRequestV1, IndexPartialScanErrorV1,
  IndexPartialSourceErrorClassV1, IndexPartialSourceErrorV1, execute_partial_index_acceleration_v1,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct OwnedCandidate {
  file_key: Vec<u8>,
  revision: Vec<u8>,
}

#[derive(Clone)]
struct OwnedChange {
  file_key: Vec<u8>,
  basis_revision: Option<Vec<u8>>,
  target_revision: Option<Vec<u8>>,
}

#[derive(Default)]
struct CandidateFeed {
  rows: Vec<OwnedCandidate>,
  source_error: Option<IndexPartialSourceErrorV1>,
  complete: bool,
  receipt_count_delta: i64,
  receipt_manifest_override: Option<Vec<u8>>,
  scans: usize,
  cancel_after_rows: Option<usize>,
  cancel_before_receipt: bool,
}

impl IndexAcceleratorCandidateSourceV1 for CandidateFeed {
  fn scan_candidates(
    &mut self,
    request: IndexAcceleratorCandidateScanRequestV1<'_>,
    visitor: &mut dyn IndexAcceleratorCandidateVisitorV1,
  ) -> Result<IndexAcceleratorCandidateScanReceiptV1, IndexPartialScanErrorV1> {
    self.scans += 1;
    if let Some(error) = self.source_error.clone() {
      return Err(IndexPartialScanErrorV1::Source(error));
    }
    for (index, row) in self.rows.iter().enumerate() {
      if self.cancel_after_rows == Some(index) {
        request.cancellation.cancel();
      }
      visitor
        .visit(IndexAcceleratorCandidateV1 { file_key: &row.file_key, indexed_revision_hash: &row.revision })
        .map_err(IndexPartialScanErrorV1::Visitor)?;
    }
    let candidate_count = adjusted_count(self.rows.len(), self.receipt_count_delta);
    if self.cancel_before_receipt {
      request.cancellation.cancel();
    }
    Ok(IndexAcceleratorCandidateScanReceiptV1 {
      generation: request.generation,
      generation_manifest_hash: self.receipt_manifest_override.clone().unwrap_or_else(|| request.generation_manifest_hash.to_vec()),
      source_namespace_root: request.source_namespace_root.to_vec(),
      query_fingerprint: request.query_fingerprint.to_vec(),
      candidate_count,
      complete: self.complete,
    })
  }
}

#[derive(Default)]
struct ChangedFeed {
  rows: Vec<OwnedChange>,
  source_error: Option<IndexPartialSourceErrorV1>,
  complete: bool,
  receipt_count_delta: i64,
  target_override: Option<Vec<u8>>,
  scans: usize,
  cancel_after_rows: Option<usize>,
  cancel_before_receipt: bool,
}

impl IndexChangedDocumentSourceV1 for ChangedFeed {
  fn scan_changed_documents(
    &mut self,
    request: IndexChangedDocumentScanRequestV1<'_>,
    visitor: &mut dyn IndexChangedDocumentVisitorV1,
  ) -> Result<IndexChangedDocumentScanReceiptV1, IndexPartialScanErrorV1> {
    self.scans += 1;
    if let Some(error) = self.source_error.clone() {
      return Err(IndexPartialScanErrorV1::Source(error));
    }
    for (index, row) in self.rows.iter().enumerate() {
      if self.cancel_after_rows == Some(index) {
        request.cancellation.cancel();
      }
      visitor
        .visit(IndexChangedDocumentV1 {
          file_key: &row.file_key,
          basis_revision_hash: row.basis_revision.as_deref(),
          target_revision_hash: row.target_revision.as_deref(),
        })
        .map_err(IndexPartialScanErrorV1::Visitor)?;
    }
    if self.cancel_before_receipt {
      request.cancellation.cancel();
    }
    Ok(IndexChangedDocumentScanReceiptV1 {
      source_namespace_root: request.source_namespace_root.to_vec(),
      target_namespace_root: self.target_override.clone().unwrap_or_else(|| request.target_namespace_root.to_vec()),
      covered_through_publication_sequence: request.covered_through_publication_sequence,
      target_publication_sequence: request.target_publication_sequence,
      changed_document_count: adjusted_count(self.rows.len(), self.receipt_count_delta),
      complete: self.complete,
    })
  }
}

#[derive(Default)]
struct Rechecker {
  outcomes: BTreeMap<Vec<u8>, Result<IndexPartialRecheckOutcomeV1, IndexPartialSourceErrorV1>>,
  calls: Vec<(Vec<u8>, IndexPartialRecheckOriginV1)>,
}

impl IndexPartialCandidateRecheckerV1 for Rechecker {
  fn recheck(&mut self, request: IndexPartialRecheckRequestV1<'_>) -> Result<IndexPartialRecheckOutcomeV1, IndexPartialSourceErrorV1> {
    self.calls.push((request.file_key.to_vec(), request.origin));
    self
      .outcomes
      .get(request.file_key)
      .cloned()
      .unwrap_or_else(|| Err(IndexPartialSourceErrorV1::corrupt("test_missing_recheck", "test fixture omitted a recheck outcome")))
  }
}

struct Case {
  algorithm: HashAlgorithm,
  owner: Vec<u8>,
  manifest: Vec<u8>,
  source_root: Vec<u8>,
  target_root: Vec<u8>,
  epoch: [u8; 16],
  definition: Vec<u8>,
  dependency: Vec<u8>,
  query: Vec<u8>,
}

impl Case {
  fn new(algorithm: HashAlgorithm) -> Self {
    let width = algorithm.hash_length();
    Self {
      algorithm,
      owner: bytes(0x11, width),
      manifest: bytes(0x22, width),
      source_root: bytes(0x33, width),
      target_root: bytes(0x44, width),
      epoch: [0x55; 16],
      definition: bytes(0x66, width),
      dependency: bytes(0x77, width),
      query: bytes(0x88, width),
    }
  }

  fn generation(&self) -> IndexCoverageGenerationV1<'_> {
    IndexCoverageGenerationV1 {
      generation: 7,
      owner_id: &self.owner,
      manifest_hash: &self.manifest,
      source_namespace_root: &self.source_root,
      coverage_epoch_id: &self.epoch,
      coverage_publication_sequence: 11,
      definition_fingerprint: &self.definition,
      dependency_fingerprint: &self.dependency,
      health: IndexCoverageGenerationHealthV1::Healthy,
    }
  }

  fn plan(&self) -> IndexCoveragePlanV1<'_> {
    plan_selected_index_coverage_v1(&IndexCoveragePlanningRequestV1 {
      hash_algorithm: self.algorithm,
      requested_namespace_root: &self.target_root,
      requested_publication_sequence: 19,
      required_owner_id: &self.owner,
      required_definition_fingerprint: &self.definition,
      required_dependency_fingerprint: &self.dependency,
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      selected_generation: Some(self.generation()),
    })
    .unwrap()
  }

  fn execute(
    &self,
    candidates: &mut CandidateFeed,
    changed: &mut ChangedFeed,
    rechecker: &mut Rechecker,
    memory: &MemoryCoordinator,
    cancellation: &CancellationToken,
    limits: IndexPartialAccelerationLimitsV1,
  ) -> Result<IndexPartialAccelerationOutcomeV1, aeordb::engine::v4::index_partial_acceleration::IndexPartialAccelerationErrorV1> {
    let plan = self.plan();
    execute_partial_index_acceleration_v1(IndexPartialAccelerationRequestV1 {
      hash_algorithm: self.algorithm,
      plan: &plan,
      query_fingerprint: &self.query,
      candidates,
      complement: changed,
      rechecker,
      memory,
      cancellation,
      limits,
    })
  }
}

fn adjusted_count(length: usize, delta: i64) -> u64 {
  let length = i64::try_from(length).unwrap();
  u64::try_from(length + delta).unwrap()
}

fn bytes(value: u8, width: usize) -> Vec<u8> {
  vec![value; width]
}

fn memory(limit: u64) -> Arc<MemoryCoordinator> {
  Arc::new(MemoryCoordinator::new(MemoryPolicy::new(limit, limit + 4 * 1024 * 1024, 1, 1024 * 1024).unwrap()))
}

fn limits() -> IndexPartialAccelerationLimitsV1 {
  IndexPartialAccelerationLimitsV1::new(64, 64, 64, 2 * 1024 * 1024).unwrap()
}

fn complete_feeds(algorithm: HashAlgorithm) -> (CandidateFeed, ChangedFeed, Rechecker) {
  let width = algorithm.hash_length();
  let key_1 = bytes(0x91, width);
  let key_2 = bytes(0x92, width);
  let key_3 = bytes(0x93, width);
  let key_4 = bytes(0x94, width);
  let rev_1 = bytes(0xa1, width);
  let rev_2_basis = bytes(0xa2, width);
  let rev_2_target = bytes(0xb2, width);
  let rev_3 = bytes(0xa3, width);
  let rev_4 = bytes(0xb4, width);
  let candidates = CandidateFeed {
    rows: vec![
      OwnedCandidate { file_key: key_2.clone(), revision: rev_2_basis.clone() },
      OwnedCandidate { file_key: key_1.clone(), revision: rev_1.clone() },
      OwnedCandidate { file_key: key_1.clone(), revision: rev_1.clone() },
      OwnedCandidate { file_key: key_3.clone(), revision: rev_3.clone() },
    ],
    complete: true,
    ..CandidateFeed::default()
  };
  let changed = ChangedFeed {
    rows: vec![
      OwnedChange { file_key: key_2.clone(), basis_revision: Some(rev_2_basis), target_revision: Some(rev_2_target.clone()) },
      OwnedChange { file_key: key_3.clone(), basis_revision: Some(rev_3), target_revision: None },
      OwnedChange { file_key: key_4.clone(), basis_revision: None, target_revision: Some(rev_4.clone()) },
    ],
    complete: true,
    ..ChangedFeed::default()
  };
  let mut rechecker = Rechecker::default();
  rechecker.outcomes.insert(key_1, Ok(IndexPartialRecheckOutcomeV1::Present { record_revision_hash: rev_1, matches: true }));
  rechecker.outcomes.insert(key_2, Ok(IndexPartialRecheckOutcomeV1::Present { record_revision_hash: rev_2_target, matches: true }));
  rechecker.outcomes.insert(key_3, Ok(IndexPartialRecheckOutcomeV1::Absent));
  rechecker.outcomes.insert(key_4, Ok(IndexPartialRecheckOutcomeV1::Present { record_revision_hash: rev_4, matches: true }));
  (candidates, changed, rechecker)
}

fn exact(outcome: IndexPartialAccelerationOutcomeV1) -> ExactPartialIndexAccelerationV1 {
  let IndexPartialAccelerationOutcomeV1::Exact(exact) = outcome else {
    panic!("expected exact partial acceleration")
  };
  exact
}

fn fallback(outcome: IndexPartialAccelerationOutcomeV1) -> (IndexPartialAccelerationFallbackReasonV1, IndexPartialAccelerationStageV1) {
  let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, diagnostic } = outcome else {
    panic!("expected authoritative fallback")
  };
  (reason, diagnostic.stage)
}

#[test]
fn exact_partial_execution_rechecks_complements_deduplicates_and_retains_memory() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let case = Case::new(algorithm);
    let (mut candidates, mut changed, mut rechecker) = complete_feeds(algorithm);
    let memory = memory(16 * 1024 * 1024);
    let baseline = memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
    let exact = exact(case.execute(&mut candidates, &mut changed, &mut rechecker, &memory, &CancellationToken::new(), limits()).unwrap());
    assert_eq!(exact.observed_candidate_count(), 4);
    assert_eq!(exact.unique_candidate_count(), 3);
    assert_eq!(exact.rechecked_candidate_count(), 3);
    assert_eq!(exact.overlap_deduplicated_count(), 2);
    assert_eq!(exact.proof().changed_document_count(), 3);
    assert_eq!(exact.proof().generation_manifest_hash(), case.manifest);
    assert_eq!(exact.proof().hash_algorithm(), algorithm);
    assert_eq!(exact.proof().source_namespace_root(), case.source_root);
    assert_eq!(exact.proof().target_namespace_root(), case.target_root);
    assert_eq!(exact.proof().query_fingerprint(), case.query);
    assert_eq!(exact.proof().covered_through_publication_sequence(), 11);
    assert_eq!(exact.proof().target_publication_sequence(), 19);
    assert_eq!(exact.proof().changed_document_set_hash().len(), algorithm.hash_length());
    assert!(exact.proof().changed_document_set_hash().iter().any(|byte| *byte != 0));
    assert_eq!(exact.matches().len(), 3);
    assert_eq!(exact.matches()[0].file_key(), bytes(0x91, algorithm.hash_length()));
    assert_eq!(exact.matches()[0].record_revision_hash(), bytes(0xa1, algorithm.hash_length()));
    assert_eq!(exact.matches()[1].file_key(), bytes(0x92, algorithm.hash_length()));
    assert_eq!(exact.matches()[1].record_revision_hash(), bytes(0xb2, algorithm.hash_length()));
    assert_eq!(exact.matches()[2].file_key(), bytes(0x94, algorithm.hash_length()));
    assert_eq!(rechecker.calls.len(), 4, "changed candidates reuse their exact complement recheck");
    assert_eq!(rechecker.calls.iter().filter(|(_, origin)| *origin == IndexPartialRecheckOriginV1::ChangedDocumentComplement).count(), 3);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, baseline + exact.retained_bytes());
    drop(exact);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, baseline);
  }
}

#[test]
fn empty_exact_complement_and_candidate_universe_return_an_exact_empty_result() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let mut candidates = CandidateFeed { complete: true, ..CandidateFeed::default() };
  let mut changed = ChangedFeed { complete: true, ..ChangedFeed::default() };
  let mut rechecker = Rechecker::default();
  let coordinator = memory(16 * 1024 * 1024);
  let exact =
    exact(case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &CancellationToken::new(), limits()).unwrap());
  assert!(exact.matches().is_empty());
  assert_eq!(exact.proof().changed_document_count(), 0);
  assert!(exact.proof().changed_document_set_hash().iter().any(|byte| *byte != 0));
  assert!(rechecker.calls.is_empty());
  drop(exact);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn planner_partial_candidate_is_never_complete_result_authority() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let plan = case.plan();
  assert!(matches!(plan, IndexCoveragePlanV1::PartialCandidate { .. }));
  assert!(plan.requires_authoritative_complement_scan());
  assert!(plan.requires_candidate_recheck());
  assert!(plan.requires_deduplication());
  assert!(!plan.generation_alone_is_complete());
}

#[test]
fn changed_candidate_basis_mismatch_and_conflicting_candidate_revisions_fall_back_without_results() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  candidates.rows[0].revision = bytes(0xee, case.algorithm.hash_length());
  let memory = memory(16 * 1024 * 1024);
  let outcome = case.execute(&mut candidates, &mut changed, &mut rechecker, &memory, &CancellationToken::new(), limits()).unwrap();
  assert_eq!(
    fallback(outcome),
    (IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt, IndexPartialAccelerationStageV1::CandidateSource)
  );
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  candidates
    .rows
    .push(OwnedCandidate { file_key: bytes(0x91, case.algorithm.hash_length()), revision: bytes(0xef, case.algorithm.hash_length()) });
  let outcome = case.execute(&mut candidates, &mut changed, &mut rechecker, &memory, &CancellationToken::new(), limits()).unwrap();
  assert_eq!(fallback(outcome).0, IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn omitted_changed_document_is_detected_by_candidate_recheck() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let width = case.algorithm.hash_length();
  let key = bytes(0x91, width);
  let old_revision = bytes(0xa1, width);
  let new_revision = bytes(0xb1, width);
  let mut candidates = CandidateFeed {
    rows: vec![OwnedCandidate { file_key: key.clone(), revision: old_revision }],
    complete: true,
    ..CandidateFeed::default()
  };
  let mut changed = ChangedFeed { complete: true, ..ChangedFeed::default() };
  let mut rechecker = Rechecker::default();
  rechecker.outcomes.insert(key, Ok(IndexPartialRecheckOutcomeV1::Present { record_revision_hash: new_revision, matches: true }));
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeComplement);
  assert_eq!(error.code(), "index_partial_recheck_revision_mismatch");
}

#[test]
fn malformed_or_incomplete_complement_evidence_fails_closed() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  changed.rows.swap(0, 1);
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeComplement);
  assert_eq!(error.code(), "index_partial_complement_order");

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  changed.target_override = Some(bytes(0xfe, case.algorithm.hash_length()));
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeComplement);
  assert_eq!(error.code(), "index_partial_complement_receipt");

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  changed.complete = false;
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.code(), "index_partial_complement_receipt");
}

#[test]
fn unavailable_or_over_bound_inputs_choose_typed_authoritative_fallbacks() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  candidates.source_error = Some(IndexPartialSourceErrorV1::unavailable("candidate_offline", "candidate source offline"));
  let outcome =
    case.execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits()).unwrap();
  assert_eq!(
    fallback(outcome),
    (IndexPartialAccelerationFallbackReasonV1::CandidateUnavailable, IndexPartialAccelerationStageV1::CandidateSource)
  );
  assert_eq!(changed.scans, 0, "candidate refusal must avoid complement work");

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  changed.source_error = Some(IndexPartialSourceErrorV1::resource_limit("complement_bound", "changed set exceeds bound"));
  let outcome =
    case.execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits()).unwrap();
  assert_eq!(fallback(outcome).0, IndexPartialAccelerationFallbackReasonV1::ComplementResourceLimit);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  rechecker.outcomes.insert(
    bytes(0x92, case.algorithm.hash_length()),
    Err(IndexPartialSourceErrorV1::unavailable("recheck_offline", "selected-root source unavailable")),
  );
  let outcome =
    case.execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits()).unwrap();
  assert_eq!(fallback(outcome).0, IndexPartialAccelerationFallbackReasonV1::RecheckUnavailable);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let result_limited = IndexPartialAccelerationLimitsV1::new(64, 64, 2, 2 * 1024 * 1024).unwrap();
  let coordinator = memory(16 * 1024 * 1024);
  let outcome =
    case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &CancellationToken::new(), result_limited).unwrap();
  assert_eq!(fallback(outcome), (IndexPartialAccelerationFallbackReasonV1::LocalResourceLimit, IndexPartialAccelerationStageV1::Local));
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn corrupt_authoritative_sources_are_distinct_from_disposable_candidate_corruption() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  candidates.source_error = Some(IndexPartialSourceErrorV1::corrupt("candidate_corrupt", "derived page corrupt"));
  let outcome =
    case.execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits()).unwrap();
  assert_eq!(fallback(outcome).0, IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  changed.source_error = Some(IndexPartialSourceErrorV1::corrupt("diff_corrupt", "namespace diff corrupt"));
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeComplement);
  assert_eq!(error.code(), "diff_corrupt");

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  rechecker.outcomes.insert(
    bytes(0x92, case.algorithm.hash_length()),
    Err(IndexPartialSourceErrorV1::corrupt("recheck_corrupt", "target authority corrupt")),
  );
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeRecheck);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  rechecker.outcomes.insert(
    bytes(0x92, case.algorithm.hash_length()),
    Ok(IndexPartialRecheckOutcomeV1::Present { record_revision_hash: vec![0x01], matches: true }),
  );
  let error = case
    .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
    .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::CorruptAuthoritativeRecheck);
  assert_eq!(error.code(), "index_partial_recheck_revision_hash");
}

#[test]
fn cancellation_and_memory_pressure_release_every_query_byte() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let cancellation = CancellationToken::new();
  candidates.cancel_after_rows = Some(1);
  let coordinator = memory(16 * 1024 * 1024);
  let error = case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &cancellation, limits()).unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Cancelled);
  let owner = coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);

  let mut candidates = CandidateFeed { complete: true, cancel_before_receipt: true, ..CandidateFeed::default() };
  let mut changed = ChangedFeed { complete: true, ..ChangedFeed::default() };
  let mut rechecker = Rechecker::default();
  let cancellation = CancellationToken::new();
  let error = case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &cancellation, limits()).unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Cancelled);
  assert_eq!(changed.scans, 0, "candidate-stage cancellation must stop before complement work");
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let mut candidates = CandidateFeed { complete: true, ..CandidateFeed::default() };
  let mut changed = ChangedFeed { complete: true, cancel_before_receipt: true, ..ChangedFeed::default() };
  let mut rechecker = Rechecker::default();
  let cancellation = CancellationToken::new();
  let error = case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &cancellation, limits()).unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Cancelled);
  assert!(rechecker.calls.is_empty(), "complement-stage cancellation must stop before selected-root rechecks");
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let tiny_limits = IndexPartialAccelerationLimitsV1::new(64, 64, 64, 1024).unwrap();
  let outcome = case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &CancellationToken::new(), tiny_limits).unwrap();
  assert_eq!(fallback(outcome), (IndexPartialAccelerationFallbackReasonV1::LocalResourceLimit, IndexPartialAccelerationStageV1::Local));
  assert_eq!(candidates.scans, 0);
  assert_eq!(changed.scans, 0);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let pressured = MemoryCoordinator::new(MemoryPolicy::new(1024, 2048, 1, 512).unwrap());
  let outcome = case.execute(&mut candidates, &mut changed, &mut rechecker, &pressured, &CancellationToken::new(), limits()).unwrap();
  assert_eq!(fallback(outcome), (IndexPartialAccelerationFallbackReasonV1::LocalResourceLimit, IndexPartialAccelerationStageV1::Local));
  assert_eq!(candidates.scans, 0);
  assert_eq!(pressured.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn malformed_candidate_receipt_is_disposable_corruption_not_partial_success() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  candidates.receipt_manifest_override = Some(bytes(0xfe, case.algorithm.hash_length()));
  let coordinator = memory(16 * 1024 * 1024);
  let outcome = case.execute(&mut candidates, &mut changed, &mut rechecker, &coordinator, &CancellationToken::new(), limits()).unwrap();
  assert_eq!(
    fallback(outcome),
    (IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt, IndexPartialAccelerationStageV1::CandidateSource)
  );
  assert_eq!(changed.scans, 0);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn changed_set_hash_binds_exact_root_interval_and_document_set() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let first = exact(
    case.execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits()).unwrap(),
  );
  let first_hash = first.proof().changed_document_set_hash().to_vec();

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let key = bytes(0x95, case.algorithm.hash_length());
  let revision = bytes(0xb5, case.algorithm.hash_length());
  changed.rows.push(OwnedChange { file_key: key.clone(), basis_revision: None, target_revision: Some(revision.clone()) });
  rechecker.outcomes.insert(key, Ok(IndexPartialRecheckOutcomeV1::Present { record_revision_hash: revision, matches: false }));
  let second = exact(
    case.execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits()).unwrap(),
  );
  assert_ne!(first_hash, second.proof().changed_document_set_hash());
  assert_eq!(second.proof().changed_document_count(), 4);

  let mut alternate_interval = Case::new(case.algorithm);
  alternate_interval.target_root = bytes(0x45, case.algorithm.hash_length());
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  let third = exact(
    alternate_interval
      .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
      .unwrap(),
  );
  assert_ne!(first_hash, third.proof().changed_document_set_hash());
  assert_eq!(third.proof().target_namespace_root(), alternate_interval.target_root);
}

#[test]
fn exact_complement_proof_binds_the_query_even_when_root_and_changed_set_are_identical() {
  let mut first_case = Case::new(HashAlgorithm::Blake3_256);
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(first_case.algorithm);
  let first = exact(
    first_case
      .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
      .unwrap(),
  );
  let first_changed_set = first.proof().changed_document_set_hash().to_vec();
  let first_query = first.proof().query_fingerprint().to_vec();

  first_case.query = bytes(0x89, first_case.algorithm.hash_length());
  let (mut candidates, mut changed, mut rechecker) = complete_feeds(first_case.algorithm);
  let second = exact(
    first_case
      .execute(&mut candidates, &mut changed, &mut rechecker, &memory(16 * 1024 * 1024), &CancellationToken::new(), limits())
      .unwrap(),
  );

  assert_eq!(first_changed_set, second.proof().changed_document_set_hash());
  assert_ne!(first_query, second.proof().query_fingerprint());
  assert_eq!(second.proof().query_fingerprint(), first_case.query);
}

#[test]
fn nvt_is_absent_from_coverage_proof_and_exact_posting_fallback_remains_the_authority() {
  let planner = include_str!("../../src/engine/v4/index_coverage_planner.rs");
  let partial = include_str!("../../src/engine/v4/index_partial_acceleration.rs");
  let nvt = include_str!("../../src/engine/v4/index_nvt.rs");
  assert!(!planner.contains("Nvt"));
  assert!(!partial.contains("Nvt"));
  assert!(nvt.contains("pub fn exact_posting_predecessor_v1"));
  assert!(nvt.contains("resolve_exact_nvt_fallback"));
  assert!(nvt.contains("exact_posting_predecessor_v1(request.field"));
  for forbidden in ["StorageEngine", "V4FirstAuthorityPublisher", "tokio::spawn", "std::thread::spawn", "server::", "axum::"] {
    assert!(!partial.contains(forbidden), "partial executor gained forbidden live/runtime authority: {forbidden}");
  }
}

#[test]
fn source_contract_errors_keep_their_stage_and_class() {
  let error = IndexPartialSourceErrorV1::resource_limit("bounded", "bounded source refusal");
  assert_eq!(error.class(), IndexPartialSourceErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "bounded");
  assert_eq!(error.context(), "bounded source refusal");
}

#[test]
fn internal_source_authority_failures_never_become_authoritative_fallbacks() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let memory = memory(16 * 1024 * 1024);
  let cancellation = CancellationToken::new();

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  candidates.source_error = Some(IndexPartialSourceErrorV1::internal("candidate_internal", "candidate authority failed"));
  let error = case.execute(&mut candidates, &mut changed, &mut rechecker, &memory, &cancellation, limits()).unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Internal);
  assert_eq!(error.code(), "candidate_internal");

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  changed.source_error = Some(IndexPartialSourceErrorV1::internal("complement_internal", "complement authority failed"));
  let error = case.execute(&mut candidates, &mut changed, &mut rechecker, &memory, &cancellation, limits()).unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Internal);
  assert_eq!(error.code(), "complement_internal");

  let (mut candidates, mut changed, mut rechecker) = complete_feeds(case.algorithm);
  rechecker.outcomes.insert(
    bytes(0x92, case.algorithm.hash_length()),
    Err(IndexPartialSourceErrorV1::internal("recheck_internal", "recheck authority failed")),
  );
  let error = case.execute(&mut candidates, &mut changed, &mut rechecker, &memory, &cancellation, limits()).unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Internal);
  assert_eq!(error.code(), "recheck_internal");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}
