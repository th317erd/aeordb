pub mod admin_routes;
pub mod api_key_self_service_routes;
pub mod backup_routes;
pub mod cluster_routes;
pub mod conflict_routes;
pub mod cors;
pub mod download_routes;
pub mod engine_routes;
pub mod gc_routes;
pub mod portal_routes;
pub mod responses;
pub mod routes;
pub mod sse_routes;
pub mod state;
pub mod task_routes;
pub mod upload_routes;
pub mod share_link_routes;
pub mod settings_routes;
pub mod share_routes;
pub mod symlink_routes;
pub mod sync_routes;
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
use crate::engine::{ApiKeyCache, DirectoryOps, EventBus, GroupCache, PeerManager, PermissionsCache, RequestContext, StorageEngine, TaskQueue};
use crate::logging::request_id_middleware;
use crate::metrics::http_metrics_layer::HttpMetricsLayer;
use crate::metrics::initialize_metrics;
use crate::plugins::PluginManager;
use state::AppState;

pub use cors::{CorsState, CorsRule, CorsConfig, build_cors_state, load_cors_config, parse_cors_origins};

/// Default cache TTL in seconds.
const DEFAULT_CACHE_TTL_SECONDS: u64 = 60;

// NOTE: The permission_middleware only checks /files/ routes for path-level
// CRUD permissions. The following routes are behind auth but have no path-level
// checks: /files/query, /blobs/*, /versions/*, /plugins/*, /system/events.
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
      let (provider, key) = match FileAuthProvider::from_identity_file(path) {
        Ok(result) => result,
        Err(error) => {
          eprintln!("Fatal: failed to create auth provider from identity file '{}': {}", path, error);
          std::process::exit(1);
        }
      };
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
  let prometheus_handle = initialize_metrics();
  let auth_provider: Arc<dyn AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  create_app_with_provider_and_metrics(auth_provider, jwt_manager, prometheus_handle, engine)
}

/// Build the application router with a specific JwtManager and engine (useful
/// for tests that need to reuse the same StorageEngine across rebuilds). No CORS.
pub fn create_app_with_jwt_and_engine(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
) -> Router {
  let prometheus_handle = initialize_metrics();
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
  let prometheus_handle = initialize_metrics();
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
  let prometheus_handle = initialize_metrics();
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
  let api_key_cache = Arc::new(ApiKeyCache::new(cache_ttl));
  let peer_manager = Arc::new(PeerManager::new());
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
    api_key_cache,
    task_queue,
    peer_manager,
    startup_time: chrono::Utc::now().timestamp_millis() as u64,
    startup_instant: std::time::Instant::now(),
    db_path: String::new(),
    rate_trackers: None,
  };

  // Routes with large body limits (file uploads: 10 GB)
  //
  // IMPORTANT: All /files/ routes that must NOT be captured by the
  // /files/{*path} wildcard live here, registered BEFORE the wildcard,
  // in the SAME router.  Putting them in a separate router and using
  // .merge() causes axum to match the wildcard instead of the specific
  // path (the wildcard wins for methods it already owns, e.g. GET/DELETE).
  let large_upload_routes = Router::new()
    .route(
      "/blobs/{hex_hash}",
      get(engine_routes::engine_get_by_hash),
    )
    // Files: query route (must be before /files/{*path} wildcard)
    .route("/files/query", post(engine_routes::query_endpoint))
    // Files: ZIP download route (must be before /files/{*path} wildcard)
    .route("/files/download", post(download_routes::download_zip))
    // Files: mkdir route (must be before /files/{*path} wildcard)
    .route("/files/mkdir", post(engine_routes::mkdir))
    // Files: share routes (must be before /files/{*path} wildcard)
    .route("/files/share", post(share_routes::share))
    .route("/files/shares", get(share_routes::list_shares).delete(share_routes::unshare))
    // Files: share-link routes (must be before /files/{*path} wildcard)
    .route("/files/share-link", post(share_link_routes::create_share_link))
    .route("/files/share-links", get(share_link_routes::list_share_links))
    .route("/files/share-links/{key_id}", delete(share_link_routes::revoke_share_link))
    // The wildcard MUST be last among /files/ routes
    .route(
      "/files/{*path}",
      put(engine_routes::engine_store_file)
        .get(engine_routes::engine_get)
        .delete(engine_routes::engine_delete_file)
        .head(engine_routes::engine_head)
        .patch(engine_routes::engine_rename),
    )
    .route("/blobs/chunks/{hash}", put(upload_routes::upload_chunk))
    .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024 * 1024)) // 10 GB
    .route_layer(from_fn_with_state(app_state.clone(), permission_middleware))
    .route_layer(from_fn_with_state(app_state.clone(), auth_middleware));

  // Routes with medium body limits (backup import: 10 MB)
  let medium_upload_routes = Router::new()
    .route("/versions/import", post(backup_routes::import_backup))
    .layer(axum::extract::DefaultBodyLimit::max(10 * 1024 * 1024)); // 10 MB

  // Routes that require authentication (default 1 MB limit)
  let protected_routes = Router::new()
    // Auth: API key self-service
    .route("/auth/keys", post(api_key_self_service_routes::create_own_key)
                        .get(api_key_self_service_routes::list_own_keys))
    .route("/auth/keys/{key_id}", delete(api_key_self_service_routes::revoke_own_key))
    .route("/auth/keys/users", get(api_key_self_service_routes::list_key_assignable_users))
    .route("/auth/keys/admin", post(routes::create_api_key).get(routes::list_api_keys))
    .route("/auth/keys/admin/{key_id}", delete(routes::revoke_api_key))
    // System: metrics, stats
    .route("/system/metrics", get(routes::metrics_endpoint))
    .route("/system/stats", get(portal_routes::get_stats))
    // System: user/group management
    .route("/system/users", post(admin_routes::create_user).get(admin_routes::list_users))
    .route(
      "/system/users/{user_id}",
      get(admin_routes::get_user)
        .patch(admin_routes::update_user)
        .delete(admin_routes::deactivate_user),
    )
    .route("/system/groups", post(admin_routes::create_group).get(admin_routes::list_groups))
    .route(
      "/system/groups/{name}",
      get(admin_routes::get_group)
        .patch(admin_routes::update_group)
        .delete(admin_routes::delete_group),
    )
    // Versions: export, diff, promote
    .route("/versions/export", post(backup_routes::export_backup))
    .route("/versions/diff", post(backup_routes::diff_backup))
    .route("/versions/promote", post(backup_routes::promote_head))
    // System: email configuration
    .route("/system/email-config", get(settings_routes::get_email_config).put(settings_routes::put_email_config))
    .route("/system/email-test", post(settings_routes::send_test_email))
    // System: GC
    .route("/system/gc", post(gc_routes::run_gc_endpoint))
    // System: repair (KV index rebuild)
    .route("/system/repair", post(engine_routes::repair_kv))
    // System: task & cron routes
    .route("/system/tasks", get(task_routes::list_tasks))
    .route("/system/tasks/reindex", post(task_routes::trigger_reindex))
    .route("/system/tasks/gc", post(task_routes::trigger_gc))
    .route("/system/tasks/cleanup", post(task_routes::trigger_cleanup))
    .route("/system/tasks/{id}", get(task_routes::get_task).delete(task_routes::cancel_task))
    .route("/system/cron", get(task_routes::list_cron).post(task_routes::create_cron))
    .route("/system/cron/{id}", delete(task_routes::delete_cron).patch(task_routes::update_cron))
    // Blobs: upload check, commit, and config (small payloads)
    .route("/blobs/check", post(upload_routes::upload_check))
    .route("/blobs/commit", post(upload_routes::upload_commit))
    .route("/blobs/config", get(upload_routes::upload_config))
    // System: SSE event stream
    .route("/system/events", get(sse_routes::event_stream))
    // NOTE: /files/query, /files/download, /files/mkdir, /files/share,
    // /files/shares, /files/share-link, /files/share-links are registered
    // in large_upload_routes (same router as /files/{*path} wildcard) to
    // prevent the wildcard from shadowing them after merge.
    // Versions: snapshot routes
    .route("/versions/snapshots", post(engine_routes::snapshot_create)
                                 .get(engine_routes::snapshot_list))
    .route("/versions/restore", post(engine_routes::snapshot_restore))
    .route("/versions/snapshots/{name}", delete(engine_routes::snapshot_delete))
    // Versions: fork routes
    .route("/versions/forks", post(engine_routes::fork_create)
                             .get(engine_routes::fork_list))
    .route("/versions/forks/{name}/promote", post(engine_routes::fork_promote))
    .route("/versions/forks/{name}", delete(engine_routes::fork_abandon))
    // Versions: file-level access routes
    .route("/versions/history/{*path}", get(version_file_routes::file_history))
    .route("/versions/restore/{*path}", post(version_file_routes::file_restore))
    // Sync: conflict management routes
    .route("/sync/conflicts", get(conflict_routes::list_conflicts))
    .route("/sync/conflicts/{*path}", get(conflict_routes::get_conflict))
    .route("/sync/resolve/{*path}", post(conflict_routes::resolve_conflict))
    .route("/sync/dismiss/{*path}", post(conflict_routes::dismiss_conflict))
    // Sync: cluster / replication routes
    .route("/sync/status", get(cluster_routes::cluster_status))
    .route("/sync/peers", post(cluster_routes::add_peer).get(cluster_routes::list_peers))
    .route("/sync/peers/{node_id}", delete(cluster_routes::remove_peer))
    .route("/sync/trigger", post(cluster_routes::trigger_sync))
    // Links: symlink routes
    .route("/links/{*path}", put(symlink_routes::create_symlink)
                            .get(symlink_routes::get_symlink)
                            .delete(symlink_routes::delete_symlink))
    // Plugin routes
    .route("/plugins/{name}", put(routes::deploy_plugin).delete(routes::remove_plugin))
    .route("/plugins/{name}/invoke", post(routes::invoke_plugin))
    .route("/plugins", get(routes::list_plugins))
    // Merge the large-upload and medium-upload routes into the protected group
    .merge(large_upload_routes)
    .merge(medium_upload_routes)
    .route_layer(from_fn_with_state(app_state.clone(), permission_middleware))
    .route_layer(from_fn_with_state(app_state.clone(), auth_middleware));

  // Public routes (no auth required)
  let public_routes = Router::new()
    .route("/system/health", get(routes::health_check))
    .route("/auth/token", post(routes::auth_token))
    .route("/auth/magic-link", post(routes::request_magic_link))
    .route("/auth/magic-link/verify", get(routes::verify_magic_link))
    .route("/auth/refresh", post(routes::refresh_token))
    // Portal (embedded dashboard UI)
    .route("/system/portal", get(portal_routes::portal_index))
    .route("/system/portal/", get(portal_routes::portal_index))
    .route("/system/portal/{filename}", get(portal_routes::portal_asset))
    .route("/system/portal/shared/{*path}", get(portal_routes::portal_shared_asset))
    // Sync routes (JWT auth, verified inside handler)
    .route("/sync/diff", post(sync_routes::sync_diff))
    .route("/sync/chunks", post(sync_routes::sync_chunks));

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


use crate::auth::middleware::auth_middleware;
use crate::auth::permission_middleware::permission_middleware;
use crate::engine::system_store;

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

  // Run system path migrations (idempotent — safe on every startup).
  system_store::migrate_system_paths(&engine)
    .expect("failed to run system path migration");

  engine
}

/// Build the application router with a specific JwtManager and engine, returning
/// the EventBus for test inspection (useful for SSE tests).
pub fn create_app_with_jwt_engine_and_event_bus(
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
) -> (Router, Arc<EventBus>) {
  let prometheus_handle = initialize_metrics();
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
