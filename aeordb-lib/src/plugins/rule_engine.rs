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
  pub fn collect_applicable_rules(&self, scope_path: &str) -> Result<Vec<String>, PluginManagerError> {
    let all_plugins = self.plugin_manager.list_plugins_accounted()?;

    let mut applicable: Vec<(usize, String)> = Vec::new();

    for plugin_metadata in all_plugins.as_slice() {
      if plugin_metadata.plugin_type != PluginType::Rule {
        continue;
      }

      // A rule plugin applies if its path is accessible from the scope_path
      // (i.e., it's at the same level or a parent level).
      if is_scope_accessible(scope_path, &plugin_metadata.path) {
        let depth = plugin_metadata.path.split('/').filter(|s| !s.is_empty()).count();
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
  /// Any deployed rule that cannot execute or return a valid decision fails
  /// the evaluation. A broken authorization rule must never become Allow.
  pub fn evaluate(&self, scope_path: &str, context: &RuleContext) -> Result<RuleDecision, PluginManagerError> {
    let applicable_rule_paths = self.collect_applicable_rules(scope_path)?;

    if applicable_rule_paths.is_empty() {
      return Ok(RuleDecision::Allow);
    }

    let context_bytes = serde_json::to_vec(context).map_err(|error| PluginManagerError::ExecutionFailed(error.to_string()))?;

    let mut most_restrictive = RuleDecision::Allow;

    for rule_path in &applicable_rule_paths {
      let response_bytes = self
        .plugin_manager
        .invoke_wasm_plugin(rule_path, &context_bytes)
        .map_err(|error| PluginManagerError::ExecutionFailed(format!("rule plugin '{rule_path}' invocation failed: {error}")))?;
      let decision = serde_json::from_slice::<RuleDecision>(&response_bytes).map_err(|error| {
        PluginManagerError::ExecutionFailed(format!("rule plugin '{rule_path}' returned an invalid rule response: {error}"))
      })?;

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
