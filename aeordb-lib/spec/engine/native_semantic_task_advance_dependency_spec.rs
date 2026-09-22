//! Removed global dependencies remain live in retained or reintroduced scopes.
use super::*;
use super::advance_order::NoAdvanceAliases;
use crate::engine::v4::index_configuration_compiler::{IndexConfigurationCompilationRequestV1, compile_index_configuration_v1};
use crate::engine::v4::parser_registry_compiler::{ParserRegistryCompilationRequestV1, compile_parser_registry_v1};
use crate::engine::v4::semantic_catalog_compiler::{SemanticCatalogCompilationRequestV1, compile_semantic_catalog_v1};
use crate::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;

fn shared_dependency_case(retained_scope: bool) {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-advance-shared-dependency", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  publisher.publish(&initial).unwrap();
  enable_node_staging(&publisher);
  seed_union_generation(&publisher);
  let body = br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#;
  seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", body)]);
  let (scope, _) = namespace_configuration_tree(&publisher, "/shared", body, vec![]);
  let requested_tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("shared", scope)]);
  let base_tree = if retained_scope { &requested_tree } else { &initial.namespace_tree.root_hash };
  let memory = MemoryCoordinator::new(MemoryPolicy::new(768 << 20, 1024 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let registry = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &NoAdvanceAliases,
    &memory,
    &|| false,
  )
  .unwrap();
  let configuration = |owner| {
    compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source: body,
        owner_path: owner,
        registry: &registry,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      },
      &NoAdvanceAliases,
      &memory,
      &|| false,
    )
  };
  let base_owners: &[&str] = if retained_scope { &["/", "/shared"] } else { &["/"] };
  let state = {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      [1; 16],
      publisher.observe().unwrap().selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    compile_semantic_catalog_v1(
      SemanticCatalogCompilationRequestV1 {
        hash_algorithm: algorithm,
        expected_configuration_count: base_owners.len() as u64,
        required_capabilities: [0; 32],
        maximum_workspace_bytes: 128 << 20,
      },
      &registry,
      base_owners.iter().map(|owner| configuration(owner)),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap()
    .semantic_state()
    .clone()
  };
  let mut next = successor_request(&publisher, 0x79, "shared-dependency-base");
  next.semantic_state = state;
  next.namespace_tree = PreparedNamespaceTreeV0 {
    root_hash: base_tree.clone(),
    stored_value: publisher.load_immutable_entity_bounded(base_tree, 1 << 20).unwrap().unwrap().stored_value,
  };
  let head = publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash;
  let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let staged = capture
      .prepare_and_stage_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &head,
          requested_directory_root: &requested_tree,
          replacements: &[NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: None }],
          workspace_parent: workspace.path(),
          bounds: union_bounds(&requested_tree),
        },
        staging_request(),
      )
      .unwrap();
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    staged.select_initial_task(selection_request(checkpoint), &mut retirement).unwrap();
  }
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let observe = || {
    publisher
      .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
        database_id: &[1; 16],
        task_id: &[2; 16],
        memory: &memory,
        cancellation: &cancellation,
      })
      .unwrap()
  };
  {
    let observed = observe();
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
    let mut start = start_request(&requested_tree, input.publication_timestamp_ms + 20);
    start.compiler_bounds.maximum_compiler_workspace_bytes = 128 << 20;
    work.start_compilation(start, &mut retirement).unwrap();
  }
  let mut output = None;
  for batch in 0..2 {
    let observed = observe();
    let input = NativeSemanticTaskWorkRequestV1 {
      monotonic_now_ms: 40_000 + batch * 20_000,
      ..work_request(observed.header().selected.header.updated_at_ms + 1)
    };
    let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
    let mut request = advance_request(&requested_tree, input.publication_timestamp_ms + 20);
    request.monotonic_now_ms = input.monotonic_now_ms + 10_000;
    request.compiler_bounds.maximum_compiler_workspace_bytes = 128 << 20;
    let result = work.advance_compilation(request, &mut retirement).unwrap();
    assert_eq!(result.configuration_steps, 1);
    assert_eq!(result.pruning_steps, 0, "dependencies must remain live or be reintroduced before pruning");
    let selected = observe();
    let checkpoint = selected.checkpoint().unwrap().unwrap();
    assert_eq!(checkpoint.dependency_count, 4);
    assert!(!selected.task().unwrap().unwrap().pins_released);
    if batch == 0 {
      assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Compiling);
      assert_eq!(checkpoint.configuration_count, u64::from(retained_scope));
      assert_eq!(checkpoint.pruning_record_count, 4);
      assert!(checkpoint.pruning_catalog_root.is_some());
      assert_eq!(checkpoint.cursor, crate::engine::v4::semantic_mutation_control::SemanticMutationCursorV1::ConfigurationOwner("/"));
    } else {
      assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Ready);
      assert_eq!(checkpoint.configuration_count, 1);
      assert!(checkpoint.pruning_catalog_root.is_none());
      output = Some(publisher.load_semantic_object(1, checkpoint.semantic_state.unwrap()).unwrap().unwrap());
    }
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, head);
  }
  let mut store = NativeSemanticCatalogStagingStoreV1::new(
    &protection,
    [1; 16],
    publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    &cancellation,
  )
  .unwrap();
  let expected = compile_semantic_catalog_v1(
    SemanticCatalogCompilationRequestV1 {
      hash_algorithm: algorithm,
      expected_configuration_count: 1,
      required_capabilities: [0; 32],
      maximum_workspace_bytes: 128 << 20,
    },
    &registry,
    [configuration("/shared")],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap();
  assert_eq!(output.unwrap(), expected.semantic_state().value);
  drop(expected);
  drop(protection);
  drop(registry);
  drop(retirement);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn native_task_advance_keeps_dependencies_shared_by_an_unchanged_scope() {
  shared_dependency_case(true);
}

#[test]
fn native_task_advance_removal_then_reintroduction_keeps_live_dependencies() {
  shared_dependency_case(false);
}
