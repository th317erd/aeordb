use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

const EMAIL_CONFIG_PATH: &str = "/.aeordb-system/email-config.json";
const EMAIL_CONFIG_DOCUMENT_MAX_BYTES: u64 = 128 * 1024;
const EMAIL_CONFIG_FIELD_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider")]
pub enum EmailConfig {
  #[serde(rename = "smtp")]
  Smtp(SmtpConfig),
  #[serde(rename = "oauth")]
  OAuth(OAuthConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpConfig {
  pub host: String,
  pub port: u16,
  pub username: String,
  pub password: String,
  pub from_address: String,
  #[serde(default = "default_from_name")]
  pub from_name: String,
  #[serde(default = "default_tls")]
  pub tls: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
  pub oauth_provider: String,
  pub client_id: String,
  pub client_secret: String,
  pub refresh_token: String,
  pub from_address: String,
  #[serde(default = "default_from_name")]
  pub from_name: String,
  pub token_url: Option<String>,
  pub send_url: Option<String>,
}

fn default_from_name() -> String {
  "AeorDB".to_string()
}

fn default_tls() -> String {
  "starttls".to_string()
}

impl EmailConfig {
  pub fn from_address(&self) -> &str {
    match self {
      EmailConfig::Smtp(c) => &c.from_address,
      EmailConfig::OAuth(c) => &c.from_address,
    }
  }

  pub fn from_name(&self) -> &str {
    match self {
      EmailConfig::Smtp(c) => &c.from_name,
      EmailConfig::OAuth(c) => &c.from_name,
    }
  }

  pub fn masked(&self) -> EngineResult<serde_json::Value> {
    let mut val =
      serde_json::to_value(self).map_err(|error| EngineError::JsonParseError(format!("Email config masking failed: {error}")))?;
    if let Some(obj) = val.as_object_mut() {
      for key in ["password", "client_secret", "refresh_token"] {
        if obj.contains_key(key) {
          obj.insert(key.to_string(), serde_json::json!("--------"));
        }
      }
      obj.insert("configured".to_string(), serde_json::json!(true));
    }
    Ok(val)
  }

  fn validate(&self) -> EngineResult<()> {
    match self {
      EmailConfig::Smtp(config) => {
        validate_identity_field("host", &config.host, false)?;
        if config.port == 0 {
          return Err(EngineError::InvalidInput("email config port must not be zero".to_string()));
        }
        validate_identity_field("username", &config.username, true)?;
        validate_secret_field("password", &config.password, true)?;
        validate_identity_field("from_address", &config.from_address, false)?;
        validate_identity_field("from_name", &config.from_name, false)?;
        validate_identity_field("tls", &config.tls, false)?;
      }
      EmailConfig::OAuth(config) => {
        validate_identity_field("oauth_provider", &config.oauth_provider, false)?;
        validate_identity_field("client_id", &config.client_id, false)?;
        validate_secret_field("client_secret", &config.client_secret, false)?;
        validate_secret_field("refresh_token", &config.refresh_token, false)?;
        validate_identity_field("from_address", &config.from_address, false)?;
        validate_identity_field("from_name", &config.from_name, false)?;
        validate_optional_identity_field("token_url", config.token_url.as_deref())?;
        validate_optional_identity_field("send_url", config.send_url.as_deref())?;
      }
    }
    Ok(())
  }
}

fn validate_field_size(field: &str, value: &str) -> EngineResult<()> {
  if value.len() > EMAIL_CONFIG_FIELD_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "email config {field} is {} bytes, exceeding the {EMAIL_CONFIG_FIELD_MAX_BYTES}-byte limit",
      value.len(),
    )));
  }
  Ok(())
}

fn validate_identity_field(field: &str, value: &str, allow_empty: bool) -> EngineResult<()> {
  validate_field_size(field, value)?;
  if !allow_empty && value.is_empty() {
    return Err(EngineError::InvalidInput(format!("email config {field} must not be empty")));
  }
  if value.chars().any(char::is_control) {
    return Err(EngineError::InvalidInput(format!("email config {field} contains control characters")));
  }
  Ok(())
}

fn validate_secret_field(field: &str, value: &str, allow_empty: bool) -> EngineResult<()> {
  validate_field_size(field, value)?;
  if !allow_empty && value.is_empty() {
    return Err(EngineError::InvalidInput(format!("email config {field} must not be empty")));
  }
  Ok(())
}

fn validate_optional_identity_field(field: &str, value: Option<&str>) -> EngineResult<()> {
  if let Some(value) = value {
    validate_identity_field(field, value, false)?;
  }
  Ok(())
}

fn validate_persisted_email_config(config: &EmailConfig) -> EngineResult<()> {
  match config.validate() {
    Err(EngineError::InvalidInput(reason)) => Err(EngineError::CorruptEntry { offset: 0, reason }),
    result => result,
  }
}

pub fn load_email_config(engine: &StorageEngine) -> EngineResult<Option<EmailConfig>> {
  let ops = DirectoryOps::new(engine);
  match ops.read_file_buffered_bounded(EMAIL_CONFIG_PATH, EMAIL_CONFIG_DOCUMENT_MAX_BYTES) {
    Ok(data) => {
      let config: EmailConfig =
        serde_json::from_slice(&data).map_err(|e| EngineError::JsonParseError(format!("Invalid email config: {}", e)))?;
      validate_persisted_email_config(&config)?;
      Ok(Some(config))
    }
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(e) => Err(e),
  }
}

pub fn save_email_config(engine: &StorageEngine, config: &EmailConfig) -> EngineResult<()> {
  config.validate()?;
  let ops = DirectoryOps::new(engine);
  let ctx = RequestContext::system();
  let data = serde_json::to_vec_pretty(config).map_err(|e| EngineError::JsonParseError(e.to_string()))?;
  if data.len() as u64 > EMAIL_CONFIG_DOCUMENT_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "email config document is {} bytes, exceeding the {EMAIL_CONFIG_DOCUMENT_MAX_BYTES}-byte limit",
      data.len(),
    )));
  }
  ops.store_file_buffered(&ctx, EMAIL_CONFIG_PATH, &data, Some("application/json"))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_smtp_config_roundtrip() {
    let config = EmailConfig::Smtp(SmtpConfig {
      host: "smtp.example.com".to_string(),
      port: 587,
      username: "user@example.com".to_string(),
      password: "secret123".to_string(),
      from_address: "noreply@example.com".to_string(),
      from_name: "Test".to_string(),
      tls: "starttls".to_string(),
    });

    let json = serde_json::to_vec(&config).unwrap();
    let deserialized: EmailConfig = serde_json::from_slice(&json).unwrap();

    assert_eq!(deserialized.from_address(), "noreply@example.com");
    assert_eq!(deserialized.from_name(), "Test");
  }

  #[test]
  fn test_oauth_config_roundtrip() {
    let config = EmailConfig::OAuth(OAuthConfig {
      oauth_provider: "google".to_string(),
      client_id: "client-id-123".to_string(),
      client_secret: "client-secret-456".to_string(),
      refresh_token: "refresh-token-789".to_string(),
      from_address: "noreply@example.com".to_string(),
      from_name: "AeorDB".to_string(),
      token_url: Some("https://oauth2.googleapis.com/token".to_string()),
      send_url: None,
    });

    let json = serde_json::to_vec(&config).unwrap();
    let deserialized: EmailConfig = serde_json::from_slice(&json).unwrap();

    assert_eq!(deserialized.from_address(), "noreply@example.com");
    assert_eq!(deserialized.from_name(), "AeorDB");
  }

  #[test]
  fn test_masked_smtp_hides_password() {
    let config = EmailConfig::Smtp(SmtpConfig {
      host: "smtp.example.com".to_string(),
      port: 587,
      username: "user@example.com".to_string(),
      password: "secret123".to_string(),
      from_address: "noreply@example.com".to_string(),
      from_name: "Test".to_string(),
      tls: "starttls".to_string(),
    });

    let masked = config.masked().unwrap();
    assert_eq!(masked["password"], "--------");
    assert_eq!(masked["configured"], true);
    // Non-secret fields should be preserved
    assert_eq!(masked["host"], "smtp.example.com");
    assert_eq!(masked["username"], "user@example.com");
  }

  #[test]
  fn test_masked_oauth_hides_secrets() {
    let config = EmailConfig::OAuth(OAuthConfig {
      oauth_provider: "google".to_string(),
      client_id: "client-id-123".to_string(),
      client_secret: "client-secret-456".to_string(),
      refresh_token: "refresh-token-789".to_string(),
      from_address: "noreply@example.com".to_string(),
      from_name: "AeorDB".to_string(),
      token_url: None,
      send_url: None,
    });

    let masked = config.masked().unwrap();
    assert_eq!(masked["client_secret"], "--------");
    assert_eq!(masked["refresh_token"], "--------");
    assert_eq!(masked["configured"], true);
    // Non-secret fields should be preserved
    assert_eq!(masked["client_id"], "client-id-123");
    assert_eq!(masked["oauth_provider"], "google");
  }

  #[test]
  fn test_smtp_defaults() {
    let json = r#"{"provider":"smtp","host":"smtp.example.com","port":587,"username":"user","password":"pass","from_address":"a@b.com"}"#;
    let config: EmailConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.from_name(), "AeorDB");
    if let EmailConfig::Smtp(smtp) = &config {
      assert_eq!(smtp.tls, "starttls");
    } else {
      panic!("Expected Smtp variant");
    }
  }

  #[test]
  fn test_invalid_json_returns_error() {
    let bad_json = b"not json at all";
    let result: Result<EmailConfig, _> = serde_json::from_slice(bad_json);
    assert!(result.is_err());
  }

  #[test]
  fn test_missing_provider_tag_returns_error() {
    let json = r#"{"host":"smtp.example.com","port":587}"#;
    let result: Result<EmailConfig, _> = serde_json::from_str(json);
    assert!(result.is_err());
  }

  #[test]
  fn test_unknown_provider_tag_returns_error() {
    let json = r#"{"provider":"sendgrid","host":"smtp.example.com"}"#;
    let result: Result<EmailConfig, _> = serde_json::from_str(json);
    assert!(result.is_err());
  }
}
