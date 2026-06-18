pub mod definitions;
pub mod http_metrics_layer;

use std::sync::OnceLock;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::engine::storage_engine::EngineMemoryStats;

static GLOBAL_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder globally and return the handle used to
/// render metrics in Prometheus text format.
///
/// Safe to call multiple times -- only the first call installs the recorder;
/// subsequent calls return the same handle.
pub fn initialize_metrics() -> PrometheusHandle {
  GLOBAL_HANDLE
    .get_or_init(|| {
      metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder().expect("failed to install Prometheus metrics recorder")
    })
    .clone()
}

pub fn record_memory_metrics(memory: &EngineMemoryStats) {
  metrics::gauge!(definitions::PROCESS_RSS_BYTES).set(memory.process.rss_bytes as f64);
  metrics::gauge!(definitions::PROCESS_PEAK_RSS_BYTES).set(memory.process.peak_rss_bytes as f64);
  metrics::gauge!(definitions::PROCESS_VIRTUAL_BYTES).set(memory.process.virtual_bytes as f64);
  metrics::gauge!(definitions::PROCESS_DATA_BYTES).set(memory.process.data_bytes as f64);
  metrics::gauge!(definitions::PROCESS_SWAP_BYTES).set(memory.process.swap_bytes as f64);
  metrics::gauge!(definitions::PROCESS_THREAD_COUNT).set(memory.process.thread_count as f64);
  metrics::gauge!(definitions::PROCESS_FD_COUNT).set(memory.process.fd_count as f64);
  metrics::gauge!(definitions::ENGINE_MEMORY_ESTIMATED_BYTES).set(memory.estimated_engine_owned_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_ESTIMATED_BYTES).set(memory.index_cache.estimated_bytes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_CACHED_INDEXES).set(memory.index_cache.cached_indexes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_DIRTY_INDEXES).set(memory.index_cache.dirty_indexes as f64);
  metrics::gauge!(definitions::INDEX_CACHE_PENDING_MUTATIONS).set(memory.index_cache.pending_mutations as f64);
  metrics::gauge!(definitions::INDEX_CACHE_ENTRIES).set(memory.index_cache.entries as f64);
  metrics::gauge!(definitions::INDEX_CACHE_VALUES).set(memory.index_cache.values as f64);
  metrics::gauge!(definitions::DIRECTORY_CACHE_ESTIMATED_BYTES).set(memory.directory_cache.estimated_bytes as f64);
  metrics::gauge!(definitions::DIRECTORY_CACHE_ENTRIES).set(memory.directory_cache.entries as f64);
}
