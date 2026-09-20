//! Metadata discovery still validates framing, task dependencies and resources.
use super::*;
#[path = "native_task_metadata_framing_spec.rs"]
mod framing;
#[path = "native_task_metadata_resource_spec.rs"]
mod resource;

fn with_fixture(mut test: impl FnMut(&V4FirstAuthorityPublisher, &MemoryCoordinator, &std::path::Path, HashAlgorithm)) {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-metadata-boundaries", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
    let memory = observation_memory();
    test(&publisher, &memory, &path, algorithm);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn metadata_scan(publisher: &V4FirstAuthorityPublisher) -> Result<SemanticMutationInventorySummaryV1, SemanticMutationObservationErrorV1> {
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation)?;
  capture.visit_metadata(|_| Ok(true))
}

#[test]
fn native_task_metadata_inventory_rejects_header_identity_and_framing_damage() {
  with_fixture(|publisher, memory, path, algorithm| {
    let key = publish_payload(publisher);
    let locator = publisher.locator(&key).unwrap().unwrap();
    let header_length = 77 + algorithm.hash_length();
    let original = fs::read(path).unwrap();
    let entity = original[locator.offset as usize..locator.offset as usize + locator.total_length as usize].to_vec();
    let high_water = publisher.observe().unwrap().selected.header.write_sequence_high_water;
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    for (case, code) in [
      ("magic", "entity_magic_or_version"),
      ("version", "unsupported_entity_version"),
      ("kind", "unknown_entry_type"),
      ("kind-version", "unsupported_entry_type_entity_version"),
      ("header-length", "header_length"),
      ("crc", "header_crc_mismatch"),
      ("total", "total_length"),
      ("flags", "unknown_entity_flags"),
      ("hash", "unknown_hash_algorithm"),
      ("wrong-hash", "hash_algorithm_mismatch"),
      ("compression", "unknown_compression_algorithm"),
      ("encryption", "unsupported_encryption_algorithm"),
      ("key-cap", "entity_component_exceeds_cap"),
      ("value-length", "entity_length_disagreement"),
      ("zero-sequence", "unreserved_write_sequence"),
      ("future-sequence", "unreserved_write_sequence"),
      ("reserved", "reserved_nonzero"),
      ("key", "first_authority_locator_identity"),
      ("key-width", "first_authority_locator_identity"),
    ] {
      let mut changed = entity.clone();
      match case {
        "magic" => changed[0] ^= 1,
        "version" => changed[4] = 2,
        "kind" => changed[5] = 0xff,
        "kind-version" => changed[4] = 1,
        "header-length" => changed[6..8].copy_from_slice(&12u16.to_le_bytes()),
        "crc" => changed[header_length - 1] ^= 1,
        "total" => changed[8..12].copy_from_slice(&(locator.total_length - 1).to_le_bytes()),
        "flags" => changed[12] = 0x80,
        "hash" => changed[13..15].copy_from_slice(&0xffffu16.to_le_bytes()),
        "wrong-hash" => changed[13..15].copy_from_slice(&HashAlgorithm::Sha256.to_u16().to_le_bytes()),
        "compression" => changed[15] = 0xff,
        "encryption" => changed[16] = 1,
        "key-cap" => changed[17..21].copy_from_slice(&u32::MAX.to_le_bytes()),
        "value-length" => changed[21..25].copy_from_slice(&0u32.to_le_bytes()),
        "zero-sequence" => changed[33..41].fill(0),
        "future-sequence" => changed[33..41].copy_from_slice(&(high_water + 1).to_le_bytes()),
        "reserved" => changed[41 + algorithm.hash_length()] = 1,
        "key" => changed[header_length] ^= 1,
        "key-width" => {
          changed[17..21].copy_from_slice(&(algorithm.hash_length() as u32 + 1).to_le_bytes());
          changed[21..25].copy_from_slice(&((256 << 10) - 1u32).to_le_bytes());
        }
        _ => unreachable!(),
      }
      if case != "crc" {
        let checksum = crc32fast::hash(&changed[..header_length - 4]);
        changed[header_length - 4..header_length].copy_from_slice(&checksum.to_le_bytes());
      }
      write_file_at_native(&publisher.file, locator.offset, &changed).unwrap();
      let before = fs::read(path).unwrap();
      let error = captured.visit_metadata(|_| Ok(true)).unwrap_err();
      assert_eq!(error.code(), code, "{algorithm:?}/{case}: {error:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(path).unwrap(), before);
      write_file_at_native(&publisher.file, locator.offset, &entity).unwrap();
      assert_eq!(captured.visit_metadata(|_| Ok(true)).unwrap().tasks, 1);
    }
    assert_eq!(fs::read(path).unwrap(), original);
  });
}

#[test]
fn native_task_metadata_inventory_does_not_skip_large_or_damaged_file_records() {
  with_fixture(|publisher, memory, path, _| {
    let key = publish_ordinary_record(publisher, "/ordinary.txt", 128 << 10);
    let before = fs::read(path).unwrap();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(memory, &cancellation).unwrap();
    {
      let capture = protection
        .capture_semantic_mutation_inventory(
          NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 64 << 10, ..bounds() },
          memory,
          &cancellation,
        )
        .unwrap();
      assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap_err().code(), "first_authority_locator_exceeds_cap");
    }
    assert_eq!(fs::read(path).unwrap(), before);
    corrupt_last_entity_byte(publisher, &key);
    let damaged = fs::read(path).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap_err().code(), "integrity_hash_mismatch");
    assert_eq!(fs::read(path).unwrap(), damaged);
  });
}

#[test]
fn native_task_metadata_inventory_preserves_slot_selection_and_control_damage_policy() {
  for case in ["a-only", "newer-b", "equal", "torn-a", "torn-b", "ambiguous", "physical-a", "physical-b", "role-a", "role-b"] {
    with_fixture(|publisher, _, path, algorithm| {
      let mut a = released_task(algorithm, 2, 1);
      let mut b = a.clone();
      if case == "ambiguous" {
        b[120..128].copy_from_slice(&102u64.to_le_bytes());
        crc(&mut b);
      }
      if case == "torn-a" {
        a[0] ^= 1;
      }
      if case == "torn-b" {
        b[0] ^= 1;
      }
      // The fixture initially contains A; stage the requested exact pair.
      let a_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
      let b_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B).unwrap();
      if case == "newer-b" {
        // Use the existing A as an identical older copy; a newer B proves selection.
        b = released_task(algorithm, 2, 2);
      }
      seed(publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &a)]);
      if case != "a-only" {
        seed(publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &b)]);
      }
      let damaged_path = if case.ends_with("-b") { &b_path } else { &a_path };
      let key = first_authority_file_path_hash(damaged_path, algorithm);
      if case.starts_with("physical-") {
        corrupt_last_entity_byte(publisher, &key);
      }
      if case.starts_with("role-") {
        let mut kv = publisher.lock_kv().unwrap();
        let mut locator = kv.get(&key).unwrap().unwrap();
        locator.type_flags = KV_TYPE_CHUNK;
        kv.insert(locator).unwrap();
      }
      let before = fs::read(path).unwrap();
      let result = metadata_scan(publisher);
      match case {
        "ambiguous" => assert_eq!(result.unwrap_err().code(), "system_control_equal_sequence"),
        "physical-a" | "physical-b" => assert_eq!(result.unwrap_err().code(), "integrity_hash_mismatch"),
        "role-a" | "role-b" => assert_eq!(result.unwrap_err().code(), scan(publisher).unwrap_err().code()),
        _ => assert_eq!(result.unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true }),
      }
      assert_eq!(fs::read(path).unwrap(), before, "{case}");
    });
  }
}

#[test]
fn native_task_metadata_inventory_handles_empty_and_b_only_without_namespace_membership() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-metadata-empty-b", None, [1; 16], algorithm, 0);
    let empty = fs::read(&path).unwrap();
    assert_eq!(metadata_scan(&publisher).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 0, complete: true });
    assert_eq!(fs::read(&path).unwrap(), empty);
    seed(
      &publisher,
      &[
        (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &released_task(algorithm, 2, 1)),
        (SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &frozen(algorithm, "generation")),
      ],
    );
    let before = fs::read(&path).unwrap();
    assert_eq!(metadata_scan(&publisher).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    assert_eq!(fs::read(&path).unwrap(), before);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection
      .capture_semantic_mutation_inventory(NativeSemanticMutationInventoryBoundsV1 { maximum_work: 1, ..bounds() }, &memory, &cancellation)
      .unwrap();
    assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap_err().code(), capture.visit(|_| Ok(true)).unwrap_err().code());
    let capture = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
    let error = capture
      .visit_metadata(|_| {
        cancellation.cancel();
        Ok(true)
      })
      .unwrap_err();
    assert!(matches!(
      error,
      SemanticMutationObservationErrorV1::Authority(FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled(_)))
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_metadata_inventory_preserves_missing_dependencies_and_malformed_task_bindings() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for (case, code) in [
      ("generation", "semantic_task_observation_generation_missing"),
      ("checkpoint", "semantic_task_observation_checkpoint_missing"),
      ("digest", "semantic_task_checkpoint_digest"),
      ("phase", "semantic_task_checkpoint_phase"),
    ] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("task-metadata-missing", None, [1; 16], algorithm, 0);
      let (mut task, checkpoint) = ready_pair(algorithm);
      if case == "digest" {
        task[144] ^= 1;
      }
      if case == "phase" {
        task[128..130].copy_from_slice(&1u16.to_le_bytes());
      }
      crc(&mut task);
      let generation = frozen(algorithm, "generation");
      populate(&publisher, &task, (case != "checkpoint").then_some(&checkpoint), (case != "generation").then_some(&generation));
      let before = fs::read(&path).unwrap();
      assert_eq!(metadata_scan(&publisher).unwrap_err().code(), code, "{case}");
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}
