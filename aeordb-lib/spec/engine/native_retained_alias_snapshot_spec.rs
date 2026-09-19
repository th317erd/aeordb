//! Retained-side compiler inputs must never consult current aliases.
#[path = "native_retained_alias_snapshot_boundary_spec.rs"]
mod boundary;
#[path = "native_retained_alias_snapshot_resource_spec.rs"]
mod resource;
use super::*;
use super::super::super::plugin_fixtures as fixtures;
use crate::engine::v4::dependency::encode_dependency_record;
use crate::engine::v4::index_configuration_compiler::{
  compile_index_configuration_v1, IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1,
};
use crate::engine::v4::parser_registry_compiler::{compile_parser_registry_v1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1};
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
    plugins: NativeSemanticPluginSourceBoundsV1 {
      maximum_module_bytes: 1 << 20,
      maximum_chunk_entity_bytes: 2 << 20,
      maximum_source_chunks: 1024,
      maximum_read_bytes: 8 << 20,
      maximum_workspace_bytes: 64 << 10,
    },
    maximum_snapshot_bytes: 8 << 20,
  }
}

fn seed_module(publisher: &V4FirstAuthorityPublisher, module: &[u8], role: &str) -> (Vec<u8>, Vec<u8>) {
  let alias = fixtures::alias(module, role);
  seed_files(
    publisher,
    &[(fixtures::artifact_path(module), "application/wasm", module), (fixtures::alias_path(), "application/octet-stream", &alias)],
  );
  (seed_retained_revision(publisher, &fixtures::alias_path()), seed_retained_revision(publisher, &fixtures::artifact_path(module)))
}

fn expected_dependency(module: &[u8], role: u16) -> Vec<u8> {
  let id = b"/org/example/both";
  let version = b"1.0.0-rc.1+02";
  let mut bytes = vec![0; 96];
  bytes[..4].copy_from_slice(&((96 + id.len() + version.len()) as u32).to_le_bytes());
  for (offset, value) in [(4, 1u16), (6, role), (12, role + 2), (14, 2), (16, 1), (18, 1)] {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[8..12].copy_from_slice(&4u32.to_le_bytes());
  bytes[20..24].copy_from_slice(&(id.len() as u32).to_le_bytes());
  bytes[24..28].copy_from_slice(&(version.len() as u32).to_le_bytes());
  bytes[32..40].copy_from_slice(&(module.len() as u64).to_le_bytes());
  bytes[40..72].copy_from_slice(blake3::hash(module).as_bytes());
  bytes.extend_from_slice(id);
  bytes.extend_from_slice(version);
  bytes
}

#[test]
fn retained_alias_snapshot_compiles_both_sides_after_reopen_without_current_fallback() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-alias-compile", None, [1; 16], algorithm, 0);
    let old_module = fixtures::module("both");
    let mut new_module = old_module.clone();
    fixtures::custom("fixture-revision", b"requested", &mut new_module);
    let (old_alias, old_artifact) = seed_module(&publisher, &old_module, "both");
    let (new_alias, new_artifact) = seed_module(&publisher, &new_module, "both");
    let base = std::collections::BTreeMap::from([
      (INDEX_SOURCE.to_string(), None),
      (PARSER_SOURCE.to_string(), None),
      (fixtures::alias_path(), Some(old_alias)),
      (fixtures::artifact_path(&old_module), Some(old_artifact)),
      (fixtures::artifact_path(&new_module), Some(new_artifact)),
    ]);
    let base_rows: Vec<_> = base.iter().map(|(path, id)| (path.as_str(), id.as_deref())).collect();
    let mut requested = base.clone();
    requested.insert(fixtures::alias_path(), Some(new_alias));
    let requested_rows: Vec<_> = requested.iter().map(|(path, id)| (path.as_str(), id.as_deref())).collect();
    seed_catalog_pair(&publisher, &base_rows, &requested_rows);
    seed_module(&publisher, &fixtures::module("parser"), "parser");
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    for (side, module) in [(SemanticSourceCatalogSideV1::Base, &old_module), (SemanticSourceCatalogSideV1::Requested, &new_module)] {
      let registry_source = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
      let snapshot = capture
        .prepare_captured_semantic_alias_snapshot(
          &[2; 16],
          1,
          side,
          snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(registry_source)),
          catalog_bounds(),
        )
        .expect("retained registry aliases must prepare from the selected side");
      let registry = compile_parser_registry_v1(
        ParserRegistryCompilationRequestV1 {
          source: Some(registry_source),
          hash_algorithm: algorithm,
          maximum_source_bytes: 64 << 10,
          maximum_workspace_bytes: 32 << 20,
        },
        &snapshot,
        &memory,
        &|| false,
      )
      .unwrap();
      assert_eq!(registry.entries().len(), 1);
      assert_eq!(registry.entries()[0].dependency_bytes(), expected_dependency(module, 1));
      let source = br#"{"$v":1,"parser":"parse","indexes":[{"name":"value","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
      let snapshot = capture
        .prepare_captured_semantic_alias_snapshot(
          &[2; 16],
          1,
          side,
          snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source)),
          catalog_bounds(),
        )
        .unwrap();
      assert_eq!(
        encode_dependency_record(&snapshot.resolve_parser_alias("parse").unwrap().unwrap()).unwrap(),
        expected_dependency(module, 1)
      );
      assert_eq!(
        encode_dependency_record(&snapshot.resolve_mapper_alias("parse").unwrap().unwrap()).unwrap(),
        expected_dependency(module, 2)
      );
      let configuration = compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source,
          owner_path: "/",
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 64 << 10,
          maximum_workspace_bytes: 128 << 20,
        },
        &snapshot,
        &memory,
        &|| false,
      )
      .unwrap();
      assert_eq!(configuration.fields().len(), 1);
      assert_eq!(configuration.dependencies().len(), 2);
      assert!(reopened.root_state.try_lock().is_ok());
      assert!(reopened.kv.try_lock().is_ok());
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_alias_snapshot_distinguishes_absent_from_unlisted_despite_current_presence() {
  for listed in [true, false] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-absence", None, [1; 16]);
    seed_module(&publisher, &fixtures::module("both"), "both");
    let alias_path = fixtures::alias_path();
    let mut rows = vec![(INDEX_SOURCE, None), (PARSER_SOURCE, None)];
    if listed {
      rows.push((alias_path.as_str(), None));
    }
    seed_catalog_pair(&publisher, &rows, &rows);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let source = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
    for side in [SemanticSourceCatalogSideV1::Base, SemanticSourceCatalogSideV1::Requested] {
      let result = capture.prepare_captured_semantic_alias_snapshot(
        &[2; 16],
        1,
        side,
        snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(source)),
        catalog_bounds(),
      );
      if listed {
        let snapshot = result.expect("explicit retained absence must prepare without current fallback");
        assert!(snapshot.resolve_parser_alias("parse").unwrap().is_none());
      } else {
        let error = result.err().expect("unlisted input must refuse");
        assert!(matches!(error, NativeSemanticPluginSourceErrorV1::Source(error) if error.code() == "semantic_source_catalog_unlisted"));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
