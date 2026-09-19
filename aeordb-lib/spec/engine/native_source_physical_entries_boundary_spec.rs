//! Provisional visits cannot substitute for a complete validated read set.
use super::*;

#[test]
fn native_source_physical_entries_keep_exact_cumulative_read_and_work_limits() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-entry-budgets", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let expected = physical_catalog_fixture(&publisher);
  let bytes = expected.values().map(|row| u64::from(row.2)).sum::<u64>();
  // Seventeen physical reads, four node admissions, six ordered leaf rows.
  let exact = NativeSemanticSourceCatalogBoundsV1 { maximum_read_bytes: bytes, maximum_work: 27, ..catalog_bounds() };
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for case in 0..3 {
    let mut bounds = exact;
    if case == 1 {
      bounds.maximum_read_bytes -= 1;
    }
    if case == 2 {
      bounds.maximum_work -= 1;
    }
    let mut visits = 0;
    let result = capture.visit_captured_source_physical_entries(&[2; 16], 1, bounds, |_| {
      visits += 1;
      Ok(())
    });
    if case == 0 {
      assert!(result.unwrap().complete);
      assert_eq!(visits, 17);
    } else {
      let error = result.unwrap_err();
      assert_eq!(error.code(), if case == 1 { "semantic_source_catalog_read_bound" } else { "semantic_source_catalog_work_bound" });
      assert!(matches!(error, SemanticMutationObservationErrorV1::ResourceRead { .. }));
      assert!(visits > 0, "late refusal must invalidate earlier provisional entries");
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
  assert!(capture.visit_captured_source_physical_entries(&[2; 16], 1, exact, |_| Ok(())).unwrap().complete);
}

#[test]
fn native_source_physical_entries_preserve_shared_source_multiplicity_without_global_deduplication() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-entry-shared", None, [1; 16], algorithm, 0);
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::Zstd);
  let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
  let rows = [(INDEX_SOURCE, Some(revision.as_slice())), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  seed_catalog_pair(&publisher, &rows, &rows);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let mut counts = BTreeMap::new();
  let summary = capture
    .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |entry| {
      *counts.entry(entry.hash.clone()).or_insert(0u64) += 1;
      Ok(())
    })
    .unwrap();
  assert!(summary.complete);
  assert_eq!(counts.values().sum::<u64>(), 18);
  assert_eq!(counts.len(), 15);
  assert_eq!(counts.get(&revision), Some(&2));
  for body in [b"first".as_slice(), b"second"] {
    assert_eq!(counts.get(&digest_parts(algorithm, &[b"chunk:", body])), Some(&2));
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_source_physical_entries_cancel_or_refuse_pressure_even_at_the_last_callback() {
  for pressure in [false, true] {
    for stop_at in [0, 1, 17] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("physical-entry-interrupt", None, [1; 16], HashAlgorithm::Blake3_256, 0);
      physical_catalog_fixture(&publisher);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(&path).unwrap();
      let interrupt = || {
        if pressure {
          memory
            .update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 96 << 20, ..Default::default() })
            .unwrap();
        } else {
          cancellation.cancel();
        }
      };
      if stop_at == 0 {
        interrupt();
      }
      let mut visits = 0;
      let error = capture
        .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| {
          visits += 1;
          if visits == stop_at {
            interrupt();
          }
          Ok(())
        })
        .unwrap_err();
      assert_eq!(error.code(), if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" });
      assert_eq!(visits, stop_at);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
      memory.update_host_sample(Default::default()).unwrap();
      let retry = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &retry).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &retry).unwrap();
      assert!(capture.visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| Ok(())).unwrap().complete);
    }
  }
}

#[test]
fn native_source_physical_entries_allocation_failure_is_not_a_complete_or_empty_graph() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-entry-allocation", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  physical_catalog_fixture(&publisher);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut visits = 0;
  let (result, allocations) = measure(11, || {
    capture.visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| {
      visits += 1;
      Ok(())
    })
  });
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().code(), "semantic_source_body_allocation");
  assert!(visits > 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
  assert!(capture.visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| Ok(())).unwrap().complete);
}

#[test]
fn native_source_physical_entries_late_corrupt_chunk_invalidates_all_provisional_visits() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-entry-corrupt-chunk", None, [1; 16], algorithm, 0);
  physical_catalog_fixture(&publisher);
  let key = digest_parts(algorithm, &[b"chunk:", b"second"]);
  let locator = publisher.locator(&key).unwrap().unwrap();
  let offset = locator.offset + u64::from(locator.total_length) - 1;
  let mut byte = [0];
  read_file_at_native(&publisher.file, offset, &mut byte).unwrap();
  byte[0] ^= 1;
  write_file_at_native(&publisher.file, offset, &byte).unwrap();
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut visits = 0;
  let error = capture
    .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| {
      visits += 1;
      Ok(())
    })
    .unwrap_err();
  assert_eq!(error.code(), "integrity_hash_mismatch");
  assert!(visits > 2 && visits < 17, "only a provisional prefix should be observed");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_source_physical_entries_do_not_consult_later_current_catalog_replacements() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-entry-old-capture", None, [1; 16], algorithm, 0);
  let expected = physical_catalog_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  // Test-only replacement of normally immutable paths proves old physical
  // snapshot ownership; no production immutable writer permits replacement.
  let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  seed_catalog_pair(&publisher, &rows, &rows);
  let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut actual = BTreeMap::new();
  assert!(
    old
      .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap()
      .complete
  );
  assert_eq!(actual, expected);
  let mut current = BTreeMap::new();
  assert!(
    fresh
      .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |entry| {
        current.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap()
      .complete
  );
  assert_eq!(current.len(), 12);
  assert_ne!(current, actual);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_source_physical_entries_reject_declared_counts_even_after_every_entry_was_observed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-entry-counts", None, [1; 16], algorithm, 0);
  physical_catalog_fixture(&publisher);
  let companion =
    publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &checkpoint_identity()).unwrap().unwrap();
  let mut decoded = crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1(&companion.bytes, algorithm).unwrap();
  decoded.protected_path_count += 1;
  let altered = encode_semantic_source_capture_v1(&decoded, algorithm).unwrap();
  seed(&publisher, &[(SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity(), SystemControlSlotV1::Immutable, &altered)]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let mut visits = 0;
  let error = capture
    .visit_captured_source_physical_entries(&[2; 16], 1, catalog_bounds(), |_| {
      visits += 1;
      Ok(())
    })
    .unwrap_err();
  assert_eq!(visits, 17);
  assert_eq!(error.code(), "semantic_source_catalog_counts");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
