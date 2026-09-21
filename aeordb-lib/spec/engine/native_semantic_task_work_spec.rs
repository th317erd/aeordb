//! Failing-first fenced work and first compiler checkpoint qualification.
//! Intended child of native_initial_semantic_task_selection_spec.rs.
#[path = "native_semantic_task_work_boundary_spec.rs"]
mod boundary;
use super::*;
use crate::engine::memory_coordinator::MemoryPolicy;
use crate::engine::v4::semantic_mutation_control::SemanticMutationPhaseV1;

fn work_request(timestamp: u64) -> NativeSemanticTaskWorkRequestV1 {
  NativeSemanticTaskWorkRequestV1 {
    holder_boot_id: [8; 16],
    acquired_at_ms: timestamp as i64,
    publication_timestamp_ms: timestamp + 1,
    monotonic_now_ms: 20_000,
    inventory_bounds: NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 256 << 10, ..capture_bounds() },
    graph_bounds: selection_bounds(),
    maximum_workspace_bytes: 16 << 20,
  }
}

fn start_request(tree: &[u8], timestamp: u64) -> NativeSemanticTaskCompilerStartRequestV1 {
  NativeSemanticTaskCompilerStartRequestV1 {
    compiler_bounds: NativeSemanticCompilerProgressBoundsV1 {
      sources: validation_bounds(tree),
      maximum_compiler_workspace_bytes: 64 << 20,
      maximum_alias_snapshot_bytes: 4 << 20,
      maximum_semantic_decode_workspace_bytes: 32 << 20,
    },
    publication_timestamp_ms: timestamp,
    monotonic_now_ms: 30_000,
    maximum_workspace_bytes: 16 << 20,
  }
}

#[test]
fn native_task_work_reopens_initial_selection_and_selects_a_real_first_compiler_checkpoint_for_all_hashes() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-work-first-compiler-checkpoint", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let root = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    enable_node_staging(&publisher);
    seed_union_generation(&publisher);
    // Same policy as the existing retained-prefix native fixtures; compilation
    // owns independent catalog, registry and source-admission reservations.
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let mut expected_task;
    let (mut expected_checkpoint, mut expected_companion);
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let workspace = tempfile::tempdir().unwrap();
      let staged = capture
        .prepare_and_stage_semantic_source_union(
          NativeSemanticSourceUnionRequestV1 {
            expected_base_root: &root,
            requested_directory_root: &initial.namespace_tree.root_hash,
            replacements: &[],
            workspace_parent: workspace.path(),
            bounds: union_bounds(&initial.namespace_tree.root_hash),
          },
          staging_request(),
        )
        .unwrap();
      let checkpoint = request(staged.source_union().captured_header().updated_at_ms + 1);
      (expected_checkpoint, expected_companion) = expected_pair(staged.source_union(), checkpoint, 0);
      staged.stage_initial_checkpoint(checkpoint).unwrap();
      let input = selection_request(checkpoint);
      expected_task = expected_initial_task(&staged, input);
      let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
      staged.select_initial_task(input, &mut retirement).unwrap();
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let mut next_identity = [0; 24];
    next_identity[..16].copy_from_slice(&[2; 16]);
    next_identity[16..].copy_from_slice(&2u64.to_le_bytes());
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let observed = publisher
        .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
          database_id: &[1; 16],
          task_id: &[2; 16],
          memory: &memory,
          cancellation: &cancellation,
        })
        .unwrap();
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      // Initial A-slot selection did not replace any incarnation: no chain has
      // yet been published. Later restart cases must reconstruct/resume it.
      assert!(publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 16, 1 << 20).unwrap().is_none());
      let mut retirement = selection_retirement(algorithm, &memory, &cancellation);
      let work = protection
        .begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement)
        .expect("real selected Captured work must acquire a fresh durable task fence after reopen");
      assert_eq!(work.reserved_checkpoint_sequence(), 2);
      assert_eq!(work.receipt().control_sequence, 2);
      assert_eq!(work.receipt().selected_slot, SystemControlSlotV1::B);
      assert!(!work.receipt().idempotent);
      expected_task[16..24].copy_from_slice(&2u64.to_le_bytes());
      expected_task[32 + 48..32 + 64].fill(8);
      expected_task[32 + 64..32 + 72].copy_from_slice(&2u64.to_le_bytes());
      expected_task[32 + 88..32 + 96].copy_from_slice(&input.acquired_at_ms.to_le_bytes());
      crc(&mut expected_task);
      assert_eq!(
        publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes,
        expected_task
      );
      assert!(publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &next_identity)
        .unwrap()
        .is_none());
      let start = start_request(&initial.namespace_tree.root_hash, input.publication_timestamp_ms + 20);
      let selected =
        work.start_compilation(start, &mut retirement).expect("compiler output must be durably selected, not only returned in memory");
      assert_eq!(selected.control_sequence, 3);
      assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
      assert!(selected.replaced_slot);
      assert!(selected.retirement_hard_publication_sequence.is_some());
      let checkpoint = publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &next_identity)
        .unwrap()
        .unwrap();
      let width = algorithm.hash_length();
      let catalog = &checkpoint.bytes[200 + 2 * width..200 + 3 * width];
      assert!(catalog.iter().any(|byte| *byte != 0));
      // Independent offsets preserve every immutable capture field. The catalog
      // root is separately admitted below against actual retained input semantics.
      expected_checkpoint[32 + 32..32 + 40].copy_from_slice(&2u64.to_le_bytes());
      expected_checkpoint[32 + 88..32 + 90].copy_from_slice(&2u16.to_le_bytes());
      expected_checkpoint[32 + 112..32 + 120].copy_from_slice(&1u64.to_le_bytes());
      expected_checkpoint[32 + 120..32 + 128].copy_from_slice(&1u64.to_le_bytes());
      expected_checkpoint[200 + 2 * width..200 + 3 * width].copy_from_slice(catalog);
      crc(&mut expected_checkpoint);
      assert_eq!(checkpoint.bytes, expected_checkpoint);
      let digest = digest_parts(algorithm, &[&expected_checkpoint]);
      expected_companion[32 + 32..32 + 40].copy_from_slice(&2u64.to_le_bytes());
      expected_companion[32 + 112 + 5 * width..32 + 112 + 6 * width].copy_from_slice(&digest);
      crc(&mut expected_companion);
      assert_eq!(
        publisher
          .load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &next_identity)
          .unwrap()
          .unwrap()
          .bytes,
        expected_companion
      );
      expected_task[16..24].copy_from_slice(&3u64.to_le_bytes());
      expected_task[32 + 88..32 + 96].copy_from_slice(&(start.publication_timestamp_ms as i64).to_le_bytes());
      expected_task[32 + 96..32 + 98].copy_from_slice(&3u16.to_le_bytes());
      expected_task[32 + 100..32 + 108].copy_from_slice(&2u64.to_le_bytes());
      expected_task[32 + 112..32 + 112 + width].copy_from_slice(&digest);
      crc(&mut expected_task);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let before = fs::read(&path).unwrap();
    {
      assert_eq!(
        publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes,
        expected_task
      );
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, root);
      assert_eq!(
        publisher
          .load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[])
          .unwrap()
          .unwrap()
          .control_sequence,
        10
      );
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(work_request(1).inventory_bounds, &memory, &cancellation).unwrap();
      let progress = capture
        .admit_captured_semantic_compiler_progress(&[2; 16], 2, start_request(&initial.namespace_tree.root_hash, 1).compiler_bounds)
        .unwrap();
      assert_eq!(progress.phase(), SemanticMutationPhaseV1::Compiling);
      assert_eq!(progress.configuration_count(), 0);
      assert_eq!(progress.construction_mode(), SemanticCompilerConstructionModeV1::Fresh);
      drop(progress);
      let mut seen = std::collections::BTreeSet::new();
      let retention = capture
        .visit_captured_semantic_task_retention_entries(
          NativeSemanticTaskRetentionBoundsV1 { maximum_work: 16384, maximum_read_bytes: 64 << 20, graphs: selection_bounds() },
          |entry| {
            seen.insert(entry.hash.clone());
            Ok(())
          },
        )
        .unwrap();
      assert!(retention.complete);
      assert_eq!(retention.tasks, 1);
      assert!(seen.contains(&root));
      for kind in [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture] {
        let path = system_control_path(kind, &next_identity, SystemControlSlotV1::Immutable).unwrap();
        assert!(seen.contains(&first_authority_file_path_hash(&path, algorithm)));
      }
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
