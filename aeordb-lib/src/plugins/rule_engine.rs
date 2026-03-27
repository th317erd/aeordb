use super::plugin_manager::{PluginManager, PluginManagerError};
use super::scoping::is_scope_accessible;
use super::types::{PluginType, RuleContext, RuleDecision};

/// Engine that collects and evaluates permission rule plugins.
pub struct RuleEngine<'a> {
  plugin_manager: &'a PluginManager,
}

impl<'a> RuleEngine<'a> {
  /// Create a new rule engine backed by the given plugin manager.
  pub fn new(plugin_manager: &'a PluginManager) -> Self {
    Self { plugin_manager }
  }

  /// Find all rule plugins that apply to the given scope path.
  ///
  /// Rules at parent scopes apply to children (inheritance).
  /// Rules are returned ordered from most specific (deepest) to least specific.
  pub fn collect_applicable_rules(
    &self,
    scope_path: &str,
  ) -> Result<Vec<String>, PluginManagerError> {
    let all_plugins = self.plugin_manager.list_plugins()?;

    let mut applicable: Vec<(usize, String)> = Vec::new();

    for plugin_metadata in &all_plugins {
      if plugin_metadata.plugin_type != PluginType::Rule {
        continue;
      }

      // A rule plugin applies if its path is accessible from the scope_path
      // (i.e., it's at the same level or a parent level).
      if is_scope_accessible(scope_path, &plugin_metadata.path) {
        let depth = plugin_metadata
          .path
          .split('/')
          .filter(|s| !s.is_empty())
          .count();
        applicable.push((depth, plugin_metadata.path.clone()));
      }
    }

    // Sort by depth descending (most specific first).
    applicable.sort_by(|a, b| b.0.cmp(&a.0));

    Ok(applicable.into_iter().map(|(_, path)| path).collect())
  }

  /// Evaluate all applicable rules for the given context.
  ///
  /// Returns the combined decision: if any rule says Deny, the result is Deny.
  /// If no rules exist, the default is Allow.
  ///
  /// For now this is a stub that collects applicable rules and combines
  /// their decisions. Actual WASM execution of rule plugins will use the
  /// existing WasmPluginRuntime with a "evaluate_rule" entry point in the future.
  pub fn evaluate(
    &self,
    scope_path: &str,
    context: &RuleContext,
  ) -> Result<RuleDecision, PluginManagerError> {
    let applicable_rule_paths = self.collect_applicable_rules(scope_path)?;

    if applicable_rule_paths.is_empty() {
      return Ok(RuleDecision::Allow);
    }

    // Evaluate each rule. For now, we attempt to invoke each rule plugin
    // with the serialized context. If invocation fails (e.g., the WASM module
    // doesn't have the right entry point yet), we default to Allow for that rule.
    let context_bytes = serde_json::to_vec(context)
      .map_err(|error| PluginManagerError::ExecutionFailed(error.to_string()))?;

    let mut most_restrictive = RuleDecision::Allow;

    for rule_path in &applicable_rule_paths {
      let decision = match self
        .plugin_manager
        .invoke_wasm_plugin(rule_path, &context_bytes)
      {
        Ok(response_bytes) => {
          // Try to parse the response as a RuleDecision.
          serde_json::from_slice::<RuleDecision>(&response_bytes)
            .unwrap_or(RuleDecision::Allow)
        }
        Err(_) => {
          // If the plugin can't be invoked, default to Allow.
          // This allows deploying rule plugin metadata before the WASM
          // implementation is ready.
          RuleDecision::Allow
        }
      };

      most_restrictive = combine_decisions(most_restrictive, decision);
    }

    Ok(most_restrictive)
  }
}

/// Combine two rule decisions, returning the most restrictive one.
///
/// Deny > Redact > Allow.
pub fn combine_decisions(left: RuleDecision, right: RuleDecision) -> RuleDecision {
  match (&left, &right) {
    (RuleDecision::Deny, _) | (_, RuleDecision::Deny) => RuleDecision::Deny,
    (RuleDecision::Redact, _) | (_, RuleDecision::Redact) => RuleDecision::Redact,
    _ => RuleDecision::Allow,
  }
}
