use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::errors::{EngineError, EngineResult};

/// The root user identity. This is the nil UUID and is hardcoded in the engine,
/// not stored as a database entity. Root bypasses all permission checks.
pub const ROOT_USER_ID: Uuid = Uuid::nil();

/// Immutable/admin-only fields that are safe for group query evaluation.
/// User-mutable fields like `username` and `email` are excluded to prevent
/// privilege escalation via self-modification.
pub const SAFE_QUERY_FIELDS: &[&str] = &["user_id", "created_at", "updated_at", "is_active"];

/// A user entity in the aeordb identity system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
  pub user_id: Uuid,
  pub username: String,
  pub email: Option<String>,
  pub is_active: bool,
  pub created_at: i64,
  pub updated_at: i64,
}

impl User {
  /// Create a new user with an auto-generated UUID v4, active by default.
  pub fn new(username: &str, email: Option<&str>) -> Self {
    let now = chrono::Utc::now().timestamp_millis();
    User {
      user_id: Uuid::new_v4(),
      username: username.to_string(),
      email: email.map(|e| e.to_string()),
      is_active: true,
      created_at: now,
      updated_at: now,
    }
  }

  /// Serialize this user to JSON bytes.
  pub fn serialize(&self) -> Vec<u8> {
    serde_json::to_vec(self).expect("User serialization should never fail")
  }

  /// Deserialize a user from JSON bytes.
  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    serde_json::from_slice(data)
      .map_err(|error| EngineError::JsonParseError(format!("Failed to deserialize User: {}", error)))
  }

  /// Return a string representation of any field (for group query evaluation).
  pub fn get_field(&self, field: &str) -> String {
    match field {
      "user_id" => self.user_id.to_string(),
      "username" => self.username.clone(),
      "email" => self.email.clone().unwrap_or_default(),
      "is_active" => self.is_active.to_string(),
      "created_at" => self.created_at.to_string(),
      "updated_at" => self.updated_at.to_string(),
      _ => String::new(),
    }
  }
}

/// SECURITY: Validates that a user_id is not the reserved nil UUID (root).
/// This MUST be called before storing any user or API key to prevent
/// privilege escalation. The only exception is `store_api_key_for_bootstrap`.
pub fn validate_user_id(user_id: &Uuid) -> EngineResult<()> {
  if *user_id == ROOT_USER_ID {
    return Err(EngineError::ReservedUserId);
  }
  Ok(())
}

/// Check if a user_id is the root identity (nil UUID).
pub fn is_root(user_id: &Uuid) -> bool {
  *user_id == ROOT_USER_ID
}
