//! Literal source order plus separately executed one-shot catalog parity.
use super::*;
use crate::engine::v4::index_configuration_compiler::{
  IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use crate::engine::v4::parser_registry_compiler::{
  ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
};
use crate::engine::v4::semantic_catalog_compiler::{SemanticCatalogCompilationRequestV1, compile_semantic_catalog_v1};
use crate::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;

pub(super) struct NoAdvanceAliases;
impl ParserAliasSnapshotV1 for NoAdvanceAliases {
  fn resolve_parser_alias(
    &self,
    _: &str,
  ) -> Result<Option<crate::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}
impl IndexConfigurationAliasSnapshotV1 for NoAdvanceAliases {
  fn resolve_mapper_alias(
    &self,
    _: &str,
  ) -> Result<Option<crate::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}

#[test]
fn native_task_advance_preserves_file_path_order_across_bounded_batches() {
  // Explicit order, not sorted with the implementation's cursor comparator.
  // Global absence is still one visited position. /a-b sorts before /a's file.
  let owners = ["/", "/!before", "/a-b", "/a", "/a/child", "/a0", "/z"];
  let body = br#"{"$v":1,"indexes":[]}"#;
  for limit in [1, 2, 7] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, _path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-advance-source-order", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    enable_node_staging(&publisher);
    seed_union_generation(&publisher);
    let (nested, _) = namespace_configuration_tree(&publisher, "/a/child", body, vec![]);
    let (parent, _) = namespace_configuration_tree(&publisher, "/a", body, vec![namespace_directory_child("child", nested)]);
    let mut children = vec![namespace_directory_child("a", parent)];
    for owner in ["/!before", "/a-b", "/a0", "/z"] {
      let (tree, _) = namespace_configuration_tree(&publisher, owner, body, vec![]);
      children.push(namespace_directory_child(&owner[1..], tree));
    }
    let tree = publish_namespace_directory(&publisher, children);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(768 << 20, 1024 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let workspace = tempfile::tempdir().unwrap();
      let staged = capture
        .prepare_and_stage_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &tree,
            replacements: &[],
            workspace_parent: workspace.path(),
            bounds: union_bounds(&tree),
          },
          staging_request(),
        )
        .unwrap();
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      staged.select_initial_task(selection_request(checkpoint), &mut retirement).unwrap();
    }
    let generation = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    {
      let observed = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
      work.start_compilation(start_request(&tree, input.publication_timestamp_ms + 20), &mut retirement).unwrap();
    }
    let mut visited = 0usize;
    let mut state = None;
    for batch in 0..7 {
      let observed = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      let input = NativeSemanticTaskWorkRequestV1 {
        monotonic_now_ms: 40_000 + batch * 20_000,
        ..work_request(observed.header().selected.header.updated_at_ms + 1)
      };
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
      let request = NativeSemanticTaskCompilerAdvanceRequestV1 {
        maximum_configuration_steps: limit,
        monotonic_now_ms: input.monotonic_now_ms + 10_000,
        ..advance_request(&tree, input.publication_timestamp_ms + 20)
      };
      let (result, allocations) = measure(0, || work.advance_compilation(request, &mut retirement));
      let result = result.expect("saved source cursor must advance a whole bounded batch");
      assert!(!allocations.injected_failure);
      // Explicit small-fixture allocation budgets across batch sizes1/2/7.
      // Total is allocation traffic, not live memory or a large-source proof.
      assert!(allocations.maximum <= 16 << 20, "batch {batch}, limit {limit}: {allocations:?}");
      assert!(allocations.total <= 128 << 20, "batch {batch}, limit {limit}: {allocations:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      let steps = (limit as usize).min(owners.len() - visited);
      visited += steps;
      assert_eq!(result.configuration_steps, steps as u64);
      assert_eq!(result.pruning_steps, 0);
      assert_eq!(result.publication.control_sequence, 5 + 2 * batch);
      let selected = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      let checkpoint = selected.checkpoint().unwrap().unwrap();
      assert_eq!(checkpoint.configuration_count, (visited - 1) as u64);
      assert_eq!(checkpoint.expected_configuration_count, 6);
      assert!(!selected.task().unwrap().unwrap().pins_released);
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
      assert_eq!(
        publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap(),
        generation
      );
      if visited == owners.len() {
        assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Ready);
        assert_eq!(checkpoint.cursor, crate::engine::v4::semantic_mutation_control::SemanticMutationCursorV1::None);
        state = Some(publisher.load_semantic_object(1, checkpoint.semantic_state.unwrap()).unwrap().unwrap());
        break;
      }
      assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Compiling);
      assert_eq!(
        checkpoint.cursor,
        crate::engine::v4::semantic_mutation_control::SemanticMutationCursorV1::ConfigurationOwner(owners[visited - 1])
      );
    }
    assert_eq!(visited, owners.len());
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: None,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &NoAdvanceAliases,
      &memory,
      &|| false,
    )
    .unwrap();
    let configurations = owners[1..].iter().map(|owner| {
      compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: body,
          owner_path: owner,
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 64 << 20,
        },
        &NoAdvanceAliases,
        &memory,
        &|| false,
      )
    });
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      [1; 16],
      publisher.observe().unwrap().selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let compiled = compile_semantic_catalog_v1(
      SemanticCatalogCompilationRequestV1 {
        hash_algorithm: algorithm,
        expected_configuration_count: 6,
        required_capabilities: [0; 32],
        maximum_workspace_bytes: 64 << 20,
      },
      &registry,
      configurations,
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(state.unwrap(), compiled.semantic_state().value);
    drop(compiled);
    drop(registry);
    drop(protection);
    drop(retirement);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
