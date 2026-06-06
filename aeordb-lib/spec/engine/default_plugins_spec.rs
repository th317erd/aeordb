use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
use aeordb::plugins::plugin_manager::PluginManager;
use aeordb::plugins::types::PluginType;
use aeordb::server::create_temp_engine_for_tests;
use std::sync::Arc;

fn load_default_plugin_wasm(plugin_dir: &str, wasm_name: &str) -> Vec<u8> {
  let release_path =
    format!("{}/../aeordb-plugins/{}/target/wasm32-unknown-unknown/release/{}.wasm", env!("CARGO_MANIFEST_DIR"), plugin_dir, wasm_name);
  let debug_path =
    format!("{}/../aeordb-plugins/{}/target/wasm32-unknown-unknown/debug/{}.wasm", env!("CARGO_MANIFEST_DIR"), plugin_dir, wasm_name);

  if let Ok(bytes) = std::fs::read(&release_path) {
    return bytes;
  }
  if let Ok(bytes) = std::fs::read(&debug_path) {
    return bytes;
  }

  panic!(
    "Default plugin WASM not found at:\n  {}\n  {}\n\
         Build it first:\n  cd aeordb-plugins/{} && cargo build --target wasm32-unknown-unknown --release",
    release_path, debug_path, plugin_dir
  );
}

fn setup_plugin(plugin_name: &str, plugin_dir: &str, wasm_name: &str) -> (Arc<StorageEngine>, PluginManager, tempfile::TempDir) {
  let (engine, temp) = create_temp_engine_for_tests();
  let pm = PluginManager::new(engine.clone());
  let wasm = load_default_plugin_wasm(plugin_dir, wasm_name);
  pm.deploy_plugin(plugin_name, &format!("default/{}", plugin_name), PluginType::Wasm, wasm)
    .unwrap_or_else(|error| panic!("deploy {} plugin: {}", plugin_name, error));

  (engine, pm, temp)
}

fn invoke_plugin(pm: &PluginManager, engine: &Arc<StorageEngine>, plugin_name: &str, body: serde_json::Value) -> serde_json::Value {
  let request = serde_json::json!({
      "arguments": serde_json::to_vec(&body).unwrap(),
      "metadata": {
          "function_name": "invoke",
          "path": format!("/plugins/{}/invoke", plugin_name),
          "plugin_path": format!("default/{}", plugin_name)
      }
  });

  let response_bytes = pm
    .invoke_wasm_plugin_with_context(
      &format!("default/{}", plugin_name),
      &serde_json::to_vec(&request).unwrap(),
      engine.clone(),
      RequestContext::system(),
    )
    .unwrap_or_else(|error| panic!("invoke {} plugin: {}", plugin_name, error));

  serde_json::from_slice(&response_bytes).expect("parse PluginResponse envelope")
}

fn response_body_json(response: &serde_json::Value) -> serde_json::Value {
  let body_bytes: Vec<u8> =
    response["body"].as_array().unwrap_or(&vec![]).iter().filter_map(|value| value.as_u64().map(|byte| byte as u8)).collect();

  serde_json::from_slice(&body_bytes)
    .unwrap_or_else(|error| panic!("parse PluginResponse body as JSON: {}\n{}", error, String::from_utf8_lossy(&body_bytes)))
}

#[test]
fn extract_plugin_extracts_crlf_lines_without_buffering_whole_file_in_plugin() {
  let (engine, pm, _temp) = setup_plugin("extract", "extract-plugin", "aeordb_extract_plugin");
  let ops = DirectoryOps::new(engine.as_ref());
  ops
    .store_file_buffered(&RequestContext::system(), "/docs/crlf.txt", b"first\r\nsecond\r\nthird\r\nfourth", Some("text/plain"))
    .expect("store CRLF text fixture");

  let response = invoke_plugin(
    &pm,
    &engine,
    "extract",
    serde_json::json!({
        "file": "/docs/crlf.txt",
        "mode": "lines",
        "start": 2,
        "end": 3
    }),
  );
  let body = response_body_json(&response);

  assert_eq!(response["status_code"], 200);
  assert_eq!(body["text"], "second\r\nthird\r\n");
  assert_eq!(body["mode"], "lines");
  assert_eq!(body["truncated"], false);
}

#[test]
fn extract_plugin_extracts_utf8_char_ranges_and_reports_truncation() {
  let (engine, pm, _temp) = setup_plugin("extract", "extract-plugin", "aeordb_extract_plugin");
  let ops = DirectoryOps::new(engine.as_ref());
  ops
    .store_file_buffered(&RequestContext::system(), "/docs/unicode.txt", "alpha βeta gamma".as_bytes(), Some("text/plain"))
    .expect("store UTF-8 text fixture");

  let response = invoke_plugin(
    &pm,
    &engine,
    "extract",
    serde_json::json!({
        "path": "/docs/unicode.txt",
        "mode": "chars",
        "start": 6,
        "end": 16,
        "max_bytes": 5
    }),
  );
  let body = response_body_json(&response);

  assert_eq!(response["status_code"], 200);
  assert_eq!(body["text"], "βeta");
  assert_eq!(body["mode"], "chars");
  assert_eq!(body["truncated"], true);
}

#[test]
fn extract_plugin_rejects_invalid_requests() {
  let (engine, pm, _temp) = setup_plugin("extract", "extract-plugin", "aeordb_extract_plugin");

  let missing_file = invoke_plugin(
    &pm,
    &engine,
    "extract",
    serde_json::json!({
        "mode": "lines",
        "start": 1,
        "end": 2
    }),
  );
  assert_eq!(missing_file["status_code"], 400);
  assert!(response_body_json(&missing_file)["error"].as_str().unwrap().contains("file"));

  let bad_mode = invoke_plugin(
    &pm,
    &engine,
    "extract",
    serde_json::json!({
        "file": "/docs/nope.txt",
        "mode": "words"
    }),
  );
  assert_eq!(bad_mode["status_code"], 400);
  assert_eq!(response_body_json(&bad_mode)["error"], "mode must be either \"lines\" or \"chars\"");
}

#[test]
fn jq_plugin_filters_json_files_and_returns_all_outputs() {
  let (engine, pm, _temp) = setup_plugin("jq", "jq-plugin", "aeordb_jq_plugin");
  let ops = DirectoryOps::new(engine.as_ref());
  ops
    .store_file_buffered(
      &RequestContext::system(),
      "/data/messages.json",
      br#"{"messages":[{"role":"user","content":"one"},{"role":"assistant","content":"two"},{"role":"user","content":"three"}]}"#,
      Some("application/json"),
    )
    .expect("store JSON fixture");

  let response = invoke_plugin(
    &pm,
    &engine,
    "jq",
    serde_json::json!({
        "file": "/data/messages.json",
        "expr": ".messages[] | select(.role == \"user\") | .content"
    }),
  );
  let body = response_body_json(&response);

  assert_eq!(response["status_code"], 200);
  assert_eq!(body["outputs"], serde_json::json!(["one", "three"]));
}

#[test]
fn jq_plugin_reports_invalid_json_and_invalid_expressions() {
  let (engine, pm, _temp) = setup_plugin("jq", "jq-plugin", "aeordb_jq_plugin");
  let ops = DirectoryOps::new(engine.as_ref());
  ops
    .store_file_buffered(&RequestContext::system(), "/data/bad.json", b"{not-json", Some("application/json"))
    .expect("store malformed JSON fixture");
  ops
    .store_file_buffered(&RequestContext::system(), "/data/good.json", br#"{"ok":true}"#, Some("application/json"))
    .expect("store JSON fixture");

  let bad_json = invoke_plugin(
    &pm,
    &engine,
    "jq",
    serde_json::json!({
        "file": "/data/bad.json",
        "expr": "."
    }),
  );
  assert_eq!(bad_json["status_code"], 400);
  assert!(response_body_json(&bad_json)["error"].as_str().unwrap().contains("failed to parse file as JSON"));

  let bad_expr = invoke_plugin(
    &pm,
    &engine,
    "jq",
    serde_json::json!({
        "file": "/data/good.json",
        "expr": ". |"
    }),
  );
  assert_eq!(bad_expr["status_code"], 400);
  assert!(response_body_json(&bad_expr)["error"].as_str().unwrap().contains("failed to compile jq expression"));
}

#[test]
fn jq_plugin_rejects_invalid_requests() {
  let (engine, pm, _temp) = setup_plugin("jq", "jq-plugin", "aeordb_jq_plugin");

  let missing_file = invoke_plugin(
    &pm,
    &engine,
    "jq",
    serde_json::json!({
        "expr": "."
    }),
  );
  assert_eq!(missing_file["status_code"], 400);
  assert!(response_body_json(&missing_file)["error"].as_str().unwrap().contains("file"));

  let missing_expr = invoke_plugin(
    &pm,
    &engine,
    "jq",
    serde_json::json!({
        "file": "/data/good.json"
    }),
  );
  assert_eq!(missing_expr["status_code"], 400);
  assert!(response_body_json(&missing_expr)["error"].as_str().unwrap().contains("expr"));
}
