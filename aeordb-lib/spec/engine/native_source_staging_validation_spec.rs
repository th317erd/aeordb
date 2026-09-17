//! Live source-closure, owner, monotonic-frontier and cumulative-budget checks.
use super::*;

#[test]
fn source_staging_rejects_regressed_header_sequence_or_write_high_water() {
  for regress_sequence in [true, false] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-regression", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let mut header = publisher.observe().unwrap().selected.header;
    if regress_sequence {
      header.slot_sequence -= 1;
    } else {
      header.write_sequence_high_water -= 1;
    }
    write_redundant_header(&publisher, &header);
    let before = fs::read(&path).unwrap();
    let error = source.stage_retained_copy(source_bounds(), header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "semantic_source_stage_regression");
    assert!(error.committed_receipt().is_none());
    assert!(fs::read(&path).unwrap() == before, "regressed authority changed database bytes");
  }
}

#[test]
fn source_staging_rejects_changed_live_chunk_bytes_or_flags_before_any_record_copy() {
  for change_flags in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-chunk-change", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let key = &source.record().chunk_hashes[0];
    let header = publisher.observe().unwrap().selected.header;
    {
      let kv = publisher.lock_kv().unwrap();
      let locator = kv.get(key).unwrap().unwrap();
      let encoded = read_entity_bounded(&publisher.file, &*kv, key, 2 << 20, header.write_sequence_high_water).unwrap().unwrap();
      let original = decode_whole_entity(&encoded, algorithm, header.write_sequence_high_water).unwrap();
      let mut body = original.stored_value.to_vec();
      if !change_flags {
        body[0] ^= 1;
      }
      let replacement = encode_whole_entity(&WholeEntityWriteV1 {
        entity_version: original.entity_version,
        entry_type: original.entry_type,
        flags: if change_flags { WHOLE_ENTITY_V1_FLAG_SYSTEM } else { original.flags },
        hash_algorithm: algorithm,
        compression_algorithm: original.compression_algorithm,
        timestamp_ms: original.timestamp_ms,
        write_sequence: original.write_sequence,
        key,
        stored_value: &body,
      })
      .unwrap();
      assert_eq!(replacement.len(), encoded.len());
      write_file_at_native(&publisher.file, locator.offset, &replacement).unwrap();
    }
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error = source.stage_retained_copy(source_bounds(), header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "semantic_source_stage_chunk_changed");
    assert!(error.committed_receipt().is_none());
    assert!(publisher.locator(source.revision()).unwrap().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn source_staging_refuses_changed_physical_owner_or_fence_before_mutation() {
  for physical in [false, true] {
    let algorithm = HashAlgorithm::Sha512;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-owner-change", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let mut header = publisher.observe().unwrap().selected.header;
    if physical {
      header.physical_instance_id = [0x97; 16];
    } else {
      header.writer_fence_epoch += 1;
    }
    write_redundant_header(&publisher, &header);
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error = source.stage_retained_copy(source_bounds(), header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "semantic_source_stage_owner");
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn source_staging_cumulative_validation_budget_refuses_and_retries_without_releasing_protection() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-budget", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::Zstd);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let required: u64 = source.record().chunk_hashes.iter().map(|key| u64::from(publisher.locator(key).unwrap().unwrap().total_length)).sum();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let mut bounds = source_bounds();
  bounds.maximum_read_bytes = required - 1;
  let error = source.stage_retained_copy(bounds, timestamp).unwrap_err();
  assert_eq!(error.code(), "semantic_source_read_bound");
  assert!(error.committed_receipt().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  bounds.maximum_read_bytes = required;
  assert!(!source.stage_retained_copy(bounds, timestamp).unwrap().idempotent);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(publisher.root_state.lock().unwrap().ensure_no_staging_protection().unwrap_err().code(), "staging_protection_active");
}
