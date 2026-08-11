use std::collections::BTreeSet;
use std::path::Path;

use aeordb_error_squelch_audit::{
  ReviewStatus, SuppressionClass, SuppressionInventory, SuppressionKind, SuppressionReview, load_inventory, production_scope,
  refreshed_inventory, reviewed_inventory, scan_source, scan_workspace, validate_inventory,
};

fn workspace_root() -> &'static Path {
  Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

#[test]
fn scanner_finds_supported_production_suppression_forms_and_excludes_test_only_items() {
  let source = r#"
fn production() {
  let _ = perform();
  _ = perform();
  let _candidate = perform().ok();
  let _defaulted = perform().unwrap_or_default();
  let _converted = perform().map_err(|_| "masked");
  let _recovered = perform().or_else(recover);
  let _forced = perform().expect("required");
  let _status = perform().is_err();
  let _variant = matches!(perform(), Err(_));
  if let Err(_) = perform() { tracing::warn!("ignored"); }
  if let Ok(value) = perform() { consume(value); }
  panic!("production panic");
}

#[cfg(test)]
mod tests {
  fn helper() { let _ = perform().unwrap(); }
}

#[test]
fn direct_test() { let _ = perform().unwrap(); }
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();
  let kinds: BTreeSet<_> = occurrences.iter().map(|occurrence| occurrence.kind).collect();

  for expected in [
    SuppressionKind::BroadErrPattern,
    SuppressionKind::DefaultOnError,
    SuppressionKind::DiscardedAssignment,
    SuppressionKind::ErrorConversion,
    SuppressionKind::ErrorRecovery,
    SuppressionKind::LoggedErrorContinues,
    SuppressionKind::PanicMacro,
    SuppressionKind::PanicMethod,
    SuppressionKind::ResultStatusProbe,
    SuppressionKind::ResultToOption,
    SuppressionKind::ResultVariantProbe,
    SuppressionKind::SuccessOnlyConditional,
  ] {
    assert!(kinds.contains(&expected), "scanner missed {expected:?}");
  }
  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::DiscardedAssignment).count(), 2);
  assert!(occurrences.iter().all(|occurrence| occurrence.enclosing_item == "production"));
}

#[test]
fn scanner_does_not_classify_explicit_process_termination_as_continuation() {
  let source = r#"
fn exits() {
  if let Err(error) = perform() {
    eprintln!("fatal: {error}");
    std::process::exit(1);
  }
}

fn aborts() {
  if let Err(error) = perform() {
    tracing::error!(%error, "fatal");
    std::process::abort();
  }
}
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();

  assert!(occurrences.iter().all(|occurrence| occurrence.kind != SuppressionKind::LoggedErrorContinues));
}

#[test]
fn scanner_finds_suppression_methods_nested_inside_macro_arguments() {
  let source = r#"
fn production() {
  let message = format!("value: {}", candidate.expect("candidate is present"));
  tracing::warn!(fallback = ?operation.ok(), %message, "operation failed");
  tracing::warn!(converted = ?operation.map_err(|_| "masked"), "operation failed");
}
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();

  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::PanicMethod).count(), 1);
  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::ResultToOption).count(), 1);
  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::ErrorConversion).count(), 1);
  assert!(occurrences.iter().all(|occurrence| occurrence.enclosing_item == "production"));
}

#[test]
fn scanner_does_not_flag_error_preserving_map_err_nested_inside_macro_arguments() {
  let source = r#"
fn production() {
  tracing::warn!(converted = ?operation.map_err(|error| wrap(error)), "operation failed");
}
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();

  assert!(occurrences.iter().all(|occurrence| occurrence.kind != SuppressionKind::ErrorConversion));
}

#[test]
fn scanner_finds_result_status_and_variant_probes_that_discard_failure_detail() {
  let source = r#"
fn production() {
  if operation().is_err() { handle_failure(); }
  if operation().is_ok() { handle_success(); }
  let available = matches!(operation(), Ok(Some(_)));
  tracing::warn!(failed = operation().is_err(), "operation state");
}
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();

  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::ResultStatusProbe).count(), 3);
  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::ResultVariantProbe).count(), 1);
}

#[test]
fn scanner_finds_result_let_else_that_discards_the_error_branch() {
  let source = r#"
fn production() {
  let Ok(value) = operation() else {
    return;
  };
  consume(value);
}
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();

  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::SuccessOnlyConditional).count(), 1);
  assert!(occurrences[0].pattern.contains("let Ok"));
}

#[test]
fn scanner_finds_named_but_unused_error_bindings_without_flagging_preserved_errors() {
  let source = r#"
fn production() {
  let _converted = operation().map_err(|error| "stable failure");
  let _also_converted = operation().map_err(|_error| stable_failure());
  let _preserved = operation().map_err(|error| wrap(error));
  let _implicitly_preserved = operation().map_err(|error| format!("operation failed: {error}"));
  match operation() {
    Ok(value) => consume(value),
    Err(_error) => reject(),
  }
}
"#;

  let occurrences = scan_source("fixture.rs", source).unwrap();

  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::ErrorConversion).count(), 2);
  assert_eq!(occurrences.iter().filter(|occurrence| occurrence.kind == SuppressionKind::BroadErrPattern).count(), 1);
}

#[test]
fn stable_identity_does_not_change_when_only_leading_lines_change() {
  let first = scan_source("fixture.rs", "fn production() { let _ = perform().ok(); }").unwrap();
  let shifted = scan_source("fixture.rs", "\n\nfn production() { let _ = perform().ok(); }").unwrap();

  assert_eq!(
    first.iter().map(|occurrence| &occurrence.id).collect::<Vec<_>>(),
    shifted.iter().map(|occurrence| &occurrence.id).collect::<Vec<_>>()
  );
  assert_ne!(first[0].line, shifted[0].line);
}

#[test]
fn validator_rejects_pending_duplicate_and_over_baseline_entries() {
  let discovered = scan_source("fixture.rs", "fn production() { let _ = perform(); }").unwrap();
  let occurrence = discovered[0].clone();
  let pending_review = SuppressionReview {
    review_status: ReviewStatus::Pending,
    class: SuppressionClass::DeliberatelyIgnored,
    rationale: "PENDING REVIEW: this is intentionally incomplete.".to_string(),
    owner: "PENDING".to_string(),
    test: "PENDING".to_string(),
    removal_condition: "PENDING REVIEW: replace this later.".to_string(),
  };
  let pending = aeordb_error_squelch_audit::ReviewedSuppression { occurrence: occurrence.clone(), review: "pending-review".to_string() };
  let inventory = SuppressionInventory {
    schema_version: 1,
    scanner: "aeordb-error-squelch-audit-v1".to_string(),
    scope: production_scope(),
    maximum_occurrences: 1,
    reviews: [("pending-review".to_string(), pending_review)].into_iter().collect(),
    entries: vec![pending.clone(), pending],
  };

  let errors = validate_inventory(&discovered, &inventory).join("\n");

  assert!(errors.contains("must exactly equal"));
  assert!(errors.contains("pending review"));
  assert!(errors.contains("duplicate identity"));
}

#[test]
fn validator_rejects_stale_metadata_unreviewed_occurrences_and_unused_reviews() {
  let discovered = scan_source("fixture.rs", "fn production() { let _ = perform(); let _ = other(); }").unwrap();
  let mut stale = discovered[0].clone();
  stale.line += 1;
  let reviewed = SuppressionReview {
    review_status: ReviewStatus::Reviewed,
    class: SuppressionClass::DeliberatelyIgnored,
    rationale: "The fixture deliberately discards a non-authoritative local result.".to_string(),
    owner: "architecture verification".to_string(),
    test: "error_squelch_architecture_spec".to_string(),
    removal_condition: "Remove when the fixture no longer exercises discarded assignments.".to_string(),
  };
  let inventory = SuppressionInventory {
    schema_version: 1,
    scanner: "aeordb-error-squelch-audit-v1".to_string(),
    scope: production_scope(),
    maximum_occurrences: 2,
    reviews: [("reviewed-fixture".to_string(), reviewed.clone()), ("unused-review".to_string(), reviewed)].into_iter().collect(),
    entries: vec![aeordb_error_squelch_audit::ReviewedSuppression { occurrence: stale, review: "reviewed-fixture".to_string() }],
  };

  let errors = validate_inventory(&discovered, &inventory).join("\n");

  assert!(errors.contains("metadata") && errors.contains("stale"));
  assert!(errors.contains("unreviewed suppression"));
  assert!(errors.contains("unused-review") && errors.contains("stale because no occurrence uses it"));
}

#[test]
fn validator_rejects_any_ceiling_above_the_exact_reviewed_inventory() {
  let discovered = scan_source("fixture.rs", "fn production() { let _value = perform().ok(); }").unwrap();
  let occurrence = discovered[0].clone();
  let reviewed = SuppressionReview {
    review_status: ReviewStatus::Reviewed,
    class: SuppressionClass::DeliberatelyIgnored,
    rationale: "The fixture deliberately converts one non-authoritative result into absence.".to_string(),
    owner: "architecture verification".to_string(),
    test: "error_squelch_architecture_spec".to_string(),
    removal_condition: "Remove when the fixture no longer exercises result-to-option conversion.".to_string(),
  };
  let inventory = SuppressionInventory {
    schema_version: 1,
    scanner: "aeordb-error-squelch-audit-v1".to_string(),
    scope: production_scope(),
    maximum_occurrences: 2,
    reviews: [("reviewed-fixture".to_string(), reviewed)].into_iter().collect(),
    entries: vec![aeordb_error_squelch_audit::ReviewedSuppression { occurrence, review: "reviewed-fixture".to_string() }],
  };

  let errors = validate_inventory(&discovered, &inventory).join("\n");

  assert!(errors.contains("must exactly equal"), "{errors}");
}

#[test]
fn refreshed_inventory_always_shrinks_the_ceiling_to_the_discovered_count() {
  let previous_discovered =
    scan_source("fixture.rs", "fn production() { let _first = perform().ok(); let _second = other().ok(); let _third = another().ok(); }")
      .unwrap();
  let previous = refreshed_inventory(&previous_discovered, None, false).unwrap();
  let discovered = scan_source("fixture.rs", "fn production() { let _first = perform().ok(); }").unwrap();

  let refreshed = refreshed_inventory(&discovered, Some(&previous), false).unwrap();

  assert_eq!(refreshed.maximum_occurrences, discovered.len());
  assert_eq!(refreshed.entries.len(), discovered.len());
}

#[test]
fn reviewed_inventory_assigns_named_semantic_policies() {
  let discovered =
    scan_source("aeordb-lib/src/engine/v4/example.rs", "fn decode() { let _value = operation().map_err(|_| typed_format_error()); }")
      .unwrap();

  let inventory = reviewed_inventory(&discovered).expect("known failure-preserving format conversion policy");

  assert_eq!(inventory.entries.len(), 1);
  assert_eq!(inventory.entries[0].review, "typed-format-failure");
  assert_eq!(inventory.reviews["typed-format-failure"].review_status, ReviewStatus::Reviewed);
  assert_eq!(inventory.reviews["typed-format-failure"].class, SuppressionClass::CorrectnessReadTraversal);
}

#[test]
fn v4_runtime_authority_errors_do_not_inherit_the_persistent_format_policy() {
  let discovered =
    scan_source("aeordb-lib/src/engine/v4/read_view.rs", "fn lock_state() { let _guard = state.lock().map_err(|_| authority_error()); }")
      .unwrap();

  let inventory = reviewed_inventory(&discovered).expect("known runtime authority failure policy");

  assert_eq!(inventory.entries.len(), 1);
  assert_eq!(inventory.entries[0].review, "authority-state-failure");
  assert_eq!(inventory.reviews["authority-state-failure"].review_status, ReviewStatus::Reviewed);
  assert_eq!(inventory.reviews["authority-state-failure"].class, SuppressionClass::DurabilityAuthority);
}

#[test]
fn reviewed_inventory_refuses_unknown_suppression_forms() {
  let discovered = scan_source("aeordb-lib/src/engine/example.rs", "fn mutate() { let _ = persist(); }").unwrap();

  let error = reviewed_inventory(&discovered).expect_err("discarded persistence results require a new explicit policy");

  assert!(error.contains("discarded_assignment"));
  assert!(error.contains("aeordb-lib/src/engine/example.rs"));
}

#[test]
fn inventory_decoder_rejects_unknown_classification_values() {
  let malformed = serde_json::json!({
    "schema_version": 1,
    "scanner": "aeordb-error-squelch-audit-v1",
    "scope": production_scope(),
    "maximum_occurrences": 1,
    "reviews": {
      "invalid": {
        "review_status": "reviewed",
        "class": "silently_ignore_everything",
        "rationale": "This intentionally uses an unknown classification value.",
        "owner": "architecture verification",
        "test": "error_squelch_architecture_spec",
        "removal_condition": "Remove when malformed classifications become accepted."
      }
    },
    "entries": []
  });

  let error = serde_json::from_value::<SuppressionInventory>(malformed).unwrap_err();

  assert!(error.to_string().contains("silently_ignore_everything"));
}

#[test]
fn repair_does_not_fabricate_a_zero_kv_length_when_layout_authority_is_unavailable() {
  let source = std::fs::read_to_string(workspace_root().join("aeordb-cli/src/commands/verify.rs")).unwrap();

  assert!(!source.contains("writer_read_lock().map(|writer| writer.file_header().kv_block_length).unwrap_or(0)"));
}

#[test]
fn hot_tail_production_api_does_not_convert_read_failures_into_absence() {
  let source = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/hot_tail.rs")).unwrap();

  assert!(!source.contains("pub fn deserialize_hot_tail("));
  assert!(!source.contains("pub fn read_hot_tail<"));
}

#[test]
fn validated_permission_reindex_and_kv_publication_state_never_panics() {
  let permission_middleware = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/auth/permission_middleware.rs")).unwrap();
  let task_worker = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/task_worker.rs")).unwrap();
  let kv_page_provider = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/kv_page_provider.rs")).unwrap();

  assert!(!permission_middleware.contains("user_id.unwrap()"));
  assert!(!task_worker.contains("expect(\"resolved config has an owner directory\")"));
  assert!(!task_worker.contains("expect(\"resolved glob config has an owner directory\")"));
  assert!(!task_worker.contains("expect(\"force creates migration memory\")"));
  assert!(!kv_page_provider.contains("expect(\"pending update exists\")"));
  assert!(!kv_page_provider.contains("expect(\"poison reason was set\")"));
}

#[test]
fn exhaustive_runtime_state_machines_do_not_retain_wildcard_panics() {
  let soak_worker = std::fs::read_to_string(workspace_root().join("aeordb-cli/src/bin/soak-worker.rs")).unwrap();
  let merge_patch = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/merge_patch.rs")).unwrap();

  assert!(!soak_worker.contains("_ => unreachable!()"));
  assert!(!merge_patch.contains("MergeDepth::FullReplace => unreachable!"));
}

#[test]
fn query_inputs_and_mutable_index_buffer_state_return_errors_instead_of_panicking() {
  let query_engine = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/query_engine.rs")).unwrap();
  let index_store = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/index_store.rs")).unwrap();

  assert!(!query_engine.contains("v.as_u64().unwrap()"));
  assert!(!query_engine.contains("v.as_i64().unwrap()"));
  assert!(!query_engine.contains("value.as_array().unwrap()"));
  assert!(!query_engine.contains("leaves.into_iter().next().unwrap()"));
  assert!(!query_engine.contains("bytes[..4].try_into().unwrap()"));
  assert!(!query_engine.contains("bytes[..8].try_into().unwrap()"));
  assert!(!query_engine.contains("built.nodes.into_iter().next().unwrap()"));
  assert!(!index_store.contains("expect(\"buffered index exists before mutation\")"));
  assert!(!index_store.contains("expect(\"buffered index exists after insertion\")"));
  assert!(!index_store.contains("expect(\"selected dirty index remains cached\")"));
}

#[test]
fn spill_replay_and_namespace_root_publication_do_not_reopen_validated_options_with_expect() {
  let emergency_spill = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/emergency_spill.rs")).unwrap();
  let namespace_mutation = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/namespace_mutation.rs")).unwrap();

  assert!(!emergency_spill.contains("expect(\"v2 artifact database identity\")"));
  assert!(!emergency_spill.contains("expect(\"v2 artifact incident identity\")"));
  assert!(!namespace_mutation.contains("expected_root.as_ref().expect(\"checked above\")"));
}

#[test]
fn deployment_and_crash_soak_startup_failures_are_returned_instead_of_panicking() {
  let deployment = std::fs::read_to_string(workspace_root().join("aeordb-cli/src/commands/deployment.rs")).unwrap();
  let crash_soak_worker = std::fs::read_to_string(workspace_root().join("aeordb-cli/src/bin/crash-soak-worker.rs")).unwrap();

  assert!(!deployment.contains("expect(\"deployment check output is serializable\")"));
  assert!(!crash_soak_worker.contains("open(&checkpoint).expect(\"open checkpoint\")"));
}

#[test]
fn fallible_server_startup_uses_fallible_metrics_initialization() {
  let server = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/server/mod.rs")).unwrap();
  let start = server.find("pub fn try_create_app_with_auth_mode_cancel_progress_and_configuration_overrides").unwrap();
  let end = server[start..].find("/// Build the application router with a specific JwtManager").unwrap() + start;
  let fallible_constructor = &server[start..end];

  assert!(fallible_constructor.contains("try_initialize_metrics()"));
  assert!(!fallible_constructor.contains("let prometheus_handle = initialize_metrics();"));
}

#[test]
fn cli_entry_points_use_fallible_logging_initialization() {
  let start = std::fs::read_to_string(workspace_root().join("aeordb-cli/src/commands/start.rs")).unwrap();
  let verify = std::fs::read_to_string(workspace_root().join("aeordb-cli/src/commands/verify.rs")).unwrap();

  assert!(start.contains("try_initialize_logging(&log_config)"));
  assert!(!start.contains("\n  initialize_logging(&log_config)"));
  assert!(verify.contains("try_initialize_logging(&LogConfig"));
  assert!(!verify.contains("\n  initialize_logging(&LogConfig"));
}

#[test]
fn database_lock_failures_preserve_non_conflict_operating_system_evidence() {
  let storage_engine = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/storage_engine.rs")).unwrap();
  let start = storage_engine.find("fn acquire_file_lock").unwrap();
  let end = storage_engine[start..].find("/// Create a new database file").unwrap() + start;
  let lock_function = &storage_engine[start..end];

  assert!(!lock_function.contains("try_lock_exclusive().map_err(|_|"));
  assert!(lock_function.contains("std::io::ErrorKind::WouldBlock"));
  assert!(lock_function.contains("error.kind()"));
  assert!(lock_function.contains("{error}"));
}

#[test]
fn acknowledged_namespace_mutations_cannot_retain_poisoned_authority_caches() {
  let cache = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/cache.rs")).unwrap();
  let storage_engine = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/storage_engine.rs")).unwrap();
  let namespace_mutation = std::fs::read_to_string(workspace_root().join("aeordb-lib/src/engine/namespace_mutation.rs")).unwrap();

  assert!(cache.contains("pub fn remove_or_clear_poisoned"));
  assert!(cache.contains("pub fn clear_recovering_poison"));
  assert!(storage_engine.contains("evict_authoritatively"));
  assert!(storage_engine.contains("evict_all_authoritatively"));
  assert!(!namespace_mutation.contains("if let Err(error) = self.engine.invalidate_all_authority_caches()"));
  assert!(!namespace_mutation.contains("Acknowledged namespace mutation could not invalidate authority caches"));
}

#[test]
fn production_suppression_allowlist_is_exact_reviewed_and_non_growing() {
  let discovered = scan_workspace(workspace_root()).unwrap();
  let inventory = load_inventory(&workspace_root().join("aeordb-lib/spec/fixtures/error-squelch-allowlist-v1.json")).unwrap();
  let errors = validate_inventory(&discovered, &inventory);

  assert!(errors.is_empty(), "error-squelch inventory violations:\n{}", errors.join("\n"));
}
