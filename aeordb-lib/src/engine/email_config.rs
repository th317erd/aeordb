use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

const EMAIL_CONFIG_PATH: &str = "/.system/email-config.json";

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

    pub fn masked(&self) -> serde_json::Value {
        let mut val = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = val.as_object_mut() {
            for key in ["password", "client_secret", "refresh_token"] {
                if obj.contains_key(key) {
                    obj.insert(key.to_string(), serde_json::json!("--------"));
                }
            }
            obj.insert("configured".to_string(), serde_json::json!(true));
        }
        val
    }
}

pub fn load_email_config(engine: &StorageEngine) -> EngineResult<Option<EmailConfig>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file(EMAIL_CONFIG_PATH) {
        Ok(data) => {
            let config: EmailConfig = serde_json::from_slice(&data)
                .map_err(|e| EngineError::JsonParseError(format!("Invalid email config: {}", e)))?;
            Ok(Some(config))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn save_email_config(engine: &StorageEngine, config: &EmailConfig) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let ctx = RequestContext::system();
    let data = serde_json::to_vec_pretty(config)
        .map_err(|e| EngineError::JsonParseError(e.to_string()))?;
    ops.store_file(&ctx, EMAIL_CONFIG_PATH, &data, Some("application/json"))?;
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

        let masked = config.masked();
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

        let masked = config.masked();
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
