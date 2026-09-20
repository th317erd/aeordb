//! Exact retained-source counts and independent fingerprint preimages.
#[path = "native_semantic_source_union_validation_boundary_spec.rs"]
mod boundary;
#[path = "native_semantic_compiler_prefix_spec.rs"]
mod compiler_prefix;
use super::*;
use super::super::super::retained_source_spec::seed_retained_revision;
use crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_checkpoint;
use crate::engine::v4::semantic_source_capture::{
  SemanticSourceCaptureV1, SemanticSourceLeafEntryV1, encode_semantic_source_capture_v1, encode_semantic_source_leaf_v1,
};

pub(super) fn validation_bounds(tree: &[u8]) -> NativeSemanticSourceUnionValidationBoundsV1 {
  let prior = union_bounds(tree);
  NativeSemanticSourceUnionValidationBoundsV1 {
    catalog: NativeSemanticSourceCatalogBoundsV1 {
      maximum_depth: 8,
      maximum_work: 100_000,
      maximum_read_bytes: 64 << 20,
      maximum_source_bytes: 1 << 20,
      maximum_chunk_entity_bytes: 2 << 20,
      maximum_source_chunks: 1024,
    },
    namespace: prior.namespace,
    maximum_plugin_module_bytes: prior.maximum_plugin_module_bytes,
    maximum_plugin_workspace_bytes: prior.maximum_plugin_workspace_bytes,
    maximum_alias_occurrences: prior.maximum_alias_occurrences,
    maximum_alias_workspace_bytes: prior.maximum_alias_workspace_bytes,
    maximum_fingerprint_workspace_bytes: prior.maximum_fingerprint_workspace_bytes,
  }
}

#[test]
fn retained_source_union_validation_preserves_standalone_namespace_cursor_send() {
  fn assert_send<T: Send>() {}
  assert_send::<NativeSemanticNamespaceSourceCursorV1<'static>>();
}

pub(super) fn independent_fingerprint(algorithm: HashAlgorithm, rows: &SourceMap) -> Vec<u8> {
  use sha2::Digest;
  let mut bytes = b"aeordb.semantic-mutation-sources.v1\0".to_vec();
  for (path, revision) in rows {
    bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
    bytes.extend_from_slice(path.as_bytes());
    match revision {
      Some(revision) => bytes.extend_from_slice(revision),
      None => bytes.resize(bytes.len() + algorithm.hash_length(), 0),
    }
  }
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(&bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(&bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(&bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(&bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(&bytes).to_vec(),
  }
}

fn seed_validation_pair(
  publisher: &V4FirstAuthorityPublisher,
  base: &[u8],
  staged: &[u8],
  base_sources: &SourceMap,
  requested_sources: &SourceMap,
  fingerprint_sources: &SourceMap,
  expected_configurations: u64,
) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let width = algorithm.hash_length();
  let mut checkpoint = frozen(algorithm, "checkpoint");
  checkpoint[120..168].fill(0);
  checkpoint[120..122].copy_from_slice(&1u16.to_le_bytes());
  checkpoint[128..136].copy_from_slice(&expected_configurations.to_le_bytes());
  checkpoint[176..200].fill(0);
  checkpoint[200..200 + width].copy_from_slice(base);
  checkpoint[200 + width..200 + 2 * width].copy_from_slice(staged);
  checkpoint[200 + 2 * width..200 + 6 * width].fill(0);
  checkpoint[200 + 8 * width..200 + 9 * width].copy_from_slice(&independent_fingerprint(algorithm, fingerprint_sources));
  crc(&mut checkpoint);
  let decoded = decode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap();
  let mut roots = Vec::new();
  for sources in [base_sources, requested_sources] {
    let rows: Vec<_> =
      sources.iter().map(|(path, revision)| SemanticSourceLeafEntryV1 { path, file_record_id: revision.as_deref() }).collect();
    let node = encode_semantic_source_leaf_v1(&[1; 16], &rows, algorithm).unwrap();
    let identity = decode_system_control(&node, algorithm).unwrap().identity;
    seed(publisher, &[(SystemControlKindV1::SemanticSourceNode, &identity, SystemControlSlotV1::Immutable, &node)]);
    roots.push(identity);
  }
  let checkpoint_hash = digest_parts(algorithm, &[&checkpoint]);
  let companion = encode_semantic_source_capture_v1(
    &SemanticSourceCaptureV1 {
      database_id: decoded.database_id,
      task_id: decoded.task_id,
      checkpoint_sequence: decoded.checkpoint_sequence,
      physical_instance_id: decoded.physical_instance_id,
      writer_fence_epoch: decoded.writer_fence_epoch,
      semantic_generation: decoded.semantic_generation,
      header_sequence: decoded.header_sequence,
      captured_at_ms: decoded.captured_at_ms,
      protected_path_count: base_sources.len() as u64,
      base_catalog_node_count: 1,
      requested_catalog_node_count: 1,
      base_namespace_root: decoded.base_namespace_root,
      staged_directory_root: decoded.staged_directory_root,
      base_source_catalog: &roots[0],
      requested_source_catalog: &roots[1],
      source_identity_fingerprint: decoded.source_identity_fingerprint,
      checkpoint_payload_hash: &checkpoint_hash,
    },
    algorithm,
  )
  .unwrap();
  let identity = checkpoint_identity();
  seed(
    publisher,
    &[
      (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
      (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
    ],
  );
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 0b1010;
  header.required_writer_capabilities[3] |= 0b1010;
  write_redundant_header(publisher, &header);
}

#[test]
fn retained_source_union_validation_empty_globals_reopens_with_independent_fingerprint() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-union-empty", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    let globals = absent_globals();
    seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &globals, &globals, &globals, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let result = capture
      .validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash))
      .expect("complete retained empty sources must validate after reopen");
    assert_eq!((result.protected_paths, result.namespace_paths), (2, 0));
    assert_eq!((result.base_configuration_count, result.requested_configuration_count), (0, 0));
    assert!(result.read_bytes > 0 && result.catalog_work > 0 && result.namespace_work > 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_source_union_validation_counts_exact_old_requested_namespace_and_global_inputs() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-union-changes", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    publisher.publish(&initial).unwrap();
    let body = br#"{"$v":1,"indexes":[]}"#;
    let changed = br#"{ "$v": 1, "indexes": [] }"#;
    let (a, a_revision) = namespace_configuration_tree(&publisher, "/a", body, vec![]);
    let (b, b_revision) = namespace_configuration_tree(&publisher, "/b", body, vec![]);
    let (new_a, _) = namespace_configuration_tree(&publisher, "/a", changed, vec![]);
    let (c, _) = namespace_configuration_tree(&publisher, "/c", body, vec![]);
    let base_tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", a), namespace_directory_child("b", b)]);
    let requested = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", new_a), namespace_directory_child("c", c)]);
    let mut next = successor_request(&publisher, 0x79, "unused");
    next.semantic_state = initial.semantic_state.clone();
    next.namespace_tree = PreparedNamespaceTreeV0 {
      root_hash: base_tree.clone(),
      stored_value: publisher.load_immutable_entity_bounded(&base_tree, 1 << 20).unwrap().unwrap().stored_value,
    };
    let base = publisher.publish_successor_authority(&next).unwrap().namespace_root.root_hash;
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", body)]);
    let global = seed_retained_revision(&publisher, INDEX_SOURCE);
    let mut base_sources = absent_globals();
    base_sources.insert(INDEX_SOURCE.to_string(), Some(global));
    let mut fingerprint_sources = base_sources.clone();
    fingerprint_sources.insert("/a/.aeordb-config/indexes.json".to_string(), Some(a_revision));
    fingerprint_sources.insert("/b/.aeordb-config/indexes.json".to_string(), Some(b_revision));
    fingerprint_sources.insert("/c/.aeordb-config/indexes.json".to_string(), None);
    seed_validation_pair(&publisher, &base, &requested, &base_sources, &absent_globals(), &fingerprint_sources, 2);
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"unrelated invalid current configuration")]);
    let mut later = successor_request(&publisher, 0x7a, "ordinary-after-capture");
    let later_tree =
      publish_namespace_directory(&publisher, vec![namespace_directory_child("ordinary-after-capture", initial.namespace_tree.root_hash)]);
    later.namespace_tree = PreparedNamespaceTreeV0 {
      root_hash: later_tree.clone(),
      stored_value: publisher.load_immutable_entity_bounded(&later_tree, 1 << 20).unwrap().unwrap().stored_value,
    };
    later.semantic_state = initial.semantic_state;
    let current = publisher.publish_successor_authority(&later).unwrap().namespace_root.root_hash;
    assert_ne!(current, base);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let result = capture
      .validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&requested))
      .expect("retained namespace union must not consult current HEAD or globals");
    assert_eq!((result.protected_paths, result.namespace_paths), (2, 3));
    assert_eq!((result.base_configuration_count, result.requested_configuration_count), (3, 2));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
