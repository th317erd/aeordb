use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::auth::JwtManager;
use crate::auth::RateLimiter;
use crate::filesystem::PathResolver;
use crate::plugins::PluginManager;
use crate::storage::RedbStorage;

#[derive(Clone)]
pub struct AppState {
  pub storage: Arc<RedbStorage>,
  pub jwt_manager: Arc<JwtManager>,
  pub plugin_manager: Arc<PluginManager>,
  pub rate_limiter: Arc<RateLimiter>,
  pub path_resolver: Arc<PathResolver>,
  pub prometheus_handle: PrometheusHandle,
}
