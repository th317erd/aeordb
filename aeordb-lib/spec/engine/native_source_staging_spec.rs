//! Guarded immutable source staging, exact original representations and retries.
use super::*;
#[path = "native_source_staging_admission_spec.rs"]
mod admission_spec;
#[path = "native_source_staging_fault_spec.rs"]
mod fault_spec;
#[path = "native_source_staging_validation_spec.rs"]
mod validation_spec;

#[test]
fn source_staging_preserves_original_records_and_chunks_after_alias_replacement_and_reopen() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for version in [0, 1] {
      for (flags, chunk_flags, system_domain) in [(0, 0, false), (1, 0, false), (0, 1, false), (1, 1, false), (1, 1, true)] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("source-staging-original", None, [1; 16], algorithm, 0);
        publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
        let compression = if version == 0 { CompressionAlgorithm::Zstd } else { CompressionAlgorithm::None };
        let original = seed_raw_source(&publisher, INDEX_SOURCE, version, flags, chunk_flags, system_domain, compression);
        let revision = digest_parts(algorithm, &[b"filec:", &original]);
        {
          let memory = observation_memory();
          let cancellation = CancellationToken::new();
          let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
          let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
          let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
          let chunks: Vec<_> = source.record().chunk_hashes.iter().map(|key| publisher.locator(key).unwrap()).collect();
          seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"replacement")]);
          let before = publisher.observe().unwrap().selected.header;
          let current_key = first_authority_file_path_hash(INDEX_SOURCE, algorithm);
          let current = publisher.locator(&current_key).unwrap();
          let receipt = source.stage_retained_copy(source_bounds(), before.updated_at_ms + 1).unwrap();
          assert!(!receipt.idempotent);
          assert_eq!(receipt.entities.len(), 1);
          assert_eq!(receipt.entities[0].key, revision);
          assert_eq!(receipt.observation.selected.header.head_hash, before.head_hash);
          assert_eq!(publisher.locator(&current_key).unwrap(), current);
          assert_eq!(source.record().chunk_hashes.iter().map(|key| publisher.locator(key).unwrap()).collect::<Vec<_>>(), chunks);
          assert!(publisher.root_state.try_lock().is_ok());
          assert!(publisher.kv.try_lock().is_ok());
        }
        drop(publisher);
        let (_coordinator, reopened) = reopen(&path);
        let memory = observation_memory();
        let cancellation = CancellationToken::new();
        let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
        let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
        let old = captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap();
        assert_eq!(old.encoded_record(), original);
        assert_eq!(old.body(), b"firstsecond");
        assert_eq!(old.entity_version(), version);
        assert_eq!(old.flags(), flags);
        assert_eq!(captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().body(), b"replacement");
      }
    }
  }
}

#[test]
fn source_staging_repeat_is_byte_stable_and_does_not_release_capture_protection() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-idempotent", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256)).unwrap();
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 1, true, CompressionAlgorithm::Zstd);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let first = source.stage_retained_copy(source_bounds(), timestamp).unwrap();
  let before = fs::read(&path).unwrap();
  let repeat = source.stage_retained_copy(source_bounds(), timestamp + 1).unwrap();
  assert!(repeat.idempotent);
  assert_eq!(first.entities[0].write_sequence, repeat.entities[0].write_sequence);
  assert!(fs::read(&path).unwrap() == before, "exact retry changed database bytes");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(publisher.root_state.lock().unwrap().ensure_no_staging_protection().unwrap_err().code(), "staging_protection_active");
  drop(source);
  drop(captured);
  drop(protection);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  publisher.root_state.lock().unwrap().ensure_no_staging_protection().unwrap();
}

#[test]
fn source_staging_does_not_enable_generic_system_entity_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-generic-refusal", None, [1; 16], algorithm, 0);
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
  let error = publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &[1; 16],
      entities: &[ImmutableEntityWriteV1 {
        entity_version: source.entity_version(),
        entry_type: EntryTypeV4::FileRecord,
        flags: source.flags(),
        key: source.revision(),
        stored_value: source.encoded_record(),
      }],
      publication_timestamp_ms: timestamp + 1,
    })
    .unwrap_err();
  assert_eq!(error.code(), "immutable_entity_representation");
  assert!(error.committed_receipt().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}
