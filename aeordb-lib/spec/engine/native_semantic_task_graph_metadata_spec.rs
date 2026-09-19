//! Ordinary payload references are not a claim of verified content health.
use super::*;

#[test]
fn native_semantic_task_graph_metadata_keeps_exact_edges_without_ordinary_payload_reads() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-metadata", None, [1; 16], algorithm, 0);
    let (expected, _, _) = seed_captured_graph(&publisher);
    let chunk_key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
    let chunk = publisher.locator(&chunk_key).unwrap().unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let deep = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
    let mut actual = PhysicalSet::new();
    let metadata = capture
      .visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(metadata.physical_reads + 1, deep.physical_reads);
    assert_eq!(metadata.read_bytes + u64::from(chunk.total_length), deep.read_bytes);
    let exact = NativeSemanticTaskGraphBoundsV1 { maximum_read_bytes: metadata.read_bytes, ..graph_bounds() };
    assert_eq!(capture.visit_captured_semantic_task_metadata_entries(&[2; 16], exact, |_| Ok(())).unwrap(), metadata);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_metadata_retains_damaged_payload_but_deep_inspection_refuses_it() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-opaque", None, [1; 16], algorithm, 0);
    let (expected, _, _) = seed_captured_graph(&publisher);
    let chunk_key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
    corrupt_last_entity_byte(&publisher, &chunk_key);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut actual = PhysicalSet::new();
    let metadata = capture
      .visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(metadata.namespace_chunks, 1);
    let error = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap_err();
    assert_eq!(error.code(), "integrity_hash_mismatch");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_metadata_refuses_missing_roles_and_invalid_reference_extents() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in ["missing", "role", "header", "kv", "overlap", "tail", "overflow", "length"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("task-graph-reference-extent", None, [1; 16], algorithm, 0);
      seed_captured_graph(&publisher);
      let key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
      let header = publisher.observe().unwrap().selected.header;
      if case == "missing" {
        assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
        seed_files(&publisher, &[]);
      } else {
        let mut kv = publisher.lock_kv().unwrap();
        let mut locator = kv.get(&key).unwrap().unwrap();
        match case {
          "role" => locator.type_flags = KV_TYPE_FILE_RECORD,
          "header" => locator.offset = 0,
          "kv" => locator.offset = header.kv_block_offset,
          "overlap" => locator.offset = header.kv_block_offset + header.kv_block_length - 1,
          "tail" => locator.offset = header.hot_tail_offset,
          "overflow" => locator.offset = u64::MAX - 1,
          "length" => locator.total_length = 1,
          _ => unreachable!(),
        }
        kv.insert(locator).unwrap();
      }
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(&path).unwrap();
      let mut visited_invalid = false;
      let error = capture
        .visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |entry| {
          visited_invalid |= entry.hash == key;
          Ok(())
        })
        .unwrap_err();
      assert_eq!(
        error.code(),
        match case {
          "missing" => "semantic_source_chunk_missing",
          "role" => "semantic_task_graph_entity_role",
          "length" => "semantic_task_graph_chunk_reference",
          _ => "semantic_task_inventory_extent",
        },
        "{case}"
      );
      assert!(!visited_invalid, "invalid reference must not reach callback: {case}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_task_graph_metadata_reference_callbacks_preserve_interruptions_and_original_errors() {
  for case in ["cancel", "pressure", "error-and-cancel"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-reference-callback", None, [1; 16], algorithm, 0);
    seed_captured_graph(&publisher);
    let key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut calls = 0;
    let error = capture
      .visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |entry| {
        if entry.hash == key {
          calls += 1;
          assert!(publisher.root_state.try_lock().is_ok());
          assert!(publisher.kv.try_lock().is_ok());
          if case == "pressure" {
            memory
              .update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 96 << 20, ..Default::default() })
              .unwrap();
          } else {
            cancellation.cancel();
          }
          if case == "error-and-cancel" {
            return Err(SemanticMutationObservationErrorV1::Resource { code: "opaque_reference_callback", message: "original cause" });
          }
        }
        Ok(())
      })
      .unwrap_err();
    assert_eq!(calls, 1);
    assert_eq!(
      error.code(),
      match case {
        "pressure" => "semantic_task_observation_memory",
        "cancel" => "semantic_task_observation_cancelled",
        _ => "opaque_reference_callback",
      }
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_metadata_charges_reference_work_and_exact_metadata_read_limits() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-reference-budget", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  seed_captured_graph(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let original = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
  for case in ["exact", "work", "read"] {
    let mut bounds =
      NativeSemanticTaskGraphBoundsV1 { maximum_work: original.work, maximum_read_bytes: original.read_bytes, ..graph_bounds() };
    if case == "work" {
      bounds.maximum_work -= 1;
    }
    if case == "read" {
      bounds.maximum_read_bytes -= 1;
    }
    let result = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], bounds, |_| Ok(()));
    if case == "exact" {
      assert_eq!(result.unwrap(), original);
    } else {
      assert_eq!(
        result.unwrap_err().code(),
        if case == "work" { "semantic_task_graph_work_bound" } else { "semantic_task_inventory_read_bound" }
      );
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_metadata_uses_captured_chunk_locators_after_live_removal() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-reference-snapshot", None, [1; 16], algorithm, 0);
  seed_captured_graph(&publisher);
  let key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
  let original = publisher.locator(&key).unwrap().unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
  seed_files(&publisher, &[]);
  let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let mut retained = None;
  old
    .visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |entry| {
      if entry.hash == key {
        retained = Some(entry.clone());
      }
      Ok(())
    })
    .unwrap();
  let retained = retained.unwrap();
  assert_eq!((retained.hash, retained.offset, retained.total_length), (original.hash, original.offset, original.total_length));
  assert_eq!(
    fresh.visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap_err().code(),
    "semantic_source_chunk_missing"
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_graph_metadata_requires_chunk_references_for_nonempty_files() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for version in [0, 1] {
      for size in [0u64, 1] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("task-graph-empty-refs", None, [1; 16], algorithm, 0);
        let (mut expected, _, _) = seed_captured_graph(&publisher);
        // Independent FileRecord bytes: /empty, absent MIME, declared size,
        // timestamps, optional whole-file identity, no metadata and no chunks.
        let mut raw = vec![6, 0];
        raw.extend_from_slice(b"/empty");
        raw.extend_from_slice(&0u16.to_le_bytes());
        raw.extend_from_slice(&size.to_le_bytes());
        raw.extend_from_slice(&11i64.to_le_bytes());
        raw.extend_from_slice(&13i64.to_le_bytes());
        if version == 1 {
          raw.extend_from_slice(&digest_parts(algorithm, &[b""]));
        }
        raw.extend_from_slice(&[0; 8]);
        let file = publish_namespace_value(&publisher, EntryTypeV4::FileRecord, version, b"filec:", &raw);
        let tree = publish_namespace_directory(
          &publisher,
          vec![NamespaceFixtureChild { name: "empty".into(), kind: EntryTypeV4::FileRecord, key: file, size, content_type: None }],
        );
        phase::use_staged_tree(&publisher, &mut expected, &tree);
        let memory = observation_memory();
        let cancellation = CancellationToken::new();
        let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
        let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
        let baseline = memory.snapshot().unwrap().reserved_bytes;
        let before = fs::read(&path).unwrap();
        let result = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |_| Ok(()));
        if size == 0 {
          let summary = result.unwrap();
          assert_eq!(
            (summary.namespace_files, summary.namespace_chunks, summary.opaque_chunk_references, summary.opaque_chunk_bytes),
            (1, 0, 0, 0)
          );
        } else {
          assert_eq!(result.unwrap_err().code(), "semantic_task_graph_file_length");
        }
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        assert_eq!(fs::read(&path).unwrap(), before);
      }
    }
  }
}
