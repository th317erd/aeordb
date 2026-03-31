pub mod engine_routes;
pub mod responses;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{delete, get, post, put};
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::trace::TraceLayer;

use crate::auth::JwtManager;
use crate::auth::RateLimiter;
use crate::engine::{DirectoryOps, StorageEngine};
use crate::filesystem::PathResolver;
use crate::logging::request_id_middleware;
use crate::metrics::http_metrics_layer::HttpMetricsLayer;
use crate::metrics::initialize_metrics;
use crate::plugins::PluginManager;
use crate::storage::{ChunkStore, RedbStorage};
use state::AppState;

const SIGNING_KEY_CONFIG: &str = "jwt_signing_key";

/// Build the full application router with all routes and middleware.
/// Loads or generates the signing key from the storage config table.
pub fn create_app(storage: Arc<RedbStorage>, engine_path: &str) -> Router {
  let jwt_manager = load_or_create_jwt_manager(&storage);
  let prometheus_handle = initialize_metrics();
  let engine = create_engine_for_storage(engine_path);
  create_app_with_jwt_and_metrics(storage, Arc::new(jwt_manager), prometheus_handle, engine)
}

/// Load an existing signing key from config, or generate a new one and persist it.
fn load_or_create_jwt_manager(storage: &RedbStorage) -> JwtManager {
  if let Ok(Some(key_bytes)) = storage.get_config(SIGNING_KEY_CONFIG) {
    if let Ok(manager) = JwtManager::from_bytes(&key_bytes) {
      return manager;
    }
  }

  let manager = JwtManager::generate();
  let key_bytes = manager.to_bytes();
  storage
    .store_config(SIGNING_KEY_CONFIG, &key_bytes)
    .expect("failed to persist JWT signing key");
  manager
}

/// Build the application router with a specific JwtManager (useful for tests).
/// Initializes a fresh metrics recorder. If a global recorder is already
/// installed (e.g. from another test), this falls back to a no-op recorder
/// and the prometheus handle will render empty output.
pub fn create_app_with_jwt(storage: Arc<RedbStorage>, jwt_manager: Arc<JwtManager>, engine: Arc<StorageEngine>) -> Router {
  let prometheus_handle = try_initialize_metrics();
  create_app_with_jwt_and_metrics(storage, jwt_manager, prometheus_handle, engine)
}

/// Build the application router with a specific JwtManager and engine (useful
/// for tests that need to reuse the same StorageEngine across rebuilds).
pub fn create_app_with_jwt_and_engine(
  storage: Arc<RedbStorage>,
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let rate_limiter = Arc::new(RateLimiter::default_config());

  let database_arc = storage.database_arc();
  let chunk_store = ChunkStore::new_with_redb(database_arc.clone());
  let path_resolver = Arc::new(PathResolver::new(database_arc, chunk_store));

  create_app_with_all(storage, jwt_manager, plugin_manager, rate_limiter, path_resolver, prometheus_handle, engine)
}

/// Build the application router with an explicit PrometheusHandle.
pub fn create_app_with_jwt_and_metrics(
  storage: Arc<RedbStorage>,
  jwt_manager: Arc<JwtManager>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
) -> Router {
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let rate_limiter = Arc::new(RateLimiter::default_config());

  let database_arc = storage.database_arc();
  let chunk_store = ChunkStore::new_with_redb(database_arc.clone());
  let path_resolver = Arc::new(PathResolver::new(database_arc, chunk_store));

  create_app_with_all(storage, jwt_manager, plugin_manager, rate_limiter, path_resolver, prometheus_handle, engine)
}

/// Build the application router with all dependencies injected (useful for tests
/// that need to control the rate limiter).
pub fn create_app_with_all(
  storage: Arc<RedbStorage>,
  jwt_manager: Arc<JwtManager>,
  plugin_manager: Arc<PluginManager>,
  rate_limiter: Arc<RateLimiter>,
  path_resolver: Arc<PathResolver>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
) -> Router {
  let app_state = AppState {
    storage,
    jwt_manager,
    plugin_manager,
    rate_limiter,
    path_resolver,
    prometheus_handle,
    engine,
  };

  // Routes that require authentication
  let protected_routes = Router::new()
    .route(
      "/{database}/{table}",
      post(routes::create_document).get(routes::list_documents),
    )
    .route(
      "/{database}/{table}/{id}",
      get(routes::get_document)
        .patch(routes::update_document)
        .delete(routes::delete_document),
    )
    .route("/admin/api-keys", post(routes::create_api_key).get(routes::list_api_keys))
    .route("/admin/api-keys/{key_id}", delete(routes::revoke_api_key))
    .route("/admin/metrics", get(routes::metrics_endpoint))
    // Filesystem routes (existing redb-based)
    .route(
      "/fs/{*path}",
      put(routes::filesystem_store_file)
        .get(routes::filesystem_get)
        .delete(routes::filesystem_delete_file)
        .head(routes::filesystem_head),
    )
    // Engine routes (new custom storage engine)
    .route(
      "/engine/{*path}",
      put(engine_routes::engine_store_file)
        .get(engine_routes::engine_get)
        .delete(engine_routes::engine_delete_file)
        .head(engine_routes::engine_head),
    )
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
    .route_layer(from_fn_with_state(app_state.clone(), auth_middleware));

  // Public routes (no auth required)
  let public_routes = Router::new()
    .route("/admin/health", get(routes::health_check))
    .route("/auth/token", post(routes::auth_token))
    .route("/auth/magic-link", post(routes::request_magic_link))
    .route("/auth/magic-link/verify", get(routes::verify_magic_link))
    .route("/auth/refresh", post(routes::refresh_token));

  public_routes
    .merge(protected_routes)
    .with_state(app_state)
    .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)) // 10 GB
    .layer(HttpMetricsLayer)
    .layer(from_fn(request_id_middleware))
    .layer(TraceLayer::new_for_http())
}

/// Initialize or retrieve the global Prometheus recorder handle.
/// Safe to call multiple times across tests and production.
fn try_initialize_metrics() -> PrometheusHandle {
  initialize_metrics()
}

use crate::auth::middleware::auth_middleware;

/// Create or open a StorageEngine at the given path.
/// Initializes the root directory so the engine is ready for file operations.
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
/// The caller should hold on to the returned `TempDir` to keep the file alive.
pub fn create_temp_engine_for_tests() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
  let engine_file = temp_dir.path().join("test.aeordb");
  let engine_path = engine_file.to_str().expect("valid temp path");
  let engine = create_engine_for_storage(engine_path);
  (engine, temp_dir)
}
