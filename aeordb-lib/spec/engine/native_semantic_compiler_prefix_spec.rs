//! Native characterization of independently admitted sources and catalogs.
//! The outer parent is retained source-union validation's native test module.
#[path = "native_semantic_compiler_prefix_alias_spec.rs"]
mod aliases;
#[path = "native_semantic_compiler_prefix_model_spec.rs"]
mod model;
use super::*;
use crate::engine::memory_coordinator::MemoryPolicy;
use crate::engine::v4::index_configuration_compiler::{
  IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use crate::engine::v4::parser_registry_compiler::{
  CompiledParserRegistryV1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
};
use crate::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogCompilationRequestV1, SemanticCatalogConfigurationMutationV1, SemanticCatalogContinuationV1,
  admit_semantic_catalog_progress_v1,
};
use crate::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;
use crate::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
use crate::engine::v4::semantic_mutation_control::{SemanticMutationCursorV1, SemanticMutationPhaseV1, encode_semantic_mutation_checkpoint};
use crate::engine::v4::system_family::embedded_system_family_registry;

struct NoAliases;

#[derive(Clone, Copy)]
enum FixtureCatalogChange {
  None,
  ExtraConfiguration,
  DamagedCatalog,
  DamagedDefinition,
}

fn stage_fixture_semantic_state(
  publisher: &V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  state: &crate::engine::v4::namespace::EncodedSemanticObjectV1,
) {
  use crate::engine::v4::semantic_catalog_compiler::SemanticCatalogStagingStoreV1;
  let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
  let mut store = NativeSemanticCatalogStagingStoreV1::new(
    &protection,
    [1; 16],
    publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    cancellation,
  )
  .unwrap();
  store.publish_semantic_objects(std::slice::from_ref(state)).unwrap();
}
impl ParserAliasSnapshotV1 for NoAliases {
  fn resolve_parser_alias(
    &self,
    _: &str,
  ) -> Result<Option<crate::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}
impl IndexConfigurationAliasSnapshotV1 for NoAliases {
  fn resolve_mapper_alias(
    &self,
    _: &str,
  ) -> Result<Option<crate::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}

fn with_prefix_fixture(
  algorithm: HashAlgorithm,
  wrong_projection: bool,
  inspect: impl FnOnce(
    &NativeSemanticMutationInventoryV1<'_>,
    &CompiledParserRegistryV1,
    &[u8],
    &NativeSemanticCatalogStagingStoreV1<'_>,
    &MemoryCoordinator,
    SemanticCatalogCompilationRequestV1,
  ),
) {
  with_adjusted_prefix_fixture(algorithm, wrong_projection, |_| {}, inspect);
}

fn with_adjusted_prefix_fixture(
  algorithm: HashAlgorithm,
  wrong_projection: bool,
  adjust: impl FnOnce(&mut crate::engine::v4::semantic_mutation_control::SemanticMutationCheckpointV1<'_>),
  inspect: impl FnOnce(
    &NativeSemanticMutationInventoryV1<'_>,
    &CompiledParserRegistryV1,
    &[u8],
    &NativeSemanticCatalogStagingStoreV1<'_>,
    &MemoryCoordinator,
    SemanticCatalogCompilationRequestV1,
  ),
) {
  with_changed_catalog_prefix_fixture(algorithm, wrong_projection, FixtureCatalogChange::None, adjust, inspect);
}

fn with_changed_catalog_prefix_fixture(
  algorithm: HashAlgorithm,
  wrong_projection: bool,
  change: FixtureCatalogChange,
  adjust: impl FnOnce(&mut crate::engine::v4::semantic_mutation_control::SemanticMutationCheckpointV1<'_>),
  inspect: impl FnOnce(
    &NativeSemanticMutationInventoryV1<'_>,
    &CompiledParserRegistryV1,
    &[u8],
    &NativeSemanticCatalogStagingStoreV1<'_>,
    &MemoryCoordinator,
    SemanticCatalogCompilationRequestV1,
  ),
) {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("compiler-prefix-source-binding", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  let body = br#"{"$v":1,"indexes":[]}"#;
  let (namespace, _) = namespace_configuration_tree(&publisher, "/!before", body, vec![]);
  let requested_tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("!before", namespace)]);
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", body)]);
  let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
  let base_sources = absent_globals();
  let mut requested_sources = base_sources.clone();
  requested_sources.insert(INDEX_SOURCE.to_string(), Some(revision));
  let mut fingerprint = base_sources.clone();
  fingerprint.insert("/!before/.aeordb-config/indexes.json".to_string(), None);

  let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let registry = crate::engine::v4::parser_registry_compiler::compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 32 << 20,
    },
    &NoAliases,
    &memory,
    &|| false,
  )
  .unwrap();
  let request = SemanticCatalogCompilationRequestV1 {
    hash_algorithm: algorithm,
    expected_configuration_count: 2,
    required_capabilities: [0; 32],
    maximum_workspace_bytes: 64 << 20,
  };
  let mut store = NativeSemanticCatalogStagingStoreV1::new(
    &protection,
    [1; 16],
    publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    &cancellation,
  )
  .unwrap();
  let configuration = compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: if wrong_projection { br#"{"$v":1,"glob":"*.other","indexes":[]}"# } else { body },
      owner_path: "/",
      registry: &registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 32 << 20,
    },
    &NoAliases,
    &memory,
    &|| false,
  )
  .unwrap();
  let mut work = SemanticCatalogContinuationV1::start(request, &registry, &mut store, &memory, &|| false)
    .unwrap()
    .apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration), &mut store)
    .unwrap();

  if matches!(change, FixtureCatalogChange::ExtraConfiguration) {
    let extra = compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source: body,
        owner_path: "/not-in-either-source",
        registry: &registry,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 32 << 20,
      },
      &NoAliases,
      &memory,
      &|| false,
    )
    .unwrap();
    work = work.apply(SemanticCatalogConfigurationMutationV1::Upsert(extra), &mut store).unwrap();
  }

  // Fixture-only assembly of matching immutable checkpoint/source-control
  // bytes. This is not evidence for production checkpoint publication.
  seed_validation_pair(&publisher, &base, &requested_tree, &base_sources, &requested_sources, &fingerprint, 2);
  let checkpoint = seed_prefix_checkpoint(&publisher, &work, SemanticMutationCursorV1::ConfigurationOwner("/"), adjust);
  let damaged = match change {
    FixtureCatalogChange::DamagedCatalog => Some(
      semantic_object_path(algorithm, if work.catalog().node_count == 1 { 2 } else { 3 }, work.catalog().root_object_id.unwrap()).unwrap(),
    ),
    FixtureCatalogChange::DamagedDefinition => Some(semantic_object_path(algorithm, 4, &registry.projection().object.object_id).unwrap()),
    _ => None,
  };
  drop(work);
  drop(protection);
  if let Some(path) = damaged {
    corrupt_last_entity_byte(&publisher, &first_authority_file_path_hash(&path, algorithm));
  }
  // Later current input must not replace the retained requested revision.
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"not the retained input")]);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let store = NativeSemanticCatalogStagingStoreV1::new(
    &protection,
    [1; 16],
    publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    &cancellation,
  )
  .unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  assert_eq!(
    capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&requested_tree)).unwrap().requested_configuration_count,
    2
  );
  inspect(&capture, &registry, &checkpoint, &store, &memory, request);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

fn seed_prefix_checkpoint(
  publisher: &V4FirstAuthorityPublisher,
  work: &SemanticCatalogContinuationV1<'_>,
  cursor: SemanticMutationCursorV1<'_>,
  adjust: impl FnOnce(&mut crate::engine::v4::semantic_mutation_control::SemanticMutationCheckpointV1<'_>),
) -> Vec<u8> {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let identity = checkpoint_identity();
  let (old_checkpoint, old_companion) = {
    let header = publisher.observe().unwrap().selected.header;
    let kv = publisher.lock_kv().unwrap();
    let load = |kind| load_immutable_system_control_file(&publisher.file, &*kv, &header, kind, &identity).unwrap().unwrap().bytes;
    (load(SystemControlKindV1::SemanticMutationCheckpoint), load(SystemControlKindV1::SemanticSourceCapture))
  };
  let system_registry = embedded_system_family_registry(algorithm).unwrap();
  let mut checkpoint = decode_semantic_mutation_checkpoint(&old_checkpoint, algorithm).unwrap();
  checkpoint.phase = work.phase();
  checkpoint.cursor = cursor;
  checkpoint.catalog_root = work.catalog().root_object_id;
  checkpoint.record_count = work.catalog().record_count;
  checkpoint.node_count = work.catalog().node_count;
  checkpoint.configuration_count = work.configuration_count();
  checkpoint.dependency_count = work.dependency_count();
  checkpoint.pruning_catalog_root = work.pruning_candidates().root_object_id;
  checkpoint.pruning_record_count = work.pruning_candidates().record_count;
  checkpoint.pruning_node_count = work.pruning_candidates().node_count;
  checkpoint.compiler_fingerprint = semantic_compiler_fingerprint_v1(algorithm);
  checkpoint.semantic_registry_fingerprint = &system_registry.semantic_projection_fingerprint;
  adjust(&mut checkpoint);
  let checkpoint = encode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap();
  let hash = digest_parts(algorithm, &[&checkpoint]);
  let old = crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1(&old_companion, algorithm).unwrap();
  let companion = encode_semantic_source_capture_v1(&SemanticSourceCaptureV1 { checkpoint_payload_hash: &hash, ..old }, algorithm).unwrap();
  seed(
    publisher,
    &[
      (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
      (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
    ],
  );
  checkpoint
}

#[test]
fn independent_source_and_catalog_admission_do_not_establish_a_compiler_prefix() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for wrong_projection in [false, true] {
      with_prefix_fixture(algorithm, wrong_projection, |_, registry, checkpoint, store, memory, request| {
        let admitted = admit_semantic_catalog_progress_v1(request, checkpoint, registry, store, memory, &|| false).unwrap();
        assert_eq!(admitted.configuration_count(), 1);
        assert_eq!(admitted.phase(), SemanticMutationPhaseV1::Compiling);
      });
    }
  }
}

fn prefix_bounds(checkpoint: &[u8], algorithm: HashAlgorithm) -> NativeSemanticCompilerProgressBoundsV1 {
  let checkpoint = decode_semantic_mutation_checkpoint(checkpoint, algorithm).unwrap();
  NativeSemanticCompilerProgressBoundsV1 {
    sources: validation_bounds(checkpoint.staged_directory_root),
    maximum_compiler_workspace_bytes: 64 << 20,
    maximum_alias_snapshot_bytes: 4 << 20,
    maximum_semantic_decode_workspace_bytes: 32 << 20,
  }
}

#[test]
fn retained_compiler_prefix_admits_global_first_after_reopen_and_current_input_change() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    with_prefix_fixture(algorithm, false, |capture, _, checkpoint, _, _, _| {
      let progress = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm)).unwrap();
      assert_eq!(progress.phase(), SemanticMutationPhaseV1::Compiling);
      assert_eq!(progress.configuration_count(), 1);
      assert_eq!(progress.construction_mode(), SemanticCompilerConstructionModeV1::Fresh);
      assert_eq!(progress.sources().requested_configuration_count, 2);
    });
  }
}

#[test]
fn retained_compiler_prefix_refuses_same_count_wrong_configuration_projection() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    with_prefix_fixture(algorithm, true, |capture, _, checkpoint, _, _, _| {
      let result = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm));
      match result {
        Ok(_) => panic!("wrong retained-source projection admitted"),
        Err(error) => assert!(error.to_string().contains("semantic_compiler_prefix_projection"), "{error}"),
      }
    });
  }
}

#[test]
fn retained_compiler_prefix_returns_consumable_existing_catalog_progress() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_prefix_fixture(algorithm, false, |capture, _, checkpoint, store, memory, _| {
      let admitted = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm)).unwrap();
      let (request, registry, progress) = admitted.into_catalog_parts();
      let continuation = SemanticCatalogContinuationV1::from_progress(request, progress, &registry, store, memory, &|| false).unwrap();
      assert_eq!(continuation.configuration_count(), 1);
      assert_eq!(continuation.phase(), SemanticMutationPhaseV1::Compiling);
    });
  }
}

#[test]
fn retained_compiler_prefix_rejects_wrong_or_unknown_configuration_cursor() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for cursor in [
      SemanticMutationCursorV1::None,
      SemanticMutationCursorV1::ConfigurationOwner("/!before"),
      SemanticMutationCursorV1::ConfigurationOwner("/missing"),
    ] {
      with_adjusted_prefix_fixture(
        algorithm,
        false,
        |checkpoint| checkpoint.cursor = cursor,
        |capture, _, checkpoint, _, _, _| {
          let error = capture
            .admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm))
            .err()
            .expect("wrong source position must refuse");
          let message = error.to_string();
          assert!(
            message.contains("semantic_compiler_prefix_projection") || message.contains("semantic_compiler_prefix_cursor"),
            "{message}"
          );
        },
      );
    }
  }
}

#[test]
fn retained_compiler_prefix_admits_only_compiling_or_pruning() {
  with_adjusted_prefix_fixture(
    HashAlgorithm::Blake3_256,
    false,
    |checkpoint| {
      checkpoint.phase = SemanticMutationPhaseV1::Captured;
      checkpoint.cursor = SemanticMutationCursorV1::None;
      checkpoint.catalog_root = None;
      checkpoint.record_count = 0;
      checkpoint.node_count = 0;
      checkpoint.configuration_count = 0;
      checkpoint.dependency_count = 0;
    },
    |capture, _, checkpoint, _, _, _| {
      let error = capture
        .admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, HashAlgorithm::Blake3_256))
        .err()
        .expect("Captured is not resumable catalog work");
      assert!(error.to_string().contains("semantic_catalog_progress_phase"), "{error}");
    },
  );
}

#[test]
fn retained_compiler_prefix_rejects_unsupported_checkpoint_profiles_and_missing_catalogs() {
  static DIFFERENT: [u8; 64] = [0xA7; 64];
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in 0..3 {
      with_adjusted_prefix_fixture(
        algorithm,
        false,
        |checkpoint| match case {
          0 => checkpoint.compiler_fingerprint = &DIFFERENT[..algorithm.hash_length()],
          1 => checkpoint.semantic_registry_fingerprint = &DIFFERENT[..algorithm.hash_length()],
          2 => checkpoint.catalog_root = Some(&DIFFERENT[..algorithm.hash_length()]),
          _ => unreachable!(),
        },
        |capture, _, checkpoint, _, memory, _| {
          let baseline = memory.snapshot().unwrap().reserved_bytes;
          let error = capture
            .admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm))
            .err()
            .expect("unusable persisted progress must refuse");
          let message = error.to_string();
          if case < 2 {
            assert!(message.contains("semantic_catalog_base_profile"), "{message}");
          } else {
            assert!(message.contains("semantic_catalog_missing"), "{message}");
          }
          assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        },
      );
    }
  }
}

#[test]
fn retained_compiler_prefix_rejects_extra_configuration_outside_both_source_sets() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_changed_catalog_prefix_fixture(
      algorithm,
      false,
      FixtureCatalogChange::ExtraConfiguration,
      |_| {},
      |capture, registry, checkpoint, store, memory, request| {
        let structurally_admitted = admit_semantic_catalog_progress_v1(request, checkpoint, registry, store, memory, &|| false).unwrap();
        assert_eq!(structurally_admitted.configuration_count(), 2);
        let error = capture
          .admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm))
          .err()
          .expect("the complete owner count must exclude bindings outside both retained source sets");
        assert!(error.to_string().contains("semantic_compiler_prefix_projection"), "{error}");
      },
    );
  }
}

#[test]
fn retained_compiler_prefix_preserves_stored_catalog_and_definition_read_failure_causes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for change in [FixtureCatalogChange::DamagedCatalog, FixtureCatalogChange::DamagedDefinition] {
      with_changed_catalog_prefix_fixture(
        algorithm,
        false,
        change,
        |_| {},
        |capture, _, checkpoint, _, _, _| {
          let error = capture
            .admit_captured_semantic_compiler_progress(&[2; 16], 1, prefix_bounds(checkpoint, algorithm))
            .err()
            .expect("damaged persisted objects must refuse without becoming absence");
          match error {
            NativeSemanticSourceUnionErrorV1::Source(source) => assert_eq!(source.code(), "integrity_hash_mismatch"),
            error => panic!("original captured read cause was replaced: {error:?}"),
          }
        },
      );
    }
  }
}
