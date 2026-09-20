//! Native callable targets with an independently enumerated slot bitmap.
#[path = "native_semantic_task_mark_boundary_spec.rs"]
mod boundary;
#[path = "native_semantic_task_mark_interruption_spec.rs"]
mod interruption;
use super::*;
use crate::engine::kv_pages::{MAX_ENTRIES_PER_PAGE, page_size};

fn flush_mark_fixture(publisher: &V4FirstAuthorityPublisher) {
  // Explicit fixture setup; the read-only API must never flush implicitly.
  let _guard = publisher.root_state.lock().unwrap();
  let mut kv = publisher.lock_kv().unwrap();
  kv.flush().unwrap();
  assert_eq!(kv.capture_settled_snapshot().unwrap().buffer_len(), 0);
}

fn mark_bounds() -> NativeSemanticTaskMarkBoundsV1 {
  NativeSemanticTaskMarkBoundsV1 { retention: retention_bounds(), maximum_slot_lookups: 100_000, maximum_slot_page_bytes: 64 << 20 }
}

#[test]
fn native_semantic_task_mark_matches_independent_captured_slot_bitmap() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-mark-slots", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    assert_eq!(expected.len(), 26);
    flush_mark_fixture(&publisher);
    let snapshot = publisher.lock_kv().unwrap().capture_settled_snapshot().unwrap();
    assert_eq!(snapshot.buffer_len(), 0);
    let mut expected_bits = vec![0u8; (snapshot.bucket_count() * MAX_ENTRIES_PER_PAGE).div_ceil(8)];
    let mut expected_locators = Vec::new();
    let slots = snapshot
      .visit_captured_slots(&CancellationToken::new(), |position, entry| {
        if let Some(locator) = expected.get(&entry.hash) {
          assert_eq!(*locator, (entry.type_flags, entry.offset, entry.total_length));
          let bit = position.bucket_index as usize * MAX_ENTRIES_PER_PAGE + position.slot_index as usize;
          expected_bits[bit / 8] |= 1 << (bit % 8);
          expected_locators.push(entry.clone());
        }
        Ok(true)
      })
      .unwrap();
    assert!(slots.complete);
    assert_eq!(expected_locators.len(), 26);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut reference_calls = 0;
    let reference = capture
      .visit_captured_semantic_task_retention_entries(retention_bounds(), |_| {
        reference_calls += 1;
        Ok(())
      })
      .unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).expect("produce the captured task contribution, not a global mark");
    assert_eq!(mark.summary().retention, reference);
    assert_eq!(mark.summary().marked_slots, 26);
    assert_eq!(mark.summary().slot_lookups, reference_calls);
    assert_eq!(mark.summary().slot_page_bytes, reference_calls * page_size(algorithm.hash_length()) as u64);
    assert_eq!(mark.bitmap_bytes(), expected_bits);
    for locator in expected_locators {
      assert!(mark.is_captured_locator_marked(&locator).unwrap());
      let changed = KVEntry { offset: locator.offset + 1, ..locator };
      assert!(!mark.is_captured_locator_marked(&changed).unwrap(), "matching key is not matching incarnation");
    }
    drop(mark);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_mark_requires_one_admitted_slot_budget() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-mark-budget", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut calls = 0;
  capture
    .visit_captured_semantic_task_retention_entries(retention_bounds(), |_| {
      calls += 1;
      Ok(())
    })
    .unwrap();
  let page_bytes = calls * page_size(HashAlgorithm::Blake3_256.hash_length()) as u64;
  for (lookups, bytes, succeeds) in [(calls, page_bytes, true), (calls - 1, page_bytes, false), (calls, page_bytes - 1, false)] {
    let result = capture.mark_captured_semantic_tasks(NativeSemanticTaskMarkBoundsV1 {
      maximum_slot_lookups: lookups,
      maximum_slot_page_bytes: bytes,
      ..mark_bounds()
    });
    if succeeds {
      let mark = result.unwrap();
      assert_eq!(mark.summary().marked_slots, 26);
      assert_eq!(mark.summary().slot_lookups, calls);
    } else {
      assert!(result.is_err(), "insufficient slot budget cannot produce a complete contribution");
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
