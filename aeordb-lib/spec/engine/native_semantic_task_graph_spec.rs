//! Whole selected-task read-set targets. No production task writer is enabled.
#[path = "native_semantic_task_graph_boundary_spec.rs"]
mod boundary;
#[path = "native_semantic_task_graph_metadata_spec.rs"]
mod metadata;
#[path = "native_semantic_task_graph_phase_spec.rs"]
mod phase;
#[path = "native_semantic_task_retention_spec.rs"]
mod retention;
#[path = "native_semantic_task_retention_characterization_spec.rs"]
mod retention_characterization;
use super::*;
use std::collections::BTreeMap;
use crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_checkpoint;
use crate::engine::v4::semantic_source_capture::{
  SemanticSourceCaptureV1, SemanticSourceLeafEntryV1, encode_semantic_source_capture_v1, encode_semantic_source_leaf_v1,
};

type PhysicalSet = BTreeMap<Vec<u8>, (u8, u64, u32)>;

fn graph_bounds() -> NativeSemanticTaskGraphBoundsV1 {
  NativeSemanticTaskGraphBoundsV1 {
    maximum_work: 8192,
    maximum_read_bytes: 64 << 20,
    maximum_namespace_workspace_bytes: 16 << 20,
    maximum_depth: 16,
    maximum_path_bytes: 1024,
    maximum_decoded_chunk_bytes: 2 << 20,
    sources: NativeSemanticSourceCatalogBoundsV1 {
      maximum_depth: 8,
      maximum_work: 4096,
      maximum_read_bytes: 64 << 20,
      maximum_source_bytes: 1 << 20,
      maximum_chunk_entity_bytes: 2 << 20,
      maximum_source_chunks: 1024,
    },
  }
}

fn include_key(publisher: &V4FirstAuthorityPublisher, set: &mut PhysicalSet, key: &[u8], role: u8) {
  let locator = publisher.locator(key).unwrap().unwrap();
  assert_eq!(locator.type_flags, role);
  set.insert(key.to_vec(), (role, locator.offset, locator.total_length));
}

fn include_system_file(publisher: &V4FirstAuthorityPublisher, set: &mut PhysicalSet, path: &str, body: &[u8]) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  include_key(publisher, set, &first_authority_file_path_hash(path, algorithm), KV_TYPE_FILE_RECORD);
  include_key(publisher, set, &digest_parts(algorithm, &[b"system::", body]), KV_TYPE_CHUNK);
}

fn include_control(
  publisher: &V4FirstAuthorityPublisher,
  set: &mut PhysicalSet,
  kind: SystemControlKindV1,
  identity: &[u8],
  slot: SystemControlSlotV1,
  body: &[u8],
) {
  include_system_file(publisher, set, &system_control_path(kind, identity, slot).unwrap(), body);
}

fn captured_pair(algorithm: HashAlgorithm, base: &[u8], staged: &[u8]) -> (Vec<u8>, Vec<u8>) {
  // Start with independent frozen envelopes, not the production task writer.
  // Fixed offsets below are the ratified ASMC/ASMT contract, including framing.
  let width = algorithm.hash_length();
  let mut checkpoint = frozen(algorithm, "checkpoint");
  checkpoint[120..168].fill(0);
  checkpoint[120..122].copy_from_slice(&1u16.to_le_bytes());
  checkpoint[176..200].fill(0);
  checkpoint[200..200 + width].copy_from_slice(base);
  checkpoint[200 + width..200 + 2 * width].copy_from_slice(staged);
  checkpoint[200 + 2 * width..200 + 6 * width].fill(0);
  crc(&mut checkpoint);
  assert_eq!(decode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap().phase, SemanticMutationPhaseV1::Captured);
  let mut task = frozen(algorithm, "task");
  task[128..130].copy_from_slice(&2u16.to_le_bytes());
  task[130..132].fill(0);
  task[144..144 + width].copy_from_slice(&digest_parts(algorithm, &[&checkpoint]));
  crc(&mut task);
  (task, checkpoint)
}

fn seed_captured_graph(publisher: &V4FirstAuthorityPublisher) -> (PhysicalSet, Vec<u8>, Vec<u8>) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let request = request_for_database_and_algorithm([1; 16], algorithm);
  let initial = publisher.publish(&request).unwrap();
  let body = b"ordinary staged file";
  let (file, _) = publish_namespace_configuration(publisher, "/photo.json", body);
  let link_bytes =
    crate::engine::symlink_record::SymlinkRecord { path: "/link".into(), target: "/not-followed".into(), created_at: 11, updated_at: 13 }
      .serialize()
      .unwrap();
  let link = publish_namespace_value(publisher, EntryTypeV4::Symlink, 0, b"symlinkc:", &link_bytes);
  let staged = publish_namespace_directory(
    publisher,
    vec![
      NamespaceFixtureChild {
        name: "photo.json".into(),
        kind: EntryTypeV4::FileRecord,
        key: file.clone(),
        size: body.len() as u64,
        content_type: Some("application/json"),
      },
      NamespaceFixtureChild { name: "link".into(), kind: EntryTypeV4::Symlink, key: link.clone(), size: 0, content_type: None },
    ],
  );
  let (task, checkpoint) = captured_pair(algorithm, &initial.namespace_root.root_hash, &staged);
  let node = encode_semantic_source_leaf_v1(
    &[1; 16],
    &[
      SemanticSourceLeafEntryV1 { path: INDEX_SOURCE, file_record_id: None },
      SemanticSourceLeafEntryV1 { path: "/.aeordb-config/parsers.json", file_record_id: None },
    ],
    algorithm,
  )
  .unwrap();
  let node_id = decode_system_control(&node, algorithm).unwrap().identity;
  let decoded = decode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap();
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
      protected_path_count: 2,
      base_catalog_node_count: 1,
      requested_catalog_node_count: 1,
      base_namespace_root: decoded.base_namespace_root,
      staged_directory_root: decoded.staged_directory_root,
      base_source_catalog: &node_id,
      requested_source_catalog: &node_id,
      source_identity_fingerprint: decoded.source_identity_fingerprint,
      checkpoint_payload_hash: &checkpoint_hash,
    },
    algorithm,
  )
  .unwrap();
  let generation = frozen(algorithm, "generation");
  let identity = checkpoint_identity();
  let controls: Vec<(SystemControlKindV1, &[u8], SystemControlSlotV1, &[u8])> = vec![
    (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task),
    (SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &generation),
    (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
    (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
    (SystemControlKindV1::SemanticSourceNode, &node_id, SystemControlSlotV1::Immutable, &node),
  ];
  seed(publisher, &controls);
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 8;
  header.required_writer_capabilities[3] |= 8;
  write_redundant_header(publisher, &header);
  let mut expected = PhysicalSet::new();
  for (kind, id, slot, bytes) in controls {
    include_control(publisher, &mut expected, kind, id, slot, bytes);
  }
  include_key(publisher, &mut expected, &initial.namespace_root.root_hash, kv_tag::DIRECTORY);
  include_key(publisher, &mut expected, &request.namespace_tree.root_hash, kv_tag::DIRECTORY);
  include_key(publisher, &mut expected, &staged, kv_tag::DIRECTORY);
  include_key(publisher, &mut expected, &file, KV_TYPE_FILE_RECORD);
  include_key(publisher, &mut expected, &link, kv_tag::SYMLINK);
  include_key(publisher, &mut expected, &digest_parts(algorithm, &[b"chunk:", body]), KV_TYPE_CHUNK);
  include_system_file(
    publisher,
    &mut expected,
    &semantic_object_path(algorithm, 1, &request.semantic_state.object_id).unwrap(),
    &request.semantic_state.value,
  );
  let admission = publisher
    .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], &initial.namespace_root.root_hash)
    .unwrap()
    .unwrap();
  include_control(
    publisher,
    &mut expected,
    SystemControlKindV1::RootAdmissionCommit,
    &initial.namespace_root.root_hash,
    SystemControlSlotV1::Immutable,
    &admission.bytes,
  );
  assert_eq!(expected.len(), 20);
  (expected, initial.namespace_root.root_hash, staged)
}

#[test]
fn native_semantic_task_graph_reads_selected_captured_closure_after_head_advance_and_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("selected-task-graph", None, [1; 16], algorithm, 0);
    let (expected, base, staged) = seed_captured_graph(&publisher);
    let mut next = successor_request(&publisher, 0x75, "unused-fixture-child");
    next.semantic_state = request_for_database_and_algorithm([1; 16], algorithm).semantic_state;
    let loaded = publisher.load_immutable_entity_bounded(&staged, 1 << 20).unwrap().unwrap();
    next.namespace_tree = PreparedNamespaceTreeV0 { root_hash: staged, stored_value: loaded.stored_value };
    let successor = publisher.publish_successor_authority(&next).unwrap();
    assert_ne!(base, successor.namespace_root.root_hash);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut actual = PhysicalSet::new();
    let mut reads = 0;
    let mut bytes = 0;
    let summary = capture
      .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        reads += 1;
        bytes += u64::from(entry.total_length);
        Ok(())
      })
      .unwrap();
    assert_eq!(summary.disposition, SemanticMutationObservationDispositionV1::CheckpointHeld);
    assert_eq!(summary.checkpoint_sequence, Some(1));
    assert_eq!(summary.source_paths, 2);
    assert_eq!(
      (summary.namespace_directories, summary.namespace_files, summary.namespace_symlinks, summary.namespace_chunks),
      (2, 1, 1, 1)
    );
    assert_eq!((summary.physical_reads, summary.read_bytes), (reads, bytes));
    assert_eq!(actual, expected);
    assert!(!actual.contains_key(&successor.namespace_root.root_hash));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_distinguishes_absent_and_released_without_checkpoint_reads() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("selected-task-dispositions", None, [1; 16], algorithm, 0);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let absent = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut task = frozen(algorithm, "task");
    task[128..130].copy_from_slice(&9u16.to_le_bytes());
    task[130..132].copy_from_slice(&1u16.to_le_bytes());
    crc(&mut task);
    let generation = frozen(algorithm, "generation");
    populate(&publisher, &task, None, Some(&generation));
    let released = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut expected = PhysicalSet::new();
    include_control(&publisher, &mut expected, SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task);
    include_control(&publisher, &mut expected, SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &generation);
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    for (capture, disposition) in
      [(&absent, SemanticMutationObservationDispositionV1::Absent), (&released, SemanticMutationObservationDispositionV1::ReleasedTerminal)]
    {
      let mut actual = PhysicalSet::new();
      let result = capture
        .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
          actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
          Ok(())
        })
        .unwrap();
      assert_eq!(result.disposition, disposition);
      assert_eq!(result.checkpoint_sequence, None);
      assert_eq!((result.source_paths, result.namespace_files, result.namespace_directories), (0, 0, 0));
      if disposition == SemanticMutationObservationDispositionV1::Absent {
        assert!(actual.is_empty());
      } else {
        assert_eq!(actual, expected);
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
