use aeordb::engine::durability_coordinator::{
  CommitClass, DurabilityCommitPlan, DurabilityCoordinator, DurabilityExecutor, DurabilityOperation, DurabilityWaiterState,
};

#[derive(Default)]
struct RecordingExecutor {
  operations: Vec<(u64, DurabilityOperation)>,
  fail_at: Option<DurabilityOperation>,
}

impl DurabilityExecutor for RecordingExecutor {
  type Error = &'static str;

  fn execute(&mut self, sequence: u64, operation: DurabilityOperation) -> Result<(), Self::Error> {
    self.operations.push((sequence, operation));
    if self.fail_at == Some(operation) {
      return Err("injected durability failure");
    }
    Ok(())
  }
}

fn header_commit_plan() -> DurabilityCommitPlan {
  DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![
      DurabilityOperation::DependencyAppend,
      DurabilityOperation::DataBarrier,
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::HeaderAb,
      DurabilityOperation::AuthorityBarrier,
      DurabilityOperation::AuthorityReadback,
    ],
  )
  .unwrap()
}

#[test]
fn frozen_commit_classes_and_operation_ids_match_the_ratified_contract() {
  assert_eq!(CommitClass::ALL, [CommitClass::HardAuthority, CommitClass::RecoverableSoftState, CommitClass::Disposable]);
  assert_eq!(DurabilityOperation::ALL.len(), 15);
  for (expected, operation) in (1u16..=15).zip(DurabilityOperation::ALL) {
    assert_eq!(operation.stable_id(), expected);
  }
}

#[test]
fn hard_plan_rejects_missing_or_misordered_proof_steps() {
  let missing_barrier = DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![
      DurabilityOperation::DependencyAppend,
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::HeaderAb,
      DurabilityOperation::AuthorityBarrier,
      DurabilityOperation::AuthorityReadback,
    ],
  );
  assert!(missing_barrier.is_err());

  let early_readback = DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![
      DurabilityOperation::DependencyAppend,
      DurabilityOperation::DataBarrier,
      DurabilityOperation::AuthorityReadback,
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::HeaderAb,
      DurabilityOperation::AuthorityBarrier,
    ],
  );
  assert!(early_readback.is_err());

  let missing_parent_sync = DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![
      DurabilityOperation::DependencyAppend,
      DurabilityOperation::DataBarrier,
      DurabilityOperation::DurableReplace,
      DurabilityOperation::AuthorityBarrier,
      DurabilityOperation::AuthorityReadback,
    ],
  );
  assert!(missing_parent_sync.is_err());

  assert!(DurabilityCommitPlan::new(CommitClass::RecoverableSoftState, vec![DurabilityOperation::AuthorityWrite]).is_err());
  assert!(DurabilityCommitPlan::new(CommitClass::Disposable, vec![DurabilityOperation::DataBarrier]).is_err());
}

#[test]
fn hard_waiters_only_succeed_after_their_exact_frontier_is_proven() {
  let coordinator = DurabilityCoordinator::new();
  let first = coordinator.admit(header_commit_plan()).unwrap();
  let second = coordinator.admit(header_commit_plan()).unwrap();
  assert!(first.sequence() < second.sequence());

  let mut second_executor = RecordingExecutor::default();
  coordinator.execute(second, &mut second_executor).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(matches!(coordinator.waiter_state(second).unwrap(), DurabilityWaiterState::Pending));

  let mut first_executor = RecordingExecutor::default();
  coordinator.execute(first, &mut first_executor).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, second.sequence());
  assert!(matches!(coordinator.waiter_state(first).unwrap(), DurabilityWaiterState::Succeeded(_)));
  assert!(matches!(coordinator.waiter_state(second).unwrap(), DurabilityWaiterState::Succeeded(_)));

  let expected: Vec<_> = header_commit_plan().operations().iter().copied().map(|operation| (first.sequence(), operation)).collect();
  assert_eq!(first_executor.operations, expected);
}

#[test]
fn failed_hard_operation_stops_the_ledger_and_never_advances_the_frontier() {
  let coordinator = DurabilityCoordinator::new();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  let mut executor = RecordingExecutor { operations: Vec::new(), fail_at: Some(DurabilityOperation::AuthorityBarrier) };

  let error = coordinator.execute(ticket, &mut executor).unwrap_err();
  assert_eq!(error.operation(), Some(DurabilityOperation::AuthorityBarrier));
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  let DurabilityWaiterState::Failed(failure) = coordinator.waiter_state(ticket).unwrap() else {
    panic!("failed hard waiter was not failed")
  };
  assert_eq!(failure.operation, DurabilityOperation::AuthorityBarrier);
  assert_eq!(executor.operations.last().unwrap().1, DurabilityOperation::AuthorityBarrier);
  assert!(!executor.operations.iter().any(|(_, operation)| *operation == DurabilityOperation::AuthorityReadback));
}

#[test]
fn soft_and_disposable_work_never_move_the_hard_frontier() {
  let coordinator = DurabilityCoordinator::new();
  let soft = coordinator
    .admit(
      DurabilityCommitPlan::new(
        CommitClass::RecoverableSoftState,
        vec![DurabilityOperation::DependencyAppend, DurabilityOperation::DataBarrier],
      )
      .unwrap(),
    )
    .unwrap();
  let disposable = coordinator.admit(DurabilityCommitPlan::new(CommitClass::Disposable, Vec::new()).unwrap()).unwrap();

  coordinator.execute(soft, &mut RecordingExecutor::default()).unwrap();
  coordinator.execute(disposable, &mut RecordingExecutor::default()).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(matches!(coordinator.waiter_state(soft).unwrap(), DurabilityWaiterState::Succeeded(_)));
  assert!(matches!(coordinator.waiter_state(disposable).unwrap(), DurabilityWaiterState::Succeeded(_)));
}

#[test]
fn tickets_are_bound_to_their_coordinator_and_cannot_execute_twice() {
  let first = DurabilityCoordinator::new();
  let second = DurabilityCoordinator::new();
  let ticket = first.admit(header_commit_plan()).unwrap();

  assert!(second.execute(ticket, &mut RecordingExecutor::default()).is_err());
  first.execute(ticket, &mut RecordingExecutor::default()).unwrap();
  assert!(first.execute(ticket, &mut RecordingExecutor::default()).is_err());
}

#[test]
fn terminal_waiters_can_be_retired_without_leaking_commit_records() {
  let coordinator = DurabilityCoordinator::new();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  coordinator.execute(ticket, &mut RecordingExecutor::default()).unwrap();

  assert!(matches!(coordinator.take_waiter_state(ticket).unwrap(), DurabilityWaiterState::Succeeded(_)));
  assert!(coordinator.waiter_state(ticket).is_err());
  assert_eq!(coordinator.snapshot().unwrap().proven, 0);
}

#[test]
fn operation_ledger_is_bounded_and_zero_capacity_is_rejected() {
  assert!(DurabilityCoordinator::with_ledger_capacity(0).is_err());
  let coordinator = DurabilityCoordinator::with_ledger_capacity(2).unwrap();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  coordinator.execute(ticket, &mut RecordingExecutor::default()).unwrap();

  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.ledger.len(), 2);
  assert_eq!(snapshot.ledger[0].operation, DurabilityOperation::AuthorityBarrier);
  assert_eq!(snapshot.ledger[1].operation, DurabilityOperation::AuthorityReadback);
}

struct PanicExecutor;

impl DurabilityExecutor for PanicExecutor {
  type Error = &'static str;

  fn execute(&mut self, _sequence: u64, operation: DurabilityOperation) -> Result<(), Self::Error> {
    if operation == DurabilityOperation::AuthorityWrite {
      panic!("injected executor panic");
    }
    Ok(())
  }
}

#[test]
fn executor_unwind_fails_the_current_operation_instead_of_stranding_it() {
  let coordinator = DurabilityCoordinator::new();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| coordinator.execute(ticket, &mut PanicExecutor)));
  assert!(unwind.is_err());

  let DurabilityWaiterState::Failed(failure) = coordinator.waiter_state(ticket).unwrap() else {
    panic!("unwound executor did not fail its waiter")
  };
  assert_eq!(failure.operation, DurabilityOperation::AuthorityWrite);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
}

#[test]
fn malformed_plan_variants_fail_closed() {
  assert!(DurabilityCommitPlan::new(CommitClass::HardAuthority, Vec::new()).is_err());
  assert!(DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::AuthorityBarrier,
      DurabilityOperation::AuthorityReadback,
    ],
  )
  .is_err());
  assert!(DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![DurabilityOperation::AuthorityBarrier, DurabilityOperation::AuthorityReadback],
  )
  .is_err());
  assert!(DurabilityCommitPlan::new(
    CommitClass::HardAuthority,
    vec![DurabilityOperation::HeaderAb, DurabilityOperation::AuthorityBarrier, DurabilityOperation::AuthorityReadback,],
  )
  .is_err());
  assert!(DurabilityCommitPlan::new(
    CommitClass::RecoverableSoftState,
    vec![DurabilityOperation::DataBarrier, DurabilityOperation::DependencyAppend],
  )
  .is_err());
}

#[test]
fn earlier_failed_hard_sequence_blocks_later_success_from_crossing_the_frontier() {
  let coordinator = DurabilityCoordinator::new();
  let failed = coordinator.admit(header_commit_plan()).unwrap();
  let later = coordinator.admit(header_commit_plan()).unwrap();
  let mut failing = RecordingExecutor { operations: Vec::new(), fail_at: Some(DurabilityOperation::DataBarrier) };
  assert!(coordinator.execute(failed, &mut failing).is_err());
  coordinator.execute(later, &mut RecordingExecutor::default()).unwrap();

  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(matches!(coordinator.waiter_state(failed).unwrap(), DurabilityWaiterState::Failed(_)));
  assert!(matches!(coordinator.waiter_state(later).unwrap(), DurabilityWaiterState::Pending));
}

#[test]
fn concurrent_execution_cannot_make_the_frontier_skip_an_unproven_ticket() {
  let coordinator = std::sync::Arc::new(DurabilityCoordinator::new());
  let tickets: Vec<_> = (0..32).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let withheld = tickets[0];

  std::thread::scope(|scope| {
    for ticket in tickets[1..].iter().rev().copied() {
      let coordinator = coordinator.clone();
      scope.spawn(move || coordinator.execute(ticket, &mut RecordingExecutor::default()).unwrap());
    }
  });
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);

  coordinator.execute(withheld, &mut RecordingExecutor::default()).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, tickets.last().unwrap().sequence());
}
