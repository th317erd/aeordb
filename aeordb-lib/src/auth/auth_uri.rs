/// Auth provider modes, determined by the `--auth` CLI flag or environment.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthMode {
  /// No authentication. All requests are allowed as root.
  /// Used with --auth=false/null/no/0 for development mode.
  Disabled,
  /// Per-database auth. Keys stored in the .aeordb file itself.
  /// This is the default and matches current behavior.
  SelfContained,
  /// Shared identity file at the given path.
  /// Used with --auth=file:///path/to/identity.
  File(String),
}

/// Parse an auth URI string into an AuthMode.
///
/// Recognized values:
/// - "false", "null", "no", "0" -> Disabled
/// - "self", "./" -> SelfContained
/// - "file:///path" -> File(path)
pub fn parse_auth_uri(uri: &str) -> Result<AuthMode, String> {
  match uri.to_lowercase().as_str() {
    "false" | "null" | "no" | "0" => Ok(AuthMode::Disabled),
    "self" | "./" => Ok(AuthMode::SelfContained),
    _ => {
      if uri.starts_with("file://") {
        let path = uri.strip_prefix("file://").unwrap();
        if path.is_empty() {
          return Err("file:// URI requires a path".to_string());
        }
        let expanded = expand_tilde(path);
        Ok(AuthMode::File(expanded))
      } else {
        Err(format!("Unknown auth URI: {}", uri))
      }
    }
  }
}

/// Resolve which auth mode to use, checking (in priority order):
/// 1. CLI flag (highest priority)
/// 2. AEORDB_AUTH environment variable
/// 3. Default identity file at ~/.config/aeordb/identity
/// 4. Fallback: SelfContained
pub fn resolve_auth_mode(cli_flag: Option<&str>) -> AuthMode {
  // 1. CLI flag (highest priority)
  if let Some(flag) = cli_flag {
    return parse_auth_uri(flag).unwrap_or(AuthMode::SelfContained);
  }

  // 2. AEORDB_AUTH environment variable
  if let Ok(env_val) = std::env::var("AEORDB_AUTH") {
    if !env_val.is_empty() {
      return parse_auth_uri(&env_val).unwrap_or(AuthMode::SelfContained);
    }
  }

  // 3. Check for ~/.config/aeordb/identity
  let default_identity = expand_tilde("~/.config/aeordb/identity");
  if std::path::Path::new(&default_identity).exists() {
    return AuthMode::File(default_identity);
  }

  // 4. Fallback: self-contained
  AuthMode::SelfContained
}

/// Expand a leading `~` or `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
  if path == "~" || path.starts_with("~/") {
    if let Some(home) = std::env::var_os("HOME") {
      return path.replacen("~", &home.to_string_lossy(), 1);
    }
  }
  path.to_string()
}
