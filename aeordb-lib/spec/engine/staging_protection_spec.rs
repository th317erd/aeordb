use super::*;

fn staging_memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap())
}

#[test]
fn native_staging_protection_is_accounted_without_holding_the_publication_mutex() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("staging-protection-lifetime", None, [0x31; 16], algorithm, 0);
    let memory = staging_memory();
    let cancellation = CancellationToken::new();
    let before = fs::read(&path).unwrap();
    let first = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let one = memory.snapshot().unwrap().reserved_bytes;
    assert!(one > 0 && one <= 1024);
    assert!(publisher.root_state.try_lock().is_ok(), "staging must not hold the publication mutex while work runs");
    let second = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 2 * one);
    drop(first);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, one);
    drop(second);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_staging_protection_pre_cancellation_creates_no_owned_state_or_writes() {
  let (_directory, path, _coordinator, publisher) = create_environment("staging-protection-cancelled", None);
  let memory = staging_memory();
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let before = fs::read(&path).unwrap();
  let error = publisher.acquire_staging_protection(&memory, &cancellation).unwrap_err();
  assert_eq!(error.code(), "staging_protection_cancelled");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert!(publisher.root_state.try_lock().is_ok());
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_staging_protection_memory_refusal_releases_admission_and_retries() {
  let (_directory, path, _coordinator, publisher) = create_environment("staging-protection-memory", None);
  let memory = staging_memory();
  let cancellation = CancellationToken::new();
  let before = fs::read(&path).unwrap();
  let limit = memory.snapshot().unwrap().policy.unwrap().ordinary_limit_bytes();
  let pressure = memory.reserve(MemoryOwner::Task, limit, AdmissionClass::Workload).unwrap();
  let error = publisher.acquire_staging_protection(&memory, &cancellation).unwrap_err();
  assert_eq!(error.code(), "staging_protection_memory");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, limit);
  drop(pressure);
  drop(publisher.acquire_staging_protection(&memory, &cancellation).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_staging_protection_rechecks_cancellation_and_pressure_after_waiting_for_authority() {
  use crate::engine::memory_coordinator::HostMemorySample;

  for cancel_while_waiting in [true, false] {
    let (_directory, path, _coordinator, publisher) = create_environment("staging-protection-wait-recheck", None);
    let memory = staging_memory();
    let cancellation = CancellationToken::new();
    let before = fs::read(&path).unwrap();
    let authority = publisher.root_state.lock().unwrap();
    std::thread::scope(|scope| {
      let acquire = scope.spawn(|| publisher.acquire_staging_protection(&memory, &cancellation).map(drop).map_err(|error| error.code()));
      let deadline = std::time::Instant::now() + Duration::from_secs(2);
      while memory.snapshot().unwrap().reserved_bytes == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
      }
      let waiting_reservation = memory.snapshot().unwrap().reserved_bytes;
      if cancel_while_waiting {
        cancellation.cancel();
      } else {
        memory.update_host_sample(HostMemorySample { rss_bytes: 32 << 20, ..HostMemorySample::default() }).unwrap();
      }
      // Release before assertions so a setup failure cannot strand the worker.
      drop(authority);
      let result = acquire.join().unwrap();
      assert!(waiting_reservation > 0, "acquisition never reached the held authority boundary");
      assert_eq!(result.unwrap_err(), if cancel_while_waiting { "staging_protection_cancelled" } else { "staging_protection_memory" });
    });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, 0);
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    drop(publisher.acquire_staging_protection(&memory, &CancellationToken::new()).unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_staging_protection_blocks_retirement_until_the_last_guard_drops() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("staging-protection-retirement", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 32 << 20).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let first = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let second = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let locator = publisher.locator(&prepared.target_root_hash).unwrap();
  let error = publisher.publish_root_retirement(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap_err();
  assert_eq!(error.code(), "staging_protection_active");
  assert!(error.committed_receipt().is_none());
  assert!(!verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert_eq!(publisher.locator(&prepared.target_root_hash).unwrap(), locator);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(fs::read(&path).unwrap() == before, "blocked retirement changed fixture bytes");
  drop(first);
  let error = publisher.publish_root_retirement(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap_err();
  assert_eq!(error.code(), "staging_protection_active");
  assert!(!verifier.called);
  assert!(fs::read(&path).unwrap() == before, "one remaining guard failed to protect bytes");
  drop(second);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  let receipt = publisher.publish_root_retirement(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap();
  assert!(!receipt.idempotent);
  assert!(verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(publisher.locator(&prepared.target_root_hash).unwrap(), locator);
}

#[test]
fn native_staging_protection_keeps_normal_publication_and_reads_available() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("staging-protection-publication", None, [0x31; 16], algorithm, 0);
    let memory = staging_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let initial = request_for_database_and_algorithm([0x31; 16], algorithm);
    let first = publisher.publish(&initial).unwrap();
    // Reuse the already-published empty directory as a real child, making a
    // distinct successor without introducing a synthetic dangling reference.
    let tree_value = serialize_child_entries(
      &[ChildEntry {
        entry_type: EntryTypeV4::DirectoryIndex.to_u8(),
        hash: initial.namespace_tree.root_hash.clone(),
        total_size: 0,
        created_at: initial.created_at_ms as i64,
        updated_at: initial.created_at_ms as i64,
        name: "staged-directory".to_string(),
        content_type: None,
        virtual_time: 1,
        node_id: 1,
      }],
      algorithm.hash_length(),
    )
    .unwrap();
    let successor = publisher
      .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
        database_id: initial.database_id,
        transaction_id: [0x73; 16],
        created_at_ms: initial.created_at_ms + 1,
        expected_head_hash: first.namespace_root.root_hash.clone(),
        namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:", &tree_value]), stored_value: tree_value },
        semantic_state: initial.semantic_state,
        required_capabilities: initial.required_capabilities,
        typed_closure_digest: initial.typed_closure_digest,
        authority_identity: initial.authority_identity,
      })
      .unwrap();
    assert!(!successor.idempotent);
    assert_ne!(successor.namespace_root.root_hash, first.namespace_root.root_hash);
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, successor.namespace_root.root_hash);
    publisher.load_selected_semantic_authority().unwrap();
    let before_read = fs::read(&path).unwrap();
    publisher.load_selected_semantic_authority().unwrap();
    assert!(fs::read(&path).unwrap() == before_read);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    let observation = publisher.observe().unwrap();
    drop(publisher);
    let reopened = V4FirstAuthorityPublisher::open(&path).unwrap();
    assert_eq!(reopened.observe().unwrap(), observation);
  }
}

#[test]
fn native_staging_protection_count_exhaustion_and_release_corruption_fail_closed() {
  let (_directory, path, _coordinator, publisher) = create_environment("staging-protection-accounting", None);
  let memory = staging_memory();
  let cancellation = CancellationToken::new();
  let before = fs::read(&path).unwrap();
  publisher.root_state.lock().unwrap().active_staging_protections = u64::MAX;
  let error = publisher.acquire_staging_protection(&memory, &cancellation).unwrap_err();
  assert_eq!(error.code(), "staging_protection_unavailable");
  assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, u64::MAX);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  publisher.root_state.lock().unwrap().active_staging_protections = 0;
  drop(publisher.acquire_staging_protection(&memory, &cancellation).unwrap());
  publisher.root_state.lock().unwrap().ensure_no_staging_protection().unwrap();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  publisher.root_state.lock().unwrap().active_staging_protections = 0;
  drop(protection);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(publisher.root_state.lock().unwrap().ensure_no_staging_protection().unwrap_err().code(), "staging_protection_accounting");
  assert_eq!(publisher.acquire_staging_protection(&memory, &cancellation).unwrap_err().code(), "staging_protection_unavailable");
  assert!(fs::read(&path).unwrap() == before);
}

#[test]
fn native_staging_protection_poisoned_owner_releases_memory_without_reopening_reclamation() {
  let (_directory, path, _coordinator, publisher) = create_environment("staging-protection-poisoned", None);
  let memory = staging_memory();
  let cancellation = CancellationToken::new();
  let before = fs::read(&path).unwrap();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _authority = publisher.root_state.lock().unwrap();
    panic!("injected authority poisoning");
  }));
  assert!(poison.is_err());
  assert!(matches!(
    publisher.acquire_staging_protection(&memory, &cancellation),
    Err(StagingProtectionErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned))
  ));
  drop(protection);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert!(publisher.root_state.lock().is_err());
  assert!(matches!(publisher.publish(&request()), Err(FirstAuthorityPublicationErrorV1::StateLockPoisoned)));
  assert!(fs::read(&path).unwrap() == before);
}
