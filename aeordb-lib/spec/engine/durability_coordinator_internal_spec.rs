use std::sync::Arc;

use super::*;

fn poison_coordinator_state(coordinator: &Arc<DurabilityCoordinator>) {
  let coordinator = Arc::clone(coordinator);
  let result = std::thread::spawn(move || {
    let _state = coordinator.state.lock().unwrap();
    panic!("injected durability coordinator poison");
  })
  .join();
  assert!(result.is_err());
}

fn poisoned_state(coordinator: &DurabilityCoordinator) -> std::sync::MutexGuard<'_, CoordinatorState> {
  match coordinator.state.lock() {
    Ok(_) => panic!("durability coordinator unexpectedly lost its poison state"),
    Err(poisoned) => poisoned.into_inner(),
  }
}

#[test]
fn dropping_a_drive_permit_clears_driver_ownership_after_state_poison() {
  let coordinator = Arc::new(DurabilityCoordinator::new());
  coordinator.state.lock().unwrap().driver_active = true;
  let permit = DurabilityDrivePermit { coordinator: &coordinator, active: true };
  poison_coordinator_state(&coordinator);

  drop(permit);

  assert!(!poisoned_state(&coordinator).driver_active);
  assert!(matches!(coordinator.snapshot(), Err(DurabilityCoordinatorError::StateUnavailable)));
}

#[test]
fn explicitly_releasing_a_drive_permit_clears_driver_ownership_after_state_poison() {
  let coordinator = Arc::new(DurabilityCoordinator::new());
  coordinator.state.lock().unwrap().driver_active = true;
  let permit = DurabilityDrivePermit { coordinator: &coordinator, active: true };
  poison_coordinator_state(&coordinator);

  permit.release();

  assert!(!poisoned_state(&coordinator).driver_active);
  assert!(matches!(coordinator.snapshot(), Err(DurabilityCoordinatorError::StateUnavailable)));
}

struct PoisoningExecutor {
  coordinator: Arc<DurabilityCoordinator>,
}

impl DurabilityExecutor for PoisoningExecutor {
  type Error = std::io::Error;

  fn execute(&mut self, _sequence: u64, _operation: DurabilityOperation) -> Result<(), Self::Error> {
    let _state = self.coordinator.state.lock().unwrap();
    panic!("injected executor poison");
  }
}

#[test]
fn execution_guard_fails_an_executing_hard_record_even_when_the_executor_poisons_state() {
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let plan = DurabilityCommitPlan::new(
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
  .unwrap();
  let ticket = coordinator.admit(plan).unwrap();
  let mut executor = PoisoningExecutor { coordinator: Arc::clone(&coordinator) };

  let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| coordinator.execute(ticket, &mut executor)));
  assert!(unwind.is_err());

  let state = poisoned_state(&coordinator);
  assert!(matches!(state.records.get(&ticket.sequence).map(|record| &record.status), Some(CommitStatus::Failed(_))));
  assert!(state.hard_failure.is_some(), "hard authority unwind must latch a hard failure even after lock poison");
  drop(state);
  assert!(matches!(coordinator.snapshot(), Err(DurabilityCoordinatorError::StateUnavailable)));
}
