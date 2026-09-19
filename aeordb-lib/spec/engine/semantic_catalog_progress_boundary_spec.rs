use super::*;

#[test]
fn valid_non_progress_phases_and_empty_compilation_refuse_before_object_reads() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let input = checkpoint(&mut fixture.store, &fixture.original, &fixture.retained, &[], 2, 1, 1);
    let width = algorithm.hash_length();
    for phase in [1u16, 2, 4, 5] {
      let mut bytes = input.bytes.clone();
      bytes[120..122].copy_from_slice(&phase.to_le_bytes());
      if phase <= 2 {
        for offset in [104, 112, 120, 128] {
          change_count(&mut bytes, offset, 0);
        }
        bytes[32 + 168 + 2 * width..32 + 168 + 3 * width].fill(0);
      } else {
        for slot in [4, 5] {
          bytes[32 + 168 + slot * width..32 + 168 + (slot + 1) * width].fill(slot as u8);
        }
        if phase == 5 {
          change_count(&mut bytes, 144, 2);
        }
      }
      repair_crc(&mut bytes);
      decode_semantic_mutation_checkpoint(&bytes, algorithm).expect("phase fixture must be valid ASMC");
      fixture.store.reads.set(0);
      let before = fixture.memory.snapshot().unwrap().reserved_bytes;
      let error = refused(admit_semantic_catalog_progress_v1(
        request(algorithm, 1),
        &bytes,
        &fixture.registry,
        &fixture.store,
        &fixture.memory,
        &|| false,
      ));
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code, .. }
        if code == if phase == 2 { "semantic_catalog_progress_empty" } else { "semantic_catalog_progress_phase" }));
      assert_eq!(fixture.store.reads.get(), 0);
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
    }
  }
}

#[test]
fn progress_adapter_preserves_malformed_checkpoint_classification_before_object_reads() {
  use aeordb::engine::v4::reader::MalformedInputClass;
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let input = checkpoint(&mut fixture.store, &fixture.original, &fixture.retained, &[], 2, 1, 1);
    let mut malformed: Vec<_> = (0..input.bytes.len()).map(|length| input.bytes[..length].to_vec()).collect();
    let mut trailing = input.bytes.clone();
    trailing.push(0);
    malformed.push(trailing);
    for offset in [0, 4, 12, 28, 32, 120, 122, 124, 32 + 168] {
      let mut bytes = input.bytes.clone();
      bytes[offset] ^= 0xff;
      repair_crc(&mut bytes);
      if decode_semantic_mutation_checkpoint(&bytes, algorithm).is_err() {
        malformed.push(bytes);
      }
    }
    // A root/count disagreement is malformed framing, not a missing catalog.
    let mut bytes = input.bytes.clone();
    change_count(&mut bytes, 152, 1);
    malformed.push(bytes);
    let before = fixture.memory.snapshot().unwrap().reserved_bytes;
    fixture.store.reads.set(0);
    for bytes in malformed {
      let expected = decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap_err();
      let class = if expected.class() == MalformedInputClass::AllocationAmplification {
        SemanticCatalogReadErrorClassV1::ResourceLimit
      } else {
        SemanticCatalogReadErrorClassV1::Corrupt
      };
      let error = refused(admit_semantic_catalog_progress_v1(
        request(algorithm, 1),
        &bytes,
        &fixture.registry,
        &fixture.store,
        &fixture.memory,
        &|| false,
      ));
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == class && error.code() == expected.code())
      );
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
    }
    assert_eq!(fixture.store.reads.get(), 0);
  }
}

#[test]
fn progress_rejects_registry_mismatch_candidate_geometry_and_coordinator_pressure() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let candidates: Vec<_> = fixture.retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).cloned().collect();
    let input = checkpoint(&mut fixture.store, &fixture.original, &fixture.retained, &candidates, 2, 1, 1);
    let different = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(br#"{"$v":1,"parsers":{"text/plain":"p"}}"#),
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &AvailableSnapshot,
      &fixture.memory,
      &|| false,
    )
    .unwrap();
    let before = fixture.memory.snapshot().unwrap().reserved_bytes;
    refused(admit_semantic_catalog_progress_v1(request(algorithm, 1), &input.bytes, &different, &fixture.store, &fixture.memory, &|| {
      false
    }));
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
    for (offset, count) in [(152, candidates.len() as u64 + 1), (160, input.pruning_nodes + 1)] {
      let mut bytes = input.bytes.clone();
      change_count(&mut bytes, offset, count);
      decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap();
      let error = refused(admit_semantic_catalog_progress_v1(
        request(algorithm, 1),
        &bytes,
        &fixture.registry,
        &fixture.store,
        &fixture.memory,
        &|| false,
      ));
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Corrupt)
      );
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
    }
    let constrained = MemoryCoordinator::new(MemoryPolicy::new(8 << 20, 16 << 20, 1, 1 << 20).unwrap());
    fixture.store.reads.set(0);
    let error = refused(admit_semantic_catalog_progress_v1(
      request(algorithm, 1),
      &input.bytes,
      &fixture.registry,
      &fixture.store,
      &constrained,
      &|| false,
    ));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
    );
    assert_eq!(fixture.store.reads.get(), 0);
    assert_eq!(constrained.snapshot().unwrap().reserved_bytes, 0);
    drop(
      admit_semantic_catalog_progress_v1(request(algorithm, 1), &input.bytes, &fixture.registry, &fixture.store, &fixture.memory, &|| {
        false
      })
      .unwrap(),
    );
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
  }
}

struct Fixture {
  memory: MemoryCoordinator,
  registry: CompiledParserRegistryV1,
  store: Store,
  original: EncodedSemanticObjectV1,
  retained: Vec<catalog_oracle::Binding>,
}

fn setup(algorithm: HashAlgorithm) -> Fixture {
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let original = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [Ok(configured(algorithm, &memory, &registry, "/"))],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap()
  .semantic_state()
  .clone();
  let retained = bindings(&store, &original);
  Fixture { memory, registry, store, original, retained }
}

fn refused(result: Result<AdmittedSemanticCatalogProgressV1, SemanticCatalogCompilationErrorV1>) -> SemanticCatalogCompilationErrorV1 {
  match result {
    Ok(_) => panic!("invalid progress was admitted"),
    Err(error) => error,
  }
}

fn repair_crc(bytes: &mut [u8]) {
  let end = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&crc.to_le_bytes());
}

fn change_count(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[32 + offset..40 + offset].copy_from_slice(&value.to_le_bytes());
  repair_crc(bytes);
}

#[test]
fn live_dependency_candidates_are_compiling_progress_but_not_pruning_progress() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let candidates: Vec<_> = fixture.retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).cloned().collect();
    assert!(!candidates.is_empty());
    for phase in [2, 3] {
      let input = checkpoint(&mut fixture.store, &fixture.original, &fixture.retained, &candidates, phase, 1, 1);
      let before = fixture.memory.snapshot().unwrap().reserved_bytes;
      let writes = fixture.store.writes;
      let result = admit_semantic_catalog_progress_v1(
        request(algorithm, 1),
        &input.bytes,
        &fixture.registry,
        &fixture.store,
        &fixture.memory,
        &|| false,
      );
      if phase == 2 {
        let progress = result.expect("a compiling candidate may still be used elsewhere");
        assert_snapshot(&progress, &input);
        drop(progress);
      } else {
        assert!(
          matches!(refused(result), SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_progress_live_candidate")
        );
      }
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
      assert_eq!(fixture.store.writes, writes);
    }
  }
}

#[test]
fn candidates_must_account_for_every_orphan_with_exact_dependency_bindings() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let retained: Vec<_> = fixture.retained.iter().filter(|binding| matches!(binding.kind, 2 | 6 | 7)).cloned().collect();
    let dependencies: Vec<_> = retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).cloned().collect();
    assert!(!dependencies.is_empty());
    for mode in 0..5 {
      let mut candidates = dependencies.clone();
      match mode {
        0 => candidates.clear(),
        1 => {
          candidates.remove(0);
        }
        2 => candidates[0] = retained.iter().find(|binding| binding.kind == 2).unwrap().clone(),
        3 => {
          candidates[0].owner[0] ^= 1;
          candidates[0].semantic[0] ^= 1;
        }
        4 => candidates[0].definition[0] ^= 1,
        _ => unreachable!(),
      }
      let input = checkpoint(&mut fixture.store, &fixture.original, &retained, &candidates, 2, 0, 0);
      if let Some(root) = input.pruning_root.as_deref() {
        SemanticCatalogReaderV1::new(algorithm, &fixture.store)
          .walk_catalog(root, SemanticCatalogTraversalBoundsV1::new(input.pruning_records, input.pruning_nodes).unwrap(), &|| false, |_| {
            Ok(())
          })
          .expect("negative fixture must have structurally valid candidate nodes");
      }
      let before = fixture.memory.snapshot().unwrap().reserved_bytes;
      let writes = fixture.store.writes;
      let error = refused(admit_semantic_catalog_progress_v1(
        request(algorithm, 0),
        &input.bytes,
        &fixture.registry,
        &fixture.store,
        &fixture.memory,
        &|| false,
      ));
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Corrupt),
        "mode {mode}"
      );
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
      assert_eq!(fixture.store.writes, writes);
    }
  }
}

#[test]
fn well_framed_wrong_profiles_counts_and_premature_pruning_do_not_create_progress() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let input = checkpoint(&mut fixture.store, &fixture.original, &fixture.retained, &[], 2, 1, 2);
    let decoded = decode_semantic_mutation_checkpoint(&input.bytes, algorithm).unwrap();
    for mode in 0..8 {
      let mut bytes = input.bytes.clone();
      let mut expected = 2;
      match mode {
        0 => bytes[32 + 168 + 6 * algorithm.hash_length()] ^= 1,
        1 => bytes[32 + 168 + 7 * algorithm.hash_length()] ^= 1,
        2 => change_count(&mut bytes, 112, decoded.record_count + 1),
        3 => change_count(&mut bytes, 120, decoded.node_count + 1),
        4 => change_count(&mut bytes, 104, 0),
        5 => change_count(&mut bytes, 128, decoded.dependency_count + 1),
        6 => bytes[32 + 88..32 + 90].copy_from_slice(&3u16.to_le_bytes()),
        7 => expected = 3,
        _ => unreachable!(),
      }
      repair_crc(&mut bytes);
      decode_semantic_mutation_checkpoint(&bytes, algorithm).expect("negative counts/profile fixture must pass framing");
      let before = fixture.memory.snapshot().unwrap().reserved_bytes;
      let writes = fixture.store.writes;
      refused(admit_semantic_catalog_progress_v1(
        request(algorithm, expected),
        &bytes,
        &fixture.registry,
        &fixture.store,
        &fixture.memory,
        &|| false,
      ));
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
      assert_eq!(fixture.store.writes, writes);
    }
  }
}

#[test]
fn progress_cancellation_and_impossible_workspace_refuse_before_catalog_reads() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let input = checkpoint(&mut fixture.store, &fixture.original, &fixture.retained, &[], 2, 1, 1);
    let before = fixture.memory.snapshot().unwrap().reserved_bytes;
    fixture.store.reads.set(0);
    let error = refused(admit_semantic_catalog_progress_v1(
      request(algorithm, 1),
      &input.bytes,
      &fixture.registry,
      &fixture.store,
      &fixture.memory,
      &|| true,
    ));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Cancelled)
    );
    let mut limited = request(algorithm, 1);
    limited.maximum_workspace_bytes = 0;
    let error =
      refused(admit_semantic_catalog_progress_v1(limited, &input.bytes, &fixture.registry, &fixture.store, &fixture.memory, &|| false));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
    );
    let mut bytes = input.bytes.clone();
    change_count(&mut bytes, 112, 1 << 40);
    decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap();
    let error = refused(admit_semantic_catalog_progress_v1(
      request(algorithm, 1),
      &bytes,
      &fixture.registry,
      &fixture.store,
      &fixture.memory,
      &|| false,
    ));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_reachability_memory"));
    assert_eq!(fixture.store.reads.get(), 0);
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
    drop(
      admit_semantic_catalog_progress_v1(request(algorithm, 1), &input.bytes, &fixture.registry, &fixture.store, &fixture.memory, &|| {
        false
      })
      .unwrap(),
    );
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
  }
}

#[test]
fn each_partial_admission_source_failure_keeps_its_class_releases_memory_and_retries() {
  for algorithm in ALGORITHMS {
    let mut fixture = setup(algorithm);
    let retained: Vec<_> = fixture.retained.iter().filter(|binding| matches!(binding.kind, 2 | 6 | 7)).cloned().collect();
    let candidates: Vec<_> = retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).cloned().collect();
    let input = checkpoint(&mut fixture.store, &fixture.original, &retained, &candidates, 3, 0, 0);
    fixture.store.present_reads.set(0);
    drop(
      admit_semantic_catalog_progress_v1(request(algorithm, 0), &input.bytes, &fixture.registry, &fixture.store, &fixture.memory, &|| {
        false
      })
      .unwrap(),
    );
    let reads = fixture.store.present_reads.get();
    assert!(reads > 1);
    let before = fixture.memory.snapshot().unwrap().reserved_bytes;
    let objects = fixture.store.objects.clone();
    for at in 1..=reads {
      for fault in [ReadFault::Unavailable, ReadFault::Missing, ReadFault::Corrupt, ReadFault::Resource] {
        fixture.store.present_reads.set(0);
        fixture.store.read_fault = Some((at, fault));
        let error = refused(admit_semantic_catalog_progress_v1(
          request(algorithm, 0),
          &input.bytes,
          &fixture.registry,
          &fixture.store,
          &fixture.memory,
          &|| false,
        ));
        let SemanticCatalogCompilationErrorV1::Catalog(error) = error else {
          panic!("source error lost its catalog class")
        };
        let expected = match fault {
          ReadFault::Unavailable => SemanticCatalogReadErrorClassV1::Unavailable,
          ReadFault::Resource => SemanticCatalogReadErrorClassV1::ResourceLimit,
          ReadFault::Missing | ReadFault::Corrupt => SemanticCatalogReadErrorClassV1::Corrupt,
        };
        assert_eq!(error.class(), expected, "read {at}");
        if matches!(fault, ReadFault::Unavailable | ReadFault::Resource) {
          assert_eq!(error.code(), "test_staging_read");
        }
        assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
        fixture.store.read_fault = None;
        drop(
          admit_semantic_catalog_progress_v1(
            request(algorithm, 0),
            &input.bytes,
            &fixture.registry,
            &fixture.store,
            &fixture.memory,
            &|| false,
          )
          .unwrap(),
        );
        assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
      }
    }
    assert_eq!(fixture.store.objects, objects);
  }
}

#[test]
fn every_partial_admission_cancellation_boundary_refuses_and_releases_the_bitmap() {
  let algorithm = ALGORITHMS[2];
  let mut fixture = setup(algorithm);
  let retained: Vec<_> = fixture.retained.iter().filter(|binding| matches!(binding.kind, 2 | 6 | 7)).cloned().collect();
  let candidates: Vec<_> = retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).cloned().collect();
  let input = checkpoint(&mut fixture.store, &fixture.original, &retained, &candidates, 3, 0, 0);
  let checks = Cell::new(0);
  drop(
    admit_semantic_catalog_progress_v1(request(algorithm, 0), &input.bytes, &fixture.registry, &fixture.store, &fixture.memory, &|| {
      checks.set(checks.get() + 1);
      false
    })
    .unwrap(),
  );
  assert!(checks.get() > 1);
  let before = fixture.memory.snapshot().unwrap().reserved_bytes;
  for at in 1..=checks.get() {
    let current = Cell::new(0);
    let error = refused(admit_semantic_catalog_progress_v1(
      request(algorithm, 0),
      &input.bytes,
      &fixture.registry,
      &fixture.store,
      &fixture.memory,
      &|| {
        current.set(current.get() + 1);
        current.get() >= at
      },
    ));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Cancelled),
      "check {at}"
    );
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
  }
  drop(
    admit_semantic_catalog_progress_v1(request(algorithm, 0), &input.bytes, &fixture.registry, &fixture.store, &fixture.memory, &|| false)
      .unwrap(),
  );
  assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, before);
}
