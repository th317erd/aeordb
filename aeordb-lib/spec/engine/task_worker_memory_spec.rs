use std::sync::Arc;

use aeordb::auth::refresh::RefreshTokenRecord;
use aeordb::engine::engine_event::{
  EVENT_ENTRIES_DELETED, EVENT_TASKS_CANCELLED, EVENT_TASKS_COMPLETED, EVENT_TASKS_DEFERRED, EVENT_TASKS_FAILED, EVENT_TASKS_STARTED,
};
use aeordb::engine::event_bus::EventBus;
use aeordb::engine::config_resolver::ConfigurationFamily;
use aeordb::engine::memory_coordinator::{AdmissionClass, HostMemorySample, MemoryOwner};
use aeordb::engine::task_queue::{TaskQueue, TaskStatus};
use aeordb::engine::task_worker::{
  process_next_task, process_next_task_with_cancel_and_pre_execute_hook, process_next_task_with_post_dequeue_hook,
  process_next_task_with_pre_execute_hook,
};
use aeordb::engine::{RequestContext, system_store};
use aeordb::plugins::PluginManager;
use aeordb::server::create_temp_engine_for_tests;
use tokio_util::sync::CancellationToken;

#[test]
fn running_task_keeps_the_policy_generation_captured_before_dequeue() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  queue.enqueue("unknown-test-task", serde_json::json!({})).unwrap();
  let captured_generation = engine.configuration_snapshot().generation;
  let engine_for_hook = engine.clone();

  assert!(process_next_task_with_post_dequeue_hook(&queue, &engine, &plugin_manager, &event_bus, move || {
    engine_for_hook
      .replace_configuration_document(ConfigurationFamily::Runtime, br#"{"schema_version":1,"maintenance":{"max_concurrent_tasks":3}}"#)
      .unwrap();
  })
  .unwrap());

  let started = events.try_recv().expect("task must emit a started event");
  assert_eq!(started.event_type, EVENT_TASKS_STARTED);
  assert_eq!(started.payload["configuration_generation"], captured_generation);
  assert!(engine.configuration_snapshot().generation > captured_generation);
}

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

#[test]
fn dequeue_accounts_large_task_records_instead_of_hiding_them_behind_fixed_workspace() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let padding = "x".repeat(512 * 1024);
  let task = queue.enqueue("unknown-test-task", serde_json::json!({"padding": padding})).unwrap();

  assert!(process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  assert_eq!(queue.get_task(&task.id).unwrap().unwrap().status, TaskStatus::Failed);

  let task_owner = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().clone();
  assert!(task_owner.peak_reserved_bytes > 512 * 1024, "task record body must be reserved before dequeue deserializes it");
  assert_eq!(task_owner.reserved_bytes, 0);
  assert_eq!(task_owner.active_reservations, 0);
}

#[test]
fn queued_cleanup_emits_one_batched_deletion_event_between_task_lifecycle_events() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let context = RequestContext::system();
  for token_hash in ["expired-a", "expired-b"] {
    system_store::store_refresh_token(
      &engine,
      &context,
      &RefreshTokenRecord {
        token_hash: token_hash.to_string(),
        user_subject: "task-user".to_string(),
        created_at: chrono::Utc::now() - chrono::Duration::hours(2),
        expires_at: chrono::Utc::now() - chrono::Duration::hours(1),
        is_revoked: false,
        key_id: None,
      },
    )
    .unwrap();
  }
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("cleanup", serde_json::json!({})).unwrap();

  assert!(process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  assert_eq!(queue.get_task(&task.id).unwrap().unwrap().status, TaskStatus::Completed);

  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_STARTED);
  let deleted = events.try_recv().expect("queued cleanup must publish its acknowledged namespace event");
  assert_eq!(deleted.event_type, EVENT_ENTRIES_DELETED);
  assert_eq!(deleted.payload["mutation_kind"], "maintenance_repair");
  let deleted_paths: std::collections::HashSet<_> =
    deleted.payload["entries"].as_array().unwrap().iter().map(|entry| entry["path"].as_str().unwrap()).collect();
  assert_eq!(
    deleted_paths,
    std::collections::HashSet::from(["/.aeordb-system/refresh-tokens/expired-a", "/.aeordb-system/refresh-tokens/expired-b",])
  );
  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_COMPLETED);
  assert!(events.try_recv().is_err(), "one cleanup batch must not emit duplicate deletion events");
}

#[test]
fn pressure_after_dequeue_requeues_with_checkpoint_and_emits_only_deferred() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("unknown-test-task", serde_json::json!({})).unwrap();
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();

  let coordinator = engine.memory_coordinator().clone();
  let soft_limit = coordinator.snapshot().unwrap().policy.unwrap().soft_limit_bytes;
  assert!(process_next_task_with_post_dequeue_hook(&queue, &engine, &plugin_manager, &event_bus, move || {
    coordinator
      .update_host_sample(HostMemorySample { rss_bytes: soft_limit, host_available_bytes: Some(u64::MAX), ..HostMemorySample::default() })
      .unwrap();
  })
  .unwrap());

  let pending = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert!(pending.started_at.is_none());
  assert!(pending.completed_at.is_none());
  assert!(pending.error.is_none());
  assert_eq!(pending.checkpoint.as_deref(), Some("/docs/page-0042.json"));

  let deferred_event = events.try_recv().expect("claimed maintenance must emit a deferred event when it is requeued");
  assert_eq!(deferred_event.event_type, EVENT_TASKS_DEFERRED);
  assert!(events.try_recv().is_err(), "deferred work must not emit started or failed events");
}

#[test]
fn cancellation_after_dequeue_wins_without_false_started_or_failed_events() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("unknown-test-task", serde_json::json!({})).unwrap();

  assert!(
    process_next_task_with_post_dequeue_hook(&queue, &engine, &plugin_manager, &event_bus, || queue.cancel(&task.id).unwrap()).unwrap()
  );

  let cancelled = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(cancelled.status, TaskStatus::Cancelled);
  assert!(cancelled.error.is_none());
  let cancelled_event = events.try_recv().expect("worker must publish the cancellation outcome");
  assert_eq!(cancelled_event.event_type, EVENT_TASKS_CANCELLED);
  assert!(events.try_recv().is_err(), "cancelled work must not emit started or failed events");
}

#[test]
fn pressure_after_started_requeues_reindex_before_config_or_listing_work() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();
  let coordinator = engine.memory_coordinator().clone();
  let pressured_coordinator = coordinator.clone();
  let soft_limit = coordinator.snapshot().unwrap().policy.unwrap().soft_limit_bytes;

  assert!(process_next_task_with_pre_execute_hook(&queue, &engine, &plugin_manager, &event_bus, move || {
    pressured_coordinator
      .update_host_sample(HostMemorySample { rss_bytes: soft_limit, host_available_bytes: Some(u64::MAX), ..HostMemorySample::default() })
      .unwrap();
  })
  .unwrap());

  let pending = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert_eq!(pending.checkpoint.as_deref(), Some("/docs/page-0042.json"));
  assert!(pending.started_at.is_none());
  assert!(pending.completed_at.is_none());
  assert!(pending.error.is_none());
  let retry_at = pending.retry_at.expect("pressure deferral must persist a retry eligibility time");
  assert!(retry_at > chrono::Utc::now().timestamp_millis());
  assert_eq!(pending.deferral_count, 1);
  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_STARTED);
  let deferred = events.try_recv().unwrap();
  assert_eq!(deferred.event_type, EVENT_TASKS_DEFERRED);
  assert_eq!(deferred.payload["retry_at"], retry_at);
  assert!(deferred.payload["retry_after_ms"].as_i64().is_some_and(|delay| delay > 0));
  assert!(events.try_recv().is_err());

  coordinator.update_host_sample(HostMemorySample::default()).unwrap();
  assert!(!process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  assert!(events.try_recv().is_err(), "worker must not churn lifecycle events before retry_at");
}

#[test]
fn worker_shutdown_after_started_requeues_checkpointed_reindex() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let task = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();
  let worker_cancel = CancellationToken::new();

  assert!(process_next_task_with_cancel_and_pre_execute_hook(&queue, &engine, &plugin_manager, &event_bus, &worker_cancel, || {
    worker_cancel.cancel()
  },)
  .unwrap());

  let pending = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert_eq!(pending.checkpoint.as_deref(), Some("/docs/page-0042.json"));
  assert!(pending.started_at.is_none());
  assert!(pending.completed_at.is_none());
  assert!(pending.error.is_none());
  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_STARTED);
  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_DEFERRED);
  assert!(events.try_recv().is_err());
}

#[test]
fn panic_after_dequeue_requeues_checkpointed_task_in_the_same_process() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = Arc::new(EventBus::new());
  let task = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _ = process_next_task_with_pre_execute_hook(&queue, &engine, &plugin_manager, &event_bus, || {
      panic!("deterministic task worker panic");
    });
  }));
  assert!(result.is_err());

  let pending = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert_eq!(pending.checkpoint.as_deref(), Some("/docs/page-0042.json"));
  assert!(pending.started_at.is_none());
  assert!(pending.completed_at.is_none());
  assert!(pending.error.is_none());
}
