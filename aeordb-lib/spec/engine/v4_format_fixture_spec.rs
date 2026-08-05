use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::database_header::{DatabaseHeaderVersion, decode_header_region, probe_header_version, read_header_region};
use aeordb::engine::v4::config_value::{CanonicalValueBounds, validate_canonical_value};
use aeordb::engine::v4::entity::decode_whole_entity;
use aeordb::engine::v4::field_definition::{decode_converter_definition, decode_field_index_definition};
use aeordb::engine::v4::index_artifact::{
  IndexControlOrManifestV1, decode_active_pointer, decode_index_control_or_manifest, select_active_pointer,
};
use aeordb::engine::v4::index_page::{
  OrderedIndexArtifactV1, OrderedIndexRoleV1, compare_order_keys, decode_ordered_index_artifact, validate_scope_catalog_pair,
};
use aeordb::engine::v4::index_nvt::{coordinate_cell, decode_nvt_tile, verified_page_hint, verified_predecessor_or_fallback};
use aeordb::engine::v4::dependency::{decode_dependency_table, decode_invocation_policy};
use aeordb::engine::v4::namespace::{SemanticObjectKind, decode_namespace_root, decode_semantic_object};
use aeordb::engine::v4::parser_plan::{ParserPlanKind, decode_parser_resolution_plan};
use aeordb::engine::v4::reader::{BoundedReader, MalformedInputClass};
use aeordb::engine::v4::scope::{ScopeMatchingMode, decode_scope_definition};
use aeordb::engine::v4::source_selector::{SourceSelectorKind, decode_source_selector};
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::HashAlgorithm;
use serde::Deserialize;

#[derive(Deserialize)]
struct FixtureManifest {
  fixtures: Vec<FixtureRow>,
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

fn test_u32(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
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

fn canonical_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
  let mut value = Vec::with_capacity(5 + payload.len());
  value.push(tag);
  value.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  value.extend_from_slice(payload);
  value
}
