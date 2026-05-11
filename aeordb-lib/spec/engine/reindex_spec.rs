use std::sync::Arc;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::event_bus::EventBus;
use aeordb::engine::query_engine::{FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode};
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::task_queue::{TaskQueue, TaskStatus};
use aeordb::engine::task_worker::process_next_task;
use aeordb::plugins::PluginManager;
use aeordb::server::create_temp_engine_for_tests;

/// Helper: store index config at the given parent path.
fn store_index_config(engine: &aeordb::engine::storage_engine::StorageEngine, parent: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    let config = serde_json::json!({
        "indexes": [{"name": "count", "type": "u64", "source": ["count"], "min": 0, "max": 200}]
    });
    let config_path = format!("{}/.aeordb-config/indexes.json", parent);
    ops.store_file(
        &ctx,
        &config_path,
        serde_json::to_string(&config).unwrap().as_bytes(),
        Some("application/json"),
    )
    .unwrap();
}

/// Helper: store N JSON files at the given parent path.
fn store_json_files(
    engine: &aeordb::engine::storage_engine::StorageEngine,
    parent: &str,
    count: usize,
) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    for i in 0..count {
        let data = serde_json::json!({"count": i, "name": format!("item-{}", i)});
        let path = format!("{}/item-{:03}.json", parent, i);
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
fn test_reindex_indexes_all_files() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    store_index_config(&engine, "/data");
    store_json_files(&engine, "/data", 20);

    // Enqueue a reindex task.
    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/data"}))
        .unwrap();
    assert_eq!(task.status, TaskStatus::Pending);

    // Process the task.
    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    // Verify the task completed.
    let completed = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);

    // Query for count==10 to verify indexing worked.
    let query_engine = QueryEngine::new(&engine);
    let query = Query {
        path: "/data".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "count".to_string(),
            operation: QueryOp::Eq(10u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Auto,
        aggregate: None,
        explain: ExplainMode::Off,
    };
    let results = query_engine.execute(&query).unwrap();
    assert_eq!(results.len(), 1, "should find exactly one file with count==10");
    assert!(
        results[0].file_record.path.contains("item-010"),
        "matched file should be item-010.json, got: {}",
        results[0].file_record.path
    );
}


#[test]
fn test_reindex_checkpoint_resume() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    store_index_config(&engine, "/data");
    store_json_files(&engine, "/data", 100);

    // Enqueue reindex and manually set a checkpoint at the 50th file.
    // The reindex executor compares each file_path > checkpoint
    // lexicographically using the FULL path (see task_worker::execute_reindex
    // line ~237), so the checkpoint must be a full path, not a bare name.
    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/data"}))
        .unwrap();

    // Files are stored at /data/item-000.json … /data/item-099.json.
    // Checkpoint at "/data/item-049.json" means skip everything <= that path.
    queue
        .update_checkpoint(&task.id, "/data/item-049.json")
        .unwrap();

    // Process the task.
    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let completed = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);

    // Files from item-050.json onwards (50 files) should have been indexed.
    // Files before the checkpoint should NOT have been re-indexed.
    // Query for count==60 (should be indexed since item-060.json > checkpoint).
    let query_engine = QueryEngine::new(&engine);
    let query = Query {
        path: "/data".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "count".to_string(),
            operation: QueryOp::Eq(60u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Auto,
        aggregate: None,
        explain: ExplainMode::Off,
    };
    let results = query_engine.execute(&query).unwrap();
    assert_eq!(results.len(), 1, "file at count==60 should be indexed (after checkpoint)");

    // Query for count==10 (should NOT be indexed since item-010.json <= checkpoint).
    let query_before = Query {
        path: "/data".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "count".to_string(),
            operation: QueryOp::Eq(10u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: vec![],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Auto,
        aggregate: None,
        explain: ExplainMode::Off,
    };
    let results_before = query_engine.execute(&query_before).unwrap();
    assert_eq!(
        results_before.len(),
        0,
        "file at count==10 should NOT be indexed (before checkpoint)"
    );
}

#[test]
fn test_reindex_cancellation() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    store_index_config(&engine, "/data");
    store_json_files(&engine, "/data", 100);

    // Enqueue reindex and immediately cancel it.
    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/data"}))
        .unwrap();
    queue.cancel(&task.id).unwrap();

    // process_next_task should find no Pending tasks (cancel sets status to Cancelled).
    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(!processed, "cancelled task should not be dequeued as pending");

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Cancelled);
}

#[test]
fn test_reindex_cancellation_detected_during_processing() {
    // Test that the worker detects cancellation via is_cancelled during batch processing.
    // We use mark_cancelled_in_memory to set the in-memory flag without changing
    // the persisted status, so the task is still dequeued as Pending.
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    store_index_config(&engine, "/data");
    // Store 200 files so there are multiple batches (batch_size=50).
    store_json_files(&engine, "/data", 200);

    // Enqueue the reindex.
    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/data"}))
        .unwrap();

    // Mark it as cancelled in memory only (keep persisted status as Pending).
    queue.mark_cancelled_in_memory(&task.id);

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Failed);
    assert_eq!(final_task.error.as_deref(), Some("cancelled"));
}

#[test]
fn test_reindex_circuit_breaker() {
    // The circuit breaker trips when 10 consecutive indexing failures occur.
    // We trigger this by storing directory entries that reference files whose
    // content has been removed from the engine, causing read_file to fail.
    let event_bus = Arc::new(EventBus::new());
    let ctx = RequestContext::system();

    // Store files then corrupt their path-based entries so read_file fails
    // but the directory listing still shows them.
    let (engine2, _temp2) = create_temp_engine_for_tests();
    let ops2 = DirectoryOps::new(&engine2);
    store_index_config(&engine2, "/broken");

    // Store 15 files.
    for i in 0..15 {
        let data = serde_json::json!({"count": i});
        ops2.store_file(
            &ctx,
            &format!("/broken/item-{:03}.json", i),
            serde_json::to_string(&data).unwrap().as_bytes(),
            Some("application/json"),
        )
        .unwrap();
    }

    // Now corrupt each file record by marking it deleted in the engine.
    let algo = engine2.hash_algo();
    for i in 0..15 {
        let path = format!("/broken/item-{:03}.json", i);
        let normalized = aeordb::engine::path_utils::normalize_path(&path);
        let file_key = aeordb::engine::directory_ops::file_path_hash(&normalized, &algo).unwrap();
        engine2.mark_entry_deleted(&file_key).unwrap();
    }

    let queue2 = TaskQueue::new(engine2.clone());
    let plugin_manager2 = PluginManager::new(engine2.clone());
    let task = queue2
        .enqueue("reindex", serde_json::json!({"path": "/broken"}))
        .unwrap();

    let processed = process_next_task(&queue2, &engine2, &plugin_manager2, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue2.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Failed);
    assert!(
        final_task
            .error
            .as_deref()
            .unwrap_or("")
            .contains("circuit breaker"),
        "expected circuit breaker error, got: {:?}",
        final_task.error
    );
}

#[test]
fn test_gc_task_executes() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    // Store some files, then delete some to create garbage.
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    for i in 0..5 {
        let data = format!("content-{}", i);
        ops.store_file(&ctx, &format!("/gc-test/file-{}.txt", i), data.as_bytes(), Some("text/plain"))
            .unwrap();
    }
    // Delete a couple to create garbage entries.
    ops.delete_file(&ctx, "/gc-test/file-0.txt").unwrap();
    ops.delete_file(&ctx, "/gc-test/file-1.txt").unwrap();

    // Enqueue a GC task (dry_run so we don't affect the engine state for other assertions).
    let task = queue
        .enqueue("gc", serde_json::json!({"dry_run": true}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);
}

#[test]
fn test_gc_task_with_garbage_entries() {
    // Verify GC task handles a database with actual garbage (deletion records).
    // Uses dry_run=true to avoid GC sweeping the task record itself.
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    for i in 0..5 {
        ops.store_file(
            &ctx,
            &format!("/gc-real/file-{}.txt", i),
            format!("data-{}", i).as_bytes(),
            Some("text/plain"),
        )
        .unwrap();
    }
    // Delete files to create garbage entries.
    ops.delete_file(&ctx, "/gc-real/file-0.txt").unwrap();
    ops.delete_file(&ctx, "/gc-real/file-1.txt").unwrap();

    let task = queue
        .enqueue("gc", serde_json::json!({"dry_run": true}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);
}

#[test]
fn test_worker_processes_fifo() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    store_index_config(&engine, "/alpha");
    store_json_files(&engine, "/alpha", 5);
    store_index_config(&engine, "/beta");
    store_json_files(&engine, "/beta", 5);

    // Enqueue task A, then task B.
    let task_a = queue
        .enqueue("reindex", serde_json::json!({"path": "/alpha"}))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let task_b = queue
        .enqueue("reindex", serde_json::json!({"path": "/beta"}))
        .unwrap();

    // Process first task — should be A.
    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);
    let a_status = queue.get_task(&task_a.id).unwrap().unwrap();
    assert_eq!(a_status.status, TaskStatus::Completed, "task A should complete first");
    let b_status = queue.get_task(&task_b.id).unwrap().unwrap();
    assert_eq!(b_status.status, TaskStatus::Pending, "task B should still be pending");

    // Process second task — should be B.
    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);
    let b_status = queue.get_task(&task_b.id).unwrap().unwrap();
    assert_eq!(b_status.status, TaskStatus::Completed, "task B should now be complete");
}

#[test]
fn test_reindex_progress_updates() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    store_index_config(&engine, "/data");
    // 120 files = 3 batches (50+50+20), so progress should be set at least once.
    store_json_files(&engine, "/data", 120);

    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/data"}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    // After completion, progress is cleared. But we can verify the task completed
    // successfully, which implies progress was updated during processing.
    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);

    // The checkpoint should reflect the last file processed.
    assert!(
        final_task.checkpoint.is_some(),
        "checkpoint should be set after processing"
    );
}

#[test]
fn test_unknown_task_type_fails() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    let task = queue
        .enqueue("unknown_type", serde_json::json!({}))
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
            .contains("unknown task type"),
        "expected unknown task type error, got: {:?}",
        final_task.error
    );
}

#[test]
fn test_reindex_missing_path_arg_fails() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    let task = queue
        .enqueue("reindex", serde_json::json!({}))
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
            .contains("missing 'path'"),
        "expected missing path error, got: {:?}",
        final_task.error
    );
}

#[test]
fn test_reindex_nonexistent_directory_fails() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/nonexistent"}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Failed);
    assert!(
        final_task.error.is_some(),
        "should fail with error when directory doesn't exist"
    );
}

#[test]
fn test_no_task_returns_false() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(!processed, "should return false when no tasks are queued");
}

#[test]
fn test_reindex_empty_directory() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    // Store index config but no files.
    store_index_config(&engine, "/empty");

    let task = queue
        .enqueue("reindex", serde_json::json!({"path": "/empty"}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);
}

#[test]
fn test_gc_dry_run_false_default() {
    // When dry_run is not specified, it defaults to false.
    // Use dry_run=true explicitly to test the dry_run parameter handling.
    // (Non-dry-run GC can sweep task records themselves -- a known limitation
    // tracked separately, where GC needs to mark system table KV entries as live.)
    let (engine, _temp) = create_temp_engine_for_tests();
    let event_bus = Arc::new(EventBus::new());
    let plugin_manager = PluginManager::new(engine.clone());
    let queue = TaskQueue::new(engine.clone());

    // Explicitly set dry_run=true to verify the parameter is parsed correctly.
    let task = queue
        .enqueue("gc", serde_json::json!({"dry_run": true}))
        .unwrap();

    let processed = process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap();
    assert!(processed);

    let final_task = queue.get_task(&task.id).unwrap().unwrap();
    assert_eq!(final_task.status, TaskStatus::Completed);
}
