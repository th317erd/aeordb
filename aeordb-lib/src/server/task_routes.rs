use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;

use crate::auth::TokenClaims;
use crate::engine::{
  create_cron_schedule, delete_cron_schedule, load_cron_config, update_cron_schedule, validate_cron_expression, CronSchedule,
  CronScheduleUpdate, EngineError, RequestContext,
};
use crate::engine::system_store;
use crate::server::responses::{ErrorResponse, engine_error_response, error_codes, require_root};
use crate::server::state::AppState;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReindexRequest {
  pub path: String,
  #[serde(default)]
  pub force: bool,
  #[serde(default)]
  pub metadata_only: bool,
  pub index_flush_writes: Option<usize>,
  pub index_flush_ms: Option<u64>,
}

#[derive(Deserialize)]
pub struct GcTaskRequest {
  #[serde(default)]
  pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_task_queue(state: &AppState) -> Result<&std::sync::Arc<crate::engine::TaskQueue>, Response> {
  state.task_queue.as_ref().ok_or_else(|| {
    ErrorResponse::new("Task queue not available. The task system may not be enabled in this configuration")
      .with_code(error_codes::SERVICE_UNAVAILABLE)
      .with_status(StatusCode::SERVICE_UNAVAILABLE)
      .into_response()
  })
}

// ---------------------------------------------------------------------------
// Task endpoints
// ---------------------------------------------------------------------------

/// GET /admin/tasks -- list all tasks with progress info.
pub async fn list_tasks(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
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
      let response: Vec<serde_json::Value> = tasks
        .iter()
        .filter_map(|task| {
          let mut json = serde_json::to_value(task).ok()?;
          if let Some(progress) = queue.get_progress(&task.id) {
            json["progress"] = serde_json::json!(progress.progress);
            json["eta_ms"] = serde_json::json!(progress.eta_ms);
          }
          Some(json)
        })
        .collect();
      (StatusCode::OK, Json(serde_json::json!({"items": response}))).into_response()
    }
    Err(e) => ErrorResponse::new(format!("Failed to list tasks: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
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

  let mut args = serde_json::json!({
      "path": body.path,
      "force": body.force,
      "metadata_only": body.metadata_only,
  });
  if let Some(index_flush_writes) = body.index_flush_writes {
    args["index_flush_writes"] = serde_json::json!(index_flush_writes);
  }
  if let Some(index_flush_ms) = body.index_flush_ms {
    args["index_flush_ms"] = serde_json::json!(index_flush_ms);
  }

  match queue.enqueue("reindex", args) {
    Ok(record) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "id": record.id,
          "task_type": record.task_type,
          "status": record.status,
      })),
    )
      .into_response(),
    Err(e) => {
      ErrorResponse::new(format!("Failed to enqueue reindex: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
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
    Ok(record) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "id": record.id,
          "task_type": record.task_type,
          "status": record.status,
      })),
    )
      .into_response(),
    Err(e) => ErrorResponse::new(format!("Failed to enqueue gc: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  }
}

/// POST /admin/tasks/cleanup -- run expired token and magic link cleanup.
///
/// Returns the number of tokens and magic links cleaned up. This operation
/// is synchronous and runs inline (no task queue needed).
pub async fn trigger_cleanup(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  match system_store::cleanup_expired_tokens(&state.engine, &ctx) {
    Ok((tokens, links)) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "tokens_cleaned": tokens,
          "links_cleaned": links,
      })),
    )
      .into_response(),
    Err(error @ EngineError::PartialOperation { .. }) => ErrorResponse::new(format!("Cleanup failed: {error}"))
      .with_code(error_codes::INTERNAL_ERROR)
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response(),
    Err(error) => engine_error_response("Cleanup failed", &error),
  }
}

/// GET /admin/tasks/{id} -- get a single task by ID.
pub async fn get_task(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>, Path(id): Path<String>) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let queue = match require_task_queue(&state) {
    Ok(q) => q,
    Err(resp) => return resp,
  };

  match queue.get_task(&id) {
    Ok(Some(task)) => match serde_json::to_value(&task) {
      Ok(mut json) => {
        if let Some(progress) = queue.get_progress(&task.id) {
          json["progress"] = serde_json::json!(progress.progress);
          json["eta_ms"] = serde_json::json!(progress.eta_ms);
        }
        (StatusCode::OK, Json(json)).into_response()
      }
      Err(e) => {
        ErrorResponse::new(format!("Failed to serialize task: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
      }
    },
    Ok(None) => ErrorResponse::new(format!("Task '{}' not found. Use GET /admin/tasks to list all tasks", id))
      .with_status(StatusCode::NOT_FOUND)
      .into_response(),
    Err(e) => ErrorResponse::new(format!("Failed to get task: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  }
}

/// DELETE /admin/tasks/{id} -- cancel a task.
pub async fn cancel_task(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>, Path(id): Path<String>) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let queue = match require_task_queue(&state) {
    Ok(q) => q,
    Err(resp) => return resp,
  };

  match queue.cancel(&id) {
    Ok(()) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "id": id,
          "status": "cancelled",
      })),
    )
      .into_response(),
    Err(e) => ErrorResponse::new(format!("Failed to cancel task: {}", e)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
  }
}

// ---------------------------------------------------------------------------
// Cron endpoints
// ---------------------------------------------------------------------------

/// GET /system/cron -- list cron schedules.
pub async fn list_cron(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  match load_cron_config(&state.engine) {
    Ok(schedules) => (StatusCode::OK, Json(serde_json::json!({"items": schedules}))).into_response(),
    Err(error) => {
      ErrorResponse::new(format!("Failed to load cron config: {error}")).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

/// POST /system/cron -- create a new cron schedule.
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
    return ErrorResponse::new(format!("Invalid cron expression: {}", msg)).with_status(StatusCode::BAD_REQUEST).into_response();
  }

  match create_cron_schedule(&state.engine, body.clone()) {
    Ok(created) => match serde_json::to_value(&created) {
      Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
      Err(e) => ErrorResponse::new(format!("Failed to serialize cron schedule: {}", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response(),
    },
    Err(crate::engine::EngineError::AlreadyExists(_)) => ErrorResponse::new(format!(
      "Cron schedule '{}' already exists. Use PATCH /system/cron/{} to update it, or choose a different ID",
      body.id, body.id
    ))
    .with_status(StatusCode::CONFLICT)
    .into_response(),
    Err(error) => {
      ErrorResponse::new(format!("Failed to mutate cron config: {error}")).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

/// DELETE /system/cron/{id} -- delete a cron schedule.
pub async fn delete_cron(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>, Path(id): Path<String>) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  match delete_cron_schedule(&state.engine, &id) {
    Ok(()) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "id": id,
          "deleted": true,
      })),
    )
      .into_response(),
    Err(crate::engine::EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Cron schedule '{}' not found. Use GET /system/cron to list all schedules", id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      ErrorResponse::new(format!("Failed to mutate cron config: {error}")).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

/// PATCH /system/cron/{id} -- update a cron schedule.
pub async fn update_cron(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(id): Path<String>,
  Json(body): Json<CronScheduleUpdate>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  if let Some(ref expression) = body.schedule {
    if let Err(msg) = validate_cron_expression(expression) {
      return ErrorResponse::new(format!("Invalid cron expression: {}", msg)).with_status(StatusCode::BAD_REQUEST).into_response();
    }
  }

  match update_cron_schedule(&state.engine, &id, body) {
    Ok(updated) => match serde_json::to_value(&updated) {
      Ok(value) => (StatusCode::OK, Json(value)).into_response(),
      Err(e) => ErrorResponse::new(format!("Failed to serialize cron schedule: {}", e))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response(),
    },
    Err(crate::engine::EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Cron schedule '{}' not found. Use GET /system/cron to list all schedules", id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      ErrorResponse::new(format!("Failed to mutate cron config: {error}")).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}
