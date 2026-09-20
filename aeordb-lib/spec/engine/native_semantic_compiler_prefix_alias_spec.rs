//! Retained alias identity changes must affect compiler-prefix admission.
use super::*;
use crate::engine::v4::index_configuration_compiler::CompiledIndexConfigurationV1;
use crate::engine::v4::semantic_catalog_compiler::compile_semantic_catalog_v1;
use crate::engine::v4::semantic_source_capture::{SemanticSourceAliasKindV1, SemanticSourceAliasRequestV1};

const REGISTRY: &[u8] = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
const MAPPER: &[u8] = br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
const EMPTY: &[u8] = br#"{"$v":1,"indexes":[]}"#;

fn source_snapshot(kind: SemanticSourceAliasKindV1, body: Option<&[u8]>) -> NativeSemanticAliasSnapshotRequestV1<'_> {
  NativeSemanticAliasSnapshotRequestV1 {
    source: SemanticSourceAliasRequestV1 {
      kind,
      source: body,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 32 << 20,
      maximum_alias_occurrences: 1024,
    },
    plugins: NativeSemanticPluginSourceBoundsV1 {
      maximum_module_bytes: 4 << 20,
      maximum_chunk_entity_bytes: 4 << 20,
      maximum_source_chunks: 1024,
      maximum_read_bytes: 32 << 20,
      maximum_workspace_bytes: 1 << 20,
    },
    maximum_snapshot_bytes: 4 << 20,
  }
}

fn compile_current_inputs(
  publisher: &V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  registry_source: Option<&[u8]>,
  configuration_source: &[u8],
) -> (CompiledParserRegistryV1, CompiledIndexConfigurationV1) {
  let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), memory, cancellation).unwrap();
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let aliases =
    capture.prepare_current_semantic_alias_snapshot(source_snapshot(SemanticSourceAliasKindV1::ParserRegistry, registry_source)).unwrap();
  let registry = crate::engine::v4::parser_registry_compiler::compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: registry_source,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &aliases,
    memory,
    &|| false,
  )
  .unwrap();
  let aliases = capture
    .prepare_current_semantic_alias_snapshot(source_snapshot(SemanticSourceAliasKindV1::IndexConfiguration, Some(configuration_source)))
    .unwrap();
  let configuration = compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: configuration_source,
      owner_path: "/",
      registry: &registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &aliases,
    memory,
    &|| false,
  )
  .unwrap();
  (registry, configuration)
}

fn module_revisions(publisher: &V4FirstAuthorityPublisher, module: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let alias = plugin_fixtures::alias(module, "both");
  seed_files(
    publisher,
    &[
      (plugin_fixtures::artifact_path(module), "application/wasm", module),
      (plugin_fixtures::alias_path(), "application/octet-stream", &alias),
    ],
  );
  (
    seed_retained_revision(publisher, &plugin_fixtures::alias_path()),
    seed_retained_revision(publisher, &plugin_fixtures::artifact_path(module)),
  )
}

fn alias_case(algorithm: HashAlgorithm, registry_change: bool, wrong_prefix: bool) {
  alias_case_with_base_profile(algorithm, registry_change, wrong_prefix, false);
}

fn alias_case_with_base_profile(algorithm: HashAlgorithm, registry_change: bool, wrong_prefix: bool, unsupported_base_profile: bool) {
  alias_case_with_base_fault(algorithm, registry_change, wrong_prefix, unsupported_base_profile, false);
}

fn alias_case_with_base_fault(
  algorithm: HashAlgorithm,
  registry_change: bool,
  wrong_prefix: bool,
  unsupported_base_profile: bool,
  missing_base_catalog: bool,
) {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("compiler-prefix-retained-alias", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  publisher.publish(&initial).unwrap();
  let body = if registry_change { EMPTY } else { MAPPER };
  let old_module = plugin_fixtures::module("both");
  let mut new_module = old_module.clone();
  plugin_fixtures::custom("source-revision", b"requested", &mut new_module);
  let (old_alias, old_artifact) = module_revisions(&publisher, &old_module);
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", body)]);
  let global = seed_retained_revision(&publisher, INDEX_SOURCE);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(768 << 20, 1024 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let (base_registry, base_configuration) = compile_current_inputs(&publisher, &memory, &cancellation, None, body);
  let request = SemanticCatalogCompilationRequestV1 {
    hash_algorithm: algorithm,
    expected_configuration_count: 1,
    required_capabilities: [0; 32],
    maximum_workspace_bytes: 128 << 20,
  };
  let previous = {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      [1; 16],
      publisher.observe().unwrap().selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    compile_semantic_catalog_v1(request, &base_registry, [Ok(base_configuration)], &mut store, &memory, &|| false).unwrap()
  };
  let mut next = successor_request(&publisher, 0x79, "unused");
  next.semantic_state = previous.semantic_state().clone();
  if unsupported_base_profile || missing_base_catalog {
    let mut state = decode_semantic_object(&next.semantic_state.value, algorithm).unwrap().semantic_state.unwrap();
    match &mut state.availability {
      SemanticAvailabilityV1::Complete { compiler_fingerprint, catalog_root, .. } => {
        if unsupported_base_profile {
          compiler_fingerprint.fill(0x42);
        }
        if missing_base_catalog {
          catalog_root.fill(0x57);
        }
      }
      _ => unreachable!(),
    }
    next.semantic_state = encode_semantic_state_object(
      &SemanticStateWriteV1 { required_capabilities: state.required_capabilities, availability: state.availability },
      algorithm,
    )
    .unwrap();
  }
  next.namespace_tree = initial.namespace_tree.clone();
  if unsupported_base_profile || missing_base_catalog {
    stage_fixture_semantic_state(&publisher, &memory, &cancellation, &next.semantic_state);
  }
  let base = publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash;
  let (new_alias, new_artifact) = module_revisions(&publisher, &new_module);
  let parser_revision = if registry_change {
    seed_files(&publisher, &[(PARSER_SOURCE.to_string(), "application/json", REGISTRY)]);
    Some(seed_retained_revision(&publisher, PARSER_SOURCE))
  } else {
    None
  };
  let (registry, configuration) = compile_current_inputs(&publisher, &memory, &cancellation, registry_change.then_some(REGISTRY), body);
  assert_eq!(registry.projection() == base_registry.projection(), !registry_change);
  let mut base_sources = absent_globals();
  base_sources.insert(INDEX_SOURCE.to_string(), Some(global));
  base_sources.insert(plugin_fixtures::alias_path(), Some(old_alias));
  base_sources.insert(plugin_fixtures::artifact_path(&old_module), Some(old_artifact));
  base_sources.insert(plugin_fixtures::artifact_path(&new_module), None);
  let mut requested = base_sources.clone();
  requested.insert(PARSER_SOURCE.to_string(), parser_revision);
  requested.insert(plugin_fixtures::alias_path(), Some(new_alias));
  requested.insert(plugin_fixtures::artifact_path(&old_module), None);
  requested.insert(plugin_fixtures::artifact_path(&new_module), Some(new_artifact));
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let mut store = NativeSemanticCatalogStagingStoreV1::new(
    &protection,
    [1; 16],
    publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    &cancellation,
  )
  .unwrap();
  let mut work = if registry_change {
    SemanticCatalogContinuationV1::start(request, &registry, &mut store, &memory, &|| false).unwrap()
  } else {
    SemanticCatalogContinuationV1::from_complete(request, &previous, &registry, &store, &memory, &|| false).unwrap()
  };
  // Changed registry: before global, fresh work must contain no configuration.
  // Changed mapper only: after global, the same JSON must use its new dependency.
  if registry_change == wrong_prefix {
    work = work.apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration), &mut store).unwrap();
  } else {
    drop(configuration);
  }
  seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &base_sources, &requested, &base_sources, 1);
  let checkpoint = seed_prefix_checkpoint(
    &publisher,
    &work,
    if registry_change { SemanticMutationCursorV1::None } else { SemanticMutationCursorV1::ConfigurationOwner("/") },
    |_| {},
  );
  drop(work);
  drop(protection);
  drop(previous);
  seed_files(&publisher, &[(plugin_fixtures::alias_path(), "application/octet-stream", b"unrelated current alias")]);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut bounds = prefix_bounds(&checkpoint, algorithm);
  bounds.maximum_compiler_workspace_bytes = 128 << 20;
  let result = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, bounds);
  if unsupported_base_profile {
    let error = result.err().expect("unsupported BASE compiler profile must not become fresh fallback");
    assert!(error.to_string().contains("semantic_catalog_base_profile"), "{error}");
  } else if missing_base_catalog {
    let error = result.err().expect("missing nonempty BASE catalog must not become fresh fallback");
    assert!(error.to_string().contains("semantic_catalog_missing"), "{error}");
  } else if wrong_prefix {
    let error = result.err().expect("stale prefix must refuse");
    assert!(error.to_string().contains("semantic_compiler_prefix_projection"), "{error}");
  } else {
    let admitted = result.unwrap();
    assert_eq!(
      admitted.construction_mode(),
      if registry_change { SemanticCompilerConstructionModeV1::Fresh } else { SemanticCompilerConstructionModeV1::Incremental }
    );
    assert_eq!(admitted.configuration_count(), u64::from(!registry_change));
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn retained_compiler_prefix_registry_changes_require_fresh_work() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for wrong in [false, true] {
      alias_case(algorithm, true, wrong);
    }
  }
}

#[test]
fn retained_compiler_prefix_same_json_uses_each_retained_alias_revision() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for wrong in [false, true] {
      alias_case(algorithm, false, wrong);
    }
  }
}

#[test]
fn retained_compiler_prefix_unknown_base_profile_refuses_even_when_registry_changes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for changed in [false, true] {
      alias_case_with_base_profile(algorithm, changed, false, true);
    }
  }
}

#[test]
fn retained_compiler_prefix_missing_base_catalog_refuses_even_when_registry_changes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for changed in [false, true] {
      alias_case_with_base_fault(algorithm, changed, false, false, true);
    }
  }
}
