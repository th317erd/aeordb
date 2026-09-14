use super::measure;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::DependencyRecordV1;
use aeordb::engine::v4::namespace::EncodedSemanticObjectV1;
use aeordb::engine::v4::parser_registry_compiler::{
  ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
};
use aeordb::engine::v4::semantic_catalog::{SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1, SemanticCatalogReadErrorV1};
use aeordb::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogCompilationErrorV1, SemanticCatalogCompilationRequestV1, SemanticCatalogStagingStoreV1, compile_semantic_catalog_v1,
};

struct NoAliases;
impl ParserAliasSnapshotV1 for NoAliases {
  fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    panic!("empty registry must not resolve aliases")
  }
}
struct NoStorage;
impl SemanticCatalogObjectSourceV1 for NoStorage {
  fn load_semantic_object(&self, _: u16, _: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    panic!("allocation refusal must precede reads")
  }
}
impl SemanticCatalogStagingStoreV1 for NoStorage {
  fn publish_semantic_objects(&mut self, _: &[EncodedSemanticObjectV1]) -> Result<(), SemanticCatalogReadErrorV1> {
    panic!("allocation refusal must precede writes")
  }
}

#[test]
fn actual_definition_wrapper_allocation_refusal_is_recoverable_before_catalog_storage_access() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 8 << 20).unwrap());
    let registry = compile_parser_registry_v1(
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
    let before = memory.snapshot().unwrap().reserved_bytes;
    let requested = registry.projection().object.value.len();
    let (result, allocations) = measure(requested, || {
      compile_semantic_catalog_v1(
        SemanticCatalogCompilationRequestV1 {
          hash_algorithm: algorithm,
          expected_configuration_count: 0,
          required_capabilities: [0; 32],
          maximum_workspace_bytes: 64 << 20,
        },
        &registry,
        std::iter::empty(),
        &mut NoStorage,
        &memory,
        &|| false,
      )
    });
    assert!(allocations.injected_failure, "wrapper allocation was not exercised");
    let error = match result {
      Err(error) => error,
      Ok(_) => panic!("refused wrapper produced a candidate"),
    };
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
  }
}
