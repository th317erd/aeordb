use std::collections::HashMap;
use std::sync::Arc;

use aeordb::engine::btree::{BTreeNode, InternalNode, LeafNode, store_btree_node};
use aeordb::engine::directory_entry::ChildEntry;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::{EntryType, RequestContext};
use aeordb::engine::version_access::resolve_file_at_version;
use aeordb::engine::version_manager::VersionManager;
use aeordb::plugins::plugin_manager::PluginManager;
use aeordb::plugins::types::PluginType;
use aeordb::server::create_temp_engine_for_tests;
use aeordb::server::legacy_v3_root_adapter::LegacyV3SelectedRootAdapterV1;
use aeordb::server::root_api::RequestedRootSelectorV1;
use aeordb_plugin_sdk::root::PluginNamespaceReadInvocationV1;
use base64::Engine as _;

fn host_call_wasm(import_name: &str, arguments: &[u8]) -> Vec<u8> {
  let encoded_arguments = arguments.iter().map(|byte| format!("\\{byte:02x}")).collect::<String>();
  wat::parse_str(format!(
    r#"
    (module
      (import "aeordb" "{import_name}" (func $host (param i32 i32) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "{encoded_arguments}")
      (func (export "handle") (param i32) (param i32) (result i64)
        (call $host (i32.const 4096) (i32.const {}))
      )
    )
    "#,
    arguments.len(),
  ))
  .unwrap()
}

fn echo_request_wasm() -> Vec<u8> {
  wat::parse_str(
    r#"
    (module
      (memory (export "memory") 1)
      (func (export "handle") (param $pointer i32) (param $length i32) (result i64)
        (i64.or
          (i64.shl (i64.extend_i32_u (local.get $pointer)) (i64.const 32))
          (i64.extend_i32_u (local.get $length))
        )
      )
    )
    "#,
  )
  .unwrap()
}

fn read_write_read_wasm(read_arguments: &[u8], write_arguments: &[u8]) -> Vec<u8> {
  let encoded_read_arguments = read_arguments.iter().map(|byte| format!("\\{byte:02x}")).collect::<String>();
  let encoded_write_arguments = write_arguments.iter().map(|byte| format!("\\{byte:02x}")).collect::<String>();
  wat::parse_str(format!(
    r#"
    (module
      (import "aeordb" "aeordb_read_file" (func $read (param i32 i32) (result i64)))
      (import "aeordb" "aeordb_write_file" (func $write (param i32 i32) (result i64)))
      (memory (export "memory") 1)
      (data (i32.const 4096) "{encoded_read_arguments}")
      (data (i32.const 8192) "{encoded_write_arguments}")
      (func (export "handle") (param i32) (param i32) (result i64)
        (drop (call $read (i32.const 4096) (i32.const {})))
        (drop (call $write (i32.const 8192) (i32.const {})))
        (call $read (i32.const 4096) (i32.const {}))
      )
    )
    "#,
    read_arguments.len(),
    write_arguments.len(),
    read_arguments.len(),
  ))
  .unwrap()
}

fn deploy_host_plugin(manager: &PluginManager, path: &str, import_name: &str, arguments: &[u8]) {
  manager.deploy_plugin(path, path, PluginType::Wasm, host_call_wasm(import_name, arguments)).unwrap();
}

fn invoke_host_plugin(
  manager: &PluginManager,
  engine: &Arc<StorageEngine>,
  path: &str,
  invocation: PluginNamespaceReadInvocationV1,
  context: RequestContext,
) -> serde_json::Value {
  let response = manager
    .invoke_wasm_plugin_with_context(path, b"{}", invocation, engine.clone(), context)
    .expect("host import invocation must return a bounded JSON response");
  serde_json::from_slice(&response).expect("host import response must be JSON")
}

fn write_permissions(engine: &StorageEngine, directory_path: &str, permissions: &PathPermissions) {
  let permission_path = format!("{}/.aeordb-permissions", directory_path.trim_end_matches('/'));
  DirectoryOps::new(engine)
    .store_file_buffered(&RequestContext::system(), &permission_path, &permissions.serialize(), Some("application/json"))
    .unwrap();
}

fn others_permission(allow: Option<&str>, deny: Option<&str>) -> PathPermissions {
  PathPermissions {
    links: vec![PermissionLink {
      group: "not-a-member".to_string(),
      allow: "--------".to_string(),
      deny: "--------".to_string(),
      others_allow: allow.map(str::to_string),
      others_deny: deny.map(str::to_string),
      path_pattern: None,
    }],
  }
}

#[test]
fn historical_read_uses_root_x_after_head_y_and_returns_exact_metadata() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/docs/value.txt", b"root-x", Some("text/plain")).unwrap();
  let root_x = engine.head_hash().unwrap();
  let root_x_hex = hex::encode(&root_x);

  operations.store_file_buffered(&system, "/docs/value.txt", b"head-y", Some("text/plain")).unwrap();
  assert_ne!(engine.head_hash().unwrap(), root_x);

  let manager = PluginManager::new(engine.clone());
  let arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/docs/value.txt",
    "root_hash": root_x_hex,
  }))
  .unwrap();
  deploy_host_plugin(&manager, "test/root-read", "aeordb_read_file", &arguments);

  let response = invoke_host_plugin(
    &manager,
    &engine,
    "test/root-read",
    PluginNamespaceReadInvocationV1::root_hash(hex::encode(&root_x)).unwrap(),
    system,
  );
  assert_eq!(base64::engine::general_purpose::STANDARD.decode(response["data"].as_str().unwrap()).unwrap(), b"root-x");
  assert_eq!(response["root"]["hash"], hex::encode(root_x));
  assert_eq!(response["root"]["state"], "retained");
}

#[test]
fn manager_overwrites_guest_visible_selector_with_host_owned_invocation() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let manager = PluginManager::new(engine.clone());
  manager.deploy_plugin("request-echo", "test/request-echo", PluginType::Wasm, echo_request_wasm()).unwrap();
  let selected = engine.head_hash().unwrap();
  let request = serde_json::to_vec(&serde_json::json!({
    "arguments": [],
    "metadata": {},
    "root_hash": "ab".repeat(32),
  }))
  .unwrap();

  let response = manager
    .invoke_wasm_plugin_with_context(
      "test/request-echo",
      &request,
      PluginNamespaceReadInvocationV1::root_hash(hex::encode(&selected)).unwrap(),
      engine,
      RequestContext::system(),
    )
    .unwrap();
  let echoed: serde_json::Value = serde_json::from_slice(&response).unwrap();
  assert_eq!(echoed["root_hash"], hex::encode(selected));
}

#[test]
fn mismatched_guest_selector_is_rejected_without_falling_back_to_head() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&RequestContext::system(), "/docs/value.txt", b"current", Some("text/plain")).unwrap();
  let manager = PluginManager::new(engine.clone());
  let arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/docs/value.txt",
    "root_hash": "ab".repeat(32),
  }))
  .unwrap();
  deploy_host_plugin(&manager, "test/root-mismatch", "aeordb_read_file", &arguments);

  let response =
    invoke_host_plugin(&manager, &engine, "test/root-mismatch", PluginNamespaceReadInvocationV1::current(), RequestContext::system());
  assert!(response["error"].as_str().is_some_and(|message| message.contains("does not match")), "unexpected response: {response}");
}

#[test]
fn historical_write_and_delete_are_rejected_without_resolving_the_root() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/mutation/delete.txt", b"keep", Some("text/plain")).unwrap();
  let unavailable_root = "ab".repeat(32);
  let manager = PluginManager::new(engine.clone());

  let cases = [
    (
      "test/root-write",
      "aeordb_write_file",
      serde_json::json!({
        "path": "/mutation/write.txt",
        "data": base64::engine::general_purpose::STANDARD.encode(b"must-not-write"),
        "content_type": "text/plain",
        "root_hash": unavailable_root,
      }),
    ),
    (
      "test/root-delete",
      "aeordb_delete_file",
      serde_json::json!({
        "path": "/mutation/delete.txt",
        "root_hash": unavailable_root,
      }),
    ),
  ];

  for (path, import_name, arguments) in cases {
    deploy_host_plugin(&manager, path, import_name, &serde_json::to_vec(&arguments).unwrap());
    let response = invoke_host_plugin(
      &manager,
      &engine,
      path,
      PluginNamespaceReadInvocationV1::root_hash(unavailable_root.clone()).unwrap(),
      system.clone(),
    );
    assert!(response["error"].as_str().is_some_and(|message| message.contains("current root")), "unexpected response: {response}");
    assert!(!response["error"].as_str().unwrap().contains("HISTORICAL_VIEW_UNAVAILABLE"));
  }

  assert!(operations.read_file_buffered("/mutation/write.txt").is_err());
  assert_eq!(operations.read_file_buffered("/mutation/delete.txt").unwrap(), b"keep");
}

#[test]
fn every_historical_read_import_uses_root_x_after_head_y() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/history/value.txt", b"alpha\nbeta\n", Some("text/plain")).unwrap();
  operations.store_file_buffered(&system, "/history/x-only.txt", b"x", Some("text/plain")).unwrap();
  let root_x = engine.head_hash().unwrap();
  let root_x_hex = hex::encode(&root_x);

  operations.store_file_buffered(&system, "/history/value.txt", b"head-y", Some("text/plain")).unwrap();
  operations.delete_file(&system, "/history/x-only.txt").unwrap();
  operations.store_file_buffered(&system, "/history/y-only.txt", b"y", Some("text/plain")).unwrap();
  assert_ne!(engine.head_hash().unwrap(), root_x);

  let manager = PluginManager::new(engine.clone());
  let cases = [
    (
      "test/root-extract",
      "aeordb_extract_file",
      serde_json::json!({
        "path": "/history/value.txt",
        "mode": "lines",
        "start": 2,
        "end": 2,
        "root_hash": root_x_hex,
      }),
    ),
    ("test/root-metadata", "aeordb_file_metadata", serde_json::json!({"path": "/history/value.txt", "root_hash": root_x_hex})),
    ("test/root-list", "aeordb_list_directory", serde_json::json!({"path": "/history", "limit": 100, "root_hash": root_x_hex})),
    (
      "test/root-query",
      "aeordb_query",
      serde_json::json!({
        "path": "/history",
        "where": {"field": "@path", "op": "contains", "value": "/history/"},
        "limit": 100,
        "include_total": true,
        "root_hash": root_x_hex,
      }),
    ),
    (
      "test/root-aggregate",
      "aeordb_aggregate",
      serde_json::json!({
        "path": "/history",
        "where": {"field": "@path", "op": "contains", "value": "/history/"},
        "aggregate": {"count": true},
        "root_hash": root_x_hex,
      }),
    ),
  ];

  let mut responses = Vec::new();
  for (path, import_name, arguments) in cases {
    deploy_host_plugin(&manager, path, import_name, &serde_json::to_vec(&arguments).unwrap());
    let response =
      invoke_host_plugin(&manager, &engine, path, PluginNamespaceReadInvocationV1::root_hash(root_x_hex.clone()).unwrap(), system.clone());
    assert_eq!(response["root"]["hash"], root_x_hex, "{import_name} response: {response}");
    assert_eq!(response["root"]["state"], "retained", "{import_name} response: {response}");
    responses.push(response);
  }

  assert_eq!(responses[0]["text"], "beta\n");
  assert_eq!(responses[0]["source_size"], 11);
  assert_eq!(responses[1]["size"], 11);
  let listed_names = responses[2]["entries"].as_array().unwrap().iter().map(|entry| entry["name"].as_str().unwrap()).collect::<Vec<_>>();
  assert!(listed_names.contains(&"x-only.txt"));
  assert!(!listed_names.contains(&"y-only.txt"));
  let query_paths = responses[3]["items"].as_array().unwrap().iter().map(|item| item["path"].as_str().unwrap()).collect::<Vec<_>>();
  assert!(query_paths.contains(&"/history/x-only.txt"), "historical query response: {}", responses[3]);
  assert!(!query_paths.contains(&"/history/y-only.txt"), "historical query response: {}", responses[3]);
  assert_eq!(responses[3]["total"], 2);
  assert_eq!(responses[4]["count"], 2);
}

#[test]
fn one_invocation_reuses_its_first_resolved_root_after_a_current_mutation() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/cache/value.txt", b"captured", Some("text/plain")).unwrap();
  let captured_root = engine.head_hash().unwrap();
  let read_arguments = serde_json::to_vec(&serde_json::json!({"path": "/cache/value.txt"})).unwrap();
  let write_arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/cache/value.txt",
    "data": base64::engine::general_purpose::STANDARD.encode(b"new-head"),
    "content_type": "text/plain",
  }))
  .unwrap();

  let manager = PluginManager::new(engine.clone());
  manager
    .deploy_plugin("read-write-read", "test/read-write-read", PluginType::Wasm, read_write_read_wasm(&read_arguments, &write_arguments))
    .unwrap();
  let response = invoke_host_plugin(&manager, &engine, "test/read-write-read", PluginNamespaceReadInvocationV1::current(), system);

  assert_eq!(base64::engine::general_purpose::STANDARD.decode(response["data"].as_str().unwrap()).unwrap(), b"captured");
  assert_eq!(response["root"]["hash"], hex::encode(captured_root));
  assert_eq!(operations.read_file_buffered("/cache/value.txt").unwrap(), b"new-head");
}

#[test]
fn current_authorization_denial_precedes_unavailable_root_resolution() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/private/value.txt", b"secret", Some("text/plain")).unwrap();
  let unavailable_root = "a6".repeat(32);
  let manager = PluginManager::new(engine.clone());
  let arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/private/value.txt",
    "root_hash": unavailable_root,
  }))
  .unwrap();
  deploy_host_plugin(&manager, "test/current-denial", "aeordb_read_file", &arguments);
  let user_context = RequestContext::from_claims(&uuid::Uuid::new_v4().to_string(), Arc::new(aeordb::engine::EventBus::new()));

  let response = invoke_host_plugin(
    &manager,
    &engine,
    "test/current-denial",
    PluginNamespaceReadInvocationV1::root_hash(unavailable_root).unwrap(),
    user_context,
  );
  let error = response["error"].as_str().unwrap();
  assert!(error.contains("Permission denied"), "unexpected response: {response}");
  assert!(!error.contains("HISTORICAL_VIEW_UNAVAILABLE"), "selected root leaked through current denial: {response}");
}

#[test]
fn selected_permission_documents_restrict_but_do_not_expand_current_authority() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/secure/value.txt", b"selected secret", Some("text/plain")).unwrap();
  write_permissions(&engine, "/secure", &others_permission(None, Some("-r------")));
  let selected_root = engine.head_hash().unwrap();
  let selected_root_hex = hex::encode(&selected_root);
  write_permissions(&engine, "/secure", &others_permission(Some("-r------"), None));

  let manager = PluginManager::new(engine.clone());
  let arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/secure/value.txt",
    "root_hash": selected_root_hex,
  }))
  .unwrap();
  deploy_host_plugin(&manager, "test/selected-denial", "aeordb_read_file", &arguments);
  let invocation = PluginNamespaceReadInvocationV1::root_hash(selected_root_hex).unwrap();
  let user_context = RequestContext::from_claims(&uuid::Uuid::new_v4().to_string(), Arc::new(aeordb::engine::EventBus::new()));

  let denied = invoke_host_plugin(&manager, &engine, "test/selected-denial", invocation.clone(), user_context);
  assert!(denied["error"].as_str().is_some_and(|error| error.contains("Permission denied")), "unexpected response: {denied}");

  let allowed = invoke_host_plugin(&manager, &engine, "test/selected-denial", invocation, system);
  assert_eq!(base64::engine::general_purpose::STANDARD.decode(allowed["data"].as_str().unwrap()).unwrap(), b"selected secret");
  assert_eq!(allowed["root"]["hash"], hex::encode(selected_root));
}

#[test]
fn malformed_unavailable_and_non_namespace_roots_fail_without_head_fallback() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/roots/value.txt", b"current", Some("text/plain")).unwrap();
  let current_root = engine.head_hash().unwrap();
  let (file_record_hash, _) = resolve_file_at_version(&engine, &current_root, "/roots/value.txt").unwrap();
  let manager = PluginManager::new(engine.clone());
  let cases = [
    (
      "test/malformed-root",
      serde_json::json!({"path": "/roots/value.txt", "root_hash": "xyz"}),
      PluginNamespaceReadInvocationV1::current(),
      "Invalid plugin root selector",
    ),
    (
      "test/conflicting-root",
      serde_json::json!({"path": "/roots/value.txt", "root_hash": hex::encode(&current_root), "snapshot": "other"}),
      PluginNamespaceReadInvocationV1::current(),
      "mutually exclusive",
    ),
    (
      "test/unavailable-root",
      serde_json::json!({"path": "/roots/value.txt", "root_hash": "a7".repeat(32)}),
      PluginNamespaceReadInvocationV1::root_hash("a7".repeat(32)).unwrap(),
      "HISTORICAL_VIEW_UNAVAILABLE",
    ),
    (
      "test/non-namespace-root",
      serde_json::json!({"path": "/roots/value.txt", "root_hash": hex::encode(&file_record_hash)}),
      PluginNamespaceReadInvocationV1::root_hash(hex::encode(&file_record_hash)).unwrap(),
      "INVALID_NAMESPACE_ROOT",
    ),
  ];

  for (path, arguments, invocation, expected) in cases {
    deploy_host_plugin(&manager, path, "aeordb_read_file", &serde_json::to_vec(&arguments).unwrap());
    let response = invoke_host_plugin(&manager, &engine, path, invocation, system.clone());
    assert!(response["error"].as_str().is_some_and(|error| error.contains(expected)), "{path} response: {response}");
    assert!(response.get("data").is_none(), "{path} fell back to current HEAD: {response}");
  }
}

#[test]
fn oversized_historical_read_returns_a_bounded_error() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/large/value.bin", &vec![b'x'; 60 * 1024], Some("application/octet-stream")).unwrap();
  let selected_root = engine.head_hash().unwrap();
  let selected_root_hex = hex::encode(&selected_root);
  operations.store_file_buffered(&system, "/large/advance.txt", b"head-y", Some("text/plain")).unwrap();
  let manager = PluginManager::new(engine.clone());
  let arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/large/value.bin",
    "root_hash": selected_root_hex,
  }))
  .unwrap();
  deploy_host_plugin(&manager, "test/large-historical-read", "aeordb_read_file", &arguments);

  let response = invoke_host_plugin(
    &manager,
    &engine,
    "test/large-historical-read",
    PluginNamespaceReadInvocationV1::root_hash(selected_root_hex).unwrap(),
    system,
  );
  assert!(
    response["error"].as_str().is_some_and(|error| error.contains("response is too large") && error.contains("aeordb_extract_file")),
    "unexpected response: {response}",
  );
  assert!(response.get("data").is_none());
}

#[test]
fn selected_root_list_rejects_duplicate_names_across_btree_leaves() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/duplicate.txt", b"first", Some("text/plain")).unwrap();
  let first_root = engine.head_hash().unwrap();
  let first_file = LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::ExplicitRoot(first_root))
    .unwrap()
    .file("/duplicate.txt")
    .unwrap();
  operations.store_file_buffered(&system, "/duplicate.txt", b"second", Some("text/plain")).unwrap();
  let second_file =
    LegacyV3SelectedRootAdapterV1::resolve(&engine, &RequestedRootSelectorV1::CurrentHead).unwrap().file("/duplicate.txt").unwrap();

  let child = |record_hash: Vec<u8>, total_size: u64| ChildEntry {
    entry_type: EntryType::FileRecord.to_u8(),
    hash: record_hash,
    total_size,
    created_at: 1,
    updated_at: 1,
    name: "duplicate.txt".to_string(),
    content_type: Some("text/plain".to_string()),
    virtual_time: 1,
    node_id: 1,
  };
  let hash_length = engine.hash_algo().hash_length();
  let first_leaf = BTreeNode::Leaf(LeafNode { entries: vec![child(first_file.record_hash, first_file.record.total_size)] });
  let second_leaf = BTreeNode::Leaf(LeafNode { entries: vec![child(second_file.record_hash, second_file.record.total_size)] });
  let first_leaf_hash = store_btree_node(&engine, &first_leaf, hash_length, &engine.hash_algo()).unwrap();
  let second_leaf_hash = store_btree_node(&engine, &second_leaf, hash_length, &engine.hash_algo()).unwrap();
  let corrupt_root =
    BTreeNode::Internal(InternalNode { keys: vec!["duplicate.txt".to_string()], children: vec![first_leaf_hash, second_leaf_hash] });
  let corrupt_root_hash = store_btree_node(&engine, &corrupt_root, hash_length, &engine.hash_algo()).unwrap();
  let corrupt_root_hex = hex::encode(&corrupt_root_hash);

  let manager = PluginManager::new(engine.clone());
  let arguments = serde_json::to_vec(&serde_json::json!({
    "path": "/",
    "limit": 10,
    "root_hash": corrupt_root_hex,
  }))
  .unwrap();
  deploy_host_plugin(&manager, "test/duplicate-selected-list", "aeordb_list_directory", &arguments);
  let response = invoke_host_plugin(
    &manager,
    &engine,
    "test/duplicate-selected-list",
    PluginNamespaceReadInvocationV1::root_hash(corrupt_root_hex).unwrap(),
    system,
  );
  assert!(
    response["error"].as_str().is_some_and(|error| error.contains("duplicate child name")),
    "selected-root list accepted duplicate B-tree child names: {response}",
  );
}

#[test]
fn snapshot_and_version_selectors_resolve_the_same_exact_root() {
  let (engine, _temporary) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  let system = RequestContext::system();
  operations.store_file_buffered(&system, "/aliases/value.txt", b"root-x", Some("text/plain")).unwrap();
  let root_x = engine.head_hash().unwrap();
  let root_x_hex = hex::encode(&root_x);
  let snapshot = VersionManager::new(&engine).create_snapshot(&system, "plugin-root-x", HashMap::new()).unwrap();
  assert_eq!(snapshot.root_hash, root_x);
  operations.store_file_buffered(&system, "/aliases/value.txt", b"head-y", Some("text/plain")).unwrap();

  let manager = PluginManager::new(engine.clone());
  let cases = [
    (
      "test/snapshot-root",
      serde_json::json!({"path": "/aliases/value.txt", "snapshot": "plugin-root-x"}),
      PluginNamespaceReadInvocationV1::snapshot("plugin-root-x").unwrap(),
    ),
    (
      "test/version-root",
      serde_json::json!({"path": "/aliases/value.txt", "version": root_x_hex}),
      PluginNamespaceReadInvocationV1::version(root_x_hex.clone()).unwrap(),
    ),
  ];

  for (path, arguments, invocation) in cases {
    deploy_host_plugin(&manager, path, "aeordb_read_file", &serde_json::to_vec(&arguments).unwrap());
    let response = invoke_host_plugin(&manager, &engine, path, invocation, system.clone());
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(response["data"].as_str().unwrap()).unwrap(), b"root-x");
    assert_eq!(response["root"]["hash"], root_x_hex);
    assert_eq!(response["root"]["state"], "retained");
  }
}

#[test]
fn runtime_architecture_binds_six_reads_and_two_mutations_without_registry_growth() {
  let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let runtime = std::fs::read_to_string(manifest.join("src/plugins/wasm_runtime.rs")).unwrap();
  let manager = std::fs::read_to_string(manifest.join("src/plugins/plugin_manager.rs")).unwrap();

  let marker = ".func_wrap(\"aeordb\", \"";
  assert_eq!(runtime.matches(marker).count(), 9);
  assert_eq!(runtime.matches("\"root\": prepared.root.root()").count(), 4);
  assert_eq!(runtime.matches("serde_json::json!(prepared.root.root())").count(), 2);

  for import_name in
    ["aeordb_read_file", "aeordb_extract_file", "aeordb_file_metadata", "aeordb_list_directory", "aeordb_query", "aeordb_aggregate"]
  {
    let start = runtime.find(&format!("{marker}{import_name}\"")).unwrap();
    let tail = &runtime[start..];
    let end = tail[marker.len()..].find(marker).map_or(tail.len(), |offset| marker.len() + offset);
    let host_function = &tail[..end];
    assert!(host_function.contains("prepare_plugin_read"), "{import_name} bypasses the selected-root preparation boundary");
    assert!(!host_function.contains("DirectoryOps::new"), "{import_name} falls back to mutable DirectoryOps");
  }

  for import_name in ["aeordb_write_file", "aeordb_delete_file"] {
    let start = runtime.find(&format!("{marker}{import_name}\"")).unwrap();
    let tail = &runtime[start..];
    let end = tail[marker.len()..].find(marker).map_or(tail.len(), |offset| marker.len() + offset);
    let host_function = &tail[..end];
    assert!(host_function.contains("authorize_plugin_mutation"));
    assert!(!host_function.contains("resolve_plugin_root"));
  }

  assert!(manager.contains("request.root = root_invocation.clone();"));
  assert!(runtime.contains("selected.visit_directory_strict(&path"));
  assert!(!runtime.contains("selected.list_directory(&path)"));
  assert_eq!(manager.matches("root_invocation: PluginNamespaceReadInvocationV1").count(), 4);
  assert_eq!(runtime.matches("root_invocation: PluginNamespaceReadInvocationV1").count(), 2);
}
