//! Actual native captured pairs, not a mock alias resolver or executor test.
use super::*;
#[path = "native_plugin_source_lifecycle_spec.rs"]
mod lifecycle;
#[path = "native_alias_snapshot_spec.rs"]
mod prepared;
#[path = "native_plugin_source_validation_spec.rs"]
mod validation;
use crate::engine::v4::semantic_source_capture::SemanticSourceAliasRoleV1;
#[path = "plugin_artifact_identity_fixtures.rs"]
mod fixtures;

fn plugin_bounds() -> NativeSemanticPluginSourceBoundsV1 {
  NativeSemanticPluginSourceBoundsV1 {
    maximum_module_bytes: 1 << 20,
    maximum_chunk_entity_bytes: 2 << 20,
    maximum_source_chunks: 1024,
    maximum_read_bytes: 8 << 20,
    maximum_workspace_bytes: 64 << 10,
  }
}

fn seed_plugin(publisher: &V4FirstAuthorityPublisher, module: &[u8], role: &str) {
  let alias = fixtures::alias(module, role);
  seed_files(
    publisher,
    &[(fixtures::artifact_path(module), "application/wasm", module), (fixtures::alias_path(), "application/octet-stream", &alias)],
  );
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
fn native_plugin_sources_bind_actual_pair_and_independent_both_role_records() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("native-plugin-pair", None, [1; 16], algorithm, 0);
    let module = fixtures::module("both");
    seed_plugin(&publisher, &module, "both");
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let pair = capture.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
    assert_eq!(pair.alias_source().record().path, fixtures::alias_path());
    assert_eq!(pair.alias_source().body(), fixtures::alias(&module, "both"));
    assert_eq!(pair.artifact_source().record().path, fixtures::artifact_path(&module));
    assert_eq!(pair.artifact_source().body(), module);
    for source in [pair.alias_source(), pair.artifact_source()] {
      assert_eq!(source.revision(), digest_parts(algorithm, &[b"filec:", source.encoded_record()]));
    }
    for (role, number) in [(SemanticSourceAliasRoleV1::Parser, 1), (SemanticSourceAliasRoleV1::Mapper, 2)] {
      assert_eq!(pair.dependency_bytes(role).unwrap(), expected_dependency(&module, number));
      let record = pair.dependency_record(role).unwrap().unwrap();
      assert_eq!(record.dependency_id, "/org/example/both");
      assert_eq!(record.role, number);
      assert_eq!(record.fingerprint, *blake3::hash(&module).as_bytes());
    }
    assert!(publisher.root_state.try_lock().is_ok());
    assert!(publisher.kv.try_lock().is_ok());
    assert!(memory.snapshot().unwrap().reserved_bytes > baseline);
    drop(pair);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(capture);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_plugin_sources_old_capture_survives_replacement_and_fresh_capture_changes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("native-plugin-replacement", None, [1; 16], algorithm, 0);
    let old_module = fixtures::module("parser");
    let new_module = fixtures::module("mapper");
    seed_plugin(&publisher, &old_module, "parser");
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    seed_plugin(&publisher, &new_module, "mapper");
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let previous = old.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
    let current = fresh.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().unwrap();
    assert_eq!(previous.artifact_source().body(), old_module);
    assert_eq!(current.artifact_source().body(), new_module);
    assert!(previous.dependency_record(SemanticSourceAliasRoleV1::Mapper).unwrap().is_none());
    assert!(current.dependency_record(SemanticSourceAliasRoleV1::Parser).unwrap().is_none());
    assert_ne!(previous.alias_source().revision(), current.alias_source().revision());
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_plugin_sources_proven_absence_does_not_consult_a_later_alias() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("native-plugin-absent", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  seed_plugin(&publisher, &fixtures::module("parser"), "parser");
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  assert!(old.read_protected_plugin_sources("parse", plugin_bounds()).unwrap().is_none());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
