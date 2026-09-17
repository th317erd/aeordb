//! Native staging refusals and representation-preserving relocation.
use super::*;
use allocation_probe::measure_nth;

#[test]
fn source_staging_missing_wrong_role_or_corrupt_chunks_refuse_without_writes() {
  for case in ["missing", "role", "integrity"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-live-locator", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let key = &source.record().chunk_hashes[0];
    let mut header = publisher.observe().unwrap().selected.header;
    if case == "integrity" {
      corrupt_last_entity_byte(&publisher, key);
    } else {
      let mut kv = publisher.lock_kv().unwrap();
      if case == "missing" {
        assert!(kv.mark_deleted(key).unwrap());
      } else {
        let mut locator = kv.get(key).unwrap().unwrap();
        locator.type_flags = KV_TYPE_FILE_RECORD;
        kv.insert(locator).unwrap();
      }
      kv.force_flush_hot_buffer().unwrap();
      header.entry_count = kv.len() as u64;
      drop(kv);
      write_redundant_header(&publisher, &header);
    }
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error = source.stage_retained_copy(source_bounds(), header.updated_at_ms + 1).unwrap_err();
    assert_eq!(
      error.code(),
      match case {
        "missing" => "semantic_source_chunk_missing",
        "role" => "semantic_source_chunk_role",
        _ => "integrity_hash_mismatch",
      }
    );
    assert!(error.committed_receipt().is_none());
    assert!(publisher.locator(source.revision()).unwrap().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(fs::read(&path).unwrap() == before, "invalid live chunk caused a write: {case}");
  }
}

#[test]
fn source_staging_exact_identity_collisions_refuse_before_even_flushing_kv() {
  for case in ["role", "flags", "version", "body", "length", "integrity"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-collision", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    source.stage_retained_copy(source_bounds(), timestamp).unwrap();
    let header = publisher.observe().unwrap().selected.header;
    {
      let mut kv = publisher.lock_kv().unwrap();
      let mut locator = kv.get(source.revision()).unwrap().unwrap();
      if case == "role" || case == "length" {
        if case == "role" {
          locator.type_flags = KV_TYPE_CHUNK;
        } else {
          locator.total_length -= 1;
        }
        kv.insert(locator).unwrap();
        kv.force_flush_hot_buffer().unwrap();
      } else if case != "integrity" {
        let original =
          read_entity_bounded(&publisher.file, &*kv, source.revision(), 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
        let entity = decode_whole_entity(&original, algorithm, header.write_sequence_high_water).unwrap();
        let mut body = entity.stored_value.to_vec();
        if case == "body" {
          *body.last_mut().unwrap() ^= 1;
        }
        let replacement = encode_whole_entity(&WholeEntityWriteV1 {
          entity_version: if case == "version" { 0 } else { entity.entity_version },
          entry_type: entity.entry_type,
          flags: if case == "flags" { 0 } else { entity.flags },
          hash_algorithm: algorithm,
          compression_algorithm: entity.compression_algorithm,
          timestamp_ms: entity.timestamp_ms,
          write_sequence: entity.write_sequence,
          key: entity.key,
          stored_value: &body,
        })
        .unwrap();
        assert_eq!(replacement.len(), original.len());
        write_file_at_native(&publisher.file, locator.offset, &replacement).unwrap();
      }
    }
    if case == "integrity" {
      corrupt_last_entity_byte(&publisher, source.revision());
    }
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error = source.stage_retained_copy(source_bounds(), timestamp + 1).unwrap_err();
    assert_eq!(error.code(), if case == "integrity" { "integrity_hash_mismatch" } else { "immutable_entity_identity_collision" });
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(fs::read(&path).unwrap() == before, "collision caused a write: {case}");
  }
}

#[test]
fn source_staging_invalid_bounds_and_timestamps_refuse_then_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-bounds", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for case in 0..9 {
    let mut bounds = source_bounds();
    let mut timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    match case {
      0 => bounds.maximum_body_bytes = (64 << 20) + 1,
      1 => bounds.maximum_chunk_entity_bytes = 0,
      2 => bounds.maximum_chunk_entity_bytes = (64 << 20) + 8193,
      3 => bounds.maximum_chunks = 0,
      4 => bounds.maximum_read_bytes = 0,
      5 => bounds.maximum_body_bytes = source.body().len() - 1,
      6 => bounds.maximum_chunks = source.record().chunk_hashes.len() as u64 - 1,
      7 => timestamp = 0,
      _ => timestamp = i64::MAX as u64 + 1,
    }
    let error = source.stage_retained_copy(bounds, timestamp).unwrap_err();
    assert_eq!(
      error.code(),
      match case {
        0..=4 => "semantic_source_bounds",
        5..=6 => "semantic_source_body_bound",
        _ => "semantic_source_stage_time",
      }
    );
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(fs::read(&path).unwrap() == before, "invalid admission caused a write: {case}");
  }
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  assert!(!source.stage_retained_copy(source_bounds(), timestamp).unwrap().idempotent);
}

#[test]
fn source_staging_accepts_empty_bodies_and_zero_body_limit() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-empty", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"")]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    assert!(source.body().is_empty());
    let mut bounds = source_bounds();
    bounds.maximum_body_bytes = 0;
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    source.stage_retained_copy(bounds, timestamp).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(source.stage_retained_copy(bounds, timestamp + 1).unwrap().idempotent);
    assert!(fs::read(&path).unwrap() == before);
  }
}

#[test]
fn source_staging_allocation_refusals_preserve_bytes_accounting_and_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-allocation", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  source.stage_retained_copy(source_bounds(), timestamp).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let chunk_size = publisher.locator(&source.record().chunk_hashes[0]).unwrap().unwrap().total_length as usize;
  let record_size = publisher.locator(source.revision()).unwrap().unwrap().total_length as usize;
  for (size, last, code) in [
    (chunk_size, false, "semantic_source_entity_allocation"),
    (record_size, false, "first_authority_readback_allocation"),
    (source.revision().len(), true, "semantic_source_stage_allocation"),
    (std::mem::size_of::<ImmutableEntityPublicationReceiptV1>(), true, "semantic_source_stage_allocation"),
  ] {
    // Last matching allocation targets the newly fallible receipt fields,
    // after captured-snapshot and lookup internals have finished allocating.
    let (warm, allocations) = measure_nth(size, usize::MAX, || source.stage_retained_copy(source_bounds(), timestamp));
    assert!(warm.unwrap().idempotent);
    assert!(allocations.matching_requests > 0);
    let occurrence = if last { allocations.matching_requests } else { 1 };
    let (result, allocations) = measure_nth(size, occurrence, || source.stage_retained_copy(source_bounds(), timestamp));
    assert!(allocations.injected_failure, "{size}/{occurrence}: {allocations:?}");
    let error = result.unwrap_err();
    assert_eq!(error.code(), code, "{size}/{occurrence}");
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(fs::read(&path).unwrap() == before, "allocation refusal changed bytes");
    assert!(source.stage_retained_copy(source_bounds(), timestamp).unwrap().idempotent);
  }
}

#[test]
fn source_staging_accepts_exact_live_chunk_relocation_with_new_publication_metadata() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-relocation", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::Zstd);
    let revision;
    {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      revision = source.revision().to_vec();
      {
        let _authority = publisher.root_state.lock().unwrap();
        let mut header = publisher.observe().unwrap().selected.header;
        let mut kv = publisher.lock_kv().unwrap();
        let mut offset = header.hot_tail_offset;
        let mut sequence = header.write_sequence_high_water;
        for key in &source.record().chunk_hashes {
          let original_locator = kv.get(key).unwrap().unwrap();
          let bytes = read_entity_bounded(&publisher.file, &*kv, key, 2 << 20, header.write_sequence_high_water).unwrap().unwrap();
          let original = decode_whole_entity(&bytes, algorithm, header.write_sequence_high_water).unwrap();
          sequence += 1;
          let relocated = encode_whole_entity(&WholeEntityWriteV1 {
            entity_version: original.entity_version,
            entry_type: original.entry_type,
            flags: original.flags,
            hash_algorithm: algorithm,
            compression_algorithm: original.compression_algorithm,
            timestamp_ms: header.updated_at_ms + 1,
            write_sequence: sequence,
            key,
            stored_value: original.stored_value,
          })
          .unwrap();
          assert_ne!(offset, original_locator.offset);
          assert_ne!(relocated, bytes);
          write_file_at_native(&publisher.file, offset, &relocated).unwrap();
          kv.insert(KVEntry { type_flags: KV_TYPE_CHUNK, hash: key.to_vec(), offset, total_length: relocated.len() as u32 }).unwrap();
          offset += relocated.len() as u64;
        }
        kv.set_hot_tail_offset(offset);
        kv.force_flush_hot_buffer().unwrap();
        header.entry_count = kv.len() as u64;
        drop(kv);
        header.hot_tail_offset = offset;
        header.write_sequence_high_water = sequence;
        header.slot_sequence += 1;
        header.updated_at_ms += 1;
        write_redundant_header(&publisher, &header);
      }
      let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
      assert!(!source.stage_retained_copy(source_bounds(), timestamp).unwrap().idempotent);
    }
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    assert_eq!(captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap().body(), b"firstsecond");
  }
}
