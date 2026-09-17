//! Separate allocator harness child: byte writers own one fallible output only.
use super::{fixture, measure};
use aeordb::engine::v4::semantic_mutation_control::{
  SemanticMutationCheckpointV1, SemanticMutationCursorV1, SemanticMutationPhaseV1, decode_semantic_mutation_checkpoint,
  decode_semantic_mutation_task, encode_semantic_mutation_checkpoint, encode_semantic_mutation_generation, encode_semantic_mutation_task,
};
use aeordb::engine::v4::system_control::decode_system_control;
use aeordb::engine::HashAlgorithm;

#[test]
fn semantic_control_output_and_identity_refusals_are_typed_and_retryable() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for (kind, identity_length) in [("task", 16), ("checkpoint", 24), ("generation", 0)] {
      let expected = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-{kind}-valid"));
      let task = (kind == "task").then(|| decode_semantic_mutation_task(&expected, algorithm).unwrap());
      let checkpoint = (kind == "checkpoint").then(|| decode_semantic_mutation_checkpoint(&expected, algorithm).unwrap());
      let control = decode_system_control(&expected, algorithm).unwrap();
      let encode = || match kind {
        "task" => encode_semantic_mutation_task(task.as_ref().unwrap(), algorithm),
        "checkpoint" => encode_semantic_mutation_checkpoint(checkpoint.as_ref().unwrap(), algorithm),
        "generation" => encode_semantic_mutation_generation(control.database_id, control.sequence, algorithm),
        _ => unreachable!(),
      };
      let (result, allocation) = measure(0, encode);
      assert_eq!(result.unwrap(), expected);
      assert_eq!(allocation.maximum, expected.len());
      assert_eq!(allocation.total, expected.len() + identity_length);
      let (result, allocation) = measure(expected.len(), encode);
      assert!(allocation.injected_failure);
      let error = result.unwrap_err();
      assert!(error.is_allocation_failure());
      assert_eq!(error.code(), "system_control_output_allocation");
      if identity_length != 0 {
        let (result, allocation) = measure(identity_length, encode);
        assert!(allocation.injected_failure);
        let error = result.unwrap_err();
        assert!(error.is_allocation_failure());
        assert_eq!(error.code(), "semantic_task_identity_allocation");
      }
      assert_eq!(encode().unwrap(), expected);
    }
  }
}

#[test]
fn maximum_checkpoint_cursor_allocates_no_second_body_or_count_sized_buffer() {
  let path = format!("/{}", "x".repeat(65_534));
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-checkpoint-valid"));
    let input = SemanticMutationCheckpointV1 {
      phase: SemanticMutationPhaseV1::Compiling,
      cursor: SemanticMutationCursorV1::ConfigurationOwner(&path),
      semantic_state: None,
      candidate_namespace_root: None,
      expected_configuration_count: u64::MAX,
      mutation_count: u64::MAX,
      ..decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap()
    };
    let length = 36 + 168 + 9 * algorithm.hash_length() + path.len();
    let (result, allocation) = measure(0, || encode_semantic_mutation_checkpoint(&input, algorithm));
    let encoded = result.unwrap();
    assert_eq!(encoded.len(), length);
    assert_eq!(allocation.maximum, length);
    assert_eq!(allocation.total, length + 24);
    let decoded = decode_semantic_mutation_checkpoint(&encoded, algorithm).unwrap();
    assert_eq!(decoded.cursor, input.cursor);
    assert_eq!(decoded.expected_configuration_count, u64::MAX);
    assert_eq!(decoded.mutation_count, u64::MAX);
    let (result, allocation) = measure(length, || encode_semantic_mutation_checkpoint(&input, algorithm));
    assert!(allocation.injected_failure);
    assert!(result.unwrap_err().is_allocation_failure());
  }
}

#[test]
fn excessive_cursor_is_rejected_before_attempting_its_output_allocation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("system-control-v1", "control-blake3-256-semantic-mutation-checkpoint-valid");
  let path = format!("/{}", "x".repeat(65_535));
  let input = SemanticMutationCheckpointV1 {
    phase: SemanticMutationPhaseV1::Compiling,
    cursor: SemanticMutationCursorV1::ConfigurationOwner(&path),
    semantic_state: None,
    candidate_namespace_root: None,
    ..decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap()
  };
  let length = 36 + 168 + 9 * algorithm.hash_length() + path.len();
  let (result, allocation) = measure(length, || encode_semantic_mutation_checkpoint(&input, algorithm));
  assert!(!allocation.injected_failure);
  let error = result.unwrap_err();
  assert!(!error.is_allocation_failure());
  assert_eq!(error.code(), "semantic_task_cursor_cap");
}
