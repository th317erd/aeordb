use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use aeordb::engine::v4::database_header::{
  DatabaseHeaderReadError, DatabaseHeaderVersion, ReadOnlyDatabaseHeader, decode_header_region, probe_header_version,
  read_database_header_read_only, read_header_region,
};
use aeordb::engine::v4::config_value::{CanonicalValueBounds, validate_canonical_value};
use aeordb::engine::v4::entity::decode_whole_entity;
use aeordb::engine::v4::field_definition::{decode_converter_definition, decode_field_index_definition};
use aeordb::engine::v4::gc::{
  GcArtifactKindV1, PhysicalIncarnationV1, decode_gc_active_control, decode_gc_artifact_envelope, decode_physical_incarnation,
  immutable_gc_artifact_key, select_gc_active_control,
};
use aeordb::engine::v4::gc_audit::{
  AuditArtifactV1, decode_audit_artifact, validate_audit_directory_child, validate_audit_manifest_directory, validate_audit_manifest_pin,
  validate_audit_pin_target, validate_run_summary_page_record,
};
use aeordb::engine::v4::gc_mark::{
  GcMarkArtifactV1, decode_gc_mark_artifact, decode_mark_workspace_manifest, decode_mark_workspace_object,
  validate_mark_checkpoint_workspace, validate_mark_mutation_journal_chain, validate_mark_workspace_object,
};
use aeordb::engine::v4::gc_state::{GcStateArtifactV1, decode_gc_state_artifact, validate_gc_directory_page};
use aeordb::engine::v4::gc_void::{
  SweepVoidArtifactV1, decode_sweep_void_artifact, validate_sweep_receipt_closure, validate_void_claim_source,
  validate_void_directory_child, validate_void_manifest_root, validate_void_settlement_closure,
};
use aeordb::engine::v4::index_artifact::{
  IndexControlOrManifestV1, decode_active_pointer, decode_index_control_or_manifest, select_active_pointer,
};
use aeordb::engine::v4::index_page::{
  OrderedIndexArtifactV1, OrderedIndexRoleV1, compare_order_keys, decode_ordered_index_artifact, validate_scope_catalog_pair,
};
use aeordb::engine::v4::index_task::{IndexTaskArtifactV1, IndexTaskKindV1, decode_index_task_artifact, validate_journal_chain};
use aeordb::engine::v4::index_nvt::{coordinate_cell, decode_nvt_tile, verified_page_hint, verified_predecessor_or_fallback};
use aeordb::engine::v4::dependency::{decode_dependency_table, decode_invocation_policy};
use aeordb::engine::v4::namespace::{SemanticObjectKind, decode_namespace_root, decode_semantic_object};
use aeordb::engine::v4::parser_plan::{ParserPlanKind, decode_parser_resolution_plan};
use aeordb::engine::v4::position::{PositionContextV1, PositionRouteV1, decode_logical_position, validate_position_context};
use aeordb::engine::v4::reader::{BoundedReader, MalformedInputClass};
use aeordb::engine::v4::scope::{ScopeMatchingMode, decode_scope_definition};
use aeordb::engine::v4::source_selector::{SourceSelectorKind, decode_source_selector};
use aeordb::engine::v4::system_control::{
  SystemControlKindV1, SystemControlSlotV1, decode_system_control, select_cutover_journal, select_system_control_pair,
};
use aeordb::engine::v4::system_family::decode_system_family_registry;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_header::{FileHeader, HEADER_REGION_SIZE};
use serde::Deserialize;

#[derive(Deserialize)]
struct FixtureManifest {
  fixtures: Vec<FixtureRow>,
}

#[test]
fn contract_gate_uses_portable_hash_and_file_size_helpers() {
  let source = include_str!("../../../scripts/plan/check-v4-contracts.sh");

  assert!(source.contains("sha256_file()"));
  assert!(source.contains("file_size_bytes()"));
  assert!(source.contains("normalize_text_lines()"));
  assert!(source.contains("normalize_inventory_paths()"));
  assert!(source.contains("tr -d '\\r'"));
  assert!(source.contains("sed 's|\\\\|/|g'"));
  assert!(source.matches("| normalize_text_lines").count() >= 12);
  assert!(source.matches("| normalize_inventory_paths").count() >= 4);
  assert!(!source.contains("stat -c"));
}

#[derive(Deserialize)]
struct FixtureRow {
  id: String,
  format_id: String,
  hash_algorithm: String,
  binary: String,
  canonical_key: Option<String>,
  expected: String,
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn manifest() -> FixtureManifest {
  serde_json::from_slice(&fs::read(fixture_root().join("format-fixture-manifest.json")).unwrap()).unwrap()
}

#[test]
fn every_database_header_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "database-header-v4").collect();
  assert_eq!(rows.len(), 10);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let observed = match decode_header_region(&bytes) {
      Ok(selected) if selected.redundancy_degraded => format!("selected:{}:redundancy-degraded", selected.header.slot_sequence),
      Ok(selected) => format!("selected:{}", selected.header.slot_sequence),
      Err(error) => format!("error:{}", error.code()),
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_whole_entity_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "whole-entity-v1").collect();
  assert_eq!(rows.len(), 2);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    let entity = decode_whole_entity(&bytes, algorithm, u64::MAX).unwrap();
    assert_eq!(format!("entity:entry-type=0x{:02x}", entity.entry_type.to_u8()), row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(entity.key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn every_namespace_and_semantic_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> =
    manifest().fixtures.into_iter().filter(|row| row.format_id == "directory-index-v1" || row.format_id == "semantic-object-v1").collect();
  assert_eq!(rows.len(), 12);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    let (observed, key) = if row.format_id == "directory-index-v1" {
      let root = decode_namespace_root(&bytes, algorithm).unwrap();
      ("directory:namespace-root".to_string(), root.root_hash)
    } else {
      let object = decode_semantic_object(&bytes, algorithm).unwrap();
      let summary = match object.kind {
        SemanticObjectKind::State { content_only_reason: None } => "semantic:state:complete".to_string(),
        SemanticObjectKind::State { content_only_reason: Some(reason) } => format!("semantic:state:content-only:reason={reason}"),
        SemanticObjectKind::CatalogLeaf { record_count } => format!("semantic:catalog-leaf:records={record_count}"),
        SemanticObjectKind::CatalogInternal { child_count } => format!("semantic:catalog-internal:children={child_count}"),
        SemanticObjectKind::Definition { class } => format!("semantic:definition:class={class}"),
      };
      (summary, object.object_id)
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn every_canonical_config_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "canonical-config-value-v1").collect();
  assert_eq!(rows.len(), 6);
  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let summary = validate_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    assert_eq!(format!("config:{}:{}={}", summary.tag_name, summary.detail_name, summary.detail), row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_invocation_and_dependency_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| row.format_id == "invocation-policy-v1" || row.format_id == "dependency-table-v1")
    .collect();
  assert_eq!(rows.len(), 12);
  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let observed = if row.format_id == "invocation-policy-v1" {
      format!("policy:{}", decode_invocation_policy(&bytes).unwrap().name())
    } else {
      format!("dependencies:records={}", decode_dependency_table(&bytes).unwrap().records.len())
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_scope_definition_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "scope-definition-v1").collect();
  assert_eq!(rows.len(), 6);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let scope = decode_scope_definition(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    let observed = if bytes.len() == 65_536 {
      "scope:relative-glob:maximum-length".to_string()
    } else {
      match scope.mode {
        ScopeMatchingMode::DirectChildren => format!("scope:direct:owner={}", scope.owner_path),
        ScopeMatchingMode::RelativePathGlob => {
          format!("scope:relative-glob:owner={}:glob={}", scope.owner_path, scope.glob.unwrap())
        }
      }
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(scope.scope_id), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn every_parser_resolution_plan_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "parser-resolution-plan-v1").collect();
  assert_eq!(rows.len(), 8);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let plan = decode_parser_resolution_plan(&bytes).unwrap();
    let kind = match plan.kind {
      ParserPlanKind::None => "none",
      ParserPlanKind::ExplicitPlugin => "explicit-plugin",
      ParserPlanKind::Automatic => "automatic",
    };
    assert_eq!(format!("parser-plan:{kind}:candidates={}", plan.candidates.len()), row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_source_selector_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "source-selector-v1").collect();
  assert_eq!(rows.len(), 14);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let selector = decode_source_selector(&bytes).unwrap();
    let kind = match selector.kind {
      SourceSelectorKind::Metadata => "metadata",
      SourceSelectorKind::JsonPath => "json-path",
      SourceSelectorKind::PluginMapper => "plugin-mapper",
      SourceSelectorKind::AlwaysMissingV0 => "always-missing-v0",
    };
    assert_eq!(format!("selector:{kind}:items={}", selector.item_count), row.expected, "fixture {}", row.id);
  }
}

#[test]
fn every_value_store_definition_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "value-store-definition-v1").collect();
  assert_eq!(rows.len(), 14);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let definition = decode_value_store_definition(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    let selector_kind = match definition.selector.kind {
      SourceSelectorKind::Metadata => 1,
      SourceSelectorKind::JsonPath => 2,
      SourceSelectorKind::PluginMapper => 3,
      SourceSelectorKind::AlwaysMissingV0 => 4,
    };
    assert_eq!(
      format!(
        "value-store:field={}:selector={selector_kind}:dependencies={}",
        definition.field_name,
        definition.dependencies.records.len()
      ),
      row.expected,
      "fixture {}",
      row.id
    );
    assert_eq!(hex::encode(definition.value_store_id), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn every_converter_and_field_index_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| row.format_id == "converter-definition-v1" || row.format_id == "field-index-definition-v1")
    .collect();
  assert_eq!(rows.len(), 100);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    let (observed, key) = if row.format_id == "converter-definition-v1" {
      let definition = decode_converter_definition(&bytes, algorithm).unwrap();
      (
        format!("converter:{}:semantics={}", definition.name, if definition.corrected { 1 } else { definition.converter_id }),
        definition.converter_fingerprint,
      )
    } else {
      let definition = decode_field_index_definition(&bytes, algorithm).unwrap();
      (
        format!(
          "field-index:{}:converter={}:operations=0x{:x}",
          definition.strategy_name, definition.converter.name, definition.operations
        ),
        definition.index_id,
      )
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn converter_and_field_index_reject_bounds_identity_and_cross_record_corruption() {
  let root = fixture_root();
  let converter = fs::read(root.join("converter-definition-v1/acnv-blake3-256-typed_exact_blake3_v1-valid.bin")).unwrap();

  let mut unknown_converter = converter.clone();
  unknown_converter[32..34].copy_from_slice(&0x7fffu16.to_le_bytes());
  assert_eq!(
    decode_converter_definition(&unknown_converter, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut wrong_source_mask = converter.clone();
  wrong_source_mask[36..40].copy_from_slice(&0u32.to_le_bytes());
  assert_eq!(
    decode_converter_definition(&wrong_source_mask, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut unknown_source_type = converter.clone();
  unknown_source_type[36..40].copy_from_slice(&(1u32 << 31).to_le_bytes());
  assert_eq!(
    decode_converter_definition(&unknown_source_type, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut nonzero_converter_reserve = converter.clone();
  nonzero_converter_reserve[16] = 1;
  assert_eq!(
    decode_converter_definition(&nonzero_converter_reserve, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut zero_converter_limit = converter.clone();
  zero_converter_limit[64..72].fill(0);
  assert_eq!(
    decode_converter_definition(&zero_converter_limit, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut wrong_bundle_fingerprint = converter.clone();
  wrong_bundle_fingerprint[88] ^= 1;
  assert_eq!(
    decode_converter_definition(&wrong_bundle_fingerprint, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut corrected_parameter = converter;
  corrected_parameter.push(0);
  corrected_parameter[8..12].copy_from_slice(&121u32.to_le_bytes());
  corrected_parameter[56..60].copy_from_slice(&1u32.to_le_bytes());
  assert_eq!(
    decode_converter_definition(&corrected_parameter, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  assert_eq!(
    decode_converter_definition(&vec![0; 65_537], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let legacy_string = fs::read(root.join("converter-definition-v1/acnv-blake3-256-string_v0-valid.bin")).unwrap();
  let mut zero_legacy_bound = legacy_string;
  zero_legacy_bound[120..124].fill(0);
  assert_eq!(
    decode_converter_definition(&zero_legacy_bound, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let field = fs::read(root.join("field-index-definition-v1/afix-blake3-256-typed_exact_blake3_v1-valid.bin")).unwrap();
  let fixed = 64;

  let mut zero_value_store = field.clone();
  zero_value_store[32..64].fill(0);
  assert_eq!(
    decode_field_index_definition(&zero_value_store, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut unknown_operation = field.clone();
  unknown_operation[fixed + 10..fixed + 18].copy_from_slice(&(3u64 | (1 << 63)).to_le_bytes());
  assert_eq!(
    decode_field_index_definition(&unknown_operation, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut wrong_strategy_fingerprint = field.clone();
  wrong_strategy_fingerprint[fixed + 72] ^= 1;
  assert_eq!(
    decode_field_index_definition(&wrong_strategy_fingerprint, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut invalid_strategy_utf8 = field.clone();
  invalid_strategy_utf8[136 + 32] = 0xff;
  assert_eq!(
    decode_field_index_definition(&invalid_strategy_utf8, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let mut nonzero_field_reserve = field.clone();
  nonzero_field_reserve[fixed + 42] = 1;
  assert_eq!(
    decode_field_index_definition(&nonzero_field_reserve, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut zero_field_limit = field.clone();
  zero_field_limit[fixed + 44..fixed + 48].fill(0);
  assert_eq!(
    decode_field_index_definition(&zero_field_limit, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let converter_start = 136 + 32 + 5;
  let mut malformed_nested_converter = field;
  malformed_nested_converter[converter_start + 88] ^= 1;
  assert_eq!(
    decode_field_index_definition(&malformed_nested_converter, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  assert_eq!(
    decode_field_index_definition(&vec![0; 256 * 1_024 + 1], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn every_index_pointer_and_manifest_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| {
      row.format_id == "index-artifact-v1" && (row.expected.starts_with("index:pointer:") || row.expected.starts_with("index:manifest:"))
    })
    .collect();
  assert_eq!(rows.len(), 28);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let decoded = decode_index_control_or_manifest(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    let (observed, key) = match decoded {
      IndexControlOrManifestV1::Pointer(pointer) => (
        format!("index:pointer:{}:slot-{}:sequence={}", pointer.kind.name(), if pointer.slot == 0 { 'a' } else { 'b' }, pointer.sequence),
        pointer.key,
      ),
      IndexControlOrManifestV1::Manifest(manifest) => (
        format!(
          "index:manifest:{}:generation={}:roots={}",
          manifest.kind.name(),
          manifest.generation,
          if manifest.populated { "populated" } else { "empty" }
        ),
        manifest.key,
      ),
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn index_pointer_and_manifest_reject_integrity_selector_capability_and_closure_corruption() {
  let root = fixture_root();
  let pointer = fs::read(root.join("index-artifact-v1/aidx-blake3-256-field-index-pointer-a.bin")).unwrap();

  let mut bad_crc = pointer.clone();
  *bad_crc.last_mut().unwrap() ^= 1;
  assert_eq!(
    decode_index_control_or_manifest(&bad_crc, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut bad_slot = pointer.clone();
  bad_slot[64] = 2;
  repair_trailing_crc(&mut bad_slot);
  assert_eq!(
    decode_index_control_or_manifest(&bad_slot, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
  );

  let mut zero_sequence = pointer;
  zero_sequence[65..73].fill(0);
  repair_trailing_crc(&mut zero_sequence);
  assert_eq!(
    decode_index_control_or_manifest(&zero_sequence, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let manifest = fs::read(root.join("index-artifact-v1/aidx-blake3-256-field-index-manifest-populated.bin")).unwrap();
  let body_start = 72;

  let mut unknown_capability = manifest.clone();
  unknown_capability[body_start + 7] = 1;
  repair_trailing_crc(&mut unknown_capability);
  assert_eq!(
    decode_index_control_or_manifest(&unknown_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownRequiredCapability
  );

  let mut generation_mismatch = manifest.clone();
  generation_mismatch[64..72].copy_from_slice(&4_099u64.to_le_bytes());
  repair_trailing_crc(&mut generation_mismatch);
  assert_eq!(
    decode_index_control_or_manifest(&generation_mismatch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut owner_mismatch = manifest.clone();
  owner_mismatch[32] ^= 1;
  repair_trailing_crc(&mut owner_mismatch);
  assert_eq!(
    decode_index_control_or_manifest(&owner_mismatch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let presence_offset = body_start + 70 + 32;
  let mut root_presence_mismatch = manifest;
  root_presence_mismatch[presence_offset] = 0;
  repair_trailing_crc(&mut root_presence_mismatch);
  assert_eq!(
    decode_index_control_or_manifest(&root_presence_mismatch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  assert_eq!(
    decode_index_control_or_manifest(&vec![0; 1_048_577], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn index_pointer_pair_selection_is_deterministic_and_fails_ambiguous() {
  let root = fixture_root();
  let a_bytes = fs::read(root.join("index-artifact-v1/aidx-blake3-256-field-index-pointer-a.bin")).unwrap();
  let b_bytes = fs::read(root.join("index-artifact-v1/aidx-blake3-256-field-index-pointer-b-max-sequence.bin")).unwrap();
  let a = decode_active_pointer(&a_bytes, HashAlgorithm::Blake3_256).unwrap();
  let b = decode_active_pointer(&b_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(select_active_pointer(&a, &b).unwrap().slot, 1);

  let mut equal_bytes = b_bytes.clone();
  equal_bytes[65..73].copy_from_slice(&1u64.to_le_bytes());
  equal_bytes[73..105].copy_from_slice(a.target_manifest_hash);
  repair_trailing_crc(&mut equal_bytes);
  let equal = decode_active_pointer(&equal_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(select_active_pointer(&a, &equal).unwrap().slot, 0);

  let mut ambiguous_bytes = equal_bytes;
  ambiguous_bytes[73] ^= 1;
  repair_trailing_crc(&mut ambiguous_bytes);
  let ambiguous = decode_active_pointer(&ambiguous_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(select_active_pointer(&a, &ambiguous).unwrap_err().class(), MalformedInputClass::AmbiguousEqualSequenceSelector);
  assert_eq!(select_active_pointer(&a, &a).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);
}

#[test]
fn every_ordered_page_and_directory_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| {
      row.format_id == "index-artifact-v1" && (row.expected.starts_with("index:page:") || row.expected.starts_with("index:directory:"))
    })
    .collect();
  assert_eq!(rows.len(), 28);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let decoded = decode_ordered_index_artifact(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    let (observed, key) = match decoded {
      OrderedIndexArtifactV1::Directory(directory) => (
        format!(
          "index:directory:{}:level={}:entries={}:live={}:pages={}:fences={}/{}",
          directory.role.name(),
          directory.level,
          directory.entries.len(),
          directory.live_count,
          directory.page_count,
          directory.lower_fence.len(),
          directory.upper_fence.len()
        ),
        directory.key,
      ),
      OrderedIndexArtifactV1::Page(page) => {
        assert_eq!(page.records.iter().map(|record| record.unwrap()).count(), page.records.len(), "fixture {} record iterator", row.id);
        let observed = match page.role {
          OrderedIndexRoleV1::ScopeOrdinal => format!("index:page:scope-catalog:ordinal:records={}", page.records.len()),
          OrderedIndexRoleV1::ScopeReverse => format!("index:page:scope-catalog:reverse:records={}", page.records.len()),
          OrderedIndexRoleV1::Value => format!("index:page:value:page-id={}:records={}", page.page_id, page.records.len()),
          OrderedIndexRoleV1::ValueDocumentState => {
            format!("index:page:document-state:value-store:page-id={}:records={}", page.page_id, page.records.len())
          }
          OrderedIndexRoleV1::Posting => format!("index:page:posting:page-id={}:records={}", page.page_id, page.records.len()),
          OrderedIndexRoleV1::IndexDocumentState => {
            format!("index:page:document-state:index:page-id={}:records={}", page.page_id, page.records.len())
          }
          OrderedIndexRoleV1::NvtTile => panic!("NVT tiles are not ordered pages"),
        };
        (observed, page.key)
      }
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn ordered_pages_reject_corrupt_aggregates_records_order_and_catalog_bijection() {
  let root = fixture_root();
  let posting_directory = fs::read(root.join("index-artifact-v1/aidx-blake3-256-posting-directory-leaf-valid.bin")).unwrap();
  let directory_body = 32 + 32 + 2;

  let mut wrong_codec = posting_directory.clone();
  wrong_codec[directory_body + 2] ^= 1;
  repair_trailing_crc(&mut wrong_codec);
  assert_eq!(
    decode_ordered_index_artifact(&wrong_codec, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut wrong_aggregate = posting_directory;
  wrong_aggregate[directory_body + 24] ^= 1;
  repair_trailing_crc(&mut wrong_aggregate);
  assert_eq!(
    decode_ordered_index_artifact(&wrong_aggregate, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let posting_directory = fs::read(root.join("index-artifact-v1/aidx-blake3-256-posting-directory-leaf-valid.bin")).unwrap();
  let mut excessive_entries = posting_directory.clone();
  excessive_entries[directory_body + 4..directory_body + 8].copy_from_slice(&65_537u32.to_le_bytes());
  repair_trailing_crc(&mut excessive_entries);
  assert_eq!(
    decode_ordered_index_artifact(&excessive_entries, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut unknown_role = posting_directory;
  unknown_role[32 + 32 + 1] = 0xff;
  repair_trailing_crc(&mut unknown_role);
  assert_eq!(
    decode_ordered_index_artifact(&unknown_role, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let posting_page = fs::read(root.join("index-artifact-v1/aidx-blake3-256-posting-page-valid.bin")).unwrap();
  let mut wrong_live_count = posting_page;
  wrong_live_count[32 + 32 + 8 + 36] ^= 1;
  repair_trailing_crc(&mut wrong_live_count);
  assert_eq!(
    decode_ordered_index_artifact(&wrong_live_count, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let ordinal = fs::read(root.join("index-artifact-v1/aidx-blake3-256-scope-ordinal-page-valid.bin")).unwrap();
  let mut wrong_scope_identity = ordinal.clone();
  let ordinal_body = 32 + 32 + 9;
  let ordinal_lower = test_u32(&wrong_scope_identity, ordinal_body + 24) as usize;
  let ordinal_upper = test_u32(&wrong_scope_identity, ordinal_body + 28) as usize;
  let ordinal_record = ordinal_body + 96 + ordinal_lower + ordinal_upper;
  wrong_scope_identity[ordinal_record + 16] ^= 1;
  repair_trailing_crc(&mut wrong_scope_identity);
  assert_eq!(
    decode_ordered_index_artifact(&wrong_scope_identity, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut wrong_state = fs::read(root.join("index-artifact-v1/aidx-blake3-256-value-document-state-page-valid.bin")).unwrap();
  let state_body = 32 + 32 + 16;
  let state_lower = test_u32(&wrong_state, state_body + 24) as usize;
  let state_upper = test_u32(&wrong_state, state_body + 28) as usize;
  let state_record = state_body + 96 + state_lower + state_upper;
  wrong_state[state_record + 1] = 5;
  repair_trailing_crc(&mut wrong_state);
  assert_eq!(
    decode_ordered_index_artifact(&wrong_state, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let reverse = fs::read(root.join("index-artifact-v1/aidx-blake3-256-scope-reverse-page-valid.bin")).unwrap();
  validate_scope_catalog_pair(&ordinal, &reverse, HashAlgorithm::Blake3_256).unwrap();
  let mut changed_reverse = reverse;
  let reverse_body = 32 + 1 + 2 * 32;
  let lower_length = test_u32(&changed_reverse, reverse_body + 24) as usize;
  let upper_length = test_u32(&changed_reverse, reverse_body + 28) as usize;
  let first_record = reverse_body + 96 + lower_length + upper_length;
  changed_reverse[first_record + 4..first_record + 12].copy_from_slice(&99u64.to_le_bytes());
  repair_trailing_crc(&mut changed_reverse);
  assert_eq!(
    validate_scope_catalog_pair(&ordinal, &changed_reverse, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let low = 255u64.to_le_bytes();
  let high = 256u64.to_le_bytes();
  assert!(low.as_slice() > high.as_slice());
  assert_eq!(
    compare_order_keys(HashAlgorithm::Blake3_256, OrderedIndexRoleV1::ScopeOrdinal, &low, &high).unwrap(),
    std::cmp::Ordering::Less
  );

  assert_eq!(
    decode_ordered_index_artifact(&vec![0; 4 * 1_024 * 1_024 + 1], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn directory_physical_hints_are_optional_and_never_correctness_authority() {
  let root = fixture_root();
  let mut bytes = fs::read(root.join("index-artifact-v1/aidx-blake3-256-posting-directory-leaf-valid.bin")).unwrap();
  let body = 32 + 32 + 2;
  let descriptor = body + 80 + 57 + 57;
  let hint = descriptor + 16 + 32 + 32;
  bytes[hint..hint + 8].copy_from_slice(&1_234u64.to_le_bytes());
  repair_trailing_crc(&mut bytes);
  let OrderedIndexArtifactV1::Directory(directory) = decode_ordered_index_artifact(&bytes, HashAlgorithm::Blake3_256).unwrap() else {
    panic!("expected directory");
  };
  assert!(!directory.entries[0].physical_hint.is_complete());

  bytes[hint + 8..hint + 12].copy_from_slice(&4_096u32.to_le_bytes());
  bytes[hint + 16..hint + 24].copy_from_slice(&77u64.to_le_bytes());
  repair_trailing_crc(&mut bytes);
  let OrderedIndexArtifactV1::Directory(directory) = decode_ordered_index_artifact(&bytes, HashAlgorithm::Blake3_256).unwrap() else {
    panic!("expected directory");
  };
  assert!(directory.entries[0].physical_hint.is_complete());
  assert!(directory.entries[0].physical_hint.matches(1_234, 4_096, 77));
  assert!(!directory.entries[0].physical_hint.matches(1_235, 4_096, 77));

  bytes[hint + 12] = 1;
  repair_trailing_crc(&mut bytes);
  assert_eq!(
    decode_ordered_index_artifact(&bytes, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );
}

#[test]
fn every_sparse_nvt_tile_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| row.format_id == "index-artifact-v1" && row.expected.starts_with("index:nvt-tile:"))
    .collect();
  assert_eq!(rows.len(), 2);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let tile = decode_nvt_tile(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    assert_eq!(tile.entries.iter().map(|entry| entry.unwrap()).count(), tile.entries.len(), "fixture {} entry iterator", row.id);
    let first = tile.entries.entry_at(0).unwrap();
    let last = tile.entries.entry_at(tile.entries.len() - 1).unwrap();
    let observed = format!(
      "index:nvt-tile:resolution={}:start={}:cells={}:entries={}:span={}/{}:basis={}:approx={}",
      tile.resolution,
      tile.tile_start_cell,
      tile.tile_cell_count,
      tile.entries.len(),
      first.relative_cell,
      last.relative_cell,
      tile.basis_posting_generation,
      tile.approximate_postings
    );
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(tile.key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn sparse_nvt_rejects_malformed_tiles_and_treats_stale_hints_as_misses() {
  let root = fixture_root();
  let baseline = fs::read(root.join("index-artifact-v1/aidx-blake3-256-nvt-tile-valid.bin")).unwrap();
  let body = 32 + 32 + 8;

  for offset in [body + 4, body + 16, body + 24, body + 28, body + 40, body + 48, body + 56] {
    let mut changed = baseline.clone();
    changed[offset] ^= 1;
    repair_trailing_crc(&mut changed);
    assert!(decode_nvt_tile(&changed, HashAlgorithm::Blake3_256).is_err(), "tile header offset {offset} accepted");
  }

  let first_entry = body + 64;
  let mut wrong_cell = baseline.clone();
  wrong_cell[first_entry + 40..first_entry + 44].copy_from_slice(&3u32.to_le_bytes());
  repair_trailing_crc(&mut wrong_cell);
  assert_eq!(
    decode_nvt_tile(&wrong_cell, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let mut wrong_sample = baseline.clone();
  wrong_sample[first_entry + 32..first_entry + 40].fill(0);
  repair_trailing_crc(&mut wrong_sample);
  assert_eq!(
    decode_nvt_tile(&wrong_sample, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let tile = decode_nvt_tile(&baseline, HashAlgorithm::Blake3_256).unwrap();
  assert!(tile.predecessor_entry(2).is_none());
  let first = tile.predecessor_entry(100).unwrap();
  assert_eq!(first.relative_cell, 3);
  assert_eq!(coordinate_cell(first.sample_coordinate, tile.resolution), Some(tile.tile_start_cell + u64::from(first.relative_cell)));
  assert_eq!(verified_page_hint(first.predecessor_page_id, 300, 400, &[301, 302, 350]), Some(301));
  assert_eq!(verified_page_hint(first.predecessor_page_id, 302, 400, &[302, 350]), None);
  assert_eq!(verified_page_hint(Some(349), 300, 400, &[301, 302, 350]), None);
  assert_eq!(verified_predecessor_or_fallback(Some(&tile), 100, 300, 400, &[301, 302, 350], 399), 301);
  assert_eq!(verified_predecessor_or_fallback(Some(&tile), 100, 302, 400, &[302, 350], 399), 399);
  assert_eq!(verified_predecessor_or_fallback(Some(&tile), 100, 300, 400, &[350, 302], 399), 399);
  assert_eq!(coordinate_cell(u64::MAX, tile.resolution), Some(tile.resolution - 1));
  assert_eq!(coordinate_cell(0, 0), None);

  let original_key = tile.key.clone();
  let mut changed_hint = baseline.clone();
  changed_hint[first_entry + 8] ^= 0x80;
  repair_trailing_crc(&mut changed_hint);
  let changed = decode_nvt_tile(&changed_hint, HashAlgorithm::Blake3_256).unwrap();
  assert_ne!(changed.key, original_key);

  let mut corrupt = baseline;
  corrupt[64] ^= 1;
  let corrupt = decode_nvt_tile(&corrupt, HashAlgorithm::Blake3_256).ok();
  assert_eq!(verified_predecessor_or_fallback(corrupt.as_ref(), 100, 300, 400, &[301, 302, 350], 399), 399);
}

#[test]
fn every_index_journal_and_checkpoint_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| {
      row.format_id == "index-artifact-v1" && (row.expected.starts_with("index:journal:") || row.expected.starts_with("index:checkpoint:"))
    })
    .collect();
  assert_eq!(rows.len(), 8);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let decoded = decode_index_task_artifact(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    let (observed, key) = match decoded {
      IndexTaskArtifactV1::Journal(journal) => (
        format!(
          "index:journal:{}:generation={}:segment={}:reset={}:records={}:sequences={}/{}",
          journal.owner_kind.name(),
          journal.generation,
          journal.segment_ordinal,
          journal.chain_reset,
          journal.records.len(),
          journal.first_sequence,
          journal.last_sequence
        ),
        journal.key,
      ),
      IndexTaskArtifactV1::Checkpoint(checkpoint) => {
        assert_eq!(
          checkpoint.attachments.iter().map(|attachment| attachment.unwrap()).count(),
          checkpoint.attachments.len(),
          "fixture {} attachment iterator",
          row.id
        );
        (
          format!(
            "index:checkpoint:{}:task={}:state={}:phase={}:sequence={}:attachments={}",
            if checkpoint.external.is_some() { "external" } else { "embedded" },
            checkpoint.task_kind.name(),
            checkpoint.state.name(),
            checkpoint.phase_name,
            checkpoint.checkpoint_sequence,
            checkpoint.attachments.len()
          ),
          checkpoint.key,
        )
      }
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(key), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn index_journals_and_checkpoints_reject_broken_chains_batches_bounds_and_external_state() {
  let root = fixture_root();
  let task = fs::read(root.join("index-artifact-v1/aidx-blake3-256-task-mutation-journal-valid.bin")).unwrap();
  let body = 32 + 24;
  let record = body + 56 + 4 * 32;

  let mut excessive_records = task.clone();
  excessive_records[body + 32..body + 36].copy_from_slice(&10_001u32.to_le_bytes());
  repair_trailing_crc(&mut excessive_records);
  assert_eq!(
    decode_index_task_artifact(&excessive_records, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut broken_batch = task.clone();
  broken_batch[record + 20..record + 24].copy_from_slice(&3u32.to_le_bytes());
  repair_trailing_crc(&mut broken_batch);
  assert_eq!(
    decode_index_task_artifact(&broken_batch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut invalid_path_key = task.clone();
  invalid_path_key[record + 24 + 5 * 32] ^= 1;
  repair_trailing_crc(&mut invalid_path_key);
  assert_eq!(
    decode_index_task_artifact(&invalid_path_key, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let second_record = record + test_u32(&task, record) as usize;
  for kind in [5u16, 6, 7] {
    let mut compatible_presence = task.clone();
    compatible_presence[record + 4..record + 6].copy_from_slice(&kind.to_le_bytes());
    compatible_presence[second_record + 4..second_record + 6].copy_from_slice(&kind.to_le_bytes());
    repair_trailing_crc(&mut compatible_presence);
    decode_index_task_artifact(&compatible_presence, HashAlgorithm::Blake3_256).unwrap();
  }
  let mut unknown_mutation = task.clone();
  unknown_mutation[record + 4..record + 6].copy_from_slice(&8u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_mutation);
  assert_eq!(
    decode_index_task_artifact(&unknown_mutation, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut missing_reset = task.clone();
  missing_reset[body..body + 4].fill(0);
  repair_trailing_crc(&mut missing_reset);
  assert_eq!(
    decode_index_task_artifact(&missing_reset, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let system = fs::read(root.join("index-artifact-v1/aidx-blake3-256-system-mutation-journal-valid.bin")).unwrap();
  let system_record = body + 56 + 4 * 32;
  for kind in [3u16, 4] {
    let mut invalid_presence = system.clone();
    invalid_presence[system_record + 4..system_record + 6].copy_from_slice(&kind.to_le_bytes());
    repair_trailing_crc(&mut invalid_presence);
    assert_eq!(
      decode_index_task_artifact(&invalid_presence, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );
  }
  let mut wrong_system_owner = system;
  wrong_system_owner[32] ^= 1;
  repair_trailing_crc(&mut wrong_system_owner);
  assert_eq!(
    decode_index_task_artifact(&wrong_system_owner, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let IndexTaskArtifactV1::Journal(journal) = decode_index_task_artifact(&task, HashAlgorithm::Blake3_256).unwrap() else {
    panic!("expected journal");
  };
  assert_eq!(journal.records.iter().map(|record| record.unwrap()).count(), journal.records.len());
  assert_eq!(validate_journal_chain(&journal, &journal).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let checkpoint = fs::read(root.join("index-artifact-v1/aidx-blake3-256-index-task-checkpoint-embedded-valid.bin")).unwrap();
  let fixed = 120 + 4 * 32;

  let mut unknown_phase = checkpoint.clone();
  unknown_phase[body + 10..body + 12].copy_from_slice(&99u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_phase);
  assert_eq!(
    decode_index_task_artifact(&unknown_phase, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut unknown_capability = checkpoint.clone();
  unknown_capability[body + 12 + 3] = 1;
  repair_trailing_crc(&mut unknown_capability);
  assert_eq!(
    decode_index_task_artifact(&unknown_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownRequiredCapability
  );

  let mut unknown_state = checkpoint.clone();
  unknown_state[body + 8..body + 10].copy_from_slice(&8u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_state);
  assert_eq!(
    decode_index_task_artifact(&unknown_state, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut reversed_time = checkpoint.clone();
  reversed_time[body + 52..body + 60].fill(0);
  repair_trailing_crc(&mut reversed_time);
  assert_eq!(
    decode_index_task_artifact(&reversed_time, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut oversized_resume = checkpoint.clone();
  oversized_resume[body + 100 + 4 * 32..body + 104 + 4 * 32].copy_from_slice(&1_048_577u32.to_le_bytes());
  repair_trailing_crc(&mut oversized_resume);
  assert_eq!(
    decode_index_task_artifact(&oversized_resume, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut excessive_attachments = checkpoint.clone();
  excessive_attachments[body + 104 + 4 * 32..body + 108 + 4 * 32].copy_from_slice(&4_097u32.to_le_bytes());
  repair_trailing_crc(&mut excessive_attachments);
  assert_eq!(
    decode_index_task_artifact(&excessive_attachments, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let resume_length = test_u32(&checkpoint, body + 100 + 4 * 32) as usize;
  let first_attachment = body + fixed + resume_length;
  let mut unknown_attachment = checkpoint;
  unknown_attachment[first_attachment..first_attachment + 2].copy_from_slice(&13u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_attachment);
  assert_eq!(
    decode_index_task_artifact(&unknown_attachment, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let external = fs::read(root.join("index-artifact-v1/aidx-blake3-256-index-task-checkpoint-external-valid.bin")).unwrap();
  let external_length = test_u32(&external, body + 112 + 4 * 32) as usize;
  let external_start = external.len() - 4 - external_length;
  let mut oversized_external = external.clone();
  oversized_external[body + 112 + 4 * 32..body + 116 + 4 * 32].copy_from_slice(&65_537u32.to_le_bytes());
  repair_trailing_crc(&mut oversized_external);
  assert_eq!(
    decode_index_task_artifact(&oversized_external, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut relative_path = external;
  relative_path[external_start + 68] = b'x';
  repair_trailing_crc(&mut relative_path);
  assert_eq!(
    decode_index_task_artifact(&relative_path, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let mut oversized_checkpoint = vec![0u8; 4 * 1_024 * 1_024 + 1];
  oversized_checkpoint[6..8].copy_from_slice(&0x0041u16.to_le_bytes());
  assert_eq!(
    decode_index_task_artifact(&oversized_checkpoint, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn every_index_task_kind_has_a_closed_phase_registry() {
  let tasks = [
    (IndexTaskKindV1::ScopeBuild, 6u16),
    (IndexTaskKindV1::ValueBuild, 6),
    (IndexTaskKindV1::FieldBuild, 6),
    (IndexTaskKindV1::NvtBuild, 5),
    (IndexTaskKindV1::Reconcile, 6),
    (IndexTaskKindV1::V0Migration, 6),
    (IndexTaskKindV1::Compaction, 5),
    (IndexTaskKindV1::IndexRepair, 5),
  ];
  for (task, maximum_phase) in tasks {
    assert!(task.phase_name(1).is_some());
    assert!(task.phase_name(maximum_phase).is_some());
    assert!(task.phase_name(0).is_none());
    assert!(task.phase_name(maximum_phase + 1).is_none());
  }
}

#[test]
fn every_logical_position_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "logical-position-v1").collect();
  assert_eq!(rows.len(), 10);

  for row in rows {
    let token = fs::read(root.join(row.binary)).unwrap();
    let position = decode_logical_position(&token, hash_algorithm(&row.hash_algorithm)).unwrap();
    assert_eq!(position.components().map(|component| component.unwrap()).count(), position.component_count as usize);
    let identity = format!(
      "tuple={}:order={}:root={}:file={}:revision={}",
      position.sort_tuple().len(),
      hex::encode(&position.order_fingerprint()[..4]),
      hex::encode(&position.namespace_root()[..4]),
      hex::encode(&position.file_key_tie()[..4]),
      hex::encode(&position.record_revision_tie()[..4])
    );
    let observed = if position.decoded_len() == 1_048_576 {
      format!(
        "position:maximum:route={}:components={}:decoded={}:{identity}",
        position.route.name(),
        position.component_count,
        position.decoded_len()
      )
    } else {
      format!("position:{}:components={}:decoded={}:{identity}", position.route.name(), position.component_count, position.decoded_len())
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(row.canonical_key, None, "public APOS token has no KV key");
  }
}

#[test]
fn logical_positions_reject_malformed_framing_components_and_amplification() {
  use base64::Engine as _;
  use base64::engine::general_purpose::URL_SAFE_NO_PAD;

  let root = fixture_root();
  let token = fs::read(root.join("logical-position-v1/apos-blake3-256-query-valid.bin")).unwrap();
  let decoded = URL_SAFE_NO_PAD.decode(&token).unwrap();

  let mut padded = token.clone();
  padded.push(b'=');
  assert_eq!(
    decode_logical_position(&padded, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  assert_eq!(
    decode_logical_position(&vec![b'a'; 1_398_103], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  assert!(decode_logical_position(b"not+base64/url", HashAlgorithm::Blake3_256).is_err());

  let mut bad_crc_decoded = decoded.clone();
  let bad_crc_offset = bad_crc_decoded.len() - 4;
  bad_crc_decoded[bad_crc_offset] ^= 1;
  let bad_crc = URL_SAFE_NO_PAD.encode(bad_crc_decoded).into_bytes();
  assert_eq!(
    decode_logical_position(&bad_crc, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  for (offset, value, class) in [
    (6usize, 99u16, MalformedInputClass::UnknownTypeKindOrEnum),
    (12, HashAlgorithm::Sha512.to_u16(), MalformedInputClass::CrossRecordClosureMismatch),
  ] {
    let mut changed = decoded.clone();
    changed[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    let changed = repair_position_crc_and_encode(&mut changed);
    assert_eq!(decode_logical_position(&changed, HashAlgorithm::Blake3_256).unwrap_err().class(), class);
  }

  for offset in [0usize, 4] {
    let mut changed = decoded.clone();
    changed[offset] ^= 0x80;
    let changed = repair_position_crc_and_encode(&mut changed);
    assert_eq!(
      decode_logical_position(&changed, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::UnknownMagicOrVersion
    );
  }

  let mut wrong_length = decoded.clone();
  wrong_length[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
  let wrong_length = repair_position_crc_and_encode(&mut wrong_length);
  assert_eq!(
    decode_logical_position(&wrong_length, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );

  let mut excessive_count = decoded.clone();
  excessive_count[14] = 33;
  let excessive_count = repair_position_crc_and_encode(&mut excessive_count);
  assert_eq!(
    decode_logical_position(&excessive_count, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut nonzero_flags = decoded.clone();
  nonzero_flags[15] = 1;
  let nonzero_flags = repair_position_crc_and_encode(&mut nonzero_flags);
  assert_eq!(
    decode_logical_position(&nonzero_flags, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  for range in [16..48, 48..80, 84..116, 116..148] {
    let mut zero_identity = decoded.clone();
    zero_identity[range].fill(0);
    let zero_identity = repair_position_crc_and_encode(&mut zero_identity);
    assert_eq!(
      decode_logical_position(&zero_identity, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch
    );
  }

  let tuple_start = 20 + 4 * 32;
  let mut bad_utf8 = decoded.clone();
  bad_utf8[tuple_start + 8] = 0xff;
  let bad_utf8 = repair_position_crc_and_encode(&mut bad_utf8);
  assert_eq!(
    decode_logical_position(&bad_utf8, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let mut component_reserved = decoded.clone();
  component_reserved[tuple_start + 3] = 1;
  let component_reserved = repair_position_crc_and_encode(&mut component_reserved);
  assert_eq!(
    decode_logical_position(&component_reserved, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut unknown_comparator = decoded.clone();
  unknown_comparator[tuple_start..tuple_start + 2].copy_from_slice(&1u16.to_le_bytes());
  let unknown_comparator = repair_position_crc_and_encode(&mut unknown_comparator);
  assert_eq!(
    decode_logical_position(&unknown_comparator, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let first_component_length = 8 + test_u32(&decoded, tuple_start + 4) as usize;
  let mut invalid_state = decoded.clone();
  invalid_state[tuple_start + first_component_length + 2] = 3;
  let invalid_state = repair_position_crc_and_encode(&mut invalid_state);
  assert_eq!(
    decode_logical_position(&invalid_state, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
  );

  let mut truncated_component = decoded.clone();
  truncated_component[tuple_start + 4..tuple_start + 8].copy_from_slice(&u32::MAX.to_le_bytes());
  let truncated_component = repair_position_crc_and_encode(&mut truncated_component);
  assert_eq!(
    decode_logical_position(&truncated_component, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );

  let mut wrong_count = decoded;
  wrong_count[14] = 2;
  let wrong_count = repair_position_crc_and_encode(&mut wrong_count);
  assert_eq!(
    decode_logical_position(&wrong_count, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn logical_positions_close_every_present_comparator_payload() {
  use base64::Engine as _;
  use base64::engine::general_purpose::URL_SAFE_NO_PAD;

  let root = fixture_root();
  let token = fs::read(root.join("logical-position-v1/apos-blake3-256-directory-listing-valid.bin")).unwrap();
  let decoded = URL_SAFE_NO_PAD.decode(token).unwrap();
  let tuple_start = 20 + 4 * 32;

  for tag in [4u16, 5, 7] {
    let mut changed = decoded.clone();
    changed[tuple_start..tuple_start + 2].copy_from_slice(&tag.to_le_bytes());
    let changed = repair_position_crc_and_encode(&mut changed);
    decode_logical_position(&changed, HashAlgorithm::Blake3_256).unwrap();
  }

  for value in [f64::NAN, f64::INFINITY, -0.0] {
    let mut changed = decoded.clone();
    changed[tuple_start..tuple_start + 2].copy_from_slice(&6u16.to_le_bytes());
    changed[tuple_start + 8..tuple_start + 16].copy_from_slice(&value.to_le_bytes());
    let changed = repair_position_crc_and_encode(&mut changed);
    assert_eq!(
      decode_logical_position(&changed, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
    );
  }

  let maximum = fs::read(root.join("logical-position-v1/apos-blake3-256-maximum-decoded-length-valid.bin")).unwrap();
  let mut maximum = URL_SAFE_NO_PAD.decode(maximum).unwrap();
  maximum[tuple_start + 8] = 2;
  let maximum = repair_position_crc_and_encode(&mut maximum);
  assert_eq!(
    decode_logical_position(&maximum, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn logical_position_context_revalidates_route_root_order_and_resolved_ties() {
  let root = fixture_root();
  let token = fs::read(root.join("logical-position-v1/apos-blake3-256-query-valid.bin")).unwrap();
  let position = decode_logical_position(&token, HashAlgorithm::Blake3_256).unwrap();
  let context = PositionContextV1 {
    route: PositionRouteV1::Query,
    namespace_root: position.namespace_root(),
    order_fingerprint: position.order_fingerprint(),
    file_key_tie: position.file_key_tie(),
    record_revision_tie: position.record_revision_tie(),
    sort_tuple: position.sort_tuple(),
  };
  validate_position_context(&position, context).unwrap();

  let mut changed_root = position.namespace_root().to_vec();
  changed_root[0] ^= 1;
  let root_mismatch = PositionContextV1 { namespace_root: &changed_root, ..context };
  assert_eq!(validate_position_context(&position, root_mismatch).unwrap_err().code(), "position_root_mismatch");

  let mut changed_order = position.order_fingerprint().to_vec();
  changed_order[0] ^= 1;
  let order_mismatch = PositionContextV1 { order_fingerprint: &changed_order, ..context };
  assert_eq!(validate_position_context(&position, order_mismatch).unwrap_err().code(), "position_order_mismatch");

  let route_mismatch = PositionContextV1 { route: PositionRouteV1::DirectoryListing, ..context };
  assert_eq!(validate_position_context(&position, route_mismatch).unwrap_err().code(), "invalid_position_cursor");

  let mut changed_tuple = position.sort_tuple().to_vec();
  changed_tuple[0] ^= 1;
  let tuple_mismatch = PositionContextV1 { sort_tuple: &changed_tuple, ..context };
  assert_eq!(validate_position_context(&position, tuple_mismatch).unwrap_err().code(), "invalid_position_cursor");

  let mut changed_file = position.file_key_tie().to_vec();
  changed_file[0] ^= 1;
  let file_mismatch = PositionContextV1 { file_key_tie: &changed_file, ..context };
  assert_eq!(validate_position_context(&position, file_mismatch).unwrap_err().code(), "invalid_position_cursor");

  let mut changed_revision = position.record_revision_tie().to_vec();
  changed_revision[0] ^= 1;
  let revision_mismatch = PositionContextV1 { record_revision_tie: &changed_revision, ..context };
  assert_eq!(validate_position_context(&position, revision_mismatch).unwrap_err().code(), "invalid_position_cursor");
}

#[test]
fn every_gc_active_control_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> =
    manifest().fixtures.into_iter().filter(|row| row.format_id == "gc-artifact-v1" && row.expected.starts_with("gc:control:")).collect();
  assert_eq!(rows.len(), 24);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let control = decode_gc_active_control(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    let observed = format!(
      "gc:control:{}:slot-{}:sequence={}:generation={}",
      control.kind.name(),
      if control.slot == 0 { 'a' } else { 'b' },
      control.sequence,
      control.generation
    );
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(control.key), row.canonical_key.unwrap(), "fixture {}", row.id);
    assert_eq!(control.kind.control_target().unwrap().is_control(), false);
  }
}

#[test]
fn gc_envelope_controls_and_pair_selection_fail_closed() {
  let root = fixture_root();
  let a = fs::read(root.join("gc-artifact-v1/agca-blake3-256-quarantine-control-a.bin")).unwrap();
  let b = fs::read(root.join("gc-artifact-v1/agca-blake3-256-quarantine-control-b.bin")).unwrap();
  let decoded_a = decode_gc_active_control(&a, HashAlgorithm::Blake3_256).unwrap();
  let decoded_b = decode_gc_active_control(&b, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(select_gc_active_control(&decoded_a, true, &decoded_b, true).unwrap().unwrap().slot, 1);
  assert_eq!(select_gc_active_control(&decoded_a, true, &decoded_b, false).unwrap().unwrap().slot, 0);
  assert!(select_gc_active_control(&decoded_a, false, &decoded_b, false).unwrap().is_none());

  for offset in [0usize, 4, 6, 8, 10, 12, 16, 18, 20] {
    let mut changed = a.clone();
    changed[offset] ^= 0x80;
    repair_trailing_crc(&mut changed);
    assert!(decode_gc_active_control(&changed, HashAlgorithm::Blake3_256).is_err(), "offset {offset} accepted");
  }

  let mut zero_generation = a.clone();
  zero_generation[24..32].fill(0);
  repair_trailing_crc(&mut zero_generation);
  assert_eq!(
    decode_gc_active_control(&zero_generation, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let body = 32 + 17;
  let mut zero_sequence = a.clone();
  zero_sequence[body..body + 8].fill(0);
  repair_trailing_crc(&mut zero_sequence);
  assert_eq!(
    decode_gc_active_control(&zero_sequence, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut zero_target = a.clone();
  zero_target[body + 8..body + 40].fill(0);
  repair_trailing_crc(&mut zero_target);
  assert_eq!(
    decode_gc_active_control(&zero_target, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut bad_crc = a.clone();
  *bad_crc.last_mut().unwrap() ^= 1;
  assert_eq!(decode_gc_artifact_envelope(&bad_crc).unwrap_err().class(), MalformedInputClass::ChecksumOrIntegrityMismatch);

  assert_eq!(GcArtifactKindV1::ALL.len(), 34);
  for kind in GcArtifactKindV1::ALL {
    assert_eq!(GcArtifactKindV1::from_u16(kind as u16), Some(kind));
  }

  let mut equal_b = b;
  equal_b[body..body + 8].copy_from_slice(&1u64.to_le_bytes());
  repair_trailing_crc(&mut equal_b);
  let equal_b = decode_gc_active_control(&equal_b, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(select_gc_active_control(&decoded_a, true, &equal_b, true).unwrap().unwrap().slot, 0);

  let mut ambiguous_b_bytes = fs::read(root.join("gc-artifact-v1/agca-blake3-256-quarantine-control-b.bin")).unwrap();
  ambiguous_b_bytes[body..body + 8].copy_from_slice(&1u64.to_le_bytes());
  ambiguous_b_bytes[body + 8] ^= 1;
  repair_trailing_crc(&mut ambiguous_b_bytes);
  let ambiguous_b = decode_gc_active_control(&ambiguous_b_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    select_gc_active_control(&decoded_a, true, &ambiguous_b, true).unwrap_err().class(),
    MalformedInputClass::AmbiguousEqualSequenceSelector
  );

  let other_b_bytes = fs::read(root.join("gc-artifact-v1/agca-blake3-256-mark-run-control-b.bin")).unwrap();
  let other_b = decode_gc_active_control(&other_b_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    select_gc_active_control(&decoded_a, true, &other_b, true).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn physical_incarnation_reader_closes_v0_v1_and_range_invariants() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let h = algorithm.hash_length();
    for version in [0u8, 1] {
      let mut bytes = vec![0u8; 24 + 2 * h];
      bytes[..h].fill(0x11);
      bytes[h..2 * h].fill(0x22);
      bytes[2 * h..2 * h + 8].copy_from_slice(&2_048u64.to_le_bytes());
      bytes[2 * h + 8..2 * h + 16].copy_from_slice(&(if version == 0 { 0 } else { 77u64 }).to_le_bytes());
      bytes[2 * h + 16..2 * h + 20].copy_from_slice(&4_096u32.to_le_bytes());
      bytes[2 * h + 20] = 2;
      bytes[2 * h + 21] = version;
      let incarnation: PhysicalIncarnationV1<'_> = decode_physical_incarnation(&bytes, algorithm).unwrap();
      assert_eq!(incarnation.entity_version, version);
      assert_eq!(incarnation.write_sequence, if version == 0 { 0 } else { 77 });

      for range in [0..h, h..2 * h, 2 * h..2 * h + 8, 2 * h + 16..2 * h + 20] {
        let mut changed = bytes.clone();
        changed[range].fill(0);
        assert!(decode_physical_incarnation(&changed, algorithm).is_err());
      }

      let mut bad_reserved = bytes.clone();
      bad_reserved[2 * h + 22] = 1;
      assert_eq!(decode_physical_incarnation(&bad_reserved, algorithm).unwrap_err().class(), MalformedInputClass::NonzeroReservedOrPadding);

      let mut overflow = bytes;
      overflow[2 * h..2 * h + 8].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
      overflow[2 * h + 16..2 * h + 20].copy_from_slice(&4u32.to_le_bytes());
      assert_eq!(
        decode_physical_incarnation(&overflow, algorithm).unwrap_err().class(),
        MalformedInputClass::LengthCountOrArithmeticOverflow
      );
    }
  }
}

#[test]
fn every_gc_lifecycle_and_inventory_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| row.format_id == "gc-artifact-v1" && !row.expected.starts_with("gc:control:"))
    .filter(|row| {
      row.expected.starts_with("gc:page:candidate:")
        || row.expected.starts_with("gc:page:root-expiry:")
        || row.expected.starts_with("gc:page:root-candidate:")
        || row.expected.starts_with("gc:page:physical-inventory:")
        || row.expected.starts_with("gc:directory:candidates:")
        || row.expected.starts_with("gc:directory:root-expiry:")
        || row.expected.starts_with("gc:directory:root-candidates:")
        || row.expected.starts_with("gc:directory:physical-inventory:")
        || row.expected.starts_with("gc:delta:candidate:")
        || row.expected.starts_with("gc:manifest:root-expiry:")
        || row.expected.starts_with("gc:manifest:root-lifecycle:")
        || row.expected.starts_with("gc:manifest:physical-inventory:")
        || row.expected.starts_with("gc:manifest:quarantine:")
        || row.expected.starts_with("gc:commit:root-retirement:")
        || row.expected.starts_with("gc:proof:root-object-reclaim:")
        || row.expected.starts_with("gc:journal:retirement:")
    })
    .collect();
  assert_eq!(rows.len(), 40);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let artifact = decode_gc_state_artifact(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    assert_eq!(artifact.summary(), row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(artifact.key()), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn gc_lifecycle_pages_manifests_and_retirement_reject_semantic_corruption() {
  let root = fixture_root();
  let candidate = fs::read(root.join("gc-artifact-v1/agca-blake3-256-candidate-page-valid.bin")).unwrap();
  let candidate_body = 32 + test_u16(&candidate, 16) as usize;
  let mut bad_page = candidate.clone();
  bad_page[candidate_body + 40] = 1;
  repair_trailing_crc(&mut bad_page);
  assert_eq!(
    decode_gc_state_artifact(&bad_page, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let directory = fs::read(root.join("gc-artifact-v1/agca-blake3-256-candidates-directory-valid.bin")).unwrap();
  let decoded_page = decode_gc_state_artifact(&candidate, HashAlgorithm::Blake3_256).unwrap();
  let decoded_directory = decode_gc_state_artifact(&directory, HashAlgorithm::Blake3_256).unwrap();
  let (GcStateArtifactV1::Page(page), GcStateArtifactV1::Directory(directory)) = (&decoded_page, &decoded_directory) else {
    panic!("expected candidate page and directory");
  };
  validate_gc_directory_page(directory, page).unwrap();

  let mut generic_database = candidate.clone();
  generic_database[32] ^= 0x40;
  repair_trailing_crc(&mut generic_database);
  let generic_database = decode_gc_state_artifact(&generic_database, HashAlgorithm::Blake3_256).unwrap();
  let GcStateArtifactV1::Page(generic_page) = generic_database else {
    panic!("expected generic candidate page");
  };
  assert_ne!(generic_page.database_id, page.database_id);

  let mut stale_directory_bytes = fs::read(root.join("gc-artifact-v1/agca-blake3-256-candidates-directory-valid.bin")).unwrap();
  let body_start = 32 + test_u16(&stale_directory_bytes, 16) as usize;
  let lower_length = test_u32(&stale_directory_bytes, body_start + 16) as usize;
  let upper_length = test_u32(&stale_directory_bytes, body_start + 20) as usize;
  let child_hash = body_start + 80 + lower_length + upper_length + 16;
  stale_directory_bytes[child_hash] ^= 1;
  repair_trailing_crc(&mut stale_directory_bytes);
  let stale_directory = decode_gc_state_artifact(&stale_directory_bytes, HashAlgorithm::Blake3_256).unwrap();
  let GcStateArtifactV1::Directory(stale_directory) = stale_directory else {
    panic!("expected stale directory");
  };
  assert_eq!(validate_gc_directory_page(&stale_directory, page).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let delta = fs::read(root.join("gc-artifact-v1/agca-blake3-256-candidate-delta-valid.bin")).unwrap();
  let delta_body = 32 + test_u16(&delta, 16) as usize;
  let mut invalid_operation = delta;
  invalid_operation[delta_body + 16 + 32] = 3;
  repair_trailing_crc(&mut invalid_operation);
  assert_eq!(
    decode_gc_state_artifact(&invalid_operation, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let lifecycle = fs::read(root.join("gc-artifact-v1/agca-blake3-256-root-lifecycle-manifest-populated.bin")).unwrap();
  let lifecycle_body = 32 + test_u16(&lifecycle, 16) as usize;
  let mut count_mismatch = lifecycle;
  count_mismatch[lifecycle_body + 76 + 3 * 32..lifecycle_body + 84 + 3 * 32].copy_from_slice(&2u64.to_le_bytes());
  repair_trailing_crc(&mut count_mismatch);
  assert_eq!(
    decode_gc_state_artifact(&count_mismatch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let inventory = fs::read(root.join("gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-empty.bin")).unwrap();
  let inventory_body = 32 + test_u16(&inventory, 16) as usize;
  let mut unknown_capability = inventory;
  unknown_capability[inventory_body + 4] ^= 1;
  repair_trailing_crc(&mut unknown_capability);
  assert_eq!(
    decode_gc_state_artifact(&unknown_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let quarantine = fs::read(root.join("gc-artifact-v1/agca-blake3-256-quarantine-manifest-empty.bin")).unwrap();
  let quarantine_body = 32 + test_u16(&quarantine, 16) as usize;
  let mut missing_lifecycle = quarantine;
  missing_lifecycle[quarantine_body + 52 + 5 * 32..quarantine_body + 52 + 6 * 32].fill(0);
  repair_trailing_crc(&mut missing_lifecycle);
  assert_eq!(
    decode_gc_state_artifact(&missing_lifecycle, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let retirement = fs::read(root.join("gc-artifact-v1/agca-blake3-256-root-retirement-commit-valid.bin")).unwrap();
  let retirement_body = 32 + test_u16(&retirement, 16) as usize;
  let mut bad_retirement_reserve = retirement;
  bad_retirement_reserve[retirement_body + 66 + 32] = 1;
  repair_trailing_crc(&mut bad_retirement_reserve);
  assert_eq!(
    decode_gc_state_artifact(&bad_retirement_reserve, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let proof = fs::read(root.join("gc-artifact-v1/agca-blake3-256-root-object-reclaim-proof-valid.bin")).unwrap();
  let proof_body = 32 + test_u16(&proof, 16) as usize;
  let mut zero_incarnations = proof;
  zero_incarnations[proof_body + 24 + 4 * 32..proof_body + 32 + 4 * 32].fill(0);
  repair_trailing_crc(&mut zero_incarnations);
  assert_eq!(
    decode_gc_state_artifact(&zero_incarnations, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn every_gc_mark_and_workspace_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| {
      row.format_id.starts_with("gc-mark-workspace")
        || row.format_id == "gc-artifact-v1"
          && (row.expected.starts_with("gc:checkpoint:mark-run:") || row.expected.starts_with("gc:journal:mark-mutation:"))
    })
    .collect();
  assert_eq!(rows.len(), 22);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    let (observed, key) = match row.format_id.as_str() {
      "gc-artifact-v1" => {
        let artifact = decode_gc_mark_artifact(&bytes, algorithm).unwrap();
        (artifact.summary(), Some(hex::encode(artifact.key())))
      }
      "gc-mark-workspace-manifest-v1" => (decode_mark_workspace_manifest(&bytes, algorithm).unwrap().summary(), None),
      "gc-mark-workspace-object-v1" => (decode_mark_workspace_object(&bytes, algorithm).unwrap().summary(), None),
      other => panic!("unexpected mark fixture format {other}"),
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
    assert_eq!(key, row.canonical_key, "fixture {}", row.id);
  }
}

#[test]
fn every_gc_mark_fixture_byte_is_integrity_protected() {
  let root = fixture_root();
  let rows: Vec<_> = manifest()
    .fixtures
    .into_iter()
    .filter(|row| {
      row.format_id.starts_with("gc-mark-workspace")
        || row.format_id == "gc-artifact-v1"
          && (row.expected.starts_with("gc:checkpoint:mark-run:") || row.expected.starts_with("gc:journal:mark-mutation:"))
    })
    .collect();

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    for offset in 0..bytes.len() {
      let mut changed = bytes.clone();
      changed[offset] ^= 1;
      let result = match row.format_id.as_str() {
        "gc-artifact-v1" => decode_gc_mark_artifact(&changed, algorithm).map(|_| ()),
        "gc-mark-workspace-manifest-v1" => decode_mark_workspace_manifest(&changed, algorithm).map(|_| ()),
        "gc-mark-workspace-object-v1" => decode_mark_workspace_object(&changed, algorithm).map(|_| ()),
        other => panic!("unexpected mark fixture format {other}"),
      };
      assert!(result.is_err(), "fixture {} accepted mutation at byte {offset}", row.id);
    }
  }
}

#[test]
fn gc_mark_workspace_rejects_semantic_corruption_and_wrong_closure() {
  let root = fixture_root();
  let checkpoint = fs::read(root.join("gc-artifact-v1/agca-blake3-256-mark-run-checkpoint-embedded.bin")).unwrap();
  let body = 32 + test_u16(&checkpoint, 16) as usize;
  let mut bad_state = checkpoint;
  bad_state[body + 6..body + 8].fill(0);
  repair_trailing_crc(&mut bad_state);
  assert_eq!(
    decode_gc_mark_artifact(&bad_state, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let journal = fs::read(root.join("gc-artifact-v1/agca-blake3-256-mark-mutation-journal-reset.bin")).unwrap();
  let body = 32 + test_u16(&journal, 16) as usize;
  let operation = body + 32 + 32 + 4 + 32 + 6 * 32;
  let mut bad_operation = journal;
  bad_operation[operation..operation + 2].copy_from_slice(&11u16.to_le_bytes());
  repair_trailing_crc(&mut bad_operation);
  assert_eq!(
    decode_gc_mark_artifact(&bad_operation, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let manifest = fs::read(root.join("gc-mark-workspace-manifest-v1/agcw-blake3-256-mark-workspace-manifest.bin")).unwrap();
  let mut bad_manifest_flags = manifest.clone();
  bad_manifest_flags[84] = 1;
  repair_trailing_crc(&mut bad_manifest_flags);
  assert_eq!(
    decode_mark_workspace_manifest(&bad_manifest_flags, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut unsafe_name = manifest.clone();
  let descriptor_name = 120 + 2 * 32 + 68;
  let slash = unsafe_name[descriptor_name..].iter().position(|byte| *byte == b'/').map(|offset| descriptor_name + offset).unwrap();
  unsafe_name[slash] = b'\\';
  repair_trailing_crc(&mut unsafe_name);
  assert_eq!(
    decode_mark_workspace_manifest(&unsafe_name, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let object = fs::read(root.join("gc-mark-workspace-object-v1/agwo-blake3-256-bitmap-valid.bin")).unwrap();
  let mut bad_unused_bits = object.clone();
  *bad_unused_bits.get_mut(114).unwrap() |= 0x80;
  repair_trailing_crc(&mut bad_unused_bits);
  assert_eq!(
    decode_mark_workspace_object(&bad_unused_bits, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let manifest = decode_mark_workspace_manifest(&manifest, HashAlgorithm::Blake3_256).unwrap();
  let object = decode_mark_workspace_object(&object, HashAlgorithm::Blake3_256).unwrap();
  validate_mark_workspace_object(
    &manifest,
    &manifest.descriptors[0],
    &object,
    &fs::read(root.join("gc-mark-workspace-object-v1/agwo-blake3-256-bitmap-valid.bin")).unwrap(),
  )
  .unwrap();
  assert_eq!(
    validate_mark_workspace_object(&manifest, &manifest.descriptors[1], &object, &[]).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  assert_eq!(
    decode_mark_workspace_manifest(&vec![0; 8 * 1024 * 1024 + 1], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  assert_eq!(
    decode_mark_workspace_object(&vec![0; 64 * 1024 * 1024 + 1], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let external_checkpoint = fs::read(root.join("gc-artifact-v1/agca-blake3-256-mark-run-checkpoint-external-canceled.bin")).unwrap();
  let GcMarkArtifactV1::Checkpoint(checkpoint) = decode_gc_mark_artifact(&external_checkpoint, HashAlgorithm::Blake3_256).unwrap() else {
    panic!("expected mark checkpoint");
  };
  assert_eq!(checkpoint.workspace_path, "C:/AeorDB/gc/31323334/51525354");
  assert!(checkpoint.canceled);

  let mut relative_checkpoint = external_checkpoint;
  let body = 72;
  let path = body + 236 + 4 * 32;
  relative_checkpoint[path] = b'x';
  repair_trailing_crc(&mut relative_checkpoint);
  assert_eq!(
    decode_gc_mark_artifact(&relative_checkpoint, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let embedded_checkpoint_bytes = fs::read(root.join("gc-artifact-v1/agca-blake3-256-mark-run-checkpoint-embedded.bin")).unwrap();
  let GcMarkArtifactV1::Checkpoint(embedded_checkpoint) =
    decode_gc_mark_artifact(&embedded_checkpoint_bytes, HashAlgorithm::Blake3_256).unwrap()
  else {
    panic!("expected mark checkpoint");
  };
  let manifest_bytes = fs::read(root.join("gc-mark-workspace-manifest-v1/agcw-blake3-256-mark-workspace-manifest.bin")).unwrap();
  let embedded_manifest = decode_mark_workspace_manifest(&manifest_bytes, HashAlgorithm::Blake3_256).unwrap();
  validate_mark_checkpoint_workspace(&embedded_checkpoint, &embedded_manifest, &manifest_bytes).unwrap();

  let mut detached_manifest_bytes = manifest_bytes;
  detached_manifest_bytes[32] ^= 1;
  repair_trailing_crc(&mut detached_manifest_bytes);
  let detached_manifest = decode_mark_workspace_manifest(&detached_manifest_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_mark_checkpoint_workspace(&embedded_checkpoint, &detached_manifest, &detached_manifest_bytes).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let previous_bytes = fs::read(root.join("gc-artifact-v1/agca-blake3-256-mark-mutation-journal-reset.bin")).unwrap();
  let GcMarkArtifactV1::MutationJournal(previous) = decode_gc_mark_artifact(&previous_bytes, HashAlgorithm::Blake3_256).unwrap() else {
    panic!("expected mark mutation journal");
  };
  let mut current_bytes = previous_bytes.clone();
  current_bytes[64..72].copy_from_slice(&2u64.to_le_bytes());
  let body = 72;
  current_bytes[body..body + 4].fill(0);
  current_bytes[body + 8..body + 16].copy_from_slice(&802u64.to_le_bytes());
  current_bytes[body + 16..body + 24].copy_from_slice(&803u64.to_le_bytes());
  current_bytes[body + 32..body + 64].copy_from_slice(&previous.key);
  let first_record = body + 64 + 4;
  current_bytes[first_record..first_record + 8].copy_from_slice(&802u64.to_le_bytes());
  let second_record = first_record + 36 + 6 * 32 + 4;
  current_bytes[second_record..second_record + 8].copy_from_slice(&803u64.to_le_bytes());
  repair_trailing_crc(&mut current_bytes);
  let GcMarkArtifactV1::MutationJournal(current) = decode_gc_mark_artifact(&current_bytes, HashAlgorithm::Blake3_256).unwrap() else {
    panic!("expected mark mutation journal");
  };
  validate_mark_mutation_journal_chain(&previous, &current).unwrap();

  let mut wrong_predecessor_bytes = current_bytes;
  wrong_predecessor_bytes[body + 32] ^= 1;
  repair_trailing_crc(&mut wrong_predecessor_bytes);
  let GcMarkArtifactV1::MutationJournal(wrong_predecessor) =
    decode_gc_mark_artifact(&wrong_predecessor_bytes, HashAlgorithm::Blake3_256).unwrap()
  else {
    panic!("expected mark mutation journal");
  };
  assert_eq!(
    validate_mark_mutation_journal_chain(&previous, &wrong_predecessor).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn every_sweep_and_void_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(is_sweep_void_fixture).collect();
  assert_eq!(rows.len(), 28);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let artifact = decode_sweep_void_artifact(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    assert_eq!(artifact.summary(), row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(artifact.key()), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn sweep_and_void_closure_rejects_detached_or_corrupt_authority() {
  let root = fixture_root();
  let read = |name: &str| fs::read(root.join(format!("gc-artifact-v1/agca-blake3-256-{name}.bin"))).unwrap();

  let proposal = read("sweep-proposal");
  let receipt = read("sweep-commit-receipt");
  let recovered = read("sweep-recovered-receipt");
  let source_page = read("void-extent-page-source");
  let source_directory = read("void-free-directory-source");
  let source_manifest = read("void-catalog-source");
  let claim = read("void-claim");
  let claim_directory = read("void-claims-directory");
  let outstanding_manifest = read("void-catalog-outstanding");
  let remaining_page = read("void-extent-page-remaining");
  let remaining_directory = read("void-free-directory-remaining");
  let settled_manifest = read("void-catalog-settled");
  let settlement = read("void-claim-settlement");

  let proposal = decode_sweep_void_artifact(&proposal, HashAlgorithm::Blake3_256).unwrap();
  let receipt = decode_sweep_void_artifact(&receipt, HashAlgorithm::Blake3_256).unwrap();
  let recovered = decode_sweep_void_artifact(&recovered, HashAlgorithm::Blake3_256).unwrap();
  let source_page = decode_sweep_void_artifact(&source_page, HashAlgorithm::Blake3_256).unwrap();
  let source_directory = decode_sweep_void_artifact(&source_directory, HashAlgorithm::Blake3_256).unwrap();
  let source_manifest = decode_sweep_void_artifact(&source_manifest, HashAlgorithm::Blake3_256).unwrap();
  let claim = decode_sweep_void_artifact(&claim, HashAlgorithm::Blake3_256).unwrap();
  let claim_directory = decode_sweep_void_artifact(&claim_directory, HashAlgorithm::Blake3_256).unwrap();
  let outstanding_manifest = decode_sweep_void_artifact(&outstanding_manifest, HashAlgorithm::Blake3_256).unwrap();
  let remaining_page = decode_sweep_void_artifact(&remaining_page, HashAlgorithm::Blake3_256).unwrap();
  let remaining_directory = decode_sweep_void_artifact(&remaining_directory, HashAlgorithm::Blake3_256).unwrap();
  let settled_manifest = decode_sweep_void_artifact(&settled_manifest, HashAlgorithm::Blake3_256).unwrap();
  let settlement = decode_sweep_void_artifact(&settlement, HashAlgorithm::Blake3_256).unwrap();

  validate_void_directory_child(&source_directory, &source_page).unwrap();
  validate_void_manifest_root(&source_manifest, &source_directory).unwrap();
  validate_void_claim_source(&claim, &source_manifest, &source_page).unwrap();
  validate_void_directory_child(&claim_directory, &claim).unwrap();
  validate_void_directory_child(&remaining_directory, &remaining_page).unwrap();
  validate_void_manifest_root(&outstanding_manifest, &remaining_directory).unwrap();
  validate_void_manifest_root(&outstanding_manifest, &claim_directory).unwrap();
  validate_sweep_receipt_closure(&proposal, &receipt, &outstanding_manifest).unwrap();
  validate_sweep_receipt_closure(&proposal, &recovered, &outstanding_manifest).unwrap();
  validate_void_settlement_closure(&settlement, &claim, &outstanding_manifest, &settled_manifest).unwrap();

  assert_eq!(
    validate_void_manifest_root(&settled_manifest, &source_directory).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    validate_sweep_receipt_closure(&proposal, &receipt, &settled_manifest).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut detached_directory_bytes = read("void-free-directory-source");
  let body = gc_artifact_body_offset(&detached_directory_bytes);
  let lower_length = test_u32(&detached_directory_bytes, body + 16) as usize;
  let upper_length = test_u32(&detached_directory_bytes, body + 20) as usize;
  detached_directory_bytes[body + 80 + lower_length + upper_length + 16] ^= 1;
  repair_trailing_crc(&mut detached_directory_bytes);
  let detached_directory = decode_sweep_void_artifact(&detached_directory_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_void_directory_child(&detached_directory, &source_page).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let SweepVoidArtifactV1::VoidExtentPage(page) = source_page else {
    panic!("expected Void extent page");
  };
  assert_eq!(page.total_bytes, 8_195);
}

#[test]
fn every_sweep_and_void_fixture_byte_is_integrity_protected() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(is_sweep_void_fixture).collect();
  assert_eq!(rows.len(), 28);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    for offset in 0..bytes.len() {
      let mut changed = bytes.clone();
      changed[offset] ^= 1;
      assert!(decode_sweep_void_artifact(&changed, algorithm).is_err(), "fixture {} accepted mutation at byte {offset}", row.id);
    }
  }
}

#[test]
fn sweep_and_void_readers_reject_semantic_corruption_and_amplification() {
  let root = fixture_root();
  let read = |name: &str| fs::read(root.join(format!("gc-artifact-v1/agca-blake3-256-{name}.bin"))).unwrap();
  let hash_width = HashAlgorithm::Blake3_256.hash_length();

  let proposal = read("sweep-proposal");
  let body = gc_artifact_body_offset(&proposal);
  let mut wrong_digest = proposal.clone();
  wrong_digest[body + 32 + hash_width] ^= 1;
  repair_trailing_crc(&mut wrong_digest);
  assert_eq!(
    decode_sweep_void_artifact(&wrong_digest, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut amplified_proposal = proposal.clone();
  amplified_proposal[body + 24 + hash_width..body + 28 + hash_width].copy_from_slice(&4_097u32.to_le_bytes());
  repair_trailing_crc(&mut amplified_proposal);
  assert_eq!(
    decode_sweep_void_artifact(&amplified_proposal, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let record_length = 24 + 2 * hash_width;
  let records = body + 32 + 2 * hash_width;
  let mut duplicate_candidate = proposal;
  duplicate_candidate.copy_within(records..records + record_length, records + record_length);
  repair_blake3_sweep_proposal_digest(&mut duplicate_candidate);
  repair_trailing_crc(&mut duplicate_candidate);
  assert_eq!(
    decode_sweep_void_artifact(&duplicate_candidate, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let mut numeric_order = read("sweep-proposal");
  let body = gc_artifact_body_offset(&numeric_order);
  let records = body + 32 + 2 * hash_width;
  numeric_order.copy_within(records..records + 2 * hash_width, records + record_length);
  numeric_order[records + 2 * hash_width..records + 2 * hash_width + 8].copy_from_slice(&255u64.to_le_bytes());
  numeric_order[records + record_length + 2 * hash_width..records + record_length + 2 * hash_width + 8]
    .copy_from_slice(&256u64.to_le_bytes());
  repair_blake3_sweep_proposal_digest(&mut numeric_order);
  repair_trailing_crc(&mut numeric_order);
  decode_sweep_void_artifact(&numeric_order, HashAlgorithm::Blake3_256).unwrap();

  numeric_order[records + 2 * hash_width..records + 2 * hash_width + 8].copy_from_slice(&256u64.to_le_bytes());
  numeric_order[records + record_length + 2 * hash_width..records + record_length + 2 * hash_width + 8]
    .copy_from_slice(&255u64.to_le_bytes());
  repair_blake3_sweep_proposal_digest(&mut numeric_order);
  repair_trailing_crc(&mut numeric_order);
  assert_eq!(
    decode_sweep_void_artifact(&numeric_order, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let receipt = read("sweep-commit-receipt");
  let body = gc_artifact_body_offset(&receipt);
  let mut wrong_totals = receipt.clone();
  wrong_totals[body + 32 + 2 * hash_width..body + 40 + 2 * hash_width].copy_from_slice(&2u64.to_le_bytes());
  repair_trailing_crc(&mut wrong_totals);
  assert_eq!(
    decode_sweep_void_artifact(&wrong_totals, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut unknown_outcome = receipt;
  let first_outcome = body + 64 + 2 * hash_width + 24 + 2 * hash_width;
  unknown_outcome[first_outcome..first_outcome + 2].copy_from_slice(&9u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_outcome);
  assert_eq!(
    decode_sweep_void_artifact(&unknown_outcome, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let page = read("void-extent-page-source");
  let body = gc_artifact_body_offset(&page);
  let row_length = 32 + 3 * hash_width;
  let first_offset = test_u64(&page, body + 80);
  let mut overlap = page.clone();
  overlap[body + 80 + row_length..body + 88 + row_length].copy_from_slice(&(first_offset + 1).to_le_bytes());
  repair_trailing_crc(&mut overlap);
  assert_eq!(
    decode_sweep_void_artifact(&overlap, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let mut reserved_extent = page;
  reserved_extent[body + 92..body + 96].copy_from_slice(&1u32.to_le_bytes());
  repair_trailing_crc(&mut reserved_extent);
  assert_eq!(
    decode_sweep_void_artifact(&reserved_extent, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let catalog = read("void-catalog-source");
  let body = gc_artifact_body_offset(&catalog);
  let mut unknown_capability = catalog.clone();
  unknown_capability[body + 4] ^= 1;
  repair_trailing_crc(&mut unknown_capability);
  assert_eq!(
    decode_sweep_void_artifact(&unknown_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownRequiredCapability
  );

  let mut count_root_mismatch = catalog;
  count_root_mismatch[body + 52 + 2 * hash_width..body + 60 + 2 * hash_width].fill(0);
  repair_trailing_crc(&mut count_root_mismatch);
  assert_eq!(
    decode_sweep_void_artifact(&count_root_mismatch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let claim = read("void-claim");
  let body = gc_artifact_body_offset(&claim);
  let mut amplified_claim = claim.clone();
  amplified_claim[body + 48 + hash_width..body + 52 + hash_width].copy_from_slice(&4_097u32.to_le_bytes());
  repair_trailing_crc(&mut amplified_claim);
  assert_eq!(
    decode_sweep_void_artifact(&amplified_claim, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut zero_claim_source = claim.clone();
  zero_claim_source[body + 48..body + 48 + hash_width].fill(0);
  repair_trailing_crc(&mut zero_claim_source);
  assert_eq!(
    decode_sweep_void_artifact(&zero_claim_source, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let settlement = read("void-claim-settlement");
  let body = gc_artifact_body_offset(&settlement);
  let mut invalid_settlement = settlement;
  invalid_settlement[body + 4..body + 6].copy_from_slice(&2u16.to_le_bytes());
  repair_trailing_crc(&mut invalid_settlement);
  assert_eq!(
    decode_sweep_void_artifact(&invalid_settlement, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let settlement = read("void-claim-settlement");
  let body = gc_artifact_body_offset(&settlement);
  let mut count_byte_mismatch = settlement;
  count_byte_mismatch[body + 16 + 2 * hash_width..body + 20 + 2 * hash_width].fill(0);
  repair_trailing_crc(&mut count_byte_mismatch);
  assert_eq!(
    decode_sweep_void_artifact(&count_byte_mismatch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  for (kind, cap) in [
    (GcArtifactKindV1::VoidCatalogManifest, 1024 * 1024),
    (GcArtifactKindV1::GcArtifactDirectoryNode, 4 * 1024 * 1024),
    (GcArtifactKindV1::VoidExtentPage, 16 * 1024 * 1024),
    (GcArtifactKindV1::SweepProposal, 16 * 1024 * 1024),
  ] {
    let mut over_cap = vec![0; cap + 1];
    over_cap[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    assert_eq!(
      decode_sweep_void_artifact(&over_cap, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::AllocationAmplification,
      "kind {}",
      kind.name()
    );
  }
}

#[test]
fn void_claim_and_settlement_closure_reconcile_generations_counts_and_bytes() {
  let root = fixture_root();
  let read = |name: &str| fs::read(root.join(format!("gc-artifact-v1/agca-blake3-256-{name}.bin"))).unwrap();
  let source_page_bytes = read("void-extent-page-source");
  let source_manifest_bytes = read("void-catalog-source");
  let claim_bytes = read("void-claim");
  let outstanding_bytes = read("void-catalog-outstanding");
  let settled_bytes = read("void-catalog-settled");
  let settlement_bytes = read("void-claim-settlement");

  let source_page = decode_sweep_void_artifact(&source_page_bytes, HashAlgorithm::Blake3_256).unwrap();
  let source_manifest = decode_sweep_void_artifact(&source_manifest_bytes, HashAlgorithm::Blake3_256).unwrap();
  let claim = decode_sweep_void_artifact(&claim_bytes, HashAlgorithm::Blake3_256).unwrap();
  let outstanding = decode_sweep_void_artifact(&outstanding_bytes, HashAlgorithm::Blake3_256).unwrap();
  let settled = decode_sweep_void_artifact(&settled_bytes, HashAlgorithm::Blake3_256).unwrap();
  let settlement = decode_sweep_void_artifact(&settlement_bytes, HashAlgorithm::Blake3_256).unwrap();
  validate_void_claim_source(&claim, &source_manifest, &source_page).unwrap();
  validate_void_settlement_closure(&settlement, &claim, &outstanding, &settled).unwrap();

  let mut wrong_claim_generation = claim_bytes.clone();
  wrong_claim_generation[24..32].copy_from_slice(&1u64.to_le_bytes());
  repair_trailing_crc(&mut wrong_claim_generation);
  let wrong_claim_generation = decode_sweep_void_artifact(&wrong_claim_generation, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_void_claim_source(&wrong_claim_generation, &source_manifest, &source_page).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut detached_claim_bytes = claim_bytes.clone();
  let claim_body = gc_artifact_body_offset(&detached_claim_bytes);
  detached_claim_bytes[claim_body + 56 + 32..claim_body + 64 + 32].copy_from_slice(&999_999u64.to_le_bytes());
  repair_trailing_crc(&mut detached_claim_bytes);
  let detached_claim = decode_sweep_void_artifact(&detached_claim_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_void_claim_source(&detached_claim, &source_manifest, &source_page).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut wrong_result_bytes = settled_bytes;
  let result_body = gc_artifact_body_offset(&wrong_result_bytes);
  let free_bytes = test_u64(&wrong_result_bytes, result_body + 60 + 2 * 32);
  wrong_result_bytes[result_body + 60 + 2 * 32..result_body + 68 + 2 * 32].copy_from_slice(&(free_bytes + 1).to_le_bytes());
  repair_trailing_crc(&mut wrong_result_bytes);
  let wrong_result_key = immutable_gc_artifact_key(HashAlgorithm::Blake3_256, GcArtifactKindV1::VoidCatalogManifest, &wrong_result_bytes);
  let wrong_result = decode_sweep_void_artifact(&wrong_result_bytes, HashAlgorithm::Blake3_256).unwrap();

  let mut wrong_settlement_bytes = settlement_bytes;
  let body = gc_artifact_body_offset(&wrong_settlement_bytes);
  wrong_settlement_bytes[body + 16 + 32..body + 16 + 2 * 32].copy_from_slice(&wrong_result_key);
  repair_trailing_crc(&mut wrong_settlement_bytes);
  let wrong_settlement = decode_sweep_void_artifact(&wrong_settlement_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_void_settlement_closure(&wrong_settlement, &claim, &outstanding, &wrong_result).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn every_gc_audit_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(is_gc_audit_fixture).collect();
  assert_eq!(rows.len(), 18);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let artifact = decode_audit_artifact(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    assert_eq!(artifact.summary(), row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(artifact.key()), row.canonical_key.unwrap(), "fixture {}", row.id);
  }
}

#[test]
fn gc_audit_closure_binds_catalog_pages_run_summary_and_pins() {
  let root = fixture_root();
  let read = |name: &str| fs::read(root.join(format!("gc-artifact-v1/agca-blake3-256-{name}.bin"))).unwrap();

  let manifest_bytes = read("audit-catalog-populated");
  let detail_page_bytes = read("audit-detail-page");
  let detail_directory_bytes = read("audit-detail-directory");
  let summary_page_bytes = read("audit-summary-page");
  let summary_directory_bytes = read("audit-summary-directory");
  let run_summary_bytes = read("gc-run-summary");
  let corrupt_bytes = read("corrupt-gc-evidence");
  let pin_bytes = read("audit-pin");

  let manifest = decode_audit_artifact(&manifest_bytes, HashAlgorithm::Blake3_256).unwrap();
  let detail_page = decode_audit_artifact(&detail_page_bytes, HashAlgorithm::Blake3_256).unwrap();
  let detail_directory = decode_audit_artifact(&detail_directory_bytes, HashAlgorithm::Blake3_256).unwrap();
  let summary_page = decode_audit_artifact(&summary_page_bytes, HashAlgorithm::Blake3_256).unwrap();
  let summary_directory = decode_audit_artifact(&summary_directory_bytes, HashAlgorithm::Blake3_256).unwrap();
  let run_summary = decode_audit_artifact(&run_summary_bytes, HashAlgorithm::Blake3_256).unwrap();
  let corrupt = decode_audit_artifact(&corrupt_bytes, HashAlgorithm::Blake3_256).unwrap();
  let pin = decode_audit_artifact(&pin_bytes, HashAlgorithm::Blake3_256).unwrap();

  validate_audit_directory_child(&detail_directory, &detail_page).unwrap();
  validate_audit_directory_child(&summary_directory, &summary_page).unwrap();
  validate_audit_manifest_directory(&manifest, &detail_directory).unwrap();
  validate_audit_manifest_directory(&manifest, &summary_directory).unwrap();
  validate_run_summary_page_record(&run_summary, &summary_page).unwrap();
  validate_audit_manifest_pin(&manifest, &pin).unwrap();
  let database_id = match &pin {
    AuditArtifactV1::Pin(pin) => pin.database_id,
    _ => panic!("expected audit pin"),
  };
  validate_audit_pin_target(&pin, database_id, GcArtifactKindV1::GcRunSummary, run_summary.key()).unwrap();
  validate_audit_pin_target(&pin, database_id, GcArtifactKindV1::CorruptGcEvidence, corrupt.key()).unwrap();

  assert_eq!(
    validate_audit_pin_target(&pin, database_id, GcArtifactKindV1::AuditCatalogActiveControl, corrupt.key()).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let AuditArtifactV1::Page(page) = summary_page else {
    panic!("expected audit summary page");
  };
  assert_eq!(page.record_count, 2);
}

#[test]
fn every_gc_audit_fixture_byte_is_integrity_protected() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(is_gc_audit_fixture).collect();
  assert_eq!(rows.len(), 18);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    for offset in 0..bytes.len() {
      let mut changed = bytes.clone();
      changed[offset] ^= 1;
      assert!(decode_audit_artifact(&changed, algorithm).is_err(), "fixture {} accepted mutation at byte {offset}", row.id);
    }
  }
}

#[test]
fn gc_audit_readers_reject_semantic_corruption_and_amplification() {
  let root = fixture_root();
  let read = |name: &str| fs::read(root.join(format!("gc-artifact-v1/agca-blake3-256-{name}.bin"))).unwrap();
  let hash_width = HashAlgorithm::Blake3_256.hash_length();

  let detail_page = read("audit-detail-page");
  let body = gc_artifact_body_offset(&detail_page);
  let lower_length = test_u32(&detail_page, body + 8) as usize;
  let upper_length = test_u32(&detail_page, body + 12) as usize;
  let first_detail = body + 64 + lower_length + upper_length;

  let mut unknown_event = detail_page.clone();
  unknown_event[first_detail + hash_width..first_detail + hash_width + 2].copy_from_slice(&99u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_event);
  assert_eq!(
    decode_audit_artifact(&unknown_event, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut invalid_payload = detail_page.clone();
  invalid_payload[first_detail + 52 + hash_width] = 0xff;
  repair_trailing_crc(&mut invalid_payload);
  assert_eq!(
    decode_audit_artifact(&invalid_payload, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut invalid_batch = detail_page.clone();
  invalid_batch[first_detail + hash_width + 28] ^= 1;
  repair_trailing_crc(&mut invalid_batch);
  assert_eq!(
    decode_audit_artifact(&invalid_batch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut reserved_detail = detail_page.clone();
  reserved_detail[first_detail + hash_width + 48] = 1;
  repair_trailing_crc(&mut reserved_detail);
  assert_eq!(
    decode_audit_artifact(&reserved_detail, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut amplified_page = detail_page.clone();
  amplified_page[body + 16..body + 20].copy_from_slice(&u32::MAX.to_le_bytes());
  amplified_page[body + 20..body + 24].copy_from_slice(&u32::MAX.to_le_bytes());
  repair_trailing_crc(&mut amplified_page);
  assert_eq!(
    decode_audit_artifact(&amplified_page, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut reserved_page = detail_page;
  reserved_page[body + 40] = 1;
  repair_trailing_crc(&mut reserved_page);
  assert_eq!(
    decode_audit_artifact(&reserved_page, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let summary_page = read("audit-summary-page");
  let body = gc_artifact_body_offset(&summary_page);
  let lower_length = test_u32(&summary_page, body + 8) as usize;
  let upper_length = test_u32(&summary_page, body + 12) as usize;
  let first_summary = body + 64 + lower_length + upper_length;
  let started_at = test_u64(&summary_page, first_summary + 16);
  let mut reversed_time = summary_page;
  reversed_time[first_summary + 24..first_summary + 32].copy_from_slice(&(started_at - 1).to_le_bytes());
  repair_trailing_crc(&mut reversed_time);
  assert_eq!(
    decode_audit_artifact(&reversed_time, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let record_length = 76 + hash_width;
  let mut duplicate_summary = read("audit-summary-page");
  duplicate_summary.copy_within(first_summary..first_summary + record_length, first_summary + record_length);
  repair_trailing_crc(&mut duplicate_summary);
  assert_eq!(
    decode_audit_artifact(&duplicate_summary, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let directory = read("audit-detail-directory");
  let mut unknown_role = directory;
  unknown_role[64..66].copy_from_slice(&99u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_role);
  assert_eq!(
    decode_audit_artifact(&unknown_role, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut reserved_directory = read("audit-detail-directory");
  let body = gc_artifact_body_offset(&reserved_directory);
  reserved_directory[body + 8] = 1;
  repair_trailing_crc(&mut reserved_directory);
  assert_eq!(
    decode_audit_artifact(&reserved_directory, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let manifest = read("audit-catalog-populated");
  let body = gc_artifact_body_offset(&manifest);
  let mut unknown_capability = manifest.clone();
  unknown_capability[body + 35] ^= 0x80;
  repair_trailing_crc(&mut unknown_capability);
  assert_eq!(
    decode_audit_artifact(&unknown_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownRequiredCapability
  );

  let mut amplified_pins = manifest;
  amplified_pins[body + 140 + 2 * hash_width..body + 144 + 2 * hash_width].copy_from_slice(&4_097u32.to_le_bytes());
  repair_trailing_crc(&mut amplified_pins);
  assert_eq!(
    decode_audit_artifact(&amplified_pins, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut mismatched_pin_length = read("audit-catalog-populated");
  let body = gc_artifact_body_offset(&mismatched_pin_length);
  mismatched_pin_length[body + 144 + 2 * hash_width..body + 148 + 2 * hash_width].fill(0);
  repair_trailing_crc(&mut mismatched_pin_length);
  assert_eq!(
    decode_audit_artifact(&mismatched_pin_length, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );

  let evidence = read("corrupt-gc-evidence");
  let body = gc_artifact_body_offset(&evidence);
  let mut invalid_optionals = evidence.clone();
  invalid_optionals[body + 11] = 0;
  repair_trailing_crc(&mut invalid_optionals);
  assert_eq!(
    decode_audit_artifact(&invalid_optionals, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut unknown_observed_kind = evidence.clone();
  unknown_observed_kind[body + 12..body + 14].copy_from_slice(&0xffffu16.to_le_bytes());
  repair_trailing_crc(&mut unknown_observed_kind);
  assert_eq!(
    decode_audit_artifact(&unknown_observed_kind, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut reserved_evidence = evidence.clone();
  reserved_evidence[body + 14] = 1;
  repair_trailing_crc(&mut reserved_evidence);
  assert_eq!(
    decode_audit_artifact(&reserved_evidence, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut amplified_evidence = evidence.clone();
  amplified_evidence[body + 64 + 3 * hash_width..body + 66 + 3 * hash_width].copy_from_slice(&65u16.to_le_bytes());
  repair_trailing_crc(&mut amplified_evidence);
  assert_eq!(
    decode_audit_artifact(&amplified_evidence, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let context_length = test_u32(&evidence, body + 60 + 3 * hash_width) as usize;
  let context = body + 68 + 3 * hash_width;
  let mut invalid_context = evidence.clone();
  invalid_context[context] = 0xff;
  repair_trailing_crc(&mut invalid_context);
  assert_eq!(
    decode_audit_artifact(&invalid_context, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let hashes = context + context_length;
  let mut duplicate_evidence = evidence;
  duplicate_evidence.copy_within(hashes..hashes + hash_width, hashes + hash_width);
  repair_trailing_crc(&mut duplicate_evidence);
  assert_eq!(
    decode_audit_artifact(&duplicate_evidence, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let pin = read("audit-pin");
  let body = gc_artifact_body_offset(&pin);
  let mut zero_pin_count = pin.clone();
  zero_pin_count[body + 20 + hash_width..body + 24 + hash_width].fill(0);
  repair_trailing_crc(&mut zero_pin_count);
  assert_eq!(
    decode_audit_artifact(&zero_pin_count, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut unknown_reason = pin.clone();
  unknown_reason[body + 16 + hash_width..body + 18 + hash_width].copy_from_slice(&99u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_reason);
  assert_eq!(
    decode_audit_artifact(&unknown_reason, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut reserved_pin = pin.clone();
  reserved_pin[body + 18 + hash_width] = 1;
  repair_trailing_crc(&mut reserved_pin);
  assert_eq!(
    decode_audit_artifact(&reserved_pin, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let hashes = body + 32 + hash_width;
  let mut duplicate_pin = pin;
  duplicate_pin.copy_within(hashes..hashes + hash_width, hashes + hash_width);
  repair_trailing_crc(&mut duplicate_pin);
  assert_eq!(
    decode_audit_artifact(&duplicate_pin, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  for (kind, cap) in [
    (GcArtifactKindV1::AuditCatalogManifest, 1024 * 1024),
    (GcArtifactKindV1::GcArtifactDirectoryNode, 4 * 1024 * 1024),
    (GcArtifactKindV1::AuditDetailPage, 16 * 1024 * 1024),
  ] {
    let mut over_cap = vec![0; cap + 1];
    over_cap[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    assert_eq!(
      decode_audit_artifact(&over_cap, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::AllocationAmplification,
      "kind {}",
      kind.name()
    );
  }
}

#[test]
fn gc_audit_closure_rejects_detached_and_cross_database_artifacts() {
  let root = fixture_root();
  let read = |name: &str| fs::read(root.join(format!("gc-artifact-v1/agca-blake3-256-{name}.bin"))).unwrap();
  let hash_width = HashAlgorithm::Blake3_256.hash_length();

  let manifest_bytes = read("audit-catalog-populated");
  let detail_page_bytes = read("audit-detail-page");
  let detail_directory_bytes = read("audit-detail-directory");
  let summary_page_bytes = read("audit-summary-page");
  let run_summary_bytes = read("gc-run-summary");
  let pin_bytes = read("audit-pin");

  let manifest = decode_audit_artifact(&manifest_bytes, HashAlgorithm::Blake3_256).unwrap();
  let detail_page = decode_audit_artifact(&detail_page_bytes, HashAlgorithm::Blake3_256).unwrap();
  let detail_directory = decode_audit_artifact(&detail_directory_bytes, HashAlgorithm::Blake3_256).unwrap();
  let summary_page = decode_audit_artifact(&summary_page_bytes, HashAlgorithm::Blake3_256).unwrap();
  let run_summary = decode_audit_artifact(&run_summary_bytes, HashAlgorithm::Blake3_256).unwrap();
  let pin = decode_audit_artifact(&pin_bytes, HashAlgorithm::Blake3_256).unwrap();
  let database_id = match &pin {
    AuditArtifactV1::Pin(pin) => pin.database_id,
    _ => panic!("expected audit pin"),
  };

  let mut detached_directory_bytes = detail_directory_bytes.clone();
  let body = gc_artifact_body_offset(&detached_directory_bytes);
  let lower_length = test_u32(&detached_directory_bytes, body + 16) as usize;
  let upper_length = test_u32(&detached_directory_bytes, body + 20) as usize;
  detached_directory_bytes[body + 80 + lower_length + upper_length + 16] ^= 1;
  repair_trailing_crc(&mut detached_directory_bytes);
  let detached_directory = decode_audit_artifact(&detached_directory_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_audit_directory_child(&detached_directory, &detail_page).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut detached_manifest_bytes = manifest_bytes.clone();
  let body = gc_artifact_body_offset(&detached_manifest_bytes);
  detached_manifest_bytes[body + 44] ^= 1;
  repair_trailing_crc(&mut detached_manifest_bytes);
  let detached_manifest = decode_audit_artifact(&detached_manifest_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_audit_manifest_directory(&detached_manifest, &detail_directory).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut detached_run_bytes = run_summary_bytes.clone();
  let body = gc_artifact_body_offset(&detached_run_bytes);
  let reclaimed_bytes = test_u64(&detached_run_bytes, body + 68);
  detached_run_bytes[body + 68..body + 76].copy_from_slice(&(reclaimed_bytes + 1).to_le_bytes());
  repair_trailing_crc(&mut detached_run_bytes);
  let detached_run = decode_audit_artifact(&detached_run_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(
    validate_run_summary_page_record(&detached_run, &summary_page).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut unrooted_pin_bytes = pin_bytes.clone();
  let body = gc_artifact_body_offset(&unrooted_pin_bytes);
  unrooted_pin_bytes[body + 16] ^= 1;
  repair_trailing_crc(&mut unrooted_pin_bytes);
  let unrooted_pin = decode_audit_artifact(&unrooted_pin_bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(validate_audit_manifest_pin(&manifest, &unrooted_pin).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut wrong_database_id = database_id.to_vec();
  wrong_database_id[0] ^= 1;
  assert_eq!(
    validate_audit_pin_target(&pin, &wrong_database_id, GcArtifactKindV1::GcRunSummary, run_summary.key()).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    validate_audit_pin_target(&pin, database_id, GcArtifactKindV1::GcRunSummary, &vec![0; hash_width]).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn every_system_control_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(is_system_control_fixture).collect();
  assert_eq!(rows.len(), 42);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    if row.format_id == "system-control-v1" {
      let control = decode_system_control(&bytes, algorithm).unwrap();
      assert_eq!(control.summary(), row.expected, "fixture {}", row.id);
      assert_eq!(control.canonical_path(), row.canonical_key.as_deref().unwrap(), "fixture {}", row.id);
    } else {
      let selected = select_cutover_journal(&bytes, algorithm).unwrap();
      assert_eq!(selected.summary(), row.expected, "fixture {}", row.id);
      assert!(row.canonical_key.is_none());
    }
  }
}

#[test]
fn every_system_family_registry_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let independent: serde_json::Value =
    serde_json::from_slice(&fs::read(root.parent().unwrap().join("system-family-registry-v1.manifest.json")).unwrap()).unwrap();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "system-family-registry-v1").collect();
  assert_eq!(rows.len(), 2);
  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let registry = decode_system_family_registry(&bytes, hash_algorithm(&row.hash_algorithm)).unwrap();
    assert_eq!(registry.summary(), row.expected, "fixture {}", row.id);
    assert_eq!(hex::encode(&registry.operational_fingerprint), row.canonical_key.as_deref().unwrap(), "fixture {}", row.id);
    let descriptors: Vec<_> = registry.iter().collect::<Result<_, _>>().unwrap();
    assert_eq!(descriptors.len(), 63);
    assert_eq!(descriptors.iter().map(|descriptor| descriptor.family_id).collect::<std::collections::BTreeSet<_>>().len(), 46);
    let fingerprint_name = if row.hash_algorithm == "blake3-256" { "blake3_256" } else { "sha512" };
    assert_eq!(
      hex::encode(registry.semantic_projection_fingerprint),
      independent["semantic_projection_fingerprints"][fingerprint_name].as_str().unwrap()
    );
  }
}

#[test]
fn every_system_family_registry_byte_is_integrity_protected() {
  let root = fixture_root();
  for row in manifest().fixtures.into_iter().filter(|row| row.format_id == "system-family-registry-v1") {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    for offset in 0..bytes.len() {
      let mut changed = bytes.clone();
      changed[offset] ^= 1;
      assert!(decode_system_family_registry(&changed, algorithm).is_err(), "fixture {} accepted mutation at byte {offset}", row.id);
    }
  }
}

#[test]
fn system_family_registry_rejects_bounds_reserves_enums_paths_order_and_policy_drift() {
  let root = fixture_root();
  let baseline = fs::read(root.join("system-family-registry-v1/asfr-blake3-256-registry-v1-valid.bin")).unwrap();

  let mut amplified = baseline.clone();
  amplified[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
  repair_trailing_crc(&mut amplified);
  assert_eq!(
    decode_system_family_registry(&amplified, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut reserved = baseline.clone();
  reserved[20] = 1;
  repair_trailing_crc(&mut reserved);
  assert_eq!(
    decode_system_family_registry(&reserved, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut unknown_domain = baseline.clone();
  unknown_domain[34] = 0xff;
  repair_trailing_crc(&mut unknown_domain);
  assert_eq!(
    decode_system_family_registry(&unknown_domain, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut incompatible = baseline.clone();
  incompatible[34] = 2;
  repair_trailing_crc(&mut incompatible);
  assert_eq!(
    decode_system_family_registry(&incompatible, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut unknown_policy = baseline.clone();
  unknown_policy[36] = 0xff;
  repair_trailing_crc(&mut unknown_policy);
  assert_eq!(
    decode_system_family_registry(&unknown_policy, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut invalid_path = baseline.clone();
  invalid_path[64] = b'x';
  repair_trailing_crc(&mut invalid_path);
  assert_eq!(
    decode_system_family_registry(&invalid_path, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let descriptor_offsets = system_family_descriptor_offsets(&baseline);
  assert_eq!(descriptor_offsets.len(), 63);
  let mut out_of_order = baseline.clone();
  let last = *descriptor_offsets.last().unwrap();
  out_of_order[last..last + 2].copy_from_slice(&1u16.to_le_bytes());
  repair_trailing_crc(&mut out_of_order);
  assert_eq!(
    decode_system_family_registry(&out_of_order, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let mut policy_drift = baseline.clone();
  let second = descriptor_offsets[1];
  policy_drift[second..second + 2].copy_from_slice(&1u16.to_le_bytes());
  repair_trailing_crc(&mut policy_drift);
  assert_eq!(
    decode_system_family_registry(&policy_drift, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let oversized = vec![0u8; 1_048_577];
  assert_eq!(
    decode_system_family_registry(&oversized, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn system_control_integrity_covers_every_byte_and_journal_slot() {
  let root = fixture_root();
  for row in manifest().fixtures.into_iter().filter(is_system_control_fixture) {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let algorithm = hash_algorithm(&row.hash_algorithm);
    for offset in 0..bytes.len() {
      let mut changed = bytes.clone();
      changed[offset] ^= 1;
      if row.format_id == "system-control-v1" {
        assert!(decode_system_control(&changed, algorithm).is_err(), "fixture {} accepted mutation at byte {offset}", row.id);
      } else {
        let selected = select_cutover_journal(&changed, algorithm)
          .unwrap_or_else(|error| panic!("fixture {} lost both journal slots after one-byte mutation at {offset}: {error}", row.id));
        assert!(selected.redundancy_degraded, "fixture {} did not report degraded redundancy at byte {offset}", row.id);
      }
    }
  }
}

#[test]
fn every_system_control_kind_rejects_repaired_crc_zero_database_identity() {
  let root = fixture_root();
  let rows: Vec<_> =
    manifest().fixtures.into_iter().filter(|row| row.format_id == "system-control-v1" && row.hash_algorithm == "blake3-256").collect();
  assert_eq!(rows.len(), SystemControlKindV1::ALL.len());
  for row in rows {
    let mut bytes = fs::read(root.join(row.binary)).unwrap();
    bytes[32..48].fill(0);
    repair_trailing_crc(&mut bytes);
    assert_eq!(
      decode_system_control(&bytes, HashAlgorithm::Blake3_256).unwrap_err().class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "fixture {}",
      row.id
    );
  }
}

#[test]
fn every_system_control_body_validator_rejects_a_kind_specific_semantic_mutation() {
  let root = fixture_root();
  let hash_width = 32;
  let cases = [
    ("index-registry", 16),
    ("index-operation", 32 + hash_width),
    ("index-degraded", 34 + hash_width),
    ("lifecycle-lkg", 16),
    ("lifecycle-diagnostics", 18),
    ("runtime-lkg", 16),
    ("runtime-diagnostics", 18),
    ("repair-ticket", 48),
    ("path-write-latch", 26 + hash_width),
    ("migration-lease", 120),
    ("migration-progress", 76),
    ("legacy-root-map", 80),
    ("legacy-root-map-page", 96 + 4 * hash_width),
    ("task-pin", 32),
    ("semantic-mutation-segment", 48 + hash_width + 10 + hash_width),
    ("root-publication-prepare", 40 + 3 * hash_width),
    ("root-admission-commit", 40 + hash_width),
    ("durability-latch", 42),
    ("emergency-spill-catalog", 32),
    ("side-by-side-cutover", 88),
  ];
  assert_eq!(cases.len(), SystemControlKindV1::ALL.len());
  for (slug, body_offset) in cases {
    let path = root.join(format!("system-control-v1/control-blake3-256-{slug}-valid.bin"));
    let mut bytes = fs::read(path).unwrap();
    bytes[32 + body_offset..32 + body_offset + 2].fill(0);
    repair_trailing_crc(&mut bytes);
    assert!(decode_system_control(&bytes, HashAlgorithm::Blake3_256).is_err(), "validator {slug} accepted its semantic mutation");
  }
}

#[test]
fn system_control_registry_paths_and_immutable_sequences_are_closed() {
  use std::collections::BTreeSet;

  assert_eq!(SystemControlKindV1::ALL.len(), 20);
  assert_eq!(SystemControlKindV1::ALL.iter().map(|kind| *kind as u16).collect::<BTreeSet<_>>().len(), 20);
  assert_eq!(SystemControlKindV1::ALL.iter().map(|kind| *kind.magic()).collect::<BTreeSet<_>>().len(), 20);
  for kind in SystemControlKindV1::ALL {
    assert_eq!(SystemControlKindV1::from_u16(kind as u16), Some(kind));
    assert_eq!(SystemControlKindV1::from_magic(kind.magic()), Some(kind));
  }

  let root = fixture_root();
  let mut immutable = fs::read(root.join("system-control-v1/control-blake3-256-root-admission-commit-valid.bin")).unwrap();
  immutable[16..24].copy_from_slice(&2u64.to_le_bytes());
  repair_trailing_crc(&mut immutable);
  assert_eq!(
    decode_system_control(&immutable, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mutable = fs::read(root.join("system-control-v1/control-blake3-256-index-degraded-valid.bin")).unwrap();
  let mutable = decode_system_control(&mutable, HashAlgorithm::Blake3_256).unwrap();
  assert!(mutable.canonical_path_for_slot(SystemControlSlotV1::A).unwrap().ends_with("/a.ctrl"));
  assert!(mutable.canonical_path_for_slot(SystemControlSlotV1::B).unwrap().ends_with("/b.ctrl"));
  assert_eq!(
    mutable.canonical_path_for_slot(SystemControlSlotV1::Immutable).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
}

#[test]
fn system_control_bounds_reserves_enums_presence_paths_and_order_fail_closed() {
  let root = fixture_root();

  let mut oversized = fs::read(root.join("system-control-v1/control-blake3-256-index-registry-valid.bin")).unwrap();
  oversized[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
  repair_trailing_crc(&mut oversized);
  assert_eq!(
    decode_system_control(&oversized, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut reserved = fs::read(root.join("system-control-v1/control-blake3-256-index-registry-valid.bin")).unwrap();
  reserved[28] = 1;
  repair_trailing_crc(&mut reserved);
  assert_eq!(
    decode_system_control(&reserved, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NonzeroReservedOrPadding
  );

  let mut unknown_state = fs::read(root.join("system-control-v1/control-blake3-256-index-degraded-valid.bin")).unwrap();
  let degraded_fallback = 32 + 32 + 34;
  unknown_state[degraded_fallback..degraded_fallback + 2].copy_from_slice(&0u16.to_le_bytes());
  repair_trailing_crc(&mut unknown_state);
  assert_eq!(
    decode_system_control(&unknown_state, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut bad_presence = fs::read(root.join("system-control-v1/control-blake3-256-repair-ticket-valid.bin")).unwrap();
  let repair_flags = 32 + 54;
  let flags = test_u16(&bad_presence, repair_flags) ^ 1;
  bad_presence[repair_flags..repair_flags + 2].copy_from_slice(&flags.to_le_bytes());
  repair_trailing_crc(&mut bad_presence);
  assert_eq!(
    decode_system_control(&bad_presence, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
  );

  let mut amplified = fs::read(root.join("system-control-v1/control-blake3-256-legacy-root-map-page-valid.bin")).unwrap();
  let root_map_count = 32 + 88 + 2 * 32;
  amplified[root_map_count..root_map_count + 4].copy_from_slice(&u32::MAX.to_le_bytes());
  repair_trailing_crc(&mut amplified);
  assert_eq!(
    decode_system_control(&amplified, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut duplicate = fs::read(root.join("system-control-v1/control-blake3-256-path-write-latch-valid.bin")).unwrap();
  let latch_rows = 32 + 32 + 32;
  duplicate.copy_within(latch_rows..latch_rows + 16, latch_rows + 16);
  repair_trailing_crc(&mut duplicate);
  assert_eq!(
    decode_system_control(&duplicate, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let mut invalid_path = fs::read(root.join("system-control-v1/control-blake3-256-emergency-spill-catalog-valid.bin")).unwrap();
  let spill_body = 32;
  let spill_fixed = 44 + 32;
  let path_encoding = spill_body + spill_fixed + 4;
  invalid_path[path_encoding..path_encoding + 2].copy_from_slice(&1u16.to_le_bytes());
  let path_start = spill_body + spill_fixed + 72;
  invalid_path[path_start] = 0;
  repair_trailing_crc(&mut invalid_path);
  assert_eq!(
    decode_system_control(&invalid_path, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );
}

#[test]
fn emergency_spill_catalog_accepts_raw_non_utf8_unix_paths() {
  let root = fixture_root();
  let mut bytes = fs::read(root.join("system-control-v1/control-blake3-256-emergency-spill-catalog-valid.bin")).unwrap();
  let spill_body = 32;
  let spill_fixed = 44 + 32;
  let path_encoding = spill_body + spill_fixed + 4;
  bytes[path_encoding..path_encoding + 2].copy_from_slice(&1u16.to_le_bytes());
  let path_start = spill_body + spill_fixed + 72;
  bytes[path_start] = 0xff;
  repair_trailing_crc(&mut bytes);

  let decoded = decode_system_control(&bytes, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(decoded.kind, SystemControlKindV1::EmergencySpillCatalog);
}

#[test]
fn system_control_pair_selection_is_deterministic_under_torn_and_ambiguous_slots() {
  let root = fixture_root();
  let bytes = fs::read(root.join("system-control-v1/control-blake3-256-index-degraded-valid.bin")).unwrap();

  let mut newer = bytes.clone();
  newer[16..24].copy_from_slice(&8u64.to_le_bytes());
  repair_trailing_crc(&mut newer);
  let selected = select_system_control_pair(HashAlgorithm::Blake3_256, &bytes, &newer).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
  assert_eq!(selected.control.sequence, 8);
  assert!(!selected.redundancy_degraded);

  let equal = select_system_control_pair(HashAlgorithm::Blake3_256, &bytes, &bytes).unwrap();
  assert_eq!(equal.selected_slot, SystemControlSlotV1::A);

  let mut torn = newer.clone();
  torn[40] ^= 1;
  let selected = select_system_control_pair(HashAlgorithm::Blake3_256, &bytes, &torn).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
  assert!(selected.redundancy_degraded);

  let mut disagreement = bytes.clone();
  let degraded_at = 32 + 24 + 32;
  let changed_time = test_i64(&disagreement, degraded_at) + 1;
  disagreement[degraded_at..degraded_at + 8].copy_from_slice(&changed_time.to_le_bytes());
  repair_trailing_crc(&mut disagreement);
  assert_eq!(
    select_system_control_pair(HashAlgorithm::Blake3_256, &bytes, &disagreement).unwrap_err().class(),
    MalformedInputClass::AmbiguousEqualSequenceSelector
  );

  let mut other_identity = bytes.clone();
  other_identity[32 + 16] ^= 1;
  repair_trailing_crc(&mut other_identity);
  assert_eq!(
    select_system_control_pair(HashAlgorithm::Blake3_256, &bytes, &other_identity).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  assert!(select_system_control_pair(HashAlgorithm::Blake3_256, &torn, &torn).is_err());

  let immutable = fs::read(root.join("system-control-v1/control-blake3-256-root-admission-commit-valid.bin")).unwrap();
  assert_eq!(
    select_system_control_pair(HashAlgorithm::Blake3_256, &immutable, &immutable).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
}

#[test]
fn external_cutover_journal_selects_equal_newer_torn_and_ambiguous_slots() {
  let root = fixture_root();
  let journal = fs::read(root.join("cutover-journal-v1/cutover-blake3-256-external-journal-valid.bin")).unwrap();
  let selected = select_cutover_journal(&journal, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
  assert_eq!(selected.sequence, 12);
  assert!(!selected.redundancy_degraded);

  let mut equal = journal.clone();
  let a_slot = equal[..1024].to_vec();
  equal[1024..].copy_from_slice(&a_slot);
  let selected = select_cutover_journal(&equal, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
  assert_eq!(selected.sequence, 11);

  let mut torn = journal.clone();
  torn[1024 + 80] ^= 1;
  let selected = select_cutover_journal(&torn, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
  assert!(selected.redundancy_degraded);

  let mut ambiguous = journal.clone();
  ambiguous[1024 + 8..1024 + 16].copy_from_slice(&11u64.to_le_bytes());
  let cutover_state = 1024 + 32 + 88;
  ambiguous[cutover_state..cutover_state + 2].copy_from_slice(&4u16.to_le_bytes());
  repair_cutover_slot_crc(&mut ambiguous[1024..]);
  assert_eq!(
    select_cutover_journal(&ambiguous, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AmbiguousEqualSequenceSelector
  );

  let mut both_torn = journal.clone();
  both_torn[80] ^= 1;
  both_torn[1024 + 80] ^= 1;
  assert_eq!(
    select_cutover_journal(&both_torn, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );
}

#[test]
fn value_store_definition_rejects_identity_child_semantic_and_dependency_corruption() {
  let root = fixture_root();
  let metadata = fs::read(root.join("value-store-definition-v1/avst-blake3-256-metadata-hash-corrected-valid.bin")).unwrap();

  let mut zero_scope = metadata.clone();
  zero_scope[32..64].fill(0);
  assert_eq!(
    decode_value_store_definition(&zero_scope, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut oversized_field = metadata.clone();
  oversized_field[64..68].copy_from_slice(&4_097u32.to_le_bytes());
  assert_eq!(
    decode_value_store_definition(&oversized_field, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut wrong_metadata_name = metadata.clone();
  wrong_metadata_name[144..149].copy_from_slice(b"@size");
  assert_eq!(
    decode_value_store_definition(&wrong_metadata_name, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut mixed_family = metadata.clone();
  mixed_family[90..92].copy_from_slice(&2u16.to_le_bytes());
  assert_eq!(
    decode_value_store_definition(&mixed_family, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut unbounded_corrected = metadata;
  unbounded_corrected[112..120].copy_from_slice(&u64::MAX.to_le_bytes());
  assert_eq!(
    decode_value_store_definition(&unbounded_corrected, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mapper = fs::read(root.join("value-store-definition-v1/avst-blake3-256-mapper-corrected-valid.bin")).unwrap();
  let fixed_start = 64;
  let field_start = 144;
  let field_length = test_u32(&mapper, fixed_start) as usize;
  let selector_length = test_u32(&mapper, fixed_start + 4) as usize;
  let parser_start = field_start + field_length + selector_length;
  let mut unresolved_ordinal = mapper;
  unresolved_ordinal[parser_start + 56..parser_start + 60].copy_from_slice(&99u32.to_le_bytes());
  assert_eq!(
    decode_value_store_definition(&unresolved_ordinal, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  assert_eq!(
    decode_value_store_definition(&vec![0; 512 * 1_024 + 1], HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn source_selector_rejects_amplification_regex_and_mapper_corruption() {
  let root = fixture_root();
  let json_root = fs::read(root.join("source-selector-v1/asel-blake3-256-json-root-valid.bin")).unwrap();
  let mut amplified = json_root;
  amplified[12..16].copy_from_slice(&1_025u32.to_le_bytes());
  assert_eq!(decode_source_selector(&amplified).unwrap_err().class(), MalformedInputClass::AllocationAmplification);

  let metadata = fs::read(root.join("source-selector-v1/asel-blake3-256-metadata-hash-valid.bin")).unwrap();
  let mut unknown_metadata = metadata;
  unknown_metadata[32..34].copy_from_slice(&9u16.to_le_bytes());
  assert_eq!(decode_source_selector(&unknown_metadata).unwrap_err().class(), MalformedInputClass::UnknownTypeKindOrEnum);

  let mixed = fs::read(root.join("source-selector-v1/asel-blake3-256-json-mixed-valid.bin")).unwrap();
  let mut invalid_regex = mixed.clone();
  invalid_regex[80] = b'[';
  assert_eq!(decode_source_selector(&invalid_regex).unwrap_err().class(), MalformedInputClass::InvalidUtf8PathGlobOrNativePath);

  let mut count_mismatch = mixed;
  count_mismatch[12..16].copy_from_slice(&3u32.to_le_bytes());
  assert_eq!(decode_source_selector(&count_mismatch).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mapper = fs::read(root.join("source-selector-v1/asel-blake3-256-mapper-corrected-valid.bin")).unwrap();
  let mut zero_ordinal = mapper.clone();
  zero_ordinal[32..36].fill(0);
  assert_eq!(decode_source_selector(&zero_ordinal).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut mismatched_policy = mapper.clone();
  mismatched_policy[71..73].copy_from_slice(&2u16.to_le_bytes());
  assert_eq!(decode_source_selector(&mismatched_policy).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut invalid_arguments = mapper;
  invalid_arguments[48] = 0xff;
  assert_eq!(decode_source_selector(&invalid_arguments).unwrap_err().class(), MalformedInputClass::UnknownTypeKindOrEnum);

  assert_eq!(decode_source_selector(&vec![0; 4_097]).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn parser_resolution_plan_rejects_amplification_order_and_context_corruption() {
  let root = fixture_root();
  let none = fs::read(root.join("parser-resolution-plan-v1/aprp-blake3-256-none-valid.bin")).unwrap();
  let mut amplified = none.clone();
  amplified[24..28].copy_from_slice(&515u32.to_le_bytes());
  assert_eq!(decode_parser_resolution_plan(&amplified).unwrap_err().class(), MalformedInputClass::AllocationAmplification);

  let automatic = fs::read(root.join("parser-resolution-plan-v1/aprp-blake3-256-automatic-valid.bin")).unwrap();
  let mut unordered = automatic.clone();
  unordered[80..95].copy_from_slice(b"text/zzzzzzzzzz");
  assert_eq!(decode_parser_resolution_plan(&unordered).unwrap_err().class(), MalformedInputClass::NoncanonicalOrderOrDuplicate);

  let mut invalid_mime = automatic.clone();
  invalid_mime[80] = b'A';
  assert_eq!(decode_parser_resolution_plan(&invalid_mime).unwrap_err().class(), MalformedInputClass::InvalidUtf8PathGlobOrNativePath);

  let mut zero_ordinal = automatic.clone();
  zero_ordinal[56..60].fill(0);
  assert_eq!(decode_parser_resolution_plan(&zero_ordinal).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut mixed_semantics = automatic;
  mixed_semantics[20..22].copy_from_slice(&2u16.to_le_bytes());
  assert_eq!(decode_parser_resolution_plan(&mixed_semantics).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut trailing = none;
  trailing.push(0);
  assert_eq!(decode_parser_resolution_plan(&trailing).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);
}

#[test]
fn scope_definition_rejects_noncanonical_context_and_amplification() {
  let root = fixture_root();
  let direct = fs::read(root.join("scope-definition-v1/ascp-blake3-256-root-direct-valid.bin")).unwrap();

  let mut noncanonical_owner = direct.clone();
  noncanonical_owner[64] = b'.';
  assert_eq!(
    decode_scope_definition(&noncanonical_owner, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let mut invalid_utf8 = direct;
  invalid_utf8[64] = 0xff;
  assert_eq!(
    decode_scope_definition(&invalid_utf8, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let glob = fs::read(root.join("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin")).unwrap();
  let mut mismatched_mode = glob;
  mismatched_mode[42..44].copy_from_slice(&1u16.to_le_bytes());
  assert_eq!(
    decode_scope_definition(&mismatched_mode, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let oversized = vec![0u8; 65_537];
  assert_eq!(
    decode_scope_definition(&oversized, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn invocation_and_dependency_readers_reject_context_and_count_corruption() {
  let root = fixture_root();
  let native = fs::read(root.join("invocation-policy-v1/aivp-blake3-256-native-valid.bin")).unwrap();
  let mut native_request = native;
  native_request[24..32].copy_from_slice(&1u64.to_le_bytes());
  assert_eq!(decode_invocation_policy(&native_request).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let wasm = fs::read(root.join("invocation-policy-v1/aivp-blake3-256-pure-wasm-valid.bin")).unwrap();
  let mut unaligned_memory = wasm;
  unaligned_memory[40..48].copy_from_slice(&65_537u64.to_le_bytes());
  assert_eq!(decode_invocation_policy(&unaligned_memory).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let empty = fs::read(root.join("dependency-table-v1/adpt-blake3-256-empty-valid.bin")).unwrap();
  let mut amplified = empty.clone();
  amplified[16..20].copy_from_slice(&1_025u32.to_le_bytes());
  assert_eq!(decode_dependency_table(&amplified).unwrap_err().class(), MalformedInputClass::AllocationAmplification);

  let mut trailing = empty;
  trailing.push(0);
  assert_eq!(decode_dependency_table(&trailing).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);
}

#[test]
fn canonical_config_rejects_aliases_order_depth_and_amplification() {
  let small_u64 = canonical_frame(0x05, &1u64.to_le_bytes());
  assert_eq!(
    validate_canonical_value(&small_u64, CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let negative_zero = canonical_frame(0x06, &(-0.0f64).to_bits().to_le_bytes());
  assert_eq!(
    validate_canonical_value(&negative_zero, CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
  );

  let null = canonical_frame(0x01, &[]);
  let mut map = 2u32.to_le_bytes().to_vec();
  for key in [b"z".as_slice(), b"a".as_slice()] {
    map.extend_from_slice(&(key.len() as u32).to_le_bytes());
    map.extend_from_slice(key);
    map.extend_from_slice(&null);
  }
  assert_eq!(
    validate_canonical_value(&canonical_frame(0x0a, &map), CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let oversized = vec![0u8; CanonicalValueBounds::CONFIG.maximum_value_length + 1];
  assert_eq!(
    validate_canonical_value(&oversized, CanonicalValueBounds::CONFIG).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn namespace_readers_reject_crc_identity_order_and_capability_corruption() {
  let directory = fs::read(fixture_root().join("directory-index-v1/adir-blake3-256-namespace-root-valid.bin")).unwrap();
  let mut bad_capability = directory.clone();
  bad_capability[36 + 3] = 1;
  repair_trailing_crc(&mut bad_capability);
  assert_eq!(
    decode_namespace_root(&bad_capability, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownRequiredCapability
  );

  let mut zero_edge = directory;
  zero_edge[72..104].fill(0);
  repair_trailing_crc(&mut zero_edge);
  assert_eq!(
    decode_namespace_root(&zero_edge, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::InvalidGraphEdgeOrCycle
  );

  let leaf = fs::read(fixture_root().join("semantic-object-v1/asem-blake3-256-catalog-leaf-valid.bin")).unwrap();
  let mut bad_crc = leaf.clone();
  *bad_crc.last_mut().unwrap() ^= 1;
  assert_eq!(
    decode_semantic_object(&bad_crc, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut trailing = leaf;
  trailing.push(0);
  assert_eq!(
    decode_semantic_object(&trailing, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );
}

#[test]
fn whole_entity_rejects_corrupt_framing_before_publication() {
  let source = fs::read(fixture_root().join("whole-entity-v1/entity-blake3-256-directory-root-valid.bin")).unwrap();

  let mut bad_crc = source.clone();
  bad_crc[105] ^= 0x80;
  assert_eq!(
    decode_whole_entity(&bad_crc, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut bad_integrity = source.clone();
  *bad_integrity.last_mut().unwrap() ^= 0x80;
  assert_eq!(decode_whole_entity(&bad_integrity, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().code(), "integrity_hash_mismatch");

  let mut unknown_type = source.clone();
  unknown_type[5] = 0xff;
  repair_entity_header_crc(&mut unknown_type, 32);
  assert_eq!(
    decode_whole_entity(&unknown_type, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );

  let mut zero_sequence = source.clone();
  zero_sequence[33..41].fill(0);
  repair_entity_header_crc(&mut zero_sequence, 32);
  assert_eq!(decode_whole_entity(&zero_sequence, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().code(), "unreserved_write_sequence");

  let mut bad_length = source.clone();
  bad_length[21..25].copy_from_slice(&u32::MAX.to_le_bytes());
  repair_entity_header_crc(&mut bad_length, 32);
  assert_eq!(
    decode_whole_entity(&bad_length, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut trailing = source;
  trailing.push(0);
  assert_eq!(
    decode_whole_entity(&trailing, HashAlgorithm::Blake3_256, u64::MAX).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );
}

#[test]
fn header_probe_distinguishes_v3_and_v4_without_writing() {
  let v4 = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  assert_eq!(probe_header_version(&v4[..8]).unwrap(), DatabaseHeaderVersion::V4);

  let mut v3 = [0u8; 8];
  v3[..4].copy_from_slice(b"AEOR");
  v3[4] = 3;
  assert_eq!(probe_header_version(&v3).unwrap(), DatabaseHeaderVersion::V3);
}

#[test]
fn reading_a_header_region_does_not_modify_the_file() {
  let source = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("probe.aeordb");
  let mut file = fs::File::create(&path).unwrap();
  file.write_all(&source).unwrap();
  drop(file);

  let before = fs::read(&path).unwrap();
  let mut file = fs::File::open(&path).unwrap();
  let selected = read_header_region(&mut file).unwrap();
  assert_eq!(selected.header.slot_sequence, 42);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn dispatching_a_legacy_header_region_does_not_modify_the_file() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("legacy.aeordb");
  let mut source = Vec::with_capacity(HEADER_REGION_SIZE + 16);
  source.extend_from_slice(&deterministic_v3_header(7).serialize().unwrap());
  source.extend_from_slice(&deterministic_v3_header(8).serialize().unwrap());
  source.extend_from_slice(b"legacy-data-tail");
  fs::write(&path, &source).unwrap();

  let before = fs::read(&path).unwrap();
  let mut file = fs::File::open(&path).unwrap();
  let ReadOnlyDatabaseHeader::V3 { header, selected_slot } = read_database_header_read_only(&mut file).unwrap() else {
    panic!("v3 file dispatched to v4")
  };
  assert_eq!(header.sequence, 8);
  assert_eq!(selected_slot, 1);
  drop(file);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn read_only_database_header_dispatch_preserves_v3_and_v4_bytes() {
  let mut legacy_a = deterministic_v3_header(7);
  let mut legacy_b = deterministic_v3_header(8);
  let mut legacy_bytes = Vec::with_capacity(HEADER_REGION_SIZE + 4);
  legacy_bytes.extend_from_slice(&legacy_a.serialize().unwrap());
  legacy_bytes.extend_from_slice(&legacy_b.serialize().unwrap());
  legacy_bytes.extend_from_slice(b"data");
  let before = legacy_bytes.clone();
  let mut legacy = Cursor::new(&mut legacy_bytes);
  let selected = read_database_header_read_only(&mut legacy).unwrap();
  let ReadOnlyDatabaseHeader::V3 { header, selected_slot } = selected else {
    panic!("v3 bytes dispatched to v4")
  };
  assert_eq!(header.sequence, 8);
  assert_eq!(selected_slot, 1);
  assert_eq!(legacy.into_inner(), &before);

  legacy_a.sequence = 9;
  legacy_b.sequence = 9;
  let mut equal = Vec::new();
  equal.extend_from_slice(&legacy_a.serialize().unwrap());
  legacy_b.entry_count += 1;
  equal.extend_from_slice(&legacy_b.serialize().unwrap());
  let mut equal = Cursor::new(equal);
  let ReadOnlyDatabaseHeader::V3 { selected_slot, .. } = read_database_header_read_only(&mut equal).unwrap() else {
    panic!("v3 bytes dispatched to v4")
  };
  assert_eq!(selected_slot, 0, "legacy equal-sequence A-slot behavior is compatibility-frozen");

  let mut v4_bytes = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  let before = v4_bytes.clone();
  let mut v4 = Cursor::new(&mut v4_bytes);
  let selected = read_database_header_read_only(&mut v4).unwrap();
  let ReadOnlyDatabaseHeader::V4(selected) = selected else {
    panic!("v4 bytes dispatched to v3")
  };
  assert_eq!(selected.header.slot_sequence, 42);
  assert_eq!(v4.into_inner(), &before);
}

#[test]
fn read_only_database_header_dispatch_rejects_short_unknown_and_cross_format_regions() {
  let Err(DatabaseHeaderReadError::Probe(error)) = read_database_header_read_only(&mut Cursor::new(vec![0u8; 4])) else {
    panic!("short header did not fail during format probing")
  };
  assert_eq!(error.class(), MalformedInputClass::TruncationOrTrailingBytes);

  let mut unknown = vec![0u8; HEADER_REGION_SIZE];
  unknown[..4].copy_from_slice(b"AEOR");
  unknown[4] = 99;
  let Err(DatabaseHeaderReadError::Probe(error)) = read_database_header_read_only(&mut Cursor::new(unknown)) else {
    panic!("unknown header version did not fail during format probing")
  };
  assert_eq!(error.class(), MalformedInputClass::UnknownMagicOrVersion);

  let mut truncated_v4 = vec![0u8; HEADER_REGION_SIZE];
  truncated_v4[..4].copy_from_slice(b"AEOR");
  truncated_v4[4] = 4;
  let Err(DatabaseHeaderReadError::V4(error)) = read_database_header_read_only(&mut Cursor::new(truncated_v4)) else {
    panic!("truncated v4 region did not retain its format error")
  };
  assert_eq!(error.class(), MalformedInputClass::TruncationOrTrailingBytes);

  let mut truncated_v3 = vec![0u8; HEADER_REGION_SIZE - 1];
  truncated_v3[..4].copy_from_slice(b"AEOR");
  truncated_v3[4] = 3;
  assert!(matches!(read_database_header_read_only(&mut Cursor::new(truncated_v3)), Err(DatabaseHeaderReadError::V3(_))));
}

#[test]
fn bounded_reader_rejects_lengths_before_allocation() {
  let mut bytes = Vec::new();
  bytes.extend_from_slice(&u32::MAX.to_le_bytes());
  let mut reader = BoundedReader::new(&bytes, 1_024).unwrap();
  let error = reader.read_u32_length_prefixed(128).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(reader.allocated_bytes(), 0);
}

#[test]
fn bounded_reader_rejects_overflow_truncation_and_trailing_bytes() {
  assert_eq!(
    BoundedReader::checked_array_bytes(usize::MAX, 2, 1_024).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );

  let mut truncated = BoundedReader::new(&[1, 2, 3], 3).unwrap();
  assert_eq!(truncated.read_exact(4).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);

  let mut trailing = BoundedReader::new(&[1, 2], 2).unwrap();
  trailing.read_u8().unwrap();
  assert_eq!(trailing.finish().unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);
}

#[test]
fn bounded_reader_accepts_exact_limits() {
  let bytes = [3u8, 0, 0, 0, b'a', b'b', b'c'];
  let mut reader = BoundedReader::new(&bytes, bytes.len()).unwrap();
  assert_eq!(reader.read_u32_length_prefixed(3).unwrap(), b"abc");
  reader.finish().unwrap();
  assert_eq!(reader.allocated_bytes(), 3);
}

fn hash_algorithm(name: &str) -> HashAlgorithm {
  match name {
    "blake3-256" => HashAlgorithm::Blake3_256,
    "sha512" => HashAlgorithm::Sha512,
    other => panic!("unsupported fixture hash algorithm {other}"),
  }
}

fn is_sweep_void_fixture(row: &FixtureRow) -> bool {
  row.format_id == "gc-artifact-v1"
    && (row.expected.starts_with("gc:proposal:sweep:")
      || row.expected.starts_with("gc:receipt:sweep-")
      || row.expected.starts_with("gc:page:void-free-extents:")
      || row.expected.starts_with("gc:directory:void-")
      || row.expected.starts_with("gc:manifest:void-catalog:")
      || row.expected.starts_with("gc:claim:void:")
      || row.expected.starts_with("gc:receipt:void-claim-settlement:"))
}

fn is_gc_audit_fixture(row: &FixtureRow) -> bool {
  row.format_id == "gc-artifact-v1"
    && (row.expected.starts_with("gc:manifest:audit-catalog:")
      || row.expected.starts_with("gc:page:audit-")
      || row.expected.starts_with("gc:directory:audit-")
      || row.expected.starts_with("gc:summary:run:")
      || row.expected.starts_with("gc:evidence:corrupt:")
      || row.expected.starts_with("gc:pin:audit:"))
}

fn is_system_control_fixture(row: &FixtureRow) -> bool {
  matches!(row.format_id.as_str(), "system-control-v1" | "cutover-journal-v1")
}

fn test_u32(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn test_u16(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn test_u64(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn test_i64(bytes: &[u8], offset: usize) -> i64 {
  i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn gc_artifact_body_offset(bytes: &[u8]) -> usize {
  32 + test_u16(bytes, 16) as usize
}

fn repair_blake3_sweep_proposal_digest(bytes: &mut [u8]) {
  let hash_width = 32;
  let body = gc_artifact_body_offset(bytes);
  let records = body + 32 + 2 * hash_width;
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"aeordb.sweep-proposal.v1\0");
  hasher.update(&bytes[records..bytes.len() - 4]);
  bytes[body + 32 + hash_width..body + 32 + 2 * hash_width].copy_from_slice(hasher.finalize().as_bytes());
}

fn repair_entity_header_crc(entity: &mut [u8], hash_width: usize) {
  let header_length = 77 + hash_width;
  let crc_offset = header_length - 4;
  let crc = crc32fast::hash(&entity[..crc_offset]);
  entity[crc_offset..header_length].copy_from_slice(&crc.to_le_bytes());
}

fn repair_trailing_crc(value: &mut [u8]) {
  let crc_offset = value.len() - 4;
  let crc = crc32fast::hash(&value[..crc_offset]);
  value[crc_offset..].copy_from_slice(&crc.to_le_bytes());
}

fn repair_cutover_slot_crc(slot: &mut [u8]) {
  assert_eq!(slot.len(), 1024);
  let crc = crc32fast::hash(&slot[..1020]);
  slot[1020..].copy_from_slice(&crc.to_le_bytes());
}

fn system_family_descriptor_offsets(bytes: &[u8]) -> Vec<usize> {
  let count = test_u32(bytes, 12) as usize;
  let mut offsets = Vec::with_capacity(count);
  let mut offset = 32usize;
  for _ in 0..count {
    offsets.push(offset);
    offset += 32 + test_u16(bytes, offset + 28) as usize;
  }
  assert_eq!(offset, bytes.len() - 4);
  offsets
}

fn deterministic_v3_header(sequence: u64) -> FileHeader {
  let mut header = FileHeader::new(HashAlgorithm::Blake3_256);
  header.sequence = sequence;
  header.created_at = 1_700_000_000_000;
  header.updated_at = 1_700_000_000_100;
  header.kv_block_offset = HEADER_REGION_SIZE as u64;
  header.kv_block_length = 4_096;
  header.kv_block_version = 1;
  header.nvt_offset = 4_608;
  header.nvt_length = 1_024;
  header.nvt_version = 1;
  header.head_hash = (0x10..0x30).collect();
  header.entry_count = 7;
  header.buffer_kvs_offset = 5_632;
  header.buffer_nvt_offset = 5_632;
  header.hot_tail_offset = 5_632;
  header.base_hash = (0x20..0x40).collect();
  header.target_hash = (0x30..0x50).collect();
  header
}

fn repair_position_crc_and_encode(decoded: &mut [u8]) -> Vec<u8> {
  use base64::Engine as _;
  use base64::engine::general_purpose::URL_SAFE_NO_PAD;

  repair_trailing_crc(decoded);
  URL_SAFE_NO_PAD.encode(decoded).into_bytes()
}

fn canonical_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
  let mut value = Vec::with_capacity(5 + payload.len());
  value.push(tag);
  value.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  value.extend_from_slice(payload);
  value
}
