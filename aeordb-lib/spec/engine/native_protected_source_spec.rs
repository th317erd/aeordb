//! Captured raw-source regressions and retained-source catalog integration.
#[path = "native_retained_source_catalog_spec.rs"]
mod retained_source_spec;
use super::*;

const INDEX_SOURCE: &str = "/.aeordb-config/indexes.json";

fn source_bounds() -> NativeSemanticSourceReadBoundsV1 {
  NativeSemanticSourceReadBoundsV1 {
    maximum_body_bytes: 1 << 20,
    maximum_chunk_entity_bytes: 2 << 20,
    maximum_chunks: 1024,
    maximum_read_bytes: 8 << 20,
  }
}

fn capture_bounds() -> NativeSemanticMutationInventoryBoundsV1 {
  NativeSemanticMutationInventoryBoundsV1 { maximum_work: 4096, maximum_entity_bytes: 4 << 20, maximum_read_bytes: 64 << 20 }
}

#[test]
fn native_protected_source_reads_exact_captured_bytes_and_revision_without_writes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("protected-source-read", None, [1; 16], algorithm, 0);
    let body = b"{ \"indexes\": [] }\n";
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", body)]);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    assert_eq!(source.body(), body);
    assert_eq!(source.record().path, INDEX_SOURCE);
    assert_eq!(source.record().content_type.as_deref(), Some("application/json"));
    assert_eq!(source.entity_version(), 1);
    assert_eq!(source.flags(), WHOLE_ENTITY_V1_FLAG_SYSTEM);
    assert_eq!(source.revision(), digest_parts(algorithm, &[b"filec:", source.encoded_record()]));
    assert!(publisher.root_state.try_lock().is_ok());
    assert!(publisher.kv.try_lock().is_ok());
    assert!(memory.snapshot().unwrap().reserved_bytes > retained);
    drop(source);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(captured);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_protected_source_uses_the_same_task_capture_across_replacement() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("protected-source-replacement", None, [1; 16], algorithm, 0);
    let old_body = b"{\"old\":true}";
    let new_body = b"{\"new\":true}";
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", old_body)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", new_body)]);
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let previous = old.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let current = fresh.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    assert_eq!(previous.body(), old_body);
    assert_eq!(current.body(), new_body);
    assert_ne!(previous.revision(), current.revision());
  }
}

#[test]
fn native_protected_source_absence_does_not_invent_a_record_or_consult_live_state() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("protected-source-absence", None, [1; 16], algorithm, 0);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"{}")]);
  let before = fs::read(&path).unwrap();
  assert!(old.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}

// Exact raw FileRecord bodies are deliberately built without FileRecord's writer.
// The existing WholeEntity writer is used only for the physical test wrapper.
fn seed_raw_source(
  publisher: &V4FirstAuthorityPublisher,
  path: &str,
  version: u8,
  record_flags: u8,
  chunk_flags: u8,
  system_domain: bool,
  compression: CompressionAlgorithm,
) -> Vec<u8> {
  let _guard = publisher.root_state.lock().unwrap();
  let mut header = publisher.observe().unwrap().selected.header;
  let algorithm = header.hash_algorithm;
  let mut sequence = header.write_sequence_high_water;
  let mut entries = Vec::new();
  let mut hashes = Vec::new();
  let mut complete_body = Vec::new();
  for body in [b"first".as_slice(), b"second".as_slice()] {
    complete_body.extend_from_slice(body);
    let domain = if system_domain { b"system::".as_slice() } else { b"chunk:".as_slice() };
    let key = digest_parts(algorithm, &[domain, body]);
    let stored = crate::engine::compression::compress(body, compression).unwrap();
    sequence += 1;
    let bytes = encode_whole_entity(&WholeEntityWriteV1 {
      entity_version: 0,
      entry_type: EntryTypeV4::Chunk,
      flags: chunk_flags,
      hash_algorithm: algorithm,
      compression_algorithm: compression,
      timestamp_ms: header.updated_at_ms + 1,
      write_sequence: sequence,
      key: &key,
      stored_value: &stored,
    })
    .unwrap();
    hashes.push(key.clone());
    entries.push((KV_TYPE_CHUNK, key, bytes));
  }
  let content_type = b"application/custom";
  let metadata = [1, 0, 2, 255];
  let mut record = Vec::new();
  record.extend_from_slice(&(path.len() as u16).to_le_bytes());
  record.extend_from_slice(path.as_bytes());
  record.extend_from_slice(&(content_type.len() as u16).to_le_bytes());
  record.extend_from_slice(content_type);
  record.extend_from_slice(&(complete_body.len() as u64).to_le_bytes());
  record.extend_from_slice(&11i64.to_le_bytes());
  record.extend_from_slice(&13i64.to_le_bytes());
  if version == 1 {
    record.extend_from_slice(&digest_parts(algorithm, &[&complete_body]));
  }
  record.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
  record.extend_from_slice(&metadata);
  record.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
  for hash in &hashes {
    record.extend_from_slice(hash);
  }
  sequence += 1;
  let key = first_authority_file_path_hash(path, algorithm);
  let bytes = encode_entity(
    version,
    EntryTypeV4::FileRecord,
    record_flags,
    algorithm,
    EntityPublicationOrder { timestamp_ms: header.updated_at_ms + 1, write_sequence: sequence },
    &key,
    &record,
  )
  .unwrap();
  entries.push((KV_TYPE_FILE_RECORD, key, bytes));
  let mut kv = publisher.lock_kv().unwrap();
  let mut offset = header.hot_tail_offset;
  for (type_flags, hash, bytes) in entries {
    write_file_at_native(&publisher.file, offset, &bytes).unwrap();
    kv.insert(KVEntry { type_flags, hash, offset, total_length: bytes.len() as u32 }).unwrap();
    offset += bytes.len() as u64;
  }
  kv.set_hot_tail_offset(offset);
  kv.force_flush_hot_buffer().unwrap();
  header.entry_count = kv.len() as u64;
  drop(kv);
  header.slot_sequence += 1;
  header.updated_at_ms += 1;
  header.hot_tail_offset = offset;
  header.write_sequence_high_water = sequence;
  let encoded = encode_database_header_slot(&header).unwrap();
  write_file_at_native(&publisher.file, 0, &encoded).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &encoded).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
  record
}

#[test]
fn native_protected_source_preserves_v0_v1_metadata_chunks_and_actual_flags() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for version in [0, 1] {
      for (record_flags, chunk_flags, system_domain) in [(0, 0, false), (1, 0, false), (0, 1, false), (1, 1, false), (1, 1, true)] {
        for compression in [CompressionAlgorithm::None, CompressionAlgorithm::Zstd] {
          let (_directory, path, _coordinator, publisher) =
            create_environment_for_algorithm_at_kv_stage("protected-source-representations", None, [1; 16], algorithm, 0);
          let original = seed_raw_source(&publisher, INDEX_SOURCE, version, record_flags, chunk_flags, system_domain, compression);
          let before = fs::read(&path).unwrap();
          let memory = observation_memory();
          let cancellation = CancellationToken::new();
          let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
          let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
          let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
          assert_eq!(source.body(), b"firstsecond");
          assert_eq!(source.encoded_record(), original);
          assert_eq!(source.entity_version(), version);
          assert_eq!(source.flags(), record_flags);
          assert_eq!(source.record().metadata, [1, 0, 2, 255]);
          assert_eq!(source.record().created_at, 11);
          assert_eq!(source.record().updated_at, 13);
          assert_eq!(source.record().content_hash.is_empty(), version == 0);
          assert_eq!(source.revision(), digest_parts(algorithm, &[b"filec:", &original]));
          assert_eq!(fs::read(&path).unwrap(), before);
        }
      }
    }
  }
}

#[test]
fn native_protected_source_admits_only_the_four_non_head_source_families() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("protected-source-families", None, [1; 16], algorithm, 0);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    for source_path in [
      INDEX_SOURCE,
      "/.aeordb-config/parsers.json",
      "/.aeordb-system/plugin-aliases/example",
      "/.aeordb-system/plugin-artifacts/blake3/example",
    ] {
      seed_files(&publisher, &[(source_path.to_string(), "application/octet-stream", b"raw source")]);
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let source = captured.read_protected_source(source_path, source_bounds()).unwrap().unwrap();
      assert_eq!(source.body(), b"raw source");
      assert_eq!(source.revision().len(), algorithm.hash_length());
      assert_eq!(fs::read(&path).unwrap(), before);
    }
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    for denied in [
      "/ordinary.txt",
      "/sub/.aeordb-config/indexes.json",
      "/.aeordb-system/plugins/example",
      "/.aeordb-config/runtime.json",
      "/",
      "relative",
      "/.aeordb-config/../indexes.json",
    ] {
      assert!(captured.read_protected_source(denied, source_bounds()).is_err(), "{denied}");
    }
  }
}

#[test]
fn native_protected_source_limits_refuse_without_absence_and_release_memory_for_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-limits", None, [1; 16]);
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 1, true, CompressionAlgorithm::None);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for (limits, code) in [
    (NativeSemanticSourceReadBoundsV1 { maximum_body_bytes: 10, ..source_bounds() }, "semantic_source_body_bound"),
    (NativeSemanticSourceReadBoundsV1 { maximum_chunks: 1, ..source_bounds() }, "semantic_source_body_bound"),
    (NativeSemanticSourceReadBoundsV1 { maximum_chunk_entity_bytes: 1, ..source_bounds() }, "semantic_source_chunk_bound"),
    (NativeSemanticSourceReadBoundsV1 { maximum_read_bytes: 1, ..source_bounds() }, "semantic_source_read_bound"),
  ] {
    let error = captured.read_protected_source(INDEX_SOURCE, limits).err().expect("limit must refuse");
    assert_eq!(error.code(), code);
    assert!(matches!(error, SemanticMutationObservationErrorV1::Resource { .. } | SemanticMutationObservationErrorV1::ResourceRead { .. }));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().body(), b"firstsecond");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_protected_source_body_allocation_is_fallible_and_retained_sources_are_separately_charged() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-allocation", None, [1; 16]);
  let body = vec![73; 131_071];
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/octet-stream", &body)]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let (result, allocations) = measure(body.len(), || captured.read_protected_source(INDEX_SOURCE, source_bounds()));
  assert!(allocations.injected_failure, "{allocations:?}");
  let error = result.err().expect("body allocation must refuse");
  assert_eq!(error.code(), "semantic_source_body_allocation");
  assert!(error.source().is_some(), "allocation cause must survive");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let first = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let with_first = memory.snapshot().unwrap().reserved_bytes;
  let second = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes - with_first, with_first - retained);
  assert_eq!(first.body(), body);
  assert_eq!(second.body(), body);
  drop(first);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, with_first);
  drop(second);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

fn rewrite_source_record(publisher: &V4FirstAuthorityPublisher, change: impl FnOnce(&mut FileRecord)) {
  let header = publisher.observe().unwrap().selected.header;
  let key = first_authority_file_path_hash(INDEX_SOURCE, header.hash_algorithm);
  let kv = publisher.lock_kv().unwrap();
  let locator = kv.get(&key).unwrap().unwrap();
  let bytes = read_entity_bounded(&publisher.file, &*kv, &key, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap();
  let mut record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), entity.entity_version).unwrap();
  change(&mut record);
  let body = record.serialize_for_version(header.hash_algorithm.hash_length(), entity.entity_version).unwrap();
  let replacement = encode_entity(
    entity.entity_version,
    entity.entry_type,
    entity.flags,
    header.hash_algorithm,
    EntityPublicationOrder { timestamp_ms: entity.timestamp_ms, write_sequence: entity.write_sequence },
    entity.key,
    &body,
  )
  .unwrap();
  assert_eq!(bytes.len(), replacement.len());
  write_file_at_native(&publisher.file, locator.offset, &replacement).unwrap();
}

#[test]
fn native_protected_source_checks_declared_length_whole_hash_path_and_missing_chunks() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in ["short", "long", "content-hash", "path", "missing-chunk"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("protected-source-malformed", None, [1; 16], algorithm, 0);
      seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 1, true, CompressionAlgorithm::None);
      rewrite_source_record(&publisher, |record| match case {
        "short" => record.total_size -= 1,
        "long" => record.total_size += 1,
        "content-hash" => record.content_hash[0] ^= 1,
        "path" => record.path = "/.aeordb-config/invalid.json".to_string(),
        "missing-chunk" => record.chunk_hashes[0].fill(0x93),
        _ => unreachable!(),
      });
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let error = captured.read_protected_source(INDEX_SOURCE, source_bounds()).err().expect("malformed source must refuse");
      let code = match case {
        "short" | "long" => "semantic_source_content_length",
        "content-hash" => "semantic_source_content_identity",
        "path" => "semantic_source_record_path",
        "missing-chunk" => "semantic_source_chunk_missing",
        _ => unreachable!(),
      };
      assert_eq!(error.code(), code, "{case}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_protected_source_empty_body_and_precancelled_absence_keep_their_meanings() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-empty", None, [1; 16]);
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/octet-stream", b"")]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured
    .read_protected_source(INDEX_SOURCE, NativeSemanticSourceReadBoundsV1 { maximum_body_bytes: 0, ..source_bounds() })
    .unwrap()
    .unwrap();
  assert!(source.body().is_empty());
  drop(source);
  let retained = memory.snapshot().unwrap().reserved_bytes;
  cancellation.cancel();
  for source_path in [INDEX_SOURCE, "/.aeordb-config/parsers.json"] {
    assert!(captured.read_protected_source(source_path, source_bounds()).is_err());
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_protected_source_compressed_body_cannot_exceed_declared_output() {
  for declared in [0, 4, 10] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-compressed-bound", None, [1; 16]);
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::Zstd);
    rewrite_source_record(&publisher, |record| record.total_size = declared);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let error = captured.read_protected_source(INDEX_SOURCE, source_bounds()).err().expect("compressed body must fit declared output");
    assert_eq!(error.code(), "semantic_source_chunk_compression");
    assert!(!error.to_string().is_empty());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_protected_source_memory_pressure_releases_and_retries() {
  use crate::engine::memory_coordinator::HostMemorySample;
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-pressure", None, [1; 16]);
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"{}")]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
  assert!(matches!(captured.read_protected_source(INDEX_SOURCE, source_bounds()), Err(SemanticMutationObservationErrorV1::Memory(_))));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  memory.update_host_sample(HostMemorySample::default()).unwrap();
  assert_eq!(captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().body(), b"{}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_protected_source_role_and_physical_read_errors_are_not_absence() {
  for case in ["record-role", "chunk-role", "chunk-integrity"] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-physical", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"{}")]);
    let key = if case == "record-role" {
      first_authority_file_path_hash(INDEX_SOURCE, algorithm)
    } else {
      first_authority_system_chunk_hash(b"{}", algorithm)
    };
    if case == "chunk-integrity" {
      corrupt_last_entity_byte(&publisher, &key);
    } else {
      let mut kv = publisher.lock_kv().unwrap();
      let mut locator = kv.get(&key).unwrap().unwrap();
      locator.type_flags = if case == "record-role" { KV_TYPE_CHUNK } else { KV_TYPE_FILE_RECORD };
      kv.insert(locator).unwrap();
      kv.force_flush_hot_buffer().unwrap();
    }
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let error = captured.read_protected_source(INDEX_SOURCE, source_bounds()).err().expect("physical failure must refuse");
    assert_eq!(
      error.code(),
      match case {
        "record-role" => "semantic_source_record_role",
        "chunk-role" => "semantic_source_chunk_role",
        _ => "integrity_hash_mismatch",
      }
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_protected_source_entity_and_raw_record_allocation_refusals_keep_causes_and_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-record-allocation", None, [1; 16]);
  let original = seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 1, true, CompressionAlgorithm::None);
  let record_size = publisher
    .lock_kv()
    .unwrap()
    .get(&first_authority_file_path_hash(INDEX_SOURCE, HashAlgorithm::Blake3_256))
    .unwrap()
    .unwrap()
    .total_length as usize;
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  drop(captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap());
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for (size, code) in [
    (record_size, "semantic_source_entity_allocation"),
    (original.len(), "semantic_source_record_allocation"),
    (32, "semantic_source_digest_allocation"),
  ] {
    let (result, allocations) = measure(size, || captured.read_protected_source(INDEX_SOURCE, source_bounds()));
    assert!(allocations.injected_failure, "{size}: {allocations:?}");
    let error = result.err().expect("source allocation must refuse");
    assert_eq!(error.code(), code);
    assert!(error.source().is_some());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap().encoded_record(), original);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_protected_source_late_cancellation_and_pressure_never_return_partial_success() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for after in [1, 2] {
    for cancel in [true, false] {
      let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-late-cancel", None, [1; 16]);
      seed_raw_source(&publisher, INDEX_SOURCE, 1, 0, 0, false, CompressionAlgorithm::Zstd);
      let before = fs::read(&path).unwrap();
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let mut chunks = 0;
      let result = captured.read_protected_source_with_observer(INDEX_SOURCE, source_bounds(), || {
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        chunks += 1;
        if chunks == after {
          if cancel {
            cancellation.cancel();
          } else {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
          }
        }
      });
      assert_eq!(chunks, after);
      assert_eq!(
        result.err().expect("late refusal must survive").code(),
        if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" }
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_protected_source_supports_the_actual_64_mib_module_bound_without_maximum_size_reservation_for_small_inputs() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("protected-source-module-bound", None, [1; 16]);
  let body = vec![0x71; 64 << 20];
  let module_path = "/.aeordb-system/plugin-artifacts/blake3/raw-reader-fixture";
  seed_files(&publisher, &[(module_path.to_string(), "application/octet-stream", &body)]);
  let before_header = publisher.observe().unwrap();
  let before_length = fs::metadata(&path).unwrap().len();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(512 << 20, 768 << 20, 1, 32 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let limits = NativeSemanticSourceReadBoundsV1 {
    maximum_body_bytes: 64 << 20,
    maximum_chunk_entity_bytes: (64 << 20) + 8192,
    maximum_chunks: 1,
    maximum_read_bytes: 65 << 20,
  };
  let source = captured.read_protected_source(module_path, limits).unwrap().unwrap();
  assert_eq!(source.body(), body);
  assert!(memory.snapshot().unwrap().reserved_bytes < retained + (65 << 20));
  drop(source);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(publisher.observe().unwrap(), before_header);
  assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
  // A large admission ceiling is not itself a retained allocation request.
  drop(captured);
  let framed_capture = protection
    .capture_semantic_mutation_inventory(
      NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: (64 << 20) + 8192, maximum_read_bytes: 65 << 20, ..capture_bounds() },
      &memory,
      &cancellation,
    )
    .unwrap();
  assert!(framed_capture.visit(|_| Ok(true)).unwrap().complete);
  assert_eq!(publisher.observe().unwrap(), before_header);
  assert_eq!(fs::metadata(&path).unwrap().len(), before_length);
  drop(framed_capture);
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", b"{}")]);
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let small = captured.read_protected_source(INDEX_SOURCE, limits).unwrap().unwrap();
  assert_eq!(small.body(), b"{}");
  assert!(memory.snapshot().unwrap().reserved_bytes < retained + (1 << 20));
}
