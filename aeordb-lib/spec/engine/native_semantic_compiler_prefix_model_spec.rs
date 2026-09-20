//! Independent owner/order model over native retained source and catalog files.
#[path = "native_semantic_compiler_prefix_resource_spec.rs"]
mod resource;
use super::*;
use std::collections::BTreeMap;
use crate::engine::v4::semantic_catalog_compiler::compile_semantic_catalog_v1;

const EMPTY: &[u8] = br#"{"$v":1,"indexes":[]}"#;
const CHANGED: &[u8] = br#"{"$v":1,"glob":"*.changed","indexes":[]}"#;
type Configurations = BTreeMap<&'static str, &'static [u8]>;

struct Model {
  base: Configurations,
  requested: Configurations,
  compiled_base: bool,
  complete_empty_base: bool,
  cursor: Option<&'static str>,
  pruning: bool,
}

fn model_sources(publisher: &V4FirstAuthorityPublisher, configurations: &Configurations) -> (Vec<u8>, SourceMap, SourceMap) {
  let mut protected = absent_globals();
  let mut namespace = SourceMap::new();
  let mut children = Vec::new();
  for (&owner, &body) in configurations {
    if owner == "/" {
      seed_files(publisher, &[(INDEX_SOURCE.to_string(), "application/json", body)]);
      protected.insert(INDEX_SOURCE.to_string(), Some(seed_retained_revision(publisher, INDEX_SOURCE)));
    } else {
      assert_eq!(owner.matches('/').count(), 1, "model only uses direct child owners");
      let (tree, revision) = namespace_configuration_tree(publisher, owner, body, vec![]);
      children.push(namespace_directory_child(&owner[1..], tree));
      namespace.insert(format!("{owner}/.aeordb-config/indexes.json"), Some(revision));
    }
  }
  (publish_namespace_directory(publisher, children), protected, namespace)
}

fn configuration(
  algorithm: HashAlgorithm,
  registry: &CompiledParserRegistryV1,
  memory: &MemoryCoordinator,
  owner: &str,
  body: &[u8],
) -> crate::engine::v4::index_configuration_compiler::CompiledIndexConfigurationV1 {
  compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: body,
      owner_path: owner,
      registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &NoAliases,
    memory,
    &|| false,
  )
  .unwrap()
}

fn with_model(
  algorithm: HashAlgorithm,
  model: Model,
  inspect: impl FnOnce(&NativeSemanticMutationInventoryV1<'_>, &[u8], &MemoryCoordinator, &CancellationToken, &V4FirstAuthorityPublisher),
) {
  with_model_operation(algorithm, model, false, |_| {}, inspect);
}

fn with_model_operation<T>(
  algorithm: HashAlgorithm,
  model: Model,
  allow_staging: bool,
  checkpoint_adjust: impl FnOnce(&mut crate::engine::v4::semantic_mutation_control::SemanticMutationCheckpointV1<'_>),
  inspect: impl FnOnce(&NativeSemanticMutationInventoryV1<'_>, &[u8], &MemoryCoordinator, &CancellationToken, &V4FirstAuthorityPublisher) -> T,
) -> (tempfile::TempDir, PathBuf, T) {
  let (directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("compiler-prefix-model", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let initial_root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  let (base_tree, base_sources, base_namespace) = model_sources(&publisher, &model.base);
  let (requested_tree, requested_sources, requested_namespace) = model_sources(&publisher, &model.requested);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let registry = crate::engine::v4::parser_registry_compiler::compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &NoAliases,
    &memory,
    &|| false,
  )
  .unwrap();
  let request = SemanticCatalogCompilationRequestV1 {
    hash_algorithm: algorithm,
    expected_configuration_count: model.requested.len() as u64,
    required_capabilities: [0; 32],
    maximum_workspace_bytes: 64 << 20,
  };
  let previous = if model.compiled_base {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      [1; 16],
      publisher.observe().unwrap().selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    Some(
      compile_semantic_catalog_v1(
        SemanticCatalogCompilationRequestV1 { expected_configuration_count: model.base.len() as u64, ..request },
        &registry,
        model.base.iter().map(|(&owner, &body)| Ok(configuration(algorithm, &registry, &memory, owner, body))),
        &mut store,
        &memory,
        &|| false,
      )
      .unwrap(),
    )
  } else {
    None
  };
  let mut next = successor_request(&publisher, 0x79, "unused");
  next.semantic_state = previous.as_ref().map(|catalog| catalog.semantic_state().clone()).unwrap_or(initial.semantic_state);
  if model.complete_empty_base {
    assert!(!model.compiled_base);
    next.semantic_state = encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::Complete {
          compiler_fingerprint: semantic_compiler_fingerprint_v1(algorithm).to_vec(),
          semantic_registry_fingerprint: embedded_system_family_registry(algorithm).unwrap().semantic_projection_fingerprint.clone(),
          catalog_root: vec![0; algorithm.hash_length()],
          catalog_record_count: 0,
          catalog_node_count: 0,
          definition_count: 0,
          dependency_count: 0,
        },
      },
      algorithm,
    )
    .unwrap();
  }
  next.namespace_tree = PreparedNamespaceTreeV0 {
    root_hash: base_tree.clone(),
    stored_value: publisher.load_immutable_entity_bounded(&base_tree, 1 << 20).unwrap().unwrap().stored_value,
  };
  let base = if model.base.is_empty() && !model.compiled_base && !model.complete_empty_base {
    // Reuse the already-admitted identical empty authority. Re-publishing the
    // same root with an unrelated fixture transaction is not a new successor.
    initial_root
  } else {
    if model.complete_empty_base {
      stage_fixture_semantic_state(&publisher, &memory, &cancellation, &next.semantic_state);
    }
    publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash
  };
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let mut store = NativeSemanticCatalogStagingStoreV1::new(
    &protection,
    [1; 16],
    publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    &cancellation,
  )
  .unwrap();
  let mut work = match previous.as_ref() {
    Some(previous) => SemanticCatalogContinuationV1::from_complete(request, previous, &registry, &store, &memory, &|| false).unwrap(),
    None => SemanticCatalogContinuationV1::start(request, &registry, &mut store, &memory, &|| false).unwrap(),
  };
  let mut owners = BTreeMap::new();
  owners.insert("/", ());
  for owner in model.base.keys().chain(model.requested.keys()) {
    owners.insert(*owner, ());
  }
  // Explicit global slot precedes byte-ordered full namespace file paths.
  let order = std::iter::once("/").chain(owners.keys().copied().filter(|owner| *owner != "/"));
  let mut expected_prefix = if model.compiled_base { model.base.clone() } else { Configurations::new() };
  for owner in order {
    let processed = model.pruning
      || model.cursor.is_some_and(|cursor| {
        owner == "/" || (cursor != "/" && format!("{owner}/.aeordb-config/indexes.json") <= format!("{cursor}/.aeordb-config/indexes.json"))
      });
    if !processed {
      continue;
    }
    let mutation = match model.requested.get(owner) {
      Some(body) => {
        expected_prefix.insert(owner, body);
        SemanticCatalogConfigurationMutationV1::Upsert(configuration(algorithm, &registry, &memory, owner, body))
      }
      None => {
        expected_prefix.remove(owner);
        SemanticCatalogConfigurationMutationV1::Remove(owner.into())
      }
    };
    work = work.apply(mutation, &mut store).unwrap();
  }
  if model.pruning {
    work = work.finish_configurations(&mut store).unwrap();
  }
  assert_eq!(work.configuration_count(), expected_prefix.len() as u64);
  let mut fingerprint = base_sources.clone();
  for (path, revision) in base_namespace {
    fingerprint.insert(path, revision);
  }
  for path in requested_namespace.keys() {
    fingerprint.entry(path.clone()).or_insert(None);
  }
  seed_validation_pair(&publisher, &base, &requested_tree, &base_sources, &requested_sources, &fingerprint, model.requested.len() as u64);
  let cursor = if model.pruning {
    SemanticMutationCursorV1::None
  } else {
    model.cursor.map(SemanticMutationCursorV1::ConfigurationOwner).unwrap_or(SemanticMutationCursorV1::None)
  };
  let checkpoint = seed_prefix_checkpoint(&publisher, &work, cursor, checkpoint_adjust);
  drop(work);
  drop(protection);
  drop(previous);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let admission = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(&checkpoint, algorithm));
  if model.complete_empty_base && !model.base.is_empty() {
    let error = admission.err().expect("Complete-empty cannot certify actual BASE configurations");
    assert!(error.to_string().contains("semantic_compiler_prefix_empty_base"), "{error}");
  } else {
    let admitted = admission.unwrap();
    assert_eq!(admitted.configuration_count(), expected_prefix.len() as u64);
    assert_eq!(
      admitted.construction_mode(),
      if model.compiled_base { SemanticCompilerConstructionModeV1::Incremental } else { SemanticCompilerConstructionModeV1::Fresh }
    );
    assert_eq!(admitted.phase(), if model.pruning { SemanticMutationPhaseV1::Pruning } else { SemanticMutationPhaseV1::Compiling });
  }
  assert_eq!(fs::read(&path).unwrap(), before, "admission itself is always read-only");
  let head = publisher.observe().unwrap().selected.header.head_hash;
  let result = inspect(&capture, &checkpoint, &memory, &cancellation, &publisher);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  if !allow_staging {
    assert_eq!(fs::read(&path).unwrap(), before);
  }
  assert_eq!(publisher.observe().unwrap().selected.header.head_hash, head, "unselected catalog staging never activates HEAD");
  drop(capture);
  drop(protection);
  drop(publisher);
  (directory, path, result)
}

#[test]
fn retained_compiler_prefix_models_fresh_global_absence_and_namespace_positions() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for cursor in [None, Some("/"), Some("/!before"), Some("/z")] {
      with_model(
        algorithm,
        Model {
          base: Configurations::new(),
          requested: Configurations::from([("/!before", EMPTY), ("/z", CHANGED)]),
          compiled_base: false,
          complete_empty_base: false,
          cursor,
          pruning: false,
        },
        |_, _, _, _, _| {},
      );
    }
  }
}

#[test]
fn retained_compiler_prefix_complete_empty_base_starts_fresh() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    with_model(
      algorithm,
      Model {
        base: Configurations::new(),
        requested: Configurations::from([("/a", EMPTY)]),
        compiled_base: false,
        complete_empty_base: true,
        cursor: None,
        pruning: false,
      },
      |_, _, _, _, _| {},
    );
  }
}

#[test]
fn retained_compiler_prefix_complete_empty_base_cannot_hide_existing_configuration() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_model(
      algorithm,
      Model {
        base: Configurations::from([("/a", EMPTY)]),
        requested: Configurations::new(),
        compiled_base: false,
        complete_empty_base: true,
        cursor: None,
        pruning: false,
      },
      |_, _, _, _, _| {},
    );
  }
}

#[test]
fn retained_compiler_prefix_pruning_dependency_cursor_is_informational() {
  static DEPENDENCY: [u8; 64] = [0xB4; 64];
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    with_model_operation(
      algorithm,
      Model {
        base: Configurations::new(),
        requested: Configurations::new(),
        compiled_base: false,
        complete_empty_base: false,
        cursor: None,
        pruning: true,
      },
      false,
      |checkpoint| checkpoint.cursor = SemanticMutationCursorV1::DependencyID(&DEPENDENCY[..algorithm.hash_length()]),
      |_, _, _, _, _| {},
    );
  }
}

#[test]
fn retained_compiler_prefix_models_incremental_replacement_addition_and_deletion() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for cursor in [None, Some("/"), Some("/!before"), Some("/a"), Some("/z")] {
      with_model(
        algorithm,
        Model {
          base: Configurations::from([("/", EMPTY), ("/!before", EMPTY), ("/a", EMPTY)]),
          requested: Configurations::from([("/!before", CHANGED), ("/z", EMPTY)]),
          compiled_base: true,
          complete_empty_base: false,
          cursor,
          pruning: false,
        },
        |_, _, _, _, _| {},
      );
    }
  }
}

#[test]
fn retained_compiler_prefix_models_pruning_after_last_source() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for compiled_base in [false, true] {
      with_model(
        algorithm,
        Model {
          base: Configurations::from([("/", EMPTY), ("/a", EMPTY)]),
          requested: Configurations::from([("/z", CHANGED)]),
          compiled_base,
          complete_empty_base: false,
          cursor: None,
          pruning: true,
        },
        |_, _, _, _, _| {},
      );
    }
  }
}

#[test]
fn retained_compiler_prefix_continues_finishes_and_reopens_with_exact_final_owners() {
  use crate::engine::v4::semantic_catalog::{SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1};
  use crate::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_v1;
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let requested = Configurations::from([("/", EMPTY), ("/!before", CHANGED), ("/z", EMPTY)]);
    let (_directory, path, completed) = with_model_operation(
      algorithm,
      Model {
        base: Configurations::new(),
        requested: requested.clone(),
        compiled_base: false,
        complete_empty_base: false,
        cursor: Some("/"),
        pruning: false,
      },
      true,
      |_| {},
      |capture, checkpoint, memory, cancellation, publisher| {
        let admitted = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm)).unwrap();
        let (request, registry, progress) = admitted.into_catalog_parts();
        let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
        let mut store = NativeSemanticCatalogStagingStoreV1::new(
          &protection,
          [1; 16],
          publisher.observe().unwrap().selected.header.updated_at_ms + 1,
          cancellation,
        )
        .unwrap();
        let mut continuation =
          SemanticCatalogContinuationV1::from_progress(request, progress, &registry, &store, memory, &|| false).unwrap();
        for (&owner, &body) in requested.iter().filter(|(owner, _)| **owner != "/") {
          continuation = continuation
            .apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration(algorithm, &registry, memory, owner, body)), &mut store)
            .unwrap();
        }
        let continuation = continuation.finish_configurations(&mut store).unwrap();
        assert_eq!(continuation.pruning_candidates().record_count, 0);
        let completed = continuation.finish(&mut store).unwrap();
        completed.semantic_state().clone()
      },
    );
    let (_coordinator, publisher) = reopen(&path);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      [1; 16],
      publisher.observe().unwrap().selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let registry = crate::engine::v4::parser_registry_compiler::compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: None,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &NoAliases,
      &memory,
      &|| false,
    )
    .unwrap();
    let request = SemanticCatalogCompilationRequestV1 {
      hash_algorithm: algorithm,
      expected_configuration_count: 3,
      required_capabilities: [0; 32],
      maximum_workspace_bytes: 64 << 20,
    };
    let before = fs::read(&path).unwrap();
    let admitted = admit_semantic_catalog_v1(request, &completed.object_id, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(admitted.configuration_count(), 3);
    let state = decode_semantic_object(&completed.value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, .. } = state.availability else {
      panic!("finish must produce Complete")
    };
    let mut owners = Vec::new();
    SemanticCatalogReaderV1::new(algorithm, &store)
      .walk_catalog(
        &catalog_root,
        SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap(),
        &|| false,
        |record| {
          if record.record_kind == 1 {
            owners.push(record.owner_key.to_vec());
          }
          Ok(())
        },
      )
      .unwrap();
    owners.sort();
    let mut expected: Vec<_> = ["/.aeordb-config/indexes.json", "/!before/.aeordb-config/indexes.json", "/z/.aeordb-config/indexes.json"]
      .iter()
      .map(|path| [b"\x01\0".as_slice(), path.as_bytes()].concat())
      .collect();
    expected.sort();
    assert_eq!(owners, expected);
    assert_eq!(fs::read(&path).unwrap(), before, "reopened Complete admission and owner checks are read-only");
    // A separately rebuilt reverse-order stream must converge to the same
    // immutable state; the explicit owner oracle above does not use its output.
    let rebuilt = compile_semantic_catalog_v1(
      request,
      &registry,
      requested.iter().rev().map(|(&owner, &body)| Ok(configuration(algorithm, &registry, &memory, owner, body))),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(rebuilt.semantic_state(), &completed);
  }
}
