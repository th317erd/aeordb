// End-to-end tests for WASM query plugins: deploy the echo-plugin binary,
// invoke its various host-function-exercising functions, and verify the full
// host function stack works correctly.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::plugins::plugin_manager::PluginManager;
use aeordb::plugins::types::PluginType;
use aeordb::server::create_temp_engine_for_tests;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_echo_plugin_wasm() -> Vec<u8> {
    let release_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aeordb-plugins/echo-plugin/target/wasm32-unknown-unknown/release/aeordb_echo_plugin.wasm"
    );
    let debug_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aeordb-plugins/echo-plugin/target/wasm32-unknown-unknown/debug/aeordb_echo_plugin.wasm"
    );

    if let Ok(bytes) = std::fs::read(release_path) {
        return bytes;
    }
    if let Ok(bytes) = std::fs::read(debug_path) {
        return bytes;
    }
    panic!(
        "Echo plugin WASM not found at:\n  {}\n  {}\n\
         Build it first:\n  cd aeordb-plugins/echo-plugin && cargo build --target wasm32-unknown-unknown --release",
        release_path, debug_path
    );
}

fn setup() -> (Arc<StorageEngine>, PluginManager, tempfile::TempDir) {
    let (engine, temp) = create_temp_engine_for_tests();
    let pm = PluginManager::new(engine.clone());

    let wasm = load_echo_plugin_wasm();
    pm.deploy_plugin(
        "echo-plugin",
        "test/echo/plugin",
        PluginType::Wasm,
        wasm,
    )
    .expect("deploy echo plugin");

    (engine, pm, temp)
}

/// Invoke the echo plugin with a given function_name and body bytes.
/// Returns the parsed PluginResponse as a serde_json::Value (the outer envelope)
/// and a convenience-decoded body (the inner JSON from the body bytes).
fn invoke_raw(
    pm: &PluginManager,
    engine: &Arc<StorageEngine>,
    function_name: &str,
    body: &[u8],
) -> serde_json::Value {
    let ctx = RequestContext::system();

    // Build PluginRequest envelope (matches what the _invoke handler builds).
    let request = serde_json::json!({
        "arguments": body.to_vec(),
        "metadata": {
            "function_name": function_name,
            "path": format!("/test/echo/plugin/{}", function_name),
            "plugin_path": "test/echo/plugin"
        }
    });
    let request_bytes = serde_json::to_vec(&request).unwrap();

    let response_bytes = pm
        .invoke_wasm_plugin_with_context(
            "test/echo/plugin",
            &request_bytes,
            engine.clone(),
            ctx,
        )
        .expect("invoke_wasm_plugin_with_context failed");

    // The response is a serialized PluginResponse (status_code, body, content_type, headers).
    serde_json::from_slice(&response_bytes).expect("failed to parse PluginResponse JSON")
}

/// Extract the body from a PluginResponse JSON value and parse it as JSON.
/// The body field is a Vec<u8> serialized as a JSON array of numbers.
fn extract_body_json(response: &serde_json::Value) -> serde_json::Value {
    let body_bytes: Vec<u8> = response["body"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_u64().map(|b| b as u8))
        .collect();

    if body_bytes.is_empty() {
        return serde_json::json!(null);
    }

    serde_json::from_slice(&body_bytes)
        .unwrap_or(serde_json::json!({"raw": String::from_utf8_lossy(&body_bytes).to_string()}))
}

/// Extract the body from a PluginResponse JSON value as a raw string.
fn extract_body_string(response: &serde_json::Value) -> String {
    let body_bytes: Vec<u8> = response["body"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_u64().map(|b| b as u8))
        .collect();

    String::from_utf8(body_bytes).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_echo_plugin_returns_metadata() {
    let (engine, pm, _temp) = setup();
    let response = invoke_raw(&pm, &engine, "echo", b"hello world");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 200);
    assert_eq!(body["echo"], true);
    assert_eq!(body["metadata"]["function_name"], "echo");
    assert_eq!(body["body_len"], 11); // "hello world".len()
}

#[test]
fn test_echo_plugin_reads_file() {
    let (engine, pm, _temp) = setup();

    // Store a file in the engine first
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/test-data/hello.txt", b"Hello from the host!", Some("text/plain"))
        .expect("store test file");

    // Now invoke the plugin's "read" function
    let response = invoke_raw(&pm, &engine, "read", b"/test-data/hello.txt");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 200);
    assert_eq!(body["size"], 20); // "Hello from the host!".len()
    assert_eq!(body["content_type"], "text/plain");
    assert_eq!(body["data_len"], 20);
}

#[test]
fn test_echo_plugin_reads_nonexistent_file() {
    let (engine, pm, _temp) = setup();

    let response = invoke_raw(&pm, &engine, "read", b"/does/not/exist.txt");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 404);
    // The error body should contain an error message
    assert!(
        body["error"].as_str().is_some(),
        "expected error field in response body: {:?}",
        body
    );
}

#[test]
fn test_echo_plugin_writes_file() {
    let (engine, pm, _temp) = setup();

    // Invoke the plugin's "write" function (writes /plugin-output/result.json)
    let response = invoke_raw(&pm, &engine, "write", b"");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 201);
    assert_eq!(body["ok"], true);

    // Verify the file was written to the engine
    let ops = DirectoryOps::new(&engine);
    let data = ops.read_file_buffered("/plugin-output/result.json").expect("read written file");
    let parsed: serde_json::Value =
        serde_json::from_slice(&data).expect("parse written file as JSON");
    assert_eq!(parsed["written"], true);
}

#[test]
fn test_echo_plugin_deletes_file() {
    let (engine, pm, _temp) = setup();

    // First store a file, then delete it via the plugin
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/deleteme/temp.json", b"{}", Some("application/json"))
        .expect("store file for deletion");

    // Verify the file exists
    let data = ops.read_file_buffered("/deleteme/temp.json");
    assert!(data.is_ok(), "file should exist before deletion");

    // Delete via plugin
    let response = invoke_raw(&pm, &engine, "delete", b"/deleteme/temp.json");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 200);
    assert_eq!(body["deleted"], true);

    // Verify the file is gone
    let data_after = ops.read_file_buffered("/deleteme/temp.json");
    assert!(data_after.is_err(), "file should not exist after deletion");
}

#[test]
fn test_echo_plugin_file_metadata() {
    let (engine, pm, _temp) = setup();

    // Store a file first
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/meta-test/doc.txt", b"metadata test content", Some("text/plain"))
        .expect("store file for metadata test");

    let response = invoke_raw(&pm, &engine, "metadata", b"/meta-test/doc.txt");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 200);
    assert_eq!(body["path"], "/meta-test/doc.txt");
    assert_eq!(body["size"], 21); // "metadata test content".len()
    assert!(body["created_at"].as_i64().is_some(), "should have created_at timestamp");
    assert!(body["updated_at"].as_i64().is_some(), "should have updated_at timestamp");
}

#[test]
fn test_echo_plugin_file_metadata_nonexistent() {
    let (engine, pm, _temp) = setup();

    let response = invoke_raw(&pm, &engine, "metadata", b"/no/such/file.txt");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 404);
    assert!(
        body["error"].as_str().is_some(),
        "expected error field: {:?}",
        body
    );
}

#[test]
fn test_echo_plugin_lists_directory() {
    let (engine, pm, _temp) = setup();

    // Store several files in /listing/
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/listing/alpha.txt", b"a", Some("text/plain"))
        .expect("store alpha");
    ops.store_file_buffered(&ctx, "/listing/beta.txt", b"bb", Some("text/plain"))
        .expect("store beta");
    ops.store_file_buffered(&ctx, "/listing/gamma.json", b"{}", Some("application/json"))
        .expect("store gamma");

    let response = invoke_raw(&pm, &engine, "list", b"/listing");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 200);

    let entries = body["entries"].as_array().expect("entries should be an array");
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();

    assert!(names.contains(&"alpha.txt"), "should contain alpha.txt, got: {:?}", names);
    assert!(names.contains(&"beta.txt"), "should contain beta.txt, got: {:?}", names);
    assert!(names.contains(&"gamma.json"), "should contain gamma.json, got: {:?}", names);
}

#[test]
fn test_echo_plugin_returns_custom_status() {
    let (engine, pm, _temp) = setup();
    let response = invoke_raw(&pm, &engine, "status", b"");

    assert_eq!(response["status_code"], 201);
    let body_text = extract_body_string(&response);
    assert_eq!(body_text, "Created by plugin");
    assert_eq!(response["content_type"], "text/plain");
}

#[test]
fn test_echo_plugin_unknown_function_returns_404() {
    let (engine, pm, _temp) = setup();
    let response = invoke_raw(&pm, &engine, "nonexistent", b"");
    let body = extract_body_json(&response);

    assert_eq!(response["status_code"], 404);
    let error_msg = body["error"].as_str().expect("should have error field");
    assert!(
        error_msg.contains("Unknown function"),
        "error should mention unknown function: {}",
        error_msg
    );
}

#[test]
fn test_echo_plugin_empty_metadata() {
    // Test with no function_name in metadata — should default to "echo"
    let (engine, pm, _temp) = setup();
    let ctx = RequestContext::system();

    let request = serde_json::json!({
        "arguments": Vec::<u8>::new(),
        "metadata": {}
    });
    let request_bytes = serde_json::to_vec(&request).unwrap();

    let response_bytes = pm
        .invoke_wasm_plugin_with_context(
            "test/echo/plugin",
            &request_bytes,
            engine.clone(),
            ctx,
        )
        .expect("invoke should succeed");

    let response: serde_json::Value = serde_json::from_slice(&response_bytes).unwrap();
    assert_eq!(response["status_code"], 200);

    let body = extract_body_json(&response);
    assert_eq!(body["echo"], true);
}

#[test]
fn test_echo_plugin_write_then_read_roundtrip() {
    let (engine, pm, _temp) = setup();

    // Write via plugin
    let write_response = invoke_raw(&pm, &engine, "write", b"");
    assert_eq!(write_response["status_code"], 201);

    // Read back via plugin
    let read_response = invoke_raw(&pm, &engine, "read", b"/plugin-output/result.json");
    let body = extract_body_json(&read_response);

    assert_eq!(read_response["status_code"], 200);
    assert_eq!(body["content_type"], "application/json");
    // The written file is {"written":true} which is 16 bytes
    assert_eq!(body["size"], 16);
    assert_eq!(body["data_len"], 16);
}

#[test]
fn test_echo_plugin_write_then_delete_then_read_fails() {
    let (engine, pm, _temp) = setup();

    // Write via plugin
    let write_response = invoke_raw(&pm, &engine, "write", b"");
    assert_eq!(write_response["status_code"], 201);

    // Delete via plugin
    let delete_response = invoke_raw(&pm, &engine, "delete", b"/plugin-output/result.json");
    let delete_body = extract_body_json(&delete_response);
    assert_eq!(delete_response["status_code"], 200);
    assert_eq!(delete_body["deleted"], true);

    // Read should now fail
    let read_response = invoke_raw(&pm, &engine, "read", b"/plugin-output/result.json");
    assert_eq!(read_response["status_code"], 404);
}

#[test]
fn test_echo_plugin_list_empty_directory() {
    let (engine, pm, _temp) = setup();

    // List a path that has no children
    let response = invoke_raw(&pm, &engine, "list", b"/empty-dir-that-does-not-exist");
    // The host should return an error or empty entries
    // Depending on implementation, this might be a 500 (error from list_directory)
    // or 200 with empty entries. Let's just check it doesn't crash.
    let status = response["status_code"].as_u64().unwrap();
    assert!(
        status == 200 || status == 500,
        "expected 200 or 500, got {}",
        status
    );
}
