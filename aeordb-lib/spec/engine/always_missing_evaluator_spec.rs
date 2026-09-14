//! A parser-free, always-missing source has no possible value allocation.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_producer_collector::{
  IndexParserExecutorV1, IndexParserExecutionRequestV1, IndexParserOutcomeV1, IndexParserExecutionErrorV1,
};
use aeordb::engine::v4::source_evaluator::{
  AuthoritativeSourceDocumentV1, AuthoritativeSourceEvaluationV1, AuthoritativeSourceEvaluationErrorV1, AuthoritativeSourceEvaluatorV1,
  AuthoritativeSourceMemoryPolicyV1,
};
use aeordb::engine::v4::value_store::decode_value_store_definition;

fn fixture(algorithm: HashAlgorithm, count: u32, maximum_bytes: u64) -> Vec<u8> {
  let profile = if algorithm.hash_length() == 64 { "sha512" } else { "blake3-256" };
  let mut bytes = std::fs::read(format!(
    "{}/spec/fixtures/v4/value-store-definition-v1/avst-{profile}-always-missing-none-valid.bin",
    env!("CARGO_MANIFEST_DIR"),
  ))
  .unwrap();
  let fixed = 32 + algorithm.hash_length();
  bytes[fixed + 36..fixed + 40].copy_from_slice(&count.to_le_bytes());
  bytes[fixed + 48..fixed + 56].copy_from_slice(&maximum_bytes.to_le_bytes());
  bytes
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 24 << 20, 1, 2 << 20).unwrap())
}

fn evaluator<'a>(
  bytes: &'a [u8],
  algorithm: HashAlgorithm,
  memory: MemoryCoordinator,
  policy: AuthoritativeSourceMemoryPolicyV1,
) -> AuthoritativeSourceEvaluatorV1<'a> {
  let definition = decode_value_store_definition(bytes, algorithm).unwrap();
  AuthoritativeSourceEvaluatorV1::from_encoded(bytes, algorithm, definition.scope_id, &definition.value_store_id, memory, policy)
    .unwrap_or_else(|error| panic!("always-missing must not allocate possible value output: {error}"))
}

struct NeverParse;
impl IndexParserExecutorV1 for NeverParse {
  fn parse(&self, _: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    panic!("always-missing cannot call a parser");
  }
}

#[test]
fn always_missing_reports_zero_output_memory_for_both_shared_evaluator_policies() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
      let bytes = fixture(algorithm, 1, 4096);
      let memory = memory();
      let evaluator = evaluator(&bytes, algorithm, memory.clone(), policy);
      assert_eq!(evaluator.maximum_outcome_retained_bytes(), 0);
      drop(evaluator);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn always_missing_ignores_explicit_legacy_unlimited_value_bounds_without_overflow() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
      for (count, maximum_bytes) in [(1, u64::MAX), (u32::MAX, 1), (u32::MAX, u64::MAX)] {
        let bytes = fixture(algorithm, count, maximum_bytes);
        let memory = memory();
        let evaluator = evaluator(&bytes, algorithm, memory.clone(), policy);
        assert_eq!(evaluator.maximum_outcome_retained_bytes(), 0);
        drop(evaluator);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      }
    }
  }
}

#[test]
fn admitted_always_missing_can_finish_at_memory_capacity_and_still_honor_cancellation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
      let bytes = fixture(algorithm, 1, 1);
      let memory = memory();
      let evaluator = evaluator(&bytes, algorithm, memory.clone(), policy);
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let pressure = memory.reserve(MemoryOwner::Task, (22 << 20) - retained, AdmissionClass::Workload).unwrap();
      let mut record = FileRecord::new("/unparsed.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
      record.total_size = u64::MAX;
      let root = vec![1; algorithm.hash_length()];
      let revision = vec![2; algorithm.hash_length()];
      let document = AuthoritativeSourceDocumentV1 { namespace_root: &root, record_revision_hash: &revision, file_record: &record };
      assert!(matches!(evaluator.evaluate(document, &NeverParse, None, &|| true), Err(AuthoritativeSourceEvaluationErrorV1::Cancelled)));
      assert!(matches!(evaluator.evaluate(document, &NeverParse, None, &|| false), Ok(AuthoritativeSourceEvaluationV1::Missing)));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 22 << 20);
      drop(pressure);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      drop(evaluator);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn parser_free_metadata_still_accounts_for_values_and_obeys_memory_admission() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
      let profile = if algorithm.hash_length() == 64 { "sha512" } else { "blake3-256" };
      let bytes = std::fs::read(format!(
        "{}/spec/fixtures/v4/value-store-definition-v1/avst-{profile}-metadata-hash-corrected-valid.bin",
        env!("CARGO_MANIFEST_DIR"),
      ))
      .unwrap();
      let memory = memory();
      let evaluator = evaluator(&bytes, algorithm, memory.clone(), policy);
      assert!(evaluator.maximum_outcome_retained_bytes() > 0);
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let pressure = memory.reserve(MemoryOwner::Task, (22 << 20) - retained, AdmissionClass::Workload).unwrap();
      let mut record = FileRecord::new("/metadata.bin".to_string(), None, 0, Vec::new());
      record.content_hash = vec![0x57; algorithm.hash_length()];
      let root = vec![1; algorithm.hash_length()];
      let revision = vec![2; algorithm.hash_length()];
      let document = AuthoritativeSourceDocumentV1 { namespace_root: &root, record_revision_hash: &revision, file_record: &record };
      assert!(matches!(
        evaluator.evaluate(document, &NeverParse, None, &|| false),
        Err(AuthoritativeSourceEvaluationErrorV1::ResourcePressure(_))
      ));
      drop(pressure);
      let outcome = evaluator.evaluate(document, &NeverParse, None, &|| false).unwrap();
      match &outcome {
        AuthoritativeSourceEvaluationV1::Values { values, .. } => {
          assert_eq!(values.len(), 1);
          assert!(values[0].ends_with(&record.content_hash));
        }
        _ => panic!("metadata must not become always-missing just because its parser is none"),
      }
      drop(outcome);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      drop(evaluator);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn always_missing_preserves_definition_identity_validation_and_initial_runtime_admission() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = fixture(algorithm, 1, 1);
    let definition = decode_value_store_definition(&bytes, algorithm).unwrap();
    let wrong_identity = vec![0xa7; algorithm.hash_length()];
    for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
      for identity in 0..2 {
        let memory = memory();
        let result = AuthoritativeSourceEvaluatorV1::from_encoded(
          &bytes,
          algorithm,
          if identity == 0 { &wrong_identity } else { definition.scope_id },
          if identity == 1 { &wrong_identity } else { &definition.value_store_id },
          memory.clone(),
          policy,
        );
        assert!(matches!(result, Err(AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration { .. })));
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      }
      let mut malformed = bytes.clone();
      malformed[12] = 1;
      let memory = memory();
      let result = AuthoritativeSourceEvaluatorV1::from_encoded(
        &malformed,
        algorithm,
        definition.scope_id,
        &definition.value_store_id,
        memory.clone(),
        policy,
      );
      assert!(matches!(result, Err(AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration { .. })));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      let small = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 1, 1 << 20).unwrap());
      let result = AuthoritativeSourceEvaluatorV1::from_encoded(
        &bytes,
        algorithm,
        definition.scope_id,
        &definition.value_store_id,
        small.clone(),
        policy,
      );
      assert!(matches!(result, Err(AuthoritativeSourceEvaluationErrorV1::ResourcePressure(_))));
      assert_eq!(small.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}
