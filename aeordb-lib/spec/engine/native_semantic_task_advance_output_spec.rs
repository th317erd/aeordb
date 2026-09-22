//! Completed-output admission rejects correctly bound but invalid output pairs.
use super::*;
use super::advance_boundary::{resumed_request, start_fixture_compilation};
use crate::engine::v4::semantic_mutation_control::{decode_semantic_mutation_checkpoint, encode_semantic_mutation_checkpoint};
use crate::engine::v4::semantic_source_capture::{
  SemanticSourceCaptureV1, decode_semantic_source_capture_v1, encode_semantic_source_capture_v1,
};

fn identity(sequence: u64) -> [u8; 24] {
  let mut identity = [2; 24];
  identity[16..].copy_from_slice(&sequence.to_le_bytes());
  identity
}

fn load_pair(publisher: &V4FirstAuthorityPublisher, sequence: u64) -> [Vec<u8>; 2] {
  [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture]
    .map(|kind| publisher.load_immutable_system_control(kind, &[1; 16], &identity(sequence)).unwrap().unwrap().bytes)
}

fn seed_bound_pair(publisher: &V4FirstAuthorityPublisher, sequence: u64, checkpoint: &[u8], old_companion: &[u8]) {
  let algorithm = HashAlgorithm::Blake3_256;
  let old = decode_semantic_source_capture_v1(old_companion, algorithm).unwrap();
  let hash = digest_parts(algorithm, &[checkpoint]);
  let companion = encode_semantic_source_capture_v1(
    &SemanticSourceCaptureV1 { checkpoint_sequence: sequence, checkpoint_payload_hash: &hash, ..old },
    algorithm,
  )
  .unwrap();
  seed(
    publisher,
    &[
      (SystemControlKindV1::SemanticMutationCheckpoint, &identity(sequence), SystemControlSlotV1::Immutable, checkpoint),
      (SystemControlKindV1::SemanticSourceCapture, &identity(sequence), SystemControlSlotV1::Immutable, &companion),
    ],
  );
}

#[test]
fn native_task_advance_completed_output_refuses_unfinished_and_inconsistent_bindings_read_only() {
  for variant in 0..6 {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      work.advance_compilation(request, fixture.retirement).unwrap();
      let pair = load_pair(fixture.publisher, if variant == 0 { 2 } else { 3 });
      let mut checkpoint = decode_semantic_mutation_checkpoint(&pair[0], HashAlgorithm::Blake3_256).unwrap();
      let changed_profile = [0x99; 32];
      let missing_state = [0x98; 32];
      match variant {
        0 => {}
        1 => checkpoint.record_count += 1,
        2 => checkpoint.dependency_count += 1,
        3 => {
          // Two records permit three Patricia nodes. Keep the envelope valid
          // so completed-output binding, not the codec, must reject this pair.
          checkpoint.record_count += 1;
          checkpoint.node_count += 2;
        }
        4 => checkpoint.compiler_fingerprint = &changed_profile,
        5 => checkpoint.semantic_state = Some(&missing_state),
        _ => unreachable!(),
      }
      let sequence = checkpoint.checkpoint_sequence;
      if variant != 0 {
        let bytes = encode_semantic_mutation_checkpoint(&checkpoint, HashAlgorithm::Blake3_256).unwrap();
        seed_bound_pair(fixture.publisher, sequence, &bytes, &pair[1]);
      }
      let before = fs::read(fixture.path).unwrap();
      let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
      {
        let capture = protection.capture_semantic_mutation_inventory(input.inventory_bounds, fixture.memory, fixture.cancellation).unwrap();
        let error = capture
          .admit_captured_semantic_compiler_output(&[2; 16], sequence, request.compiler_bounds)
          .err()
          .expect("malformed output must not be admitted");
        if variant == 0 {
          assert!(error.to_string().contains("semantic_compiler_output_phase"), "{error}");
        } else if (1..=3).contains(&variant) {
          assert!(error.to_string().contains("semantic_compiler_output_binding"), "variant {variant}: {error}");
        }
      }
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(fixture.path).unwrap(), before);
    });
  }
}

#[test]
fn native_task_advance_reserved_checkpoint_collision_is_not_overwritten_or_skipped() {
  with_initial_task_for_work(|mut fixture| {
    start_fixture_compilation(&mut fixture);
    let pair = load_pair(fixture.publisher, 2);
    let mut checkpoint = decode_semantic_mutation_checkpoint(&pair[0], HashAlgorithm::Blake3_256).unwrap();
    checkpoint.checkpoint_sequence = 3;
    let conflicting = encode_semantic_mutation_checkpoint(&checkpoint, HashAlgorithm::Blake3_256).unwrap();
    seed_bound_pair(fixture.publisher, 3, &conflicting, &pair[1]);
    let existing = load_pair(fixture.publisher, 3);
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = resumed_request(&fixture);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(work.reserved_checkpoint_sequence(), 3);
    let reached = std::cell::Cell::new(false);
    let after_candidate = std::cell::RefCell::new(None);
    let error = work
      .advance_compilation_observed(
        advance_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        (
          || {},
          || {
            reached.set(true);
            *after_candidate.borrow_mut() = Some(fs::read(fixture.path).unwrap());
          },
          || {},
        ),
        (
          &mut NoopFirstAuthorityDependencyObserverV1,
          &mut NoopFirstAuthorityDependencyObserverV1,
          &mut NoopFirstAuthorityDependencyObserverV1,
        ),
      )
      .unwrap_err();
    assert!(reached.get(), "{error}");
    assert!(error.to_string().contains("semantic_source_node_collision"), "{error}");
    assert!(error.committed_receipt().is_none());
    assert!(error.committed_checkpoint_receipt().is_none());
    assert_eq!(load_pair(fixture.publisher, 3), existing);
    assert_eq!(fs::read(fixture.path).unwrap(), *after_candidate.borrow().as_ref().unwrap());
    assert!(fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &identity(4))
      .unwrap()
      .is_none());
    let selected = observe_work_task(&fixture);
    assert_eq!(selected.task().unwrap().unwrap().control_sequence, 4);
    assert_eq!(selected.task().unwrap().unwrap().checkpoint_sequence, 2);
  });
}
