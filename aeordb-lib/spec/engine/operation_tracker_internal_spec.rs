use std::sync::Arc;

use super::*;

fn poison_tracker_state(tracker: &Arc<EngineOperationTracker>) {
  let tracker = Arc::clone(tracker);
  let result = std::thread::spawn(move || {
    let _state = tracker.state.lock().unwrap();
    panic!("injected operation tracker poison");
  })
  .join();
  assert!(result.is_err());
}

#[test]
fn poisoned_tracker_fails_closed_without_hiding_active_operations() {
  let tracker = Arc::new(EngineOperationTracker::default());
  let engine_id = Arc::as_ptr(&tracker) as usize;
  let operation = tracker.begin(engine_id, "held_read").unwrap();
  poison_tracker_state(&tracker);

  tracker.begin_shutdown();
  let held = tracker.snapshot();
  assert!(held.shutting_down);
  assert_eq!(held.active_operations, 1);
  assert_eq!(held.operations, vec![("held_read".to_string(), 1)]);

  drop(operation);
  let drained = tracker.wait_until_idle(std::time::Duration::ZERO);
  assert!(drained.shutting_down);
  assert_eq!(drained.active_operations, 0);
  assert!(drained.operations.is_empty());
  assert!(matches!(tracker.begin(engine_id, "late_read"), Err(EngineError::ShuttingDown)));
}

#[test]
fn poisoned_tracker_still_releases_exclusive_maintenance() {
  let tracker = Arc::new(EngineOperationTracker::default());
  let engine_id = Arc::as_ptr(&tracker) as usize;
  let operation = tracker.begin(engine_id, "maintenance_owner").unwrap();
  let maintenance = tracker.begin_maintenance(engine_id, std::time::Duration::ZERO).unwrap();
  poison_tracker_state(&tracker);

  drop(maintenance);

  let state = match tracker.state.lock() {
    Ok(_) => panic!("operation tracker unexpectedly lost its poison state"),
    Err(poisoned) => poisoned.into_inner(),
  };
  assert!(!state.maintenance_in_progress);
  assert!(state.shutting_down);
  drop(state);
  drop(operation);
  assert_eq!(tracker.snapshot().active_operations, 0);
}

#[test]
fn poisoned_tracker_waits_for_a_live_operation_to_drain() {
  let tracker = Arc::new(EngineOperationTracker::default());
  let engine_id = Arc::as_ptr(&tracker) as usize;
  let (started_tx, started_rx) = std::sync::mpsc::channel();
  let (release_tx, release_rx) = std::sync::mpsc::channel();
  let tracker_for_worker = Arc::clone(&tracker);
  let worker = std::thread::spawn(move || {
    let operation = tracker_for_worker.begin(engine_id, "slow_read").unwrap();
    started_tx.send(()).unwrap();
    release_rx.recv().unwrap();
    drop(operation);
  });
  started_rx.recv().unwrap();
  poison_tracker_state(&tracker);
  tracker.begin_shutdown();
  release_tx.send(()).unwrap();

  let drained = tracker.wait_until_idle(std::time::Duration::from_secs(1));

  worker.join().unwrap();
  assert!(drained.shutting_down);
  assert_eq!(drained.active_operations, 0);
  assert!(drained.operations.is_empty());
}
