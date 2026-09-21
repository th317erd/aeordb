//! Phase-specific retained edges; these fixtures do not publish production tasks.
#[path = "native_semantic_task_graph_structure_spec.rs"]
mod structure;
#[path = "native_unselected_semantic_checkpoint_phase_spec.rs"]
mod unselected;
use super::*;
use crate::engine::v4::dependency::{DependencyRecordV1, encode_dependency_record};
use crate::engine::v4::namespace::{
  EncodedNamespaceRootV1, EncodedSemanticObjectV1, NamespaceRootWriteV1, SemanticAvailabilityV1, SemanticCatalogRecordV1,
  SemanticStateWriteV1, encode_namespace_root, encode_semantic_catalog_leaf, encode_semantic_definition_object,
  encode_semantic_state_object,
};
use crate::engine::v4::plugin_artifact_identity::plugin_artifact_path_v1;
use crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1;

const MODULE: &[u8] = b"\0asm\x01\0\0\0";

pub(super) fn use_staged_tree(publisher: &V4FirstAuthorityPublisher, expected: &mut PhysicalSet, tree: &[u8]) {
  let width = publisher.observe().unwrap().selected.header.hash_algorithm.hash_length();
  let mut checkpoint = load_checkpoint(publisher);
  checkpoint[200 + width..200 + 2 * width].copy_from_slice(tree);
  crc(&mut checkpoint);
  replace_checkpoint(publisher, expected, &checkpoint, 2);
}

fn semantic_edge(publisher: &V4FirstAuthorityPublisher, expected: &mut PhysicalSet, kind: u16, object: &EncodedSemanticObjectV1) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  publisher
    .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &[1; 16],
      objects: std::slice::from_ref(object),
      publication_timestamp_ms: publisher.observe().unwrap().selected.header.updated_at_ms + 1,
    })
    .unwrap();
  include_system_file(publisher, expected, &semantic_object_path(algorithm, kind, &object.object_id).unwrap(), &object.value);
}

fn dependency_catalog(publisher: &V4FirstAuthorityPublisher, expected: &mut PhysicalSet, executable: bool) -> EncodedSemanticObjectV1 {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let record = DependencyRecordV1 {
    kind: if executable { 1 } else { 2 },
    role: 1,
    flags: if executable { 4 } else { 0 },
    abi: if executable { 5 } else { 0 },
    executor_profile: if executable { 4 } else { 1 },
    fingerprint_semantics: if executable { 1 } else { 2 },
    artifact_kind: if executable { 1 } else { 0 },
    artifact_length: if executable { MODULE.len() as u64 } else { 0 },
    fingerprint: if executable { *blake3::hash(MODULE).as_bytes() } else { [5; 32] },
    dependency_id: "/org/example/retained",
    version: "1.0.0",
  };
  let class = if executable { 6 } else { 7 };
  let definition = encode_semantic_definition_object(class, &encode_dependency_record(&record).unwrap(), algorithm).unwrap();
  let catalog = encode_semantic_catalog_leaf(
    &[SemanticCatalogRecordV1 {
      record_kind: class,
      semantic_id: &definition.semantic_id,
      definition_object_id: &definition.object.object_id,
      owner_key: &definition.semantic_id,
    }],
    algorithm,
  )
  .unwrap();
  semantic_edge(publisher, expected, 4, &definition.object);
  semantic_edge(publisher, expected, 2, &catalog);
  if executable {
    let path = plugin_artifact_path_v1(&record.fingerprint).unwrap();
    seed_files(publisher, &[(path.clone(), "application/wasm", MODULE)]);
    include_system_file(publisher, expected, &path, MODULE);
  }
  catalog
}

fn seed_candidate(publisher: &V4FirstAuthorityPublisher, root: &EncodedNamespaceRootV1) {
  // Test-only raw construction, as for the existing control fixtures. Deliberately
  // no production namespace publication, admission or capability relaxation.
  let _guard = publisher.root_state.lock().unwrap();
  let mut header = publisher.observe().unwrap().selected.header;
  let sequence = header.write_sequence_high_water + 1;
  let bytes = encode_whole_entity(&WholeEntityWriteV1 {
    entity_version: 1,
    entry_type: EntryTypeV4::DirectoryIndex,
    flags: WHOLE_ENTITY_V1_FLAG_SYSTEM,
    hash_algorithm: header.hash_algorithm,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: header.updated_at_ms + 1,
    write_sequence: sequence,
    key: &root.root_hash,
    stored_value: &root.value,
  })
  .unwrap();
  let mut kv = publisher.lock_kv().unwrap();
  write_file_at_native(&publisher.file, header.hot_tail_offset, &bytes).unwrap();
  kv.insert(KVEntry {
    type_flags: kv_tag::DIRECTORY,
    hash: root.root_hash.clone(),
    offset: header.hot_tail_offset,
    total_length: bytes.len() as u32,
  })
  .unwrap();
  header.hot_tail_offset += bytes.len() as u64;
  kv.set_hot_tail_offset(header.hot_tail_offset);
  kv.force_flush_hot_buffer().unwrap();
  header.entry_count = kv.len() as u64;
  drop(kv);
  header.slot_sequence += 1;
  header.updated_at_ms += 1;
  header.write_sequence_high_water = sequence;
  write_redundant_header(publisher, &header);
}

fn load_checkpoint(publisher: &V4FirstAuthorityPublisher) -> Vec<u8> {
  publisher
    .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &checkpoint_identity())
    .unwrap()
    .unwrap()
    .bytes
}

fn replace_checkpoint(publisher: &V4FirstAuthorityPublisher, expected: &mut PhysicalSet, checkpoint: &[u8], state: u16) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let width = algorithm.hash_length();
  let identity = checkpoint_identity();
  let companion_old =
    publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &identity).unwrap().unwrap().bytes;
  let mut companion = decode_semantic_source_capture_v1(&companion_old, algorithm).unwrap();
  let decoded = decode_semantic_mutation_checkpoint(checkpoint, algorithm).unwrap();
  let hash = digest_parts(algorithm, &[checkpoint]);
  companion.checkpoint_payload_hash = &hash;
  companion.base_namespace_root = decoded.base_namespace_root;
  companion.staged_directory_root = decoded.staged_directory_root;
  let companion = encode_semantic_source_capture_v1(&companion, algorithm).unwrap();
  let mut task = frozen(algorithm, "task");
  task[128..130].copy_from_slice(&state.to_le_bytes());
  task[130..132].fill(0);
  task[144..144 + width].copy_from_slice(&hash);
  crc(&mut task);
  let controls: Vec<(SystemControlKindV1, &[u8], SystemControlSlotV1, &[u8])> = vec![
    (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task),
    (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, checkpoint),
    (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
  ];
  for (kind, id, slot, _) in &controls {
    let path = system_control_path(*kind, id, *slot).unwrap();
    let path_key = first_authority_file_path_hash(&path, algorithm);
    let old = load_canonical_system_file_at_path(
      &publisher.file,
      &*publisher.lock_kv().unwrap(),
      &publisher.observe().unwrap().selected.header,
      &path,
      SYSTEM_CONTROL_CONTENT_TYPE,
      1 << 20,
    )
    .unwrap()
    .unwrap();
    expected.remove(&path_key);
    expected.remove(&digest_parts(algorithm, &[b"system::", &old.body]));
  }
  seed(publisher, &controls);
  for (kind, id, slot, body) in controls {
    include_control(publisher, expected, kind, id, slot, body);
  }
}

struct PhaseFixture {
  expected: PhysicalSet,
  catalog: EncodedSemanticObjectV1,
  output: Option<EncodedSemanticObjectV1>,
  candidate: Option<EncodedNamespaceRootV1>,
}

fn phase_fixture(publisher: &V4FirstAuthorityPublisher, phase: u16, rebase: bool) -> PhaseFixture {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let width = algorithm.hash_length();
  let (mut expected, _, staged) = seed_captured_graph(publisher);
  let catalog = dependency_catalog(publisher, &mut expected, true);
  let mut checkpoint = load_checkpoint(publisher);
  checkpoint[120..122].copy_from_slice(&phase.to_le_bytes());
  for offset in [144, 152, 160] {
    checkpoint[offset..offset + 8].copy_from_slice(&1u64.to_le_bytes());
  }
  checkpoint[200 + 2 * width..200 + 3 * width].copy_from_slice(&catalog.object_id);
  if phase == 3 {
    let pruning = dependency_catalog(publisher, &mut expected, false);
    checkpoint[184..192].copy_from_slice(&1u64.to_le_bytes());
    checkpoint[192..200].copy_from_slice(&1u64.to_le_bytes());
    checkpoint[200 + 3 * width..200 + 4 * width].copy_from_slice(&pruning.object_id);
  }
  let mut output = None;
  let mut candidate = None;
  if phase >= 4 {
    let state = encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::Complete {
          compiler_fingerprint: checkpoint[200 + 6 * width..200 + 7 * width].to_vec(),
          semantic_registry_fingerprint: checkpoint[200 + 7 * width..200 + 8 * width].to_vec(),
          catalog_root: catalog.object_id.clone(),
          catalog_record_count: 1,
          catalog_node_count: 1,
          definition_count: 1,
          dependency_count: 1,
        },
      },
      algorithm,
    )
    .unwrap();
    semantic_edge(publisher, &mut expected, 1, &state);
    let tree = if rebase {
      let body = b"ordinary concurrent rebase";
      let (file, _) = publish_namespace_configuration(publisher, "/rebase.json", body);
      let tree = publish_namespace_directory(
        publisher,
        vec![NamespaceFixtureChild {
          name: "rebase.json".into(),
          kind: EntryTypeV4::FileRecord,
          key: file.clone(),
          size: body.len() as u64,
          content_type: Some("application/json"),
        }],
      );
      include_key(publisher, &mut expected, &tree, kv_tag::DIRECTORY);
      include_key(publisher, &mut expected, &file, KV_TYPE_FILE_RECORD);
      include_key(publisher, &mut expected, &digest_parts(algorithm, &[b"chunk:", body]), KV_TYPE_CHUNK);
      tree
    } else {
      staged
    };
    let root = encode_namespace_root(
      &NamespaceRootWriteV1 { required_capabilities: [0; 32], namespace_tree_root: tree, semantic_state_root: state.object_id.clone() },
      algorithm,
    )
    .unwrap();
    seed_candidate(publisher, &root);
    include_key(publisher, &mut expected, &root.root_hash, kv_tag::DIRECTORY);
    checkpoint[200 + 4 * width..200 + 5 * width].copy_from_slice(&state.object_id);
    checkpoint[200 + 5 * width..200 + 6 * width].copy_from_slice(&root.root_hash);
    output = Some(state);
    candidate = Some(root);
  }
  if phase == 5 {
    let generation = u64::from_le_bytes(checkpoint[96..104].try_into().unwrap()) + 1;
    checkpoint[176..184].copy_from_slice(&generation.to_le_bytes());
  }
  crc(&mut checkpoint);
  replace_checkpoint(
    publisher,
    &mut expected,
    &checkpoint,
    match phase {
      2 | 3 => 3,
      4 => 4,
      5 => 6,
      _ => panic!("unexpected phase"),
    },
  );
  PhaseFixture { expected, catalog, output, candidate }
}

#[test]
fn native_semantic_task_graph_reads_all_checkpoint_phases_without_alias_or_candidate_admission() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for phase in 2..=5 {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("task-graph-phases", None, [1; 16], algorithm, 0);
      let fixture = phase_fixture(&publisher, phase, phase == 5);
      if let Some(candidate) = &fixture.candidate {
        assert!(publisher
          .load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], &candidate.root_hash)
          .unwrap()
          .is_none());
      }
      drop(publisher);
      let (_coordinator, publisher) = reopen(&path);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let mut actual = PhysicalSet::new();
      let summary = capture
        .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
          assert!(publisher.root_state.try_lock().is_ok());
          assert!(publisher.kv.try_lock().is_ok());
          actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
          Ok(())
        })
        .unwrap();
      assert_eq!(summary.disposition, SemanticMutationObservationDispositionV1::CheckpointHeld);
      assert_eq!(summary.namespace_files, if phase == 5 { 2 } else { 1 });
      assert_eq!(actual, fixture.expected, "phase {phase}, {algorithm:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_task_graph_missing_or_corrupt_late_phase_edges_fail_without_writes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for branch in ["catalog", "definition", "archive", "output", "candidate"] {
      for missing in [false, true] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("task-graph-late-edge", None, [1; 16], algorithm, 0);
        let fixture = phase_fixture(&publisher, 4, false);
        let key = match branch {
          "catalog" => first_authority_file_path_hash(&semantic_object_path(algorithm, 2, &fixture.catalog.object_id).unwrap(), algorithm),
          "definition" => {
            let node = crate::engine::v4::namespace::decode_semantic_catalog_node(&fixture.catalog.value, algorithm).unwrap();
            let crate::engine::v4::namespace::SemanticCatalogNodeV1::Leaf(leaf) = node else {
              panic!("one-record fixture leaf");
            };
            let record = leaf.records().next().unwrap().unwrap();
            first_authority_file_path_hash(&semantic_object_path(algorithm, 4, record.definition_object_id).unwrap(), algorithm)
          }
          "archive" => first_authority_file_path_hash(&plugin_artifact_path_v1(blake3::hash(MODULE).as_bytes()).unwrap(), algorithm),
          "output" => first_authority_file_path_hash(
            &semantic_object_path(algorithm, 1, &fixture.output.as_ref().unwrap().object_id).unwrap(),
            algorithm,
          ),
          "candidate" => fixture.candidate.as_ref().unwrap().root_hash.clone(),
          _ => unreachable!(),
        };
        if missing {
          assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
          seed_files(&publisher, &[]);
        } else {
          corrupt_last_entity_byte(&publisher, &key);
        }
        let memory = observation_memory();
        let cancellation = CancellationToken::new();
        let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
        let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
        let baseline = memory.snapshot().unwrap().reserved_bytes;
        let before = fs::read(&path).unwrap();
        let mut calls = 0;
        let error = capture
          .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| {
            calls += 1;
            Ok(())
          })
          .unwrap_err();
        let expected = if !missing {
          "integrity_hash_mismatch"
        } else {
          match branch {
            "catalog" => "semantic_catalog_missing",
            "definition" => "semantic_definition_missing",
            "archive" => "semantic_task_graph_archive_missing",
            "output" => "semantic_task_graph_state_missing",
            "candidate" => "semantic_task_graph_entity_missing",
            _ => unreachable!(),
          }
        };
        assert_eq!(error.code(), expected, "{branch}, missing={missing}, {algorithm:?}");
        assert!(calls > 12, "must reach the late branch");
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        assert_eq!(fs::read(&path).unwrap(), before);
      }
    }
  }
}

#[test]
fn native_semantic_task_graph_binds_archive_counts_output_and_candidate_instead_of_only_walking() {
  for case in ["archive-length", "archive-digest", "counts", "output", "candidate"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let width = algorithm.hash_length();
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-bindings", None, [1; 16], algorithm, 0);
    let mut fixture = phase_fixture(&publisher, 4, false);
    let mut checkpoint = load_checkpoint(&publisher);
    match case {
      "archive-length" | "archive-digest" => {
        let body: &[u8] = if case == "archive-length" { b"short" } else { b"12345678" };
        seed_files(&publisher, &[(plugin_artifact_path_v1(blake3::hash(MODULE).as_bytes()).unwrap(), "application/wasm", body)]);
      }
      "counts" => {
        checkpoint[160..168].fill(0);
      }
      "output" => {
        let output = decode_semantic_object(&fixture.output.as_ref().unwrap().value, algorithm).unwrap().semantic_state.unwrap();
        let mut availability = output.availability;
        if let SemanticAvailabilityV1::Complete { compiler_fingerprint, .. } = &mut availability {
          compiler_fingerprint[0] ^= 1;
        }
        let output =
          encode_semantic_state_object(&SemanticStateWriteV1 { required_capabilities: [0; 32], availability }, algorithm).unwrap();
        semantic_edge(&publisher, &mut fixture.expected, 1, &output);
        checkpoint[200 + 4 * width..200 + 5 * width].copy_from_slice(&output.object_id);
      }
      "candidate" => {
        let old = crate::engine::v4::namespace::decode_namespace_root(&fixture.candidate.as_ref().unwrap().value, algorithm).unwrap();
        let other = request_for_database_and_algorithm([1; 16], algorithm).semantic_state;
        let candidate = encode_namespace_root(
          &NamespaceRootWriteV1 {
            required_capabilities: [0; 32],
            namespace_tree_root: old.namespace_tree_root,
            semantic_state_root: other.object_id,
          },
          algorithm,
        )
        .unwrap();
        seed_candidate(&publisher, &candidate);
        checkpoint[200 + 5 * width..200 + 6 * width].copy_from_slice(&candidate.root_hash);
      }
      _ => unreachable!(),
    }
    crc(&mut checkpoint);
    replace_checkpoint(&publisher, &mut fixture.expected, &checkpoint, 4);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap_err();
    let expected = match case {
      "archive-length" | "archive-digest" => "semantic_task_graph_archive_identity",
      "counts" => "semantic_task_graph_catalog_counts",
      "output" => "semantic_task_graph_output_binding",
      "candidate" => "semantic_task_graph_candidate_closure",
      _ => unreachable!(),
    };
    assert_eq!(error.code(), expected, "{case}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_visits_wide_directories_linearly_without_global_deduplication() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let mut summaries = Vec::new();
    for width in [16usize, 64] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("task-graph-wide", None, [1; 16], algorithm, 0);
      let (mut expected, _, _) = seed_captured_graph(&publisher);
      let leaf = publish_namespace_directory(&publisher, vec![]);
      let children = (0..width).map(|index| namespace_directory_child(&format!("d{index:03}"), leaf.clone())).collect();
      let tree = publish_namespace_directory(&publisher, children);
      use_staged_tree(&publisher, &mut expected, &tree);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let mut leaf_reads = 0;
      let summary = capture
        .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
          if entry.hash == leaf {
            leaf_reads += 1;
          }
          Ok(())
        })
        .unwrap();
      assert_eq!(summary.namespace_directories, width as u64 + 2);
      assert_eq!(summary.namespace_files, 0);
      // The initial admitted root uses the same empty leaf. Its authority load
      // and namespace walk are two further reads beyond the staged children.
      assert_eq!(leaf_reads, width + 2);
      summaries.push(summary);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
    assert_eq!(summaries[1].physical_reads - summaries[0].physical_reads, 48);
    assert_eq!(summaries[1].work - summaries[0].work, 96);
  }
}

#[test]
fn native_semantic_task_graph_depth_workspace_and_chunk_bounds_refuse_and_retry() {
  for case in ["depth", "workspace", "chunk"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-limits", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    if case == "depth" {
      let mut tree = publish_namespace_directory(&publisher, vec![]);
      for _ in 0..5 {
        tree = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", tree)]);
      }
      use_staged_tree(&publisher, &mut expected, &tree);
    }
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut bounds = graph_bounds();
    match case {
      "depth" => bounds.maximum_depth = 4,
      "workspace" => bounds.maximum_namespace_workspace_bytes = 1,
      "chunk" => bounds.maximum_decoded_chunk_bytes = 1,
      _ => unreachable!(),
    }
    let error = capture.visit_captured_semantic_task_physical_entries(&[2; 16], bounds, |_| Ok(())).unwrap_err();
    if case != "chunk" {
      assert_eq!(
        error.code(),
        if case == "depth" { "semantic_task_graph_namespace_depth" } else { "semantic_task_graph_namespace_memory" }
      );
    } else {
      assert_eq!(error.code(), "semantic_source_content_length");
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_preserves_fallible_root_read_allocation_and_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-allocation", None, [1; 16], algorithm, 0);
  let (_, base, _) = seed_captured_graph(&publisher);
  let length = publisher.locator(&base).unwrap().unwrap().total_length as usize;
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let (result, allocations) =
    measure(length, || capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().code(), "first_authority_readback_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_graph_retains_historical_base_complete_catalog_after_later_head() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let width = algorithm.hash_length();
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-base-catalog", None, [1; 16], algorithm, 0);
    let mut fixture = phase_fixture(&publisher, 4, false);
    let native = dependency_catalog(&publisher, &mut fixture.expected, false);
    let state = encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::Complete {
          compiler_fingerprint: vec![0x42; width],
          semantic_registry_fingerprint: vec![0x43; width],
          catalog_root: native.object_id.clone(),
          catalog_record_count: 1,
          catalog_node_count: 1,
          definition_count: 1,
          dependency_count: 1,
        },
      },
      algorithm,
    )
    .unwrap();
    semantic_edge(&publisher, &mut fixture.expected, 1, &state);
    let mut request = successor_request(&publisher, 0x76, "historical");
    // Use the actual admitted empty tree; the opaque helper fixture child is
    // intentionally not a complete namespace graph.
    request.namespace_tree = request_for_database_and_algorithm([1; 16], algorithm).namespace_tree;
    request.semantic_state = state;
    let base = publisher.publish_successor_authority(&request).unwrap();
    let mut checkpoint = load_checkpoint(&publisher);
    checkpoint[200..200 + width].copy_from_slice(&base.namespace_root.root_hash);
    crc(&mut checkpoint);
    replace_checkpoint(&publisher, &mut fixture.expected, &checkpoint, 4);
    let mut next = successor_request(&publisher, 0x77, "later");
    next.semantic_state = request_for_database_and_algorithm([1; 16], algorithm).semantic_state;
    next.namespace_tree = request_for_database_and_algorithm([1; 16], algorithm).namespace_tree;
    let head = publisher.publish_successor_authority(&next).unwrap();
    assert_ne!(base.namespace_root.root_hash, head.namespace_root.root_hash);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let native_path = first_authority_file_path_hash(&semantic_object_path(algorithm, 2, &native.object_id).unwrap(), algorithm);
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut native_reads = 0;
    let mut seen_base = false;
    capture
      .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
        assert_ne!(entry.hash, head.namespace_root.root_hash);
        if entry.hash == base.namespace_root.root_hash {
          seen_base = true;
        }
        if entry.hash == native_path {
          native_reads += 1;
        }
        Ok(())
      })
      .unwrap();
    assert!(seen_base);
    assert_eq!(native_reads, 1);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
