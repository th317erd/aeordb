//! True restart, hash-width and literal persisted Ready task expectations.
use super::*;
use super::advance_boundary::{resumed_request, start_fixture_compilation};

#[test]
fn native_task_advance_ready_reopens_with_exact_task_bytes_for_all_hashes() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let mut expected_task = Vec::new();
    let mut expected_checkpoint = Vec::new();
    let mut expected_companion = Vec::new();
    let mut expected_head = Vec::new();
    let mut candidate = Vec::new();
    let mut tree = Vec::new();
    let mut identity = [0; 24];
    identity[..16].fill(2);
    identity[16..].copy_from_slice(&3u64.to_le_bytes());
    let (_directory, path) = with_initial_task_for_work_algorithm(algorithm, |mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      expected_task = fixture
        .publisher
        .load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16])
        .unwrap()
        .unwrap()
        .bytes;
      let request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      assert_eq!(work.advance_compilation(request, fixture.retirement).unwrap().publication.control_sequence, 5);
      expected_checkpoint = fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &identity)
        .unwrap()
        .unwrap()
        .bytes;
      expected_companion = fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &identity)
        .unwrap()
        .unwrap()
        .bytes;
      // Literal ASMT offsets, not a production serializer/decoder round trip.
      expected_task[16..24].copy_from_slice(&5u64.to_le_bytes());
      expected_task[32 + 88..32 + 96].copy_from_slice(&(request.publication_timestamp_ms as i64).to_le_bytes());
      expected_task[32 + 96..32 + 98].copy_from_slice(&4u16.to_le_bytes());
      expected_task[32 + 100..32 + 108].copy_from_slice(&3u64.to_le_bytes());
      expected_task[32 + 112..32 + 112 + algorithm.hash_length()].copy_from_slice(&digest_parts(algorithm, &[&expected_checkpoint]));
      crc(&mut expected_task);
      assert_eq!(
        fixture
          .publisher
          .load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16])
          .unwrap()
          .unwrap()
          .bytes,
        expected_task
      );
      assert_eq!(&expected_checkpoint[32 + 88..32 + 90], &4u16.to_le_bytes());
      assert_eq!(&expected_checkpoint[32 + 144..32 + 152], &0u64.to_le_bytes());
      let selected = observe_work_task(&fixture);
      candidate = selected.checkpoint().unwrap().unwrap().candidate_namespace_root.unwrap().to_vec();
      expected_head = fixture.publisher.observe().unwrap().selected.header.head_hash;
      tree = fixture.tree.to_vec();
    });
    let (_coordinator, publisher) = reopen(&path);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let before = fs::read(&path).unwrap();
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let selected = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      assert_eq!(selected.task().unwrap().unwrap().state, SemanticMutationTaskStateV1::ReadyToActivate);
      assert!(!selected.task().unwrap().unwrap().pins_released);
      assert_eq!(
        publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes,
        expected_task
      );
      assert_eq!(
        publisher
          .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &identity)
          .unwrap()
          .unwrap()
          .bytes,
        expected_checkpoint
      );
      assert_eq!(
        publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &identity).unwrap().unwrap().bytes,
        expected_companion
      );
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, expected_head);
      assert!(publisher.load_immutable_system_control(SystemControlKindV1::RootAdmissionCommit, &[1; 16], &candidate).unwrap().is_none());
      let capture = protection.capture_semantic_mutation_inventory(work_request(1).inventory_bounds, &memory, &cancellation).unwrap();
      let mut retained = std::collections::BTreeSet::new();
      capture
        .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 3, selection_bounds(), |entry| {
          retained.insert(entry.hash.clone());
          Ok(())
        })
        .unwrap();
      assert!(retained.contains(&candidate));
      assert!(retained.contains(&expected_head));
      let bounds = advance_request(&tree, 1).compiler_bounds;
      let error = capture.admit_captured_semantic_compiler_progress(&[2; 16], 3, bounds).err().expect("Ready is not unfinished progress");
      assert!(error.to_string().contains("semantic_catalog_progress_phase"), "{error}");
      drop(capture.admit_captured_semantic_compiler_output(&[2; 16], 3, bounds).unwrap());
      let summary = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 64, 64, 64, 4 << 20).unwrap().unwrap();
      let mut retirement = RetirementJournalOwnerV1::resume_chain(
        algorithm,
        [1; 16],
        &summary,
        RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
        &cancellation,
        &memory,
      )
      .unwrap();
      let error = protection
        .begin_semantic_task_work(
          &selected,
          work_request(selected.header().selected.header.updated_at_ms + 1),
          &memory,
          &cancellation,
          &mut retirement,
        )
        .unwrap_err();
      assert_eq!(error.code(), "semantic_task_work_phase");
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
