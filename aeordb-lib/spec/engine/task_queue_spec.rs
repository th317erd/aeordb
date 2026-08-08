use std::thread;
use std::time::Duration;

use aeordb::engine::EntryType;
use aeordb::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use aeordb::engine::task_queue::{ProgressInfo, TaskQueue, TaskRecord, TaskStatus};
use aeordb::server::create_temp_engine_for_tests;

#[test]
fn test_enqueue_creates_pending_task() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let args = serde_json::json!({"path": "/docs"});
  let record = queue.enqueue("reindex", args.clone()).unwrap();

  assert_eq!(record.status, TaskStatus::Pending);
  assert_eq!(record.task_type, "reindex");
  assert_eq!(record.args, args);
  assert!(record.started_at.is_none());
  assert!(record.completed_at.is_none());
  assert!(record.error.is_none());
  assert!(record.checkpoint.is_none());

  // Verify it can be retrieved.
  let fetched = queue.get_task(&record.id).unwrap().expect("task should exist");
  assert_eq!(fetched.id, record.id);
  assert_eq!(fetched.status, TaskStatus::Pending);
  assert_eq!(fetched.task_type, "reindex");
  assert_eq!(fetched.args, args);
}

#[test]
fn persisted_task_transitions_each_use_one_hard_acknowledgement() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let before_enqueue = engine.durability_snapshot().unwrap();

  let record = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();

  let after_enqueue = engine.durability_snapshot().unwrap();
  assert_eq!(after_enqueue.next_sequence, before_enqueue.next_sequence + 1);
  assert!(after_enqueue.hard_frontier > before_enqueue.hard_frontier);

  queue.update_checkpoint(&record.id, "page:2").unwrap();

  let after_checkpoint = engine.durability_snapshot().unwrap();
  assert_eq!(after_checkpoint.next_sequence, after_enqueue.next_sequence + 1);
  assert!(after_checkpoint.hard_frontier > after_enqueue.hard_frontier);
  assert_eq!(queue.get_task(&record.id).unwrap().unwrap().checkpoint.as_deref(), Some("page:2"));
}

#[test]
fn shared_task_authority_preserves_the_exact_v3_storage_contract() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let record = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  let task_key = blake3::hash(format!("::aeordb:task:{}", record.id).as_bytes()).as_bytes().to_vec();
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();

  let (task_header, stored_task_key, task_bytes) = engine.get_entry_verified(&task_key).unwrap().unwrap();
  assert_eq!(task_header.entry_type, EntryType::FileRecord);
  assert_eq!(task_header.entry_version, 0);
  assert_eq!(task_header.flags, 0);
  assert_eq!(stored_task_key, task_key);
  assert_eq!(task_bytes, serde_json::to_vec(&record).unwrap());

  let (registry_header, stored_registry_key, registry_bytes) = engine.get_entry_verified(&registry_key).unwrap().unwrap();
  assert_eq!(registry_header.entry_type, EntryType::FileRecord);
  assert_eq!(registry_header.entry_version, 0);
  assert_eq!(registry_header.flags, 0);
  assert_eq!(stored_registry_key, registry_key);
  assert_eq!(registry_bytes, serde_json::to_vec(&vec![record.id]).unwrap());
}

#[test]
fn task_pruning_retires_rows_and_registry_under_one_hard_acknowledgement() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let first = queue.enqueue("cleanup", serde_json::json!({"ordinal": 1})).unwrap();
  let second = queue.enqueue("cleanup", serde_json::json!({"ordinal": 2})).unwrap();
  queue.update_status(&first.id, TaskStatus::Completed, None).unwrap();
  queue.update_status(&second.id, TaskStatus::Completed, None).unwrap();
  let before_prune = engine.durability_snapshot().unwrap();

  assert_eq!(queue.prune_completed(0, 0).unwrap(), 2);

  let after_prune = engine.durability_snapshot().unwrap();
  assert_eq!(after_prune.next_sequence, before_prune.next_sequence + 1);
  assert!(after_prune.hard_frontier > before_prune.hard_frontier);
  assert!(queue.list_tasks().unwrap().is_empty());
  assert!(queue.get_task(&first.id).unwrap().is_none());
  assert!(queue.get_task(&second.id).unwrap().is_none());

  assert_eq!(queue.prune_completed(0, 0).unwrap(), 0);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, after_prune.next_sequence);
}

#[test]
fn task_pruning_refuses_before_changing_rows_or_registry_under_waiter_pressure() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let record = queue.enqueue("cleanup", serde_json::json!({})).unwrap();
  queue.update_status(&record.id, TaskStatus::Completed, None).unwrap();
  let memory = engine.memory_coordinator();
  let snapshot = memory.snapshot().unwrap();
  let remaining = snapshot.policy.unwrap().emergency_reserve_bytes - snapshot.critical_reserved_bytes;
  let pressure = memory
    .reserve(MemoryOwner::DurabilityWaiters, remaining.saturating_sub(1), AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite))
    .unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  assert!(queue.prune_completed(0, 0).is_err());

  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(queue.list_tasks().unwrap().len(), 1);
  assert!(queue.get_task(&record.id).unwrap().is_some());

  drop(pressure);
  assert_eq!(queue.prune_completed(0, 0).unwrap(), 1);
  assert!(queue.list_tasks().unwrap().is_empty());
}

#[test]
fn task_pruning_bounds_each_locator_batch_and_leaves_registry_closure_complete() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let mut ids = Vec::new();
  for ordinal in 0..260 {
    let id = format!("legacy-{ordinal:04}");
    let record = TaskRecord {
      id: id.clone(),
      task_type: "cleanup".to_string(),
      args: serde_json::json!({"ordinal": ordinal}),
      status: TaskStatus::Completed,
      created_at: 1,
      started_at: Some(2),
      completed_at: Some(3),
      error: None,
      checkpoint: None,
      retry_at: None,
      deferral_count: 0,
    };
    let key = blake3::hash(format!("::aeordb:task:{id}").as_bytes()).as_bytes().to_vec();
    engine.store_entry(EntryType::FileRecord, &key, &serde_json::to_vec(&record).unwrap()).unwrap();
    ids.push(id);
  }
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  engine.store_entry(EntryType::FileRecord, &registry_key, &serde_json::to_vec(&ids).unwrap()).unwrap();
  let queue = TaskQueue::new(engine.clone());

  assert_eq!(queue.prune_completed(0, 0).unwrap(), 256);
  assert_eq!(queue.list_tasks().unwrap().len(), 4);
  assert_eq!(queue.prune_completed(0, 0).unwrap(), 4);
  assert!(queue.list_tasks().unwrap().is_empty());
}

#[test]
fn test_dequeue_returns_oldest_pending() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let r1 = queue.enqueue("reindex", serde_json::json!({"id": 1})).unwrap();
  thread::sleep(Duration::from_millis(5));
  let _r2 = queue.enqueue("reindex", serde_json::json!({"id": 2})).unwrap();
  thread::sleep(Duration::from_millis(5));
  let _r3 = queue.enqueue("reindex", serde_json::json!({"id": 3})).unwrap();

  let dequeued = queue.dequeue_next().unwrap().expect("should have a pending task");
  assert_eq!(dequeued.id, r1.id);
}

#[test]
fn test_update_status_persists() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&record.id, TaskStatus::Running, None).unwrap();

  let updated = queue.get_task(&record.id).unwrap().unwrap();
  assert_eq!(updated.status, TaskStatus::Running);
  assert!(updated.started_at.is_some());

  queue.update_status(&record.id, TaskStatus::Completed, None).unwrap();
  let completed = queue.get_task(&record.id).unwrap().unwrap();
  assert_eq!(completed.status, TaskStatus::Completed);
  assert!(completed.completed_at.is_some());
}

#[test]
fn test_task_survives_reload() {
  let (engine, _temp) = create_temp_engine_for_tests();

  let task_id;
  {
    let queue = TaskQueue::new(engine.clone());
    let record = queue.enqueue("reindex", serde_json::json!({"path": "/data"})).unwrap();
    task_id = record.id;
  }

  // Create a new TaskQueue from the same engine -- task should persist.
  let queue2 = TaskQueue::new(engine);
  let tasks = queue2.list_tasks().unwrap();
  assert_eq!(tasks.len(), 1);
  assert_eq!(tasks[0].id, task_id);
  assert_eq!(tasks[0].task_type, "reindex");
}

#[test]
fn test_cancel_sets_flag() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  assert!(!queue.is_cancelled(&record.id));

  queue.cancel(&record.id).unwrap();
  assert!(queue.is_cancelled(&record.id));

  let fetched = queue.get_task(&record.id).unwrap().unwrap();
  assert_eq!(fetched.status, TaskStatus::Cancelled);
}

#[test]
fn test_progress_tracking() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();

  let info = ProgressInfo {
    task_id: record.id.clone(),
    task_type: "reindex".to_string(),
    args: serde_json::json!({"path": "/docs"}),
    progress: 0.42,
    eta_ms: Some(5000),
    indexed_count: 42,
    total_count: 100,
    stale_since: None,
    message: Some("indexing /docs/sub".to_string()),
  };
  queue.set_progress(&record.id, info);

  let retrieved = queue.get_progress(&record.id).expect("progress should exist");
  assert_eq!(retrieved.task_id, record.id);
  assert_eq!(retrieved.task_type, "reindex");
  assert!((retrieved.progress - 0.42).abs() < f64::EPSILON);
  assert_eq!(retrieved.eta_ms, Some(5000));
  assert_eq!(retrieved.indexed_count, 42);
  assert_eq!(retrieved.total_count, 100);
  assert!(retrieved.stale_since.is_none());
  assert_eq!(retrieved.message, Some("indexing /docs/sub".to_string()));

  // Clear and verify gone.
  queue.clear_progress(&record.id);
  assert!(queue.get_progress(&record.id).is_none());
}

#[test]
fn test_prune_completed() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  // Enqueue and complete 5 tasks.
  let mut ids = Vec::new();
  for i in 0..5 {
    let record = queue.enqueue("reindex", serde_json::json!({"i": i})).unwrap();
    queue.update_status(&record.id, TaskStatus::Completed, None).unwrap();
    ids.push(record.id);
    thread::sleep(Duration::from_millis(5));
  }

  // Prune with max_count=2, very large max_age so age doesn't trigger.
  let pruned = queue.prune_completed(i64::MAX, 2).unwrap();
  assert_eq!(pruned, 3);

  let remaining = queue.list_tasks().unwrap();
  assert_eq!(remaining.len(), 2);

  // The 2 newest should remain (ids[3] and ids[4]).
  let remaining_ids: Vec<&str> = remaining.iter().map(|t| t.id.as_str()).collect();
  assert!(remaining_ids.contains(&ids[3].as_str()));
  assert!(remaining_ids.contains(&ids[4].as_str()));
}

#[test]
fn test_get_reindex_progress_for_path() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({"path": "/docs/"})).unwrap();
  let info = ProgressInfo {
    task_id: record.id.clone(),
    task_type: "reindex".to_string(),
    args: serde_json::json!({"path": "/docs/"}),
    progress: 0.5,
    eta_ms: None,
    indexed_count: 50,
    total_count: 100,
    stale_since: None,
    message: None,
  };
  queue.set_progress(&record.id, info);

  // Query with a sub-path -- should match because "/docs/sub/" starts with "/docs/".
  let found = queue.get_reindex_progress_for_path("/docs/sub/file.json");
  assert!(found.is_some());
  let found = found.unwrap();
  assert_eq!(found.task_id, record.id);

  // Query with an unrelated path -- should not match.
  let not_found = queue.get_reindex_progress_for_path("/other/path");
  assert!(not_found.is_none());
}

// -------------------------------------------------------------------------
// Edge-case and failure-path tests
// -------------------------------------------------------------------------

#[test]
fn test_dequeue_returns_none_when_empty() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let result = queue.dequeue_next().unwrap();
  assert!(result.is_none());
}

#[test]
fn test_dequeue_skips_non_pending() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let r1 = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&r1.id, TaskStatus::Running, None).unwrap();

  let r2 = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&r2.id, TaskStatus::Completed, None).unwrap();

  thread::sleep(Duration::from_millis(5));
  let r3 = queue.enqueue("reindex", serde_json::json!({})).unwrap();

  let dequeued = queue.dequeue_next().unwrap().expect("should find pending task");
  assert_eq!(dequeued.id, r3.id);
}

#[test]
fn test_get_task_returns_none_for_nonexistent() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let result = queue.get_task("nonexistent-id").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_update_status_errors_on_nonexistent() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let result = queue.update_status("nonexistent-id", TaskStatus::Running, None);
  assert!(result.is_err());
}

#[test]
fn test_update_checkpoint_persists() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_checkpoint(&record.id, "page:42").unwrap();

  let fetched = queue.get_task(&record.id).unwrap().unwrap();
  assert_eq!(fetched.checkpoint, Some("page:42".to_string()));
}

#[test]
fn requeue_running_preserves_checkpoint_and_clears_execution_state() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);
  let task = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  queue.dequeue_next().unwrap().expect("task must be claimed");
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();

  assert!(queue.requeue_running(&task.id).unwrap());

  let pending = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert!(pending.started_at.is_none());
  assert!(pending.completed_at.is_none());
  assert!(pending.error.is_none());
  assert_eq!(pending.checkpoint.as_deref(), Some("/docs/page-0042.json"));
}

#[test]
fn deferred_oldest_task_is_not_reclaimed_or_allowed_to_block_newer_eligible_work() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);
  let oldest = queue.enqueue("reindex", serde_json::json!({"path": "/large"})).unwrap();
  let newer = queue.enqueue("gc", serde_json::json!({"dry_run": true})).unwrap();

  let claimed = queue.dequeue_next().unwrap().expect("oldest task must be claimed first");
  assert_eq!(claimed.id, oldest.id);
  let retry_at = chrono::Utc::now().timestamp_millis() + 60_000;
  let deferred = queue.defer_running_until(&oldest.id, retry_at).unwrap().expect("running task must be deferred");
  assert_eq!(deferred.status, TaskStatus::Pending);
  assert_eq!(deferred.retry_at, Some(retry_at));
  assert_eq!(deferred.deferral_count, 1);

  let next = queue.dequeue_next().unwrap().expect("newer eligible task must bypass deferred work");
  assert_eq!(next.id, newer.id);
  assert!(queue.finish_running(&newer.id, TaskStatus::Completed, None).unwrap());
  assert!(queue.dequeue_next().unwrap().is_none(), "deferred task must not be rewritten before retry_at");

  let persisted = queue.get_task(&oldest.id).unwrap().unwrap();
  assert_eq!(persisted.status, TaskStatus::Pending);
  assert_eq!(persisted.retry_at, Some(retry_at));
  assert_eq!(persisted.deferral_count, 1);
}

#[test]
fn requeue_running_never_overwrites_a_concurrent_cancellation() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);
  let task = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  queue.dequeue_next().unwrap().expect("task must be claimed");
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();
  queue.cancel(&task.id).unwrap();

  assert!(!queue.requeue_running(&task.id).unwrap());

  let cancelled = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(cancelled.status, TaskStatus::Cancelled);
  assert!(cancelled.completed_at.is_some());
  assert_eq!(cancelled.checkpoint.as_deref(), Some("/docs/page-0042.json"));
}

#[test]
fn startup_recovery_requeues_interrupted_tasks_without_losing_checkpoints() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let task = queue.enqueue("reindex", serde_json::json!({"path": "/docs"})).unwrap();
  queue.dequeue_next().unwrap().expect("task must be claimed");
  queue.update_checkpoint(&task.id, "/docs/page-0042.json").unwrap();
  drop(queue);

  let restarted = TaskQueue::new(engine);
  assert_eq!(restarted.recover_interrupted_tasks().unwrap(), 1);
  assert_eq!(restarted.recover_interrupted_tasks().unwrap(), 0, "recovery must be idempotent");

  let pending = restarted.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert!(pending.started_at.is_none());
  assert!(pending.completed_at.is_none());
  assert!(pending.error.is_none());
  assert_eq!(pending.checkpoint.as_deref(), Some("/docs/page-0042.json"));
}

#[test]
fn worker_completion_never_overwrites_a_persisted_cancellation() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);
  let task = queue.enqueue("backup", serde_json::json!({})).unwrap();
  queue.dequeue_next().unwrap().expect("task must be claimed");
  queue.cancel(&task.id).unwrap();

  assert!(!queue.finish_running(&task.id, TaskStatus::Completed, None).unwrap());
  let cancelled = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(cancelled.status, TaskStatus::Cancelled);
  assert!(cancelled.error.is_none());
}

#[test]
fn independent_queue_wrappers_serialize_transitions_through_engine_authority() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let queue_a = std::sync::Arc::new(TaskQueue::new(engine.clone()));
  let queue_b = std::sync::Arc::new(TaskQueue::new(engine.clone()));
  let task = queue_a.enqueue("backup", serde_json::json!({})).unwrap();
  queue_a.dequeue_next().unwrap().expect("task must be claimed");
  let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

  std::thread::scope(|scope| {
    let barrier_a = barrier.clone();
    let queue_a = queue_a.clone();
    let task_id = task.id.clone();
    let checkpoint = scope.spawn(move || {
      barrier_a.wait();
      queue_a.update_checkpoint(&task_id, "archive:0042")
    });
    let barrier_b = barrier.clone();
    let queue_b = queue_b.clone();
    let task_id = task.id.clone();
    let cancellation = scope.spawn(move || {
      barrier_b.wait();
      queue_b.cancel(&task_id)
    });
    barrier.wait();
    checkpoint.join().unwrap().unwrap();
    cancellation.join().unwrap().unwrap();
  });

  let persisted = queue_a.get_task(&task.id).unwrap().unwrap();
  assert_eq!(persisted.status, TaskStatus::Cancelled);
  assert_eq!(persisted.checkpoint.as_deref(), Some("archive:0042"));
}

#[test]
fn startup_recovery_fails_closed_on_a_malformed_registry() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  engine.store_entry(aeordb::engine::EntryType::FileRecord, &registry_key, br#"{"broken":true}"#).unwrap();
  let queue = TaskQueue::new(engine);

  let error = queue.recover_interrupted_tasks().expect_err("malformed task authority must fail startup recovery");
  assert!(error.to_string().contains("deserialization error"));
}

#[test]
fn startup_recovery_fails_closed_when_registry_task_is_missing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let task = queue.enqueue("backup", serde_json::json!({})).unwrap();
  let task_key = blake3::hash(format!("::aeordb:task:{}", task.id).as_bytes()).as_bytes().to_vec();
  engine.mark_entry_deleted(&task_key).unwrap();

  let error = queue.recover_interrupted_tasks().expect_err("missing registry task must fail startup recovery");
  assert!(error.to_string().contains("references missing task"));
}

#[test]
fn startup_recovery_fails_closed_when_task_id_does_not_match_registry_key() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let task = queue.enqueue("backup", serde_json::json!({})).unwrap();
  let mut mismatched = queue.get_task(&task.id).unwrap().unwrap();
  mismatched.id = "different-task-id".to_string();
  let task_key = blake3::hash(format!("::aeordb:task:{}", task.id).as_bytes()).as_bytes().to_vec();
  engine.store_entry(aeordb::engine::EntryType::FileRecord, &task_key, &serde_json::to_vec(&mismatched).unwrap()).unwrap();

  let error = queue.recover_interrupted_tasks().expect_err("mismatched registry task must fail startup recovery");
  assert!(error.to_string().contains("does not match registry id"));
}

#[test]
fn startup_recovery_fails_closed_on_duplicate_registry_ids() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  let task = queue.enqueue("backup", serde_json::json!({})).unwrap();
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  let duplicate_registry = serde_json::to_vec(&vec![task.id.clone(), task.id]).unwrap();
  engine.store_entry(aeordb::engine::EntryType::FileRecord, &registry_key, &duplicate_registry).unwrap();

  let error = queue.recover_interrupted_tasks().expect_err("duplicate task authority must fail startup recovery");
  assert!(error.to_string().contains("duplicate id"));
}

#[test]
fn test_update_status_with_error() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&record.id, TaskStatus::Failed, Some("disk full".to_string())).unwrap();

  let fetched = queue.get_task(&record.id).unwrap().unwrap();
  assert_eq!(fetched.status, TaskStatus::Failed);
  assert_eq!(fetched.error, Some("disk full".to_string()));
  assert!(fetched.completed_at.is_some());
}

#[test]
fn test_prune_by_age() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let record = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&record.id, TaskStatus::Completed, None).unwrap();

  // Sleep briefly so the task is "old".
  thread::sleep(Duration::from_millis(50));

  // Prune with max_age_ms=10 (task completed >10ms ago), max_count very large.
  let pruned = queue.prune_completed(10, 1000).unwrap();
  assert_eq!(pruned, 1);

  let remaining = queue.list_tasks().unwrap();
  assert!(remaining.is_empty());
}

#[test]
fn test_prune_does_not_remove_active_tasks() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let pending = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  let running = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&running.id, TaskStatus::Running, None).unwrap();

  let completed = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.update_status(&completed.id, TaskStatus::Completed, None).unwrap();

  // Prune all terminal tasks.
  let pruned = queue.prune_completed(0, 0).unwrap();
  assert_eq!(pruned, 1); // only the completed one

  let remaining = queue.list_tasks().unwrap();
  assert_eq!(remaining.len(), 2);
  let remaining_ids: Vec<&str> = remaining.iter().map(|t| t.id.as_str()).collect();
  assert!(remaining_ids.contains(&pending.id.as_str()));
  assert!(remaining_ids.contains(&running.id.as_str()));
}

#[test]
fn test_cancel_nonexistent_task_errors() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let result = queue.cancel("nonexistent-id");
  assert!(result.is_err());
}

#[test]
fn test_task_record_serialization_roundtrip() {
  use aeordb::engine::task_queue::TaskRecord;

  let record = TaskRecord {
    id: "test-id".to_string(),
    task_type: "reindex".to_string(),
    args: serde_json::json!({"path": "/docs", "force": true}),
    status: TaskStatus::Running,
    created_at: 1000,
    started_at: Some(2000),
    completed_at: None,
    error: None,
    checkpoint: Some("page:5".to_string()),
    retry_at: Some(3000),
    deferral_count: 2,
  };

  let serialized = serde_json::to_vec(&record).unwrap();
  let deserialized: TaskRecord = serde_json::from_slice(&serialized).unwrap();

  assert_eq!(deserialized.id, record.id);
  assert_eq!(deserialized.task_type, record.task_type);
  assert_eq!(deserialized.args, record.args);
  assert_eq!(deserialized.status, record.status);
  assert_eq!(deserialized.created_at, record.created_at);
  assert_eq!(deserialized.started_at, record.started_at);
  assert_eq!(deserialized.completed_at, record.completed_at);
  assert_eq!(deserialized.error, record.error);
  assert_eq!(deserialized.checkpoint, record.checkpoint);
  assert_eq!(deserialized.retry_at, record.retry_at);
  assert_eq!(deserialized.deferral_count, record.deferral_count);
}

#[test]
fn legacy_task_record_without_retry_fields_is_immediately_eligible() {
  use aeordb::engine::task_queue::TaskRecord;

  let bytes = br#"{
    "id":"legacy-id",
    "task_type":"reindex",
    "args":{"path":"/docs"},
    "status":"pending",
    "created_at":1000,
    "started_at":null,
    "completed_at":null,
    "error":null,
    "checkpoint":null
  }"#;
  let record: TaskRecord = serde_json::from_slice(bytes).unwrap();
  assert_eq!(record.retry_at, None);
  assert_eq!(record.deferral_count, 0);
}

#[test]
fn test_status_serializes_lowercase() {
  let pending_json = serde_json::to_string(&TaskStatus::Pending).unwrap();
  assert_eq!(pending_json, "\"pending\"");

  let running_json = serde_json::to_string(&TaskStatus::Running).unwrap();
  assert_eq!(running_json, "\"running\"");

  let completed_json = serde_json::to_string(&TaskStatus::Completed).unwrap();
  assert_eq!(completed_json, "\"completed\"");

  let failed_json = serde_json::to_string(&TaskStatus::Failed).unwrap();
  assert_eq!(failed_json, "\"failed\"");

  let cancelled_json = serde_json::to_string(&TaskStatus::Cancelled).unwrap();
  assert_eq!(cancelled_json, "\"cancelled\"");
}

#[test]
fn test_list_tasks_returns_all() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  queue.enqueue("reindex", serde_json::json!({})).unwrap();
  queue.enqueue("backup", serde_json::json!({})).unwrap();
  queue.enqueue("cleanup", serde_json::json!({})).unwrap();

  let tasks = queue.list_tasks().unwrap();
  assert_eq!(tasks.len(), 3);

  let types: Vec<&str> = tasks.iter().map(|t| t.task_type.as_str()).collect();
  assert!(types.contains(&"reindex"));
  assert!(types.contains(&"backup"));
  assert!(types.contains(&"cleanup"));
}

#[test]
fn test_multiple_enqueue_unique_ids() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let r1 = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  let r2 = queue.enqueue("reindex", serde_json::json!({})).unwrap();
  let r3 = queue.enqueue("reindex", serde_json::json!({})).unwrap();

  assert_ne!(r1.id, r2.id);
  assert_ne!(r2.id, r3.id);
  assert_ne!(r1.id, r3.id);
}
