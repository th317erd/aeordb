use aeordb::engine::durability_coordinator::{
  CommitClass, DurabilityCommitPlan, DurabilityCoordinator, DurabilityExecutor, DurabilityFailureDisposition, DurabilityGroupExecutor,
  DEFAULT_GROUP_COMMIT_MAX_BYTES, DEFAULT_GROUP_COMMIT_MAX_DELAY, DurabilityGroupPolicy, DurabilityOperation, DurabilityWaiterState,
  OsErrorClass, RetryClass, classify_io_error, classify_native_durability_error,
};
use aeordb::engine::native_durability::{platform_file_identity, preallocate_file};

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
  let mut later_executor = RecordingExecutor::default();
  assert!(coordinator.execute(later, &mut later_executor).is_err());

  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 0);
  assert!(matches!(coordinator.waiter_state(failed).unwrap(), DurabilityWaiterState::Failed(_)));
  assert!(matches!(coordinator.waiter_state(later).unwrap(), DurabilityWaiterState::Pending));
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
fn persistent_enum_ids_match_the_generated_registry() {
  for (expected, value) in (1u16..=13).zip(OsErrorClass::ALL) {
    assert_eq!(value.stable_id(), expected);
  }
  for (expected, value) in (0u16..=5).zip(RetryClass::ALL) {
    assert_eq!(value.stable_id(), expected);
  }
}
