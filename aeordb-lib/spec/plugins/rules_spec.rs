use aeordb::plugins::rule_engine::{combine_decisions, RuleEngine};
use aeordb::plugins::types::{PluginType, RuleContext, RuleDecision};
use aeordb::plugins::PluginManager;
use aeordb::server::create_temp_engine_for_tests;

fn test_manager() -> (PluginManager, tempfile::TempDir) {
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let plugin_manager = PluginManager::new(engine);
  (plugin_manager, temp_dir)
}

fn sample_context() -> RuleContext {
  RuleContext {
    user_subject: "test-user".to_string(),
    user_roles: vec!["user".to_string()],
    operation: "read".to_string(),
    database: "mydb".to_string(),
    schema: "public".to_string(),
    table: "users".to_string(),
    column: None,
  }
}

/// A minimal valid WASM module (does nothing useful — will fail invocation
/// but the rule engine gracefully handles that).
fn dummy_wasm_bytes() -> Vec<u8> {
  wat::parse_str(
    r#"
    (module
      (memory (export "memory") 1)
      (func (export "handle") (param i32 i32) (result i64)
        (i64.const 0)
      )
    )
    "#,
  )
  .expect("valid WAT")
}

#[test]
fn test_no_rules_means_allow() {
  let (plugin_manager, _temp_dir) = test_manager();
  let engine = RuleEngine::new(&plugin_manager);
  let context = sample_context();

  let decision = engine.evaluate("mydb/public/users", &context).unwrap();
  assert_eq!(decision, RuleDecision::Allow);
}

#[test]
fn test_deploy_rule_plugin() {
  let (plugin_manager, _temp_dir) = test_manager();

  let result = plugin_manager.deploy_plugin(
    "test-rule",
    "mydb/public/users",
    PluginType::Rule,
    dummy_wasm_bytes(),
  );

  assert!(result.is_ok());
  let record = result.unwrap();
  assert_eq!(record.plugin_type, PluginType::Rule);
  assert_eq!(record.path, "mydb/public/users");
}

#[test]
fn test_rule_collected_from_hierarchy() {
  let (plugin_manager, _temp_dir) = test_manager();

  // Deploy a rule at the database level.
  plugin_manager
    .deploy_plugin(
      "db-level-rule",
      "mydb",
      PluginType::Rule,
      dummy_wasm_bytes(),
    )
    .unwrap();

  // Deploy a rule at the schema level.
  plugin_manager
    .deploy_plugin(
      "schema-level-rule",
      "mydb/public",
      PluginType::Rule,
      dummy_wasm_bytes(),
    )
    .unwrap();

  let engine = RuleEngine::new(&plugin_manager);
  let applicable = engine
    .collect_applicable_rules("mydb/public/users")
    .unwrap();

  // Both rules should apply (schema-level first since it's more specific).
  assert_eq!(applicable.len(), 2);
  assert_eq!(applicable[0], "mydb/public");
  assert_eq!(applicable[1], "mydb");
}

#[test]
fn test_rule_inherits_to_child_scopes() {
  let (plugin_manager, _temp_dir) = test_manager();

  plugin_manager
    .deploy_plugin(
      "parent-rule",
      "mydb",
      PluginType::Rule,
      dummy_wasm_bytes(),
    )
    .unwrap();

  let engine = RuleEngine::new(&plugin_manager);

  // Rule at "mydb" should apply to "mydb/public/users".
  let applicable = engine
    .collect_applicable_rules("mydb/public/users")
    .unwrap();
  assert_eq!(applicable.len(), 1);
  assert_eq!(applicable[0], "mydb");
}

#[test]
fn test_rule_does_not_apply_to_sibling_scopes() {
  let (plugin_manager, _temp_dir) = test_manager();

  plugin_manager
    .deploy_plugin(
      "sibling-rule",
      "mydb/private",
      PluginType::Rule,
      dummy_wasm_bytes(),
    )
    .unwrap();

  let engine = RuleEngine::new(&plugin_manager);

  // Rule at "mydb/private" should NOT apply to "mydb/public/users".
  let applicable = engine
    .collect_applicable_rules("mydb/public/users")
    .unwrap();
  assert!(applicable.is_empty());
}

#[test]
fn test_allow_decision_passes() {
  assert_eq!(
    combine_decisions(RuleDecision::Allow, RuleDecision::Allow),
    RuleDecision::Allow
  );
}

#[test]
fn test_deny_decision_blocks() {
  assert_eq!(
    combine_decisions(RuleDecision::Allow, RuleDecision::Deny),
    RuleDecision::Deny
  );
  assert_eq!(
    combine_decisions(RuleDecision::Deny, RuleDecision::Allow),
    RuleDecision::Deny
  );
  assert_eq!(
    combine_decisions(RuleDecision::Deny, RuleDecision::Deny),
    RuleDecision::Deny
  );
}

#[test]
fn test_most_restrictive_rule_wins() {
  // Deny > Redact > Allow
  assert_eq!(
    combine_decisions(RuleDecision::Redact, RuleDecision::Allow),
    RuleDecision::Redact
  );
  assert_eq!(
    combine_decisions(RuleDecision::Deny, RuleDecision::Redact),
    RuleDecision::Deny
  );
  assert_eq!(
    combine_decisions(RuleDecision::Allow, RuleDecision::Redact),
    RuleDecision::Redact
  );
}

#[test]
fn test_evaluate_with_no_rules_returns_allow() {
  let (plugin_manager, _temp_dir) = test_manager();
  let engine = RuleEngine::new(&plugin_manager);
  let context = sample_context();

  let decision = engine.evaluate("mydb/public/users", &context).unwrap();
  assert_eq!(decision, RuleDecision::Allow);
}

#[test]
fn test_evaluate_with_deployed_rules_does_not_crash() {
  let (plugin_manager, _temp_dir) = test_manager();

  // Deploy a rule that returns an empty response (which defaults to Allow).
  plugin_manager
    .deploy_plugin(
      "eval-rule",
      "mydb/public",
      PluginType::Rule,
      dummy_wasm_bytes(),
    )
    .unwrap();

  let engine = RuleEngine::new(&plugin_manager);
  let context = sample_context();

  // The dummy WASM returns 0 (empty response), so the engine defaults to Allow.
  let decision = engine.evaluate("mydb/public/users", &context).unwrap();
  assert_eq!(decision, RuleDecision::Allow);
}

#[test]
fn test_non_rule_plugins_ignored_by_rule_engine() {
  let (plugin_manager, _temp_dir) = test_manager();

  // Deploy a regular WASM plugin (not a rule).
  plugin_manager
    .deploy_plugin(
      "regular-plugin",
      "mydb/public",
      PluginType::Wasm,
      dummy_wasm_bytes(),
    )
    .unwrap();

  let engine = RuleEngine::new(&plugin_manager);
  let applicable = engine
    .collect_applicable_rules("mydb/public/users")
    .unwrap();
  assert!(applicable.is_empty(), "non-rule plugins should be ignored");
}

#[test]
fn test_rule_context_serializes_correctly() {
  let context = sample_context();
  let bytes = serde_json::to_vec(&context).unwrap();
  let deserialized: RuleContext = serde_json::from_slice(&bytes).unwrap();
  assert_eq!(deserialized.user_subject, "test-user");
  assert_eq!(deserialized.operation, "read");
  assert_eq!(deserialized.database, "mydb");
  assert_eq!(deserialized.column, None);
}

#[test]
fn test_rule_decision_serializes_correctly() {
  let allow_json = serde_json::to_string(&RuleDecision::Allow).unwrap();
  assert_eq!(allow_json, r#""allow""#);

  let deny_json = serde_json::to_string(&RuleDecision::Deny).unwrap();
  assert_eq!(deny_json, r#""deny""#);

  let redact_json = serde_json::to_string(&RuleDecision::Redact).unwrap();
  assert_eq!(redact_json, r#""redact""#);
}
