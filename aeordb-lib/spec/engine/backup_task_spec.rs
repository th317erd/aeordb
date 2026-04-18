use std::sync::Arc;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::event_bus::EventBus;
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::task_queue::{TaskQueue, TaskStatus};
use aeordb::engine::task_worker::process_next_task;
use aeordb::plugins::PluginManager;
use aeordb::server::create_temp_engine_for_tests;

/// Helper: store some files so the engine has a non-empty HEAD to export.
fn populate_engine(engine: &aeordb::engine::storage_engine::StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    for i in 0..5 {
        let data = serde_json::json!({"index": i, "name": format!("item-{}", i)});
        let path = format!("/data/item-{:03}.json", i);
        ops.store_file(
            &ctx,
            &path,
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }
}

#[test]
fn test_backup_task_creates_file() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({"backup_dir": backup_dir_path}),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    let backup_files: Vec<_> = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .collect();
    assert_eq!(backup_files.len(), 1, "expected exactly one backup file");

    let filename = backup_files[0].file_name().to_string_lossy().to_string();
    assert!(
        filename.starts_with("backup_") && filename.ends_with(".aeordb"),
        "filename should match backup_YYYYMMDD_HHMMSS.aeordb pattern, got: {}",
        filename
    );

    let metadata = std::fs::metadata(backup_files[0].path()).unwrap();
    assert!(metadata.len() > 0, "backup file should not be empty");
}

#[test]
fn test_backup_task_default_directory() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let task = queue
        .enqueue("backup", serde_json::json!({}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    let _ = std::fs::remove_dir_all("./backups");
}

#[test]
fn test_backup_task_retention_removes_old_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    for i in 0..4 {
        let fake_name = format!("backup_2024010{}_120000.aeordb", i + 1);
        let fake_path = backup_dir.path().join(&fake_name);
        std::fs::write(&fake_path, b"fake backup data").unwrap();
    }

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({
                "backup_dir": backup_dir_path,
                "retention_count": 2
            }),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    let remaining: Vec<_> = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .collect();

    assert_eq!(
        remaining.len(),
        2,
        "should keep exactly 2 backups (retention_count=2), found {}",
        remaining.len()
    );
}

#[test]
fn test_backup_task_retention_zero_keeps_all() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    for i in 0..3 {
        let fake_name = format!("backup_2024010{}_120000.aeordb", i + 1);
        std::fs::write(backup_dir.path().join(&fake_name), b"fake").unwrap();
    }

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({
                "backup_dir": backup_dir_path,
                "retention_count": 0
            }),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    let remaining: Vec<_> = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .collect();

    assert_eq!(
        remaining.len(),
        4,
        "retention_count=0 should keep all backups, found {}",
        remaining.len()
    );
}

#[test]
fn test_backup_task_invalid_directory_fails() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let invalid_dir = format!("{}/subdir", temp_file.path().to_string_lossy());

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({"backup_dir": invalid_dir}),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Failed);
    assert!(
        final_task
            .error
            .as_deref()
            .unwrap_or("")
            .contains("failed to create backup directory"),
        "expected directory creation error, got: {:?}",
        final_task.error
    );
}

#[test]
fn test_backup_task_invalid_snapshot_fails() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({
                "backup_dir": backup_dir_path,
                "snapshot": "nonexistent-snapshot"
            }),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Failed);
    assert!(
        final_task
            .error
            .as_deref()
            .unwrap_or("")
            .contains("backup export failed"),
        "expected export failure for bad snapshot, got: {:?}",
        final_task.error
    );
}

#[test]
fn test_backup_task_empty_engine() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({"backup_dir": backup_dir_path}),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    // Count only .aeordb backup files (engine may create WAL files alongside).
    let backup_files: Vec<_> = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .collect();
    assert_eq!(backup_files.len(), 1);
}

#[test]
fn test_backup_task_retention_ignores_non_backup_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    std::fs::write(backup_dir.path().join("readme.txt"), b"do not delete").unwrap();
    std::fs::write(backup_dir.path().join("config.json"), b"{}").unwrap();
    std::fs::write(backup_dir.path().join("other.aeordb"), b"not a backup_").unwrap();

    for i in 0..3 {
        let fake_name = format!("backup_2024010{}_120000.aeordb", i + 1);
        std::fs::write(backup_dir.path().join(&fake_name), b"fake").unwrap();
    }

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({
                "backup_dir": backup_dir_path,
                "retention_count": 1
            }),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    // Verify exactly 1 backup file remains (retention_count=1).
    let backup_files: Vec<_> = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .collect();

    assert_eq!(
        backup_files.len(),
        1,
        "should have exactly 1 retained backup, got {}",
        backup_files.len()
    );

    // Non-backup files should all still exist.
    assert!(backup_dir.path().join("readme.txt").exists());
    assert!(backup_dir.path().join("config.json").exists());
    assert!(backup_dir.path().join("other.aeordb").exists());
}

#[test]
fn test_backup_file_is_valid_aeordb() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({"backup_dir": backup_dir_path}),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    let backup_file = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .expect("backup file should exist");

    let backup_path = backup_file.path().to_string_lossy().to_string();
    let backup_engine = aeordb::engine::storage_engine::StorageEngine::open_for_import(&backup_path);
    assert!(
        backup_engine.is_ok(),
        "backup file should be openable as a valid .aeordb, got: {:?}",
        backup_engine.err()
    );
}

#[test]
fn test_backup_task_retention_one_keeps_newest() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    populate_engine(&engine);

    let backup_dir = tempfile::tempdir().unwrap();
    let backup_dir_path = backup_dir.path().to_string_lossy().to_string();

    let old_name = "backup_20240101_120000.aeordb";
    std::fs::write(backup_dir.path().join(old_name), b"old data").unwrap();

    let task = queue
        .enqueue(
            "backup",
            serde_json::json!({
                "backup_dir": backup_dir_path,
                "retention_count": 1
            }),
        )
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    let remaining: Vec<_> = std::fs::read_dir(&backup_dir_path)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("backup_") && name.ends_with(".aeordb")
        })
        .collect();

    assert_eq!(remaining.len(), 1);
    let surviving_name = remaining[0].file_name().to_string_lossy().to_string();
    assert_ne!(
        surviving_name, old_name,
        "the old backup should have been removed"
    );
}
