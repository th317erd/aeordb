use std::sync::Arc;

use super::*;

#[test]
fn poisoned_cancellation_authority_fails_closed_for_running_workers() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let cancellation_state = Arc::clone(&queue.cancelled);

  let poisoner = std::thread::spawn(move || {
    let _guard = cancellation_state.write().unwrap();
    panic!("inject cancellation-state poison");
  });
  assert!(poisoner.join().is_err());

  assert!(queue.is_cancelled("not-yet-recorded"));
}

#[test]
fn poisoned_cancellation_authority_rejects_in_memory_mutation() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let cancellation_state = Arc::clone(&queue.cancelled);

  let poisoner = std::thread::spawn(move || {
    let _guard = cancellation_state.write().unwrap();
    panic!("inject cancellation-state poison");
  });
  assert!(poisoner.join().is_err());

  let error = queue.mark_cancelled_in_memory("task-id").unwrap_err();
  assert!(matches!(error, EngineError::IoError(_)));
}

#[test]
fn persisted_cancel_is_visible_even_when_in_memory_authority_is_poisoned() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let task = queue.enqueue("cleanup", serde_json::json!({})).unwrap();
  let cancellation_state = Arc::clone(&queue.cancelled);

  let poisoner = std::thread::spawn(move || {
    let _guard = cancellation_state.write().unwrap();
    panic!("inject cancellation-state poison");
  });
  assert!(poisoner.join().is_err());

  let error = queue.cancel(&task.id).unwrap_err();
  assert!(matches!(error, EngineError::IoError(_)));
  assert_eq!(queue.get_task(&task.id).unwrap().unwrap().status, TaskStatus::Cancelled);
  assert!(queue.is_cancelled(&task.id));
}

#[test]
fn poisoned_active_cancellation_authority_rejects_registration() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let active_state = Arc::clone(&queue.active_cancellations);

  let poisoner = std::thread::spawn(move || {
    let _guard = active_state.write().unwrap();
    panic!("inject active-cancellation poison");
  });
  assert!(poisoner.join().is_err());

  let parent = CancellationToken::new();
  let result = queue.register_active_cancellation("task-id", &parent);
  assert!(matches!(result, Err(EngineError::IoError(_))));
}

#[test]
fn active_cancellation_drop_does_not_recover_poisoned_state() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let parent = CancellationToken::new();
  let active = queue.register_active_cancellation("task-id", &parent).unwrap();
  let active_state = Arc::clone(&queue.active_cancellations);

  let poisoner = std::thread::spawn(move || {
    let _guard = active_state.write().unwrap();
    panic!("inject active-cancellation poison");
  });
  assert!(poisoner.join().is_err());

  drop(active);
}

#[test]
fn poisoned_progress_telemetry_degrades_without_reusing_unknown_state() {
  let (engine, _temporary) = crate::server::create_temp_engine_for_tests();
  let queue = Arc::new(TaskQueue::new(engine));
  let progress_state = Arc::clone(&queue.progress);

  let poisoner = std::thread::spawn(move || {
    let _guard = progress_state.write().unwrap();
    panic!("inject progress-state poison");
  });
  assert!(poisoner.join().is_err());

  let info = ProgressInfo {
    task_id: "task-id".to_string(),
    task_type: "reindex".to_string(),
    args: serde_json::json!({"path": "/"}),
    progress: 0.5,
    eta_ms: Some(100),
    indexed_count: 1,
    total_count: 2,
    stale_since: None,
    message: None,
  };
  queue.set_progress("task-id", info);
  assert!(queue.get_progress("task-id").is_none());
  assert!(queue.get_reindex_progress_for_path("/docs/file.txt").is_none());
  queue.clear_progress("task-id");
}

#[test]
fn task_queue_never_recovers_poisoned_lock_contents() {
  let source = include_str!("../../src/engine/task_queue.rs");

  assert!(!source.contains(".into_inner()"));
}
