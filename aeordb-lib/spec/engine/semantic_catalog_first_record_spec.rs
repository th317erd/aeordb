use super::*;

#[path = "semantic_catalog_first_record_boundary_spec.rs"]
mod boundary;

#[path = "semantic_catalog_first_record_progression_spec.rs"]
mod progression;

#[test]
fn first_record_matches_independent_digest_order_without_scanning_siblings() {
  for algorithm in ALGORITHMS {
    for count in [1, 96] {
      let (store, bindings, root, bounds) = fixture(algorithm, count);
      let expected = bindings.iter().min_by_key(|binding| lookup(algorithm, binding)).unwrap();
      let visited = Cell::new(0);
      let actual = SemanticCatalogReaderV1::new(algorithm, &store)
        .with_first_record(&root, bounds, &|| false, |record| {
          visited.set(visited.get() + 1);
          Ok((record.record_kind, record.owner_key.to_vec(), record.semantic_id.to_vec(), record.definition_object_id.to_vec()))
        })
        .expect("an admitted nonempty catalog must expose its first binding");
      assert_eq!(actual, (expected.kind, expected.owner.clone(), expected.semantic.clone(), expected.definition.clone()));
      assert_eq!(visited.get(), 1);
      let reads = store.reads.borrow();
      assert!(reads.len() <= 2 * (algorithm.hash_length() + 1));
      assert!(reads.iter().all(|(kind, _)| matches!(kind, 2 | 3)));
      if count > 1 {
        assert!(reads.len() < store.objects.len(), "first selection scanned the whole tree");
      }
    }
  }
}

#[test]
fn first_record_is_the_same_exact_binding_as_ordinal_zero() {
  for algorithm in ALGORITHMS {
    let (store, bindings, root, bounds) = fixture(algorithm, 96);
    let expected = bindings.iter().min_by_key(|binding| lookup(algorithm, binding)).unwrap();
    // The independent oracle selects the key; the preexisting exact lookup
    // supplies its ordinal without using the new first-selection implementation.
    let ordinal = SemanticCatalogReaderV1::new(algorithm, &store)
      .with_record_ordinal(&root, bounds, expected.kind, &expected.owner, &|| false, |position, _| Ok(position))
      .unwrap();
    assert_eq!(ordinal, Some(0));
    store.reads.borrow_mut().clear();
    let actual = SemanticCatalogReaderV1::new(algorithm, &store)
      .with_first_record(&root, bounds, &|| false, |record| Ok((record.record_kind, record.owner_key.to_vec())))
      .expect("first selection must return the exact ordinal-zero binding");
    assert_eq!(actual, (expected.kind, expected.owner.clone()));
    assert!(store.reads.borrow().len() <= 2 * (algorithm.hash_length() + 1));
  }
}
