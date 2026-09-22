//! Real staged source modes for first compiler selection qualification.
use super::*;
use crate::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_semantic_object, encode_semantic_state_object};
use crate::engine::v4::index_configuration_compiler::{
  IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use crate::engine::v4::parser_registry_compiler::{
  ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
};
use crate::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogCompilationRequestV1, compile_semantic_catalog_v1, SemanticCatalogStagingStoreV1,
};
use crate::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;
use crate::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
use crate::engine::v4::system_family::embedded_system_family_registry;

struct NoModeAliases;
impl ParserAliasSnapshotV1 for NoModeAliases {
  fn resolve_parser_alias(
    &self,
    _: &str,
  ) -> Result<Option<crate::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}
impl IndexConfigurationAliasSnapshotV1 for NoModeAliases {
  fn resolve_mapper_alias(
    &self,
    _: &str,
  ) -> Result<Option<crate::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum BaseMode {
  ContentOnly,
  CompleteEmpty,
  Incremental,
  IncrementalRemoval,
  IncrementalPruning,
  ChangedRegistry,
  EmptyWithConfiguration,
  UnsupportedProfile,
}

fn compiler_mode_case(algorithm: HashAlgorithm, mode: BaseMode) {
  let _ = compiler_mode_case_with_inspect(algorithm, mode, |_| {});
}

pub(super) fn compiler_mode_case_with_inspect(
  algorithm: HashAlgorithm,
  mode: BaseMode,
  inspect: impl FnOnce(TaskWorkFixture<'_>),
) -> (tempfile::TempDir, PathBuf) {
  let (directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-work-compiler-mode", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let mut root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  enable_node_staging(&publisher);
  seed_union_generation(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(768 << 20, 1024 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let configuration_body: &[u8] = if matches!(mode, BaseMode::IncrementalPruning) {
    br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#
  } else {
    br#"{"$v":1,"indexes":[]}"#
  };
  let configured = !matches!(mode, BaseMode::CompleteEmpty);
  if configured {
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", configuration_body)]);
  }
  let complete = !matches!(mode, BaseMode::ContentOnly);
  if complete {
    let state = if matches!(mode, BaseMode::CompleteEmpty | BaseMode::EmptyWithConfiguration) {
      encode_semantic_state_object(
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
      .unwrap()
    } else {
      let registry = compile_parser_registry_v1(
        ParserRegistryCompilationRequestV1 {
          source: None,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 64 << 20,
        },
        &NoModeAliases,
        &memory,
        &|| false,
      )
      .unwrap();
      let configuration = compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: configuration_body,
          owner_path: "/",
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 64 << 20,
        },
        &NoModeAliases,
        &memory,
        &|| false,
      )
      .unwrap();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let mut store = NativeSemanticCatalogStagingStoreV1::new(
        &protection,
        [1; 16],
        publisher.observe().unwrap().selected.header.updated_at_ms + 1,
        &cancellation,
      )
      .unwrap();
      let previous = compile_semantic_catalog_v1(
        SemanticCatalogCompilationRequestV1 {
          hash_algorithm: algorithm,
          expected_configuration_count: 1,
          required_capabilities: [0; 32],
          maximum_workspace_bytes: 64 << 20,
        },
        &registry,
        [Ok(configuration)],
        &mut store,
        &memory,
        &|| false,
      )
      .unwrap();
      previous.semantic_state().clone()
    };
    let state = if matches!(mode, BaseMode::UnsupportedProfile) {
      let mut decoded = decode_semantic_object(&state.value, algorithm).unwrap().semantic_state.unwrap();
      match &mut decoded.availability {
        SemanticAvailabilityV1::Complete { compiler_fingerprint, .. } => compiler_fingerprint.fill(0x42),
        _ => unreachable!(),
      }
      encode_semantic_state_object(
        &SemanticStateWriteV1 { required_capabilities: decoded.required_capabilities, availability: decoded.availability },
        algorithm,
      )
      .unwrap()
    } else {
      state
    };
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let mut store = NativeSemanticCatalogStagingStoreV1::new(
        &protection,
        [1; 16],
        publisher.observe().unwrap().selected.header.updated_at_ms + 1,
        &cancellation,
      )
      .unwrap();
      store.publish_semantic_objects(std::slice::from_ref(&state)).unwrap();
    }
    let mut next = successor_request(&publisher, 0x79, "compiler-mode-base");
    next.semantic_state = state;
    next.namespace_tree = initial.namespace_tree.clone();
    root = publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash;
  }
  let mut requested_parser = None;
  if matches!(mode, BaseMode::ChangedRegistry) {
    let module = plugin_fixtures::module("both");
    let alias = plugin_fixtures::alias(&module, "both");
    let parsers = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
    seed_files(
      &publisher,
      &[
        (plugin_fixtures::artifact_path(&module), "application/wasm", &module),
        (plugin_fixtures::alias_path(), "application/octet-stream", &alias),
        (PARSER_SOURCE.to_owned(), "application/json", parsers),
      ],
    );
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = capture.read_protected_source(PARSER_SOURCE, source_bounds()).unwrap().unwrap();
      requested_parser = Some(source.revision().to_vec());
      source.stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1).unwrap();
    }
    let key = first_authority_file_path_hash(PARSER_SOURCE, algorithm);
    assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
    seed_files(&publisher, &[]);
  }
  let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut replacements = requested_parser
      .as_ref()
      .map(|revision| NativeSemanticSourceReplacementV1 { path: PARSER_SOURCE, file_record_id: Some(revision) })
      .into_iter()
      .collect::<Vec<_>>();
    if matches!(mode, BaseMode::IncrementalRemoval | BaseMode::IncrementalPruning) {
      replacements.push(NativeSemanticSourceReplacementV1 { path: INDEX_SOURCE, file_record_id: None });
    }
    let staged = capture
      .prepare_and_stage_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &root,
          requested_directory_root: &initial.namespace_tree.root_hash,
          replacements: &replacements,
          workspace_parent: workspace.path(),
          bounds: union_bounds(&initial.namespace_tree.root_hash),
        },
        staging_request(),
      )
      .unwrap();
    let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
    staged.stage_initial_checkpoint(checkpoint).unwrap();
    staged.select_initial_task(selection_request(checkpoint), &mut retirement).unwrap();
  }
  {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
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
    let mut start = start_request(&initial.namespace_tree.root_hash, input.publication_timestamp_ms + 20);
    start.compiler_bounds.maximum_compiler_workspace_bytes = 128 << 20;
    let before = fs::read(&path).unwrap();
    let result = work.start_compilation(start, &mut retirement);
    if matches!(mode, BaseMode::EmptyWithConfiguration | BaseMode::UnsupportedProfile) {
      let error = result.unwrap_err();
      assert!(error.committed_receipt().is_none());
      let expected = if matches!(mode, BaseMode::EmptyWithConfiguration) {
        "semantic_compiler_prefix_empty_base"
      } else {
        "semantic_catalog_base_profile"
      };
      assert!(error.to_string().contains(expected), "{error}");
      assert_eq!(fs::read(&path).unwrap(), before);
    } else {
      assert_eq!(result.unwrap().control_sequence, 3);
      let capture = protection.capture_semantic_mutation_inventory(work_request(1).inventory_bounds, &memory, &cancellation).unwrap();
      let progress = capture.admit_captured_semantic_compiler_progress(&[2; 16], 2, start.compiler_bounds).unwrap();
      let incremental = matches!(mode, BaseMode::Incremental | BaseMode::IncrementalRemoval | BaseMode::IncrementalPruning);
      assert_eq!(
        progress.construction_mode(),
        if incremental { SemanticCompilerConstructionModeV1::Incremental } else { SemanticCompilerConstructionModeV1::Fresh }
      );
      assert_eq!(progress.configuration_count(), u64::from(incremental));
      if matches!(mode, BaseMode::IncrementalRemoval | BaseMode::IncrementalPruning) {
        assert_eq!(progress.sources().requested_configuration_count, 0);
        assert_eq!(progress.sources().base_configuration_count, 1);
      }
    }
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
  }
  inspect(TaskWorkFixture {
    publisher: &publisher,
    memory: &memory,
    cancellation: &cancellation,
    path: &path,
    tree: &initial.namespace_tree.root_hash,
    retirement: &mut retirement,
  });
  drop(retirement);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  (directory, path)
}

#[test]
fn native_task_work_compiler_content_only_starts_fresh_from_real_sources() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    compiler_mode_case(algorithm, BaseMode::ContentOnly);
  }
}

#[test]
fn native_task_work_compiler_complete_empty_starts_fresh() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    compiler_mode_case(algorithm, BaseMode::CompleteEmpty);
  }
}

#[test]
fn native_task_work_compiler_nonempty_complete_starts_incrementally() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    compiler_mode_case(algorithm, BaseMode::Incremental);
  }
}

#[test]
fn native_task_work_compiler_incremental_start_retains_configs_pending_removal() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    compiler_mode_case(algorithm, BaseMode::IncrementalRemoval);
  }
}

#[test]
fn native_task_work_compiler_changed_registry_starts_fresh_after_base_admission() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    compiler_mode_case(algorithm, BaseMode::ChangedRegistry);
  }
}

#[test]
fn native_task_work_compiler_complete_empty_cannot_hide_configurations() {
  compiler_mode_case(HashAlgorithm::Blake3_256, BaseMode::EmptyWithConfiguration);
}

#[test]
fn native_task_work_compiler_unsupported_complete_base_cannot_fall_back_to_fresh() {
  compiler_mode_case(HashAlgorithm::Blake3_256, BaseMode::UnsupportedProfile);
}
