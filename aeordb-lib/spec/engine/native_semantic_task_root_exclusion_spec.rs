//! Native retirement must not mistake released process guards for task absence.
#[path = "native_semantic_task_root_exclusion_boundary_spec.rs"]
mod boundary;
use super::*;

fn root_retirement_owner(memory: &MemoryCoordinator, cancellation: &CancellationToken) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [1; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

fn seed_retirement_task(publisher: &V4FirstAuthorityPublisher, base: &[u8]) {
  let algorithm = HashAlgorithm::Blake3_256;
  let staged = request_for_database([1; 16]).namespace_tree.root_hash;
  let (task, checkpoint) = captured_pair(algorithm, base, &staged);
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
  let identity = checkpoint_identity();
  let generation = frozen(algorithm, "generation");
  seed(
    publisher,
    &[
      (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task),
      (SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &generation),
      (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
      (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
      (SystemControlKindV1::SemanticSourceNode, &node_id, SystemControlSlotV1::Immutable, &node),
    ],
  );
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 8;
  header.required_writer_capabilities[3] |= 8;
  write_redundant_header(publisher, &header);
  flush_mark_fixture(publisher);
}

fn retirement_verifier(prepared: &PreparedGuardedRootRetirementV1) -> DatabaseRootRetirementAuthorityVerifierV1 {
  DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: [1; 16],
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  }
}

#[test]
fn native_semantic_task_root_exclusion_characterizes_retained_root_after_reopen() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("task-root-retained", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut retirement_owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, [1; 16]);
  seed_retirement_task(&publisher, &prepared.target_root_hash);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let before = fs::read(&path).unwrap();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  assert_eq!(mark.summary().retention.tasks, 1);
  assert!(mark.summary().retention.complete);
  let root = publisher.locator(&prepared.target_root_hash).unwrap().unwrap();
  assert_eq!(root.type_flags, kv_tag::DIRECTORY);
  assert!(mark.is_captured_locator_marked(&root).unwrap());
  let error = publisher
    .publish_root_retirement(prepared.request(&cancellation), &mut retirement_verifier(&prepared), &mut retirement_owner)
    .unwrap_err();
  assert_eq!(error.code(), "staging_protection_active");
  drop(mark);
  drop(capture);
  drop(protection);
  assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, 0);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_root_exclusion_requires_proof_at_retirement_after_reopen() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("task-root-final", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut retirement_owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, [1; 16]);
  seed_retirement_task(&publisher, &prepared.target_root_hash);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, 0);
  let before = fs::read(&path).unwrap();
  let error = publisher
    .publish_root_retirement(prepared.request(&cancellation), &mut retirement_verifier(&prepared), &mut retirement_owner)
    .expect_err("a reopened durable task must not be retired using only an external absence claim");
  assert_eq!(error.code(), "semantic_task_root_exclusion_required");
  assert!(error.committed_receipt().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_root_exclusion_outlives_only_capture_and_releases_memory() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("root-exclusion-lifetime", None, [1; 16], algorithm, 0);
    flush_mark_fixture(&publisher);
    let root = digest_parts(algorithm, &[b"unreferenced root"]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    let before = fs::read(&path).unwrap();
    let proof = publisher.qualify_semantic_task_root_exclusion(&mark, &root).expect("qualify one unreferenced root");
    drop(mark);
    drop(capture);
    drop(protection);
    assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, 0);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    assert!(retained > 0 && retained <= 8192, "retain only bounded decision evidence, not snapshot/bitmap: {retained}");
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(proof);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_semantic_task_root_exclusion_requires_proof_at_physical_reclaim() {
  let algorithm = HashAlgorithm::Blake3_256;
  let inventory = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let database_id: [u8; 16] = decode_physical_inventory_manifest_v1(&inventory, algorithm).unwrap().database_id.try_into().unwrap();
  let (_directory, path, _coordinator, mut publisher) = create_environment_for_database("task-root-reclaim", None, database_id);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let mut verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let retired = publisher.publish_root_retirement(retirement.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap();
  assert!(!retired.idempotent);
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &inventory, &cancellation, &memory);
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 2;
  header.required_writer_capabilities[3] |= 2;
  header.slot_sequence += 1;
  write_redundant_header(&publisher, &header);
  let before = fs::read(&path).unwrap();
  let error = publisher
    .publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner)
    .expect_err("a task-capable database must require native task evidence before physical reclaim");
  assert_eq!(error.code(), "semantic_task_root_exclusion_required");
  assert!(error.committed_receipt().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}
