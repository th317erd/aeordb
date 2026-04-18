use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TokenClaims;
use crate::engine::{
    is_root, load_cron_config, save_cron_config, validate_cron_expression,
    CronConfig, CronSchedule, RequestContext,
};
use crate::engine::system_store;
use crate::server::state::AppState;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReindexRequest {
    pub path: String,
}

#[derive(Deserialize)]
pub struct GcTaskRequest {
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Deserialize)]
pub struct UpdateCronRequest {
    pub enabled: Option<bool>,
    pub schedule: Option<String>,
    pub task_type: Option<String>,
    pub args: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_root(claims: &TokenClaims) -> Result<(), Response> {
    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| {
        (StatusCode::FORBIDDEN, Json(serde_json::json!({
            "error": "Invalid user ID"
        }))).into_response()
    })?;

    if !is_root(&user_id) {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({
            "error": "Only root user can manage tasks"
        }))).into_response());
    }

    Ok(())
}

fn require_task_queue(state: &AppState) -> Result<&std::sync::Arc<crate::engine::TaskQueue>, Response> {
    state.task_queue.as_ref().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
            "error": "Task queue not available"
        }))).into_response()
    })
}

// ---------------------------------------------------------------------------
// Task endpoints
// ---------------------------------------------------------------------------

/// GET /admin/tasks -- list all tasks with progress info.
pub async fn list_tasks(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.list_tasks() {
        Ok(tasks) => {
            let response: Vec<serde_json::Value> = tasks.iter().map(|task| {
                let mut json = serde_json::to_value(task).unwrap();
                if let Some(progress) = queue.get_progress(&task.id) {
                    json["progress"] = serde_json::json!(progress.progress);
                    json["eta_ms"] = serde_json::json!(progress.eta_ms);
                }
                json
            }).collect();
            (StatusCode::OK, Json(serde_json::json!(response))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to list tasks: {}", e)
            }))).into_response()
        }
    }
}

/// POST /admin/tasks/reindex -- enqueue a reindex task.
pub async fn trigger_reindex(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<ReindexRequest>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.enqueue("reindex", serde_json::json!({"path": body.path})) {
        Ok(record) => {
            (StatusCode::OK, Json(serde_json::json!({
                "id": record.id,
                "task_type": record.task_type,
                "status": record.status,
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to enqueue reindex: {}", e)
            }))).into_response()
        }
    }
}

/// POST /admin/tasks/gc -- enqueue a GC task.
pub async fn trigger_gc(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<GcTaskRequest>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.enqueue("gc", serde_json::json!({"dry_run": body.dry_run})) {
        Ok(record) => {
            (StatusCode::OK, Json(serde_json::json!({
                "id": record.id,
                "task_type": record.task_type,
                "status": record.status,
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to enqueue gc: {}", e)
            }))).into_response()
        }
    }
}

/// POST /admin/tasks/cleanup -- run expired token and magic link cleanup.
///
/// Returns the number of tokens and magic links cleaned up. This operation
/// is synchronous and runs inline (no task queue needed).
pub async fn trigger_cleanup(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
    match system_store::cleanup_expired_tokens(&state.engine, &ctx) {
        Ok((tokens, links)) => {
            (StatusCode::OK, Json(serde_json::json!({
                "tokens_cleaned": tokens,
                "links_cleaned": links,
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Cleanup failed: {}", e)
            }))).into_response()
        }
    }
}

/// GET /admin/tasks/{id} -- get a single task by ID.
pub async fn get_task(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.get_task(&id) {
        Ok(Some(task)) => {
            let mut json = serde_json::to_value(&task).unwrap();
            if let Some(progress) = queue.get_progress(&task.id) {
                json["progress"] = serde_json::json!(progress.progress);
                json["eta_ms"] = serde_json::json!(progress.eta_ms);
            }
            (StatusCode::OK, Json(json)).into_response()
        }
        Ok(None) => {
            (StatusCode::NOT_FOUND, Json(serde_json::json!({
                "error": format!("Task '{}' not found", id)
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to get task: {}", e)
            }))).into_response()
        }
    }
}

/// DELETE /admin/tasks/{id} -- cancel a task.
pub async fn cancel_task(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.cancel(&id) {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::json!({
                "id": id,
                "status": "cancelled",
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to cancel task: {}", e)
            }))).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Cron endpoints
// ---------------------------------------------------------------------------

/// GET /admin/cron -- list cron schedules.
pub async fn list_cron(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let schedules = load_cron_config(&state.engine);
    (StatusCode::OK, Json(serde_json::json!(schedules))).into_response()
}

/// POST /admin/cron -- create a new cron schedule.
pub async fn create_cron(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<CronSchedule>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    // Validate expression
    if let Err(msg) = validate_cron_expression(&body.schedule) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": format!("Invalid cron expression: {}", msg)
        }))).into_response();
    }

    let mut schedules = load_cron_config(&state.engine);

    // Check for duplicate ID
    if schedules.iter().any(|s| s.id == body.id) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({
            "error": format!("Cron schedule '{}' already exists", body.id)
        }))).into_response();
    }

    schedules.push(body.clone());
    let config = CronConfig { schedules };
    match save_cron_config(&state.engine, &config) {
        Ok(()) => {
            (StatusCode::CREATED, Json(serde_json::to_value(&body).unwrap())).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to save cron config: {}", e)
            }))).into_response()
        }
    }
}

/// DELETE /admin/cron/{id} -- delete a cron schedule.
pub async fn delete_cron(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let mut schedules = load_cron_config(&state.engine);
    let original_len = schedules.len();
    schedules.retain(|s| s.id != id);

    if schedules.len() == original_len {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": format!("Cron schedule '{}' not found", id)
        }))).into_response();
    }

    let config = CronConfig { schedules };
    match save_cron_config(&state.engine, &config) {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::json!({
                "id": id,
                "deleted": true,
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to save cron config: {}", e)
            }))).into_response()
        }
    }
}

/// PATCH /admin/cron/{id} -- update a cron schedule.
pub async fn update_cron(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCronRequest>,
) -> Response {
    if let Err(resp) = require_root(&claims) {
        return resp;
    }

    let mut schedules = load_cron_config(&state.engine);
    let schedule = match schedules.iter_mut().find(|s| s.id == id) {
        Some(s) => s,
        None => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({
                "error": format!("Cron schedule '{}' not found", id)
            }))).into_response();
        }
    };

    // Apply updates
    if let Some(enabled) = body.enabled {
        schedule.enabled = enabled;
    }
    if let Some(ref expression) = body.schedule {
        if let Err(msg) = validate_cron_expression(expression) {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": format!("Invalid cron expression: {}", msg)
            }))).into_response();
        }
        schedule.schedule = expression.clone();
    }
    if let Some(ref task_type) = body.task_type {
        schedule.task_type = task_type.clone();
    }
    if let Some(ref args) = body.args {
        schedule.args = args.clone();
    }

    let updated = schedule.clone();
    let config = CronConfig { schedules };
    match save_cron_config(&state.engine, &config) {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::to_value(&updated).unwrap())).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("Failed to save cron config: {}", e)
            }))).into_response()
        }
    }
}
