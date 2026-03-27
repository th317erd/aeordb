use std::sync::Arc;

use aeordb::plugins::plugin_manager::{PluginManager, PluginManagerError};
use aeordb::plugins::types::PluginType;

/// Compile a minimal valid WASM module for testing.
fn minimal_wasm_bytes() -> Vec<u8> {
  let wat = r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      (i64.or
        (i64.shl
          (i64.extend_i32_u (local.get $request_ptr))
          (i64.const 32)
        )
        (i64.extend_i32_u (local.get $request_len))
      )
    )
  )
  "#;
  wat::parse_str(wat).expect("WAT should be valid")
}

/// Create a fresh in-memory PluginManager.
fn test_manager() -> PluginManager {
  let backend = redb::backends::InMemoryBackend::new();
  let database = redb::Database::builder()
    .create_with_backend(backend)
    .expect("in-memory database");
  PluginManager::new(Arc::new(database))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_deploy_plugin_stores_in_database() {
  let manager = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  let record = manager
    .deploy_plugin("my_plugin", "db/schema/table", PluginType::Wasm, wasm_bytes.clone())
    .expect("deploy should succeed");

  assert_eq!(record.name, "my_plugin");
  assert_eq!(record.path, "db/schema/table");
  assert_eq!(record.plugin_type, PluginType::Wasm);
  assert!(!record.wasm_bytes.is_empty());
}

#[test]
fn test_get_deployed_plugin() {
  let manager = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager
    .deploy_plugin("my_plugin", "db/schema/table", PluginType::Wasm, wasm_bytes.clone())
    .expect("deploy should succeed");

  let retrieved = manager
    .get_plugin("db/schema/table")
    .expect("get should not error")
    .expect("plugin should exist");

  assert_eq!(retrieved.name, "my_plugin");
  assert_eq!(retrieved.path, "db/schema/table");
  assert_eq!(retrieved.wasm_bytes, wasm_bytes);
}

#[test]
fn test_list_deployed_plugins() {
  let manager = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager
    .deploy_plugin("plugin_a", "db/schema/alpha", PluginType::Wasm, wasm_bytes.clone())
    .expect("deploy alpha");
  manager
    .deploy_plugin("plugin_b", "db/schema/beta", PluginType::Wasm, wasm_bytes.clone())
    .expect("deploy beta");

  let plugins = manager.list_plugins().expect("list should succeed");
  assert_eq!(plugins.len(), 2);

  let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
  assert!(names.contains(&"plugin_a"));
  assert!(names.contains(&"plugin_b"));
}

#[test]
fn test_remove_deployed_plugin() {
  let manager = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager
    .deploy_plugin("doomed", "db/schema/doomed", PluginType::Wasm, wasm_bytes)
    .expect("deploy");

  let removed = manager
    .remove_plugin("db/schema/doomed")
    .expect("remove should not error");
  assert!(removed, "should return true when plugin existed");

  let after = manager
    .get_plugin("db/schema/doomed")
    .expect("get should not error");
  assert!(after.is_none(), "plugin should no longer exist");
}

#[test]
fn test_deploy_duplicate_path_overwrites() {
  let manager = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  let first = manager
    .deploy_plugin("v1", "db/schema/func", PluginType::Wasm, wasm_bytes.clone())
    .expect("first deploy");

  let second = manager
    .deploy_plugin("v2", "db/schema/func", PluginType::Wasm, wasm_bytes.clone())
    .expect("second deploy");

  // Should reuse the same plugin_id.
  assert_eq!(first.plugin_id, second.plugin_id);
  // But the name should be updated.
  assert_eq!(second.name, "v2");

  // Only one plugin should exist.
  let plugins = manager.list_plugins().expect("list");
  assert_eq!(plugins.len(), 1);
  assert_eq!(plugins[0].name, "v2");
}

#[test]
fn test_get_nonexistent_plugin_returns_none() {
  let manager = test_manager();

  let result = manager
    .get_plugin("nonexistent/path")
    .expect("get should not error");
  assert!(result.is_none());
}

#[test]
fn test_remove_nonexistent_plugin_returns_false() {
  let manager = test_manager();

  let removed = manager
    .remove_plugin("nonexistent/path")
    .expect("remove should not error");
  assert!(!removed, "should return false when plugin did not exist");
}

#[test]
fn test_list_empty_returns_empty_vec() {
  let manager = test_manager();

  let plugins = manager.list_plugins().expect("list should succeed");
  assert!(plugins.is_empty());
}

#[test]
fn test_deploy_invalid_wasm_rejected() {
  let manager = test_manager();
  let garbage = vec![0x00, 0x61, 0x73, 0x6d, 0xFF, 0xFF, 0xFF, 0xFF];

  let result = manager.deploy_plugin("bad", "db/schema/bad", PluginType::Wasm, garbage);
  assert!(result.is_err(), "should reject invalid WASM");
  match result.unwrap_err() {
    PluginManagerError::InvalidPlugin(_) => {}
    other => panic!("expected InvalidPlugin, got: {:?}", other),
  }
}

#[test]
fn test_invoke_wasm_plugin() {
  let manager = test_manager();
  let wasm_bytes = minimal_wasm_bytes();

  manager
    .deploy_plugin("echo", "db/schema/echo", PluginType::Wasm, wasm_bytes)
    .expect("deploy");

  let response = manager
    .invoke_wasm_plugin("db/schema/echo", b"hello")
    .expect("invoke should succeed");

  assert_eq!(response, b"hello");
}

#[test]
fn test_invoke_nonexistent_plugin_returns_not_found() {
  let manager = test_manager();

  let result = manager.invoke_wasm_plugin("missing/path", b"data");
  assert!(result.is_err());
  match result.unwrap_err() {
    PluginManagerError::NotFound(_) => {}
    other => panic!("expected NotFound, got: {:?}", other),
  }
}

#[test]
fn test_deploy_native_plugin_skips_wasm_validation() {
  let manager = test_manager();
  // For a native plugin, the bytes are just stored as-is (no WASM validation).
  let dummy_bytes = b"not real wasm but that is fine for native".to_vec();

  let record = manager
    .deploy_plugin("native_func", "db/schema/native", PluginType::Native, dummy_bytes.clone())
    .expect("deploy native should succeed");

  assert_eq!(record.plugin_type, PluginType::Native);
  assert_eq!(record.wasm_bytes, dummy_bytes);
}
