//! Generated v4 contract identities.
//!
//! This module does not activate v4 readers or writers. It gives later phases
//! one checked source for permanent IDs and limits.

#[rustfmt::skip]
pub mod contract_generated;
pub mod admission;
pub mod config_value;
pub mod configuration_controls;
pub mod control_store;
pub mod coverage_journal;
pub mod coverage_runtime;
pub mod database_header;
pub mod dependency;
pub mod deployment_guard;
pub mod durability_recovery;
pub mod entity;
pub mod field_definition;
pub mod first_authority;
pub mod gc;
pub mod gc_audit;
pub mod gc_lifecycle;
pub mod gc_lineage_recovery;
pub mod gc_maintenance;
pub mod gc_mark;
pub mod gc_mark_convergence;
pub mod gc_mark_runtime;
pub mod gc_mark_workspace;
pub mod gc_quarantine;
pub mod gc_quarantine_publication;
pub mod gc_quarantine_transition;
pub mod gc_retirement;
pub mod gc_root_reclaim;
pub mod gc_root_transition;
pub mod gc_run;
pub mod gc_state;
pub mod gc_sweep;
pub mod gc_sweep_reconciliation;
pub mod gc_sweep_removal;
pub mod gc_void;
pub mod gc_void_claim;
pub mod gc_void_publication;
pub mod gc_void_runtime;
pub mod gc_void_settlement;
pub mod hash;
pub mod header_publication;
pub mod index_artifact;
pub mod index_converter;
pub mod index_converter_v0;
pub mod index_coordinator;
pub mod index_coordinator_recovery;
pub mod index_copy_on_write;
pub mod index_definition_runtime;
pub mod index_manifest;
pub mod index_nvt;
pub mod index_page;
pub mod index_producer_admission;
pub mod index_producer_collector;
pub mod index_producer_coordinator;
pub mod index_producer_executor;
pub mod index_producer_source;
pub mod index_producer_worker;
pub mod index_record;
pub mod index_runtime_owner;
pub mod index_scope_ordinal_authority;
pub mod index_scope_ordinal_checkpoint;
pub mod index_scope_ordinal_checkpoint_store;
pub mod index_semantic_registry;
pub mod index_semantic_source;
pub mod index_source;
pub mod index_task;
pub mod namespace;
pub mod parser_plan;
pub mod position;
pub mod read_view;
pub mod reader;
pub mod root_authority;
pub mod scope;
pub mod semantic_store;
pub mod source_selector;
pub mod system_control;
pub mod system_family;
pub mod text_fold;
pub mod transfer_closure;
pub mod value_store;

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use sha2::{Digest, Sha256};

  use super::contract_generated as contract;

  #[test]
  fn generated_contract_matches_the_checked_in_registry_digest() {
    let source = include_bytes!("../../../spec/fixtures/v4/format-contract-registry.json");
    assert_eq!(hex::encode(Sha256::digest(source)), contract::CONTRACT_REGISTRY_SHA256);
    assert_eq!(blake3::hash(source).to_hex().as_str(), contract::CONTRACT_REGISTRY_BLAKE3);

    let architecture = include_bytes!("../../../spec/fixtures/v4/architecture-contract-registry.json");
    assert_eq!(hex::encode(Sha256::digest(architecture)), contract::ARCHITECTURE_REGISTRY_SHA256);

    let semantics = include_bytes!("../../../spec/semantics/v1/fingerprint-registry.json");
    assert_eq!(hex::encode(Sha256::digest(semantics)), contract::SEMANTICS_REGISTRY_SHA256);
  }

  #[test]
  fn generated_permanent_ids_are_unique_within_their_scopes() {
    assert_unique(contract::CAPABILITY_BITS.iter().map(|value| value.id));
    assert_unique(contract::ENTRY_TYPES.iter().map(|value| value.id as u16));
    assert_unique(contract::KV_TAGS.iter().map(|value| value.id as u16));
    assert_unique(contract::SYSTEM_FAMILIES.iter().map(|value| value.id));
    assert_eq!(contract::SYSTEM_FAMILIES.len(), 46);
    assert_eq!(contract::FORMAT_LIMITS.len(), 21);
    assert_eq!(contract::ROUTE_CLASSES.len(), 7);
    assert_eq!(contract::CONFIGURATION_PROPERTIES.len(), 41);
    assert_eq!(contract::DYNAMIC_RECORDS.len(), 8);
    assert_eq!(contract::HARD_TRANSITIONS.len(), 12);
    assert_eq!(contract::CLEANUP_RESULT_CLASSES.len(), 4);
    assert_eq!(contract::SEMANTIC_BUNDLES.len(), 37);
    assert_eq!(contract::SEMANTIC_BUNDLES.iter().filter(|row| row.kind == contract::SemanticBundleKind::Converter).count(), 25);
    assert_eq!(contract::SEMANTIC_BUNDLES.iter().filter(|row| row.kind == contract::SemanticBundleKind::Strategy).count(), 12);
    assert!(contract::SEMANTIC_BUNDLES.iter().all(|row| row.fingerprint_blake3 != [0; 32]));
  }

  #[test]
  fn generated_configuration_names_are_unique_and_mechanical() {
    let environments = contract::CONFIGURATION_PROPERTIES.iter().map(|property| property.environment);
    assert_unique_strings(environments);
    let cli_names = contract::CONFIGURATION_PROPERTIES.iter().map(|property| property.cli);
    assert_unique_strings(cli_names);
    let flush =
      contract::CONFIGURATION_PROPERTIES.iter().find(|property| property.path == "index.flush_after_mutations").expect("frozen property");
    assert_eq!(flush.environment, "AEORDB_INDEX_FLUSH_AFTER_MUTATIONS");
    assert_eq!(flush.cli, "--index-flush-after-mutations");
    let spill =
      contract::CONFIGURATION_PROPERTIES.iter().find(|property| property.path == "recovery.emergency_spill_dir").expect("frozen property");
    assert_eq!(spill.redaction, Some("root_only_path"));
  }

  fn assert_unique(values: impl Iterator<Item = u16>) {
    let values: Vec<_> = values.collect();
    assert_eq!(values.iter().copied().collect::<BTreeSet<_>>().len(), values.len());
  }

  fn assert_unique_strings<'a>(values: impl Iterator<Item = &'a str>) {
    let values: Vec<_> = values.collect();
    assert_eq!(values.iter().copied().collect::<BTreeSet<_>>().len(), values.len());
  }
}
