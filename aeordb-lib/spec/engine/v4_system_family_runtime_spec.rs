use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::system_family_policy::GenericDataPathSelection;
use aeordb::engine::{HashAlgorithm, SystemFamilyPolicyResolver};
use aeordb::engine::v4::admission::{AdmissionModeV1, BinaryCapabilityProfileV1, V4AdmissionResult, admit_v4_header};
use aeordb::engine::v4::database_header::{SelectedDatabaseHeaderV4, decode_header_region};
use aeordb::engine::v4::system_family::{
  AbsencePolicyV1, EventPolicyV1, GcPolicyV1, IndexPolicyV1, MigrationPolicyV1, RepairPolicyV1, SemanticRoleV1, SensitivityV1,
  SpillPolicyV1, SystemFamilyClassificationV1, SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilyPolicyV1,
  SystemFamilySubjectV1, SystemFamilyTransferOperationV1, TransferPolicyV1, UnknownChildPolicyV1, VerifyPolicyV1, classify_system_family,
  embedded_system_family_registry, require_complete_system_family,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RegistryOracle {
  source_row_count: usize,
  descriptor_count: u32,
  source_rows: Vec<FamilyOracle>,
}

#[derive(Debug, Deserialize)]
struct FamilyOracle {
  family_id: String,
  matcher_count: u32,
  policy: PolicyOracle,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
struct PolicyOracle {
  semantic_role: u8,
  gc_policy: u8,
  physical_copy_policy: u8,
  logical_backup_policy: u8,
  data_export_policy: u8,
  peer_replication_policy: u8,
  cluster_join_policy: u8,
  client_sync_policy: u8,
  import_policy: u8,
  verify_policy: u8,
  repair_policy: u8,
  migration_policy: u8,
  spill_policy: u8,
  sensitivity: u8,
  event_policy: u8,
  absence_policy: u8,
  unknown_child_policy: u8,
  index_policy: u8,
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures")
}

fn header(name: &str) -> SelectedDatabaseHeaderV4 {
  decode_header_region(&fs::read(fixture_root().join("v4/database-header-v4").join(name)).unwrap()).unwrap()
}

fn registry_oracle() -> RegistryOracle {
  serde_json::from_slice(&fs::read(fixture_root().join("system-family-registry-v1.manifest.json")).unwrap()).unwrap()
}

fn family_id(value: &str) -> u16 {
  u16::from_str_radix(value.strip_prefix("0x").expect("frozen family ID prefix"), 16).expect("frozen family ID")
}

fn raw_policy(policy: SystemFamilyPolicyV1) -> PolicyOracle {
  PolicyOracle {
    semantic_role: policy.semantic_role.as_u8(),
    gc_policy: policy.gc_policy.bits(),
    physical_copy_policy: policy.physical_copy_policy.as_u8(),
    logical_backup_policy: policy.logical_backup_policy.as_u8(),
    data_export_policy: policy.data_export_policy.as_u8(),
    peer_replication_policy: policy.peer_replication_policy.as_u8(),
    cluster_join_policy: policy.cluster_join_policy.as_u8(),
    client_sync_policy: policy.client_sync_policy.as_u8(),
    import_policy: policy.import_policy.as_u8(),
    verify_policy: policy.verify_policy.as_u8(),
    repair_policy: policy.repair_policy.as_u8(),
    migration_policy: policy.migration_policy.as_u8(),
    spill_policy: policy.spill_policy.as_u8(),
    sensitivity: policy.sensitivity.as_u8(),
    event_policy: policy.event_policy.as_u8(),
    absence_policy: policy.absence_policy.as_u8(),
    unknown_child_policy: policy.unknown_child_policy.as_u8(),
    index_policy: policy.index_policy.as_u8(),
  }
}

#[test]
fn every_runtime_policy_and_descriptor_matches_the_independent_manifest() {
  let oracle = registry_oracle();
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(oracle.source_row_count, 46);
  assert_eq!(oracle.source_rows.len(), oracle.source_row_count);
  assert_eq!(registry.family_count as usize, oracle.source_row_count);
  assert_eq!(registry.descriptor_count, oracle.descriptor_count);

  let expected: BTreeMap<_, _> = oracle.source_rows.iter().map(|row| (family_id(&row.family_id), row)).collect();
  let mut actual_matcher_counts = BTreeMap::<u16, u32>::new();
  for descriptor in registry.iter() {
    let descriptor = descriptor.unwrap();
    let row = expected.get(&descriptor.family_id).expect("runtime family exists in independent manifest");
    assert_eq!(raw_policy(descriptor.policy), row.policy, "policy mismatch for {}", row.family_id);
    *actual_matcher_counts.entry(descriptor.family_id).or_default() += 1;
  }

  assert_eq!(actual_matcher_counts.len(), oracle.source_row_count);
  for (expected_family_id, row) in expected {
    assert_eq!(actual_matcher_counts.get(&expected_family_id), Some(&row.matcher_count), "matcher count mismatch for {}", row.family_id);
  }
}

#[test]
fn embedded_registries_are_cached_per_hash_algorithm_and_reused_by_admission() {
  let algorithms =
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];
  for algorithm in algorithms {
    let first = embedded_system_family_registry(algorithm).unwrap();
    let second = embedded_system_family_registry(algorithm).unwrap();
    assert!(std::ptr::eq(first, second));
    assert_eq!(first.bytes, include_bytes!("../fixtures/system-family-registry-v1.bin"));
  }

  for (algorithm, fixture) in
    [(HashAlgorithm::Blake3_256, "header-blake3-256-valid-ab.bin"), (HashAlgorithm::Sha512, "header-sha512-valid-ab.bin")]
  {
    let selected = header(fixture);
    let admission = admit_v4_header(&selected, AdmissionModeV1::SemanticReadOnly, BinaryCapabilityProfileV1::current(), None).unwrap();
    let V4AdmissionResult::SemanticReadOnly(read) = admission else {
      panic!("expected semantic read-only admission");
    };
    assert!(std::ptr::eq(read.registry, embedded_system_family_registry(algorithm).unwrap()));
  }
}

#[test]
fn strict_ancestors_of_absolute_families_are_runtime_only_structural_containers() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  for path in [
    "/.aeordb-config",
    "/.aeordb-indexes",
    "/.aeordb-logs",
    "/.aeordb-system",
    "/.aeordb-system/cluster",
    "/.aeordb-system/controls",
    "/.aeordb-system/users",
  ] {
    let classification = classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap();
    assert_eq!(classification, SystemFamilyClassificationV1::StructuralContainer, "{path}");
    assert_eq!(classification.family_id(), None);
    assert_eq!(require_complete_system_family(classification, "strict traversal").unwrap(), None);
  }

  for path in ["/.aeordb-system/future", "/docs/.aeordb-future/value"] {
    assert_eq!(
      classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap(),
      SystemFamilyClassificationV1::UnknownProtected,
      "{path}"
    );
  }

  for (path, family_id) in [("/docs/.aeordb-config", 0x0008), ("/docs/.aeordb-indexes", 0x0060), ("/docs/.aeordb-logs", 0x0061)] {
    let classification = classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap();
    assert_eq!(classification.family_id(), Some(family_id), "reserved subtree container {path}");
  }

  for (path, family_id) in [("/.aeordb-indexes/text.idx", 0x0060), ("/.aeordb-logs/index.log", 0x0061)] {
    let classification = classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap();
    assert_eq!(classification.family_id(), Some(family_id), "root reserved subtree child {path}");
  }
}

#[test]
fn typed_policies_pin_permissions_conflicts_controls_and_secrets() {
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let permissions = known_path_policy(registry, "/docs/.aeordb-permissions");
  assert_eq!(permissions.logical_backup_policy, TransferPolicyV1::RequiredInclude);
  assert_eq!(permissions.peer_replication_policy, TransferPolicyV1::RequiredInclude);
  assert_eq!(permissions.data_export_policy, TransferPolicyV1::RequiredInclude);
  assert_eq!(permissions.index_policy, IndexPolicyV1::IncludeUnderOrdinaryScope);
  assert_eq!(permissions.event_policy, EventPolicyV1::AuthorizedNamespace);

  let conflicts = known_path_policy(registry, "/.aeordb-conflicts/item.json");
  assert_eq!(conflicts.logical_backup_policy, TransferPolicyV1::RequiredInclude);
  assert_eq!(conflicts.peer_replication_policy, TransferPolicyV1::OmitDeclared);
  assert_eq!(conflicts.data_export_policy, TransferPolicyV1::OmitDeclared);
  assert_eq!(conflicts.migration_policy, MigrationPolicyV1::RequiredCopy);
  assert_eq!(conflicts.index_policy, IndexPolicyV1::ExcludeFromAllIndexes);

  let controls = known_path_policy(registry, "/.aeordb-system/controls/v1/index-registry/a.ctrl");
  assert_eq!(controls.semantic_role, SemanticRoleV1::None);
  assert_eq!(controls.logical_backup_policy, TransferPolicyV1::OmitDeclared);
  assert_eq!(controls.peer_replication_policy, TransferPolicyV1::NodeLocal);
  assert_eq!(controls.data_export_policy, TransferPolicyV1::OmitDeclared);
  assert_eq!(controls.verify_policy, VerifyPolicyV1::StrictRequired);
  assert_eq!(controls.repair_policy, RepairPolicyV1::OwnerSpecific);
  assert_eq!(controls.absence_policy, AbsencePolicyV1::FatalIfAuthoritative);
  assert_eq!(controls.index_policy, IndexPolicyV1::NotApplicable);

  let api_keys = known_path_policy(registry, "/.aeordb-system/api-keys/key.json");
  assert_eq!(api_keys.logical_backup_policy, TransferPolicyV1::RedactOmit);
  assert_eq!(api_keys.peer_replication_policy, TransferPolicyV1::NodeLocal);
  assert_eq!(api_keys.sensitivity, SensitivityV1::Credential);
  assert_eq!(api_keys.spill_policy, SpillPolicyV1::Ineligible);
  assert_eq!(api_keys.unknown_child_policy, UnknownChildPolicyV1::ClassifyByRegistry);

  let email = known_path_policy(registry, "/.aeordb-system/email-config.json");
  assert_eq!(email.logical_backup_policy, TransferPolicyV1::RedactOmit);
  assert_eq!(email.peer_replication_policy, TransferPolicyV1::NodeLocal);
  assert_eq!(email.sensitivity, SensitivityV1::Secret);
  assert_eq!(email.gc_policy, GcPolicyV1::PIN_WHILE_AUTHORITATIVE);
}

#[test]
fn every_transfer_operation_selects_the_independent_policy_column() {
  let oracle = registry_oracle();
  let registry = embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap();
  let expected: BTreeMap<_, _> = oracle.source_rows.iter().map(|row| (family_id(&row.family_id), row)).collect();
  let operations = [
    SystemFamilyTransferOperationV1::PhysicalCopy,
    SystemFamilyTransferOperationV1::LogicalBackup,
    SystemFamilyTransferOperationV1::DataExport,
    SystemFamilyTransferOperationV1::PeerReplication,
    SystemFamilyTransferOperationV1::ClusterJoin,
    SystemFamilyTransferOperationV1::ClientSync,
    SystemFamilyTransferOperationV1::Import,
  ];

  for descriptor in registry.iter() {
    let descriptor = descriptor.unwrap();
    let row = expected.get(&descriptor.family_id).unwrap();
    for operation in operations {
      assert_eq!(
        descriptor.policy.transfer_policy(operation).as_u8(),
        oracle_transfer_policy(row.policy, operation),
        "{} {operation:?}",
        row.family_id
      );
    }
  }
}

fn oracle_transfer_policy(policy: PolicyOracle, operation: SystemFamilyTransferOperationV1) -> u8 {
  match operation {
    SystemFamilyTransferOperationV1::PhysicalCopy => policy.physical_copy_policy,
    SystemFamilyTransferOperationV1::LogicalBackup => policy.logical_backup_policy,
    SystemFamilyTransferOperationV1::DataExport => policy.data_export_policy,
    SystemFamilyTransferOperationV1::PeerReplication => policy.peer_replication_policy,
    SystemFamilyTransferOperationV1::ClusterJoin => policy.cluster_join_policy,
    SystemFamilyTransferOperationV1::ClientSync => policy.client_sync_policy,
    SystemFamilyTransferOperationV1::Import => policy.import_policy,
  }
}

#[test]
fn shared_resolver_preserves_structure_and_rejects_unknown_protected_state() {
  let resolver = SystemFamilyPolicyResolverV1::embedded(HashAlgorithm::Blake3_256).unwrap();
  assert!(std::ptr::eq(resolver.registry(), embedded_system_family_registry(HashAlgorithm::Blake3_256).unwrap()));

  assert_eq!(
    resolver.transfer_policy(SystemFamilySubjectV1::Path("/docs/readme.md"), SystemFamilyTransferOperationV1::PeerReplication).unwrap(),
    SystemFamilyPolicyDecisionV1::Ordinary,
  );
  assert_eq!(
    resolver.transfer_policy(SystemFamilySubjectV1::Path("/.aeordb-system"), SystemFamilyTransferOperationV1::PeerReplication).unwrap(),
    SystemFamilyPolicyDecisionV1::StructuralContainer,
  );
  assert_eq!(
    resolver
      .transfer_policy(SystemFamilySubjectV1::Path("/.aeordb-conflicts/item.json"), SystemFamilyTransferOperationV1::PeerReplication)
      .unwrap(),
    SystemFamilyPolicyDecisionV1::Known { family_id: 0x001a, policy: TransferPolicyV1::OmitDeclared },
  );
  assert_eq!(
    resolver.index_policy(SystemFamilySubjectV1::Path("/docs/.aeordb-permissions")).unwrap(),
    SystemFamilyPolicyDecisionV1::Known { family_id: 0x0019, policy: IndexPolicyV1::IncludeUnderOrdinaryScope },
  );

  let error = resolver
    .transfer_policy(SystemFamilySubjectV1::Path("/docs/.aeordb-future/value"), SystemFamilyTransferOperationV1::LogicalBackup)
    .unwrap_err();
  assert_eq!(error.code(), "unknown_protected_system_family");
}

#[test]
fn generic_data_policy_includes_permissions_conceals_protected_state_and_rejects_unknowns() {
  let resolver = SystemFamilyPolicyResolver::new(HashAlgorithm::Blake3_256).unwrap();

  for path in ["/docs/readme.md", "/docs/.aeordb-permissions"] {
    assert_eq!(resolver.generic_data_path_selection(path).unwrap(), GenericDataPathSelection::Include, "{path}");
    resolver.require_generic_data_leaf_path(path).unwrap();
  }

  for path in [
    "/.aeordb-conflicts/item.json",
    "/.aeordb-system/api-keys/key.json",
    "/.aeordb-system/controls/v1/index-registry/a.ctrl",
    "/docs/.aeordb-indexes/text.idx",
    "/docs/.aeordb-logs/index.log",
  ] {
    assert_eq!(resolver.generic_data_path_selection(path).unwrap(), GenericDataPathSelection::Conceal, "{path}");
    let error = resolver.require_generic_data_leaf_path(path).unwrap_err();
    assert!(matches!(error, aeordb::engine::EngineError::SystemFamilyPolicy { code: "system_family_generic_data_concealed", .. }));
  }

  assert_eq!(resolver.generic_data_path_selection("/.aeordb-system").unwrap(), GenericDataPathSelection::StructuralContainer);
  let error = resolver.generic_data_path_selection("/docs/.aeordb-future/value").unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::SystemFamilyPolicy { code: "unknown_protected_system_family", .. }));
}

#[test]
fn production_sources_do_not_restore_legacy_generic_path_policy() {
  fn collect_rust_sources(path: &Path, output: &mut String) {
    for entry in fs::read_dir(path).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        collect_rust_sources(&path, output);
      } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
        output.push_str(&fs::read_to_string(path).unwrap());
      }
    }
  }

  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut sources = String::new();
  collect_rust_sources(&source_root, &mut sources);
  for forbidden in ["is_system_path", "is_internal_path", "let system_prefixes =", "starts_with(\"/.aeordb-\")"] {
    assert!(!sources.contains(forbidden), "legacy generic path policy returned to production source: {forbidden}");
  }

  let directory_ops = fs::read_to_string(source_root.join("engine/directory_ops.rs")).unwrap();
  assert!(directory_ops.contains("fn v0_path_uses_detached_system_storage"));
  assert!(directory_ops.contains("pub fn v0_system_entry_flags"));
  assert!(directory_ops.contains("pub(crate) fn v0_is_detached_system_path"));

  let json_store = fs::read_to_string(source_root.join("engine/json_store.rs")).unwrap();
  assert!(json_store.contains("list_directory_strict"));
  assert!(!json_store.contains("silently skipped"));
  let system_store = fs::read_to_string(source_root.join("engine/system_store.rs")).unwrap();
  assert!(system_store.contains("list_directory_window_strict"));
  assert!(!system_store.contains("if let Ok(data)"));
  let index_store = fs::read_to_string(source_root.join("engine/index_store.rs")).unwrap();
  assert!(!index_store.contains("partial results are better than a total failure"));
  let permission_middleware = fs::read_to_string(source_root.join("auth/permission_middleware.rs")).unwrap();
  assert!(permission_middleware.contains("fn require_active_api_key"));
  assert!(!permission_middleware.contains("if let Ok(Some(key_record))"));
  assert!(!permission_middleware.contains("accessible_child_names(&user_uuid, engine_path).unwrap_or_default()"));
  assert!(!permission_middleware.contains("exists(path).unwrap_or(false)"));
  let route_permissions = fs::read_to_string(source_root.join("server/route_permissions.rs")).unwrap();
  assert!(!route_permissions.contains("unwrap_or(false)"));
  let engine_routes = fs::read_to_string(source_root.join("server/engine_routes.rs")).unwrap();
  assert!(engine_routes.contains("list_directory_recursive_strict"));
  assert!(!engine_routes.contains("directory_ops.list_directory("));
  let share_routes = fs::read_to_string(source_root.join("server/share_routes.rs")).unwrap();
  assert!(share_routes.contains("PathPermissions::deserialize_stored"));
  assert!(!share_routes.contains("PathPermissions::deserialize(&data)"));
  assert!(!share_routes.contains("grants_index_cache.get(&(), &state.engine) {\n    Ok(index) => index,\n    Err(_)"));
  let wasm_runtime = fs::read_to_string(source_root.join("plugins/wasm_runtime.rs")).unwrap();
  assert!(wasm_runtime.contains("list_directory_window_strict"));
  assert!(!wasm_runtime.contains("dir_ops.list_directory_window(&path"));
  let sync_routes = fs::read_to_string(source_root.join("server/sync_routes.rs")).unwrap();
  assert!(!sync_routes.contains("has_descendant_grants(&user_id, \"/\").unwrap_or(false)"));
  let sync_engine = fs::read_to_string(source_root.join("engine/sync_engine.rs")).unwrap();
  assert!(!sync_engine.contains("get_peer_sync_state(&self.engine, peer_node_id).ok().flatten()"));
}

fn known_path_policy(registry: &aeordb::engine::v4::system_family::SystemFamilyRegistryV1<'_>, path: &str) -> SystemFamilyPolicyV1 {
  let classification = classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap();
  let SystemFamilyClassificationV1::Known(family) = classification else {
    panic!("{path} did not resolve to a known family: {classification:?}");
  };
  family.policy
}
