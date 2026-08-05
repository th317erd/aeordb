use aeordb::engine::durability_coordinator::{
  CommitClass, DurabilityCommitPlan, DurabilityCoordinator, DurabilityExecutor, DurabilityFailureDisposition, DurabilityGroupExecutor,
  DEFAULT_GROUP_COMMIT_MAX_BYTES, DEFAULT_GROUP_COMMIT_MAX_DELAY, DurabilityGroupPolicy, DurabilityHardTurn, DurabilityOperation,
  DurabilityWaiterState, NativeFileBarrierKind, OsErrorClass, RetryClass, classify_io_error, classify_native_durability_error,
};
use aeordb::engine::native_durability::{platform_file_identity, preallocate_file};
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::kv_store::KV_TYPE_CHUNK;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::{DirectoryOps, RequestContext};

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
  assert!(coordinator.execute(second, &mut second_executor).is_err());
  assert!(second_executor.operations.is_empty());
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(matches!(coordinator.waiter_state(second).unwrap(), DurabilityWaiterState::Pending));

  let mut grouped_executor = RecordingGroupExecutor::default();
  coordinator.execute_group(&[second, first], &mut grouped_executor).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, second.sequence());
  assert!(matches!(coordinator.waiter_state(first).unwrap(), DurabilityWaiterState::Succeeded(_)));
  assert!(matches!(coordinator.waiter_state(second).unwrap(), DurabilityWaiterState::Succeeded(_)));

  let expected_sequences = vec![first.sequence(), second.sequence()];
  assert!(grouped_executor.calls.iter().all(|(sequences, _)| sequences == &expected_sequences));
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
  let first = coordinator.admit(header_commit_plan()).unwrap();
  let later = coordinator.admit(header_commit_plan()).unwrap();
  let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| coordinator.execute(first, &mut PanicExecutor)));
  assert!(unwind.is_err());

  let DurabilityWaiterState::Failed(failure) = coordinator.waiter_state(first).unwrap() else {
    panic!("unwound executor did not fail its waiter")
  };
  assert_eq!(failure.operation, DurabilityOperation::AuthorityWrite);
  assert!(matches!(
    coordinator.waiter_state(later).unwrap(),
    DurabilityWaiterState::Failed(later_failure) if later_failure.operation == DurabilityOperation::AuthorityWrite
  ));
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert_eq!(coordinator.snapshot().unwrap().pending_hard, 0);
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
  let mut later_executor = RecordingExecutor::default();
  assert!(coordinator.execute(later, &mut later_executor).is_err());

  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(matches!(coordinator.waiter_state(failed).unwrap(), DurabilityWaiterState::Failed(_)));
  assert!(matches!(coordinator.waiter_state(later).unwrap(), DurabilityWaiterState::Failed(_)));
  assert!(later_executor.operations.is_empty());
}

#[test]
fn large_reversed_group_is_canonicalized_without_skipping_the_frontier() {
  let coordinator = DurabilityCoordinator::new();
  let tickets: Vec<_> = (0..32).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let reversed: Vec<_> = tickets.iter().rev().copied().collect();
  let expected_sequences: Vec<_> = tickets.iter().map(|ticket| ticket.sequence()).collect();
  let mut executor = RecordingGroupExecutor::default();

  coordinator.execute_group(&reversed, &mut executor).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, tickets.last().unwrap().sequence());
  assert!(executor.calls.iter().all(|(sequences, _)| sequences == &expected_sequences));
}

#[derive(Default)]
struct RecordingGroupExecutor {
  calls: Vec<(Vec<u64>, DurabilityOperation)>,
  fail_at: Option<DurabilityOperation>,
  failures_before_success: usize,
  disposition: Option<DurabilityFailureDisposition>,
}

impl DurabilityGroupExecutor for RecordingGroupExecutor {
  type Error = &'static str;

  fn execute_group(&mut self, sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error> {
    self.calls.push((sequences.to_vec(), operation));
    if self.fail_at == Some(operation) && self.failures_before_success > 0 {
      self.failures_before_success -= 1;
      return Err("injected grouped durability failure");
    }
    Ok(())
  }

  fn classify_error(&self, _operation: DurabilityOperation, _error: &Self::Error, _mutation_started: bool) -> DurabilityFailureDisposition {
    self.disposition.unwrap_or(DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::Never))
  }
}

#[test]
fn compatible_hard_tickets_execute_as_one_ordered_group() {
  let coordinator = DurabilityCoordinator::new();
  let tickets: Vec<_> = (0..3).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let mut executor = RecordingGroupExecutor::default();

  coordinator.execute_group(&tickets, &mut executor).unwrap();
  assert_eq!(executor.calls.len(), header_commit_plan().operations().len());
  for (sequences, _) in &executor.calls {
    assert_eq!(sequences, &tickets.iter().map(|ticket| ticket.sequence()).collect::<Vec<_>>());
  }
  assert_eq!(executor.calls.iter().filter(|(_, operation)| *operation == DurabilityOperation::DataBarrier).count(), 1);
  assert_eq!(executor.calls.iter().filter(|(_, operation)| *operation == DurabilityOperation::AuthorityBarrier).count(), 1);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, tickets.last().unwrap().sequence());
  assert!(tickets.iter().all(|ticket| matches!(coordinator.waiter_state(*ticket).unwrap(), DurabilityWaiterState::Succeeded(_))));
}

#[derive(Clone)]
struct SharedBarrierExecutor {
  data_barriers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
  authority_barriers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
  fail_authority: bool,
}

impl DurabilityGroupExecutor for SharedBarrierExecutor {
  type Error = &'static str;

  fn execute_group(&mut self, _sequences: &[u64], operation: DurabilityOperation) -> Result<(), Self::Error> {
    use std::sync::atomic::Ordering;
    match operation {
      DurabilityOperation::DataBarrier => {
        self.data_barriers.fetch_add(1, Ordering::SeqCst);
      }
      DurabilityOperation::AuthorityBarrier => {
        self.authority_barriers.fetch_add(1, Ordering::SeqCst);
        if self.fail_authority {
          return Err("injected live authority barrier failure");
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn classify_error(&self, _operation: DurabilityOperation, _error: &Self::Error, _mutation_started: bool) -> DurabilityFailureDisposition {
    DurabilityFailureDisposition::serious(OsErrorClass::MediaIo, RetryClass::AfterRepair)
  }
}

fn drive_live_waiter(
  coordinator: std::sync::Arc<DurabilityCoordinator>,
  ticket: aeordb::engine::durability_coordinator::DurabilityTicket,
  mut executor: SharedBarrierExecutor,
) -> DurabilityWaiterState {
  loop {
    match coordinator.wait_for_hard_turn(ticket).unwrap() {
      DurabilityHardTurn::Complete(state) => return coordinator.take_waiter_state(ticket).unwrap_or(state),
      DurabilityHardTurn::Drive(permit) => {
        let group = coordinator.select_ready_hard_group(true).unwrap();
        if !group.is_empty() {
          let _ = coordinator.execute_group(&group, &mut executor);
        }
        permit.release();
      }
    }
  }
}

#[test]
fn live_hard_waiters_elect_one_driver_and_share_physical_barriers() {
  use std::sync::atomic::{AtomicUsize, Ordering};

  let policy = DurabilityGroupPolicy::new(DEFAULT_GROUP_COMMIT_MAX_BYTES, std::time::Duration::from_millis(20)).unwrap();
  let coordinator = std::sync::Arc::new(DurabilityCoordinator::with_policy(policy));
  let tickets: Vec<_> = (0..2).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let data_barriers = std::sync::Arc::new(AtomicUsize::new(0));
  let authority_barriers = std::sync::Arc::new(AtomicUsize::new(0));
  let executor =
    SharedBarrierExecutor { data_barriers: data_barriers.clone(), authority_barriers: authority_barriers.clone(), fail_authority: false };

  let handles: Vec<_> = tickets
    .iter()
    .copied()
    .map(|ticket| {
      let coordinator = coordinator.clone();
      let executor = executor.clone();
      std::thread::spawn(move || drive_live_waiter(coordinator, ticket, executor))
    })
    .collect();
  let states: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();

  assert!(states.iter().all(|state| matches!(state, DurabilityWaiterState::Succeeded(_))));
  assert_eq!(data_barriers.load(Ordering::SeqCst), 1);
  assert_eq!(authority_barriers.load(Ordering::SeqCst), 1);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, tickets[1].sequence());
}

#[test]
fn live_group_failure_reaches_every_waiter() {
  use std::sync::atomic::{AtomicUsize, Ordering};

  let policy = DurabilityGroupPolicy::new(DEFAULT_GROUP_COMMIT_MAX_BYTES, std::time::Duration::ZERO).unwrap();
  let coordinator = std::sync::Arc::new(DurabilityCoordinator::with_policy(policy));
  let tickets: Vec<_> = (0..2).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let data_barriers = std::sync::Arc::new(AtomicUsize::new(0));
  let authority_barriers = std::sync::Arc::new(AtomicUsize::new(0));
  let executor =
    SharedBarrierExecutor { data_barriers: data_barriers.clone(), authority_barriers: authority_barriers.clone(), fail_authority: true };

  let handles: Vec<_> = tickets
    .iter()
    .copied()
    .map(|ticket| {
      let coordinator = coordinator.clone();
      let executor = executor.clone();
      std::thread::spawn(move || drive_live_waiter(coordinator, ticket, executor))
    })
    .collect();
  let states: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();

  assert!(states
    .iter()
    .all(|state| matches!(state, DurabilityWaiterState::Failed(failure) if failure.operation == DurabilityOperation::AuthorityBarrier)));
  assert_eq!(data_barriers.load(Ordering::SeqCst), 1);
  assert_eq!(authority_barriers.load(Ordering::SeqCst), 1);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
}

#[test]
fn bounded_group_failure_halts_and_retires_every_pending_hard_waiter() {
  let policy = DurabilityGroupPolicy::new(1024 * 1024, std::time::Duration::ZERO).unwrap();
  let coordinator = DurabilityCoordinator::with_policy(policy);
  let tickets: Vec<_> = (0..3).map(|_| coordinator.admit_sized(header_commit_plan(), 700 * 1024).unwrap()).collect();
  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![tickets[0]]);

  let mut executor = RecordingGroupExecutor {
    calls: Vec::new(),
    fail_at: Some(DurabilityOperation::AuthorityBarrier),
    failures_before_success: usize::MAX,
    disposition: Some(DurabilityFailureDisposition::serious(OsErrorClass::MediaIo, RetryClass::AfterRepair)),
  };
  assert!(coordinator.execute_group(&[tickets[0]], &mut executor).is_err());

  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.hard_frontier, 0);
  assert_eq!(snapshot.pending_hard, 0);
  assert_eq!(snapshot.failed, tickets.len());
  for ticket in tickets {
    let DurabilityWaiterState::Failed(failure) = coordinator.take_waiter_state(ticket).unwrap() else {
      panic!("hard-frontier failure left a waiter pending")
    };
    assert_eq!(failure.operation, DurabilityOperation::AuthorityBarrier);
    assert_eq!(failure.os_error_class, Some(OsErrorClass::MediaIo));
  }
  assert_eq!(coordinator.snapshot().unwrap().failed, 0);

  let error = coordinator.admit(header_commit_plan()).unwrap_err();
  assert_eq!(error.operation(), Some(DurabilityOperation::AuthorityBarrier));
}

#[test]
fn driver_setup_failure_can_halt_all_pending_hard_waiters_before_execution() {
  let coordinator = DurabilityCoordinator::new();
  let tickets: Vec<_> = (0..2).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();

  let failures = coordinator
    .fail_pending_hard(
      DurabilityOperation::DependencyAppend,
      "driver could not acquire storage authority",
      DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::AfterRepair),
      1,
    )
    .unwrap();
  assert_eq!(failures.len(), tickets.len());
  assert_eq!(coordinator.snapshot().unwrap().pending_hard, 0);
  for ticket in tickets {
    assert!(matches!(
      coordinator.take_waiter_state(ticket).unwrap(),
      DurabilityWaiterState::Failed(failure)
        if failure.operation == DurabilityOperation::DependencyAppend
          && failure.message == "driver could not acquire storage authority"
    ));
  }
}

#[test]
fn live_driver_rejects_soft_tickets_and_propagates_an_earlier_hard_failure() {
  let coordinator = DurabilityCoordinator::new();
  let failed = coordinator.admit(header_commit_plan()).unwrap();
  let blocked = coordinator.admit(header_commit_plan()).unwrap();
  let soft = coordinator
    .admit(
      DurabilityCommitPlan::new(
        CommitClass::RecoverableSoftState,
        vec![DurabilityOperation::DependencyAppend, DurabilityOperation::DataBarrier],
      )
      .unwrap(),
    )
    .unwrap();

  let mut executor = RecordingExecutor { operations: Vec::new(), fail_at: Some(DurabilityOperation::AuthorityBarrier) };
  assert!(coordinator.execute(failed, &mut executor).is_err());
  let DurabilityHardTurn::Complete(DurabilityWaiterState::Failed(blocked_failure)) = coordinator.wait_for_hard_turn(blocked).unwrap()
  else {
    panic!("a later hard waiter ignored the failed frontier")
  };
  assert_eq!(blocked_failure.operation, DurabilityOperation::AuthorityBarrier);
  assert!(coordinator.wait_for_hard_turn(soft).is_err());
}

#[test]
fn non_contiguous_hard_group_refuses_before_executor_mutation() {
  let coordinator = DurabilityCoordinator::new();
  let tickets: Vec<_> = (0..3).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let mut executor = RecordingGroupExecutor::default();

  assert!(coordinator.execute_group(&[tickets[0], tickets[2]], &mut executor).is_err());
  assert!(executor.calls.is_empty());
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(tickets.iter().all(|ticket| matches!(coordinator.waiter_state(*ticket).unwrap(), DurabilityWaiterState::Pending)));
}

#[test]
fn mixed_or_duplicate_groups_refuse_before_executor_mutation() {
  let coordinator = DurabilityCoordinator::new();
  let hard = coordinator.admit(header_commit_plan()).unwrap();
  let soft = coordinator
    .admit(
      DurabilityCommitPlan::new(
        CommitClass::RecoverableSoftState,
        vec![DurabilityOperation::DependencyAppend, DurabilityOperation::DataBarrier],
      )
      .unwrap(),
    )
    .unwrap();
  let mut executor = RecordingGroupExecutor::default();

  assert!(coordinator.execute_group(&[hard, soft], &mut executor).is_err());
  assert!(coordinator.execute_group(&[hard, hard], &mut executor).is_err());
  assert!(executor.calls.is_empty());
  assert!(matches!(coordinator.waiter_state(hard).unwrap(), DurabilityWaiterState::Pending));
  assert!(matches!(coordinator.waiter_state(soft).unwrap(), DurabilityWaiterState::Pending));
}

#[test]
fn grouped_failure_fails_every_waiter_without_crossing_the_frontier() {
  let coordinator = DurabilityCoordinator::new();
  let tickets: Vec<_> = (0..4).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let mut executor = RecordingGroupExecutor {
    calls: Vec::new(),
    fail_at: Some(DurabilityOperation::AuthorityBarrier),
    failures_before_success: 1,
    disposition: Some(DurabilityFailureDisposition::serious(OsErrorClass::MediaIo, RetryClass::Never)),
  };

  assert!(coordinator.execute_group(&tickets, &mut executor).is_err());
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  for ticket in tickets {
    let DurabilityWaiterState::Failed(failure) = coordinator.waiter_state(ticket).unwrap() else {
      panic!("group member was not failed")
    };
    assert_eq!(failure.operation, DurabilityOperation::AuthorityBarrier);
    assert_eq!(failure.os_error_class, Some(OsErrorClass::MediaIo));
    assert!(failure.serious);
  }
  assert!(!executor.calls.iter().any(|(_, operation)| *operation == DurabilityOperation::AuthorityReadback));
}

#[test]
fn retryable_group_failure_retries_with_a_strict_attempt_bound() {
  let coordinator = DurabilityCoordinator::new();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  let mut executor = RecordingGroupExecutor {
    calls: Vec::new(),
    fail_at: Some(DurabilityOperation::DataBarrier),
    failures_before_success: 2,
    disposition: Some(DurabilityFailureDisposition::transient(OsErrorClass::InterruptedNoProgress, RetryClass::Immediate)),
  };

  coordinator.execute_group(&[ticket], &mut executor).unwrap();
  assert_eq!(executor.calls.iter().filter(|(_, operation)| *operation == DurabilityOperation::DataBarrier).count(), 3);
  assert!(matches!(coordinator.waiter_state(ticket).unwrap(), DurabilityWaiterState::Succeeded(_)));
}

#[test]
fn uncertain_completion_is_never_blindly_replayed() {
  let coordinator = DurabilityCoordinator::new();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  let mut executor = RecordingGroupExecutor {
    calls: Vec::new(),
    fail_at: Some(DurabilityOperation::AuthorityBarrier),
    failures_before_success: usize::MAX,
    disposition: Some(DurabilityFailureDisposition::uncertain(OsErrorClass::TimeoutUnknown)),
  };

  assert!(coordinator.execute_group(&[ticket], &mut executor).is_err());
  assert_eq!(executor.calls.iter().filter(|(_, operation)| *operation == DurabilityOperation::AuthorityBarrier).count(), 1);
}

#[test]
fn io_error_matrix_matches_the_frozen_retry_and_latch_policy() {
  let cases = [
    (std::io::Error::from(std::io::ErrorKind::Interrupted), false, OsErrorClass::InterruptedNoProgress, RetryClass::Immediate, false),
    (std::io::Error::from(std::io::ErrorKind::StorageFull), false, OsErrorClass::NoSpace, RetryClass::AfterRepair, true),
    (std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem), false, OsErrorClass::ReadOnly, RetryClass::AfterRepair, true),
    (std::io::Error::from(std::io::ErrorKind::PermissionDenied), false, OsErrorClass::Permission, RetryClass::AfterRepair, true),
    (std::io::Error::from(std::io::ErrorKind::WriteZero), true, OsErrorClass::ShortWrite, RetryClass::Never, true),
    (std::io::Error::from(std::io::ErrorKind::TimedOut), true, OsErrorClass::TimeoutUnknown, RetryClass::AfterRepair, true),
    (std::io::Error::from(std::io::ErrorKind::WouldBlock), false, OsErrorClass::OtherPersistentIo, RetryClass::BoundedBackoff, false),
  ];

  for (error, mutation_started, expected_class, expected_retry, expected_serious) in cases {
    let disposition = classify_io_error(&error, mutation_started);
    assert_eq!(disposition.os_error_class, Some(expected_class));
    assert_eq!(disposition.retry_class, expected_retry);
    assert_eq!(disposition.serious, expected_serious);
  }

  #[cfg(unix)]
  {
    let quota = classify_io_error(&std::io::Error::from_raw_os_error(libc::EDQUOT), false);
    assert_eq!(quota.os_error_class, Some(OsErrorClass::Quota));
    assert_eq!(quota.retry_class, RetryClass::AfterRepair);
    assert!(quota.serious);
  }
}

#[test]
fn native_platform_errors_preserve_io_evidence_and_do_not_latch_invalid_requests() {
  let temp = tempfile::tempdir().unwrap();
  let missing = platform_file_identity(temp.path().join("missing")).unwrap_err();
  let missing_disposition = classify_native_durability_error(&missing, false);
  assert_eq!(missing.io_error_kind(), Some(std::io::ErrorKind::NotFound));
  assert_eq!(missing_disposition.os_error_class, Some(OsErrorClass::OtherPersistentIo));
  assert!(missing_disposition.serious);

  let file = std::fs::File::create(temp.path().join("data")).unwrap();
  let invalid = preallocate_file(&file, 0).unwrap_err();
  let invalid_disposition = classify_native_durability_error(&invalid, false);
  assert_eq!(invalid_disposition.os_error_class, None);
  assert_eq!(invalid_disposition.retry_class, RetryClass::Never);
  assert!(!invalid_disposition.serious);
  assert!(!invalid_disposition.uncertain_completion);
}

#[test]
fn group_policy_enforces_frozen_bounds_and_selects_only_compatible_prefixes() {
  assert_eq!(DEFAULT_GROUP_COMMIT_MAX_BYTES, 64 * 1024 * 1024);
  assert_eq!(DEFAULT_GROUP_COMMIT_MAX_DELAY, std::time::Duration::from_millis(100));
  assert!(DurabilityGroupPolicy::new(1024 * 1024 - 1, std::time::Duration::ZERO).is_err());
  assert!(DurabilityGroupPolicy::new(1024 * 1024, std::time::Duration::from_millis(1001)).is_err());

  let policy = DurabilityGroupPolicy::new(1024 * 1024, std::time::Duration::ZERO).unwrap();
  let coordinator = DurabilityCoordinator::with_policy(policy);
  let first = coordinator.admit_sized(header_commit_plan(), 512 * 1024).unwrap();
  let second = coordinator.admit_sized(header_commit_plan(), 512 * 1024).unwrap();
  let third = coordinator.admit_sized(header_commit_plan(), 1).unwrap();

  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![first, second]);
  let mut executor = RecordingGroupExecutor::default();
  coordinator.execute_group(&[first, second], &mut executor).unwrap();
  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![third]);
}

#[test]
fn oversized_hard_ticket_is_a_singleton_instead_of_an_accidental_write_limit() {
  let policy = DurabilityGroupPolicy::new(1024 * 1024, std::time::Duration::ZERO).unwrap();
  let coordinator = DurabilityCoordinator::with_policy(policy);
  let oversized = coordinator.admit_sized(header_commit_plan(), 4 * 1024 * 1024).unwrap();
  let next = coordinator.admit_sized(header_commit_plan(), 1).unwrap();

  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![oversized]);
  coordinator.execute(oversized, &mut RecordingExecutor::default()).unwrap();
  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![next]);
}

#[test]
fn disk_kv_finalization_cannot_publish_database_header_authority() {
  let source = include_str!("../../src/engine/disk_kv_store.rs");
  assert!(!source.contains("write_header_to_inactive_slot"));
  assert!(!source.contains("read_active_header"));
}

#[test]
fn recoverable_file_barriers_use_the_ledger_without_moving_hard_authority() {
  use std::io::Write;

  let temp = tempfile::tempdir().unwrap();
  let path = temp.path().join("soft-barrier.bin");
  let mut file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  file.write_all(b"recoverable soft state").unwrap();
  let coordinator = DurabilityCoordinator::new();

  coordinator.execute_recoverable_file_barrier(&file, NativeFileBarrierKind::Data, 22).unwrap();
  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.hard_frontier, 0);
  assert_eq!(snapshot.proven, 0);
  assert_eq!(snapshot.ledger.len(), 2);
  assert_eq!(snapshot.ledger[0].operation, DurabilityOperation::DependencyAppend);
  assert_eq!(snapshot.ledger[1].operation, DurabilityOperation::DataBarrier);
  assert!(snapshot.ledger.iter().all(|entry| entry.succeeded));
  assert_eq!(std::fs::read(path).unwrap(), b"recoverable soft state");
}

#[test]
fn storage_engine_uses_one_sequence_space_for_kv_dependencies_and_header_authority() {
  let temp = tempfile::tempdir().unwrap();
  let path = temp.path().join("one-durability-sequence.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();

  let snapshot = engine.durability_snapshot().unwrap();
  assert!(snapshot.hard_frontier >= 3, "KV and header work did not share one sequence space: {snapshot:?}");
  assert!(snapshot.ledger.iter().any(|entry| entry.operation == DurabilityOperation::DataBarrier));
  assert_eq!(snapshot.ledger.last().unwrap().operation, DurabilityOperation::AuthorityReadback);
}

#[test]
fn acknowledged_transaction_is_one_hard_commit_instead_of_separate_soft_barriers() {
  let temp = tempfile::tempdir().unwrap();
  let path = temp.path().join("one-transaction-commit.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let before = engine.durability_snapshot().unwrap();

  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/one.txt", b"one transaction", Some("text/plain")).unwrap();

  let after = engine.durability_snapshot().unwrap();
  assert_eq!(after.next_sequence, before.next_sequence + 1, "one acknowledged transaction must consume exactly one durability ticket");
  let delta = &after.ledger[before.ledger.len()..];
  assert_eq!(
    delta.iter().map(|entry| entry.operation).collect::<Vec<_>>(),
    header_commit_plan().operations(),
    "the transaction must execute dependency durability and header authority as one hard plan"
  );
  assert!(delta.iter().all(|entry| entry.succeeded));
}

#[test]
fn timer_flush_is_one_hard_commit_and_not_two_soft_barriers_plus_authority() {
  let temp = tempfile::tempdir().unwrap();
  let path = temp.path().join("one-timer-commit.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let value = b"timer dependency";
  let key = engine.compute_hash(value).unwrap();
  engine.store_entry_typed(EntryType::Chunk, &key, value, KV_TYPE_CHUNK).unwrap();
  let before = engine.durability_snapshot().unwrap();

  engine.try_flush_hot_buffer();

  let after = engine.durability_snapshot().unwrap();
  assert_eq!(after.next_sequence, before.next_sequence + 1, "one timer flush must consume exactly one durability ticket");
  let delta = &after.ledger[before.ledger.len()..];
  assert_eq!(delta.iter().map(|entry| entry.operation).collect::<Vec<_>>(), header_commit_plan().operations());
  assert!(delta.iter().all(|entry| entry.succeeded));
}

#[test]
fn production_code_cannot_call_raw_file_barriers_outside_the_native_adapter() {
  fn visit(directory: &std::path::Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        visit(&path, violations);
      } else if path.extension().and_then(|value| value.to_str()) == Some("rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("native_durability.rs")
      {
        for (line_number, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
          let trimmed = line.trim_start();
          if !trimmed.starts_with("//") && (line.contains(".sync_data()") || line.contains(".sync_all()")) {
            violations.push(format!("{}:{}:{trimmed}", path.display(), line_number + 1));
          }
        }
      }
    }
  }

  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let mut violations = Vec::new();
  visit(&package.join("src/engine"), &mut violations);
  visit(&package.join("../aeordb-cli/src"), &mut violations);
  assert!(violations.is_empty(), "raw file barriers bypass the native durability adapter:\n{}", violations.join("\n"));
}

#[test]
fn production_transaction_guards_require_explicit_fallible_completion() {
  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  fn visit(directory: &std::path::Path, sources: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        visit(&path, sources);
      } else if path.extension().and_then(|value| value.to_str()) == Some("rs")
        && path.file_name().and_then(|value| value.to_str()) != Some("storage_engine.rs")
      {
        sources.push(path);
      }
    }
  }

  let mut sources = Vec::new();
  visit(&package.join("src/engine"), &mut sources);
  let mut guard_count = 0usize;
  let mut completion_count = 0usize;
  let mut violations = Vec::new();

  for path in sources {
    let source = std::fs::read_to_string(&path).unwrap();
    guard_count += source.matches("TransactionGuard::new(").count();
    completion_count += source.matches("txn.commit_after(").count();
    completion_count += source.matches("txn.finish_after(").count();

    for (line_number, line) in source.lines().enumerate() {
      if line.contains("TransactionGuard::new(") && line.contains("let _") {
        violations.push(format!("{}:{}:{line}", path.display(), line_number + 1));
      }
      if line.contains("drop(txn)") {
        violations.push(format!("{}:{}:{line}", path.display(), line_number + 1));
      }
      if line.contains("txn.commit()") || line.contains("txn.finish(") {
        violations.push(format!("{}:{}:{line}", path.display(), line_number + 1));
      }
    }
  }

  assert!(violations.is_empty(), "transaction guards may not rely on infallible Drop completion:\n{}", violations.join("\n"));
  assert!(
    completion_count >= guard_count,
    "every production transaction guard needs an explicit namespace-releasing commit/finish: {guard_count} guards, {completion_count} completions"
  );
}

#[test]
fn batch_publication_and_metrics_follow_hard_commit_completion() {
  let source = include_str!("../../src/engine/batch_commit.rs");
  let finish_start = source.find("fn finish_batch_commit(").unwrap();
  let finish_end = source[finish_start..].find("\nfn update_directory(").unwrap() + finish_start;
  let finish_body = &source[finish_start..finish_end];
  assert!(!finish_body.contains("ctx.emit("), "batch helper published an event before its caller completed hard authority");
  assert!(!finish_body.contains("record_file_write("), "batch helper changed acknowledged-write counters before hard authority completed");

  for (start_marker, end_marker) in [
    ("pub fn commit_files(", "\n/// Atomically commit multiple small files"),
    ("pub fn commit_buffered_files(", "\nfn finish_batch_commit("),
  ] {
    let start = source.find(start_marker).unwrap();
    let end = source[start..].find(end_marker).unwrap() + start;
    let body = &source[start..end];
    let commit = body.find("commit_after(").expect("batch path must await hard completion");
    let publication = body.find("publish_batch_success(").expect("batch path must publish its event and counters after completion");
    assert!(commit < publication, "batch event/counter publication preceded hard completion");
  }
}

#[test]
fn persistent_enum_ids_match_the_generated_registry() {
  for (expected, value) in (1u16..=13).zip(OsErrorClass::ALL) {
    assert_eq!(value.stable_id(), expected);
  }
  for (expected, value) in (0u16..=5).zip(RetryClass::ALL) {
    assert_eq!(value.stable_id(), expected);
  }
}
