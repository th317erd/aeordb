pub mod responses;
pub mod routes;
pub mod state;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::storage::RedbStorage;
use state::AppState;

/// Build the full application router with all routes and middleware.
pub fn create_app(storage: Arc<RedbStorage>) -> Router {
  let app_state = AppState { storage };

  Router::new()
    .route("/admin/health", get(routes::health_check))
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
    .with_state(app_state)
    .layer(TraceLayer::new_for_http())
}
