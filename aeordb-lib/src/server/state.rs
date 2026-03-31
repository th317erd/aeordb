use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::auth::JwtManager;
use crate::auth::RateLimiter;
use crate::engine::StorageEngine;
use crate::plugins::PluginManager;

#[derive(Clone)]
pub struct AppState {
  pub jwt_manager: Arc<JwtManager>,
  pub plugin_manager: Arc<PluginManager>,
  pub rate_limiter: Arc<RateLimiter>,
  pub prometheus_handle: PrometheusHandle,
  pub engine: Arc<StorageEngine>,
}
