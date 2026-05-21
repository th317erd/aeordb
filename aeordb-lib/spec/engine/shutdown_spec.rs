use std::sync::Arc;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
use aeordb::engine::EventBus;
use aeordb::engine::heartbeat::spawn_heartbeat;
use tokio_util::sync::CancellationToken;

// =============================================================================
// Helper functions
// =============================================================================

/// Create a fresh engine in a temp directory.
fn create_engine(temp_dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let engine_file = temp_dir.path().join("test.aeordb");
    let engine_path = engine_file.to_str().unwrap();
    let engine = StorageEngine::create(engine_path).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

/// Reopen an existing engine file (simulates a restart after shutdown).
fn reopen_engine(temp_dir: &tempfile::TempDir) -> StorageEngine {
    let engine_file = temp_dir.path().join("test.aeordb");
    let engine_path = engine_file.to_str().unwrap();
    StorageEngine::open(engine_path).expect("reopen should work")
}

// =============================================================================
// Basic shutdown behavior
// =============================================================================

#[test]
fn test_engine_shutdown_empty_db() {
    // Shutdown on a freshly created engine should succeed without panic.
    let temp_dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&temp_dir);
    let result = engine.shutdown();
    assert!(result.is_ok(), "shutdown on empty DB should succeed");
}

#[test]
fn test_engine_shutdown_after_writes() {
    // Write data, then shutdown -- should not panic or error.
    let temp_dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&temp_dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    // Write several files to exercise the KV write buffer
    for i in 0..20 {
        let path = format!("/data/file-{}.txt", i);
        let content = format!("content for file {}", i);
        ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
    }

    let result = engine.shutdown();
    assert!(result.is_ok(), "shutdown after writes should succeed");
}

#[test]
fn test_engine_shutdown_data_durable() {
    // Write data, shutdown, reopen, and verify data is still present.
    let temp_dir = tempfile::tempdir().unwrap();
    {
        let engine = create_engine(&temp_dir);
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);

        ops.store_file_buffered(&ctx, "/important.txt", b"critical data", Some("text/plain")).unwrap();
        ops.store_file_buffered(&ctx, "/config/app.json", b"{\"key\": \"value\"}", Some("application/json")).unwrap();
        ops.store_file_buffered(&ctx, "/logs/event.log", b"event 1\nevent 2\n", Some("text/plain")).unwrap();

        engine.shutdown().expect("shutdown should succeed");
    }

    // Reopen and verify all data survived
    let engine = reopen_engine(&temp_dir);
    let ops = DirectoryOps::new(&engine);

    let data = ops.read_file_buffered("/important.txt").expect("file should exist after shutdown+reopen");
    assert_eq!(data, b"critical data");

    let data = ops.read_file_buffered("/config/app.json").expect("config should exist after shutdown+reopen");
    assert_eq!(data, b"{\"key\": \"value\"}");

    let data = ops.read_file_buffered("/logs/event.log").expect("log should exist after shutdown+reopen");
    assert_eq!(data, b"event 1\nevent 2\n");
}

#[test]
fn test_engine_shutdown_idempotent() {
    // Calling shutdown multiple times should not panic or corrupt state.
    let temp_dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&temp_dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

    assert!(engine.shutdown().is_ok());
    assert!(engine.shutdown().is_ok());
    assert!(engine.shutdown().is_ok());
}

#[test]
fn test_engine_shutdown_with_large_write_buffer() {
    // Write enough entries to fill the write buffer without triggering auto-flush,
    // then verify shutdown flushes everything.
    let temp_dir = tempfile::tempdir().unwrap();
    {
        let engine = create_engine(&temp_dir);
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);

        // Write many small files (the write buffer threshold is 512, so
        // write fewer than that to ensure some remain in buffer at shutdown time).
        for i in 0..100 {
            let path = format!("/batch/item-{:04}.txt", i);
            let content = format!("item number {}", i);
            ops.store_file_buffered(&ctx, &path, content.as_bytes(), Some("text/plain")).unwrap();
        }

        engine.shutdown().expect("shutdown should flush write buffer");
    }

    // Reopen and verify a sampling of entries
    let engine = reopen_engine(&temp_dir);
    let ops = DirectoryOps::new(&engine);

    let data = ops.read_file_buffered("/batch/item-0000.txt").expect("first item should exist");
    assert_eq!(data, b"item number 0");

    let data = ops.read_file_buffered("/batch/item-0050.txt").expect("middle item should exist");
    assert_eq!(data, b"item number 50");

    let data = ops.read_file_buffered("/batch/item-0099.txt").expect("last item should exist");
    assert_eq!(data, b"item number 99");
}

#[test]
fn test_engine_shutdown_preserves_stats() {
    // Verify stats are correct before shutdown.
    let temp_dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&temp_dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    ops.store_file_buffered(&ctx, "/a.txt", b"aaa", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/b.txt", b"bbb", Some("text/plain")).unwrap();

    let stats_before = engine.stats();
    assert!(stats_before.file_count >= 2, "should have at least 2 files before shutdown");

    engine.shutdown().expect("shutdown should succeed");

    // Stats should still be queryable after shutdown
    let stats_after = engine.stats();
    assert_eq!(stats_before.file_count, stats_after.file_count);
}

// =============================================================================
// CancellationToken behavior
// =============================================================================

#[test]
fn test_cancellation_token_not_cancelled_initially() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
}

#[test]
fn test_cancellation_token_cancel() {
    let token = CancellationToken::new();
    token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn test_cancellation_token_clone_propagates() {
    let token = CancellationToken::new();
    let clone1 = token.clone();
    let clone2 = token.clone();

    assert!(!clone1.is_cancelled());
    assert!(!clone2.is_cancelled());

    token.cancel();

    assert!(clone1.is_cancelled());
    assert!(clone2.is_cancelled());
}

#[tokio::test]
async fn test_cancellation_token_cancelled_future() {
    let token = CancellationToken::new();
    let clone = token.clone();

    // Cancel from another task
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        clone.cancel();
    });

    // This should complete within a short timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        token.cancelled(),
    ).await;

    assert!(result.is_ok(), "cancelled() future should resolve after cancel()");
}

#[test]
fn test_cancellation_token_child_does_not_propagate_up() {
    let parent = CancellationToken::new();
    let child = parent.child_token();

    child.cancel();

    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled(), "cancelling child should not cancel parent");
}

#[test]
fn test_cancellation_token_parent_propagates_to_child() {
    let parent = CancellationToken::new();
    let child = parent.child_token();

    parent.cancel();

    assert!(parent.is_cancelled());
    assert!(child.is_cancelled(), "cancelling parent should cancel child");
}

// =============================================================================
// Heartbeat cancellation
// =============================================================================

#[tokio::test]
async fn test_heartbeat_cancellation_stops_task() {
    let bus = Arc::new(EventBus::new());
    let cancel = CancellationToken::new();

    let handle = spawn_heartbeat(bus.clone(), 1, cancel.clone());

    // Cancel immediately
    cancel.cancel();

    // The task should exit within a reasonable timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle,
    ).await;

    assert!(result.is_ok(), "heartbeat should have exited after cancellation");
}

#[tokio::test]
async fn test_heartbeat_runs_until_cancelled() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let cancel = CancellationToken::new();

    let handle = spawn_heartbeat(bus.clone(), 1, cancel.clone());

    // Wait for at least one heartbeat event
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await;
    assert!(event.is_ok(), "should receive at least one heartbeat before cancellation");

    // Now cancel and verify the task exits
    cancel.cancel();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle,
    ).await;
    assert!(result.is_ok(), "heartbeat should exit after cancellation");
}

// =============================================================================
// Shutdown with hot directory (crash recovery journal)
// =============================================================================

#[test]
fn test_engine_shutdown_with_hot_dir() {
    // Verify shutdown works correctly when the hot directory (crash recovery
    // journal) is enabled.
    let temp_dir = tempfile::tempdir().unwrap();
    let hot_dir = temp_dir.path();

    let engine_file = temp_dir.path().join("hottest.aeordb");
    let engine_path = engine_file.to_str().unwrap();
    let engine = StorageEngine::create_with_hot_dir(engine_path, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.ensure_root_directory(&ctx).unwrap();

    // Write data
    ops.store_file_buffered(&ctx, "/hot-test.txt", b"hot data", Some("text/plain")).unwrap();

    // Shutdown should flush both KV buffer and hot file buffer
    let result = engine.shutdown();
    assert!(result.is_ok(), "shutdown with hot dir should succeed");

    // Reopen and verify
    drop(engine);
    let engine = StorageEngine::open_with_hot_dir(engine_path, Some(hot_dir)).unwrap();
    let ops = DirectoryOps::new(&engine);
    let data = ops.read_file_buffered("/hot-test.txt").expect("data should survive shutdown+reopen with hot dir");
    assert_eq!(data, b"hot data");
}

// =============================================================================
// Edge cases and failure modes
// =============================================================================

#[test]
fn test_engine_usable_after_shutdown() {
    // The engine should still be functional after shutdown (shutdown just
    // flushes buffers, it does not invalidate the engine).
    let temp_dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&temp_dir);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);

    ops.store_file_buffered(&ctx, "/before.txt", b"before shutdown", Some("text/plain")).unwrap();
    engine.shutdown().unwrap();

    // Write after shutdown should still work
    ops.store_file_buffered(&ctx, "/after.txt", b"after shutdown", Some("text/plain")).unwrap();
    let data = ops.read_file_buffered("/after.txt").unwrap();
    assert_eq!(data, b"after shutdown");
}

#[test]
fn test_engine_shutdown_then_reopen_write_read() {
    // Full cycle: create -> write -> shutdown -> reopen -> write more -> shutdown -> reopen -> verify all
    let temp_dir = tempfile::tempdir().unwrap();

    // Session 1: create and write
    {
        let engine = create_engine(&temp_dir);
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        ops.store_file_buffered(&ctx, "/session1.txt", b"from session 1", Some("text/plain")).unwrap();
        engine.shutdown().unwrap();
    }

    // Session 2: reopen and write more
    {
        let engine = reopen_engine(&temp_dir);
        let ctx = RequestContext::system();
        let ops = DirectoryOps::new(&engine);
        ops.store_file_buffered(&ctx, "/session2.txt", b"from session 2", Some("text/plain")).unwrap();
        engine.shutdown().unwrap();
    }

    // Session 3: verify both sessions' data
    {
        let engine = reopen_engine(&temp_dir);
        let ops = DirectoryOps::new(&engine);

        let data = ops.read_file_buffered("/session1.txt").expect("session 1 data should persist");
        assert_eq!(data, b"from session 1");

        let data = ops.read_file_buffered("/session2.txt").expect("session 2 data should persist");
        assert_eq!(data, b"from session 2");
    }
}

#[test]
fn test_engine_shutdown_return_value() {
    // Verify the return type is EngineResult<()> and it returns Ok on success
    let temp_dir = tempfile::tempdir().unwrap();
    let engine = create_engine(&temp_dir);
    let result: aeordb::engine::errors::EngineResult<()> = engine.shutdown();
    assert!(result.is_ok());
}
