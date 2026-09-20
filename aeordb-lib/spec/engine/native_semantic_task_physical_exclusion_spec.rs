//! Native quarantine boundary tests; not a service GC scheduler qualification.
#[path = "native_semantic_task_physical_exclusion_boundary_spec.rs"]
mod boundary;
use super::*;

fn quarantine_capability_case(capabilities: Option<(bool, bool)>) {
  let (_directory, path, _coordinator, mut publisher) = create_environment("task-quarantine-final", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 32 << 20).unwrap()));
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut owner, &cancellation, &memory);
  let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: prepared.manifest.key.clone(),
    expected_request: prepared.authority_snapshot.clone(),
    snapshot: prepared.authority_snapshot.clone(),
  };
  if let Some((reader, writer)) = capabilities {
    let mut header = publisher.observe().unwrap().selected.header;
    header.slot_sequence += 1;
    if reader {
      header.required_reader_capabilities[3] |= 2;
    }
    if writer {
      header.required_writer_capabilities[3] |= 2;
    }
    write_redundant_header(&publisher, &header);
  }
  drop(publisher);
  let (_coordinator, publisher) = reopen(&path);
  let before = fs::read(&path).unwrap();
  let result = publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut owner);
  if capabilities.is_some() {
    let error = result.expect_err("task-capable quarantine selection requires native exclusion, not only an external absence claim");
    assert_eq!(error.code(), "semantic_task_physical_exclusion_required");
    assert!(error.committed_receipt().is_none());
    assert!(!verifier.called);
    assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.prior_manifest_key);
    assert_eq!(fs::read(&path).unwrap(), before);
  } else {
    let receipt = result.unwrap();
    assert!(!receipt.idempotent);
    assert!(verifier.called);
    assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.manifest.key);
    verifier.called = false;
    // Existing immutable-publication retry settles buffered KV state before
    // testing idempotency; qualify unchanged bytes against that explicit base.
    flush_mark_fixture(&publisher);
    let before_retry = fs::read(&path).unwrap();
    let retry = publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut owner).unwrap();
    assert!(retry.idempotent);
    assert!(!verifier.called);
    assert_eq!(fs::read(&path).unwrap(), before_retry);
  }
}

#[test]
fn native_semantic_task_physical_exclusion_characterizes_legacy_quarantine_retry() {
  quarantine_capability_case(None);
}

#[test]
fn native_semantic_task_physical_exclusion_requires_proof_before_quarantine_selection() {
  for masks in [(true, false), (false, true), (true, true)] {
    quarantine_capability_case(Some(masks));
  }
}

#[test]
fn native_semantic_task_physical_exclusion_requires_proof_before_sweep_removal() {
  for masks in [(true, false), (false, true), (true, true)] {
    guarded_sweep_fixture_for_task_capability(Some(masks));
  }
}

#[test]
fn native_semantic_task_physical_exclusion_qualifies_empty_quarantine_at_captured_frontier() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("task-quarantine-proof", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 32 << 20).unwrap()));
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut owner, &cancellation, &memory);
  let mut header = publisher.observe().unwrap().selected.header;
  header.slot_sequence += 1;
  header.required_reader_capabilities[3] |= 2;
  header.required_writer_capabilities[3] |= 2;
  write_redundant_header(&publisher, &header);
  flush_mark_fixture(&publisher);
  let before = fs::read(&path).unwrap();
  let proof = {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    assert_eq!(mark.summary().marked_slots, 0);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut wrong_key = prepared.manifest.clone();
    wrong_key.key.fill(0xff);
    let mut truncated = prepared.manifest.clone();
    truncated.value.pop();
    for artifact in [&wrong_key, &truncated, &prepared.lifecycle_manifest] {
      assert!(publisher
        .qualify_semantic_task_quarantine_exclusion(
          &mark,
          artifact,
          NativeSemanticTaskPhysicalExclusionBoundsV1 {
            maximum_support_artifacts: 1024,
            maximum_work: 100_000,
            maximum_read_bytes: 64 << 20,
          },
        )
        .is_err());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
    publisher
      .qualify_semantic_task_quarantine_exclusion(
        &mark,
        &prepared.manifest,
        NativeSemanticTaskPhysicalExclusionBoundsV1 {
          maximum_support_artifacts: 1024,
          maximum_work: 100_000,
          maximum_read_bytes: 64 << 20,
        },
      )
      .expect("empty task contribution must qualify the exact empty quarantine manifest")
  };
  assert_eq!(fs::read(&path).unwrap(), before, "qualification cannot publish or flush");
  let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: prepared.manifest.key.clone(),
    expected_request: prepared.authority_snapshot.clone(),
    snapshot: prepared.authority_snapshot.clone(),
  };
  let mut request = prepared.request(&cancellation);
  request.task_exclusion = Some(&proof);
  let receipt = publisher.publish_physical_quarantine(request, &mut verifier, &mut owner).unwrap();
  assert!(!receipt.idempotent);
  assert!(verifier.called);
  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.manifest.key);
  verifier.called = false;
  let before = fs::read(&path).unwrap();
  assert_eq!(
    publisher.publish_physical_quarantine(request, &mut verifier, &mut owner).unwrap_err().code(),
    "semantic_task_physical_exclusion_stale"
  );
  assert!(!verifier.called);
  assert_eq!(fs::read(&path).unwrap(), before);
  flush_mark_fixture(&publisher);
  let fresh = {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    publisher
      .qualify_semantic_task_quarantine_exclusion(
        &mark,
        &prepared.manifest,
        NativeSemanticTaskPhysicalExclusionBoundsV1 {
          maximum_support_artifacts: 1024,
          maximum_work: 100_000,
          maximum_read_bytes: 64 << 20,
        },
      )
      .unwrap()
  };
  request.task_exclusion = Some(&fresh);
  let before = fs::read(&path).unwrap();
  assert!(publisher.publish_physical_quarantine(request, &mut verifier, &mut owner).unwrap().idempotent);
  assert!(!verifier.called);
  assert_eq!(fs::read(&path).unwrap(), before);
}
