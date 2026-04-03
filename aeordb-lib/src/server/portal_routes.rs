use axum::extract::State;
use axum::response::{Html, IntoResponse, Json};
use axum::http::{header, StatusCode};
use super::state::AppState;
use crate::engine::storage_engine::DatabaseStats;

// Embed portal assets at compile time
const PORTAL_HTML: &str = include_str!("../portal/index.html");
const PORTAL_APP_MJS: &str = include_str!("../portal/app.mjs");
const PORTAL_DASHBOARD_MJS: &str = include_str!("../portal/dashboard.mjs");
const PORTAL_USERS_MJS: &str = include_str!("../portal/users.mjs");

/// Serve the main portal HTML page.
pub async fn portal_index() -> Html<&'static str> {
    Html(PORTAL_HTML)
}

/// Serve portal JS assets with correct content type.
pub async fn portal_asset(
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    let (content, content_type) = match filename.as_str() {
        "app.mjs" => (PORTAL_APP_MJS, "application/javascript; charset=utf-8"),
        "dashboard.mjs" => (PORTAL_DASHBOARD_MJS, "application/javascript; charset=utf-8"),
        "users.mjs" => (PORTAL_USERS_MJS, "application/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response(),
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], content).into_response()
}

/// Return database stats as JSON.
pub async fn get_stats(State(state): State<AppState>) -> Json<DatabaseStats> {
    let stats = state.engine.stats();
    Json(stats)
}
