//! Actual partial-catalog persistence without selecting a task or HEAD.
#[path = "semantic_catalog_continuation_native_spec.rs"]
mod continuation;
use super::*;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::index_configuration_compiler::{
  IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use aeordb::engine::v4::parser_registry_compiler::{
  ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
};
use aeordb::engine::v4::namespace::decode_semantic_object;
use aeordb::engine::v4::semantic_catalog::{SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1, SemanticCatalogReadErrorClassV1};
use aeordb::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogCompilationErrorV1, SemanticCatalogCompilationRequestV1, SemanticCatalogStagingStoreV1, compile_semantic_catalog_v1,
  admit_semantic_catalog_progress_v1,
};
use aeordb::engine::v4::semantic_catalog_mutation::{
  SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1, plan_semantic_catalog_mutation_v1,
};
use aeordb::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;
use tokio_util::sync::CancellationToken;
#[path = "../support/semantic_progress_checkpoint.rs"]
mod input;

struct Snapshot;
impl ParserAliasSnapshotV1 for Snapshot {
  fn resolve_parser_alias(
    &self,
    _: &str,
  ) -> Result<Option<aeordb::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}
impl IndexConfigurationAliasSnapshotV1 for Snapshot {
  fn resolve_mapper_alias(
    &self,
    _: &str,
  ) -> Result<Option<aeordb::engine::v4::dependency::DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}

#[derive(Default)]
struct Catalog {
  root: Option<Vec<u8>>,
  records: u64,
  nodes: u64,
}
impl Catalog {
  fn assert_snapshot(&self, actual: SemanticCatalogSnapshotV1<'_>) {
    assert_eq!(actual.root_object_id, self.root.as_deref());
    assert_eq!((actual.record_count, actual.node_count), (self.records, self.nodes));
  }
  fn snapshot(&self) -> SemanticCatalogSnapshotV1<'_> {
    SemanticCatalogSnapshotV1 { root_object_id: self.root.as_deref(), record_count: self.records, node_count: self.nodes }
  }
  fn change(
    &mut self,
    algorithm: HashAlgorithm,
    mutation: SemanticCatalogMutationV1<'_>,
    store: &mut NativeSemanticCatalogStagingStoreV1<'_>,
    memory: &MemoryCoordinator,
  ) {
    let plan = plan_semantic_catalog_mutation_v1(
      SemanticCatalogMutationRequestV1 {
        hash_algorithm: algorithm,
        snapshot: self.snapshot(),
        mutation,
        maximum_workspace_bytes: 32 << 20,
      },
      store,
      memory,
      &|| false,
    )
    .unwrap();
    store.publish_semantic_objects(plan.objects()).unwrap();
    self.root = plan.root_object_id().map(<[u8]>::to_vec);
    self.records = plan.record_count();
    self.nodes = plan.node_count();
  }
}

#[test]
fn unused_dependency_progress_reopens_reads_only_and_retries_at_every_hash() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    drop(coordinator);
    let path = directory.path().join("migration-execution.aeordb");
    let initial = publisher.observe().unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 32 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: None,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &Snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let request = SemanticCatalogCompilationRequestV1 {
      hash_algorithm: algorithm,
      expected_configuration_count: 1,
      required_capabilities: [0; 32],
      maximum_workspace_bytes: 64 << 20,
    };
    let configuration = compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source: br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#,
        owner_path: "/",
        registry: &registry,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      },
      &Snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      initial.selected.header.database_id,
      initial.selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let original = compile_semantic_catalog_v1(request, &registry, [Ok(configuration)], &mut store, &memory, &|| false)
      .unwrap()
      .semantic_state()
      .clone();
    let state = decode_semantic_object(&original.value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, dependency_count, .. } =
      state.availability
    else {
      panic!("complete");
    };
    let mut main = Catalog { root: Some(catalog_root), records: catalog_record_count, nodes: catalog_node_count };
    let mut candidates = Catalog::default();
    // This small fixture has nine records. No production whole-catalog vector
    // is introduced: production admission uses streamed traversal plus bitmap.
    let mut bindings = Vec::new();
    SemanticCatalogReaderV1::new(algorithm, &store)
      .walk_catalog(
        main.root.as_deref().unwrap(),
        SemanticCatalogTraversalBoundsV1::new(main.records, main.nodes).unwrap(),
        &|| false,
        |record| {
          bindings.push((record.record_kind, record.owner_key.to_vec(), record.semantic_id.to_vec(), record.definition_object_id.to_vec()));
          Ok(())
        },
      )
      .unwrap();
    assert_eq!(bindings.len(), 9);
    for (kind, owner, semantic, definition) in &bindings {
      if matches!(kind, 6 | 7) {
        candidates.change(
          algorithm,
          SemanticCatalogMutationV1::Upsert(SemanticCatalogRecordV1 {
            record_kind: *kind,
            owner_key: owner,
            semantic_id: semantic,
            definition_object_id: definition,
          }),
          &mut store,
          &memory,
        );
      } else if *kind != 2 {
        main.change(algorithm, SemanticCatalogMutationV1::Remove { record_kind: *kind, owner_key: owner }, &mut store, &memory);
      }
    }
    assert_eq!(candidates.records, dependency_count);
    assert!(candidates.records > 0);
    assert_eq!(main.records, candidates.records + 1);
    let bytes = input::checkpoint(algorithm, &original, main.snapshot(), candidates.snapshot(), dependency_count);
    let staged = publisher.observe().unwrap();
    assert_eq!(staged.selected.header.head_hash, initial.selected.header.head_hash);
    drop(protection);
    drop(publisher);
    let reopened = V4FirstAuthorityPublisher::open(&path).unwrap();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let source = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      initial.selected.header.database_id,
      staged.selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let request = SemanticCatalogCompilationRequestV1 { expected_configuration_count: 0, ..request };
    for mode in 0..4 {
      let mut checkpoint = bytes.clone();
      if mode == 1 {
        checkpoint[32 + 168 + 3 * algorithm.hash_length()] ^= 0x80;
        let end = checkpoint.len() - 4;
        let crc = crc32fast::hash(&checkpoint[..end]);
        checkpoint[end..].copy_from_slice(&crc.to_le_bytes());
      }
      let limited = SemanticCatalogCompilationRequestV1 {
        maximum_workspace_bytes: if mode == 3 { 0 } else { request.maximum_workspace_bytes },
        ..request
      };
      let result = admit_semantic_catalog_progress_v1(limited, &checkpoint, &registry, &source, &memory, &|| mode == 2);
      if mode == 0 {
        let progress = result.unwrap();
        main.assert_snapshot(progress.catalog());
        candidates.assert_snapshot(progress.pruning_candidates());
        assert_eq!(progress.dependency_count(), dependency_count);
        assert_eq!(progress.configuration_count(), 0);
      } else {
        let error = match result {
          Ok(_) => panic!("invalid progress admitted"),
          Err(error) => error,
        };
        let expected = match mode {
          1 => SemanticCatalogReadErrorClassV1::Corrupt,
          2 => SemanticCatalogReadErrorClassV1::Cancelled,
          _ => SemanticCatalogReadErrorClassV1::ResourceLimit,
        };
        assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == expected));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      let retry = admit_semantic_catalog_progress_v1(request, &bytes, &registry, &source, &memory, &|| false).unwrap();
      main.assert_snapshot(retry.catalog());
      drop(retry);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      assert_eq!(std::fs::read(&path).unwrap(), before);
      assert_eq!(reopened.observe().unwrap(), staged);
    }
    drop(protection);
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
