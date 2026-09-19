//! Physical reader fixtures. Ordered expected rows are independent of traversal.
#[path = "native_source_catalog_build_spec.rs"]
mod assembly;
#[path = "native_source_physical_entries_spec.rs"]
mod physical_entries;
#[path = "native_retained_alias_snapshot_spec.rs"]
mod retained_alias_snapshot;
use super::*;
#[path = "native_source_catalog_resource_spec.rs"]
mod resource_spec;
#[path = "native_source_catalog_validation_spec.rs"]
mod validation_spec;
use crate::engine::v4::semantic_source_capture::{
  SemanticSourceCaptureV1, SemanticSourceLeafEntryV1, SemanticSourceChildV1, encode_semantic_source_capture_v1,
  encode_semantic_source_leaf_v1, encode_semantic_source_internal_v1, decode_semantic_source_capture_binding_v1,
};
use crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_checkpoint;

const PARSER_SOURCE: &str = "/.aeordb-config/parsers.json";
const ALIAS_SOURCE: &str = "/.aeordb-system/plugin-aliases/example";

fn catalog_bounds() -> NativeSemanticSourceCatalogBoundsV1 {
  NativeSemanticSourceCatalogBoundsV1 {
    maximum_depth: 8,
    maximum_work: 4096,
    maximum_read_bytes: 64 << 20,
    maximum_source_bytes: 1 << 20,
    maximum_chunk_entity_bytes: 2 << 20,
    maximum_source_chunks: 1024,
  }
}

fn seed_catalog_node(publisher: &V4FirstAuthorityPublisher, bytes: &[u8]) -> Vec<u8> {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let id = decode_system_control(bytes, algorithm).unwrap().identity;
  seed(publisher, &[(SystemControlKindV1::SemanticSourceNode, &id, SystemControlSlotV1::Immutable, bytes)]);
  id
}

fn seed_catalog_tree(publisher: &V4FirstAuthorityPublisher, rows: &[(&str, Option<&[u8]>)], split: bool) -> (Vec<u8>, u64) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let entries: Vec<_> =
    rows.iter().map(|(path, file_record_id)| SemanticSourceLeafEntryV1 { path, file_record_id: *file_record_id }).collect();
  if !split {
    return (seed_catalog_node(publisher, &encode_semantic_source_leaf_v1(&[1; 16], &entries, algorithm).unwrap()), 1);
  }
  let left = seed_catalog_node(publisher, &encode_semantic_source_leaf_v1(&[1; 16], &entries[..1], algorithm).unwrap());
  let right = seed_catalog_node(publisher, &encode_semantic_source_leaf_v1(&[1; 16], &entries[1..], algorithm).unwrap());
  let root = encode_semantic_source_internal_v1(
    &[1; 16],
    &[SemanticSourceChildV1 { separator: None, node_id: &left }, SemanticSourceChildV1 { separator: Some(rows[1].0), node_id: &right }],
    algorithm,
  )
  .unwrap();
  (seed_catalog_node(publisher, &root), 3)
}

fn seed_catalog_pair(
  publisher: &V4FirstAuthorityPublisher,
  base: &[(&str, Option<&[u8]>)],
  requested: &[(&str, Option<&[u8]>)],
) -> Vec<u8> {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let (base_root, base_nodes) = seed_catalog_tree(publisher, base, false);
  let (requested_root, requested_nodes) = seed_catalog_tree(publisher, requested, true);
  let checkpoint = frozen(algorithm, "checkpoint");
  let decoded = decode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap();
  let checkpoint_hash = digest_parts(algorithm, &[&checkpoint]);
  let capture = encode_semantic_source_capture_v1(
    &SemanticSourceCaptureV1 {
      database_id: decoded.database_id,
      task_id: decoded.task_id,
      checkpoint_sequence: decoded.checkpoint_sequence,
      physical_instance_id: decoded.physical_instance_id,
      writer_fence_epoch: decoded.writer_fence_epoch,
      semantic_generation: decoded.semantic_generation,
      header_sequence: decoded.header_sequence,
      captured_at_ms: decoded.captured_at_ms,
      protected_path_count: base.len() as u64,
      base_catalog_node_count: base_nodes,
      requested_catalog_node_count: requested_nodes,
      base_namespace_root: decoded.base_namespace_root,
      staged_directory_root: decoded.staged_directory_root,
      base_source_catalog: &base_root,
      requested_source_catalog: &requested_root,
      source_identity_fingerprint: decoded.source_identity_fingerprint,
      checkpoint_payload_hash: &checkpoint_hash,
    },
    algorithm,
  )
  .unwrap();
  decode_semantic_source_capture_binding_v1(&capture, &checkpoint, algorithm).unwrap();
  let identity = checkpoint_identity();
  seed(
    publisher,
    &[
      (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
      (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &capture),
    ],
  );
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 8;
  header.required_writer_capabilities[3] |= 8;
  write_redundant_header(publisher, &header);
  capture
}

#[test]
fn native_catalog_pairs_different_shapes_and_exact_old_new_sources_after_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-pair", None, [1; 16], algorithm, 0);
    seed_raw_source(&publisher, INDEX_SOURCE, 0, 1, 0, false, CompressionAlgorithm::Zstd);
    let old = seed_retained_revision(&publisher, INDEX_SOURCE);
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"new captured input")]);
    let new = seed_retained_revision(&publisher, INDEX_SOURCE);
    seed_catalog_pair(
      &publisher,
      &[(INDEX_SOURCE, Some(&old)), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)],
      &[(INDEX_SOURCE, Some(&new)), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)],
    );
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"unrelated current input")]);
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut rows = Vec::new();
    let summary = captured
      .visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |path, base, requested| {
        assert!(reopened.root_state.try_lock().is_ok());
        assert!(reopened.kv.try_lock().is_ok());
        rows.push((path.to_string(), base.map(|source| source.body().to_vec()), requested.map(|source| source.body().to_vec())));
        Ok(true)
      })
      .unwrap();
    assert_eq!(summary, SemanticSourceCatalogSummaryV1 { paths: 3, base_nodes: 1, requested_nodes: 3, complete: true });
    assert_eq!(
      rows,
      vec![
        (INDEX_SOURCE.to_string(), Some(b"firstsecond".to_vec()), Some(b"new captured input".to_vec())),
        (PARSER_SOURCE.to_string(), None, None),
        (ALIAS_SOURCE.to_string(), None, None),
      ]
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_point_distinguishes_unlisted_absent_and_retained_present() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-catalog-point", None, [1; 16], algorithm, 0);
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 1, true, CompressionAlgorithm::None);
  let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
  let rows = [(INDEX_SOURCE, Some(revision.as_slice())), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  seed_catalog_pair(&publisher, &rows, &rows);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  for side in [SemanticSourceCatalogSideV1::Base, SemanticSourceCatalogSideV1::Requested] {
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let result = captured.read_captured_protected_source(&[2; 16], 1, side, INDEX_SOURCE, catalog_bounds()).unwrap();
    assert_eq!(result.disposition(), SemanticSourceLookupDispositionV1::Present);
    assert_eq!(result.source().expect("listed present retained input").body(), b"firstsecond");
    assert!(memory.snapshot().unwrap().reserved_bytes > baseline);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    for (path, disposition) in [
      (PARSER_SOURCE, SemanticSourceLookupDispositionV1::Absent),
      ("/.aeordb-system/plugin-aliases/unlisted", SemanticSourceLookupDispositionV1::Unlisted),
    ] {
      let result = captured.read_captured_protected_source(&[2; 16], 1, side, path, catalog_bounds()).unwrap();
      assert_eq!(result.disposition(), disposition);
      assert!(result.source().is_none());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_catalog_pair_rejects_a_different_last_path_even_with_matching_counts() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-catalog-path-mismatch", None, [1; 16], algorithm, 0);
  seed_catalog_pair(
    &publisher,
    &[(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)],
    &[(INDEX_SOURCE, None), (PARSER_SOURCE, None), ("/.aeordb-system/plugin-aliases/other", None)],
  );
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let error = captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap_err();
  assert_eq!(error.code(), "semantic_source_catalog_paths");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
