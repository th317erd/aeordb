use std::sync::Arc;
use std::time::Instant;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::auth::provider::AuthProvider;
use crate::auth::JwtManager;
use crate::auth::RateLimiter;
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{GroupLoader, ApiKeyLoader};
use crate::engine::PeerManager;
use crate::engine::StorageEngine;
use crate::engine::EventBus;
use crate::engine::TaskQueue;
use crate::engine::RateTrackerSet;
use crate::plugins::PluginManager;

#[derive(Clone)]
pub struct AppState {
  pub jwt_manager: Arc<JwtManager>,
  pub auth_provider: Arc<dyn AuthProvider>,
  pub plugin_manager: Arc<PluginManager>,
  pub rate_limiter: Arc<RateLimiter>,
  pub prometheus_handle: PrometheusHandle,
  pub engine: Arc<StorageEngine>,
  pub event_bus: Arc<EventBus>,
  pub group_cache: Arc<Cache<GroupLoader>>,
  pub api_key_cache: Arc<Cache<ApiKeyLoader>>,
  pub task_queue: Option<Arc<TaskQueue>>,
  pub peer_manager: Arc<PeerManager>,
  pub startup_time: u64,
  pub startup_instant: Instant,
  pub db_path: String,
  /// Rate trackers for throughput calculations (writes/sec, reads/sec, etc.).
  /// Populated during server startup; None in test/legacy contexts.
  pub rate_trackers: Option<Arc<RateTrackerSet>>,
}
