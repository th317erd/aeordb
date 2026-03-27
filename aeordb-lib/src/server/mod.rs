pub mod responses;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post};
use tower_http::trace::TraceLayer;

use crate::auth::JwtManager;
use crate::auth::middleware::auth_middleware;
use crate::storage::RedbStorage;
use state::AppState;

/// Build the full application router with all routes and middleware.
pub fn create_app(storage: Arc<RedbStorage>) -> Router {
  let jwt_manager = Arc::new(JwtManager::generate());
  create_app_with_jwt(storage, jwt_manager)
}

/// Build the application router with a specific JwtManager (useful for tests).
pub fn create_app_with_jwt(storage: Arc<RedbStorage>, jwt_manager: Arc<JwtManager>) -> Router {
  let app_state = AppState {
    storage,
    jwt_manager,
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
