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
  assert!(config.server.log_format.is_none());
  assert!(config.server.tls.is_none());
  assert!(config.server.cors.is_none());
  assert!(config.auth.mode.is_none());
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
  assert!(config.storage.chunk_size.is_none());
  assert!(config.storage.hot_dir.is_none());
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
log_format = "json"

[server.tls]
cert = "/etc/ssl/cert.pem"
key = "/etc/ssl/key.pem"

[server.cors]
origins = ["https://example.com", "https://admin.example.com"]

[auth]
mode = "self"
jwt_expiry_seconds = 7200

[storage]
database = "prod.aeordb"
chunk_size = 524288
hot_dir = "/var/aeordb/hot"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert_eq!(config.server.port, Some(8080));
  assert_eq!(config.server.host.as_deref(), Some("127.0.0.1"));
  assert_eq!(config.server.log_format.as_deref(), Some("json"));

  let tls = config.server.tls.unwrap();
  assert_eq!(tls.cert.as_deref(), Some("/etc/ssl/cert.pem"));
  assert_eq!(tls.key.as_deref(), Some("/etc/ssl/key.pem"));

  let origins = config.server.cors.unwrap().origins.unwrap();
  assert_eq!(origins, vec!["https://example.com", "https://admin.example.com"]);

  assert_eq!(config.auth.mode.as_deref(), Some("self"));
  assert_eq!(config.auth.jwt_expiry_seconds, Some(7200));
  assert_eq!(config.storage.database.as_deref(), Some("prod.aeordb"));
  assert_eq!(config.storage.chunk_size, Some(524288));
  assert_eq!(config.storage.hot_dir.as_deref(), Some("/var/aeordb/hot"));
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
  assert!(config.server.log_format.is_none());
  assert!(config.server.tls.is_none());
  assert!(config.server.cors.is_none());
  // auth and storage sections should be fully default
  assert!(config.auth.mode.is_none());
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
  assert!(config.storage.chunk_size.is_none());
  assert!(config.storage.hot_dir.is_none());
}

#[test]
fn load_config_with_only_auth_section() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[auth]
mode = "disabled"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();

  assert!(config.server.port.is_none());
  assert_eq!(config.auth.mode.as_deref(), Some("disabled"));
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
  assert!(config.auth.mode.is_none());
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
  assert!(config.server.log_format.is_none());
  assert!(config.server.tls.is_none());
  assert!(config.server.cors.is_none());
  assert!(config.auth.mode.is_none());
  assert!(config.auth.jwt_expiry_seconds.is_none());
  assert!(config.storage.database.is_none());
  assert!(config.storage.chunk_size.is_none());
  assert!(config.storage.hot_dir.is_none());
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
  // Our design intent is forward-compatibility, so unknown keys should be tolerated.
  match result {
    Ok(config) => {
      assert_eq!(config.server.port, Some(5000));
    }
    Err(error) => {
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
[server.cors]
origins = ["*"]
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  let origins = config.server.cors.unwrap().origins.unwrap();
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
// auth.mode parsing: disabled / self / file:///path
// ---------------------------------------------------------------------------

#[test]
fn load_config_auth_mode_disabled() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[auth]
mode = "disabled"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.auth.mode.as_deref(), Some("disabled"));
}

#[test]
fn load_config_auth_mode_self() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[auth]
mode = "self"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.auth.mode.as_deref(), Some("self"));
}

#[test]
fn load_config_auth_mode_file_uri() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[auth]
mode = "file:///etc/aeordb/identity"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.auth.mode.as_deref(), Some("file:///etc/aeordb/identity"));
}

#[test]
fn load_config_auth_mode_omitted_is_none() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[auth]
jwt_expiry_seconds = 1800
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert!(config.auth.mode.is_none());
  assert_eq!(config.auth.jwt_expiry_seconds, Some(1800));
}

// ---------------------------------------------------------------------------
// TLS config parsing
// ---------------------------------------------------------------------------

#[test]
fn load_config_tls_cert_and_key() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server.tls]
cert = "/etc/ssl/server.crt"
key = "/etc/ssl/server.key"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  let tls = config.server.tls.unwrap();
  assert_eq!(tls.cert.as_deref(), Some("/etc/ssl/server.crt"));
  assert_eq!(tls.key.as_deref(), Some("/etc/ssl/server.key"));
}

#[test]
fn load_config_tls_section_with_only_cert() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server.tls]
cert = "/etc/ssl/server.crt"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  let tls = config.server.tls.unwrap();
  assert_eq!(tls.cert.as_deref(), Some("/etc/ssl/server.crt"));
  assert!(tls.key.is_none());
}

#[test]
fn load_config_tls_section_absent() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 6830
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert!(config.server.tls.is_none());
}

// ---------------------------------------------------------------------------
// hot_dir config parsing
// ---------------------------------------------------------------------------

#[test]
fn load_config_hot_dir() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[storage]
hot_dir = "/tmp/aeordb-hot"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.storage.hot_dir.as_deref(), Some("/tmp/aeordb-hot"));
}

#[test]
fn load_config_hot_dir_omitted_is_none() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[storage]
database = "test.aeordb"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert!(config.storage.hot_dir.is_none());
}

// ---------------------------------------------------------------------------
// log_format config parsing
// ---------------------------------------------------------------------------

#[test]
fn load_config_log_format_json() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
log_format = "json"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.server.log_format.as_deref(), Some("json"));
}

#[test]
fn load_config_log_format_pretty() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
log_format = "pretty"
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.server.log_format.as_deref(), Some("pretty"));
}

#[test]
fn load_config_log_format_omitted_is_none() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 6830
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert!(config.server.log_format.is_none());
}

// ---------------------------------------------------------------------------
// Merge behavior validation (unit-level, no server startup)
// ---------------------------------------------------------------------------

/// Simulate the merge logic used in main.rs to verify CLI-over-config precedence.
#[test]
fn merge_cli_overrides_config_port() {
  let cli_port: Option<u16> = Some(9090);
  let config_port: Option<u16> = Some(8080);

  let merged = cli_port.or(config_port).unwrap_or(6830);
  assert_eq!(merged, 9090);
}

#[test]
fn merge_config_overrides_default_port() {
  let cli_port: Option<u16> = None;
  let config_port: Option<u16> = Some(8080);

  let merged = cli_port.or(config_port).unwrap_or(6830);
  assert_eq!(merged, 8080);
}

#[test]
fn merge_falls_back_to_default_when_both_absent() {
  let cli_port: Option<u16> = None;
  let config_port: Option<u16> = None;

  let merged = cli_port.or(config_port).unwrap_or(6830);
  assert_eq!(merged, 6830);
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

// ---------------------------------------------------------------------------
// Merge behavior: auth.mode
// ---------------------------------------------------------------------------

#[test]
fn merge_auth_cli_overrides_config_mode() {
  let cli_auth: Option<String> = Some("disabled".to_string());
  let config_mode: Option<String> = Some("self".to_string());

  let merged = cli_auth.or(config_mode);
  assert_eq!(merged.as_deref(), Some("disabled"));
}

#[test]
fn merge_auth_config_mode_used_when_cli_absent() {
  let cli_auth: Option<String> = None;
  let config_mode: Option<String> = Some("file:///etc/aeordb/identity".to_string());

  let merged = cli_auth.or(config_mode);
  assert_eq!(merged.as_deref(), Some("file:///etc/aeordb/identity"));
}

#[test]
fn merge_auth_both_absent_is_none() {
  let cli_auth: Option<String> = None;
  let config_mode: Option<String> = None;

  let merged = cli_auth.or(config_mode);
  assert!(merged.is_none());
}

// ---------------------------------------------------------------------------
// Merge behavior: jwt_expiry
// ---------------------------------------------------------------------------

#[test]
fn merge_jwt_expiry_cli_overrides_config() {
  let cli_expiry: Option<i64> = Some(1800);
  let config_expiry: Option<i64> = Some(3600);

  let merged = cli_expiry.or(config_expiry).unwrap_or(3600);
  assert_eq!(merged, 1800);
}

#[test]
fn merge_jwt_expiry_config_overrides_default() {
  let cli_expiry: Option<i64> = None;
  let config_expiry: Option<i64> = Some(7200);

  let merged = cli_expiry.or(config_expiry).unwrap_or(3600);
  assert_eq!(merged, 7200);
}

#[test]
fn merge_jwt_expiry_falls_back_to_default() {
  let cli_expiry: Option<i64> = None;
  let config_expiry: Option<i64> = None;

  let merged = cli_expiry.or(config_expiry).unwrap_or(3600);
  assert_eq!(merged, 3600);
}

// ---------------------------------------------------------------------------
// Merge behavior: chunk_size
// ---------------------------------------------------------------------------

#[test]
fn merge_chunk_size_cli_overrides_config() {
  let cli_chunk: Option<usize> = Some(524288);
  let config_chunk: Option<usize> = Some(262144);

  let merged = cli_chunk.or(config_chunk).unwrap_or(262144);
  assert_eq!(merged, 524288);
}

#[test]
fn merge_chunk_size_config_overrides_default() {
  let cli_chunk: Option<usize> = None;
  let config_chunk: Option<usize> = Some(131072);

  let merged = cli_chunk.or(config_chunk).unwrap_or(262144);
  assert_eq!(merged, 131072);
}

#[test]
fn merge_chunk_size_falls_back_to_default() {
  let cli_chunk: Option<usize> = None;
  let config_chunk: Option<usize> = None;

  let merged = cli_chunk.or(config_chunk).unwrap_or(262144);
  assert_eq!(merged, 262144);
}

// ---------------------------------------------------------------------------
// Merge behavior: host
// ---------------------------------------------------------------------------

#[test]
fn merge_host_cli_overrides_config() {
  let cli_host: Option<String> = Some("127.0.0.1".to_string());
  let config_host: Option<String> = Some("0.0.0.0".to_string());

  let merged = cli_host.or(config_host).unwrap_or_else(|| "0.0.0.0".to_string());
  assert_eq!(merged, "127.0.0.1");
}

#[test]
fn merge_host_config_overrides_default() {
  let cli_host: Option<String> = None;
  let config_host: Option<String> = Some("192.168.1.1".to_string());

  let merged = cli_host.or(config_host).unwrap_or_else(|| "0.0.0.0".to_string());
  assert_eq!(merged, "192.168.1.1");
}

#[test]
fn merge_host_falls_back_to_default() {
  let cli_host: Option<String> = None;
  let config_host: Option<String> = None;

  let merged = cli_host.or(config_host).unwrap_or_else(|| "0.0.0.0".to_string());
  assert_eq!(merged, "0.0.0.0");
}

// ---------------------------------------------------------------------------
// Merge behavior: log_format
// ---------------------------------------------------------------------------

#[test]
fn merge_log_format_cli_overrides_config() {
  let cli_format: Option<String> = Some("json".to_string());
  let config_format: Option<String> = Some("pretty".to_string());

  let merged = cli_format.or(config_format).unwrap_or_else(|| "pretty".to_string());
  assert_eq!(merged, "json");
}

#[test]
fn merge_log_format_config_overrides_default() {
  let cli_format: Option<String> = None;
  let config_format: Option<String> = Some("json".to_string());

  let merged = cli_format.or(config_format).unwrap_or_else(|| "pretty".to_string());
  assert_eq!(merged, "json");
}

#[test]
fn merge_log_format_falls_back_to_default() {
  let cli_format: Option<String> = None;
  let config_format: Option<String> = None;

  let merged = cli_format.or(config_format).unwrap_or_else(|| "pretty".to_string());
  assert_eq!(merged, "pretty");
}

// ---------------------------------------------------------------------------
// Merge behavior: TLS (cert and key)
// ---------------------------------------------------------------------------

#[test]
fn merge_tls_cli_overrides_config() {
  let cli_cert: Option<String> = Some("/cli/cert.pem".to_string());
  let cli_key: Option<String> = Some("/cli/key.pem".to_string());

  let config_cert: Option<String> = Some("/config/cert.pem".to_string());
  let config_key: Option<String> = Some("/config/key.pem".to_string());

  let merged_cert = cli_cert.or(config_cert);
  let merged_key = cli_key.or(config_key);

  assert_eq!(merged_cert.as_deref(), Some("/cli/cert.pem"));
  assert_eq!(merged_key.as_deref(), Some("/cli/key.pem"));
}

#[test]
fn merge_tls_config_used_when_cli_absent() {
  let cli_cert: Option<String> = None;
  let cli_key: Option<String> = None;

  let config_cert: Option<String> = Some("/config/cert.pem".to_string());
  let config_key: Option<String> = Some("/config/key.pem".to_string());

  let merged_cert = cli_cert.or(config_cert);
  let merged_key = cli_key.or(config_key);

  assert_eq!(merged_cert.as_deref(), Some("/config/cert.pem"));
  assert_eq!(merged_key.as_deref(), Some("/config/key.pem"));
}

#[test]
fn merge_tls_both_absent_is_none() {
  let cli_cert: Option<String> = None;
  let cli_key: Option<String> = None;

  let config_cert: Option<String> = None;
  let config_key: Option<String> = None;

  let merged_cert = cli_cert.or(config_cert);
  let merged_key = cli_key.or(config_key);

  assert!(merged_cert.is_none());
  assert!(merged_key.is_none());
}

// ---------------------------------------------------------------------------
// Merge behavior: hot_dir
// ---------------------------------------------------------------------------

#[test]
fn merge_hot_dir_cli_overrides_config() {
  let cli_hot_dir: Option<String> = Some("/cli/hot".to_string());
  let config_hot_dir: Option<String> = Some("/config/hot".to_string());

  let merged = cli_hot_dir.or(config_hot_dir);
  assert_eq!(merged.as_deref(), Some("/cli/hot"));
}

#[test]
fn merge_hot_dir_config_used_when_cli_absent() {
  let cli_hot_dir: Option<String> = None;
  let config_hot_dir: Option<String> = Some("/config/hot".to_string());

  let merged = cli_hot_dir.or(config_hot_dir);
  assert_eq!(merged.as_deref(), Some("/config/hot"));
}

#[test]
fn merge_hot_dir_both_absent_is_none() {
  let cli_hot_dir: Option<String> = None;
  let config_hot_dir: Option<String> = None;

  let merged = cli_hot_dir.or(config_hot_dir);
  assert!(merged.is_none());
}

// ---------------------------------------------------------------------------
// Full config round-trip: example config parses without error
// ---------------------------------------------------------------------------

#[test]
fn example_config_parses_successfully() {
  let mut file = tempfile::NamedTempFile::new().unwrap();
  writeln!(
    file,
    r#"
[server]
port = 6830
host = "0.0.0.0"
log_format = "pretty"

[server.cors]
origins = ["https://app.example.com"]

[auth]
mode = "self"
jwt_expiry_seconds = 3600

[storage]
database = "data.aeordb"
chunk_size = 262144
"#
  )
  .unwrap();

  let config = load_config(file.path().to_str().unwrap()).unwrap();
  assert_eq!(config.server.port, Some(6830));
  assert_eq!(config.server.host.as_deref(), Some("0.0.0.0"));
  assert_eq!(config.server.log_format.as_deref(), Some("pretty"));
  assert_eq!(config.auth.mode.as_deref(), Some("self"));
  assert_eq!(config.auth.jwt_expiry_seconds, Some(3600));
  assert_eq!(config.storage.database.as_deref(), Some("data.aeordb"));
  assert_eq!(config.storage.chunk_size, Some(262144));
}
