//! Real captured native inputs feed both existing compiler snapshot traits.
use super::*;
#[path = "native_alias_table_allocation_spec.rs"]
mod table_allocation;
#[path = "native_alias_snapshot_validation_spec.rs"]
mod validation;
use crate::engine::v4::dependency::encode_dependency_record;
use crate::engine::v4::index_configuration_compiler::{
  compile_index_configuration_v1, IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1,
};
use crate::engine::v4::parser_registry_compiler::{
  compile_parser_registry_v1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
};
use crate::engine::v4::semantic_source_capture::{SemanticSourceAliasKindV1, SemanticSourceAliasRequestV1};

fn snapshot_request(kind: SemanticSourceAliasKindV1, source: Option<&[u8]>) -> NativeSemanticAliasSnapshotRequestV1<'_> {
  NativeSemanticAliasSnapshotRequestV1 {
    source: SemanticSourceAliasRequestV1 {
      kind,
      source,
      maximum_source_bytes: 64 << 10,
      maximum_workspace_bytes: 32 << 20,
      maximum_alias_occurrences: 1024,
    },
    plugins: plugin_bounds(),
    maximum_snapshot_bytes: 8 << 20,
  }
}

#[test]
fn native_alias_snapshot_compiles_real_registry_and_mixed_role_configuration() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("native-alias-compiler", None, [1; 16], algorithm, 0);
    let module = fixtures::module("both");
    seed_plugin(&publisher, &module, "both");
    let before = fs::read(&path).unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let registry_source = br#"{"$v":1,"parsers":{"text/z":"parse","TEXT/A":"parse"}}"#;
    let registry_snapshot = capture
      .prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(registry_source)))
      .expect("captured native registry aliases must prepare");
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(registry_source),
        hash_algorithm: algorithm,
        maximum_source_bytes: 64 << 10,
        maximum_workspace_bytes: 32 << 20,
      },
      &registry_snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(registry.entries().iter().map(|entry| entry.essence()).collect::<Vec<_>>(), ["text/a", "text/z"]);
    for entry in registry.entries() {
      assert_eq!(entry.dependency_bytes(), expected_dependency(&module, 1));
    }
    let configuration_source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"z","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}},{"name":"a","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
    let configuration_snapshot = capture
      .prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(configuration_source)))
      .expect("captured mixed-role aliases must prepare");
    for (record, role) in [
      (configuration_snapshot.resolve_parser_alias("parse").unwrap().unwrap(), 1),
      (configuration_snapshot.resolve_mapper_alias("parse").unwrap().unwrap(), 2),
    ] {
      assert_eq!(encode_dependency_record(&record).unwrap(), expected_dependency(&module, role));
    }
    let compiled = compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source: configuration_source,
        owner_path: "/",
        registry: &registry,
        hash_algorithm: algorithm,
        maximum_source_bytes: 64 << 10,
        maximum_workspace_bytes: 128 << 20,
      },
      &configuration_snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(compiled.fields().len(), 2);
    assert_eq!(compiled.dependencies().len(), 2);
    assert!(publisher.root_state.try_lock().is_ok());
    assert!(publisher.kv.try_lock().is_ok());
    drop(compiled);
    drop(configuration_snapshot);
    drop(registry);
    drop(registry_snapshot);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_alias_snapshot_borrows_stay_fixed_after_alias_replacement() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("native-alias-stability", None, [1; 16]);
  let old_module = fixtures::module("parser");
  let new_module = fixtures::module("both");
  seed_plugin(&publisher, &old_module, "parser");
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
  let request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source));
  let old = capture.prepare_current_semantic_alias_snapshot(request).expect("old aliases must prepare");
  seed_plugin(&publisher, &new_module, "both");
  let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let current = fresh.prepare_current_semantic_alias_snapshot(request).unwrap();
  let old_again = capture.prepare_current_semantic_alias_snapshot(request).unwrap();
  for snapshot in [&old, &old_again] {
    assert_eq!(snapshot.resolve_parser_alias("parse").unwrap().unwrap().fingerprint, *blake3::hash(&old_module).as_bytes());
  }
  assert_eq!(current.resolve_parser_alias("parse").unwrap().unwrap().fingerprint, *blake3::hash(&new_module).as_bytes());
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_distinguishes_captured_absence_unused_and_unprepared_lookups() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("native-alias-absence", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let missing = br#"{"$v":1,"parsers":{"text/plain":"absent"}}"#;
  let snapshot = capture
    .prepare_current_semantic_alias_snapshot(snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(missing)))
    .expect("captured alias absence must be representable");
  assert!(snapshot.resolve_parser_alias("absent").unwrap().is_none());
  assert!(matches!(snapshot.resolve_parser_alias("unprepared"), Err(SemanticCompilationErrorV1::Operational { .. })));
  assert!(matches!(snapshot.resolve_mapper_alias("absent"), Err(SemanticCompilationErrorV1::Operational { .. })));
  for (kind, source) in [
    (SemanticSourceAliasKindV1::ParserRegistry, None),
    (SemanticSourceAliasKindV1::IndexConfiguration, Some(br#"{"$v":1,"parser":"unused","indexes":[]}"#.as_slice())),
    (
      SemanticSourceAliasKindV1::IndexConfiguration,
      Some(br#"{"$v":1,"parser":"unused","indexes":[{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#.as_slice()),
    ),
  ] {
    let mut request = snapshot_request(kind, source);
    request.source.maximum_alias_occurrences = 0;
    let prepared = capture.prepare_current_semantic_alias_snapshot(request).unwrap();
    assert!(matches!(prepared.resolve_parser_alias("unused"), Err(SemanticCompilationErrorV1::Operational { .. })));
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}
