use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

fn root_proof<'publisher>(
  publisher: &'publisher V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  root: &[u8],
) -> NativeSemanticTaskRootExclusionV1<'publisher> {
  flush_mark_fixture(publisher);
  let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), memory, cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  publisher.qualify_semantic_task_root_exclusion(&mark, root).unwrap()
}

fn declare_task_capability(publisher: &V4FirstAuthorityPublisher) {
  let mut header = publisher.observe().unwrap().selected.header;
  header.slot_sequence += 1;
  header.required_reader_capabilities[3] |= 2;
  header.required_writer_capabilities[3] |= 2;
  write_redundant_header(publisher, &header);
}

#[test]
fn native_semantic_task_root_exclusion_retirement_requires_fresh_exact_target_evidence() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-retirement", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
  declare_task_capability(&publisher);
  let proof = root_proof(&publisher, &memory, &cancellation, &prepared.target_root_hash);
  let mut request = prepared.request(&cancellation);
  request.task_exclusion = Some(&proof);
  let receipt = publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap();
  assert!(!receipt.idempotent);
  let before = fs::read(&path).unwrap();
  assert_eq!(
    publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap_err().code(),
    "semantic_task_root_exclusion_stale"
  );
  assert_eq!(fs::read(&path).unwrap(), before);
  let fresh = root_proof(&publisher, &memory, &cancellation, &prepared.target_root_hash);
  request.task_exclusion = Some(&fresh);
  let retry_before = fs::read(&path).unwrap();
  assert!(publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap().idempotent);
  assert_eq!(fs::read(&path).unwrap(), retry_before);
}

#[test]
fn native_semantic_task_root_exclusion_cannot_cross_owners_or_root_identity() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-owner", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (_other_directory, _other_path, _other_coordinator, other) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-other", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
  declare_task_capability(&publisher);
  let wrong_target = root_proof(&publisher, &memory, &cancellation, &[0xff; 32]);
  let wrong_owner = root_proof(&other, &memory, &cancellation, &prepared.target_root_hash);
  let before = fs::read(&path).unwrap();
  for (proof, code) in [(&wrong_target, "semantic_task_root_exclusion_target"), (&wrong_owner, "semantic_task_root_exclusion_owner")] {
    let mut request = prepared.request(&cancellation);
    request.task_exclusion = Some(proof);
    assert_eq!(publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap_err().code(), code);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_root_exclusion_qualifier_rejects_retained_roots_and_foreign_marks() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("root-exclusion-retained", None, [1; 16], algorithm, 0);
    let (_other_directory, _other_path, _other_coordinator, other) =
      create_environment_for_algorithm_at_kv_stage("root-exclusion-foreign", None, [1; 16], algorithm, 0);
    let (_, base, _) = seed_captured_graph(&publisher);
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    assert_eq!(publisher.qualify_semantic_task_root_exclusion(&mark, &base).unwrap_err().code(), "semantic_task_root_retained");
    assert_eq!(other.qualify_semantic_task_root_exclusion(&mark, &base).unwrap_err().code(), "semantic_task_root_exclusion_owner");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_root_exclusion_allows_unrelated_root_with_an_active_task() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-unrelated", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
  let mut next = successor_request(&publisher, 0x96, "unused");
  let body = b"unrelated retained file";
  let (file, _) = publish_namespace_configuration(&publisher, "/retained.json", body);
  let directory = publish_namespace_directory(
    &publisher,
    vec![NamespaceFixtureChild {
      name: "retained.json".into(),
      kind: EntryTypeV4::FileRecord,
      key: file,
      size: body.len() as u64,
      content_type: Some("application/json"),
    }],
  );
  next.semantic_state = request_for_database([1; 16]).semantic_state;
  let loaded = publisher.load_immutable_entity_bounded(&directory, 1 << 20).unwrap().unwrap();
  next.namespace_tree = PreparedNamespaceTreeV0 { root_hash: directory, stored_value: loaded.stored_value };
  let next = publisher.publish_successor_authority(&next).unwrap();
  assert_ne!(next.namespace_root.root_hash, prepared.target_root_hash);
  seed_retirement_task(&publisher, &next.namespace_root.root_hash);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let proof = root_proof(&publisher, &memory, &cancellation, &prepared.target_root_hash);
  let mut request = prepared.request(&cancellation);
  request.task_exclusion = Some(&proof);
  let receipt = publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap();
  assert_eq!(receipt.namespace_root_hash, prepared.target_root_hash);
  assert!(!receipt.idempotent);
  flush_mark_fixture(&publisher);
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  assert_eq!(capture.mark_captured_semantic_tasks(mark_bounds()).unwrap().summary().retention.tasks, 1);
}

#[test]
fn native_semantic_task_root_exclusion_identity_role_and_allocation_are_bounded() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("root-exclusion-identity", None, [1; 16], algorithm, 0);
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let width = algorithm.hash_length();
    for root in [Vec::new(), vec![0; width], vec![1; width - 1], vec![1; width + 1]] {
      assert_eq!(publisher.qualify_semantic_task_root_exclusion(&mark, &root).unwrap_err().code(), "semantic_task_root_exclusion_identity");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    let root = digest_parts(algorithm, &[b"absent root"]);
    let (proof, allocations) =
      allocation_probe::measure_nth(width, usize::MAX, || publisher.qualify_semantic_task_root_exclusion(&mark, &root));
    drop(proof.unwrap());
    assert!(allocations.matching_requests > 0);
    let (result, failure) =
      allocation_probe::measure_nth(width, allocations.matching_requests, || publisher.qualify_semantic_task_root_exclusion(&mark, &root));
    assert!(failure.injected_failure);
    assert_eq!(result.unwrap_err().code(), "semantic_task_root_exclusion_allocation");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    drop(publisher.qualify_semantic_task_root_exclusion(&mark, &root).unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
    seed_files(&publisher, &[("/ordinary".into(), "application/octet-stream", b"ordinary")]);
    flush_mark_fixture(&publisher);
    let next = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = next.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    let file_key = first_authority_file_path_hash("/ordinary", algorithm);
    assert_eq!(publisher.qualify_semantic_task_root_exclusion(&mark, &file_key).unwrap_err().code(), "semantic_task_root_exclusion_role");
  }
}

#[test]
fn native_semantic_task_root_exclusion_final_gate_rechecks_its_own_cancellation_and_memory() {
  for pressure in [false, true] {
    let (_directory, path, _coordinator, mut publisher) =
      create_environment_for_algorithm_at_kv_stage("root-exclusion-interruption", None, [1; 16], HashAlgorithm::Blake3_256, 0);
    let request_memory = Arc::new(observation_memory());
    let request_cancellation = CancellationToken::new();
    let mut owner = root_retirement_owner(&request_memory, &request_cancellation);
    let prepared =
      prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &request_cancellation, &request_memory, true, [1; 16]);
    declare_task_capability(&publisher);
    let proof_memory = observation_memory();
    let proof_cancellation = CancellationToken::new();
    let proof = root_proof(&publisher, &proof_memory, &proof_cancellation, &prepared.target_root_hash);
    let retained = proof_memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    if pressure {
      proof_memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
    } else {
      proof_cancellation.cancel();
    }
    let mut request = prepared.request(&request_cancellation);
    request.task_exclusion = Some(&proof);
    let error = publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap_err();
    assert!(matches!(error, RootRetirementPublicationErrorV1::TaskRetention(_)));
    assert_eq!(error.code(), if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" });
    assert!(error.source().is_some());
    assert!(error.committed_receipt().is_none());
    assert_eq!(proof_memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(proof);
    assert_eq!(proof_memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_semantic_task_root_exclusion_any_publication_invalidates_the_captured_frontier() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-frontier", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
  declare_task_capability(&publisher);
  let proof = root_proof(&publisher, &memory, &cancellation, &prepared.target_root_hash);
  let root_before = publisher.locator(&prepared.target_root_hash).unwrap();
  seed_files(&publisher, &[("/ordinary".into(), "application/octet-stream", b"changed")]);
  assert_eq!(publisher.locator(&prepared.target_root_hash).unwrap(), root_before);
  let before = fs::read(&path).unwrap();
  let mut request = prepared.request(&cancellation);
  request.task_exclusion = Some(&proof);
  assert_eq!(
    publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap_err().code(),
    "semantic_task_root_exclusion_stale"
  );
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_root_exclusion_physical_reclaim_preserves_qualified_retry_and_original_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let inventory = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let database_id: [u8; 16] = decode_physical_inventory_manifest_v1(&inventory, algorithm).unwrap().database_id.try_into().unwrap();
  let (_directory, path, _coordinator, mut publisher) = create_environment_for_database("root-exclusion-reclaim-retry", None, database_id);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, database_id);
  let mut verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let retired = publisher.publish_root_retirement(retirement.request(&cancellation), &mut verifier, &mut owner).unwrap();
  assert!(!retired.idempotent);
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &inventory, &cancellation, &memory);
  declare_task_capability(&publisher);
  let proof_memory = observation_memory();
  let proof_cancellation = CancellationToken::new();
  let proof = root_proof(&publisher, &proof_memory, &proof_cancellation, &retirement.target_root_hash);
  let before = fs::read(&path).unwrap();
  let mut request = reclaim.request(&cancellation, &retirement.pin_coordinator);
  request.task_exclusion = Some(&proof);
  proof_memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
  let error = publisher.publish_root_reclaim(request, &mut owner).unwrap_err();
  assert!(matches!(error, RootReclaimPublicationErrorV1::TaskRetention(_)));
  assert!(error.source().is_some());
  assert_eq!(error.code(), "semantic_task_observation_memory");
  assert!(error.committed_receipt().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
  proof_memory.update_host_sample(HostMemorySample::default()).unwrap();
  let receipt = publisher.publish_root_reclaim(request, &mut owner).unwrap();
  assert!(!receipt.idempotent);
  assert_eq!(receipt.namespace_root_hash, retirement.target_root_hash);
  let before = fs::read(&path).unwrap();
  assert_eq!(publisher.publish_root_reclaim(request, &mut owner).unwrap_err().code(), "semantic_task_root_exclusion_stale");
  assert_eq!(fs::read(&path).unwrap(), before);
  let fresh = root_proof(&publisher, &proof_memory, &proof_cancellation, &retirement.target_root_hash);
  request.task_exclusion = Some(&fresh);
  let before = fs::read(&path).unwrap();
  assert!(publisher.publish_root_reclaim(request, &mut owner).unwrap().idempotent);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_root_exclusion_new_task_selection_invalidates_prior_absence() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-new-task", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
  declare_task_capability(&publisher);
  let proof = root_proof(&publisher, &memory, &cancellation, &prepared.target_root_hash);
  seed_retirement_task(&publisher, &prepared.target_root_hash);
  let before = fs::read(&path).unwrap();
  let mut request = prepared.request(&cancellation);
  request.task_exclusion = Some(&proof);
  assert_eq!(
    publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap_err().code(),
    "semantic_task_root_exclusion_stale"
  );
  assert_eq!(fs::read(&path).unwrap(), before);
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  assert_eq!(
    publisher.qualify_semantic_task_root_exclusion(&mark, &prepared.target_root_hash).unwrap_err().code(),
    "semantic_task_root_retained"
  );
}

#[test]
fn native_semantic_task_root_exclusion_released_task_allows_previously_retained_root() {
  let (_directory, path, _coordinator, mut publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-released", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let memory = Arc::new(observation_memory());
  let cancellation = CancellationToken::new();
  let mut owner = root_retirement_owner(&memory, &cancellation);
  let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
  seed_retirement_task(&publisher, &prepared.target_root_hash);
  let mut task =
    publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes;
  let sequence = u64::from_le_bytes(task[16..24].try_into().unwrap()) + 1;
  task[16..24].copy_from_slice(&sequence.to_le_bytes());
  task[128..130].copy_from_slice(&9u16.to_le_bytes());
  task[130..132].copy_from_slice(&1u16.to_le_bytes());
  crc(&mut task);
  seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task)]);
  flush_mark_fixture(&publisher);
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let proof = root_proof(&publisher, &memory, &cancellation, &prepared.target_root_hash);
  let mut request = prepared.request(&cancellation);
  request.task_exclusion = Some(&proof);
  let receipt = publisher.publish_root_retirement(request, &mut retirement_verifier(&prepared), &mut owner).unwrap();
  assert!(!receipt.idempotent);
}

#[test]
fn native_semantic_task_root_exclusion_either_capability_mask_requires_native_evidence() {
  for (reader, writer) in [(true, false), (false, true), (true, true)] {
    let (_directory, path, _coordinator, mut publisher) =
      create_environment_for_algorithm_at_kv_stage("root-exclusion-capabilities", None, [1; 16], HashAlgorithm::Blake3_256, 0);
    let memory = Arc::new(observation_memory());
    let cancellation = CancellationToken::new();
    let mut owner = root_retirement_owner(&memory, &cancellation);
    let prepared = prepare_guarded_root_retirement_for_database(&mut publisher, &mut owner, &cancellation, &memory, true, [1; 16]);
    let mut header = publisher.observe().unwrap().selected.header;
    if reader {
      header.required_reader_capabilities[3] |= 2;
    }
    if writer {
      header.required_writer_capabilities[3] |= 2;
    }
    write_redundant_header(&publisher, &header);
    let before = fs::read(&path).unwrap();
    assert_eq!(
      publisher
        .publish_root_retirement(prepared.request(&cancellation), &mut retirement_verifier(&prepared), &mut owner)
        .unwrap_err()
        .code(),
      "semantic_task_root_exclusion_required"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_root_exclusion_qualification_refuses_pressure_then_retries() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("root-exclusion-admission", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let limit = memory.snapshot().unwrap().policy.unwrap().ordinary_limit_bytes();
  let pressure = memory.reserve(MemoryOwner::Task, limit - retained, AdmissionClass::Workload).unwrap();
  assert_eq!(publisher.qualify_semantic_task_root_exclusion(&mark, &[1; 32]).unwrap_err().code(), "semantic_task_observation_memory");
  drop(pressure);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  drop(publisher.qualify_semantic_task_root_exclusion(&mark, &[1; 32]).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  cancellation.cancel();
  assert_eq!(publisher.qualify_semantic_task_root_exclusion(&mark, &[1; 32]).unwrap_err().code(), "semantic_task_observation_cancelled");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
