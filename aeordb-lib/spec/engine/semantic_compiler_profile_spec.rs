//! Independent source-to-byte conformance before profile emission or activation.
#[path = "../support/semantic_compiler_profile_cases.rs"]
mod cases;
#[allow(dead_code)]
#[path = "../support/semantic_compiler_profile_oracle.rs"]
mod oracle;

use std::path::PathBuf;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::DependencyRecordV1;
use aeordb::engine::v4::index_configuration_compiler::{
  IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1, default_index_configuration_v1,
};
use aeordb::engine::v4::namespace::{EncodedSemanticDefinitionObjectV1, decode_semantic_definition_record};
use aeordb::engine::v4::parser_registry_compiler::{
  ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
};
use aeordb::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;

struct Snapshot(u8);
impl Snapshot {
  fn resolve(&self, role: u16, alias: &str) -> Result<Option<DependencyRecordV1<'static>>, SemanticCompilationErrorV1> {
    if alias == "missing" {
      return Ok(None);
    }
    Ok(Some(DependencyRecordV1 {
      kind: 1,
      role,
      flags: 4,
      abi: role + 2,
      executor_profile: 2,
      fingerprint_semantics: 1,
      artifact_kind: 1,
      artifact_length: 123,
      fingerprint: [if alias == "second" { self.0 + 1 } else { self.0 }; 32],
      dependency_id: "/org/example/shared",
      version: "1.2.3",
    }))
  }
}
impl ParserAliasSnapshotV1 for Snapshot {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(1, alias)
  }
}
impl IndexConfigurationAliasSnapshotV1 for Snapshot {
  fn resolve_mapper_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(2, alias)
  }
}

fn root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}
fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 32 << 20, 16 << 20).unwrap())
}
fn object(class: u16, identity: &[u8], bytes: &[u8]) -> oracle::Object {
  oracle::Object { class, identity: hex::encode(identity), bytes: hex::encode(bytes) }
}
fn wrapped(value: &EncodedSemanticDefinitionObjectV1, algorithm: HashAlgorithm) -> oracle::Object {
  let decoded = decode_semantic_definition_record(&value.object.value, algorithm).unwrap();
  object(decoded.class, &value.semantic_id, decoded.definition)
}

fn verify_case(case: &oracle::Case) {
  for expected in &case.expected {
    let algorithm = HashAlgorithm::from_u16(expected.algorithm).unwrap();
    let memory = memory();
    let snapshot = Snapshot(case.fingerprint);
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: case.registry.as_deref().map(str::as_bytes),
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      },
      &snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let mut actual = oracle::Expected {
      algorithm: expected.algorithm,
      registry: wrapped(registry.projection(), algorithm),
      scope: None,
      projection: None,
      fields: Vec::new(),
      dependencies: Vec::new(),
    };
    if let Some(source) = &case.source {
      let compiled = compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: source.as_bytes(),
          owner_path: &case.owner,
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 256 << 20,
        },
        &snapshot,
        &memory,
        &|| false,
      )
      .unwrap_or_else(|error| panic!("{}: {error}", case.name));
      actual.scope = Some(object(3, &compiled.scope().scope_id, &compiled.scope().value));
      actual.projection = Some(wrapped(compiled.projection(), algorithm));
      for field in compiled.fields() {
        actual.fields.push(oracle::Field {
          name: field.field_name().into(),
          value: object(4, &field.value_store().value_store_id, &field.value_store().value),
          indexes: field.field_indexes().iter().map(|value| object(5, &value.index_id, &value.value)).collect(),
        });
      }
      actual.dependencies = compiled.dependencies().iter().map(|value| wrapped(value, algorithm)).collect();
    }
    // Report the first differing structural field, not megabytes of hex.
    let actual = serde_json::to_value(&actual).unwrap();
    let expected = serde_json::to_value(expected).unwrap();
    compare_output(&actual, &expected, &format!("{} / hash {}", case.name, algorithm as u16));
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn compare_output(actual: &serde_json::Value, expected: &serde_json::Value, path: &str) {
  match (actual, expected) {
    (serde_json::Value::Object(actual), serde_json::Value::Object(expected)) => {
      assert_eq!(actual.len(), expected.len(), "{path}: object members");
      for (name, expected) in expected {
        compare_output(actual.get(name).unwrap(), expected, &format!("{path}.{name}"));
      }
    }
    (serde_json::Value::Array(actual), serde_json::Value::Array(expected)) => {
      assert_eq!(actual.len(), expected.len(), "{path}: array length");
      for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        compare_output(actual, expected, &format!("{path}[{index}]"));
      }
    }
    (serde_json::Value::String(actual), serde_json::Value::String(expected)) if actual.len() > 128 || expected.len() > 128 => {
      assert_eq!(actual.len(), expected.len(), "{path}: string length");
      let first = actual.bytes().zip(expected.bytes()).position(|(left, right)| left != right);
      assert!(first.is_none(), "{path}: first differing character {first:?}");
    }
    _ => assert_eq!(actual, expected, "{path}"),
  }
}

#[test]
fn independently_authored_recipes_match_complete_compiler_output_for_all_five_algorithms() {
  let cases = cases::valid(&root());
  assert!(cases.len() > 90);
  for case in cases {
    verify_case(&case);
  }
}

#[test]
fn every_frozen_source_failure_has_its_exact_class_and_releases_memory() {
  let memory = memory();
  let snapshot = Snapshot(0x42);
  for case in cases::invalid() {
    for algorithm in 1..=5 {
      let algorithm = HashAlgorithm::from_u16(algorithm).unwrap();
      let input = ParserRegistryCompilationRequestV1 {
        source: None,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      };
      let error = if case.registry {
        compile_parser_registry_v1(
          ParserRegistryCompilationRequestV1 { source: Some(case.source.as_bytes()), ..input },
          &snapshot,
          &memory,
          &|| false,
        )
        .err()
        .unwrap()
      } else {
        let registry = compile_parser_registry_v1(input, &snapshot, &memory, &|| false).unwrap();
        compile_index_configuration_v1(
          IndexConfigurationCompilationRequestV1 {
            source: case.source.as_bytes(),
            owner_path: "/",
            registry: &registry,
            hash_algorithm: algorithm,
            maximum_source_bytes: 1 << 20,
            maximum_workspace_bytes: 256 << 20,
          },
          &snapshot,
          &memory,
          &|| false,
        )
        .err()
        .unwrap_or_else(|| panic!("{} unexpectedly accepted", case.name))
      };
      let actual = match error {
        SemanticCompilationErrorV1::InvalidSource { .. } => "InvalidSource",
        SemanticCompilationErrorV1::DependencyUnavailable { .. } => "DependencyUnavailable",
        other => panic!("{}: {other}", case.name),
      };
      assert_eq!(actual, case.outcome, "{}", case.name);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn actual_bootstrap_source_matches_the_independent_complete_recipe() {
  let mut case = cases::valid(&root()).into_iter().find(|case| case.name == "complete-bootstrap").unwrap();
  case.source = Some(std::str::from_utf8(default_index_configuration_v1()).unwrap().into());
  verify_case(&case);
}

fn corpus() -> [Vec<u8>; 4] {
  oracle::FILES.map(|name| std::fs::read(root().join("semantic-compiler-profile-v1").join(name)).unwrap())
}

#[test]
fn frozen_corpus_is_reproducible_and_every_complete_output_still_conforms() {
  let files = corpus();
  let expected = cases::valid(&root());
  let invalid = cases::invalid();
  assert!(files[3] == oracle::packet(b"SCV1", &expected), "valid corpus differs from independent recipes");
  assert!(files[1] == oracle::packet(b"SCI1", &invalid), "invalid corpus differs from independent recipes");
  let cases: Vec<oracle::Case> = oracle::unpack(b"SCV1", &files[3]);
  let mut names = std::collections::BTreeSet::new();
  for case in cases {
    assert!(names.insert(case.name.clone()));
    assert_eq!(case.expected.iter().map(|value| value.algorithm).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
    verify_case(&case);
  }
  let properties: serde_json::Value = serde_json::from_slice(&files[2]).unwrap();
  assert_eq!(properties["policy_fields"], serde_json::json!(oracle::POLICY_NAMES));
  assert_eq!(properties["pure_wasm_defaults"], serde_json::json!(oracle::policy(true)));
  assert_eq!(properties["native_defaults"], serde_json::json!(oracle::policy(false)));
  let recipe = oracle::recipe("x", &[1]);
  for (group, field_names, defaults) in [
    ("source_limits", oracle::SOURCE_NAMES.as_slice(), recipe.source_limits.as_slice()),
    ("converter_limits", oracle::CONVERTER_NAMES.as_slice(), recipe.converter_limits.as_slice()),
    ("field_limits", oracle::FIELD_NAMES.as_slice(), recipe.field_limits.as_slice()),
  ] {
    let rows = properties[group].as_array().unwrap();
    assert_eq!(rows.len(), field_names.len());
    for (index, row) in rows.iter().enumerate() {
      assert_eq!(row[0], field_names[index]);
      assert_eq!(row[1], defaults[index]);
    }
  }
}

#[test]
fn producer_fingerprints_match_independent_frozen_file_frames_for_every_registered_hash() {
  let files = corpus();
  let expected: serde_json::Value =
    serde_json::from_slice(&std::fs::read(root().join("semantic-compiler-profile-v1/fingerprints.json")).unwrap()).unwrap();
  assert_eq!(expected.as_array().unwrap().len(), 5);
  for algorithm in 1..=5 {
    let row = &expected[algorithm as usize - 1];
    assert_eq!(row["algorithm"], algorithm);
    let fingerprint = oracle::fingerprint(algorithm, &files);
    assert_eq!(row["fingerprint"], fingerprint);
    let algorithm = HashAlgorithm::from_u16(algorithm).unwrap();
    let emitted = semantic_compiler_fingerprint_v1(algorithm);
    assert_eq!(hex::encode(emitted), fingerprint);
    assert_eq!(emitted.len(), algorithm.hash_length());
    assert!(emitted.iter().any(|byte| *byte != 0));
    assert!(std::ptr::eq(emitted.as_ptr(), semantic_compiler_fingerprint_v1(algorithm).as_ptr()));
  }
}

#[test]
fn profile_framing_binds_every_file_byte_order_and_length_boundary() {
  let files = corpus();
  for algorithm in 1..=5 {
    let baseline = oracle::fingerprint(algorithm, &files);
    for index in 0..4 {
      for position in [0, files[index].len() / 2, files[index].len() - 1] {
        let mut changed = files.clone();
        changed[index][position] ^= 1;
        assert_ne!(oracle::fingerprint(algorithm, &changed), baseline, "file {index} byte {position}");
      }
    }
    let mut reordered = files.clone();
    reordered.swap(0, 2);
    assert_ne!(oracle::fingerprint(algorithm, &reordered), baseline);
    let mut moved_boundary = files.clone();
    let last = moved_boundary[0].pop().unwrap();
    moved_boundary[1].insert(0, last);
    assert_eq!(files.concat(), moved_boundary.concat());
    assert_ne!(oracle::fingerprint(algorithm, &moved_boundary), baseline);
    assert_ne!(hex::encode(oracle::digest(algorithm, &files.concat())), baseline);
  }
}

#[test]
fn selected_width_profiles_fit_complete_empty_states_without_rejecting_unknown_retained_producers() {
  use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_semantic_object, encode_semantic_state_object};
  for algorithm in 1..=5 {
    let algorithm = HashAlgorithm::from_u16(algorithm).unwrap();
    let width = algorithm.hash_length();
    for fingerprint in [semantic_compiler_fingerprint_v1(algorithm).to_vec(), vec![0xa5; width]] {
      let request = SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::Complete {
          compiler_fingerprint: fingerprint.clone(),
          semantic_registry_fingerprint: vec![0x41; width],
          catalog_root: vec![0; width],
          catalog_record_count: 0,
          catalog_node_count: 0,
          definition_count: 0,
          dependency_count: 0,
        },
      };
      let encoded = encode_semantic_state_object(&request, algorithm).unwrap();
      assert_eq!(&encoded.value[80..80 + width], fingerprint);
      assert_eq!(decode_semantic_object(&encoded.value, algorithm).unwrap().semantic_state.unwrap().availability, request.availability);
    }
  }
}
