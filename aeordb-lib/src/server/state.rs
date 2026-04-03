use std::sync::Arc;

use metrics_exporter_prometheus::PrometheusHandle;

use crate::auth::provider::AuthProvider;
use crate::auth::JwtManager;
use crate::auth::RateLimiter;
use crate::engine::GroupCache;
use crate::engine::PermissionsCache;
use crate::engine::StorageEngine;
use crate::plugins::PluginManager;

#[derive(Clone)]
pub struct AppState {
  pub jwt_manager: Arc<JwtManager>,
  pub auth_provider: Arc<dyn AuthProvider>,
  pub plugin_manager: Arc<PluginManager>,
  pub rate_limiter: Arc<RateLimiter>,
  pub prometheus_handle: PrometheusHandle,
  pub engine: Arc<StorageEngine>,
  pub group_cache: Arc<GroupCache>,
  pub permissions_cache: Arc<PermissionsCache>,
}
