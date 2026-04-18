use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;

use crate::auth::TokenClaims;
use crate::engine::{
    load_cron_config, save_cron_config, validate_cron_expression,
    CronConfig, CronSchedule, RequestContext,
};
use crate::engine::system_store;
use crate::server::responses::{ErrorResponse, error_codes, require_root};
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

fn require_task_queue(state: &AppState) -> Result<&std::sync::Arc<crate::engine::TaskQueue>, Response> {
    state.task_queue.as_ref().ok_or_else(|| {
        ErrorResponse::new("Task queue not available")
            .with_code(error_codes::SERVICE_UNAVAILABLE)
            .with_status(StatusCode::SERVICE_UNAVAILABLE)
            .into_response()
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
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.list_tasks() {
        Ok(tasks) => {
            let response: Vec<serde_json::Value> = tasks.iter().filter_map(|task| {
                let mut json = serde_json::to_value(task).ok()?;
                if let Some(progress) = queue.get_progress(&task.id) {
                    json["progress"] = serde_json::json!(progress.progress);
                    json["eta_ms"] = serde_json::json!(progress.eta_ms);
                }
                Some(json)
            }).collect();
            (StatusCode::OK, Json(serde_json::json!({"items": response}))).into_response()
        }
        Err(e) => {
            ErrorResponse::new(format!("Failed to list tasks: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// POST /admin/tasks/reindex -- enqueue a reindex task.
pub async fn trigger_reindex(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<ReindexRequest>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

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
            ErrorResponse::new(format!("Failed to enqueue reindex: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// POST /admin/tasks/gc -- enqueue a GC task.
pub async fn trigger_gc(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<GcTaskRequest>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

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
            ErrorResponse::new(format!("Failed to enqueue gc: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
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
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
    match system_store::cleanup_expired_tokens(&state.engine, &ctx) {
        Ok((tokens, links)) => {
            (StatusCode::OK, Json(serde_json::json!({
                "tokens_cleaned": tokens,
                "links_cleaned": links,
            }))).into_response()
        }
        Err(e) => {
            ErrorResponse::new(format!("Cleanup failed: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// GET /admin/tasks/{id} -- get a single task by ID.
pub async fn get_task(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let queue = match require_task_queue(&state) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    match queue.get_task(&id) {
        Ok(Some(task)) => {
            match serde_json::to_value(&task) {
                Ok(mut json) => {
                    if let Some(progress) = queue.get_progress(&task.id) {
                        json["progress"] = serde_json::json!(progress.progress);
                        json["eta_ms"] = serde_json::json!(progress.eta_ms);
                    }
                    (StatusCode::OK, Json(json)).into_response()
                }
                Err(e) => ErrorResponse::new(format!("Failed to serialize task: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response(),
            }
        }
        Ok(None) => {
            ErrorResponse::new(format!("Task '{}' not found", id))
                .with_status(StatusCode::NOT_FOUND)
                .into_response()
        }
        Err(e) => {
            ErrorResponse::new(format!("Failed to get task: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// DELETE /admin/tasks/{id} -- cancel a task.
pub async fn cancel_task(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

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
            ErrorResponse::new(format!("Failed to cancel task: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
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
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let schedules = load_cron_config(&state.engine);
    (StatusCode::OK, Json(serde_json::json!({"items": schedules}))).into_response()
}

/// POST /admin/cron -- create a new cron schedule.
pub async fn create_cron(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<CronSchedule>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    // Validate expression
    if let Err(msg) = validate_cron_expression(&body.schedule) {
        return ErrorResponse::new(format!("Invalid cron expression: {}", msg))
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let mut schedules = load_cron_config(&state.engine);

    // Check for duplicate ID
    if schedules.iter().any(|s| s.id == body.id) {
        return ErrorResponse::new(format!("Cron schedule '{}' already exists", body.id))
            .with_status(StatusCode::CONFLICT)
            .into_response();
    }

    schedules.push(body.clone());
    let config = CronConfig { schedules };
    match save_cron_config(&state.engine, &config) {
        Ok(()) => {
            match serde_json::to_value(&body) {
                Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
                Err(e) => ErrorResponse::new(format!("Failed to serialize cron schedule: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response(),
            }
        }
        Err(e) => {
            ErrorResponse::new(format!("Failed to save cron config: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// DELETE /admin/cron/{id} -- delete a cron schedule.
pub async fn delete_cron(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(id): Path<String>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let mut schedules = load_cron_config(&state.engine);
    let original_len = schedules.len();
    schedules.retain(|s| s.id != id);

    if schedules.len() == original_len {
        return ErrorResponse::new(format!("Cron schedule '{}' not found", id))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
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
            ErrorResponse::new(format!("Failed to save cron config: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
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
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let mut schedules = load_cron_config(&state.engine);
    let schedule = match schedules.iter_mut().find(|s| s.id == id) {
        Some(s) => s,
        None => {
            return ErrorResponse::new(format!("Cron schedule '{}' not found", id))
                .with_status(StatusCode::NOT_FOUND)
                .into_response();
        }
    };

    // Apply updates
    if let Some(enabled) = body.enabled {
        schedule.enabled = enabled;
    }
    if let Some(ref expression) = body.schedule {
        if let Err(msg) = validate_cron_expression(expression) {
            return ErrorResponse::new(format!("Invalid cron expression: {}", msg))
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
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
            match serde_json::to_value(&updated) {
                Ok(value) => (StatusCode::OK, Json(value)).into_response(),
                Err(e) => ErrorResponse::new(format!("Failed to serialize cron schedule: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response(),
            }
        }
        Err(e) => {
            ErrorResponse::new(format!("Failed to save cron config: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
