use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::admission::{AdmissionModeV1, BinaryCapabilityProfileV1, V4AdmissionResult, admit_v4_header};
use aeordb::engine::v4::database_header::{SelectedDatabaseHeaderV4, decode_header_region};
use aeordb::engine::v4::system_family::{
  AbsencePolicyV1, EventPolicyV1, GcPolicyV1, IndexPolicyV1, MigrationPolicyV1, RepairPolicyV1, SemanticRoleV1, SensitivityV1,
  SpillPolicyV1, SystemFamilyClassificationV1, SystemFamilyPolicyV1, SystemFamilySubjectV1, TransferPolicyV1, UnknownChildPolicyV1,
  VerifyPolicyV1, classify_system_family, embedded_system_family_registry, require_complete_system_family,
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
  for path in ["/.aeordb-config", "/.aeordb-system", "/.aeordb-system/cluster", "/.aeordb-system/controls", "/.aeordb-system/users"] {
    let classification = classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap();
    assert_eq!(classification, SystemFamilyClassificationV1::StructuralContainer, "{path}");
    assert_eq!(classification.family_id(), None);
    assert_eq!(require_complete_system_family(classification, "strict traversal").unwrap(), None);
  }

  for path in ["/.aeordb-system/future", "/docs/.aeordb-future/value", "/docs/.aeordb-config"] {
    assert_eq!(
      classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap(),
      SystemFamilyClassificationV1::UnknownProtected,
      "{path}"
    );
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

fn known_path_policy(registry: &aeordb::engine::v4::system_family::SystemFamilyRegistryV1<'_>, path: &str) -> SystemFamilyPolicyV1 {
  let classification = classify_system_family(registry, SystemFamilySubjectV1::Path(path)).unwrap();
  let SystemFamilyClassificationV1::Known(family) = classification else {
    panic!("{path} did not resolve to a known family: {classification:?}");
  };
  family.policy
}
