//! Source/compilation admission failures preserve reserved task authority.
use super::*;
use super::advance_boundary::{resumed_request, start_fixture_compilation};

#[test]
fn native_task_advance_enforces_exact_publication_workspace_boundary() {
  // Literal envelope fields plus four64KiB overlapping controls, sixteen
  // publication projections and256KiB scratch; no production sizing helper.
  let exact = (3 * 36 + 168 + 112 + 112 + 16 * 32 + 4 * 65_536) * 16 + 262_144;
  assert_eq!(exact, 4_472_640);
  for admitted in [false, true] {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let mut request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      request.maximum_workspace_bytes = if admitted { exact } else { exact - 1 };
      let before = fs::read(fixture.path).unwrap();
      let result = work.advance_compilation(request, fixture.retirement);
      if admitted {
        assert_eq!(result.unwrap().phase, SemanticMutationPhaseV1::Ready);
      } else {
        assert_eq!(result.unwrap_err().code(), "semantic_task_work_workspace");
        assert_eq!(fs::read(fixture.path).unwrap(), before);
      }
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
    });
  }
}

#[test]
fn native_task_advance_selected_checkpoint_allocation_failure_preserves_authority() {
  with_initial_task_for_work(|mut fixture| {
    start_fixture_compilation(&mut fixture);
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = resumed_request(&fixture);
    let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let before = fs::read(fixture.path).unwrap();
    // ASMC envelope36 + fixed168 + nine32-byte identities, without cursor.
    let (result, allocations) =
      measure(492, || work.advance_compilation(advance_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement));
    assert!(allocations.injected_failure, "{allocations:?}");
    let error = result.unwrap_err();
    assert!(error.committed_receipt().is_none());
    assert!(error.committed_checkpoint_receipt().is_none());
    assert!(error.committed_candidate_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
  });
}

#[test]
fn native_task_advance_read_and_compiler_limits_refuse_before_writes() {
  for variant in 0..6 {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let mut request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      match variant {
        0 => request.compiler_bounds.maximum_semantic_decode_workspace_bytes = 1,
        1 => request.compiler_bounds.maximum_compiler_workspace_bytes = 1,
        2 => request.compiler_bounds.sources.catalog.maximum_work = 0,
        3 => request.compiler_bounds.sources.namespace.maximum_work = 0,
        4 => request.compiler_bounds.sources.catalog.maximum_read_bytes = 1,
        5 => request.compiler_bounds.sources.namespace.sources.maximum_read_bytes = 1,
        _ => unreachable!(),
      }
      let before = fs::read(fixture.path).unwrap();
      let error = work.advance_compilation(request, fixture.retirement).unwrap_err();
      assert!(error.committed_receipt().is_none(), "variant {variant}: {error}");
      assert!(error.committed_checkpoint_receipt().is_none());
      assert!(error.committed_candidate_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
    });
  }
}

#[test]
fn native_task_advance_missing_retained_objects_refuses_without_writes() {
  for variant in 0..4 {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let checkpoint = observed.checkpoint().unwrap().unwrap();
      let mut identity = [0; 24];
      identity[..16].fill(2);
      identity[16..].copy_from_slice(&2u64.to_le_bytes());
      let key = match variant {
        0 => checkpoint.base_namespace_root.to_vec(),
        1 => first_authority_file_path_hash(
          &system_control_path(SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable).unwrap(),
          HashAlgorithm::Blake3_256,
        ),
        2 => first_authority_file_path_hash(
          &crate::engine::v4::semantic_store::semantic_object_path(HashAlgorithm::Blake3_256, 2, checkpoint.catalog_root.unwrap()).unwrap(),
          HashAlgorithm::Blake3_256,
        ),
        3 => checkpoint.staged_directory_root.to_vec(),
        _ => unreachable!(),
      };
      assert!(fixture.publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
      seed_files(fixture.publisher, &[]);
      let before = fs::read(fixture.path).unwrap();
      let error = work
        .advance_compilation(
          advance_request(fixture.tree, fixture.publisher.observe().unwrap().selected.header.updated_at_ms + 20),
          fixture.retirement,
        )
        .unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
      assert_eq!(
        fixture
          .publisher
          .load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16])
          .unwrap()
          .unwrap()
          .control_sequence,
        4
      );
    });
  }
}

#[test]
fn native_task_advance_late_cancellation_preserves_original_task_commit_semantics() {
  for cancel_retirement in [false, true] {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let attempt = if cancel_retirement { fixture.cancellation.clone() } else { CancellationToken::new() };
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, &attempt, fixture.retirement).unwrap();
      let mut observer = CancelRetirementAfterCommitObserver { cancellation: attempt.clone() };
      let result = work.advance_compilation_observed(
        advance_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        (|| {}, || {}, || {}),
        (&mut NoopFirstAuthorityDependencyObserverV1, &mut NoopFirstAuthorityDependencyObserverV1, &mut observer),
      );
      assert!(attempt.is_cancelled());
      assert_eq!(fixture.cancellation.is_cancelled(), cancel_retirement);
      let receipt = if cancel_retirement {
        let error = result.unwrap_err();
        assert_eq!(error.code(), "mutable_control_retirement_flush");
        assert!(error.committed_candidate_receipt().is_none());
        assert!(error.committed_checkpoint_receipt().is_none());
        let receipt = error.committed_receipt().expect("postcommit retirement cancellation must retain the task receipt").clone();
        assert!(receipt.retirement_hard_publication_sequence.is_none());
        receipt
      } else {
        let receipt = result.unwrap().publication;
        assert!(receipt.retirement_hard_publication_sequence.is_some());
        receipt
      };
      assert_eq!(receipt.control_sequence, 5);
      let selected =
        fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap();
      assert_eq!(selected.control_digest, receipt.control_digest);
    });
  }
}
