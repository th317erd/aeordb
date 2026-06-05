use aeordb_plugin_sdk::aeordb_query_plugin;
use aeordb_plugin_sdk::prelude::*;
use jaq_all::jaq_core::Vars;
use serde::Deserialize;

aeordb_query_plugin!(jq_handle);

#[derive(Debug, Deserialize)]
struct JqPayload {
  #[serde(alias = "path")]
  file: String,
  expr: String,
}

fn jq_handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
  let payload = match json::parse_request::<JqPayload>(&request) {
    Ok(payload) => payload,
    Err(error) => return Ok(PluginResponse::error(400, error.to_string())),
  };

  if payload.file.trim().is_empty() {
    return Ok(PluginResponse::error(400, "file is required"));
  }
  if payload.expr.trim().is_empty() {
    return Ok(PluginResponse::error(400, "expr is required"));
  }

  let file = match ctx.read_file(&payload.file) {
    Ok(file) => file,
    Err(error) => return Ok(PluginResponse::error(500, error.to_string())),
  };

  let input = match jaq_all::json::read::parse_single(&file.data) {
    Ok(value) => value,
    Err(error) => {
      return Ok(PluginResponse::error(400, format!("failed to parse file as JSON: {}", error)));
    }
  };

  let filter = match compile_filter(&payload.expr) {
    Ok(filter) => filter,
    Err(error) => return Ok(PluginResponse::error(400, error)),
  };

  match run_filter(&filter, input) {
    Ok(outputs) => {
      PluginResponse::json(200, &json::json!({ "outputs": outputs })).map_err(|error| PluginError::SerializationFailed(error.to_string()))
    }
    Err(error) => Ok(PluginResponse::error(500, error)),
  }
}

fn compile_filter(expr: &str) -> Result<jaq_all::data::Filter, String> {
  jaq_all::compile_with(expr, jaq_all::defs(), jaq_all::data::base_funs(), &[])
    .map_err(|reports| format!("failed to compile jq expression: {:?}", reports))
}

fn run_filter(filter: &jaq_all::data::Filter, input: jaq_all::json::Val) -> Result<Vec<serde_json::Value>, String> {
  let runner = jaq_all::data::Runner::default();
  let inputs = core::iter::once(Ok::<jaq_all::json::Val, String>(input));
  let mut outputs = Vec::new();

  jaq_all::data::run(
    &runner,
    filter,
    Vars::new([]),
    inputs,
    |error| error,
    |output| {
      let value = output.map_err(|error| error.to_string())?;
      outputs.push(jaq_value_to_json(value)?);
      Ok(())
    },
  )?;

  Ok(outputs)
}

fn jaq_value_to_json(value: jaq_all::json::Val) -> Result<serde_json::Value, String> {
  let mut bytes = Vec::new();
  jaq_all::json::write::write(&mut bytes, &jaq_all::json::write::Pp::default(), 0, &value)
    .map_err(|error| format!("failed to serialize jq output: {}", error))?;

  serde_json::from_slice(&bytes).map_err(|error| format!("jq output is not representable as JSON: {}", error))
}
