use axum::{
  extract::{Request, State},
  http::{Method, StatusCode},
  middleware::Next,
  response::{IntoResponse, Response},
  Json,
};
use uuid::Uuid;

use crate::auth::jwt::TokenClaims;
use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
use crate::server::responses::ErrorResponse;
use crate::server::state::AppState;

/// Special file names that map to Configure or Deploy operations.
const CONFIGURE_FILES: &[&str] = &[".config", ".permissions"];
const DEPLOY_FILES: &[&str] = &[".functions"];

/// Axum middleware that checks crudlify permissions on `/engine/` routes.
///
/// This runs AFTER `auth_middleware` (which has already validated the JWT
/// and inserted `TokenClaims` into request extensions).
///
/// Steps:
/// 1. Extract user_id from TokenClaims.
/// 2. Map the HTTP method + path to a CrudlifyOp.
/// 3. Call PermissionResolver::check_permission.
/// 4. If denied, return 403 Forbidden. Otherwise, continue.
pub async fn permission_middleware(
  State(state): State<AppState>,
  request: Request,
  next: Next,
) -> Response {
  // Only apply to /engine/ routes.
  let request_path = request.uri().path().to_string();
  if !request_path.starts_with("/engine/") {
    return next.run(request).await;
  }

  // Extract the engine sub-path (strip the "/engine/" prefix).
  let engine_path = &request_path["/engine/".len()..];

  // Extract claims from extensions (set by auth_middleware).
  let claims = match request.extensions().get::<TokenClaims>() {
    Some(claims) => claims.clone(),
    None => {
      // No claims means auth middleware didn't run or failed -- deny.
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Authentication required".to_string(),
        }),
      )
        .into_response();
    }
  };

  // Parse user_id from claims.sub.
  // If the sub is not a valid UUID, skip permission checking for backward
  // compatibility with legacy tokens that predate the UUID-based user system.
  let user_id = match Uuid::parse_str(&claims.sub) {
    Ok(user_id) => user_id,
    Err(_) => {
      tracing::debug!(
        sub = %claims.sub,
        "Skipping permission check: sub is not a valid UUID"
      );
      return next.run(request).await;
    }
  };

  // Determine the crudlify operation.
  let operation = http_to_crudlify(request.method(), engine_path, &state);

  // Check permission.
  let resolver = PermissionResolver::new(
    &state.engine,
    &state.group_cache,
    &state.permissions_cache,
  );

  match resolver.check_permission(&user_id, engine_path, operation) {
    Ok(true) => next.run(request).await,
    Ok(false) => {
      tracing::warn!(
        user_id = %user_id,
        path = %engine_path,
        operation = ?operation,
        "Permission denied"
      );
      (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse {
          error: "Permission denied".to_string(),
        }),
      )
        .into_response()
    }
    Err(error) => {
      tracing::error!(
        user_id = %user_id,
        path = %engine_path,
        "Permission check failed: {}",
        error
      );
      (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: "Permission check failed".to_string(),
        }),
      )
        .into_response()
    }
  }
}

/// Map an HTTP method and path to a CrudlifyOp.
///
/// - PUT to .config/.permissions -> Configure
/// - PUT to .functions -> Deploy
/// - PUT (new file) -> Create, PUT (existing file) -> Update
/// - GET on directory (ends with '/') -> List
/// - GET/HEAD -> Read
/// - DELETE -> Delete
/// - POST to /_invoke -> Invoke
pub fn http_to_crudlify(method: &Method, path: &str, state: &AppState) -> CrudlifyOp {
  // Check for special file names in the path.
  let file_name = path.rsplit('/').next().unwrap_or("");

  if *method == Method::PUT {
    // Configure operations.
    for special in CONFIGURE_FILES {
      if file_name == *special || path.contains(&format!("/{}/", special)) {
        return CrudlifyOp::Configure;
      }
    }

    // Deploy operations.
    for special in DEPLOY_FILES {
      if file_name == *special {
        return CrudlifyOp::Deploy;
      }
    }

    // Check if the file already exists to determine Create vs Update.
    let directory_ops = crate::engine::directory_ops::DirectoryOps::new(&state.engine);
    if directory_ops.exists(path).unwrap_or(false) {
      return CrudlifyOp::Update;
    }
    return CrudlifyOp::Create;
  }

  if *method == Method::POST {
    if path.contains("/_invoke") {
      return CrudlifyOp::Invoke;
    }
    // Default POST to Create.
    return CrudlifyOp::Create;
  }

  if *method == Method::GET {
    if path.ends_with('/') {
      return CrudlifyOp::List;
    }
    return CrudlifyOp::Read;
  }

  if *method == Method::HEAD {
    return CrudlifyOp::Read;
  }

  if *method == Method::DELETE {
    return CrudlifyOp::Delete;
  }

  // Fallback: treat unknown methods as Read.
  CrudlifyOp::Read
}
