use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::engine::{DirectoryOps, EngineError, EngineResult, StorageEngine};

#[derive(Debug, Clone, Deserialize)]
pub struct CorsRule {
  pub path: String,
  pub origins: Vec<String>,
  #[serde(default = "default_methods")]
  pub methods: Vec<String>,
  #[serde(default = "default_headers")]
  pub allow_headers: Vec<String>,
  #[serde(default = "default_max_age")]
  pub max_age: u64,
  #[serde(default)]
  pub allow_credentials: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CorsConfig {
  pub rules: Vec<CorsRule>,
}

#[derive(Debug, Clone)]
pub struct CorsState {
  /// Origins from the CLI --cors flag (None = no CORS at all).
  pub default_origins: Option<Vec<String>>,
  /// Per-path rules from /.aeordb-config/cors.json.
  pub rules: Vec<CorsRule>,
}

fn default_methods() -> Vec<String> {
  vec!["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS"].into_iter().map(String::from).collect()
}

fn default_headers() -> Vec<String> {
  vec!["Content-Type", "Authorization"].into_iter().map(String::from).collect()
}

fn default_max_age() -> u64 {
  3600
}

/// Parse the CLI --cors flag value into a list of allowed origins.
pub fn parse_cors_origins(flag: &str) -> Vec<String> {
  flag.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Load per-path CORS rules from /.aeordb-config/cors.json in the engine.
/// A missing file means no path-specific CORS policy. Malformed or unreadable
/// authority is returned to startup instead of being presented as no policy.
pub fn load_cors_config(engine: &StorageEngine) -> EngineResult<Vec<CorsRule>> {
  let ops = DirectoryOps::new(engine);
  match ops.read_file_buffered("/.aeordb-config/cors.json") {
    Ok(data) => serde_json::from_slice::<CorsConfig>(&data)
      .map(|config| config.rules)
      .map_err(|error| EngineError::JsonParseError(format!("Invalid CORS configuration: {error}"))),
    Err(EngineError::NotFound(_)) => Ok(Vec::new()),
    Err(error) => Err(error),
  }
}

/// Build a CorsState from the CLI flag and the engine config file.
pub fn build_cors_state(cors_flag: Option<&str>, engine: &StorageEngine) -> EngineResult<CorsState> {
  let default_origins = cors_flag.map(parse_cors_origins);
  let rules = load_cors_config(engine)?;
  Ok(CorsState { default_origins, rules })
}

fn response_header(value: &str, name: &'static str) -> Result<HeaderValue, Response> {
  HeaderValue::from_str(value).map_err(|error| {
    tracing::error!(header = name, %error, "CORS policy produced an invalid response header");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
  })
}

/// CORS middleware: checks per-path rules first, then falls back to CLI default.
/// Must be the outermost layer so preflight OPTIONS bypass auth.
pub async fn cors_middleware(axum::extract::State(cors_state): axum::extract::State<CorsState>, request: Request, next: Next) -> Response {
  let origin = request.headers().get("origin").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();

  let path = request.uri().path().to_string();
  let is_preflight = request.method() == Method::OPTIONS;

  // Find matching rule (first match wins)
  let matching_rule = cors_state.rules.iter().find(|rule| {
    if rule.path.ends_with('*') {
      let prefix = &rule.path[..rule.path.len() - 1];
      path.starts_with(prefix)
    } else {
      path == rule.path
    }
  });

  // Determine CORS policy
  let (allowed_origins, methods, headers, max_age, credentials) = if let Some(rule) = matching_rule {
    (rule.origins.clone(), rule.methods.join(", "), rule.allow_headers.join(", "), rule.max_age, rule.allow_credentials)
  } else if let Some(ref defaults) = cors_state.default_origins {
    (defaults.clone(), "GET, POST, PUT, DELETE, HEAD, OPTIONS".to_string(), "Content-Type, Authorization".to_string(), 3600u64, false)
  } else {
    // No CORS configured at all -- pass through without headers
    return next.run(request).await;
  };

  // Check if origin is allowed
  let origin_allowed = allowed_origins.iter().any(|o| o == "*" || o == &origin);

  if !origin_allowed && !origin.is_empty() {
    if is_preflight {
      return StatusCode::FORBIDDEN.into_response();
    }
    // Not allowed -- omit CORS headers
    return next.run(request).await;
  }

  // Compute the value for Access-Control-Allow-Origin
  let allow_origin = if allowed_origins.contains(&"*".to_string()) { "*".to_string() } else { origin };

  // Handle preflight
  if is_preflight {
    let allow_origin = match response_header(&allow_origin, "access-control-allow-origin") {
      Ok(value) => value,
      Err(response) => return response,
    };
    let methods = match response_header(&methods, "access-control-allow-methods") {
      Ok(value) => value,
      Err(response) => return response,
    };
    let headers = match response_header(&headers, "access-control-allow-headers") {
      Ok(value) => value,
      Err(response) => return response,
    };
    let max_age = match response_header(&max_age.to_string(), "access-control-max-age") {
      Ok(value) => value,
      Err(response) => return response,
    };
    let mut response = StatusCode::NO_CONTENT.into_response();
    let hdrs = response.headers_mut();

    hdrs.insert("access-control-allow-origin", allow_origin);
    hdrs.insert("access-control-allow-methods", methods);
    hdrs.insert("access-control-allow-headers", headers);
    hdrs.insert("access-control-max-age", max_age);
    if credentials {
      hdrs.insert("access-control-allow-credentials", HeaderValue::from_static("true"));
    }
    return response;
  }

  // Normal request -- run handler then attach CORS headers
  let allow_origin = match response_header(&allow_origin, "access-control-allow-origin") {
    Ok(value) => value,
    Err(response) => return response,
  };
  let mut response = next.run(request).await;
  let hdrs = response.headers_mut();

  hdrs.insert("access-control-allow-origin", allow_origin);
  if credentials {
    hdrs.insert("access-control-allow-credentials", HeaderValue::from_static("true"));
  }

  response
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_cors_origins_wildcard() {
    let origins = parse_cors_origins("*");
    assert_eq!(origins, vec!["*"]);
  }

  #[test]
  fn test_parse_cors_origins_multiple() {
    let origins = parse_cors_origins("https://a.com,https://b.com");
    assert_eq!(origins, vec!["https://a.com", "https://b.com"]);
  }

  #[test]
  fn test_parse_cors_origins_with_spaces() {
    let origins = parse_cors_origins("https://a.com , https://b.com");
    assert_eq!(origins, vec!["https://a.com", "https://b.com"]);
  }

  #[test]
  fn test_parse_cors_origins_empty_string() {
    let origins = parse_cors_origins("");
    assert!(origins.is_empty());
  }

  #[test]
  fn test_default_methods() {
    let m = default_methods();
    assert!(m.contains(&"GET".to_string()));
    assert!(m.contains(&"OPTIONS".to_string()));
    assert_eq!(m.len(), 6);
  }

  #[test]
  fn test_default_headers() {
    let h = default_headers();
    assert!(h.contains(&"Content-Type".to_string()));
    assert!(h.contains(&"Authorization".to_string()));
    assert_eq!(h.len(), 2);
  }

  #[test]
  fn test_default_max_age() {
    assert_eq!(default_max_age(), 3600);
  }

  #[test]
  fn test_cors_rule_deserialization_defaults() {
    let json = r#"{"path": "/test/*", "origins": ["*"]}"#;
    let rule: CorsRule = serde_json::from_str(json).unwrap();
    assert_eq!(rule.path, "/test/*");
    assert_eq!(rule.origins, vec!["*"]);
    assert_eq!(rule.methods.len(), 6);
    assert_eq!(rule.allow_headers.len(), 2);
    assert_eq!(rule.max_age, 3600);
    assert!(!rule.allow_credentials);
  }

  #[test]
  fn test_cors_rule_deserialization_full() {
    let json = r#"{
            "path": "/api/*",
            "origins": ["https://example.com"],
            "methods": ["GET", "POST"],
            "allow_headers": ["X-Custom"],
            "max_age": 600,
            "allow_credentials": true
        }"#;
    let rule: CorsRule = serde_json::from_str(json).unwrap();
    assert_eq!(rule.methods, vec!["GET", "POST"]);
    assert_eq!(rule.allow_headers, vec!["X-Custom"]);
    assert_eq!(rule.max_age, 600);
    assert!(rule.allow_credentials);
  }

  #[test]
  fn test_cors_config_deserialization() {
    let json = r#"{"rules": [{"path": "/a", "origins": ["*"]}, {"path": "/b", "origins": ["https://b.com"]}]}"#;
    let config: CorsConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.rules.len(), 2);
  }
}
