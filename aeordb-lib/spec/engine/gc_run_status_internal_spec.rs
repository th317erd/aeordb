use super::*;
use crate::engine::v4::gc_run::{GcRunIDV1, GcRunInvocationV1, GcRunModeV1, GcRunPhaseV1};

fn status(run_byte: u8, state: GcRunStateV1, phase: Option<GcRunPhaseV1>, observed_at_ms: i64) -> GcRunStatusV1 {
  GcRunStatusV1 {
    run_id: GcRunIDV1::new([run_byte; 16]).unwrap(),
    invocation: GcRunInvocationV1::Embedded,
    mode: GcRunModeV1::NonDestructiveMark,
    state,
    phase,
    phase_progress: 0.0,
    overall_progress: 0.0,
    completed_units: 0,
    total_units: None,
    eta_ms: None,
    memory_reserved_bytes: 0,
    scratch_used_bytes: 0,
    mutation_journal_lag: 0,
    checkpoint_age_ms: None,
    started_at_ms: 1,
    observed_at_ms,
    completed_at_ms: is_terminal(state).then_some(observed_at_ms),
    code: None,
    message: None,
  }
}

fn snapshot(status: GcRunStatusV1, task_id: Option<&str>) -> GcRunStatusSnapshotV1 {
  GcRunStatusSnapshotV1 { status, task_id: task_id.map(str::to_string) }
}

struct PanickingObserver;

impl GcRunProgressSinkV1 for PanickingObserver {
  fn publish(&self, _status: &GcRunStatusV1) {
    panic!("intentional optional observer failure");
  }
}

#[test]
fn newer_projection_supersedes_older_publishers_without_retaining_history() {
  let registry = GcRunStatusRegistryV1::default();
  let first = snapshot(status(1, GcRunStateV1::Running, Some(GcRunPhaseV1::Prepare), 10), None);
  let second_task = uuid::Uuid::new_v4().to_string();
  let second = snapshot(status(2, GcRunStateV1::Running, Some(GcRunPhaseV1::Inventory), 11), Some(&second_task));
  let stale_terminal = snapshot(status(1, GcRunStateV1::Complete, Some(GcRunPhaseV1::Finalize), 12), None);

  assert!(registry.publish(1, first).is_some());
  assert!(registry.publish(2, second.clone()).is_some());
  assert!(registry.publish(1, stale_terminal).is_none());
  assert_eq!(registry.latest(), Some(second.clone()));
  assert_eq!(registry.for_task(&second_task), Some(second));
}

#[test]
fn one_projection_rejects_identity_timestamp_and_terminal_state_regression() {
  let registry = GcRunStatusRegistryV1::default();
  let running = snapshot(status(3, GcRunStateV1::Running, Some(GcRunPhaseV1::Mark), 20), None);
  assert!(registry.publish(7, running.clone()).is_some());

  let wrong_identity = snapshot(status(4, GcRunStateV1::Running, Some(GcRunPhaseV1::Mark), 21), None);
  let stale_timestamp = snapshot(status(3, GcRunStateV1::Running, Some(GcRunPhaseV1::Inventory), 19), None);
  assert!(registry.publish(7, wrong_identity).is_none());
  assert!(registry.publish(7, stale_timestamp).is_none());

  let terminal = snapshot(status(3, GcRunStateV1::Failed, Some(GcRunPhaseV1::Mark), 22), None);
  assert!(registry.publish(7, terminal.clone()).is_some());
  let running_again = snapshot(status(3, GcRunStateV1::Running, Some(GcRunPhaseV1::Finalize), 23), None);
  assert!(registry.publish(7, running_again).is_none());
  assert_eq!(registry.latest(), Some(terminal));
}

#[test]
fn projection_sequence_exhaustion_fails_before_status_or_event_publication() {
  let registry = Arc::new(GcRunStatusRegistryV1::default());
  registry.next_projection_sequence.store(u64::MAX, Ordering::Release);
  let result = registry.projection_sink(None, None, None);
  assert!(matches!(result, Err(EngineError::ResourceExhausted(message)) if message.contains("sequence is exhausted")));
  assert!(registry.latest().is_none());
}

#[test]
fn optional_observer_panic_cannot_prevent_mandatory_status_retention() {
  let registry = Arc::new(GcRunStatusRegistryV1::default());
  let sink = registry.projection_sink(None, None, Some(Arc::new(PanickingObserver))).unwrap();
  let expected = status(6, GcRunStateV1::Running, Some(GcRunPhaseV1::Prepare), 40);

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.publish(&expected)));
  assert!(result.is_err());
  assert_eq!(registry.latest(), Some(snapshot(expected, None)));
}

#[test]
fn concurrent_publishers_retain_the_newest_projection_without_a_lock_or_history() {
  let registry = Arc::new(GcRunStatusRegistryV1::default());
  let older = registry.projection_sink(None, None, None).unwrap();
  let newer = registry.projection_sink(None, None, None).unwrap();
  let barrier = Arc::new(std::sync::Barrier::new(3));

  let older_barrier = Arc::clone(&barrier);
  let older_thread = std::thread::spawn(move || {
    older_barrier.wait();
    older.publish(&status(7, GcRunStateV1::Complete, Some(GcRunPhaseV1::Finalize), 50));
  });
  let newer_barrier = Arc::clone(&barrier);
  let newer_thread = std::thread::spawn(move || {
    newer_barrier.wait();
    newer.publish(&status(8, GcRunStateV1::Running, Some(GcRunPhaseV1::Inventory), 51));
  });
  barrier.wait();
  older_thread.join().unwrap();
  newer_thread.join().unwrap();

  let retained = registry.latest().unwrap();
  assert_eq!(retained.status.run_id, GcRunIDV1::new([8; 16]).unwrap());
  assert_eq!(retained.status.state, GcRunStateV1::Running);
}
