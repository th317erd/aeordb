//! Native checkpoint staging with independently constructed envelope bytes.
#[path = "native_captured_semantic_checkpoint_boundary_spec.rs"]
mod boundary;
use super::super::retained_validation::{independent_fingerprint, validation_bounds};
use super::super::staged_source::staging_request;
use super::*;
use crate::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
use crate::engine::v4::system_family::embedded_system_family_registry;

fn request(timestamp: u64) -> NativeCapturedSemanticCheckpointRequestV1 {
  NativeCapturedSemanticCheckpointRequestV1 {
    task_id: [2; 16],
    mutation_count: 3,
    captured_at_ms: timestamp as i64,
    publication_timestamp_ms: timestamp + 10,
    maximum_workspace_bytes: 16 << 20,
  }
}

fn envelope(magic: &[u8; 4], body: &[u8]) -> Vec<u8> {
  let total = 36 + body.len();
  let mut result = vec![0; total];
  result[..4].copy_from_slice(magic);
  result[4..6].copy_from_slice(&1u16.to_le_bytes());
  result[6..8].copy_from_slice(&32u16.to_le_bytes());
  result[8..12].copy_from_slice(&(total as u32).to_le_bytes());
  result[16..24].copy_from_slice(&1u64.to_le_bytes());
  result[24..28].copy_from_slice(&(body.len() as u32).to_le_bytes());
  result[32..32 + body.len()].copy_from_slice(body);
  crc(&mut result);
  result
}

fn expected_pair(
  source: &NativeSemanticSourceUnionV1<'_>,
  request: NativeCapturedSemanticCheckpointRequestV1,
  count: u64,
) -> (Vec<u8>, Vec<u8>) {
  let header = source.captured_header();
  let width = header.hash_algorithm.hash_length();
  let mut checkpoint = vec![0; 168 + 9 * width];
  checkpoint[..16].copy_from_slice(&header.database_id);
  checkpoint[16..32].copy_from_slice(&request.task_id);
  checkpoint[32..40].copy_from_slice(&1u64.to_le_bytes());
  checkpoint[40..56].copy_from_slice(&header.physical_instance_id);
  for (offset, value) in [
    (56, header.writer_fence_epoch),
    (64, source.generation_selection().control_sequence),
    (72, header.slot_sequence),
    (80, request.captured_at_ms as u64),
    (96, count),
    (136, request.mutation_count),
  ] {
    checkpoint[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
  }
  checkpoint[88..90].copy_from_slice(&1u16.to_le_bytes());
  let registry = embedded_system_family_registry(header.hash_algorithm).unwrap();
  for (index, identity) in [
    (0, source.base_authority().root_hash.as_slice()),
    (1, source.requested_directory_root()),
    (6, semantic_compiler_fingerprint_v1(header.hash_algorithm)),
    (7, registry.semantic_projection_fingerprint.as_slice()),
    (8, source.fingerprint().digest()),
  ] {
    checkpoint[168 + index * width..168 + (index + 1) * width].copy_from_slice(identity);
  }
  let checkpoint = envelope(b"ASMC", &checkpoint);
  let checkpoint_hash = digest_parts(header.hash_algorithm, &[&checkpoint]);
  let mut companion = vec![0; 112 + 6 * width];
  companion[..88].copy_from_slice(&checkpoint[32..120]);
  for (offset, value) in [(88, source.catalogs().path_count()), (96, source.catalogs().node_count()), (104, source.catalogs().node_count())]
  {
    companion[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
  }
  for (index, identity) in [
    source.base_authority().root_hash.as_slice(),
    source.requested_directory_root(),
    source.catalogs().base_root(),
    source.catalogs().requested_root(),
    source.fingerprint().digest(),
    checkpoint_hash.as_slice(),
  ]
  .into_iter()
  .enumerate()
  {
    companion[112 + index * width..112 + (index + 1) * width].copy_from_slice(identity);
  }
  (checkpoint, envelope(b"ASCM", &companion))
}

#[test]
fn native_captured_checkpoint_stages_exact_initial_pair_and_reopens_without_selecting_a_task() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("captured-checkpoint-empty", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    enable_node_staging(&publisher);
    seed_union_generation(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let expected;
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let parent = tempfile::tempdir().unwrap();
      let staged = capture
        .prepare_and_stage_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &initial.namespace_tree.root_hash,
            replacements: &[],
            workspace_parent: parent.path(),
            bounds: union_bounds(&initial.namespace_tree.root_hash),
          },
          staging_request(),
        )
        .unwrap();
      let input = request(staged.source_union().captured_header().updated_at_ms + 1);
      expected = expected_pair(staged.source_union(), input, 0);
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let result =
        staged.stage_initial_checkpoint(input).expect("qualified retained source union must stage initial checkpoint dependencies");
      assert_eq!(result.controls.len(), 2);
      assert!(!result.idempotent);
      assert_eq!(result.observation.selected.header.head_hash, root);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      let bytes = fs::read(&path).unwrap();
      let repeat = staged
        .stage_initial_checkpoint(NativeCapturedSemanticCheckpointRequestV1 {
          publication_timestamp_ms: input.publication_timestamp_ms + 100,
          ..input
        })
        .unwrap();
      assert!(repeat.idempotent);
      assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let bytes = fs::read(&path).unwrap();
    for (kind, expected) in
      [(SystemControlKindV1::SemanticMutationCheckpoint, expected.0), (SystemControlKindV1::SemanticSourceCapture, expected.1)]
    {
      assert_eq!(publisher.load_immutable_system_control(kind, &[1; 16], &checkpoint_identity()).unwrap().unwrap().bytes, expected);
    }
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    assert_eq!(capture.visit(|_| panic!("unselected checkpoint cannot become a task")).unwrap().tasks, 0);
    let validated =
      capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash)).unwrap();
    assert_eq!(validated.requested_configuration_count, 0);
    assert_eq!(fs::read(&path).unwrap(), bytes);
  }
}

#[test]
fn native_captured_checkpoint_counts_requested_sources_and_keeps_historical_capture() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("captured-checkpoint-count", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    enable_node_staging(&publisher);
    seed_union_generation(&publisher);
    let body = br#"{"$v":1,"indexes":[]}"#;
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", body)]);
    let (directory, _) = namespace_configuration_tree(&publisher, "/new", body, vec![]);
    let requested = publish_namespace_directory(&publisher, vec![namespace_directory_child("new", directory)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let parent = tempfile::tempdir().unwrap();
    let staged = capture
      .prepare_and_stage_semantic_source_union(
        NativeSemanticSourceUnionRequestV1 {
          expected_base_root: &root,
          requested_directory_root: &requested,
          replacements: &[],
          workspace_parent: parent.path(),
          bounds: union_bounds(&requested),
        },
        staging_request(),
      )
      .unwrap();
    let input = request(staged.source_union().captured_header().updated_at_ms + 1);
    let expected = expected_pair(staged.source_union(), input, 2);
    let mut fingerprint = absent_globals();
    fingerprint.insert(
      INDEX_SOURCE.into(),
      Some(capture.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().revision().to_vec()),
    );
    fingerprint.insert("/new/.aeordb-config/indexes.json".into(), None);
    assert_eq!(staged.source_union().fingerprint().digest(), independent_fingerprint(algorithm, &fingerprint));
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", b"unrelated current source")]);
    let receipt = staged.stage_initial_checkpoint(input).expect("later current inputs must not replace the retained captured checkpoint");
    assert_eq!(receipt.observation.selected.header.head_hash, root);
    for (kind, expected) in
      [(SystemControlKindV1::SemanticMutationCheckpoint, expected.0), (SystemControlKindV1::SemanticSourceCapture, expected.1)]
    {
      assert_eq!(publisher.load_immutable_system_control(kind, &[1; 16], &checkpoint_identity()).unwrap().unwrap().bytes, expected);
    }
  }
}
