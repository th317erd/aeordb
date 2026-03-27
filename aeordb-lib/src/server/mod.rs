pub mod responses;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post, put};
use tower_http::trace::TraceLayer;

use crate::auth::JwtManager;
use crate::auth::middleware::auth_middleware;
use crate::plugins::PluginManager;
use crate::storage::RedbStorage;
use state::AppState;

const SIGNING_KEY_CONFIG: &str = "jwt_signing_key";

/// Build the full application router with all routes and middleware.
/// Loads or generates the signing key from the storage config table.
pub fn create_app(storage: Arc<RedbStorage>) -> Router {
  let jwt_manager = load_or_create_jwt_manager(&storage);
  create_app_with_jwt(storage, Arc::new(jwt_manager))
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
pub fn create_app_with_jwt(storage: Arc<RedbStorage>, jwt_manager: Arc<JwtManager>) -> Router {
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));

  let app_state = AppState {
    storage,
    jwt_manager,
    plugin_manager,
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
    .route("/auth/token", post(routes::auth_token));

  public_routes
    .merge(protected_routes)
    .with_state(app_state)
    .layer(TraceLayer::new_for_http())
}
