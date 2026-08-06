use std::sync::Arc;

use aeordb::engine::engine_event::{EVENT_TASKS_FAILED, EVENT_TASKS_STARTED};
use aeordb::engine::event_bus::EventBus;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::task_queue::{TaskQueue, TaskStatus};
use aeordb::engine::task_worker::process_next_task;
use aeordb::plugins::PluginManager;
use aeordb::server::create_temp_engine_for_tests;

#[test]
fn soft_pressure_defers_before_dequeue_and_retries_after_pressure_clears() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("unknown-test-task", serde_json::json!({})).unwrap();

  let coordinator = engine.memory_coordinator();
  let before = coordinator.snapshot().unwrap();
  let policy = before.policy.expect("test engine must have a resolved memory policy");
  let pressure_bytes = policy.soft_limit_bytes.saturating_sub(before.accounted_bytes);
  let pressure = coordinator.reserve(MemoryOwner::Query, pressure_bytes, AdmissionClass::Workload).unwrap();

  assert!(!process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  let deferred = queue.get_task(&task.id).unwrap().expect("task must remain stored");
  assert_eq!(deferred.status, TaskStatus::Pending);
  assert!(deferred.started_at.is_none());
  assert!(events.try_recv().is_err(), "a deferred task must not emit a started event");

  let pressure_snapshot = coordinator.snapshot().unwrap();
  let task_owner = pressure_snapshot.owner(MemoryOwner::Task).unwrap();
  assert_eq!(task_owner.deferrals, 1);
  assert_eq!(task_owner.active_reservations, 0);

  drop(pressure);
  assert!(process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  let failed = queue.get_task(&task.id).unwrap().expect("task must remain stored");
  assert_eq!(failed.status, TaskStatus::Failed);

  let started_event = events.try_recv().expect("admitted retry must emit a started event");
  assert_eq!(started_event.event_type, EVENT_TASKS_STARTED);
  let failed_event = events.try_recv().expect("unknown task must emit a failed event");
  assert_eq!(failed_event.event_type, EVENT_TASKS_FAILED);

  let released = coordinator.snapshot().unwrap();
  let task_owner = released.owner(MemoryOwner::Task).unwrap();
  assert!(task_owner.peak_reserved_bytes > 0);
  assert_eq!(task_owner.reserved_bytes, 0);
  assert_eq!(task_owner.active_reservations, 0);
}

#[test]
fn hard_pressure_rejects_before_dequeue_without_failing_the_task() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("unknown-test-task", serde_json::json!({})).unwrap();

  let coordinator = engine.memory_coordinator();
  let before = coordinator.snapshot().unwrap();
  let policy = before.policy.expect("test engine must have a resolved memory policy");
  let pressure_bytes = policy.ordinary_limit_bytes().saturating_sub(before.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Query, pressure_bytes, AdmissionClass::Workload).unwrap();

  assert!(!process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  let deferred = queue.get_task(&task.id).unwrap().expect("task must remain stored");
  assert_eq!(deferred.status, TaskStatus::Pending);
  assert!(deferred.started_at.is_none());
  assert!(deferred.completed_at.is_none());
  assert!(deferred.error.is_none());
  assert!(events.try_recv().is_err(), "a rejected task must not emit task lifecycle events");

  let pressure_snapshot = coordinator.snapshot().unwrap();
  let task_owner = pressure_snapshot.owner(MemoryOwner::Task).unwrap();
  assert_eq!(task_owner.rejections, 1);
  assert_eq!(task_owner.reserved_bytes, 0);
  assert_eq!(task_owner.active_reservations, 0);
}
