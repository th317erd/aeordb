use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep;

use crate::engine::backup;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::engine_event::{EngineEvent, EVENT_TASKS_COMPLETED, EVENT_TASKS_FAILED, EVENT_TASKS_STARTED};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::event_bus::EventBus;
use crate::engine::gc::run_gc;
use crate::engine::index_store::{
  IndexWriteBuffer, IndexWriteBufferOptions, DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL, DEFAULT_INDEX_BUFFER_FLUSH_WRITES,
};
use crate::engine::index_config_resolver::{glob_matches, IndexConfigResolver};
use crate::engine::indexing_pipeline::IndexingPipeline;
use crate::engine::path_utils::normalize_path;
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
      let cancel_clone = cancel.clone();

      // Use spawn_blocking since engine work is CPU-bound.
      let result = tokio::task::spawn_blocking(move || {
        process_next_task_internal_with_cancel(&queue_clone, &engine_clone, &plugin_manager_clone, &event_bus_clone, &cancel_clone)
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
  // Tests + sync callers without a cancel token get a dummy "never-cancelled"
  // token. Production goes through process_next_task_internal_with_cancel.
  let dummy_cancel = tokio_util::sync::CancellationToken::new();
  process_next_task_internal_with_cancel(queue, engine, plugin_manager, event_bus, &dummy_cancel)
}

fn process_next_task_internal_with_cancel(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
  cancel: &tokio_util::sync::CancellationToken,
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
    "reindex" => execute_reindex(queue, &task, engine, plugin_manager, cancel),
    "gc" => execute_gc(queue, &task, engine),
    "backup" => execute_backup(&task, engine),
    "cleanup" => execute_cleanup(&task, engine, event_bus),
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
  cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, String> {
  let path = task.args.get("path").and_then(|v| v.as_str()).ok_or_else(|| "missing 'path' argument".to_string())?;
  let force = task.args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
  let metadata_only = task.args.get("metadata_only").and_then(|v| v.as_bool()).unwrap_or(false);
  let index_flush_options = reindex_index_buffer_options(&task.args);

  let ops = DirectoryOps::new(engine);
  let resolver = IndexConfigResolver::new(engine);
  let reindex_root = normalize_path(path);

  // Verify index config exists and load it to check for glob. Forced
  // reindex doubles as the schema-migration path, so it must also be able to
  // run without an indexes.json and simply migrate every file in the subtree.
  let config_path = IndexConfigResolver::config_path_for_directory(&reindex_root);
  let config = match resolver.load_config(&reindex_root) {
    Ok(Some(config)) => Some(config),
    Ok(None) if force => {
      tracing::info!(
        path = %reindex_root,
        config_path = %config_path,
        "forced reindex running migration-only because no index config was found"
      );
      None
    }
    Ok(None) => return Err(format!("cannot read index config at {}: not found", config_path)),
    Err(e) if force => {
      tracing::info!(
        path = %reindex_root,
        config_path = %config_path,
        error = %e,
        "forced reindex running migration-only because no index config was readable"
      );
      None
    }
    Err(e) => return Err(format!("cannot read index config at {}: {}", config_path, e)),
  };

  // Build a sorted list of full file paths to reindex.
  let prefix = reindex_root.trim_end_matches('/');
  let mut file_paths: Vec<String> = if force {
    collect_current_file_record_paths(engine, &reindex_root)?
  } else if let Some(ref config) = config {
    if let Some(ref glob_pattern) = config.glob {
      // Glob mode: recursive listing filtered by glob pattern.
      let all_entries = crate::engine::directory_listing::list_directory_recursive(engine, &reindex_root, -1, None, None)
        .map_err(|e| format!("cannot list directory {}: {}", reindex_root, e))?;

      all_entries
        .into_iter()
        .filter(|entry| entry.entry_type == EntryType::FileRecord.to_u8())
        .filter(|entry| !crate::engine::directory_ops::is_internal_path(&entry.path))
        .filter(|entry| {
          let relative = entry.path.trim_start_matches(prefix).trim_start_matches('/');
          glob_matches(glob_pattern, relative)
        })
        .map(|entry| entry.path)
        .collect()
    } else {
      // Non-glob mode: direct children only.
      let entries = ops.list_directory(&reindex_root).map_err(|e| format!("cannot list directory {}: {}", reindex_root, e))?;

      entries
        .into_iter()
        .filter(|entry| entry.entry_type == EntryType::FileRecord.to_u8())
        .map(|entry| format!("{}/{}", prefix, entry.name))
        .filter(|path| !crate::engine::directory_ops::is_internal_path(path))
        .collect()
    }
  } else {
    Vec::new()
  };
  file_paths.sort();

  // If there's a checkpoint, skip paths at or before it.
  if let Some(ref checkpoint) = task.checkpoint {
    file_paths.retain(|p| p.as_str() > checkpoint.as_str());
  }

  let total_count = file_paths.len();
  if total_count == 0 {
    return Ok("reindexed 0 files".to_string());
  }

  let pipeline = IndexingPipeline::with_plugin_manager(engine, plugin_manager);
  let ctx = RequestContext::system();
  let mut index_buffer = IndexWriteBuffer::new(engine, index_flush_options);

  let mut indexed_count: usize = 0;
  let mut migrated_count: usize = 0;
  let mut consecutive_failures: usize = 0;
  let mut batch_times: Vec<Duration> = Vec::new();
  let start = Instant::now();

  // Process in batches.
  for batch in file_paths.chunks(REINDEX_BATCH_SIZE) {
    let batch_start = Instant::now();
    let mut last_processed_path: Option<&str> = None;

    for file_path in batch {
      if force {
        match ops.migrate_file_record_to_current_version(file_path) {
          Ok(true) => {
            migrated_count += 1;
          }
          Ok(false) => {}
          Err(error) => {
            tracing::warn!(
              path = %file_path,
              error = %error,
              "forced reindex could not migrate FileRecord"
            );
            consecutive_failures += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              return Err(format!("circuit breaker: {} consecutive indexing failures", CIRCUIT_BREAKER_THRESHOLD));
            }
            indexed_count += 1;
            continue;
          }
        }

        if config.is_none() || skip_indexing_path(file_path) {
          consecutive_failures = 0;
          indexed_count += 1;
          last_processed_path = Some(file_path);
          continue;
        }
      }

      let index_result = if metadata_only {
        pipeline.run_metadata_only_buffered(&ctx, file_path, &mut index_buffer)
      } else {
        // Read file content only for full parser/content reindexing. Metadata-only
        // reindexing reads the FileRecord header through the pipeline instead.
        let data = match ops.read_file_buffered(file_path) {
          Ok(data) => data,
          Err(_) => {
            consecutive_failures += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              return Err(format!("circuit breaker: {} consecutive indexing failures", CIRCUIT_BREAKER_THRESHOLD));
            }
            indexed_count += 1;
            last_processed_path = Some(file_path);
            continue;
          }
        };

        let content_type = ops.get_metadata(file_path).ok().flatten().and_then(|record| record.content_type);
        pipeline.run_buffered(&ctx, file_path, &data, content_type.as_deref(), &mut index_buffer)
      };

      match index_result {
        Ok(()) => {
          consecutive_failures = 0;
        }
        Err(_) => {
          consecutive_failures += 1;
          if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
            return Err(format!("circuit breaker: {} consecutive indexing failures", CIRCUIT_BREAKER_THRESHOLD));
          }
        }
      }

      indexed_count += 1;
      last_processed_path = Some(file_path);

      match index_buffer.flush_if_due() {
        Ok(true) => {
          let _ = queue.update_checkpoint(&task.id, file_path);
        }
        Ok(false) => {}
        Err(error) => return Err(format!("index flush failed: {}", error)),
      }
    }

    let batch_duration = batch_start.elapsed();
    batch_times.push(batch_duration);
    if batch_times.len() > ROLLING_AVERAGE_WINDOW {
      batch_times.remove(0);
    }

    // Only advance the checkpoint past buffered index mutations after they have
    // been flushed. If there are no pending index mutations, all completed work
    // is durable and the batch checkpoint is safe.
    if index_buffer.stats().pending_mutations == 0 {
      if let Some(last_path) = last_processed_path {
        let _ = queue.update_checkpoint(&task.id, last_path);
      }
    }

    // Compute progress and ETA.
    let progress = indexed_count as f64 / total_count as f64;
    let eta_ms = compute_eta(&batch_times, total_count, indexed_count);

    let index_stats = index_buffer.stats();
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
        message: Some(format!(
          "indexed {}/{} files, migrated {}, metadata_only={}, index_mutations={}, pending_index_mutations={}, index_flushes={}, cached_indexes={}",
          indexed_count,
          total_count,
          migrated_count,
          metadata_only,
          index_stats.mutations,
          index_stats.pending_mutations,
          index_stats.flushes,
          index_stats.cached_indexes
        )),
      },
    );

    // Check for per-task or shutdown cancellation after each batch. The
    // outer cancel covers graceful shutdown — without polling it here the
    // worker can't exit during a long reindex.
    if queue.is_cancelled(&task.id) || cancel.is_cancelled() {
      return Err("cancelled".to_string());
    }
  }

  let flushed_indexes = index_buffer.flush_all().map_err(|error| format!("final index flush failed: {}", error))?;
  if let Some(last_path) = file_paths.last() {
    let _ = queue.update_checkpoint(&task.id, last_path);
  }

  let elapsed_ms = start.elapsed().as_millis();
  let index_stats = index_buffer.stats();
  let index_summary = format!(
    ", metadata_only={}, index_mutations={}, index_flushes={}, flushed_indexes={} (+{} final), cached_indexes={}",
    metadata_only, index_stats.mutations, index_stats.flushes, index_stats.flushed_indexes, flushed_indexes, index_stats.cached_indexes
  );
  if force {
    Ok(format!("reindexed {} files, migrated {} records in {}ms{}", indexed_count, migrated_count, elapsed_ms, index_summary))
  } else {
    Ok(format!("reindexed {} files in {}ms{}", indexed_count, elapsed_ms, index_summary))
  }
}

fn reindex_index_buffer_options(args: &serde_json::Value) -> IndexWriteBufferOptions {
  let flush_after_writes = args
    .get("index_flush_writes")
    .and_then(|value| value.as_u64())
    .and_then(|value| usize::try_from(value).ok())
    .unwrap_or(DEFAULT_INDEX_BUFFER_FLUSH_WRITES)
    .max(1);

  let flush_after =
    args.get("index_flush_ms").and_then(|value| value.as_u64()).map(Duration::from_millis).unwrap_or(DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL);

  IndexWriteBufferOptions::new(flush_after_writes, flush_after)
}

fn collect_current_file_record_paths(engine: &StorageEngine, base_path: &str) -> Result<Vec<String>, String> {
  let normalized_base = crate::engine::path_utils::normalize_path(base_path);
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let entries = engine.entries_by_type(crate::engine::KV_TYPE_FILE_RECORD).map_err(|error| error.to_string())?;
  let mut paths = std::collections::BTreeSet::new();

  for (hash, value) in entries {
    let Ok(Some((header, _key, _value))) = engine.get_entry_including_deleted(&hash) else {
      continue;
    };
    let Ok(record) = crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) else {
      continue;
    };
    if !path_in_reindex_scope(&normalized_base, &record.path) {
      continue;
    }

    let Ok(path_key) = crate::engine::directory_ops::file_path_hash(&record.path, &algo) else {
      continue;
    };
    if engine.get_entry(&path_key).map_err(|error| error.to_string())?.is_some() {
      paths.insert(record.path);
    }
  }

  Ok(paths.into_iter().collect())
}

fn path_in_reindex_scope(base_path: &str, candidate_path: &str) -> bool {
  if base_path == "/" {
    return true;
  }

  candidate_path == base_path || candidate_path.strip_prefix(base_path.trim_end_matches('/')).is_some_and(|suffix| suffix.starts_with('/'))
}

fn skip_indexing_path(path: &str) -> bool {
  crate::engine::directory_ops::is_internal_path(path) || crate::engine::directory_ops::is_system_path(path)
}

/// Execute a garbage collection task.
fn execute_gc(_queue: &TaskQueue, task: &TaskRecord, engine: &StorageEngine) -> Result<String, String> {
  let dry_run = task.args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);

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
fn execute_backup(task: &TaskRecord, engine: &StorageEngine) -> Result<String, String> {
  let backup_dir = task.args.get("backup_dir").and_then(|v| v.as_str()).unwrap_or("./backups/");

  let retention_count = task.args.get("retention_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

  let snapshot_name = task.args.get("snapshot").and_then(|v| v.as_str());

  // Ensure the backup directory exists.
  std::fs::create_dir_all(backup_dir).map_err(|error| format!("failed to create backup directory '{}': {}", backup_dir, error))?;

  // Build a timestamped output filename (with milliseconds to avoid collisions).
  let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
  let filename = match snapshot_name {
    Some(name) => format!("backup-{}-{}.aeordb", name, timestamp),
    None => format!("backup-head-{}.aeordb", timestamp),
  };
  let output_path = std::path::Path::new(backup_dir).join(&filename);
  let output_path_string = output_path.to_string_lossy().to_string();

  // Run the export. Scheduled backups don't include system data —
  // they're for user data history, not credential rotation.
  let result = backup::export_snapshot(engine, snapshot_name, &output_path_string, false)
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
    filename, result.chunks_written, result.files_written, result.directories_written,
  ))
}

/// Execute a cleanup task: remove expired refresh tokens and used/expired
/// magic links from the system store. Intended to run on a default hourly cron.
fn execute_cleanup(_task: &TaskRecord, engine: &StorageEngine, _event_bus: &EventBus) -> Result<String, String> {
  let ctx = RequestContext::system();
  let (tokens, links) =
    crate::engine::system_store::cleanup_expired_tokens(engine, &ctx).map_err(|error| format!("cleanup failed: {}", error))?;
  Ok(format!("cleaned {} tokens and {} magic links", tokens, links))
}

/// Remove oldest `.aeordb` files in `backup_dir` until at most `keep` remain.
///
/// Files are sorted by modification time (oldest first), and excess files are deleted.
fn enforce_backup_retention(backup_dir: &str, keep: usize) -> Result<(), String> {
  let mut aeordb_files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

  let entries = std::fs::read_dir(backup_dir).map_err(|error| format!("failed to read backup directory: {}", error))?;

  for entry in entries {
    let entry = entry.map_err(|error| format!("failed to read directory entry: {}", error))?;
    let path = entry.path();
    if path.extension().and_then(|ext| ext.to_str()) == Some("aeordb") {
      let modified = entry.metadata().and_then(|metadata| metadata.modified()).unwrap_or(std::time::UNIX_EPOCH);
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
fn compute_eta(batch_times: &[Duration], total_count: usize, indexed_count: usize) -> Option<i64> {
  if batch_times.is_empty() || indexed_count >= total_count {
    return None;
  }

  let total_batch_ms: u128 = batch_times.iter().map(|d| d.as_millis()).sum();
  let average_batch_ms = total_batch_ms / batch_times.len() as u128;
  let remaining_files = total_count - indexed_count;
  let remaining_batches = remaining_files.div_ceil(REINDEX_BATCH_SIZE);
  let eta_ms = average_batch_ms * remaining_batches as u128;

  Some(eta_ms as i64)
}
