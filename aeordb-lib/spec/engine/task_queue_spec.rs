use std::thread;
use std::time::Duration;

use aeordb::engine::task_queue::{TaskQueue, TaskStatus, ProgressInfo};
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
