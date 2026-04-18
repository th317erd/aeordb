use serde::Deserialize;
use std::path::Path;

/// Top-level configuration loaded from a TOML file.
///
/// Every field is optional at the TOML level so that operators only need to
/// specify the values they want to override.  Built-in defaults are applied
/// when a field is absent.
#[derive(Debug, Deserialize, Default)]
pub struct AeorConfig {
  #[serde(default)]
  pub server: ServerConfig,
  #[serde(default)]
  pub auth: AuthConfig,
  #[serde(default)]
  pub storage: StorageConfig,
}

#[derive(Debug, Deserialize, Default)]
pub struct ServerConfig {
  /// TCP port the HTTP server listens on (default: 3000).
  pub port: Option<u16>,
  /// Bind address (default: "0.0.0.0").
  pub host: Option<String>,
  /// Structured log format: "pretty" or "json" (default: "pretty").
  pub log_format: Option<String>,
  /// TLS certificate and key paths.
  pub tls: Option<TlsConfig>,
  /// CORS configuration.
  pub cors: Option<CorsConfig>,
}

#[derive(Debug, Deserialize, Default)]
pub struct TlsConfig {
  /// Path to the TLS certificate PEM file.
  pub cert: Option<String>,
  /// Path to the TLS private key PEM file.
  pub key: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CorsConfig {
  /// Allowed origins.  Use `["*"]` to allow all.
  pub origins: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AuthConfig {
  /// Auth mode: "disabled", "self", or "file:///path/to/identity" (default: "self").
  pub mode: Option<String>,
  /// JWT token lifetime in seconds (default: 3600).
  pub jwt_expiry_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StorageConfig {
  /// Path to the database file (default: "data.aeordb").
  pub database: Option<String>,
  /// Write chunk size in bytes (default: 262144 = 256 KiB).
  pub chunk_size: Option<usize>,
  /// Directory for write-ahead hot files (defaults to database file's parent directory).
  pub hot_dir: Option<String>,
}

/// Load and parse a TOML configuration file.
///
/// * If `path` points to an existing file it is read and parsed.
/// * If the file does not exist an error is returned -- callers that want
///   the "no config" default should avoid calling this function entirely.
pub fn load_config(path: &str) -> Result<AeorConfig, String> {
  let file_path = Path::new(path);

  if !file_path.exists() {
    return Err(format!("Config file not found: {path}"));
  }

  let contents = std::fs::read_to_string(file_path)
    .map_err(|error| format!("Failed to read config file '{path}': {error}"))?;

  let config: AeorConfig = toml::from_str(&contents)
    .map_err(|error| format!("Failed to parse config file '{path}': {error}"))?;

  Ok(config)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_config_has_all_none_fields() {
    let config = AeorConfig::default();
    assert!(config.server.port.is_none());
    assert!(config.server.host.is_none());
    assert!(config.server.log_format.is_none());
    assert!(config.server.tls.is_none());
    assert!(config.server.cors.is_none());
    assert!(config.auth.mode.is_none());
    assert!(config.auth.jwt_expiry_seconds.is_none());
    assert!(config.storage.database.is_none());
    assert!(config.storage.chunk_size.is_none());
    assert!(config.storage.hot_dir.is_none());
  }

  #[test]
  fn load_config_returns_error_for_missing_file() {
    let result = load_config("/nonexistent/path/to/config.toml");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Config file not found"));
  }
}
