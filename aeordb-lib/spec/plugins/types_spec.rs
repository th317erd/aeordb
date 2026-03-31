use aeordb::plugins::types::{
  PluginType, RuleDecision, RuleContext, PluginMetadata,
  serialize_for_ffi, deserialize_from_ffi,
};

// ---------------------------------------------------------------------------
// PluginType Display
// ---------------------------------------------------------------------------

#[test]
fn test_plugin_type_display_wasm() {
  assert_eq!(format!("{}", PluginType::Wasm), "wasm");
}

#[test]
fn test_plugin_type_display_native() {
  assert_eq!(format!("{}", PluginType::Native), "native");
}

#[test]
fn test_plugin_type_display_rule() {
  assert_eq!(format!("{}", PluginType::Rule), "rule");
}

// ---------------------------------------------------------------------------
// PluginType FromStr
// ---------------------------------------------------------------------------

#[test]
fn test_plugin_type_from_str_wasm() {
  let parsed: PluginType = "wasm".parse().unwrap();
  assert_eq!(parsed, PluginType::Wasm);
}

#[test]
fn test_plugin_type_from_str_native() {
  let parsed: PluginType = "native".parse().unwrap();
  assert_eq!(parsed, PluginType::Native);
}

#[test]
fn test_plugin_type_from_str_rule() {
  let parsed: PluginType = "rule".parse().unwrap();
  assert_eq!(parsed, PluginType::Rule);
}

#[test]
fn test_plugin_type_from_str_unknown_returns_error() {
  let result: Result<PluginType, String> = "javascript".parse();
  assert!(result.is_err());
  let error_message = result.unwrap_err();
  assert!(
    error_message.contains("unknown plugin type"),
    "Error should mention 'unknown plugin type', got: {}",
    error_message,
  );
}

// ---------------------------------------------------------------------------
// PluginType serde roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_plugin_type_serde_roundtrip() {
  let types = vec![PluginType::Wasm, PluginType::Native, PluginType::Rule];
  for plugin_type in types {
    let serialized = serde_json::to_string(&plugin_type).unwrap();
    let deserialized: PluginType = serde_json::from_str(&serialized).unwrap();
    assert_eq!(plugin_type, deserialized);
  }
}

#[test]
fn test_plugin_type_serde_json_values() {
  assert_eq!(serde_json::to_string(&PluginType::Wasm).unwrap(), r#""wasm""#);
  assert_eq!(serde_json::to_string(&PluginType::Native).unwrap(), r#""native""#);
  assert_eq!(serde_json::to_string(&PluginType::Rule).unwrap(), r#""rule""#);
}

// ---------------------------------------------------------------------------
// RuleDecision serde
// ---------------------------------------------------------------------------

#[test]
fn test_rule_decision_serde_roundtrip() {
  let decisions = vec![RuleDecision::Allow, RuleDecision::Deny, RuleDecision::Redact];
  for decision in decisions {
    let serialized = serde_json::to_string(&decision).unwrap();
    let deserialized: RuleDecision = serde_json::from_str(&serialized).unwrap();
    assert_eq!(decision, deserialized);
  }
}

#[test]
fn test_rule_decision_json_values() {
  assert_eq!(serde_json::to_string(&RuleDecision::Allow).unwrap(), r#""allow""#);
  assert_eq!(serde_json::to_string(&RuleDecision::Deny).unwrap(), r#""deny""#);
  assert_eq!(serde_json::to_string(&RuleDecision::Redact).unwrap(), r#""redact""#);
}

// ---------------------------------------------------------------------------
// RuleContext serde
// ---------------------------------------------------------------------------

#[test]
fn test_rule_context_serde_roundtrip() {
  let context = RuleContext {
    user_subject: "user-123".to_string(),
    user_roles: vec!["admin".to_string(), "editor".to_string()],
    operation: "read".to_string(),
    database: "mydb".to_string(),
    schema: "public".to_string(),
    table: "users".to_string(),
    column: Some("email".to_string()),
  };

  let serialized = serde_json::to_string(&context).unwrap();
  let deserialized: RuleContext = serde_json::from_str(&serialized).unwrap();

  assert_eq!(deserialized.user_subject, "user-123");
  assert_eq!(deserialized.user_roles, vec!["admin", "editor"]);
  assert_eq!(deserialized.operation, "read");
  assert_eq!(deserialized.database, "mydb");
  assert_eq!(deserialized.schema, "public");
  assert_eq!(deserialized.table, "users");
  assert_eq!(deserialized.column, Some("email".to_string()));
}

#[test]
fn test_rule_context_with_null_column() {
  let context = RuleContext {
    user_subject: "user-456".to_string(),
    user_roles: vec![],
    operation: "write".to_string(),
    database: "testdb".to_string(),
    schema: "main".to_string(),
    table: "orders".to_string(),
    column: None,
  };

  let serialized = serde_json::to_string(&context).unwrap();
  let deserialized: RuleContext = serde_json::from_str(&serialized).unwrap();

  assert!(deserialized.column.is_none());
  assert!(deserialized.user_roles.is_empty());
}

// ---------------------------------------------------------------------------
// FFI serialize / deserialize
// ---------------------------------------------------------------------------

#[test]
fn test_serialize_for_ffi_rule_decision() {
  let decision = RuleDecision::Allow;
  let bytes = serialize_for_ffi(&decision).unwrap();
  let deserialized: RuleDecision = deserialize_from_ffi(&bytes).unwrap();
  assert_eq!(deserialized, RuleDecision::Allow);
}

#[test]
fn test_serialize_for_ffi_rule_context() {
  let context = RuleContext {
    user_subject: "ffi-user".to_string(),
    user_roles: vec!["reader".to_string()],
    operation: "delete".to_string(),
    database: "prod".to_string(),
    schema: "public".to_string(),
    table: "secrets".to_string(),
    column: None,
  };

  let bytes = serialize_for_ffi(&context).unwrap();
  let deserialized: RuleContext = deserialize_from_ffi(&bytes).unwrap();
  assert_eq!(deserialized.user_subject, "ffi-user");
  assert_eq!(deserialized.operation, "delete");
}

#[test]
fn test_deserialize_from_ffi_invalid_bytes_returns_error() {
  let invalid_bytes = b"not valid json at all";
  let result: Result<RuleDecision, _> = deserialize_from_ffi(invalid_bytes);
  assert!(result.is_err());
}

#[test]
fn test_serialize_for_ffi_plugin_metadata() {
  let metadata = PluginMetadata {
    plugin_id: uuid::Uuid::new_v4(),
    name: "test-plugin".to_string(),
    path: "db/schema/table".to_string(),
    plugin_type: PluginType::Wasm,
    created_at: chrono::Utc::now(),
  };

  let bytes = serialize_for_ffi(&metadata).unwrap();
  let deserialized: PluginMetadata = deserialize_from_ffi(&bytes).unwrap();
  assert_eq!(deserialized.name, "test-plugin");
  assert_eq!(deserialized.plugin_type, PluginType::Wasm);
}
