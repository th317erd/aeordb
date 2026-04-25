use axum::{
  extract::{Request, State},
  http::{Method, StatusCode},
  middleware::Next,
  response::{IntoResponse, Response},
  Json,
};
use uuid::Uuid;

use crate::auth::jwt::TokenClaims;
use crate::engine::api_key_rules::{match_rules, check_operation_permitted, operation_to_flag_char, KeyRule};
use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
use crate::server::responses::ErrorResponse;
use crate::server::state::AppState;

/// Extension type for passing active API key rules to downstream handlers.
/// When present, handlers should filter listings and query results to exclude
/// entries the key cannot access.
#[derive(Clone, Debug)]
pub struct ActiveKeyRules(pub Vec<KeyRule>);

/// Special file names that map to Configure or Deploy operations.
const CONFIGURE_FILES: &[&str] = &[".config", ".permissions"];
const DEPLOY_FILES: &[&str] = &[".functions"];

/// Axum middleware that checks crudlify permissions on `/files/` routes.
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
  mut request: Request,
  next: Next,
) -> Response {
  let request_path = request.uri().path().to_string();
  let is_files_route = request_path.starts_with("/files/") && request_path != "/files/query";

  // For non-files routes, we still need to load key rules for downstream filtering
  // (e.g. /files/query endpoint filters results by key rules). But we skip the path-level
  // permission checks that are files-specific.
  if !is_files_route {
    // Load and insert key rules for downstream handlers if a scoped key is present.
    if let Some(ref key_id) = request.extensions().get::<TokenClaims>().and_then(|c| c.key_id.clone()) {
      if let Ok(Some(key_record)) = state.api_key_cache.get_key(key_id, &state.engine) {
        if !key_record.is_revoked && key_record.expires_at > chrono::Utc::now().timestamp_millis() {
          if !key_record.rules.is_empty() {
            request.extensions_mut().insert(ActiveKeyRules(key_record.rules.clone()));
          }
        }
      }
    }
    return next.run(request).await;
  }

  // Extract the files sub-path (strip the "/files/" prefix).
  let engine_path = &request_path["/files/".len()..];

  // Extract claims from extensions (set by auth_middleware).
  let claims = match request.extensions().get::<TokenClaims>() {
    Some(claims) => claims.clone(),
    None => {
      // No claims means auth middleware didn't run or failed -- deny.
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Authentication required".to_string(),
          code: None,
        }),
      )
        .into_response();
    }
  };

  // Parse user_id from claims.sub.
  // Share keys use "share:<id>" as sub — they skip the permission resolver.
  // Normal users must have UUID identities.
  let is_share_key = claims.sub.starts_with("share:");
  let user_id = if is_share_key {
    None
  } else {
    match Uuid::parse_str(&claims.sub) {
      Ok(user_id) => Some(user_id),
      Err(_) => {
        tracing::warn!(
          sub = %claims.sub,
          "Rejecting request: sub is not a valid UUID"
        );
        return (
          StatusCode::FORBIDDEN,
          Json(ErrorResponse {
            error: "Invalid user identity".to_string(),
            code: None,
          }),
        )
          .into_response();
      }
    }
  };

  // Determine the crudlify operation (needed for both key enforcement and permission check).
  let operation = http_to_crudlify(request.method(), engine_path, &state);

  // --- API Key scope enforcement ---
  // If the JWT was issued from a scoped API key, enforce the key's rules.
  // Denied by key rules = 404 (not 403) — the resource doesn't exist for this key.
  if let Some(ref key_id) = claims.key_id {
    let key_record = match state.api_key_cache.get_key(key_id, &state.engine) {
      Ok(Some(record)) => record,
      Ok(None) => {
        // Key not found in DB — token is stale
        return (
          StatusCode::UNAUTHORIZED,
          Json(ErrorResponse {
            error: "API key not found".to_string(),
            code: None,
          }),
        )
          .into_response();
      }
      Err(error) => {
        tracing::error!("Failed to load API key {}: {}", key_id, error);
        return (
          StatusCode::INTERNAL_SERVER_ERROR,
          Json(ErrorResponse {
            error: "Failed to verify API key".to_string(),
            code: None,
          }),
        )
          .into_response();
      }
    };

    // Check if key is revoked.
    if key_record.is_revoked {
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "API key has been revoked".to_string(),
          code: None,
        }),
      )
        .into_response();
    }

    // Check if key is expired.
    let now_millis = chrono::Utc::now().timestamp_millis();
    if key_record.expires_at <= now_millis {
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "API key expired".to_string(),
          code: None,
        }),
      )
        .into_response();
    }

    // If key has rules, enforce them.
    if !key_record.rules.is_empty() {
      let flag_char = operation_to_flag_char(&operation);
      let match_path = format!("/{}", engine_path);

      match match_rules(&key_record.rules, &match_path) {
        Some(rule) => {
          if !check_operation_permitted(&rule.permitted, flag_char) {
            // Operation not permitted by key — return 404 (not 403!).
            return (
              StatusCode::NOT_FOUND,
              Json(serde_json::json!({"error": format!("Not found: {}", engine_path)})),
            )
              .into_response();
          }
        }
        None => {
          // No rule matches — path doesn't exist for this key.
          return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Not found: {}", engine_path)})),
          )
            .into_response();
        }
      }
    }
    // Insert key rules into request extensions for downstream handler filtering.
    // Handlers use Option<Extension<ActiveKeyRules>> to detect and filter listings/queries.
    if !key_record.rules.is_empty() {
      request.extensions_mut().insert(ActiveKeyRules(key_record.rules.clone()));
    }
    // Empty rules = full pass-through, no extension inserted.
  }

  // For share keys, the API key rules are the sole permission authority.
  // Skip the user/group permission resolver entirely.
  if is_share_key {
    return next.run(request).await;
  }

  // Check permission (normal user flow).
  let resolver = PermissionResolver::new(
    &state.engine,
    &state.group_cache,
    &state.permissions_cache,
  );

  match resolver.check_permission(&user_id.unwrap(), engine_path, operation) {
    Ok(true) => next.run(request).await,
    Ok(false) => {
      tracing::warn!(
        user_id = %user_id.unwrap(),
        path = %engine_path,
        operation = ?operation,
        "Permission denied"
      );
      (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse {
          error: "Permission denied".to_string(),
          code: None,
        }),
      )
        .into_response()
    }
    Err(error) => {
      tracing::error!(
        user_id = %user_id.unwrap(),
        path = %engine_path,
        "Permission check failed: {}",
        error
      );
      (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
          error: "Permission check failed".to_string(),
          code: None,
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
/// - POST to /plugins/{name}/invoke -> Invoke
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
    if path.ends_with("/invoke") && path.starts_with("plugins/") {
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
