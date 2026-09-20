//! Existing bits do not bind a current locator to a captured incarnation.
use super::*;
use crate::engine::kv_pages::MAX_ENTRIES_PER_PAGE;
use crate::engine::v4::gc_mark_runtime::DenseMarkBitmapV1;

fn flush_fixture(publisher: &V4FirstAuthorityPublisher) {
  let _guard = publisher.root_state.lock().unwrap();
  publisher.lock_kv().unwrap().flush().unwrap();
}

#[test]
fn native_semantic_task_mark_characterizes_captured_locator_binding() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-mark-characterization", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    flush_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let snapshot = publisher.lock_kv().unwrap().capture_settled_snapshot().unwrap();
    assert_eq!(snapshot.buffer_len(), 0);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut bitmap =
      DenseMarkBitmapV1::new(snapshot.bucket_count() as u64, MAX_ENTRIES_PER_PAGE as u32, cancellation.clone(), &memory).unwrap();
    let mut actual = PhysicalSet::new();
    let before = fs::read(&path).unwrap();
    let summary = old
      .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
        let (position, captured) = snapshot.find_captured_slot(&entry.hash).unwrap().unwrap();
        assert_eq!(&captured, entry);
        bitmap.mark(position).unwrap();
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert!(summary.complete);
    assert_eq!(summary.tasks, 2);
    assert_eq!(actual, expected);
    assert_eq!(bitmap.marked_count(), 26);
    assert_eq!(fs::read(&path).unwrap(), before);

    let task_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
    let task_key = first_authority_file_path_hash(&task_path, algorithm);
    let (old_position, old_locator) = snapshot.find_captured_slot(&task_key).unwrap().unwrap();
    assert!(bitmap.is_marked(old_position).unwrap());
    let mut replacement =
      publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes;
    let sequence = u64::from_le_bytes(replacement[16..24].try_into().unwrap()) + 1;
    replacement[16..24].copy_from_slice(&sequence.to_le_bytes());
    crc(&mut replacement);
    seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &replacement)]);
    flush_fixture(&publisher);
    let current = publisher.lock_kv().unwrap().capture_settled_snapshot().unwrap();
    let (current_position, current_locator) = current.find_captured_slot(&task_key).unwrap().unwrap();
    assert_eq!(current.bucket_count(), snapshot.bucket_count());
    assert_eq!(current_position, old_position, "fixture keeps the same slot but replaces its incarnation");
    assert_ne!(current_locator.offset, old_locator.offset);
    assert_eq!(current_locator.hash, old_locator.hash);
    assert!(bitmap.is_marked(current_position).unwrap(), "detached bits alone cannot distinguish the newer incarnation");
    assert_eq!(snapshot.find_captured_slot(&task_key).unwrap().unwrap().1, old_locator);
    let before = fs::read(&path).unwrap();
    let mut repeated = PhysicalSet::new();
    assert!(
      old
        .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
          repeated.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
          Ok(())
        })
        .unwrap()
        .complete
    );
    assert_eq!(repeated, expected);
    assert_eq!(fs::read(&path).unwrap(), before);

    // Even an identical buffered override has no admitted stable slot.
    publisher.lock_kv().unwrap().insert(current_locator).unwrap();
    let buffered = publisher.lock_kv().unwrap().capture_settled_snapshot().unwrap();
    assert!(buffered.buffer_len() > 0);
    assert!(buffered.find_captured_slot(&task_key).unwrap_err().to_string().contains("flushed"));
    assert!(buffered
      .visit_captured_slots(&cancellation, |_, _| panic!("buffered layout must refuse before callbacks"))
      .unwrap_err()
      .to_string()
      .contains("flushed"));
    drop(bitmap);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}
