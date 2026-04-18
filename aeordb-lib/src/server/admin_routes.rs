use axum::{
  Extension,
  extract::{Path, State},
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::{ErrorResponse, GroupResponse, UserResponse, require_root};
use super::state::AppState;
use crate::engine::{Group, RequestContext, User};
use crate::engine::user::SAFE_QUERY_FIELDS;
use crate::engine::system_store;
use crate::auth::TokenClaims;

// ---------------------------------------------------------------------------
// User endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
  pub username: String,
  pub email: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
  pub username: Option<String>,
  pub email: Option<String>,
  pub is_active: Option<bool>,
}

/// POST /admin/users -- create a new user.
pub async fn create_user(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateUserRequest>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let user = User::new(&payload.username, payload.email.as_deref());

  if let Err(error) = system_store::store_user(&state.engine, &ctx, &user) {
    tracing::error!("Failed to create user: {}", error);
    return ErrorResponse::new(format!("Failed to create user: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (StatusCode::CREATED, Json(UserResponse::from(&user))).into_response()
}

/// GET /admin/users -- list all users.
pub async fn list_users(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  match system_store::list_users(&state.engine) {
    Ok(users) => {
      let responses: Vec<UserResponse> = users.iter().map(UserResponse::from).collect();
      (StatusCode::OK, Json(responses)).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list users: {}", error);
      ErrorResponse::new(format!("Failed to list users: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /admin/users/{user_id} -- get a single user.
pub async fn get_user(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(user_id_string): Path<String>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let user_id = match Uuid::parse_str(&user_id_string) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid user_id: {}", user_id_string))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  match system_store::get_user(&state.engine, &user_id) {
    Ok(Some(user)) => (StatusCode::OK, Json(UserResponse::from(&user))).into_response(),
    Ok(None) => {
      ErrorResponse::new(format!("User not found: {}", user_id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to get user: {}", error);
      ErrorResponse::new(format!("Failed to get user: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// PATCH /admin/users/{user_id} -- update a user.
pub async fn update_user(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(user_id_string): Path<String>,
  Json(payload): Json<UpdateUserRequest>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let user_id = match Uuid::parse_str(&user_id_string) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid user_id: {}", user_id_string))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let mut user = match system_store::get_user(&state.engine, &user_id) {
    Ok(Some(user)) => user,
    Ok(None) => {
      return ErrorResponse::new(format!("User not found: {}", user_id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to get user for update: {}", error);
      return ErrorResponse::new(format!("Failed to get user: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  if let Some(ref username) = payload.username {
    user.username = username.clone();
  }
  if let Some(ref email) = payload.email {
    user.email = Some(email.clone());
  }
  if let Some(is_active) = payload.is_active {
    user.is_active = is_active;
  }
  user.updated_at = chrono::Utc::now().timestamp_millis();

  if let Err(error) = system_store::update_user(&state.engine, &ctx, &user) {
    tracing::error!("Failed to update user: {}", error);
    return ErrorResponse::new(format!("Failed to update user: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (StatusCode::OK, Json(UserResponse::from(&user))).into_response()
}

/// DELETE /admin/users/{user_id} -- deactivate a user (soft delete).
pub async fn deactivate_user(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(user_id_string): Path<String>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let user_id = match Uuid::parse_str(&user_id_string) {
    Ok(id) => id,
    Err(_) => {
      return ErrorResponse::new(format!("Invalid user_id: {}", user_id_string))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let mut user = match system_store::get_user(&state.engine, &user_id) {
    Ok(Some(user)) => user,
    Ok(None) => {
      return ErrorResponse::new(format!("User not found: {}", user_id))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to get user for deactivation: {}", error);
      return ErrorResponse::new(format!("Failed to get user: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  user.is_active = false;
  user.updated_at = chrono::Utc::now().timestamp_millis();

  if let Err(error) = system_store::update_user(&state.engine, &ctx, &user) {
    tracing::error!("Failed to deactivate user: {}", error);
    return ErrorResponse::new(format!("Failed to deactivate user: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "deactivated": true,
      "user_id": user_id.to_string(),
    })),
  )
    .into_response()
}

// ---------------------------------------------------------------------------
// Group endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateGroupRequest {
  pub name: String,
  pub default_allow: String,
  pub default_deny: String,
  pub query_field: String,
  pub query_operator: String,
  pub query_value: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateGroupRequest {
  pub default_allow: Option<String>,
  pub default_deny: Option<String>,
  pub query_field: Option<String>,
  pub query_operator: Option<String>,
  pub query_value: Option<String>,
}

/// POST /admin/groups -- create a new group.
pub async fn create_group(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateGroupRequest>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let group = match Group::new(
    &payload.name,
    &payload.default_allow,
    &payload.default_deny,
    &payload.query_field,
    &payload.query_operator,
    &payload.query_value,
  ) {
    Ok(group) => group,
    Err(error) => {
      return ErrorResponse::new(format!("Invalid group: {}", error))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  if let Err(error) = system_store::store_group(&state.engine, &ctx, &group) {
    tracing::error!("Failed to create group: {}", error);
    return ErrorResponse::new(format!("Failed to create group: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (StatusCode::CREATED, Json(GroupResponse::from(&group))).into_response()
}

/// GET /admin/groups -- list all groups.
pub async fn list_groups(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  match system_store::list_groups(&state.engine) {
    Ok(groups) => {
      let responses: Vec<GroupResponse> = groups.iter().map(GroupResponse::from).collect();
      (StatusCode::OK, Json(responses)).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to list groups: {}", error);
      ErrorResponse::new(format!("Failed to list groups: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /admin/groups/{name} -- get a single group.
pub async fn get_group(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  match system_store::get_group(&state.engine, &name) {
    Ok(Some(group)) => (StatusCode::OK, Json(GroupResponse::from(&group))).into_response(),
    Ok(None) => {
      ErrorResponse::new(format!("Group not found: {}", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to get group: {}", error);
      ErrorResponse::new(format!("Failed to get group: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// PATCH /admin/groups/{name} -- update a group.
pub async fn update_group(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
  Json(payload): Json<UpdateGroupRequest>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let mut group = match system_store::get_group(&state.engine, &name) {
    Ok(Some(group)) => group,
    Ok(None) => {
      return ErrorResponse::new(format!("Group not found: {}", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to get group for update: {}", error);
      return ErrorResponse::new(format!("Failed to get group: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  };

  // If query_field is being changed, validate it against the safe whitelist.
  if let Some(ref query_field) = payload.query_field {
    if !SAFE_QUERY_FIELDS.contains(&query_field.as_str()) {
      return ErrorResponse::new(format!(
        "Unsafe query field: '{}' is not allowed in group queries",
        query_field,
      ))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
    group.query_field = query_field.clone();
  }

  if let Some(ref default_allow) = payload.default_allow {
    group.default_allow = default_allow.clone();
  }
  if let Some(ref default_deny) = payload.default_deny {
    group.default_deny = default_deny.clone();
  }
  if let Some(ref query_operator) = payload.query_operator {
    group.query_operator = query_operator.clone();
  }
  if let Some(ref query_value) = payload.query_value {
    group.query_value = query_value.clone();
  }
  group.updated_at = chrono::Utc::now().timestamp_millis();

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  if let Err(error) = system_store::update_group(&state.engine, &ctx, &group) {
    tracing::error!("Failed to update group: {}", error);
    return ErrorResponse::new(format!("Failed to update group: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (StatusCode::OK, Json(GroupResponse::from(&group))).into_response()
}

/// DELETE /admin/groups/{name} -- delete a group.
pub async fn delete_group(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  // Check if the group exists first.
  match system_store::get_group(&state.engine, &name) {
    Ok(Some(_)) => {}
    Ok(None) => {
      return ErrorResponse::new(format!("Group not found: {}", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
    Err(error) => {
      tracing::error!("Failed to check group for deletion: {}", error);
      return ErrorResponse::new(format!("Failed to delete group: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  if let Err(error) = system_store::delete_group(&state.engine, &ctx, &name) {
    tracing::error!("Failed to delete group: {}", error);
    return ErrorResponse::new(format!("Failed to delete group: {}", error))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response();
  }

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "deleted": true,
      "name": name,
    })),
  )
    .into_response()
}
