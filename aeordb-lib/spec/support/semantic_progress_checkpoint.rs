//! Composition fixture only. Independent ASMC byte expectations live in the
//! compiler admission tests. These placeholder capture identities grant no
//! task selection, source-position, ownership or retention authority.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::namespace::{EncodedSemanticObjectV1, SemanticAvailabilityV1, decode_semantic_object};
use aeordb::engine::v4::semantic_catalog_mutation::SemanticCatalogSnapshotV1;
use aeordb::engine::v4::semantic_mutation_control::{
  SemanticMutationCheckpointV1, SemanticMutationCursorV1, SemanticMutationPhaseV1, encode_semantic_mutation_checkpoint,
};

pub(super) fn checkpoint(
  algorithm: HashAlgorithm,
  original: &EncodedSemanticObjectV1,
  catalog: SemanticCatalogSnapshotV1<'_>,
  candidates: SemanticCatalogSnapshotV1<'_>,
  dependencies: u64,
) -> Vec<u8> {
  let state = decode_semantic_object(&original.value, algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { compiler_fingerprint, semantic_registry_fingerprint, .. } = state.availability else {
    panic!("fixture needs a complete baseline");
  };
  let placeholder = vec![1; algorithm.hash_length()];
  encode_semantic_mutation_checkpoint(
    &SemanticMutationCheckpointV1 {
      database_id: &[1; 16],
      task_id: &[2; 16],
      checkpoint_sequence: 1,
      physical_instance_id: &[3; 16],
      writer_fence_epoch: 1,
      semantic_generation: 1,
      header_sequence: 1,
      captured_at_ms: 1,
      phase: SemanticMutationPhaseV1::Pruning,
      cursor: SemanticMutationCursorV1::None,
      expected_configuration_count: 0,
      configuration_count: 0,
      record_count: catalog.record_count,
      node_count: catalog.node_count,
      dependency_count: dependencies,
      mutation_count: 1,
      activation_generation: 0,
      pruning_record_count: candidates.record_count,
      pruning_node_count: candidates.node_count,
      base_namespace_root: &placeholder,
      staged_directory_root: &placeholder,
      catalog_root: catalog.root_object_id,
      pruning_catalog_root: candidates.root_object_id,
      semantic_state: None,
      candidate_namespace_root: None,
      compiler_fingerprint: &compiler_fingerprint,
      semantic_registry_fingerprint: &semantic_registry_fingerprint,
      source_identity_fingerprint: &placeholder,
    },
    algorithm,
  )
  .unwrap()
}
