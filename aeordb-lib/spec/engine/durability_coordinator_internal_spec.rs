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
