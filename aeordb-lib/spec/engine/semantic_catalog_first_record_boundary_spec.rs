use super::*;

#[test]
fn first_record_invalid_root_and_early_cancellation_never_read_storage() {
  for algorithm in ALGORITHMS {
    let (store, _, root, bounds) = fixture(algorithm, 2);
    let width = algorithm.hash_length();
    for candidate in [Vec::new(), vec![0; width], vec![1; width - 1], vec![1; width + 1]] {
      let error = SemanticCatalogReaderV1::new(algorithm, &store).with_first_record(&candidate, bounds, &|| false, |_| Ok(())).unwrap_err();
      assert_eq!(error.class(), Class::Corrupt);
      assert!(store.reads.borrow().is_empty());
    }
    let error = SemanticCatalogReaderV1::new(algorithm, &store).with_first_record(&root, bounds, &|| true, |_| Ok(())).unwrap_err();
    assert_eq!(error.class(), Class::Cancelled);
    assert!(store.reads.borrow().is_empty());
  }
}

#[test]
fn first_record_preserves_each_source_failure_and_allows_clean_retry() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (mut store, _, root, bounds) = fixture(algorithm, 96);
    let read = |store: &Store| {
      SemanticCatalogReaderV1::new(algorithm, store).with_first_record(&root, bounds, &|| false, |record| Ok(record.semantic_id.to_vec()))
    };
    let expected = read(&store).unwrap();
    let count = store.reads.borrow().len();
    for at in 1..=count {
      for class in [Class::Cancelled, Class::Unavailable, Class::ResourceLimit, Class::Corrupt] {
        store.reads.borrow_mut().clear();
        store.failure = Some((at, class));
        let error = read(&store).unwrap_err();
        assert_eq!((error.class(), error.code()), (class, "test_source"));
        store.failure = None;
        store.reads.borrow_mut().clear();
        assert_eq!(read(&store).unwrap(), expected);
      }
    }
  }
}

#[test]
fn first_record_cancellation_at_every_boundary_and_callback_error_keep_the_original_cause() {
  let algorithm = ALGORITHMS[0];
  let (store, _, root, bounds) = fixture(algorithm, 96);
  let reader = SemanticCatalogReaderV1::new(algorithm, &store);
  let checks = Cell::new(0);
  reader
    .with_first_record(
      &root,
      bounds,
      &|| {
        checks.set(checks.get() + 1);
        false
      },
      |_| Ok(()),
    )
    .unwrap();
  for at in 1..=checks.get() {
    let observed = Cell::new(0);
    let error = reader
      .with_first_record(
        &root,
        bounds,
        &|| {
          observed.set(observed.get() + 1);
          observed.get() >= at
        },
        |_| Ok(()),
      )
      .unwrap_err();
    assert_eq!(error.class(), Class::Cancelled);
  }
  let cancelled = Cell::new(false);
  let error = reader
    .with_first_record(&root, bounds, &|| cancelled.get(), |_| {
      cancelled.set(true);
      Ok(())
    })
    .unwrap_err();
  assert_eq!(error.class(), Class::Cancelled);
  for class in [Class::Cancelled, Class::Unavailable, Class::ResourceLimit, Class::Corrupt] {
    cancelled.set(false);
    let error = reader
      .with_first_record(&root, bounds, &|| cancelled.get(), |_| -> Result<(), SemanticCatalogReadErrorV1> {
        cancelled.set(true);
        Err(match class {
          Class::Cancelled => SemanticCatalogReadErrorV1::cancelled("first_callback", "caller cancellation"),
          Class::Unavailable => SemanticCatalogReadErrorV1::unavailable("first_callback", "caller unavailable"),
          Class::ResourceLimit => SemanticCatalogReadErrorV1::resource("first_callback", "caller resource"),
          Class::Corrupt => SemanticCatalogReadErrorV1::corrupt("first_callback", "caller malformed"),
        })
      })
      .unwrap_err();
    assert_eq!((error.class(), error.code()), (class, "first_callback"));
  }
}

#[test]
fn first_record_rejects_missing_ambiguous_damaged_wrong_kind_and_wrong_identity_nodes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for mode in 0..5 {
      let (mut store, _, root, bounds) = fixture(algorithm, 2);
      let original = store.objects.remove(&(3, root.clone())).unwrap();
      match mode {
        0 => {}
        1 => {
          store.objects.insert((2, root.clone()), original.clone());
          store.objects.insert((3, root.clone()), original);
        }
        2 => {
          let mut damaged = original;
          damaged[0] ^= 1;
          store.objects.insert((3, root.clone()), damaged);
        }
        3 => {
          store.objects.insert((2, root.clone()), original);
        }
        4 => {
          let replacement = oracle(algorithm, &[binding(algorithm, 500), binding(algorithm, 501)], 0).0;
          store.objects.insert((3, root.clone()), replacement.value);
        }
        _ => unreachable!(),
      }
      let visited = Cell::new(false);
      let error = SemanticCatalogReaderV1::new(algorithm, &store)
        .with_first_record(&root, bounds, &|| false, |_| {
          visited.set(true);
          Ok(())
        })
        .unwrap_err();
      assert_eq!(error.class(), Class::Corrupt, "mode {mode}");
      assert!(!visited.get());
      if mode == 4 {
        assert_eq!(error.code(), "semantic_catalog_identity");
      }
    }
  }
}

#[test]
fn first_record_rejects_wrong_parent_counts_depth_and_prefix() {
  for algorithm in ALGORITHMS {
    let (mut store, _, root, bounds) = fixture(algorithm, 2);
    for wrong in [SemanticCatalogTraversalBoundsV1::new(3, 3).unwrap(), SemanticCatalogTraversalBoundsV1::new(2, 1).unwrap()] {
      let error = SemanticCatalogReaderV1::new(algorithm, &store).with_first_record(&root, wrong, &|| false, |_| Ok(())).unwrap_err();
      assert_eq!(error.class(), Class::Corrupt);
    }
    let width = algorithm.hash_length();
    let mut bytes = store.objects[&(3, root)].clone();
    let prefix = u16::from_le_bytes(bytes[38..40].try_into().unwrap()) as usize;
    let first = 32 + 20 + prefix + 12;
    let second = first + 12 + width;
    let first_identity = bytes[first..first + width].to_vec();
    let second_identity = bytes[second..second + width].to_vec();
    bytes[first..first + width].copy_from_slice(&second_identity);
    bytes[second..second + width].copy_from_slice(&first_identity);
    let end = bytes.len() - 4;
    let checksum = crc32fast::hash(&bytes[..end]);
    bytes[end..].copy_from_slice(&checksum.to_le_bytes());
    let forged = digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &3u16.to_le_bytes(), &bytes].concat());
    store.objects.insert((3, forged.clone()), bytes);
    let error = SemanticCatalogReaderV1::new(algorithm, &store).with_first_record(&forged, bounds, &|| false, |_| Ok(())).unwrap_err();
    assert_eq!(error.code(), "semantic_catalog_leaf_closure");
  }
}

#[test]
fn first_record_does_not_claim_to_validate_untouched_siblings() {
  for algorithm in ALGORITHMS {
    let (mut store, bindings, root, bounds) = fixture(algorithm, 96);
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    let expected = reader.with_first_record(&root, bounds, &|| false, |record| Ok(record.semantic_id.to_vec())).unwrap();
    let loaded: Vec<_> = store.reads.borrow().iter().cloned().collect();
    let unvisited = store.objects.keys().find(|key| !loaded.contains(key)).cloned().unwrap();
    store.objects.remove(&unvisited);
    store.reads.borrow_mut().clear();
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    assert_eq!(reader.with_first_record(&root, bounds, &|| false, |record| Ok(record.semantic_id.to_vec())).unwrap(), expected);
    assert!(reader.walk_catalog(&root, bounds, &|| false, |_| Ok(())).is_err());
    assert_eq!(bindings.len(), 96);
  }
}
