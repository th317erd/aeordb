use aeordb::engine::durability_coordinator::{
  CommitClass, DurabilityCommitPlan, DurabilityCoordinator, DurabilityExecutor, DurabilityFailureDisposition, DurabilityGroupExecutor,
  DEFAULT_GROUP_COMMIT_MAX_BYTES, DEFAULT_GROUP_COMMIT_MAX_DELAY, DurabilityGroupPolicy, DurabilityHardTurn, DurabilityOperation,
  DurabilityWaiterState, MAX_DURABILITY_WAITER_RECORDS, MAX_GROUP_COMMIT_RECORDS, NativeFileBarrierKind, OsErrorClass, RetryClass,
  classify_io_error, classify_native_durability_error,
};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
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

#[test]
fn durability_snapshot_retains_only_the_last_completed_barrier() {
  let coordinator = DurabilityCoordinator::new();
  assert!(coordinator.snapshot().unwrap().last_barrier.is_none());

  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  coordinator.execute(ticket, &mut RecordingExecutor::default()).unwrap();

  let completed = coordinator.snapshot().unwrap().last_barrier.expect("last barrier");
  assert_eq!(completed.operation, DurabilityOperation::AuthorityBarrier);
  assert_eq!(completed.first_sequence, ticket.sequence());
  assert_eq!(completed.last_sequence, ticket.sequence());
  assert_eq!(completed.waiter_count, 1);
  assert!(completed.succeeded);
  assert_eq!(completed.attempts, 1);
  assert!(completed.completed_at_ms > 0);
  assert!(completed.error.is_none());
}

#[test]
fn durability_snapshot_records_failed_and_unwound_barriers_with_bounded_evidence() {
  let coordinator = DurabilityCoordinator::new();
  let failed = coordinator.admit(header_commit_plan()).unwrap();
  let mut executor = RecordingExecutor { operations: Vec::new(), fail_at: Some(DurabilityOperation::AuthorityBarrier) };
  assert!(coordinator.execute(failed, &mut executor).is_err());

  let failure = coordinator.snapshot().unwrap().last_barrier.expect("failed barrier");
  assert_eq!(failure.operation, DurabilityOperation::AuthorityBarrier);
  assert!(!failure.succeeded);
  assert_eq!(failure.attempts, 1);
  assert!(failure.error.as_deref().is_some_and(|message| message.contains("injected durability failure")));
  assert!(failure.error.as_ref().unwrap().len() <= 4 * 1024);

  struct PanicAtBarrier;
  impl DurabilityExecutor for PanicAtBarrier {
    type Error = &'static str;

    fn execute(&mut self, _sequence: u64, operation: DurabilityOperation) -> Result<(), Self::Error> {
      if operation == DurabilityOperation::AuthorityBarrier {
        panic!("injected barrier unwind");
      }
      Ok(())
    }
  }

  let coordinator = DurabilityCoordinator::new();
  let unwound = coordinator.admit(header_commit_plan()).unwrap();
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| coordinator.execute(unwound, &mut PanicAtBarrier)));
  assert!(result.is_err());
  let unwind = coordinator.snapshot().unwrap().last_barrier.expect("unwound barrier");
  assert_eq!(unwind.operation, DurabilityOperation::AuthorityBarrier);
  assert!(!unwind.succeeded);
  assert!(unwind.error.as_deref().is_some_and(|message| message.contains("unwound")));
}

fn durability_reserved_bytes(coordinator: &MemoryCoordinator) -> u64 {
  coordinator.snapshot().unwrap().owner(MemoryOwner::DurabilityWaiters).unwrap().reserved_bytes
}

fn memory_bounded_durability_coordinator(emergency_reserve_bytes: u64) -> (std::sync::Arc<MemoryCoordinator>, DurabilityCoordinator) {
  let hard_limit_bytes = emergency_reserve_bytes + 16 * 1024;
  let memory =
    std::sync::Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1024, hard_limit_bytes, 1, emergency_reserve_bytes).unwrap()));
  let durability = DurabilityCoordinator::with_policy_and_memory_coordinator(
    DurabilityGroupPolicy::new(1024 * 1024, std::time::Duration::ZERO).unwrap(),
    4,
    std::sync::Arc::clone(&memory),
  )
  .unwrap();
  (memory, durability)
}

#[test]
fn durability_waiter_admission_is_bounded_before_sequence_or_queue_mutation() {
  let (memory, coordinator) = memory_bounded_durability_coordinator(48 * 1024);
  let baseline = durability_reserved_bytes(&memory);
  let mut tickets = Vec::new();
  loop {
    match coordinator.admit_sized(header_commit_plan(), 0) {
      Ok(ticket) => tickets.push(ticket),
      Err(aeordb::engine::durability_coordinator::DurabilityCoordinatorError::ResourceExhausted(_)) => break,
      Err(error) => panic!("unexpected durability admission error: {error}"),
    }
  }

  assert!(!tickets.is_empty());
  let after_refusal = coordinator.snapshot().unwrap();
  assert_eq!(after_refusal.admitted, tickets.len());
  assert_eq!(after_refusal.pending_hard, tickets.len());
  assert_eq!(after_refusal.next_sequence, tickets.last().unwrap().sequence() + 1);
  assert!(durability_reserved_bytes(&memory) > baseline);

  coordinator
    .fail_pending_hard(
      DurabilityOperation::DependencyAppend,
      "retire pressure-test waiters",
      DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::AfterRepair),
      1,
    )
    .unwrap();
  for ticket in tickets {
    assert!(matches!(coordinator.take_waiter_state(ticket).unwrap(), DurabilityWaiterState::Failed(_)));
  }
  assert_eq!(durability_reserved_bytes(&memory), baseline);
}

#[test]
fn failed_hard_ticket_is_failed_and_retired_by_one_coordinator_transition() {
  let (memory, coordinator) = memory_bounded_durability_coordinator(256 * 1024);
  let baseline = durability_reserved_bytes(&memory);
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  assert!(durability_reserved_bytes(&memory) > baseline);

  let waiter = coordinator
    .fail_pending_hard_and_take(
      ticket,
      DurabilityOperation::DependencyAppend,
      "injected grouped transaction failure",
      DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::AfterRepair),
      1,
    )
    .unwrap();

  let DurabilityWaiterState::Failed(failure) = waiter else {
    panic!("failed ticket did not return terminal failure evidence");
  };
  assert_eq!(failure.sequence, ticket.sequence());
  assert!(failure.message.contains("injected grouped transaction failure"));
  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.pending_hard, 0);
  assert_eq!(snapshot.failed, 0, "the exact failed ticket must be retired before returning");
  assert_eq!(durability_reserved_bytes(&memory), baseline);
  assert!(matches!(
    coordinator.fail_pending_hard_and_take(
      ticket,
      DurabilityOperation::DependencyAppend,
      "duplicate cleanup",
      DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::AfterRepair),
      1,
    ),
    Err(aeordb::engine::durability_coordinator::DurabilityCoordinatorError::UnknownTicket)
  ));
}

#[test]
fn durability_waiter_reservation_survives_success_failure_and_unwind_until_retirement() {
  let (memory, coordinator) = memory_bounded_durability_coordinator(256 * 1024);
  let baseline = durability_reserved_bytes(&memory);

  let succeeded = coordinator.admit(header_commit_plan()).unwrap();
  let after_success_admission = durability_reserved_bytes(&memory);
  assert!(after_success_admission > baseline);
  coordinator.execute(succeeded, &mut RecordingExecutor::default()).unwrap();
  assert_eq!(durability_reserved_bytes(&memory), after_success_admission);
  assert!(matches!(coordinator.take_waiter_state(succeeded).unwrap(), DurabilityWaiterState::Succeeded(_)));
  assert_eq!(durability_reserved_bytes(&memory), baseline);

  let (memory, coordinator) = memory_bounded_durability_coordinator(256 * 1024);
  let baseline = durability_reserved_bytes(&memory);
  let failed = coordinator.admit(header_commit_plan()).unwrap();
  let after_failure_admission = durability_reserved_bytes(&memory);
  let mut failing = RecordingExecutor { operations: Vec::new(), fail_at: Some(DurabilityOperation::AuthorityBarrier) };
  assert!(coordinator.execute(failed, &mut failing).is_err());
  assert_eq!(durability_reserved_bytes(&memory), after_failure_admission);
  assert!(matches!(coordinator.take_waiter_state(failed).unwrap(), DurabilityWaiterState::Failed(_)));
  assert_eq!(durability_reserved_bytes(&memory), baseline);

  let (memory, coordinator) = memory_bounded_durability_coordinator(256 * 1024);
  let baseline = durability_reserved_bytes(&memory);
  let unwound = coordinator.admit(header_commit_plan()).unwrap();
  let after_unwind_admission = durability_reserved_bytes(&memory);
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| coordinator.execute(unwound, &mut PanicExecutor)));
  assert!(result.is_err());
  assert_eq!(durability_reserved_bytes(&memory), after_unwind_admission);
  assert!(matches!(coordinator.take_waiter_state(unwound).unwrap(), DurabilityWaiterState::Failed(_)));
  assert_eq!(durability_reserved_bytes(&memory), baseline);
}

#[test]
fn zero_byte_estimates_cannot_create_an_unbounded_execution_group() {
  let coordinator = DurabilityCoordinator::new();
  let tickets: Vec<_> = (0..=MAX_GROUP_COMMIT_RECORDS).map(|_| coordinator.admit_sized(header_commit_plan(), 0).unwrap()).collect();
  let selected = coordinator.select_ready_hard_group(true).unwrap();

  assert_eq!(selected.len(), MAX_GROUP_COMMIT_RECORDS);
  assert_eq!(selected.first(), tickets.first());
  assert_eq!(selected.last(), tickets.get(MAX_GROUP_COMMIT_RECORDS - 1));

  let mut executor = RecordingGroupExecutor::default();
  assert!(coordinator.execute_group(&tickets, &mut executor).is_err());
  assert!(executor.calls.is_empty());
}

#[test]
fn unconfigured_coordinator_retains_legacy_admission_behind_a_structural_ceiling() {
  let coordinator = DurabilityCoordinator::new();
  let disposable_plan = || DurabilityCommitPlan::new(CommitClass::Disposable, Vec::new()).unwrap();
  for _ in 0..MAX_DURABILITY_WAITER_RECORDS {
    coordinator.admit(disposable_plan()).unwrap();
  }
  let before_refusal = coordinator.snapshot().unwrap();

  let error = coordinator.admit(disposable_plan()).unwrap_err();

  assert!(matches!(error, aeordb::engine::durability_coordinator::DurabilityCoordinatorError::ResourceExhausted(_)));
  let after_refusal = coordinator.snapshot().unwrap();
  assert_eq!(after_refusal.next_sequence, before_refusal.next_sequence);
  assert_eq!(after_refusal.admitted, MAX_DURABILITY_WAITER_RECORDS);
}

#[test]
fn concurrent_durability_admission_never_crosses_the_emergency_reserve() {
  let (memory, coordinator) = memory_bounded_durability_coordinator(96 * 1024);
  let baseline = durability_reserved_bytes(&memory);
  let coordinator = std::sync::Arc::new(coordinator);
  let workers: Vec<_> = (0..32)
    .map(|_| {
      let coordinator = std::sync::Arc::clone(&coordinator);
      std::thread::spawn(move || coordinator.admit_sized(header_commit_plan(), 0))
    })
    .collect();
  let mut tickets = Vec::new();
  let mut refusals = 0usize;
  for worker in workers {
    match worker.join().unwrap() {
      Ok(ticket) => tickets.push(ticket),
      Err(aeordb::engine::durability_coordinator::DurabilityCoordinatorError::ResourceExhausted(_)) => refusals += 1,
      Err(error) => panic!("unexpected concurrent admission error: {error}"),
    }
  }

  assert!(!tickets.is_empty());
  assert!(refusals > 0);
  let memory_snapshot = memory.snapshot().unwrap();
  assert!(memory_snapshot.critical_reserved_bytes <= memory_snapshot.policy.unwrap().emergency_reserve_bytes);

  coordinator
    .fail_pending_hard(
      DurabilityOperation::DependencyAppend,
      "retire concurrent pressure-test waiters",
      DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::AfterRepair),
      1,
    )
    .unwrap();
  for ticket in tickets {
    assert!(matches!(coordinator.take_waiter_state(ticket).unwrap(), DurabilityWaiterState::Failed(_)));
  }
  assert_eq!(durability_reserved_bytes(&memory), baseline);
}

#[test]
fn durability_failure_evidence_is_bounded_by_the_admitted_waiter_charge() {
  let (_memory, coordinator) = memory_bounded_durability_coordinator(96 * 1024);
  let tickets: Vec<_> = (0..2).map(|_| coordinator.admit(header_commit_plan()).unwrap()).collect();
  let oversized = "failure-evidence-".repeat(1024);
  coordinator
    .fail_pending_hard(
      DurabilityOperation::AuthorityBarrier,
      oversized,
      DurabilityFailureDisposition::serious(OsErrorClass::MediaIo, RetryClass::AfterRepair),
      1,
    )
    .unwrap();

  for ticket in tickets {
    let DurabilityWaiterState::Failed(failure) = coordinator.take_waiter_state(ticket).unwrap() else {
      panic!("waiter did not retain bounded failure evidence")
    };
    assert!(failure.message.len() <= 4 * 1024);
    assert!(failure.message.is_char_boundary(failure.message.len()));
  }
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

  let hard_failure = coordinator.hard_failure().unwrap().expect("hard failure evidence");
  assert_eq!(hard_failure.operation, DurabilityOperation::AuthorityBarrier);
  assert_eq!(hard_failure.os_error_class, Some(OsErrorClass::MediaIo));
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
fn hard_frontier_failure_only_blocks_new_hard_authority_admission() {
  let coordinator = DurabilityCoordinator::new();
  let failed = coordinator.admit(header_commit_plan()).unwrap();
  let mut executor = RecordingExecutor { operations: Vec::new(), fail_at: Some(DurabilityOperation::AuthorityBarrier) };
  assert!(coordinator.execute(failed, &mut executor).is_err());

  let soft = DurabilityCommitPlan::new(
    CommitClass::RecoverableSoftState,
    vec![DurabilityOperation::DependencyAppend, DurabilityOperation::DataBarrier],
  )
  .unwrap();
  assert!(coordinator.admit(soft).is_ok());
  assert!(coordinator.admit(DurabilityCommitPlan::new(CommitClass::Disposable, Vec::new()).unwrap()).is_ok());
  assert!(coordinator.admit(header_commit_plan()).is_err());
}

#[test]
fn coordinator_idle_wait_distinguishes_nonterminal_work_from_retained_receipts() {
  let coordinator = DurabilityCoordinator::new();
  let ticket = coordinator.admit(header_commit_plan()).unwrap();

  let pending = coordinator.wait_until_idle(std::time::Duration::ZERO).unwrap();
  assert_eq!(pending.admitted, 1);
  assert_eq!(pending.pending_hard, 1);

  let mut executor = RecordingExecutor { operations: Vec::new(), fail_at: None };
  coordinator.execute(ticket, &mut executor).unwrap();
  let idle = coordinator.wait_until_idle(std::time::Duration::ZERO).unwrap();
  assert_eq!(idle.admitted, 0);
  assert_eq!(idle.executing, 0);
  assert_eq!(idle.pending_hard, 0);
  assert_eq!(idle.proven, 1, "terminal receipts do not block shutdown draining");
}

#[test]
fn coordinator_idle_wait_wakes_on_completion_and_reports_timeout_age() {
  let coordinator = std::sync::Arc::new(DurabilityCoordinator::new());
  let ticket = coordinator.admit(header_commit_plan()).unwrap();
  std::thread::sleep(std::time::Duration::from_millis(2));
  let timed_out = coordinator.wait_until_idle(std::time::Duration::ZERO).unwrap();
  assert_eq!(timed_out.admitted, 1);
  assert!(timed_out.oldest_pending_age_ms.is_some_and(|age| age >= 1));

  let worker_coordinator = std::sync::Arc::clone(&coordinator);
  let worker = std::thread::spawn(move || {
    std::thread::sleep(std::time::Duration::from_millis(10));
    let mut executor = RecordingExecutor { operations: Vec::new(), fail_at: None };
    worker_coordinator.execute(ticket, &mut executor).unwrap();
  });
  let idle = coordinator.wait_until_idle(std::time::Duration::from_secs(1)).unwrap();
  worker.join().unwrap();

  assert_eq!(idle.admitted, 0);
  assert_eq!(idle.executing, 0);
  assert_eq!(idle.pending_hard, 0);
  assert!(!idle.driver_active);
  assert_eq!(idle.oldest_pending_age_ms, None);
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
fn disabled_group_policy_preserves_durability_with_immediate_singleton_commits() {
  let coordinator = DurabilityCoordinator::new();
  coordinator.disable_grouping("durability.group_commit_max_bytes is unresolved").unwrap();
  let first = coordinator.admit_sized(header_commit_plan(), 1).unwrap();
  let second = coordinator.admit_sized(header_commit_plan(), 1).unwrap();

  let disabled = coordinator.group_policy_snapshot().unwrap();
  assert_eq!(disabled.policy, None);
  assert_eq!(disabled.disabled_reason.as_deref(), Some("durability.group_commit_max_bytes is unresolved"));
  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![first]);

  coordinator.execute(first, &mut RecordingExecutor::default()).unwrap();
  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![second]);

  let policy = DurabilityGroupPolicy::new(1024 * 1024, std::time::Duration::ZERO).unwrap();
  coordinator.reconfigure_group_policy(policy).unwrap();
  let third = coordinator.admit_sized(header_commit_plan(), 1).unwrap();
  let enabled = coordinator.group_policy_snapshot().unwrap();
  assert_eq!(enabled.policy, Some(policy));
  assert_eq!(enabled.disabled_reason, None);
  assert_eq!(coordinator.select_ready_hard_group(false).unwrap(), vec![second, third]);
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
fn batch_publication_has_one_namespace_authority_and_no_legacy_writer() {
  let source = include_str!("../../src/engine/batch_commit.rs");
  for forbidden in [
    "fn finish_batch_commit(",
    "fn update_directory(",
    "fn propagate_up(",
    "TransactionGuard",
    "publish_file_record_entries",
    "EntryType::FileRecord",
    "EntryType::DirectoryIndex",
    ".update_head(",
    "flush_batch_and_update_head(",
  ] {
    assert!(!source.contains(forbidden), "batch_commit.rs retained forbidden namespace-writer token: {forbidden}");
  }

  for (start_marker, end_marker) in [
    ("pub fn commit_files(", "\n/// Atomically commit multiple small files"),
    ("pub(crate) fn commit_buffered_files_with_kind(", "\nfn content_type_needs_sniffing("),
  ] {
    let start = source.find(start_marker).unwrap();
    let end = source[start..].find(end_marker).unwrap() + start;
    let body = &source[start..end];
    assert_eq!(
      body.matches("execute_file_publications(").count(),
      1,
      "each batch producer must delegate exactly once to the namespace coordinator"
    );
  }
  assert!(
    !source.contains("read_chunk_data(engine, first_hash)?.unwrap_or_default()"),
    "blob commit must not convert a vanished first chunk into an empty MIME sample"
  );
}

#[test]
fn wave_two_http_routes_delegate_one_logical_namespace_mutation() {
  let source = include_str!("../../src/server/engine_routes.rs");

  let copy_start = source.find("pub async fn copy_files(").unwrap();
  let copy_end = source[copy_start..].find("\n// POST /files/search").unwrap() + copy_start;
  let copy_body = &source[copy_start..copy_end];
  assert_eq!(copy_body.matches("ops.copy_paths(").count(), 1, "HTTP copy must publish the full request through one plural operation");
  assert!(!copy_body.contains("ops.copy_path("), "HTTP copy retained a per-source partial-publication loop");

  let merge_start = source.find("async fn do_merge_patch(").unwrap();
  let merge_end = source[merge_start..].find("\nasync fn do_rename(").unwrap() + merge_start;
  let merge_body = &source[merge_start..merge_end];
  assert_eq!(
    merge_body.matches("ops.merge_json_file_bounded(").count(),
    1,
    "HTTP merge-patch must delegate one coordinator-owned read-modify-write"
  );
  for forbidden in ["apply_merge_patch(", "ops.read_file_buffered(", "ops.store_file_buffered("] {
    assert!(!merge_body.contains(forbidden), "HTTP merge-patch retained handler-local mutation token: {forbidden}");
  }

  assert!(
    !source.contains("get_symlink(&path_for_blocking).ok().flatten()"),
    "HTTP path dispatch must propagate symlink read failures instead of silently reclassifying corrupt entries"
  );
}

#[test]
fn copy_file_returns_the_acknowledged_plan_without_a_post_commit_read() {
  let source = include_str!("../../src/engine/directory_ops.rs");
  let start = source.find("pub fn copy_file(").unwrap();
  let end = source[start..].find("\n  /// Recursively copy a path").unwrap() + start;
  let body = &source[start..end];
  assert_eq!(body.matches("execute_copy_mappings(").count(), 1);
  assert!(!body.contains("get_metadata("), "copy_file must not fail after hard acknowledgement because a follow-up metadata read failed");
}

#[test]
fn staged_file_publications_revalidate_chunk_authority_inside_the_namespace_plan() {
  let source = include_str!("../../src/engine/directory_ops.rs");
  for (start_marker, end_marker) in [
    ("fn execute_file_publication(", "\n  pub(crate) fn execute_file_publications("),
    ("pub(crate) fn execute_file_publications(", "\n  /// Store a file at the given path"),
    ("pub fn rename_file(", "\n  /// Copy a file to a new path"),
  ] {
    let start = source.find(start_marker).unwrap();
    let end = source[start..].find(end_marker).unwrap() + start;
    assert!(
      source[start..end].contains("validate_existing_file_chunks("),
      "{start_marker} must revalidate staged/referenced chunks while namespace authority is held"
    );
  }
}

#[test]
fn wave_two_chunk_staging_never_treats_an_arbitrary_kv_row_as_a_chunk() {
  let directory_ops = include_str!("../../src/engine/directory_ops.rs");
  for (start_marker, end_marker) in [
    ("pub fn store_chunk(", "\n  /// Finalize a file from pre-stored chunk hashes."),
    ("pub fn store_file_compressed(", "\n  /// Restore a file from an existing FileRecord"),
  ] {
    let start = directory_ops.find(start_marker).unwrap();
    let end = directory_ops[start..].find(end_marker).unwrap() + start;
    let body = &directory_ops[start..end];
    assert!(body.contains("validate_existing_chunk_locator("), "{start_marker} must use typed chunk-locator validation");
    assert!(!body.contains("has_entry(&chunk_key)"), "{start_marker} must not deduplicate against an untyped KV existence check");
  }

  let buffered_start = directory_ops.find("fn prepare_buffered_file_publication(").unwrap();
  let buffered_end = directory_ops[buffered_start..].find("\n}\n\n").unwrap() + buffered_start;
  let buffered_body = &directory_ops[buffered_start..buffered_end];
  assert!(buffered_body.contains("validate_existing_chunk_locator("));
  assert!(!buffered_body.contains("has_entry(&chunk_key)"));
  let merge_start = directory_ops.find("fn execute_json_merge_patches(").unwrap();
  let merge_end = directory_ops[merge_start..].find("\n  fn prepare_json_merge(").unwrap() + merge_start;
  assert!(directory_ops[merge_start..merge_end].contains("prepare_buffered_file_publication("));

  let batch_commit = include_str!("../../src/engine/batch_commit.rs");
  let batch_start = batch_commit.find("fn store_buffered_chunk(").unwrap();
  let batch_body = &batch_commit[batch_start..];
  assert!(batch_body.contains("validate_existing_chunk_locator("));
  assert!(!batch_body.contains("has_entry(&chunk_key)"));

  let upload_routes = include_str!("../../src/server/upload_routes.rs");
  let check_start = upload_routes.find("pub async fn upload_check(").unwrap();
  let check_end = upload_routes[check_start..].find("\npub struct CheckRequest").unwrap() + check_start;
  let check_body = &upload_routes[check_start..check_end];
  assert!(check_body.contains("validate_existing_chunk_locator("));
  assert!(!check_body.contains("has_entry(&hash_bytes)"));

  let upload_start = upload_routes.find("pub async fn upload_chunk(").unwrap();
  let upload_end = upload_routes[upload_start..].find("\npub struct CommitRequest").unwrap() + upload_start;
  let upload_body = &upload_routes[upload_start..upload_end];
  assert!(upload_body.contains("validate_existing_chunk_locator("));
  assert!(!upload_body.contains("has_entry(&computed_bytes)"));
}

#[test]
fn wave_three_sync_receive_has_one_typed_namespace_and_checkpoint_authority() {
  let sync_apply = include_str!("../../src/engine/sync_apply.rs");
  assert_eq!(sync_apply.matches(".apply_sync_merge(context, operations)").count(), 1);
  assert_eq!(sync_apply.matches(".apply_sync_receipt(context, operations, conflicts, &immutable_versions)").count(), 1);
  assert!(!sync_apply.contains("unrecorded_conflicts("));
  assert!(sync_apply.contains("remote_conflict_versions("));
  for forbidden in ["store_entry(", "store_file_buffered(", "delete_file(", "delete_symlink(", "TransactionGuard"] {
    assert!(!sync_apply.contains(forbidden), "sync_apply.rs retained forbidden independent writer token: {forbidden}");
  }

  let sync_engine = include_str!("../../src/engine/sync_engine.rs");
  assert_eq!(sync_engine.matches("apply_merge_operations_with_conflicts(").count(), 2);

  let directory_ops = include_str!("../../src/engine/directory_ops.rs");
  let receipt_start = directory_ops.find("  pub(crate) fn apply_sync_receipt(").unwrap();
  let receipt_end = directory_ops[receipt_start..].find("\n  /// Store a file at the given path").unwrap() + receipt_start;
  let receipt = &directory_ops[receipt_start..receipt_end];
  assert_eq!(receipt.matches("unrecorded_conflicts(planning_engine, conflicts)").count(), 1);
  assert!(receipt.contains("unrecorded_conflict_paths_by_hash"));
  assert!(!receipt.contains("unrecorded_conflict_paths ="));
  let transfer_start = sync_engine.find("  fn transfer_missing_remote_diff_chunks(").unwrap();
  let transfer_end = sync_engine[transfer_start..].find("\n  /// Load sync state for a peer").unwrap() + transfer_start;
  let transfer_body = &sync_engine[transfer_start..transfer_end];
  assert!(transfer_body.contains("apply_sync_chunks("));
  for forbidden in ["store_entry(", "has_entry("] {
    assert!(!transfer_body.contains(forbidden), "local sync chunk transfer retained untyped writer token: {forbidden}");
  }

  assert!(!sync_engine.contains("response.json()"), "remote sync responses must use the bounded typed decoder");
  let local_apply = sync_engine.find("apply_merge_operations_with_conflicts(").unwrap();
  let local_checkpoint = sync_engine[local_apply..].find("self.save_sync_state_hex(").unwrap() + local_apply;
  assert!(local_apply < local_checkpoint, "local sync checkpoint must follow the shared merge acknowledgement");
  assert!(!sync_engine[local_apply..local_checkpoint].contains("store_conflict("));

  let remote_cycle = sync_engine.find("async fn do_sync_cycle_remote(").unwrap();
  let remote_apply = sync_engine[remote_cycle..].find("apply_merge_operations_with_conflicts(").unwrap() + remote_cycle;
  let remote_checkpoint = sync_engine[remote_apply..].find("self.save_sync_state_hex(").unwrap() + remote_apply;
  assert!(remote_apply < remote_checkpoint, "remote sync checkpoint must follow the shared merge acknowledgement");
  assert!(sync_engine[remote_cycle..remote_apply].contains("three_way_merge(&local_diff, &remote_diff)"));
  assert!(sync_engine.contains("last_local_root_hash"));
  assert!(!sync_engine[remote_cycle..remote_checkpoint].contains("apply_merge_operations(&"));
  assert_eq!(sync_engine.matches("self.sync_request_context()").count(), 2);

  let server = include_str!("../../src/server/mod.rs");
  assert!(server.contains(".with_event_bus(Arc::clone(&event_bus))"));

  let sync_routes = include_str!("../../src/server/sync_routes.rs");
  assert!(sync_routes.contains("compute_sync_diff_accounted_with_cancellation("));
  assert!(!sync_routes.contains("compute_sync_diff_accounted(&state.engine"));
  assert!(sync_routes.contains("build_sync_diff_response("));
  assert!(sync_routes.contains("build_sync_chunks_response("));
  assert!(sync_routes.matches("ResponseBuildGuard::new()").count() >= 2);
  assert!(sync_routes.matches("tokio::task::spawn_blocking").count() >= 2);
  assert!(sync_routes.contains("cancellation.check()?"));
  assert!(sync_routes.contains("body_from_tempfile(response_file"));
}

#[test]
fn wave_three_conflict_resolution_and_cleanup_share_one_receipt() {
  let source = include_str!("../../src/engine/conflict_store.rs");
  let resolve_start = source.find("pub fn resolve_conflict(").unwrap();
  let resolve_end = source[resolve_start..].find("\npub fn dismiss_conflict(").unwrap() + resolve_start;
  let resolve_body = &source[resolve_start..resolve_end];
  assert_eq!(resolve_body.matches("ops.apply_sync_merge(").count(), 1);
  for forbidden in ["store_file_buffered(", "delete_file(", "store_conflict("] {
    assert!(!resolve_body.contains(forbidden), "conflict resolution retained split writer token: {forbidden}");
  }

  let dismiss_body = &source[resolve_end..];
  assert_eq!(dismiss_body.matches("ops.apply_sync_merge(").count(), 1);
  assert!(!dismiss_body.contains("ops.delete_file("), "conflict dismissal must not acknowledge cleanup through a second writer");
}

#[test]
fn wave_four_cron_uses_strict_atomic_config_and_observable_task_errors() {
  let cron = include_str!("../../src/engine/cron_scheduler.rs");
  assert!(cron.contains("fn mutate_cron_config"));
  assert!(!cron.contains("pub fn mutate_cron_config"), "generic namespace-locked callbacks must not be public API");
  assert!(cron.contains("pub fn create_cron_schedule"));
  assert!(cron.contains("pub fn update_cron_schedule"));
  assert!(cron.contains("pub fn delete_cron_schedule"));
  assert!(cron.contains("transform_file_buffered"));
  assert!(cron.contains("pub fn run_cron_tick"));
  assert!(!cron.contains("Err(_) => Vec::new()"));
  assert!(!cron.contains("Err(_) => false"));
  assert!(!cron.contains("let _ = queue.enqueue"));

  let routes = include_str!("../../src/server/task_routes.rs");
  assert_eq!(routes.matches("load_cron_config(&state.engine").count(), 1, "only the read-only list route may load directly");
  assert_eq!(routes.matches("create_cron_schedule(&state.engine").count(), 1);
  assert_eq!(routes.matches("delete_cron_schedule(&state.engine").count(), 1);
  assert_eq!(routes.matches("update_cron_schedule(&state.engine").count(), 1);
  assert!(!routes.contains("mutate_cron_config"), "HTTP routes must not own generic namespace-locked callbacks");
  assert!(!routes.contains("save_cron_config("), "cron routes retained split read/replace authority");
}

#[test]
fn wave_four_credentials_use_engine_owned_cache_fanout_and_typed_transitions() {
  let storage = include_str!("../../src/engine/storage_engine.rs");
  assert!(storage.contains("pub group_cache: Arc<Cache<GroupLoader>>"));
  assert!(storage.contains("pub api_key_cache: Arc<Cache<ApiKeyLoader>>"));

  let directory = include_str!("../../src/engine/directory_ops.rs");
  assert!(!directory.contains("invalidate_caches_for_paths"));
  let namespace = include_str!("../../src/engine/namespace_mutation.rs");
  assert!(namespace.contains("invalidate_caches_for_paths"));
  assert!(namespace.contains("invalidate_all_authority_caches"));
  assert!(namespace.contains("reconcile_live_namespace_from_head"));
  assert!(namespace.contains("pub(crate) fn set_incremental_head_hash"));
  assert!(namespace.contains("pub fn set_whole_root_hash"));
  assert!(namespace.contains("whole_root_publication && acknowledgement.previous_root_hash != acknowledgement.root_hash"));
  assert!(directory.contains("set_incremental_head_hash"));
  let version_manager = include_str!("../../src/engine/version_manager.rs");
  let backup = include_str!("../../src/engine/backup.rs");
  assert!(!version_manager.contains("set_incremental_head_hash"));
  assert!(!backup.contains("set_incremental_head_hash"));
  assert!(!version_manager.contains("reconcile_live_namespace_from_head"));
  assert!(!backup.contains("reconcile_live_namespace_from_head"));

  let system = include_str!("../../src/engine/system_store.rs");
  assert!(system.contains("API_KEY_STORE.transform"));
  assert!(system.contains("MAGIC_LINK_STORE.transform"));
  assert!(system.contains("REFRESH_TOKEN_STORE.transform"));
  let auth_provider = include_str!("../../src/auth/provider.rs");
  assert!(
    !auth_provider.contains("self.store_api_key_for_bootstrap(record)"),
    "root authority must not default to the bootstrap escape hatch"
  );

  let server = include_str!("../../src/server/mod.rs");
  assert!(server.contains("auth_engine"));
  assert!(!server.contains("Cache::new_bounded(GroupLoader"));
  assert!(!server.contains("Cache::new_bounded(ApiKeyLoader"));
  let plugin_manager = include_str!("../../src/plugins/plugin_manager.rs");
  assert!(plugin_manager.contains("pub fn invoke_wasm_plugin_with_auth("));
  assert!(plugin_manager.contains("pub fn invoke_wasm_plugin_with_authority_engines("));
  let wasm_runtime = include_str!("../../src/plugins/wasm_runtime.rs");
  assert!(wasm_runtime.contains("pub fn call_handle_with_context("));
  assert!(wasm_runtime.contains("pub fn call_handle_with_authority_engines("));

  for source in [
    include_str!("../../src/server/routes.rs"),
    include_str!("../../src/server/admin_routes.rs"),
    include_str!("../../src/server/api_key_self_service_routes.rs"),
    include_str!("../../src/server/share_link_routes.rs"),
    include_str!("../../src/server/share_routes.rs"),
    include_str!("../../src/server/engine_routes.rs"),
    include_str!("../../src/server/version_routes.rs"),
  ] {
    assert!(!source.contains("evict_caches_for_path"), "routes must not own post-commit cache invalidation");
  }
  let sharing = include_str!("../../src/server/share_routes.rs");
  assert_eq!(sharing.matches("PermissionStore::new").count(), 2, "share and unshare must use typed permission authority");
  assert!(!sharing.contains("store_file_buffered"), "share routes must not publish permission files directly");
  assert!(!sharing.contains("PermissionLink"), "share routes must not own permission document mutation");
  assert!(sharing.contains("let changed_paths = grant.changed_paths"));
  assert!(sharing.contains("if changed_paths.is_empty()"), "idempotent share retries must not emit false notifications");
  assert!(sharing.contains("let notify_paths = changed_paths.clone()"), "mixed retries may notify only paths that changed");
  assert!(!sharing.contains(".ok().flatten()"), "share routes must not squelch authority failures");
  let admin = include_str!("../../src/server/admin_routes.rs");
  assert!(!admin.contains("ops.read_file_buffered(&path)"), "API-key update must not manually deserialize outside authority");
  for source in [include_str!("../../src/server/routes.rs"), include_str!("../../src/server/api_key_self_service_routes.rs")] {
    assert!(!source.contains("store_api_key_for_bootstrap"), "authenticated routes must not masquerade as initial bootstrap");
    assert!(source.contains("store_api_key_with_root_authority"));
  }
}

#[test]
fn startup_root_validation_uses_one_bounded_strict_authority_probe() {
  let directory = include_str!("../../src/engine/directory_ops.rs");
  let start = directory.find("  pub fn ensure_root_directory(").unwrap();
  let end = directory[start..].find("\n  fn repair_workspace_file_child(").unwrap() + start;
  let body = &directory[start..end];

  assert!(body.contains("list_directory_window_strict(\"/\", 0, 1)"));
  assert!(!body.contains("list_directory(\"/\")"), "startup must not materialize and count the generic root listing");
  assert!(!body.contains("appears empty"), "zero ordinary children are not an integrity diagnostic");
  assert!(body.contains("root directory is not completely readable"));
  assert!(body.contains("let head_hash = planning_engine.head_hash()?"));
  assert!(body.contains("!head_hash.is_empty() && !head_hash.iter().all"));
  assert!(body.contains("root directory locator is missing while namespace authority"));
  assert!(body.contains("return Err(EngineError::CorruptEntry"));
  assert!(body.contains("aeordb verify --repair"));
}

#[test]
fn wave_four_peer_and_startup_authority_has_no_split_or_best_effort_path() {
  let system = include_str!("../../src/engine/system_store.rs");
  assert!(system.contains("pub struct PeerConfigStore"), "peer configuration needs one typed authority owner");
  assert!(system.contains("PEER_CONFIGS_DOC.transform"), "peer mutations must decide against current state while authority is held");
  assert!(system.contains("pub fn initialize_node_id"), "node identity needs create-once authority");

  let cluster = include_str!("../../src/server/cluster_routes.rs");
  assert!(!cluster.contains("system_store::store_peer_configs"), "cluster routes retained whole-document replacement authority");
  assert!(!cluster.contains("system_store::get_peer_configs"), "cluster mutation routes retained split peer-list reads");
  let add_start = cluster.find("pub async fn add_peer(").unwrap();
  let add_end = cluster[add_start..].find("\n/// GET /admin/cluster/peers").unwrap() + add_start;
  let add_body = &cluster[add_start..add_end];
  let persistent_add = add_body.find("PeerConfigStore::new").expect("typed peer persistence");
  let runtime_add = add_body.find("peer_manager.add_peer").expect("post-ack runtime peer publication");
  assert!(persistent_add < runtime_add, "runtime peer publication must follow persistent acknowledgement");

  let server = include_str!("../../src/server/mod.rs");
  assert!(!server.contains("Failed to persist node_id at startup"), "node identity failure must abort construction");
  assert!(!server.contains("peer synchronization remains disabled"), "malformed peer authority must abort construction");
  assert!(!server.contains("Failed to install bundled plugins"), "bundled plugin authority failure must abort construction");
  assert!(server.contains("try_create_app_with_all"), "production startup needs a fallible router-construction path");

  let cli_start = include_str!("../../../aeordb-cli/src/commands/start.rs");
  assert!(
    !cli_start.contains("Warning: failed to register some --peers"),
    "explicit --peers registration must fail startup when it is not acknowledged"
  );
}

#[test]
fn wave_four_plugin_policy_and_cache_identity_share_acknowledged_authority() {
  let plugin_manager = include_str!("../../src/plugins/plugin_manager.rs");
  let deploy_start = plugin_manager.find("  fn deploy_plugin_record(").unwrap();
  let deploy_end = plugin_manager[deploy_start..].find("\n  /// Retrieve a deployed plugin").unwrap() + deploy_start;
  let deploy = &plugin_manager[deploy_start..deploy_end];

  assert_eq!(deploy.matches("transform_file_buffered(").count(), 1);
  assert!(!deploy.contains("self.get_plugin("), "deploy policy must not read current state outside mutation authority");
  assert!(!deploy.contains("system_store::store_plugin"), "typed deployment must not retain the raw storage writer");
  assert!(plugin_manager.contains("struct PluginCacheKey"));
  assert!(plugin_manager.contains("checksum: String"));
  assert!(plugin_manager.contains("decode_stored_plugin_record"));
  assert!(plugin_manager.contains("PLUGIN_RECORD_MAX_BYTES"));
  assert!(plugin_manager.contains("invalidate_cached_runtime_after_ack"));
  assert!(!plugin_manager.contains("self.invalidate_cached_runtime(path)?"), "cache cleanup must not become pre-ack authority");
}

#[test]
fn wave_four_jwt_initialization_has_one_bounded_persistent_winner() {
  let system_store = include_str!("../../src/engine/system_store.rs");
  let auth_provider = include_str!("../../src/auth/provider.rs");
  let cluster_join = include_str!("../../src/engine/cluster_join.rs");
  let cluster_routes = include_str!("../../src/server/cluster_routes.rs");
  let cli_start = include_str!("../../../aeordb-cli/src/commands/start.rs");

  assert!(system_store.contains("pub fn initialize_jwt_signing_key"));
  assert!(system_store.contains("read_file_buffered_bounded(&path, LEGACY_CONFIG_VALUE_MAX_BYTES)"));
  assert!(system_store.contains("NamespaceMutationKind::SystemWrite"));
  assert!(auth_provider.contains("system_store::initialize_jwt_signing_key"));
  assert!(!auth_provider.contains("system_store::store_config"), "first-run auth must not retain split read/store publication");
  assert!(cluster_join.contains("system_store::get_jwt_signing_key"));
  assert!(cluster_routes.contains("system_store::get_jwt_signing_key"));
  assert!(cli_start.contains("system_store::store_jwt_signing_key"));
  assert!(!cli_start.contains("system_store::store_config(&engine, &ctx, \"jwt_signing_key\""));
}

#[test]
fn wave_four_email_config_is_bounded_and_never_squelches_masking_failure() {
  let email_config = include_str!("../../src/engine/email_config.rs");
  let settings_routes = include_str!("../../src/server/settings_routes.rs");

  assert!(email_config.contains("EMAIL_CONFIG_DOCUMENT_MAX_BYTES"));
  assert!(email_config.contains("EMAIL_CONFIG_FIELD_MAX_BYTES"));
  assert!(email_config.contains("read_file_buffered_bounded(EMAIL_CONFIG_PATH, EMAIL_CONFIG_DOCUMENT_MAX_BYTES)"));
  assert!(email_config.contains("pub fn masked(&self) -> EngineResult<serde_json::Value>"));
  assert!(!email_config.contains("serde_json::to_value(self).unwrap_or_default()"));
  assert!(settings_routes.contains("EngineError::InvalidInput(_) => StatusCode::BAD_REQUEST"));
  assert!(settings_routes.contains("EngineError::ResourceExhausted(_) => StatusCode::PAYLOAD_TOO_LARGE"));
}

#[test]
fn wave_four_legacy_system_path_migration_uses_one_atomic_file_transition() {
  let system_store = include_str!("../../src/engine/system_store.rs");
  let directory_operations = include_str!("../../src/engine/directory_ops.rs");
  let server = include_str!("../../src/server/mod.rs");

  let start = system_store.find("fn migrate_directory(").unwrap();
  let body = &system_store[start..];
  assert!(body.contains("migrate_system_file_alias"));
  assert!(!body.contains("read_file_buffered("), "system migration retained split source reads");
  assert!(!body.contains("store_file_buffered("), "system migration retained split destination writes");
  assert!(!body.contains("delete_file("), "system migration retained split source deletion");
  assert!(directory_operations.contains("pub(crate) fn migrate_system_file_alias"));
  assert!(directory_operations.contains("SYSTEM_FILE_ALIAS_RECORD_MAX_BYTES"));
  assert!(directory_operations.contains("resolve_current_file_record_from_bounded"));
  assert!(directory_operations.contains("NamespaceMutationKind::SystemWrite"));
  assert!(server.contains("system_store::migrate_system_paths(&engine)?"));
}

#[test]
fn wave_four_touched_system_and_plugin_failures_have_explicit_direction() {
  let system_store = include_str!("../../src/engine/system_store.rs");
  let plugin_routes = include_str!("../../src/server/routes.rs");
  let indexing_pipeline = include_str!("../../src/engine/indexing_pipeline.rs");
  let engine_routes = include_str!("../../src/server/engine_routes.rs");
  let metric_definitions = include_str!("../../src/metrics/definitions.rs");

  for obsolete_writer in
    ["pub fn store_permissions(", "pub fn store_node_id(", "pub fn store_peer_configs(", "pub fn store_plugin(", "pub fn remove_plugin("]
  {
    assert!(!system_store.contains(obsolete_writer), "legacy system-store writer remains: {obsolete_writer}");
  }
  assert!(!system_store.contains("pub fn get_permissions("), "the removed raw permission writer retained its duplicate reader API");
  assert!(!system_store.contains("pub fn get_plugin("), "plugin records must be read through PluginManager validation");
  assert!(!system_store.contains("pub fn list_plugins("), "plugin records must be enumerated through PluginManager validation");
  let plugin_manager = include_str!("../../src/plugins/plugin_manager.rs");
  for (owner, source) in [("system store", system_store), ("plugin manager", plugin_manager)] {
    for raw_bypass in [".store_entry(", ".store_entry_typed(", ".mark_entry_deleted(", "TransactionGuard"] {
      assert!(!source.contains(raw_bypass), "{owner} retained raw namespace-authority bypass {raw_bypass}");
    }
  }

  let invoke_start = plugin_routes.find("pub async fn invoke_plugin(").unwrap();
  let invoke_end = plugin_routes[invoke_start..].find("/// GET /plugins").unwrap() + invoke_start;
  let invoke = &plugin_routes[invoke_start..invoke_end];
  assert!(!invoke.contains("serde_json::to_vec(&plugin_request).unwrap_or_default()"));
  assert!(invoke.contains("request_serialization_failed"));
  assert!(invoke.contains("StatusCode::INTERNAL_SERVER_ERROR"));

  let log_start = indexing_pipeline.find("fn log_system(").unwrap();
  let log = &indexing_pipeline[log_start..];
  assert!(log.contains("transform_file_buffered"));
  assert!(log.contains("record_system_soft_failure"));
  assert!(!log.contains("read_file_buffered(&log_path).unwrap_or_default()"));
  assert!(!log.contains("let _ = ops.store_file_buffered"));

  let scheduling_start = engine_routes.find("// Auto-trigger reindex when indexes.json is stored").unwrap();
  let scheduling_end = engine_routes[scheduling_start..].find("let response_body").unwrap() + scheduling_start;
  let scheduling = &engine_routes[scheduling_start..scheduling_end];
  assert!(scheduling.contains("tokio::task::spawn_blocking"));
  assert!(scheduling.contains("schedule_automatic_reindex_after_commit"));
  assert!(scheduling.contains("\"worker_join\""));
  let orchestration_start = engine_routes.find("fn schedule_automatic_reindex_after_commit(").unwrap();
  let orchestration_end = engine_routes[orchestration_start..].find("// engine_get helper functions").unwrap() + orchestration_start;
  let orchestration = &engine_routes[orchestration_start..orchestration_end];
  assert!(orchestration.contains("record_system_soft_failure"));
  assert!(!orchestration.contains("if let Ok(tasks) = queue.list_tasks()"));
  assert!(!orchestration.contains("let _ = queue.cancel"));
  assert!(!orchestration.contains(".read_file_buffered(config_path)\n        .ok()"));
  assert!(!orchestration.contains("PathIndexConfig::deserialize(&data).ok()"));
  assert!(!orchestration.contains("let _ = queue.enqueue"));
  assert!(metric_definitions.contains("SYSTEM_SOFT_FAILURES_TOTAL"));
}

#[test]
fn wave_five_reindex_retains_exact_partial_outcomes_and_a_contiguous_checkpoint() {
  let task_worker = include_str!("../../src/engine/task_worker.rs");
  let errors = include_str!("../../src/engine/errors.rs");

  assert!(errors.contains("PartialOperation {"), "maintenance partial outcomes lost their typed error contract");
  assert!(task_worker.contains("ReindexFailureSummary"));
  assert!(task_worker.contains("failures.into_error(completed_count, false)"));
  assert!(task_worker.contains("failures.into_error(completed_count, true)"));
  assert!(task_worker.contains("checkpoint_path"));
  assert!(!task_worker.contains("last_processed_path"), "reindex can again checkpoint paths that were merely attempted");
  assert!(!task_worker.contains("fn reindex_circuit_breaker_error"), "circuit breaker again discards exact prior failure evidence");
  assert!(!task_worker.contains("index_buffer.flush_all()?"), "reindex flush failure can erase prior partial evidence");
  assert!(!task_worker.contains("index_buffer.stats()?"), "reindex buffer-state failure can erase prior partial evidence");
  assert!(!task_worker.contains("queue.update_checkpoint(&task.id, path)?"), "reindex checkpoint failure can erase prior partial evidence");
  for serious_failure in [
    "EngineError::IoError(_)",
    "EngineError::InvalidMagic",
    "EngineError::PartialOperation { .. }",
    "EngineError::SystemFamilyPolicy { .. }",
    "EngineError::DurabilityFailure(_)",
    "EngineError::PostMutationDurabilityFailure(_)",
  ] {
    assert!(task_worker.contains(serious_failure), "reindex serious-failure guard lost {serious_failure}");
  }
}

#[test]
fn wave_five_cleanup_uses_bounded_conditional_namespace_authority() {
  let system_store = include_str!("../../src/engine/system_store.rs");
  let task_worker = include_str!("../../src/engine/task_worker.rs");
  let task_routes = include_str!("../../src/server/task_routes.rs");
  let sse_routes = include_str!("../../src/server/sse_routes.rs");
  let start = system_store.find("trait CredentialCleanupRecord").expect("cleanup implementation");
  let end = system_store[start..]
    .find("// ---------------------------------------------------------------------------\n// Cluster / Replication")
    .expect("cleanup section")
    + start;
  let cleanup = &system_store[start..end];

  assert!(cleanup.contains("OperationMemoryBudget"), "cleanup scan lost process-wide maintenance admission");
  assert!(cleanup.contains("visit_live_directory_children_strict"), "cleanup again materializes a complete credential directory");
  assert!(cleanup.contains("delete_files_batch_with_kind"), "cleanup no longer uses one conditional namespace batch");
  assert!(cleanup.contains("optional_matching_identity"), "cleanup can delete a credential replaced after scan");
  assert!(cleanup.contains("NamespaceMutationKind::MaintenanceRepair"));
  assert!(!cleanup.contains("ops.delete_file("), "cleanup again acknowledges one delete per credential");
  assert!(!cleanup.contains("list_directory_strict("), "cleanup again retains an unbounded full listing");

  let task_start = task_worker.find("fn execute_cleanup(").expect("cleanup task adapter");
  let task_end = task_worker[task_start..].find("/// Remove oldest").expect("cleanup task adapter end") + task_start;
  let cleanup_task = &task_worker[task_start..task_end];
  assert!(cleanup_task.contains("RequestContext::with_bus"), "queued cleanup again discards acknowledged deletion events");
  assert!(!cleanup_task.contains("RequestContext::system()"), "queued cleanup uses an eventless request context");

  let route_start = task_routes.find("pub async fn trigger_cleanup(").expect("cleanup HTTP adapter");
  let route_end = task_routes[route_start..].find("/// GET /admin/tasks/{id}").expect("cleanup HTTP adapter end") + route_start;
  let cleanup_route = &task_routes[route_start..route_end];
  assert!(cleanup_route.contains("engine_error_response"), "cleanup HTTP errors no longer retain typed retry direction");
  assert!(
    cleanup_route.contains("EngineError::PartialOperation"),
    "root cleanup HTTP no longer retains exact acknowledged partial evidence"
  );
  assert!(cleanup_route.contains("error_codes::INTERNAL_ERROR"));
  assert!(!cleanup_route.contains("Err(e) =>"), "cleanup HTTP adapter again flattens every failure into one untyped response");

  assert!(sse_routes.contains("project_event_for_subscriber"), "batched cleanup events are no longer projected per subscriber");
  assert!(sse_routes.contains("SystemFamilyPolicyResolver"), "protected cleanup paths can leak through non-root SSE subscriptions");
  assert!(sse_routes.contains("PermissionResolver"), "ordinary SSE paths can bypass current user/group authority");
  assert!(sse_routes.contains("current_key_rules"), "long-lived SSE streams can retain stale API-key authority");
  assert!(sse_routes.contains("event.recipient_user_id.is_some()"), "recipient-addressed events can leak through the global SSE stream");
  assert!(sse_routes.contains("NonRootEventVisibility::RootOnly"), "administrative SSE payloads can leak to non-root subscribers");
  assert!(!sse_routes.contains("any_path_allowed_by_rules"), "one allowed batch member again exposes denied sibling paths");
}

#[test]
fn wave_five_authorized_mutation_events_retain_one_producer_projector_and_client_adapter() {
  let namespace_mutation = include_str!("../../src/engine/namespace_mutation.rs");
  let directory_ops = include_str!("../../src/engine/directory_ops.rs");
  let version_manager = include_str!("../../src/engine/version_manager.rs");
  let backup = include_str!("../../src/engine/backup.rs");
  let root_public_schema = include_str!("../../src/server/root_public_schema.rs");
  let sse_routes = include_str!("../../src/server/sse_routes.rs");
  let portal_routes = include_str!("../../src/server/portal_routes.rs");
  let server = include_str!("../../src/server/mod.rs");
  let file_event_contract = include_str!("../../src/portal/file-event-contract.mjs");
  let files = include_str!("../../src/portal/files.mjs");
  let app = include_str!("../../src/portal/app.mjs");

  assert_eq!(namespace_mutation.matches("pub fn annotate_event_payload(").count(), 1, "mutation event metadata gained a second producer");
  assert!(namespace_mutation.contains(".map(LogicalAffectedRelationship::from_source_identity)"));
  assert!(namespace_mutation.contains(".collect::<EngineResult<Vec<_>>>()?"));
  for (owner, source) in [("directory operations", directory_ops), ("version manager", version_manager), ("backup", backup)] {
    assert!(source.contains("acknowledgement.annotate_event_payload"), "{owner} bypasses the canonical mutation event producer");
    assert!(!source.contains("\"affected_relationships\""), "{owner} independently constructs public mutation relationships");
  }

  assert_eq!(root_public_schema.matches("pub struct PublicAffectedRelationshipV1").count(), 1, "public relationship schema is duplicated");
  assert_eq!(sse_routes.matches("fn project_event_for_subscriber(").count(), 1, "SSE gained a second subscriber projector");
  assert_eq!(
    sse_routes.matches("serde_json::from_value::<PublicAffectedRelationshipV1>").count(),
    1,
    "SSE relationship authority is decoded outside the sole projector"
  );
  assert_eq!(
    sse_routes.matches("project_event_for_subscriber(").count(),
    3,
    "an SSE delivery path bypasses or duplicates the sole projector"
  );

  assert_eq!(file_event_contract.matches("JSON.parse(serializedEvent)").count(), 1, "the client contract gained a second event parser");
  assert!(file_event_contract.contains("Object.hasOwn(response, alternateCollectionName)"));
  assert!(!file_event_contract.contains("response.items ||"));
  assert!(!file_event_contract.contains("response.results ||"));
  assert_eq!(
    files.matches("class AeorDBFileBrowserPortal extends AeorFileBrowserPortal").count(),
    1,
    "the bundled client adapter is duplicated"
  );
  assert_eq!(
    files.matches("this._readRootedResponse(response,").count(),
    2,
    "browse and search no longer share one rooted response adapter"
  );
  assert_eq!(files.matches("projectForDirectory(event.data, tab.path)").count(), 1, "mutation SSE gained a second client projection path");
  assert!(files.contains("queueMicrotask("));
  assert!(files.contains("this._refreshListingInBackground(tab)"));
  for forbidden_refresh_path in ["setTimeout(", "debounce", "_fetchListing("] {
    assert!(!files.contains(forbidden_refresh_path), "bundled client retained forbidden refresh path {forbidden_refresh_path}");
  }

  assert_eq!(server.matches(".route(\"/shared/{*path}\"").count(), 1, "portal shared-asset delivery is no longer one wildcard route");
  assert!(!server.contains(".route(\"/shared/file-event-contract.mjs\""), "client contract gained a dedicated public route");
  assert_eq!(portal_routes.matches("\"file-event-contract.mjs\" =>").count(), 1, "client contract asset mapping is duplicated");
  assert_eq!(
    app.matches("browser.refreshActiveListingFromEvent({").count(),
    1,
    "share notices gained a second file-browser refresh adapter"
  );
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
