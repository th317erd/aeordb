use std::time::Duration;

use clap::Args;
use reqwest::{Client, Response, Url, redirect};
use serde_json::Value;

const MAX_STATUS_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Args)]
pub struct StatusArgs {
  /// Base URL of the running AeorDB server.
  #[arg(long, default_value = "http://127.0.0.1:6830")]
  pub target: String,
  /// Root API key to exchange for a short-lived bearer token.
  #[arg(long, conflicts_with = "token")]
  pub api_key: Option<String>,
  /// Existing root bearer token.
  #[arg(long, conflicts_with = "api_key")]
  pub token: Option<String>,
  /// Emit the exact machine-readable server response.
  #[arg(long)]
  pub json: bool,
}

pub struct StatusRequest {
  pub target: String,
  pub api_key: Option<String>,
  pub token: Option<String>,
}

pub async fn run(arguments: StatusArgs) -> Result<(), String> {
  let api_key = arguments.api_key.or_else(|| std::env::var("AEORDB_ROOT_KEY").ok().filter(|value| !value.is_empty()));
  let response = fetch_status(StatusRequest { target: arguments.target, api_key, token: arguments.token }).await?;
  if arguments.json {
    println!("{}", serde_json::to_string_pretty(&response).map_err(|error| format!("could not serialize status response: {error}"))?);
  } else {
    print!("{}", render_human_status(&response)?);
  }
  Ok(())
}

pub async fn fetch_status(request: StatusRequest) -> Result<Value, String> {
  if request.api_key.is_some() && request.token.is_some() {
    return Err("--api-key and --token cannot be used together".to_string());
  }
  let target = normalize_target(&request.target)?;
  let client = Client::builder()
    .connect_timeout(Duration::from_secs(5))
    .timeout(Duration::from_secs(30))
    .redirect(redirect::Policy::none())
    .build()
    .map_err(|error| format!("could not create HTTP client: {error}"))?;
  let token = match (request.token, request.api_key) {
    (Some(token), None) => Some(token),
    (None, Some(api_key)) => Some(exchange_api_key(&client, &target, &api_key).await?),
    (None, None) => None,
    (Some(_), Some(_)) => unreachable!("credential conflict checked above"),
  };

  let mut request = client.get(endpoint(&target, "system/stats")?);
  if let Some(token) = token.as_deref() {
    request = request.bearer_auth(token);
  }
  let response = request.send().await.map_err(|error| format!("could not reach AeorDB status endpoint: {error}"))?;
  parse_json_response(response, "status", MAX_STATUS_RESPONSE_BYTES, true, token.as_deref()).await
}

async fn exchange_api_key(client: &Client, target: &Url, api_key: &str) -> Result<String, String> {
  let response = client
    .post(endpoint(target, "auth/token")?)
    .json(&serde_json::json!({"api_key": api_key}))
    .send()
    .await
    .map_err(|error| format!("could not reach AeorDB token endpoint: {error}"))?;
  // A token endpoint is allowed to explain its failure, but its response is
  // not a trustworthy place to repeat a root credential into CLI output.
  let response = parse_json_response(response, "token exchange", MAX_ERROR_RESPONSE_BYTES, false, None).await?;
  response["token"]
    .as_str()
    .filter(|token| !token.is_empty())
    .map(str::to_string)
    .ok_or_else(|| "AeorDB token exchange response did not contain a token".to_string())
}

async fn parse_json_response(
  mut response: Response,
  operation: &str,
  maximum_bytes: usize,
  include_error_detail: bool,
  sensitive_value: Option<&str>,
) -> Result<Value, String> {
  let status = response.status();
  if response.content_length().is_some_and(|length| length > maximum_bytes as u64) {
    return Err(format!("AeorDB {operation} response exceeded the {maximum_bytes}-byte diagnostic limit"));
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await.map_err(|error| format!("could not read AeorDB {operation} response: {error}"))? {
    if body.len().saturating_add(chunk.len()) > maximum_bytes {
      return Err(format!("AeorDB {operation} response exceeded the {maximum_bytes}-byte diagnostic limit"));
    }
    body.extend_from_slice(&chunk);
  }
  if !status.is_success() {
    let detail = if include_error_detail { bounded_error_detail(&body, sensitive_value) } else { String::new() };
    return Err(format!("AeorDB {operation} returned HTTP {}{detail}", status.as_u16()));
  }
  serde_json::from_slice(&body).map_err(|error| format!("AeorDB {operation} response was not valid JSON: {error}"))
}

fn bounded_error_detail(body: &[u8], sensitive_value: Option<&str>) -> String {
  let mut text = String::from_utf8_lossy(body).trim().to_string();
  if let Some(sensitive_value) = sensitive_value.filter(|value| !value.is_empty()) {
    text = text.replace(sensitive_value, "<redacted>");
  }
  if text.is_empty() {
    String::new()
  } else {
    format!(": {text}")
  }
}

fn normalize_target(target: &str) -> Result<Url, String> {
  let mut target = Url::parse(target).map_err(|error| format!("invalid AeorDB target URL: {error}"))?;
  if target.scheme() != "http" && target.scheme() != "https" {
    return Err("AeorDB target URL must use http or https".to_string());
  }
  target.set_query(None);
  target.set_fragment(None);
  if !target.path().ends_with('/') {
    let path = format!("{}/", target.path());
    target.set_path(&path);
  }
  Ok(target)
}

fn endpoint(target: &Url, relative: &str) -> Result<Url, String> {
  target.join(relative).map_err(|error| format!("could not construct AeorDB endpoint URL: {error}"))
}

pub fn render_human_status(status: &Value) -> Result<String, String> {
  let version = required_string(status, &["identity", "version"])?;
  let database = required_string(status, &["identity", "database_path"])?;
  let uptime = required_u64(status, &["identity", "uptime_seconds"])?;
  let rss = required_u64(status, &["memory", "process", "rss_bytes"])?;
  let accounted = required_u64(status, &["memory", "coordinator", "accounted_bytes"])?;
  let unaccounted = required_u64(status, &["memory", "coordinator", "unaccounted_rss_bytes"])?;
  let pressure = required_string(status, &["memory", "coordinator", "pressure"])?;
  let read_only = required_bool(status, &["durability", "latch", "read_only"])?;
  let frontier = required_u64(status, &["durability", "frontier", "hard_frontier"])?;
  let waiters = required_u64(status, &["durability", "frontier", "waiter_depth"])?;
  let repair_state = required_string(status, &["durability", "repair", "state"])?;
  let runtime = configuration_state(status, "runtime")?;
  let lifecycle = configuration_state(status, "lifecycle")?;

  Ok(format!(
    "AeorDB {version} · {database} · uptime {uptime}s\n\
Memory: {} RSS · {} accounted · {} unaccounted · pressure {pressure}\n\
Durability: {} · frontier {frontier} · waiters {waiters} · repair {repair_state}\n\
Configuration: runtime {runtime}, lifecycle {lifecycle}\n",
    format_bytes(rss),
    format_bytes(accounted),
    format_bytes(unaccounted),
    if read_only { "read-only" } else { "writable" },
  ))
}

fn configuration_state(status: &Value, family: &str) -> Result<&'static str, String> {
  let family = status
    .get("configuration")
    .and_then(|configuration| configuration.get(family))
    .and_then(|family| family.get("status"))
    .ok_or_else(|| format!("status response is missing configuration.{family}.status"))?;
  if family.get("degraded").and_then(Value::as_bool).unwrap_or(true) {
    return Ok("degraded");
  }
  if family.get("valid").and_then(Value::as_bool).unwrap_or(false) {
    Ok("valid")
  } else {
    Ok("invalid")
  }
}

fn value_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
  path.iter().try_fold(value, |value, field| value.get(*field))
}

fn required_string<'a>(value: &'a Value, path: &[&str]) -> Result<&'a str, String> {
  value_at(value, path).and_then(Value::as_str).ok_or_else(|| format!("status response is missing string field {}", path.join(".")))
}

fn required_u64(value: &Value, path: &[&str]) -> Result<u64, String> {
  value_at(value, path).and_then(Value::as_u64).ok_or_else(|| format!("status response is missing integer field {}", path.join(".")))
}

fn required_bool(value: &Value, path: &[&str]) -> Result<bool, String> {
  value_at(value, path).and_then(Value::as_bool).ok_or_else(|| format!("status response is missing boolean field {}", path.join(".")))
}

fn format_bytes(bytes: u64) -> String {
  const MEBIBYTE: f64 = 1024.0 * 1024.0;
  const GIBIBYTE: f64 = MEBIBYTE * 1024.0;
  if bytes as f64 >= GIBIBYTE {
    format!("{:.2} GiB", bytes as f64 / GIBIBYTE)
  } else {
    format!("{:.2} MiB", bytes as f64 / MEBIBYTE)
  }
}
