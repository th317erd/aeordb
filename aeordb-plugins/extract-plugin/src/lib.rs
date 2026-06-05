use aeordb_plugin_sdk::aeordb_query_plugin;
use aeordb_plugin_sdk::prelude::*;
use serde::Deserialize;

aeordb_query_plugin!(extract_handle);

#[derive(Debug, Deserialize)]
struct ExtractPayload {
  #[serde(alias = "path")]
  file: String,
  mode: String,
  start: Option<u64>,
  end: Option<u64>,
  max_bytes: Option<usize>,
}

fn extract_handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
  let payload = match json::parse_request::<ExtractPayload>(&request) {
    Ok(payload) => payload,
    Err(error) => return Ok(PluginResponse::error(400, error.to_string())),
  };

  if payload.file.trim().is_empty() {
    return Ok(PluginResponse::error(400, "file is required"));
  }

  if !matches!(payload.mode.as_str(), "lines" | "chars") {
    return Ok(PluginResponse::error(400, "mode must be either \"lines\" or \"chars\""));
  }

  let extract_request = ExtractRequest { mode: payload.mode, start: payload.start, end: payload.end, max_bytes: payload.max_bytes };

  match ctx.extract_file(&payload.file, extract_request) {
    Ok(extracted) => PluginResponse::json(200, &extracted).map_err(|error| PluginError::SerializationFailed(error.to_string())),
    Err(error) => Ok(PluginResponse::error(500, error.to_string())),
  }
}
