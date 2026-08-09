use super::*;

#[test]
fn recovery_identity_without_controls_returns_a_typed_durability_failure() {
  let error = recovery_database_id(None, None).unwrap_err();

  assert!(matches!(error, EngineError::DurabilityFailure(ref message) if message.contains("disappeared")));
}

#[test]
fn contradictory_empty_recovery_state_returns_a_typed_durability_failure() {
  let error = recovery_reason(None, None, true, true, true).unwrap_err();

  assert!(matches!(error, EngineError::DurabilityFailure(ref message) if message.contains("disappeared")));
}

#[test]
fn persisted_durability_recovery_paths_contain_no_panic_macros() {
  let source = include_str!("../../src/engine/v4/durability_recovery.rs");

  assert!(!source.contains("unreachable!("));
  assert!(!source.contains(".expect("));
}
