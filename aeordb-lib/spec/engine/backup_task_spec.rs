use std::sync::Arc;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::event_bus::EventBus;
use aeordb::engine::engine_event::{EVENT_TASKS_CANCELLED, EVENT_TASKS_STARTED};
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryOwner};
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::task_queue::{TaskQueue, TaskStatus};
use aeordb::engine::task_worker::{process_next_task, process_next_task_with_pre_execute_hook};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{backup, EngineError};
use aeordb::plugins::PluginManager;
use aeordb::server::create_temp_engine_for_tests;

/// Helper: store a few files so the backup has something to export.
fn populate_engine(engine: &aeordb::engine::storage_engine::StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  for i in 0..5 {
    let data = serde_json::json!({"id": i, "value": format!("item-{}", i)});
    let path = format!("/data/item-{}.json", i);
    ops.store_file_buffered(&ctx, &path, serde_json::to_string(&data).unwrap().as_bytes(), Some("application/json")).unwrap();
  }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn test_backup_task_creates_export_file() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  let backup_dir = _temp.path().join("backups");
  let backup_dir_string = backup_dir.to_string_lossy().to_string();

  let task = queue.enqueue("backup", serde_json::json!({"backup_dir": backup_dir_string})).unwrap();
  assert_eq!(task.status, TaskStatus::Pending);

  let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
  assert!(processed);

  // Task should have completed.
  let finished = queue.get_task(&task.id).unwrap().expect("task should exist");
  assert_eq!(finished.status, TaskStatus::Completed);

  // At least one .aeordb file should exist in the backup directory.
  let files: Vec<_> = std::fs::read_dir(&backup_dir)
    .unwrap()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("aeordb"))
    .collect();
  assert_eq!(files.len(), 1, "expected exactly one backup file");

  // The file should contain data (not be empty).
  let file_size = files[0].metadata().unwrap().len();
  assert!(file_size > 0, "backup file should not be empty");
}

#[test]
fn test_backup_task_with_snapshot() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  // Create a snapshot to back up.
  let ctx = RequestContext::system();
  let version_manager = VersionManager::new(&engine);
  version_manager.create_snapshot(&ctx, "snap-for-backup", std::collections::HashMap::new()).unwrap();

  let backup_dir = _temp.path().join("snap-backups");
  let backup_dir_string = backup_dir.to_string_lossy().to_string();

  let task = queue
    .enqueue(
      "backup",
      serde_json::json!({
          "backup_dir": backup_dir_string,
          "snapshot": "snap-for-backup"
      }),
    )
    .unwrap();

  process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();

  let finished = queue.get_task(&task.id).unwrap().expect("task should exist");
  assert_eq!(finished.status, TaskStatus::Completed);

  let files: Vec<_> = std::fs::read_dir(&backup_dir)
    .unwrap()
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("aeordb"))
    .collect();
  assert_eq!(files.len(), 1);

  // Filename should contain the snapshot name.
  let filename = files[0].file_name().to_string_lossy().to_string();
  assert!(filename.contains("snap-for-backup"), "filename should contain snapshot name, got: {}", filename,);
}

#[test]
fn test_backup_task_default_backup_dir() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  // Use default backup_dir (./backups/)
  let task = queue.enqueue("backup", serde_json::json!({})).unwrap();

  process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();

  let finished = queue.get_task(&task.id).unwrap().expect("task should exist");
  assert_eq!(finished.status, TaskStatus::Completed);

  // Clean up the default directory.
  let _ = std::fs::remove_dir_all("./backups/");
}

#[test]
fn backup_task_rejects_malformed_retention_before_creating_an_artifact() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());
  let backup_dir = _temp.path().join("malformed-retention");
  let task = queue
    .enqueue(
      "backup",
      serde_json::json!({
        "backup_dir": backup_dir.to_string_lossy(),
        "retention_count": "2"
      }),
    )
    .unwrap();

  assert!(process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  let finished = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(finished.status, TaskStatus::Failed);
  assert!(finished.error.as_deref().is_some_and(|error| error.contains("retention_count") && error.contains("integer")));
  assert!(!backup_dir.exists(), "malformed task arguments must be rejected before the backup destination is created");
}

#[cfg(unix)]
#[test]
fn backup_retention_ignores_non_file_aeordb_entries_instead_of_assigning_them_epoch_age() {
  use std::os::unix::fs::symlink;

  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());
  let backup_dir = _temp.path().join("retention-non-file");
  std::fs::create_dir_all(&backup_dir).unwrap();
  let broken_link = backup_dir.join("foreign.aeordb");
  symlink(backup_dir.join("missing-target"), &broken_link).unwrap();

  let task = queue
    .enqueue(
      "backup",
      serde_json::json!({
        "backup_dir": backup_dir.to_string_lossy(),
        "retention_count": 1
      }),
    )
    .unwrap();

  assert!(process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());
  assert_eq!(queue.get_task(&task.id).unwrap().unwrap().status, TaskStatus::Completed);
  assert!(broken_link.symlink_metadata().is_ok(), "retention must not delete a non-file entry merely because its name ends in .aeordb");
}

// ---------------------------------------------------------------------------
// Retention enforcement
// ---------------------------------------------------------------------------

#[test]
fn test_backup_retention_removes_oldest_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  let backup_dir = _temp.path().join("retention-test");
  let backup_dir_string = backup_dir.to_string_lossy().to_string();

  // Create 3 backups then run a 4th with retention_count=2.
  for _ in 0..3 {
    let task = queue.enqueue("backup", serde_json::json!({"backup_dir": backup_dir_string})).unwrap();
    process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    let finished = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(finished.status, TaskStatus::Completed);
    // Small delay so timestamps differ.
    std::thread::sleep(std::time::Duration::from_millis(50));
  }

  // Verify we have 3 files.
  let count_before: usize = std::fs::read_dir(&backup_dir)
    .unwrap()
    .filter_map(|e| e.ok())
    .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("aeordb"))
    .count();
  assert_eq!(count_before, 3);

  // Run a 4th backup with retention=2.
  let task = queue
    .enqueue(
      "backup",
      serde_json::json!({
          "backup_dir": backup_dir_string,
          "retention_count": 2
      }),
    )
    .unwrap();
  process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();

  let finished = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(finished.status, TaskStatus::Completed);

  // Should now have exactly 2 files.
  let count_after: usize = std::fs::read_dir(&backup_dir)
    .unwrap()
    .filter_map(|e| e.ok())
    .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("aeordb"))
    .count();
  assert_eq!(count_after, 2, "retention should keep only 2 backups, found {}", count_after,);
}

#[test]
fn test_backup_retention_zero_keeps_all() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  let backup_dir = _temp.path().join("no-retention");
  let backup_dir_string = backup_dir.to_string_lossy().to_string();

  // Create 3 backups with retention_count=0 (unlimited).
  for _ in 0..3 {
    let task = queue
      .enqueue(
        "backup",
        serde_json::json!({
            "backup_dir": backup_dir_string,
            "retention_count": 0
        }),
      )
      .unwrap();
    process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    let finished = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(finished.status, TaskStatus::Completed);
    std::thread::sleep(std::time::Duration::from_millis(50));
  }

  // All 3 should remain.
  let count: usize = std::fs::read_dir(&backup_dir)
    .unwrap()
    .filter_map(|e| e.ok())
    .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("aeordb"))
    .count();
  assert_eq!(count, 3, "retention_count=0 should keep all backups");
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn test_backup_task_invalid_snapshot_fails() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  let backup_dir = _temp.path().join("fail-backups");
  let backup_dir_string = backup_dir.to_string_lossy().to_string();

  let task = queue
    .enqueue(
      "backup",
      serde_json::json!({
          "backup_dir": backup_dir_string,
          "snapshot": "nonexistent-snapshot"
      }),
    )
    .unwrap();

  process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();

  let finished = queue.get_task(&task.id).unwrap().expect("task should exist");
  assert_eq!(finished.status, TaskStatus::Failed);
  assert!(finished.error.is_some(), "failed task should have an error message");
}

#[test]
fn test_backup_task_unwritable_dir_fails() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  populate_engine(&engine);

  // Use /proc/fake as an unwritable directory.
  let task = queue.enqueue("backup", serde_json::json!({"backup_dir": "/proc/fake/deeply/nested/backup"})).unwrap();

  process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();

  let finished = queue.get_task(&task.id).unwrap().expect("task should exist");
  assert_eq!(finished.status, TaskStatus::Failed);
  assert!(finished.error.is_some());
}

#[test]
fn test_backup_task_empty_engine_succeeds() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());

  // Don't populate -- export an empty engine.
  let backup_dir = _temp.path().join("empty-backups");
  let backup_dir_string = backup_dir.to_string_lossy().to_string();

  let task = queue.enqueue("backup", serde_json::json!({"backup_dir": backup_dir_string})).unwrap();

  process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();

  let finished = queue.get_task(&task.id).unwrap().expect("task should exist");
  assert_eq!(finished.status, TaskStatus::Completed);
}

#[test]
fn active_backup_defers_on_new_host_pressure_and_removes_partial_artifacts() {
  let (engine, temp) = create_temp_engine_for_tests();
  populate_engine(&engine);
  let output = temp.path().join("pressure-backup.aeordb");
  let output_string = output.to_string_lossy().into_owned();
  let coordinator = engine.memory_coordinator().clone();
  let soft_limit = coordinator.snapshot().unwrap().policy.unwrap().soft_limit_bytes;

  let error = backup::export_snapshot_with_post_admission_hook(&engine, None, &output_string, false, move || {
    coordinator
      .update_host_sample(HostMemorySample { rss_bytes: soft_limit, host_available_bytes: Some(u64::MAX), ..HostMemorySample::default() })
      .unwrap();
  })
  .expect_err("an active backup must defer when host pressure rises");
  assert!(matches!(error, EngineError::ResourceExhausted(_)));
  assert!(!output.exists(), "a deferred backup must never publish its destination");
  let artifacts = std::fs::read_dir(temp.path())
    .unwrap()
    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
    .filter(|name| name.starts_with("pressure-backup.aeordb.part-"))
    .collect::<Vec<_>>();
  assert!(artifacts.is_empty(), "deferred backup left partial artifacts: {artifacts:?}");

  let snapshot = engine.memory_coordinator().snapshot().unwrap();
  let owner = snapshot.owner(MemoryOwner::BackupRestore).unwrap();
  assert!(owner.deferrals >= 1);
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn backup_task_defers_before_creating_destination_under_new_host_pressure() {
  let (engine, temp) = create_temp_engine_for_tests();
  let event_bus = Arc::new(EventBus::new());
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());
  let backup_dir = temp.path().join("deferred-task-backups");
  let task = queue.enqueue("backup", serde_json::json!({"backup_dir": backup_dir.to_string_lossy()})).unwrap();
  let coordinator = engine.memory_coordinator().clone();
  let soft_limit = coordinator.snapshot().unwrap().policy.unwrap().soft_limit_bytes;

  assert!(process_next_task_with_pre_execute_hook(&queue, &engine, &plugin_manager, &event_bus, move || {
    coordinator
      .update_host_sample(HostMemorySample { rss_bytes: soft_limit, host_available_bytes: Some(u64::MAX), ..HostMemorySample::default() })
      .unwrap();
  })
  .unwrap());

  let pending = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(pending.status, TaskStatus::Pending);
  assert!(!backup_dir.exists(), "deferred backup must not create its destination directory");
}

#[test]
fn running_backup_cancellation_reaches_export_and_publishes_no_artifact() {
  let (engine, temp) = create_temp_engine_for_tests();
  populate_engine(&engine);
  let event_bus = Arc::new(EventBus::new());
  let mut events = event_bus.subscribe();
  let plugin_manager = PluginManager::new(engine.clone());
  let queue = TaskQueue::new(engine.clone());
  let backup_dir = temp.path().join("cancelled-backup");
  let task = queue.enqueue("backup", serde_json::json!({"backup_dir": backup_dir.to_string_lossy()})).unwrap();

  assert!(
    process_next_task_with_pre_execute_hook(&queue, &engine, &plugin_manager, &event_bus, || queue.cancel(&task.id).unwrap()).unwrap()
  );
  let cancelled = queue.get_task(&task.id).unwrap().unwrap();
  assert_eq!(cancelled.status, TaskStatus::Cancelled);
  assert!(cancelled.error.is_none());
  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_STARTED);
  assert_eq!(events.try_recv().unwrap().event_type, EVENT_TASKS_CANCELLED);
  assert!(events.try_recv().is_err());
  let published = std::fs::read_dir(&backup_dir).map(|entries| entries.count()).unwrap_or(0);
  assert_eq!(published, 0, "a cancelled running backup must not publish any artifact");
}
