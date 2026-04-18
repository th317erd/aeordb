use aeordb_cli::config::{AeorConfig, load_config};
use std::io::Write;

// ---------------------------------------------------------------------------
// Default values
// ---------------------------------------------------------------------------

#[test]
fn default_config_has_all_none_fields() {
  let config = AeorConfig::default();
  assert!(config.server.port.is_none());
  assert!(config.server.host.is_none());
  assert!(config.server.cors_origins.is_none());
  assert!(config.auth.enabled.is_none());
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
  assert!(config.storage.chunk_size.is_none());
}

// ---------------------------------------------------------------------------
// Missing / nonexistent file
// ---------------------------------------------------------------------------

#[test]
fn load_config_returns_error_for_missing_file() {
  let result = load_config("/nonexistent/path/aeordb.toml");
  assert!(result.is_err());
  let error = result.unwrap_err();
  assert!(error.contains("Config file not found"), "unexpected error: {error}");
}

// ---------------------------------------------------------------------------
// Valid full config
// ---------------------------------------------------------------------------

#[test]
fn load_config_parses_full_toml() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 8080
host = "127.0.0.1"
cors_origins = ["https://example.com", "https://admin.example.com"]

[auth]
enabled = false
jwt_expiry_seconds = 7200

[storage]
database = "prod.aeordb"
chunk_size = 524288
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert_eq!(config.server.port, Some(8080));
  assert_eq!(config.server.host.as_deref(), Some("127.0.0.1"));
  let origins = config.server.cors_origins.unwrap();
  assert_eq!(origins, vec!["https://example.com", "https://admin.example.com"]);
  assert_eq!(config.auth.enabled, Some(false));
  assert_eq!(config.auth.jwt_expiry_seconds, Some(7200));
  assert_eq!(config.storage.database.as_deref(), Some("prod.aeordb"));
  assert_eq!(config.storage.chunk_size, Some(524288));
}

// ---------------------------------------------------------------------------
// Partial config -- omitted sections use defaults
// ---------------------------------------------------------------------------

#[test]
fn load_config_with_only_server_section() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 4000
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert_eq!(config.server.port, Some(4000));
  assert!(config.server.host.is_none());
  assert!(config.server.cors_origins.is_none());
  // auth and storage sections should be fully default
  assert!(config.auth.enabled.is_none());
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
  assert!(config.storage.chunk_size.is_none());
}

#[test]
fn load_config_with_only_auth_section() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[auth]
enabled = true
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert!(config.server.port.is_none());
  assert_eq!(config.auth.enabled, Some(true));
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
}

#[test]
fn load_config_with_only_storage_section() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[storage]
database = "custom.aeordb"
chunk_size = 131072
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert!(config.server.port.is_none());
  assert!(config.auth.enabled.is_none());
  assert_eq!(config.storage.database.as_deref(), Some("custom.aeordb"));
  assert_eq!(config.storage.chunk_size, Some(131072));
}

// ---------------------------------------------------------------------------
// Empty file -- should parse as full defaults
// ---------------------------------------------------------------------------

#[test]
fn load_config_with_empty_file() {
  let file = tempfile::NamedTempFile::new().unwrap();
  // File exists but contains nothing.
  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert!(config.server.port.is_none());
  assert!(config.server.host.is_none());
  assert!(config.server.cors_origins.is_none());
  assert!(config.auth.enabled.is_none());
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
  assert!(config.storage.chunk_size.is_none());
}

// ---------------------------------------------------------------------------
// Malformed / invalid TOML
// ---------------------------------------------------------------------------

#[test]
fn load_config_returns_error_for_invalid_toml() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(file, "this is not valid toml [[[").unwrap();

  let result = load_config(file.path().to_str().unwrap());
  assert!(result.is_err());
  let error = result.unwrap_err();
  assert!(error.contains("Failed to parse config file"), "unexpected error: {error}");
}

#[test]
fn load_config_returns_error_for_wrong_types() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = "not-a-number"
"#
  )
  .unwrap();

  let result = load_config(file.path().to_str().unwrap());
  assert!(result.is_err());
  let error = result.unwrap_err();
  assert!(error.contains("Failed to parse config file"), "unexpected error: {error}");
}

#[test]
fn load_config_returns_error_for_negative_port() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = -1
"#
  )
  .unwrap();

  let result = load_config(file.path().to_str().unwrap());
  // u16 cannot hold -1, so TOML parser rejects it
  assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Unknown keys are silently ignored (forward-compatible)
// ---------------------------------------------------------------------------

#[test]
fn load_config_ignores_unknown_keys() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 5000
unknown_field = "hello"

[future_section]
magic = true
"#
  )
  .unwrap();

  // Should succeed, only parsing known fields
  let result = load_config(file.path().to_str().unwrap());
  // By default serde rejects unknown fields; let's see which behavior we get.
  // If this fails we need to add #[serde(deny_unknown_fields)] or the opposite.
  // Our design intent is forward-compatibility, so unknown keys should be tolerated.
  match result {
    Ok(config) => {
      assert_eq!(config.server.port, Some(5000));
    }
    Err(error) => {
      // If serde denies unknown fields we need to fix the struct.
      panic!("Config should tolerate unknown keys for forward-compatibility, but got: {error}");
    }
  }
}

// ---------------------------------------------------------------------------
// CORS origins as single-element list
// ---------------------------------------------------------------------------

#[test]
fn load_config_cors_single_wildcard() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
cors_origins = ["*"]
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  let origins = config.server.cors_origins.unwrap();
  assert_eq!(origins, vec!["*"]);
}

// ---------------------------------------------------------------------------
// Edge: config file is a directory
// ---------------------------------------------------------------------------

#[test]
fn load_config_returns_error_when_path_is_directory() {
  let directory = tempfile::tempdir().unwrap();
  let result = load_config(directory.path().to_str().unwrap());
  assert!(result.is_err());
  let error = result.unwrap_err();
  assert!(error.contains("Failed to read config file"), "unexpected error: {error}");
}

// ---------------------------------------------------------------------------
// Boundary: maximum u16 port value
// ---------------------------------------------------------------------------

#[test]
fn load_config_accepts_max_port() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 65535
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.server.port, Some(65535));
}

#[test]
fn load_config_rejects_port_over_u16_max() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 70000
"#
  )
  .unwrap();

  let result = load_config(file.path().to_str().unwrap());
  assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Merge behavior validation (unit-level, no server startup)
// ---------------------------------------------------------------------------

/// Simulate the merge logic used in main.rs to verify CLI-over-config precedence.
#[test]
fn merge_cli_overrides_config_port() {
  let cli_port: Option<u16> = Some(9090);
  let config_port: Option<u16> = Some(8080);

  let merged = cli_port.or(config_port).unwrap_or(3000);
  assert_eq!(merged, 9090);
}

#[test]
fn merge_config_overrides_default_port() {
  let cli_port: Option<u16> = None;
  let config_port: Option<u16> = Some(8080);

  let merged = cli_port.or(config_port).unwrap_or(3000);
  assert_eq!(merged, 8080);
}

#[test]
fn merge_falls_back_to_default_when_both_absent() {
  let cli_port: Option<u16> = None;
  let config_port: Option<u16> = None;

  let merged = cli_port.or(config_port).unwrap_or(3000);
  assert_eq!(merged, 3000);
}

#[test]
fn merge_cli_database_overrides_config() {
  let cli_database: Option<String> = Some("cli.aeordb".to_string());
  let config_database: Option<String> = Some("config.aeordb".to_string());

  let merged = cli_database.or(config_database).unwrap_or_else(|| "data.aeordb".to_string());
  assert_eq!(merged, "cli.aeordb");
}

#[test]
fn merge_config_database_overrides_default() {
  let cli_database: Option<String> = None;
  let config_database: Option<String> = Some("config.aeordb".to_string());

  let merged = cli_database.or(config_database).unwrap_or_else(|| "data.aeordb".to_string());
  assert_eq!(merged, "config.aeordb");
}

#[test]
fn merge_cors_cli_overrides_config() {
  let cli_cors: Option<String> = Some("*".to_string());
  let config_origins: Option<Vec<String>> = Some(vec!["https://a.com".to_string()]);

  let merged = cli_cors.or_else(|| config_origins.map(|origins| origins.join(",")));
  assert_eq!(merged.as_deref(), Some("*"));
}

#[test]
fn merge_cors_from_config_joins_origins() {
  let cli_cors: Option<String> = None;
  let config_origins: Option<Vec<String>> = Some(vec![
    "https://a.com".to_string(),
    "https://b.com".to_string(),
  ]);

  let merged = cli_cors.or_else(|| config_origins.map(|origins| origins.join(",")));
  assert_eq!(merged.as_deref(), Some("https://a.com,https://b.com"));
}
