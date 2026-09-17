//! Next native-catalog unit: exact old source reads, not publication permission.
use super::*;
#[path = "native_source_catalog_spec.rs"]
mod catalog_spec;

#[test]
fn retained_source_all_profiles_reject_bad_revision_widths_and_zero_without_leaking_memory() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-source-profile", None, [1; 16], algorithm, 0);
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::None);
    let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    for bad in [vec![], vec![0; algorithm.hash_length()], vec![1; algorithm.hash_length() - 1], vec![1; algorithm.hash_length() + 1]] {
      let error =
        captured.read_retained_protected_source(INDEX_SOURCE, &bad, source_bounds()).err().expect("invalid retained ID must refuse");
      assert_eq!(error.code(), "semantic_source_retained_identity");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    assert_eq!(captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap().body(), b"firstsecond");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_source_checks_content_revision_before_body_output_allocation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-source-identity", None, [1; 16], algorithm, 0);
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::None);
    let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
    let header = publisher.observe().unwrap().selected.header;
    {
      let kv = publisher.lock_kv().unwrap();
      let locator = kv.get(&revision).unwrap().unwrap();
      let encoded = read_entity_bounded(&publisher.file, &*kv, &revision, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
      let original = decode_whole_entity(&encoded, algorithm, header.write_sequence_high_water).unwrap();
      let mut bad_body = original.stored_value.to_vec();
      *bad_body.last_mut().unwrap() ^= 1;
      let bad = encode_entity(
        original.entity_version,
        original.entry_type,
        original.flags,
        algorithm,
        EntityPublicationOrder { timestamp_ms: original.timestamp_ms, write_sequence: original.write_sequence },
        &revision,
        &bad_body,
      )
      .unwrap();
      assert_eq!(bad.len(), encoded.len());
      write_file_at_native(&publisher.file, locator.offset, &bad).unwrap();
    }
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let (result, allocations) = measure(11, || captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()));
    assert_eq!(result.err().expect("incorrect content alias must refuse").code(), "semantic_source_retained_identity");
    assert_eq!(allocations.matching_requests, 0, "{allocations:?}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn retained_source_resource_and_allocation_failures_preserve_causes_and_retry() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retained-source-resources", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::None);
  let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut bounds = source_bounds();
  bounds.maximum_read_bytes = 1;
  let error = captured.read_retained_protected_source(INDEX_SOURCE, &revision, bounds).err().expect("read budget must refuse");
  assert!(matches!(error, SemanticMutationObservationErrorV1::ResourceRead { code: "semantic_source_read_bound", .. }));
  let (result, allocations) = measure(11, || captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()));
  assert!(allocations.injected_failure);
  assert_eq!(result.err().expect("output allocation must refuse").code(), "semantic_source_body_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap().body(), b"firstsecond");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

// Test-only physical assembly, retaining the exact original FileRecord body.
// This is deliberately not a production captured-copy operation.
fn seed_retained_revision(publisher: &V4FirstAuthorityPublisher, path: &str) -> Vec<u8> {
  let _guard = publisher.root_state.lock().unwrap();
  let mut header = publisher.observe().unwrap().selected.header;
  let algorithm = header.hash_algorithm;
  let mut kv = publisher.lock_kv().unwrap();
  let current = first_authority_file_path_hash(path, algorithm);
  let encoded = read_entity_bounded(&publisher.file, &*kv, &current, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
  let original = decode_whole_entity(&encoded, algorithm, header.write_sequence_high_water).unwrap();
  let revision = digest_parts(algorithm, &[b"filec:", original.stored_value]);
  assert!(kv.get(&revision).unwrap().is_none());
  let sequence = header.write_sequence_high_water + 1;
  let copied = encode_entity(
    original.entity_version,
    original.entry_type,
    original.flags,
    algorithm,
    EntityPublicationOrder { timestamp_ms: header.updated_at_ms + 1, write_sequence: sequence },
    &revision,
    original.stored_value,
  )
  .unwrap();
  let offset = header.hot_tail_offset;
  write_file_at_native(&publisher.file, offset, &copied).unwrap();
  kv.insert(KVEntry { type_flags: KV_TYPE_FILE_RECORD, hash: revision.clone(), offset, total_length: copied.len() as u32 }).unwrap();
  header.hot_tail_offset += copied.len() as u64;
  kv.set_hot_tail_offset(header.hot_tail_offset);
  kv.force_flush_hot_buffer().unwrap();
  header.entry_count = kv.len() as u64;
  drop(kv);
  header.slot_sequence += 1;
  header.updated_at_ms += 1;
  header.write_sequence_high_water = sequence;
  let slot = encode_database_header_slot(&header).unwrap();
  write_file_at_native(&publisher.file, 0, &slot).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &slot).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
  revision
}

#[test]
fn retained_source_reads_old_raw_revision_after_current_replacement_and_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for version in [0, 1] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-protected-source-reopen", None, [1; 16], algorithm, 0);
      publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
      let original = seed_raw_source(&publisher, INDEX_SOURCE, version, 1, 0, false, CompressionAlgorithm::Zstd);
      let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
      seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"new current input")]);
      drop(publisher);
      let (_coordinator, reopened) = reopen(&path);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let old = captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap();
      let current = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      assert_eq!(old.encoded_record(), original);
      assert_eq!(old.body(), b"firstsecond");
      assert_eq!(old.revision(), revision);
      assert_eq!(old.entity_version(), version);
      assert_eq!(old.flags(), 1);
      assert_eq!(old.record().metadata, [1, 0, 2, 255]);
      assert_eq!(current.body(), b"new current input");
      assert_ne!(current.revision(), old.revision());
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn retained_source_missing_revision_cannot_fall_back_to_a_current_path() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retained-protected-source-missing", None, [1; 16], algorithm, 0);
  let original = seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::None);
  let revision = digest_parts(algorithm, &[b"filec:", &original]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  assert!(captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().is_some());
  let result = captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds());
  assert_eq!(result.err().expect("retained identity does not exist").code(), "semantic_source_retained_missing");
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn retained_source_keeps_capture_memory_and_rejects_wrong_original_path() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-protected-source-accounting", None, [1; 16], algorithm, 0);
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 1, true, CompressionAlgorithm::None);
    let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let source = captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap();
    assert_eq!(source.body(), b"firstsecond");
    assert!(memory.snapshot().unwrap().reserved_bytes > baseline);
    assert!(publisher.root_state.try_lock().is_ok());
    assert!(publisher.kv.try_lock().is_ok());
    drop(source);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(captured.read_retained_protected_source("/.aeordb-config/parsers.json", &revision, source_bounds()).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).is_ok());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(captured);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
