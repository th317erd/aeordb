//! Immutable candidate/pair receipts remain distinct from committed task state.
use super::*;
use super::advance_boundary::{resumed_request, start_fixture_compilation};
use super::super::compiler_boundary::FailingTaskReplacementObserver;

fn advance_selection(publisher: &V4FirstAuthorityPublisher) -> LoadedMutableSystemControlV1 {
  publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap()
}

fn advance_identity(sequence: u64) -> [u8; 24] {
  let mut identity = [0; 24];
  identity[..16].fill(2);
  identity[16..].copy_from_slice(&sequence.to_le_bytes());
  identity
}

fn advance_publication_fault(boundary: usize) {
  for committed in [false, true] {
    let (_directory, path) = with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let before = advance_selection(fixture.publisher);
      assert_eq!(before.control_sequence, 4);
      let mut early = FailingVisibilityObserver;
      let mut late = FailingPostCommitObserver;
      let replaced_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
      let replaced_key = first_authority_file_path_hash(&replaced_path, HashAlgorithm::Blake3_256);
      let previous = fixture.publisher.lock_kv().unwrap().snapshot_handle().load().get(&replaced_key).unwrap().unwrap();
      let mut replacement = FailingTaskReplacementObserver { previous, called: false };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if committed {
        &mut late
      } else if boundary == 2 {
        &mut replacement
      } else {
        &mut early
      };
      let mut noop1 = NoopFirstAuthorityDependencyObserverV1;
      let mut noop2 = NoopFirstAuthorityDependencyObserverV1;
      let observers = match boundary {
        0 => {
          (observer, &mut noop1 as &mut dyn FirstAuthorityDependencyObserverV1, &mut noop2 as &mut dyn FirstAuthorityDependencyObserverV1)
        }
        1 => {
          (&mut noop1 as &mut dyn FirstAuthorityDependencyObserverV1, observer, &mut noop2 as &mut dyn FirstAuthorityDependencyObserverV1)
        }
        2 => {
          (&mut noop1 as &mut dyn FirstAuthorityDependencyObserverV1, &mut noop2 as &mut dyn FirstAuthorityDependencyObserverV1, observer)
        }
        _ => unreachable!(),
      };
      let reached = std::cell::Cell::new(0usize);
      let error = work
        .advance_compilation_observed(
          advance_request(fixture.tree, input.publication_timestamp_ms + 20),
          fixture.retirement,
          (|| reached.set(1), || reached.set(2), || reached.set(3)),
          observers,
        )
        .unwrap_err();
      assert_eq!(reached.get(), boundary + 1, "fault did not reach its intended publication: {error}");
      assert_eq!(replacement.called, boundary == 2 && !committed);
      assert_eq!(error.committed_candidate_receipt().is_some(), boundary == 0 && committed, "{error}");
      assert_eq!(error.committed_checkpoint_receipt().is_some(), boundary == 1 && committed, "{error}");
      assert_eq!(error.committed_receipt().is_some(), boundary == 2 && committed, "{error}");
      let current = advance_selection(fixture.publisher);
      if boundary == 2 && committed {
        assert_eq!(current.control_sequence, 5);
        assert_eq!(current.control_digest, error.committed_receipt().unwrap().control_digest);
        assert_eq!(observe_work_task(&fixture).task().unwrap().unwrap().state, SemanticMutationTaskStateV1::ReadyToActivate);
      } else {
        assert_eq!(current, before);
      }
      if let Some(receipt) = error.committed_candidate_receipt() {
        assert_eq!(receipt.entities.len(), 1);
        assert!(fixture
          .publisher
          .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], &receipt.entities[0].key)
          .unwrap()
          .is_none());
      }
    });
    let (_coordinator, publisher) = reopen(&path);
    assert_eq!(advance_selection(&publisher).control_sequence, if boundary == 2 && committed { 5 } else { 4 });
    for kind in [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture] {
      assert_eq!(
        publisher.load_immutable_system_control(kind, &[1; 16], &advance_identity(3)).unwrap().is_some(),
        boundary == 2 || boundary == 1 && committed
      );
    }
  }
}

#[test]
fn native_task_advance_candidate_failure_preserves_distinct_staging_receipt() {
  advance_publication_fault(0);
}

#[test]
fn native_task_advance_checkpoint_failure_preserves_distinct_pair_receipt() {
  advance_publication_fault(1);
}

#[test]
fn native_task_advance_selection_failure_preserves_committed_task_receipt() {
  advance_publication_fault(2);
}

fn advance_cancel_at_boundary(boundary: usize) {
  with_initial_task_for_work(|mut fixture| {
    start_fixture_compilation(&mut fixture);
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = resumed_request(&fixture);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let before = advance_selection(fixture.publisher);
    let bytes = std::cell::RefCell::new(None);
    let at = |target| {
      if target == boundary {
        fixture.cancellation.cancel();
        *bytes.borrow_mut() = Some(fs::read(fixture.path).unwrap());
      }
    };
    let error = work
      .advance_compilation_observed(
        advance_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        (|| at(0), || at(1), || at(2)),
        (
          &mut NoopFirstAuthorityDependencyObserverV1,
          &mut NoopFirstAuthorityDependencyObserverV1,
          &mut NoopFirstAuthorityDependencyObserverV1,
        ),
      )
      .unwrap_err();
    assert!(bytes.borrow().is_some(), "did not reach boundary: {error}");
    assert!(error.committed_candidate_receipt().is_none());
    assert!(error.committed_checkpoint_receipt().is_none());
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), *bytes.borrow().as_ref().unwrap());
    assert_eq!(advance_selection(fixture.publisher), before);
  });
}

#[test]
fn native_task_advance_checks_cancellation_at_each_publication_boundary() {
  for boundary in 0..3 {
    advance_cancel_at_boundary(boundary);
  }
}
