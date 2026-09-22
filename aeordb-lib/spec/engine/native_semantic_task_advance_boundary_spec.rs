//! Callable advance refusals must precede any immutable or mutable write.
use super::*;

pub(super) fn start_fixture_compilation(fixture: &mut TaskWorkFixture<'_>) {
  let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
  let observed = observe_work_task(fixture);
  let input = work_request(observed.header().selected.header.updated_at_ms + 1);
  let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
  work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
}

pub(super) fn resumed_request(fixture: &TaskWorkFixture<'_>) -> NativeSemanticTaskWorkRequestV1 {
  NativeSemanticTaskWorkRequestV1 {
    monotonic_now_ms: 35_000,
    ..work_request(fixture.publisher.observe().unwrap().selected.header.updated_at_ms + 1)
  }
}

#[test]
fn native_task_advance_refuses_captured_work_without_starting_it() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let before = fs::read(fixture.path).unwrap();
    let error =
      work.advance_compilation(advance_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap_err();
    assert_eq!(error.code(), "semantic_task_work_phase");
    assert!(error.committed_receipt().is_none());
    assert!(error.committed_checkpoint_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
  });
}

#[test]
fn native_task_advance_refuses_invalid_steps_clocks_and_workspace_before_writing() {
  for variant in 0..8 {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let mut request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      let expected = match variant {
        0 => {
          request.maximum_configuration_steps = 0;
          "semantic_task_advance_steps"
        }
        1 => {
          request.maximum_pruning_steps = 0;
          "semantic_task_advance_steps"
        }
        2 => {
          request.publication_timestamp_ms = 0;
          "semantic_task_work_time"
        }
        3 => {
          request.publication_timestamp_ms = i64::MAX as u64 + 1;
          "semantic_task_work_time"
        }
        4 => {
          request.publication_timestamp_ms = input.publication_timestamp_ms - 1;
          "semantic_task_work_time"
        }
        5 => {
          request.monotonic_now_ms = 0;
          "semantic_task_work_time"
        }
        6 => {
          request.monotonic_now_ms = input.monotonic_now_ms - 1;
          "semantic_task_work_time"
        }
        7 => {
          request.maximum_workspace_bytes = 1;
          "semantic_task_work_workspace"
        }
        _ => unreachable!(),
      };
      let before = fs::read(fixture.path).unwrap();
      let error = work.advance_compilation(request, fixture.retirement).unwrap_err();
      assert_eq!(error.code(), expected, "variant {variant}: {error}");
      assert!(error.committed_receipt().is_none());
      assert!(error.committed_checkpoint_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
    });
  }
}

#[test]
fn native_task_advance_refuses_superseded_reservation_without_staging() {
  with_initial_task_for_work(|mut fixture| {
    start_fixture_compilation(&mut fixture);
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = resumed_request(&fixture);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let newer = observe_work_task(&fixture);
    let newer_request = resumed_request(&fixture);
    let replacement =
      protection.begin_semantic_task_work(&newer, newer_request, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(replacement.reserved_checkpoint_sequence(), 4);
    drop(replacement);
    let before = fs::read(fixture.path).unwrap();
    let error =
      work.advance_compilation(advance_request(fixture.tree, newer_request.publication_timestamp_ms + 20), fixture.retirement).unwrap_err();
    assert_eq!(error.code(), "semantic_task_work_conflict");
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
  });
}
