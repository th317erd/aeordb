//! Cumulative budgets, linear traversal, callback lifetime and allocation refusal.
use super::*;
use crate::engine::v4::semantic_source_capture::{decode_semantic_source_capture_v1, decode_semantic_source_node_v1};

fn file_read_cost(publisher: &V4FirstAuthorityPublisher, key: &[u8]) -> (u64, u64) {
  let header = publisher.observe().unwrap().selected.header;
  let kv = publisher.lock_kv().unwrap();
  let locator = kv.get(key).unwrap().unwrap();
  let bytes = read_entity_bounded(&publisher.file, &*kv, key, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap();
  let record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), entity.entity_version).unwrap();
  let mut bytes = u64::from(locator.total_length);
  for hash in &record.chunk_hashes {
    bytes += u64::from(kv.get(hash).unwrap().unwrap().total_length);
  }
  (bytes, 1 + record.chunk_hashes.len() as u64)
}

fn control_read_cost(publisher: &V4FirstAuthorityPublisher, kind: SystemControlKindV1, id: &[u8]) -> (Vec<u8>, u64, u64) {
  let header = publisher.observe().unwrap().selected.header;
  let path = system_control_path(kind, id, SystemControlSlotV1::Immutable).unwrap();
  let (bytes, reads) = file_read_cost(publisher, &first_authority_file_path_hash(&path, header.hash_algorithm));
  let kv = publisher.lock_kv().unwrap();
  let loaded = load_immutable_system_control_file(&publisher.file, &*kv, &header, kind, id).unwrap().unwrap();
  (loaded.bytes, bytes, reads)
}

// A tiny fixture-only oracle, not the production iterator or its counters.
fn node_read_cost(publisher: &V4FirstAuthorityPublisher, id: &[u8]) -> (u64, u64, u64, u64) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let (body, mut bytes, mut reads) = control_read_cost(publisher, SystemControlKindV1::SemanticSourceNode, id);
  let node = decode_semantic_source_node_v1(&body, algorithm).unwrap();
  let mut nodes = 1;
  let mut rows = 0;
  if let Some(entries) = node.leaf_entries() {
    for entry in entries {
      rows += 1;
      if let Some(hash) = entry.unwrap().file_record_id {
        let (source_bytes, source_reads) = file_read_cost(publisher, hash);
        bytes += source_bytes;
        reads += source_reads;
      }
    }
  } else {
    for child in node.children().unwrap() {
      let cost = node_read_cost(publisher, child.unwrap().node_id);
      bytes += cost.0;
      reads += cost.1;
      nodes += cost.2;
      rows += cost.3;
    }
  }
  (bytes, reads, nodes, rows)
}

#[test]
fn native_catalog_total_read_and_work_bounds_prove_one_pass_without_per_source_resets() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-total-bounds", None, [1; 16], algorithm, 0);
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::Zstd);
    let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
    let rows = [(INDEX_SOURCE, Some(revision.as_slice())), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
    seed_catalog_pair(&publisher, &rows, &rows);
    let (manifest, manifest_bytes, manifest_reads) =
      control_read_cost(&publisher, SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity());
    let (_, checkpoint_bytes, checkpoint_reads) =
      control_read_cost(&publisher, SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity());
    let manifest = decode_semantic_source_capture_v1(&manifest, algorithm).unwrap();
    let base = node_read_cost(&publisher, manifest.base_source_catalog);
    let requested = node_read_cost(&publisher, manifest.requested_source_catalog);
    let expected_bytes = manifest_bytes + checkpoint_bytes + base.0 + requested.0;
    let expected_work = manifest_reads + checkpoint_reads + base.1 + requested.1 + base.2 + requested.2 + base.3 + requested.3;
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    for case in 0..3 {
      let mut bounds = catalog_bounds();
      bounds.maximum_read_bytes = expected_bytes - u64::from(case == 1);
      bounds.maximum_work = expected_work - u64::from(case == 2);
      let result = captured.visit_captured_protected_source_pairs(&[2; 16], 1, bounds, |_, _, _| Ok(true));
      if case == 0 {
        assert!(result.unwrap().complete);
      } else {
        let error = result.unwrap_err();
        assert!(matches!(error, SemanticMutationObservationErrorV1::ResourceRead { .. }), "{error:?}");
        assert_eq!(error.code(), if case == 1 { "semantic_source_catalog_read_bound" } else { "semantic_source_catalog_work_bound" });
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    assert!(captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap().complete);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_catalog_operational_limits_and_projection_allocation_refuse_then_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-catalog-resource-refusal", None, [1; 16], algorithm, 0);
  let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  seed_catalog_pair(&publisher, &rows, &rows);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  for case in 0..9 {
    let mut bounds = catalog_bounds();
    match case {
      0 => bounds.maximum_depth = 0,
      1 => bounds.maximum_depth = 257,
      2 => bounds.maximum_work = 0,
      3 => bounds.maximum_read_bytes = 0,
      4 => bounds.maximum_source_bytes = (64 << 20) + 1,
      5 => bounds.maximum_chunk_entity_bytes = 0,
      6 => bounds.maximum_chunk_entity_bytes = (64 << 20) + 8193,
      7 => bounds.maximum_source_chunks = 0,
      8 => bounds.maximum_depth = 1,
      _ => unreachable!(),
    }
    let result = captured.visit_captured_protected_source_pairs(&[2; 16], 1, bounds, |_, _, _| Ok(true));
    assert_eq!(result.unwrap_err().code(), if case == 8 { "semantic_source_catalog_depth" } else { "semantic_source_catalog_bounds" });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert!(captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap().complete);
  for bytes in [256 * std::mem::size_of::<crate::engine::directory_entry::ChildEntry>(), 128 * std::mem::size_of::<String>()] {
    let (result, allocations) =
      measure(bytes, || captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)));
    assert!(allocations.injected_failure, "{allocations:?}");
    assert!(matches!(
      result.unwrap_err(),
      SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", .. }
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap().complete);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_catalog_callbacks_stop_fail_nest_and_cancel_at_the_last_row_without_success_or_leaks() {
  use crate::engine::memory_coordinator::HostMemorySample;
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-catalog-callbacks", None, [1; 16], algorithm, 0);
  let rows = [(INDEX_SOURCE, None), (PARSER_SOURCE, None), (ALIAS_SOURCE, None)];
  seed_catalog_pair(&publisher, &rows, &rows);
  let before = fs::read(&path).unwrap();
  for case in 0..5 {
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut calls = 0;
    let result = captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |path, _, _| {
      calls += 1;
      assert!(publisher.root_state.try_lock().is_ok());
      assert!(publisher.kv.try_lock().is_ok());
      if case == 2 {
        let reserved = memory.snapshot().unwrap().reserved_bytes;
        assert!(matches!(
          captured
            .read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Requested, path, catalog_bounds())
            .unwrap()
            .disposition(),
          SemanticSourceLookupDispositionV1::Absent
        ));
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      }
      if calls == 3 {
        match case {
          0 => return Ok(false),
          1 => return Err(SemanticMutationObservationErrorV1::Invalid { code: "test_visitor", message: "visitor refusal" }),
          3 => cancellation.cancel(),
          4 => memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap(),
          _ => {}
        }
      }
      Ok(true)
    });
    assert_eq!(calls, 3);
    match case {
      0 => {
        let summary = result.unwrap();
        assert!(!summary.complete);
        assert_eq!(summary.paths, 3);
      }
      1 => assert_eq!(result.unwrap_err().code(), "test_visitor"),
      2 => assert!(result.unwrap().complete),
      3 => assert!(result.is_err()),
      4 => assert!(matches!(result.unwrap_err(), SemanticMutationObservationErrorV1::Memory(_))),
      _ => unreachable!(),
    }
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    if case != 3 {
      assert!(captured.visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |_, _, _| Ok(true)).unwrap().complete);
    }
    drop(captured);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}
