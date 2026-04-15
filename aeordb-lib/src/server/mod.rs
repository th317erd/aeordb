pub mod admin_routes;
pub mod api_key_self_service_routes;
pub mod backup_routes;
pub mod cors;
pub mod engine_routes;
pub mod gc_routes;
pub mod portal_routes;
pub mod responses;
pub mod routes;
pub mod sse_routes;
pub mod state;
pub mod task_routes;
pub mod upload_routes;
pub mod symlink_routes;
pub mod version_file_routes;

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
use crate::engine::{DirectoryOps, EventBus, GroupCache, PermissionsCache, RequestContext, StorageEngine, TaskQueue};
use crate::logging::request_id_middleware;
use crate::metrics::http_metrics_layer::HttpMetricsLayer;
use crate::metrics::initialize_metrics;
use crate::plugins::PluginManager;
use state::AppState;

pub use cors::{CorsState, CorsRule, CorsConfig, build_cors_state, load_cors_config, parse_cors_origins};

/// Default cache TTL in seconds.
const DEFAULT_CACHE_TTL_SECONDS: u64 = 60;

// NOTE: The permission_middleware only checks /engine/ routes for path-level
// CRUD permissions. The following routes are behind auth but have no path-level
// checks: /query, /upload/*, /version/*, /{db}/{schema}/{table}/_deploy,
// /{db}/{schema}/{table}/{fn}/_invoke, /events/stream.
// Consider expanding permission checks to these routes in a future update.

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
  hot_dir: Option<&std::path::Path>,
  cors_flag: Option<&str>,
) -> (Router, Option<String>, Arc<StorageEngine>, Arc<EventBus>, Arc<TaskQueue>) {
  let engine = create_engine_with_hot_dir(engine_path, hot_dir);
  let event_bus = Arc::new(EventBus::new());
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
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let task_queue = Arc::new(TaskQueue::new(engine.clone()));
  let cors_state = build_cors_state(cors_flag, &engine);
  let router = create_app_with_all_and_task_queue(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine.clone(), event_bus.clone(), cors_state, Some(task_queue.clone()));
  (router, bootstrap_key, engine, event_bus, task_queue)
}

/// Build the application router with a specific JwtManager (useful for tests).
/// Creates a FileAuthProvider backed by the given engine. No CORS.
pub fn create_app_with_jwt(jwt_manager: Arc<JwtManager>, engine: Arc<StorageEngine>) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  create_app_with_provider_and_metrics(auth_provider, jwt_manager, prometheus_handle, engine)
}

/// Build the application router with a specific JwtManager and engine (useful
/// for tests that need to reuse the same StorageEngine across rebuilds). No CORS.
pub fn create_app_with_jwt_and_engine(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let event_bus = Arc::new(EventBus::new());
  let cors_state = CorsState { default_origins: None, rules: vec![] };

  create_app_with_all(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine, event_bus, cors_state)
}

/// Build the application router with a specific JwtManager, engine, and TaskQueue (for task tests).
pub fn create_app_with_jwt_engine_and_task_queue(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
  task_queue: Arc<TaskQueue>,
) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let event_bus = Arc::new(EventBus::new());
  let cors_state = CorsState { default_origins: None, rules: vec![] };

  create_app_with_all_and_task_queue(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine, event_bus, cors_state, Some(task_queue))
}

/// Build the application router with a specific JwtManager, engine, and CORS state (for CORS tests).
pub fn create_app_with_jwt_engine_and_cors(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
  cors_state: CorsState,
) -> Router {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let event_bus = Arc::new(EventBus::new());

  create_app_with_all(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine, event_bus, cors_state)
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

/// Build the application router with an auth provider and metrics. No CORS.
fn create_app_with_provider_and_metrics(
  auth_provider: Arc<dyn AuthProvider>,
  jwt_manager: Arc<JwtManager>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
) -> Router {
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let event_bus = Arc::new(EventBus::new());
  let cors_state = CorsState { default_origins: None, rules: vec![] };

  create_app_with_all(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine, event_bus, cors_state)
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
  event_bus: Arc<EventBus>,
  cors_state: CorsState,
) -> Router {
  create_app_with_all_and_task_queue(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine, event_bus, cors_state, None)
}

/// Build the application router with all dependencies injected, including an optional TaskQueue.
pub fn create_app_with_all_and_task_queue(
  auth_provider: Arc<dyn AuthProvider>,
  jwt_manager: Arc<JwtManager>,
  plugin_manager: Arc<PluginManager>,
  rate_limiter: Arc<RateLimiter>,
  prometheus_handle: PrometheusHandle,
  engine: Arc<StorageEngine>,
  event_bus: Arc<EventBus>,
  cors_state: CorsState,
  task_queue: Option<Arc<TaskQueue>>,
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
    event_bus,
    group_cache,
    permissions_cache,
    task_queue,
  };

  // Routes with large body limits (file uploads: 10 GB)
  let large_upload_routes = Router::new()
    .route(
      "/engine/_hash/{hex_hash}",
      get(engine_routes::engine_get_by_hash),
    )
    .route(
      "/engine/{*path}",
      put(engine_routes::engine_store_file)
        .get(engine_routes::engine_get)
        .delete(engine_routes::engine_delete_file)
        .head(engine_routes::engine_head),
    )
    .route("/upload/chunks/{hash}", put(upload_routes::upload_chunk))
    .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)); // 10 GB

  // Routes with medium body limits (backup import: 10 MB)
  let medium_upload_routes = Router::new()
    .route("/admin/import", post(backup_routes::import_backup))
    .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024)); // 10 MB

  // Routes that require authentication (default 1 MB limit)
  let protected_routes = Router::new()
    .route("/api-keys", post(api_key_self_service_routes::create_own_key)
                       .get(api_key_self_service_routes::list_own_keys))
    .route("/api-keys/{key_id}", delete(api_key_self_service_routes::revoke_own_key))
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
    // Backup routes (export, diff, promote — import handled above with 10MB limit)
    .route("/admin/export", post(backup_routes::export_backup))
    .route("/admin/diff", post(backup_routes::diff_backup))
    .route("/admin/promote", post(backup_routes::promote_head))
    .route("/admin/gc", post(gc_routes::run_gc_endpoint))
    // Task & cron routes
    .route("/admin/tasks", get(task_routes::list_tasks))
    .route("/admin/tasks/reindex", post(task_routes::trigger_reindex))
    .route("/admin/tasks/gc", post(task_routes::trigger_gc))
    .route("/admin/tasks/{id}", get(task_routes::get_task).delete(task_routes::cancel_task))
    .route("/admin/cron", get(task_routes::list_cron).post(task_routes::create_cron))
    .route("/admin/cron/{id}", delete(task_routes::delete_cron).patch(task_routes::update_cron))
    // Upload check and commit (small payloads)
    .route("/upload/check", post(upload_routes::upload_check))
    .route("/upload/commit", post(upload_routes::upload_commit))
    // SSE event stream
    .route("/events/stream", get(sse_routes::event_stream))
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
    // Version: file-level access routes
    .route("/version/file-history/{*path}", get(version_file_routes::file_history))
    .route("/version/file-restore/{*path}", post(version_file_routes::file_restore))
    // Symlink routes
    .route("/engine-symlink/{*path}", post(symlink_routes::create_symlink))
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
    // Merge the large-upload and medium-upload routes into the protected group
    .merge(large_upload_routes)
    .merge(medium_upload_routes)
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
    .route("/portal/{filename}", get(portal_routes::portal_asset))
    // Upload config (public, no auth)
    .route("/upload/config", get(upload_routes::upload_config));

  let router = public_routes
    .merge(protected_routes)
    .with_state(app_state)
    .layer(axum::extract::DefaultBodyLimit::max(1 * 1024 * 1024)) // 1 MB default for non-upload routes
    .layer(HttpMetricsLayer)
    .layer(from_fn(request_id_middleware))
    .layer(TraceLayer::new_for_http());

  // CORS middleware is the OUTERMOST layer so it can handle OPTIONS preflight
  // before auth middleware rejects for missing tokens.
  if cors_state.default_origins.is_some() || !cors_state.rules.is_empty() {
    router.layer(from_fn_with_state(cors_state, cors::cors_middleware))
  } else {
    router
  }
}


/// Initialize or retrieve the global Prometheus recorder handle.
fn try_initialize_metrics() -> PrometheusHandle {
  initialize_metrics()
}

use crate::auth::middleware::auth_middleware;
use crate::auth::permission_middleware::permission_middleware;

/// Create or open a StorageEngine at the given path (no hot file — for tests/tools).
pub fn create_engine_for_storage(engine_path: &str) -> Arc<StorageEngine> {
  create_engine_with_hot_dir(engine_path, None)
}

/// Create or open a StorageEngine with an optional hot directory for crash recovery.
pub fn create_engine_with_hot_dir(engine_path: &str, hot_dir: Option<&std::path::Path>) -> Arc<StorageEngine> {
  let path = std::path::Path::new(engine_path);
  let engine = if path.exists() {
    StorageEngine::open_with_hot_dir(engine_path, hot_dir)
      .expect("failed to open storage engine")
  } else {
    StorageEngine::create_with_hot_dir(engine_path, hot_dir)
      .expect("failed to create storage engine")
  };
  let engine = Arc::new(engine);
  let ctx = RequestContext::system();
  let directory_ops = DirectoryOps::new(&engine);
  directory_ops
    .ensure_root_directory(&ctx)
    .expect("failed to create engine root directory");
  engine
}

/// Build the application router with a specific JwtManager and engine, returning
/// the EventBus for test inspection (useful for SSE tests).
pub fn create_app_with_jwt_engine_and_event_bus(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
) -> (Router, Arc<EventBus>) {
  let prometheus_handle = try_initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let event_bus = Arc::new(EventBus::new());
  let cors_state = CorsState { default_origins: None, rules: vec![] };

  let router = create_app_with_all(auth_provider, jwt_manager, plugin_manager, rate_limiter, prometheus_handle, engine, event_bus.clone(), cors_state);
  (router, event_bus)
}

/// Create an engine backed by a temporary file (for tests).
pub fn create_temp_engine_for_tests() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
  let engine_file = temp_dir.path().join("test.aeordb");
  let engine_path = engine_file.to_str().expect("valid temp path");
  let engine = create_engine_for_storage(engine_path);
  (engine, temp_dir)
}
