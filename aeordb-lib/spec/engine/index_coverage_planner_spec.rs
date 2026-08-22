use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_coverage_planner::{
  IndexAuthoritativeFallbackReasonV1, IndexCoverageGenerationHealthV1, IndexCoverageGenerationV1, IndexCoveragePlanV1,
  IndexCoveragePlanningErrorClassV1, IndexCoveragePlanningRequestV1, IndexHistoricalViewUnavailableReasonV1,
  IndexSemanticQueryAvailabilityV1, plan_selected_index_coverage_v1,
};
use aeordb::engine::v4::namespace::SemanticUnavailableReasonV1;

struct Case {
  algorithm: HashAlgorithm,
  requested_root: Vec<u8>,
  source_root: Vec<u8>,
  owner_id: Vec<u8>,
  manifest_hash: Vec<u8>,
  definition_fingerprint: Vec<u8>,
  dependency_fingerprint: Vec<u8>,
  epoch_id: [u8; 16],
}

impl Case {
  fn new(algorithm: HashAlgorithm) -> Self {
    let width = algorithm.hash_length();
    Self {
      algorithm,
      requested_root: vec![0x11; width],
      source_root: vec![0x12; width],
      owner_id: vec![0x21; width],
      manifest_hash: vec![0x31; width],
      definition_fingerprint: vec![0x41; width],
      dependency_fingerprint: vec![0x51; width],
      epoch_id: [0x61; 16],
    }
  }

  fn generation(&self, health: IndexCoverageGenerationHealthV1) -> IndexCoverageGenerationV1<'_> {
    IndexCoverageGenerationV1 {
      generation: 7,
      owner_id: &self.owner_id,
      manifest_hash: &self.manifest_hash,
      source_namespace_root: &self.source_root,
      coverage_epoch_id: &self.epoch_id,
      coverage_publication_sequence: 40,
      definition_fingerprint: &self.definition_fingerprint,
      dependency_fingerprint: &self.dependency_fingerprint,
      health,
    }
  }

  fn request<'a>(&'a self, selected_generation: Option<IndexCoverageGenerationV1<'a>>) -> IndexCoveragePlanningRequestV1<'a> {
    IndexCoveragePlanningRequestV1 {
      hash_algorithm: self.algorithm,
      requested_namespace_root: &self.requested_root,
      requested_publication_sequence: 50,
      required_owner_id: &self.owner_id,
      required_definition_fingerprint: &self.definition_fingerprint,
      required_dependency_fingerprint: &self.dependency_fingerprint,
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      selected_generation,
    }
  }
}

#[test]
fn exact_compatible_generation_is_complete_even_when_the_runtime_is_degraded() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha3_512] {
    let mut case = Case::new(algorithm);
    case.source_root.clone_from(&case.requested_root);
    let plan = plan_selected_index_coverage_v1(&case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Degraded)))).unwrap();
    let IndexCoveragePlanV1::Complete { generation } = plan else {
      panic!("exact immutable coverage was not accepted for {algorithm:?}");
    };
    assert_eq!(generation.generation, 7);
    assert_eq!(generation.manifest_hash, case.manifest_hash);
    assert!(!plan.requires_candidate_recheck());
    assert!(!plan.requires_deduplication());
    assert!(plan.generation_alone_is_complete());
  }
}

#[test]
fn an_older_generation_is_only_a_partial_candidate_for_the_exact_executor() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let plan = plan_selected_index_coverage_v1(&case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Healthy)))).unwrap();
  let IndexCoveragePlanV1::PartialCandidate { generation, target_namespace_root, target_publication_sequence } = &plan else {
    panic!("older compatible generation was not retained as an executor-only candidate");
  };
  assert_eq!(generation.manifest_hash, case.manifest_hash);
  assert_eq!(*target_namespace_root, case.requested_root);
  assert_eq!(*target_publication_sequence, 50);
  assert!(plan.requires_authoritative_complement_scan());
  assert!(plan.requires_candidate_recheck());
  assert!(plan.requires_deduplication());
  assert!(!plan.generation_alone_is_complete());
}

#[test]
fn degraded_partial_generation_is_ignored_for_authoritative_evaluation() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let degraded = plan_selected_index_coverage_v1(&case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Degraded)))).unwrap();
  assert_eq!(degraded, IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::DegradedPartialGeneration });
}

#[test]
fn absent_or_semantically_incompatible_generations_fall_back_without_claiming_partial_results() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  assert_eq!(
    plan_selected_index_coverage_v1(&case.request(None)).unwrap(),
    IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::NoSelectedGeneration }
  );

  let foreign_owner = vec![0x97; case.algorithm.hash_length()];
  let request = IndexCoveragePlanningRequestV1 {
    required_owner_id: &foreign_owner,
    ..case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Healthy)))
  };
  assert_eq!(
    plan_selected_index_coverage_v1(&request).unwrap(),
    IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::IncompatibleOwner }
  );

  let foreign_definition = vec![0x99; case.algorithm.hash_length()];
  let request = IndexCoveragePlanningRequestV1 {
    required_definition_fingerprint: &foreign_definition,
    ..case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Healthy)))
  };
  assert_eq!(
    plan_selected_index_coverage_v1(&request).unwrap(),
    IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::IncompatibleDefinition }
  );

  let foreign_dependency = vec![0x98; case.algorithm.hash_length()];
  let request = IndexCoveragePlanningRequestV1 {
    required_dependency_fingerprint: &foreign_dependency,
    ..case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Healthy)))
  };
  assert_eq!(
    plan_selected_index_coverage_v1(&request).unwrap(),
    IndexCoveragePlanV1::AuthoritativeOnly { reason: IndexAuthoritativeFallbackReasonV1::IncompatibleDependencies }
  );
}

#[test]
fn content_only_and_unavailable_dependencies_never_borrow_current_semantics() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let content_only = IndexCoveragePlanningRequestV1 {
    semantic_availability: IndexSemanticQueryAvailabilityV1::ContentOnly(SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
    ..case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Healthy)))
  };
  assert_eq!(
    plan_selected_index_coverage_v1(&content_only).unwrap(),
    IndexCoveragePlanV1::HistoricalViewUnavailable {
      reason: IndexHistoricalViewUnavailableReasonV1::ContentOnly(SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
    }
  );

  let dependency_unavailable = IndexCoveragePlanningRequestV1 {
    semantic_availability: IndexSemanticQueryAvailabilityV1::DependencyUnavailable,
    ..case.request(Some(case.generation(IndexCoverageGenerationHealthV1::Healthy)))
  };
  assert_eq!(
    plan_selected_index_coverage_v1(&dependency_unavailable).unwrap(),
    IndexCoveragePlanV1::HistoricalViewUnavailable { reason: IndexHistoricalViewUnavailableReasonV1::DependencyUnavailable }
  );
}

#[test]
fn malformed_or_nonhistorical_generation_evidence_fails_closed() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let zero_owner = vec![0; case.algorithm.hash_length()];
  let malformed_generation =
    IndexCoverageGenerationV1 { owner_id: &zero_owner, ..case.generation(IndexCoverageGenerationHealthV1::Healthy) };
  let error = plan_selected_index_coverage_v1(&case.request(Some(malformed_generation))).unwrap_err();
  assert_eq!(error.class(), IndexCoveragePlanningErrorClassV1::CorruptSelectedGeneration);

  let nonhistorical =
    IndexCoverageGenerationV1 { coverage_publication_sequence: 50, ..case.generation(IndexCoverageGenerationHealthV1::Healthy) };
  let error = plan_selected_index_coverage_v1(&case.request(Some(nonhistorical))).unwrap_err();
  assert_eq!(error.class(), IndexCoveragePlanningErrorClassV1::CorruptSelectedGeneration);
  assert_eq!(error.code(), "index_coverage_nonhistorical_partial_generation");
}

#[test]
fn malformed_request_or_future_exact_generation_fails_closed() {
  let case = Case::new(HashAlgorithm::Blake3_256);
  let zero_root = vec![0; case.algorithm.hash_length()];
  for request in [
    IndexCoveragePlanningRequestV1 { requested_namespace_root: &zero_root, ..case.request(None) },
    IndexCoveragePlanningRequestV1 { requested_publication_sequence: 0, ..case.request(None) },
  ] {
    let error = plan_selected_index_coverage_v1(&request).unwrap_err();
    assert_eq!(error.class(), IndexCoveragePlanningErrorClassV1::InvalidRequest);
  }

  let mut exact = Case::new(HashAlgorithm::Blake3_256);
  exact.source_root.clone_from(&exact.requested_root);
  let future = IndexCoverageGenerationV1 {
    source_namespace_root: &exact.requested_root,
    coverage_publication_sequence: 51,
    ..exact.generation(IndexCoverageGenerationHealthV1::Healthy)
  };
  assert_eq!(
    plan_selected_index_coverage_v1(&exact.request(Some(future))).unwrap_err().class(),
    IndexCoveragePlanningErrorClassV1::CorruptSelectedGeneration
  );
}

#[test]
fn the_coverage_planner_has_no_nvt_input_or_correctness_dependency() {
  let source = include_str!("../../src/engine/v4/index_coverage_planner.rs");
  assert!(!source.contains("index_nvt"));
  assert!(!source.contains("Nvt"));
}
