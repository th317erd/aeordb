//! Background index cleanup worker with debounced batching.
//!
//! When files are deleted, index entries become stale. Rather than doing
//! expensive synchronous index I/O on the delete path (which was taking
//! ~450ms per delete), we queue paths for background cleanup.
//!
//! The worker debounces: it waits 50ms for more paths to arrive, then
//! flushes the batch. If 100 paths accumulate before the timeout, it
//! flushes immediately. This makes bulk deletes efficient (one batch
//! instead of N independent cleanups).

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::engine::index_store::IndexManager;
use crate::engine::indexing_pipeline::IndexingPipeline;
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::storage_engine::StorageEngine;

/// Debounce timeout: flush after this much idle time.
const DEBOUNCE_MS: u64 = 50;

/// Maximum batch size: flush immediately when this many paths are queued.
const MAX_BATCH_SIZE: usize = 100;

/// Handle for sending delete paths to the background cleanup worker.
#[derive(Clone)]
pub struct IndexCleanupSender {
    tx: mpsc::UnboundedSender<String>,
}

impl IndexCleanupSender {
    /// Queue a file path for background index cleanup.
    /// Returns immediately — cleanup happens asynchronously.
    pub fn queue(&self, path: String) {
        let _ = self.tx.send(path);
    }
}

/// Spawn the background index cleanup worker. Returns a sender handle
/// that can be cloned and shared across request handlers.
pub fn spawn_index_cleanup_worker(engine: Arc<StorageEngine>) -> IndexCleanupSender {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(cleanup_loop(rx, engine));
    IndexCleanupSender { tx }
}

async fn cleanup_loop(mut rx: mpsc::UnboundedReceiver<String>, engine: Arc<StorageEngine>) {
    let mut batch: Vec<String> = Vec::new();

    loop {
        // If batch is empty, block until we get at least one path
        if batch.is_empty() {
            match rx.recv().await {
                Some(path) => batch.push(path),
                None => return, // Channel closed, shutdown
            }
        }

        // Collect more paths with debounce timeout
        loop {
            if batch.len() >= MAX_BATCH_SIZE {
                break; // Flush immediately at max
            }

            match tokio::time::timeout(
                std::time::Duration::from_millis(DEBOUNCE_MS),
                rx.recv(),
            ).await {
                Ok(Some(path)) => batch.push(path),
                Ok(None) => return, // Channel closed
                Err(_) => break,    // Timeout — flush
            }
        }

        // Flush the batch
        let paths = std::mem::take(&mut batch);
        let engine_clone = Arc::clone(&engine);

        if let Err(e) = tokio::task::spawn_blocking(move || {
            process_batch(&engine_clone, &paths);
        }).await {
            tracing::warn!("Index cleanup task panicked: {}", e);
        }
    }
}

fn process_batch(engine: &StorageEngine, paths: &[String]) {
    let index_manager = IndexManager::new(engine);
    let algo = engine.hash_algo();

    for path in paths {
        let normalized = normalize_path(path);
        let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());

        let file_key = match crate::engine::directory_ops::file_path_hash(&normalized, &algo) {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("Index cleanup: failed to hash path '{}': {}", path, e);
                continue;
            }
        };

        // Remove from parent directory indexes
        if let Ok(index_names) = index_manager.list_indexes(&parent) {
            for field_name in &index_names {
                if let Ok(Some(mut index)) = index_manager.load_index(&parent, field_name) {
                    index.remove(&file_key);
                    if let Err(e) = index_manager.save_index(&parent, &index) {
                        tracing::warn!("Index cleanup: failed to save index '{}' at '{}': {}", field_name, parent, e);
                    }
                }
            }
        }

        // Check ancestor directories for glob-based configs
        let pipeline = IndexingPipeline::new(engine);
        if let Ok(Some((_config, config_dir))) = pipeline.find_config_for_path(&normalized) {
            if config_dir != parent {
                if let Ok(ancestor_index_names) = index_manager.list_indexes(&config_dir) {
                    for field_name in &ancestor_index_names {
                        if let Ok(Some(mut index)) = index_manager.load_index(&config_dir, field_name) {
                            index.remove(&file_key);
                            if let Err(e) = index_manager.save_index(&config_dir, &index) {
                                tracing::warn!("Index cleanup: failed to save ancestor index '{}' at '{}': {}", field_name, config_dir, e);
                            }
                        }
                    }
                }
            }
        }
    }

    if !paths.is_empty() {
        tracing::debug!("Index cleanup: processed {} paths", paths.len());
    }
}
