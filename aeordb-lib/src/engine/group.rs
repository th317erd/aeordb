use serde::{Deserialize, Serialize};

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::user::{SAFE_QUERY_FIELDS, User};

/// A query-based group in the aeordb identity system.
/// All groups are query groups -- static membership is expressed via
/// `user_id IN (...)` syntax.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
  pub name: String,
  pub default_allow: String,
  pub default_deny: String,
  pub query_field: String,
  pub query_operator: String,
  pub query_value: String,
  pub created_at: i64,
  pub updated_at: i64,
}

impl Group {
  /// Create a new group, validating that the query_field is in the safe whitelist.
  pub fn new(
    name: &str,
    default_allow: &str,
    default_deny: &str,
    query_field: &str,
    query_operator: &str,
    query_value: &str,
  ) -> EngineResult<Self> {
    if !SAFE_QUERY_FIELDS.contains(&query_field) {
      return Err(EngineError::UnsafeQueryField(query_field.to_string()));
    }

    let now = chrono::Utc::now().timestamp_millis();
    Ok(Group {
      name: name.to_string(),
      default_allow: default_allow.to_string(),
      default_deny: default_deny.to_string(),
      query_field: query_field.to_string(),
      query_operator: query_operator.to_string(),
      query_value: query_value.to_string(),
      created_at: now,
      updated_at: now,
    })
  }

  /// Serialize this group to JSON bytes.
  pub fn serialize(&self) -> Vec<u8> {
    serde_json::to_vec(self).expect("Group serialization should never fail")
  }

  /// Deserialize a group from JSON bytes.
  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    serde_json::from_slice(data)
      .map_err(|error| EngineError::JsonParseError(format!("Failed to deserialize Group: {}", error)))
  }

  /// Evaluate whether a user is a member of this group by running
  /// the group's query against the user's fields.
  pub fn evaluate_membership(&self, user: &User) -> bool {
    let user_value = user.get_field(&self.query_field);
    if user_value.is_empty() && self.query_field != "email" {
      return false;
    }

    match self.query_operator.as_str() {
      "eq" => user_value == self.query_value,
      "neq" => user_value != self.query_value,
      "contains" => user_value.contains(&self.query_value),
      "starts_with" => user_value.starts_with(&self.query_value),
      "in" => self.query_value.split(',').any(|v| v.trim() == user_value),
      "lt" => user_value < self.query_value,
      "gt" => user_value > self.query_value,
      _ => false,
    }
  }
}
