pub mod admin_routes;
pub mod backup_routes;
pub mod engine_routes;
pub mod portal_routes;
pub mod responses;
pub mod routes;
pub mod state;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{delete, get, post, put};
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::trace::TraceLayer;

use crate::auth::{AuthProvider, FileAuthProvider, JwtManager, NoAuthProvider};
use crate::auth::auth_uri::AuthMode;
use crate::auth::RateLimiter;
use crate::engine::{DirectoryOps, GroupCache, PermissionsCache, StorageEngine};
use crate::logging::request_id_middleware;
use crate::metrics::http_metrics_layer::HttpMetricsLayer;
use crate::metrics::initialize_metrics;
use crate::plugins::PluginManager;
use state::AppState;

/// Default cache TTL in seconds.
const DEFAULT_CACHE_TTL_SECONDS: u64 = 60;

/// Build the full application router with all routes and middleware.
/// Uses SelfContained auth mode (current default behavior).
pub fn create_app(engine_path: &str) -> Router {
  let engine = create_engine_for_storage(engine_path);
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let jwt_manager = Arc::new(JwtManager::from_bytes(&auth_provider.jwt_manager().to_bytes())
    .expect("failed to reconstruct JWT manager from auth provider"));
  let prometheus_handle = initialize_metrics();
  create_app_with_provider_and_metrics(auth_provider, jwt_manager, prometheus_handle, engine)
}

/// Build the full application router with a specific auth mode.
/// Returns the router and optionally a bootstrap key (for file:// mode).
pub fn create_app_with_auth_mode(
  engine_path: &str,
  auth_mode: &AuthMode,
) -> (Router, Option<String>) {
  let engine = create_engine_for_storage(engine_path);
  let (auth_provider, bootstrap_key): (Arc<dyn AuthProvider>, Option<String>) = match auth_mode {
    AuthMode::Disabled => (Arc::new(NoAuthProvider::new()), None),
    AuthMode::SelfContained => {
      let provider = FileAuthProvider::new(engine.clone());
      (Arc::new(provider), None)
    }
    AuthMode::File(path) => {
      let (provider, key) = FileAuthProvider::from_identity_file(path)
        .expect("failed to create auth provider from identity file");
      (Arc::new(provider), key)
    }
  };

  let jwt_manager = Arc::new(JwtManager::from_bytes(&auth_provider.jwt_manager().to_bytes())
    .expect("failed to reconstruct JWT manager from auth provider"));
  let prometheus_handle = initialize_metrics();
  let router = create_app_with_provider_and_metrics(auth_provider, jwt_manager, prometheus_handle, engine);
  (router, bootstrap_key)
}

/// Build the application router with a specific JwtManager (useful for tests).
/// Creates a FileAuthProvider backed by the given engine.
pub fn create_app_with_jwt(jwt_manager: Arc<JwtManager>, engine: Arc<StorageEngine>) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  create_app_with_provider_and_metrics(auth_provider, jwt_manager, prometheus_handle, engine)
}

/// Build the application router with a specific JwtManager and engine (useful
/// for tests that need to reuse the same StorageEngine across rebuilds).
pub fn create_app_with_jwt_and_engine(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());

  create_app_with_all(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine)
}

/// Build the application router with an explicit PrometheusHandle.
pub fn create_app_with_jwt_and_metrics(
  jwt_manager: Arc<JwtManager>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
) -> Router {
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  create_app_with_provider_and_metrics(auth_provider, jwt_manager, prometheus_handle, engine)
}

/// Build the application router with an auth provider and metrics.
fn create_app_with_provider_and_metrics(
  auth_provider: Arc<dyn AuthProvider>,
  jwt_manager: Arc<JwtManager>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
) -> Router {
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());

  create_app_with_all(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine)
}

/// Build the application router with all dependencies injected (useful for tests
/// that need to control the rate limiter).
pub fn create_app_with_all(
  auth_provider: Arc<dyn AuthProvider>,
  jwt_manager: Arc<JwtManager>,
  plugin_manager: Arc<PluginManager>,
  rate_limiter: Arc<RateLimiter>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
) -> Router {
  let cache_ttl = Duration::from_secs(DEFAULT_CACHE_TTL_SECONDS);
  let group_cache = Arc::new(GroupCache::new(cache_ttl));
  let permissions_cache = Arc::new(PermissionsCache::new(cache_ttl));

  let app_state = AppState {
    jwt_manager,
    auth_provider,
    plugin_manager,
    rate_limiter,
    prometheus_handle,
    engine,
    group_cache,
    permissions_cache,
  };

  // Routes that require authentication
  let protected_routes = Router::new()
    .route("/admin/api-keys", post(routes::create_api_key).get(routes::list_api_keys))
    .route("/admin/api-keys/{key_id}", delete(routes::revoke_api_key))
    .route("/admin/metrics", get(routes::metrics_endpoint))
    .route("/api/stats", get(portal_routes::get_stats))
    // Admin user/group management
    .route("/admin/users", post(admin_routes::create_user).get(admin_routes::list_users))
    .route(
      "/admin/users/{user_id}",
      get(admin_routes::get_user)
        .patch(admin_routes::update_user)
        .delete(admin_routes::deactivate_user),
    )
    .route("/admin/groups", post(admin_routes::create_group).get(admin_routes::list_groups))
    .route(
      "/admin/groups/{name}",
      get(admin_routes::get_group)
        .patch(admin_routes::update_group)
        .delete(admin_routes::delete_group),
    )
    // Backup routes (export, diff, import, promote)
    .route("/admin/export", post(backup_routes::export_backup))
    .route("/admin/diff", post(backup_routes::diff_backup))
    .route("/admin/import", post(backup_routes::import_backup))
    .route("/admin/promote", post(backup_routes::promote_head))
    // Engine routes (custom storage engine)
    .route(
      "/engine/{*path}",
      put(engine_routes::engine_store_file)
        .get(engine_routes::engine_get)
        .delete(engine_routes::engine_delete_file)
        .head(engine_routes::engine_head),
    )
    // Query route
    .route("/query", post(engine_routes::query_endpoint))
    // Version: snapshot routes
    .route("/version/snapshot", post(engine_routes::snapshot_create))
    .route("/version/snapshots", get(engine_routes::snapshot_list))
    .route("/version/restore", post(engine_routes::snapshot_restore))
    .route("/version/snapshot/{name}", delete(engine_routes::snapshot_delete))
    // Version: fork routes
    .route("/version/fork", post(engine_routes::fork_create))
    .route("/version/forks", get(engine_routes::fork_list))
    .route("/version/fork/{name}/promote", post(engine_routes::fork_promote))
    .route("/version/fork/{name}", delete(engine_routes::fork_abandon))
    // Plugin routes
    .route(
      "/{database}/{schema}/{table}/_deploy",
      put(routes::deploy_plugin),
    )
    .route(
      "/{database}/{schema}/{table}/{function_name}/_invoke",
      post(routes::invoke_plugin),
    )
    .route(
      "/{database}/_plugins",
      get(routes::list_plugins),
    )
    .route(
      "/{database}/{schema}/{table}/{function_name}/_remove",
      delete(routes::remove_plugin),
    )
    .route_layer(from_fn_with_state(app_state.clone(), permission_middleware))
    .route_layer(from_fn_with_state(app_state.clone(), auth_middleware));

  // Public routes (no auth required)
  let public_routes = Router::new()
    .route("/admin/health", get(routes::health_check))
    .route("/auth/token", post(routes::auth_token))
    .route("/auth/magic-link", post(routes::request_magic_link))
    .route("/auth/magic-link/verify", get(routes::verify_magic_link))
    .route("/auth/refresh", post(routes::refresh_token))
    // Portal (embedded dashboard UI)
    .route("/portal", get(portal_routes::portal_index))
    .route("/portal/", get(portal_routes::portal_index))
    .route("/portal/{filename}", get(portal_routes::portal_asset));

  public_routes
    .merge(protected_routes)
    .with_state(app_state)
    .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)) // 10 GB
    .layer(HttpMetricsLayer)
    .layer(from_fn(request_id_middleware))
    .layer(TraceLayer::new_for_http())
}


/// Initialize or retrieve the global Prometheus recorder handle.
fn try_initialize_metrics() -> PrometheusHandle {
  initialize_metrics()
}

use crate::auth::middleware::auth_middleware;
use crate::auth::permission_middleware::permission_middleware;

/// Create or open a StorageEngine at the given path.
pub fn create_engine_for_storage(engine_path: &str) -> Arc<StorageEngine> {
  let path = std::path::Path::new(engine_path);
  let engine = if path.exists() {
    StorageEngine::open(engine_path)
      .expect("failed to open storage engine")
  } else {
    StorageEngine::create(engine_path)
      .expect("failed to create storage engine")
  };
  let engine = Arc::new(engine);
  let directory_ops = DirectoryOps::new(&engine);
  directory_ops
    .ensure_root_directory()
    .expect("failed to create engine root directory");
  engine
}

/// Create an engine backed by a temporary file (for tests).
pub fn create_temp_engine_for_tests() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
  let engine_file = temp_dir.path().join("test.aeordb");
  let engine_path = engine_file.to_str().expect("valid temp path");
  let engine = create_engine_for_storage(engine_path);
  (engine, temp_dir)
}
