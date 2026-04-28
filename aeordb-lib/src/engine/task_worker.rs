use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep;

use crate::engine::backup;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::engine_event::{
    EngineEvent, EVENT_TASKS_COMPLETED, EVENT_TASKS_FAILED, EVENT_TASKS_STARTED,
};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::event_bus::EventBus;
use crate::engine::gc::run_gc;
use crate::engine::indexing_pipeline::IndexingPipeline;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::task_queue::{ProgressInfo, TaskQueue, TaskRecord, TaskStatus};
use crate::plugins::PluginManager;

/// Maximum completed tasks to keep after pruning.
const PRUNE_MAX_COUNT: usize = 100;
/// Maximum age (in milliseconds) of completed tasks before pruning.
const PRUNE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000; // 24 hours
/// Number of files to process per batch during reindex.
const REINDEX_BATCH_SIZE: usize = 50;
/// Number of consecutive indexing failures before the circuit breaker trips.
const CIRCUIT_BREAKER_THRESHOLD: usize = 10;
/// Number of recent batch times to keep for ETA calculation.
const ROLLING_AVERAGE_WINDOW: usize = 10;

/// Spawn a background task worker that dequeues and executes tasks in a loop.
///
/// Follows the heartbeat pattern: `tokio::spawn` + loop + sleep.
/// Accepts a [`CancellationToken`](tokio_util::sync::CancellationToken) for
/// graceful shutdown. When the token is cancelled, the worker finishes
/// processing the current task (if any) and then exits.
///
/// Returns a JoinHandle that resolves when the task exits.
pub fn spawn_task_worker(
    queue: Arc<TaskQueue>,
    engine: Arc<StorageEngine>,
    plugin_manager: Arc<PluginManager>,
    event_bus: Arc<EventBus>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                tracing::info!("Task worker shutting down");
                break;
            }

            let queue_clone = queue.clone();
            let engine_clone = engine.clone();
            let plugin_manager_clone = plugin_manager.clone();
            let event_bus_clone = event_bus.clone();

            // Use spawn_blocking since engine work is CPU-bound.
            let result = tokio::task::spawn_blocking(move || {
                process_next_task_internal(
                    &queue_clone,
                    &engine_clone,
                    &plugin_manager_clone,
                    &event_bus_clone,
                )
            })
            .await;

            let sleep_duration = match result {
                Ok(Ok(true)) => {
                    // A task was processed; brief pause before checking for more.
                    Duration::from_secs(1)
                }
                Ok(Ok(false)) => {
                    // No task found; wait longer before polling again.
                    Duration::from_secs(2)
                }
                Ok(Err(_)) | Err(_) => {
                    // Error occurred; wait before retrying.
                    Duration::from_secs(2)
                }
            };

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Task worker shutting down");
                    break;
                }
                _ = sleep(sleep_duration) => {}
            }
        }
    })
}

/// Process the next pending task from the queue. Returns true if a task was processed.
///
/// This is one iteration of the worker loop -- dequeue, execute, update status.
/// Designed for direct use in tests without spawning the infinite loop.
pub fn process_next_task(
    queue: &TaskQueue,
    engine: &StorageEngine,
    plugin_manager: &PluginManager,
    event_bus: &EventBus,
) -> EngineResult<bool> {
    process_next_task_internal(queue, engine, plugin_manager, event_bus)
}

fn process_next_task_internal(
    queue: &TaskQueue,
    engine: &StorageEngine,
    plugin_manager: &PluginManager,
    event_bus: &EventBus,
) -> EngineResult<bool> {
    // H18: dequeue_next atomically finds the oldest pending task and marks
    // it Running under a lock, preventing double-dequeue.
    let task = match queue.dequeue_next()? {
        Some(task) => task,
        None => return Ok(false),
    };

    // Emit task started event.
    let started_event = EngineEvent::new(
        EVENT_TASKS_STARTED,
        "system",
        serde_json::json!({
            "task_id": task.id,
            "task_type": task.task_type,
            "args": task.args,
        }),
    );
    event_bus.emit(started_event);

    // Execute based on task type.
    let result = match task.task_type.as_str() {
        "reindex" => execute_reindex(queue, &task, engine, plugin_manager),
        "gc" => execute_gc(queue, &task, engine),
        "backup" => execute_backup(&task, engine),
        unknown => Err(format!("unknown task type: {}", unknown)),
    };

    match result {
        Ok(summary) => {
            queue.update_status(&task.id, TaskStatus::Completed, None)?;
            let completed_event = EngineEvent::new(
                EVENT_TASKS_COMPLETED,
                "system",
                serde_json::json!({
                    "task_id": task.id,
                    "task_type": task.task_type,
                    "summary": summary,
                }),
            );
            event_bus.emit(completed_event);
        }
        Err(error_message) => {
            queue.update_status(&task.id, TaskStatus::Failed, Some(error_message.clone()))?;
            let failed_event = EngineEvent::new(
                EVENT_TASKS_FAILED,
                "system",
                serde_json::json!({
                    "task_id": task.id,
                    "task_type": task.task_type,
                    "error": error_message,
                }),
            );
            event_bus.emit(failed_event);
        }
    }

    // Clear progress and prune old tasks.
    queue.clear_progress(&task.id);
    let _ = queue.prune_completed(PRUNE_MAX_AGE_MS, PRUNE_MAX_COUNT);

    Ok(true)
}

/// Execute a reindex task: re-run the indexing pipeline on all files under a directory.
fn execute_reindex(
    queue: &TaskQueue,
    task: &TaskRecord,
    engine: &StorageEngine,
    plugin_manager: &PluginManager,
) -> Result<String, String> {
    let path = task
        .args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'path' argument".to_string())?;

    let ops = DirectoryOps::new(engine);

    // Verify index config exists.
    let config_path = if path.ends_with('/') {
        format!("{}.config/indexes.json", path)
    } else {
        format!("{}/.aeordb-config/indexes.json", path)
    };
    ops.read_file(&config_path)
        .map_err(|e| format!("cannot read index config at {}: {}", config_path, e))?;

    // List directory entries and filter to file records only.
    let entries = ops
        .list_directory(path)
        .map_err(|e| format!("cannot list directory {}: {}", path, e))?;

    let mut file_entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.entry_type == EntryType::FileRecord.to_u8())
        .collect();

    // Sort alphabetically by name for deterministic ordering.
    file_entries.sort_by(|a, b| a.name.cmp(&b.name));

    // If there's a checkpoint, skip entries at or before it.
    if let Some(ref checkpoint) = task.checkpoint {
        file_entries.retain(|entry| entry.name.as_str() > checkpoint.as_str());
    }

    let total_count = file_entries.len();
    if total_count == 0 {
        return Ok("reindexed 0 files".to_string());
    }

    let pipeline = IndexingPipeline::with_plugin_manager(engine, plugin_manager);
    let ctx = RequestContext::system();

    let mut indexed_count: usize = 0;
    let mut consecutive_failures: usize = 0;
    let mut batch_times: Vec<Duration> = Vec::new();
    let start = Instant::now();

    // Process in batches.
    for batch in file_entries.chunks(REINDEX_BATCH_SIZE) {
        let batch_start = Instant::now();

        for entry in batch {
            let file_path = if path.ends_with('/') {
                format!("{}{}", path, entry.name)
            } else {
                format!("{}/{}", path, entry.name)
            };

            // Read file content.
            let data = match ops.read_file(&file_path) {
                Ok(data) => data,
                Err(_) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
                        return Err(format!(
                            "circuit breaker: {} consecutive indexing failures",
                            CIRCUIT_BREAKER_THRESHOLD
                        ));
                    }
                    indexed_count += 1;
                    continue;
                }
            };

            // Get metadata for content_type.
            let content_type = ops
                .get_metadata(&file_path)
                .ok()
                .flatten()
                .and_then(|record| record.content_type);

            // Run the indexing pipeline.
            match pipeline.run(&ctx, &file_path, &data, content_type.as_deref()) {
                Ok(()) => {
                    consecutive_failures = 0;
                }
                Err(_) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
                        return Err(format!(
                            "circuit breaker: {} consecutive indexing failures",
                            CIRCUIT_BREAKER_THRESHOLD
                        ));
                    }
                }
            }

            indexed_count += 1;
        }

        let batch_duration = batch_start.elapsed();
        batch_times.push(batch_duration);
        if batch_times.len() > ROLLING_AVERAGE_WINDOW {
            batch_times.remove(0);
        }

        // Update checkpoint to the last file name in this batch.
        if let Some(last_entry) = batch.last() {
            let _ = queue.update_checkpoint(&task.id, &last_entry.name);
        }

        // Compute progress and ETA.
        let progress = indexed_count as f64 / total_count as f64;
        let eta_ms = compute_eta(&batch_times, total_count, indexed_count);

        queue.set_progress(
            &task.id,
            ProgressInfo {
                task_id: task.id.clone(),
                task_type: task.task_type.clone(),
                args: task.args.clone(),
                progress,
                eta_ms,
                indexed_count,
                total_count,
                stale_since: None,
                message: Some(format!("indexed {}/{} files", indexed_count, total_count)),
            },
        );

        // Check for cancellation after each batch.
        if queue.is_cancelled(&task.id) {
            return Err("cancelled".to_string());
        }
    }

    let elapsed_ms = start.elapsed().as_millis();
    Ok(format!(
        "reindexed {} files in {}ms",
        indexed_count, elapsed_ms
    ))
}

/// Execute a garbage collection task.
fn execute_gc(
    _queue: &TaskQueue,
    task: &TaskRecord,
    engine: &StorageEngine,
) -> Result<String, String> {
    let dry_run = task
        .args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let ctx = RequestContext::system();
    let result = run_gc(engine, &ctx, dry_run).map_err(|e| format!("gc failed: {}", e))?;

    Ok(format!(
        "gc completed: {} garbage entries, {} bytes reclaimed, dry_run={}",
        result.garbage_entries, result.reclaimed_bytes, result.dry_run
    ))
}

/// Execute a backup task: export HEAD (or a named snapshot) to a timestamped `.aeordb` file.
///
/// Task args:
/// - `backup_dir` (string) -- destination directory, default `"./backups/"`.
/// - `retention_count` (integer) -- keep at most this many `.aeordb` files in
///   `backup_dir`. 0 means unlimited. Default: 0.
/// - `snapshot` (string, optional) -- export a named snapshot instead of HEAD.
fn execute_backup(
    task: &TaskRecord,
    engine: &StorageEngine,
) -> Result<String, String> {
    let backup_dir = task
        .args
        .get("backup_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("./backups/");

    let retention_count = task
        .args
        .get("retention_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    let snapshot_name = task
        .args
        .get("snapshot")
        .and_then(|v| v.as_str());

    // Ensure the backup directory exists.
    std::fs::create_dir_all(backup_dir)
        .map_err(|error| format!("failed to create backup directory '{}': {}", backup_dir, error))?;

    // Build a timestamped output filename (with milliseconds to avoid collisions).
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let filename = match snapshot_name {
        Some(name) => format!("backup-{}-{}.aeordb", name, timestamp),
        None => format!("backup-head-{}.aeordb", timestamp),
    };
    let output_path = std::path::Path::new(backup_dir).join(&filename);
    let output_path_string = output_path.to_string_lossy().to_string();

    // Run the export.
    let result = backup::export_snapshot(engine, snapshot_name, &output_path_string)
        .map_err(|error| format!("backup export failed: {}", error))?;

    // Enforce retention policy if configured.
    if retention_count > 0 {
        if let Err(error) = enforce_backup_retention(backup_dir, retention_count) {
            // Retention failure is not fatal -- log but do not fail the task.
            tracing::warn!(
                backup_dir = %backup_dir,
                retention_count = %retention_count,
                error = %error,
                "backup retention enforcement failed"
            );
        }
    }

    Ok(format!(
        "backup created: {} ({} chunks, {} files, {} dirs)",
        filename,
        result.chunks_written,
        result.files_written,
        result.directories_written,
    ))
}

/// Remove oldest `.aeordb` files in `backup_dir` until at most `keep` remain.
///
/// Files are sorted by modification time (oldest first), and excess files are deleted.
fn enforce_backup_retention(backup_dir: &str, keep: usize) -> Result<(), String> {
    let mut aeordb_files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

    let entries = std::fs::read_dir(backup_dir)
        .map_err(|error| format!("failed to read backup directory: {}", error))?;

    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read directory entry: {}", error))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("aeordb") {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            aeordb_files.push((path, modified));
        }
    }

    if aeordb_files.len() <= keep {
        return Ok(());
    }

    // Sort oldest first.
    aeordb_files.sort_by_key(|(_path, modified)| *modified);

    let remove_count = aeordb_files.len() - keep;
    for (path, _modified) in aeordb_files.iter().take(remove_count) {
        if let Err(error) = std::fs::remove_file(path) {
            tracing::warn!(path = %path.display(), error = %error, "failed to remove old backup");
        }
    }

    Ok(())
}

/// Compute estimated time remaining based on rolling average of batch durations.
fn compute_eta(
    batch_times: &[Duration],
    total_count: usize,
    indexed_count: usize,
) -> Option<i64> {
    if batch_times.is_empty() || indexed_count >= total_count {
        return None;
    }

    let total_batch_ms: u128 = batch_times.iter().map(|d| d.as_millis()).sum();
    let average_batch_ms = total_batch_ms / batch_times.len() as u128;
    let remaining_files = total_count - indexed_count;
    let remaining_batches =
        (remaining_files + REINDEX_BATCH_SIZE - 1) / REINDEX_BATCH_SIZE;
    let eta_ms = average_batch_ms * remaining_batches as u128;

    Some(eta_ms as i64)
}
