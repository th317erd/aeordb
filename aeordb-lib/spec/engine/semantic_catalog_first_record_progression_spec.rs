use super::*;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::semantic_catalog_mutation::{
  plan_semantic_catalog_mutation_v1, SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1,
};

#[test]
fn first_record_progresses_through_cow_removals_and_matches_the_independent_remaining_tree() {
  for algorithm in ALGORITHMS {
    let (mut store, mut expected, initial_root, initial_bounds) = fixture(algorithm, 40);
    let mut current_root = initial_root.clone();
    let mut current_bounds = initial_bounds;
    let mut nodes = store.objects.len() as u64;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
    expected.sort_by_key(|binding| lookup(algorithm, binding));
    let initial_objects = store.objects.clone();
    while !expected.is_empty() {
      store.reads.borrow_mut().clear();
      let actual = SemanticCatalogReaderV1::new(algorithm, &store)
        .with_first_record(&current_root, current_bounds, &|| false, |record| Ok((record.record_kind, record.owner_key.to_vec())))
        .unwrap();
      assert_eq!(actual, (expected[0].kind, expected[0].owner.clone()));
      assert!(store.reads.borrow().len() <= 2 * (algorithm.hash_length() + 1));
      store.reads.borrow_mut().clear();
      let plan = plan_semantic_catalog_mutation_v1(
        SemanticCatalogMutationRequestV1 {
          hash_algorithm: algorithm,
          snapshot: SemanticCatalogSnapshotV1 {
            root_object_id: Some(&current_root),
            record_count: expected.len() as u64,
            node_count: nodes,
          },
          mutation: SemanticCatalogMutationV1::Remove { record_kind: actual.0, owner_key: &actual.1 },
          maximum_workspace_bytes: 32 << 20,
        },
        &store,
        &memory,
        &|| false,
      )
      .unwrap();
      assert!(store.reads.borrow().len() <= 2 * (algorithm.hash_length() + 2));
      assert_eq!(plan.record_count(), expected.len() as u64 - 1);
      for object in plan.objects() {
        let kind = u16::from_le_bytes(object.value[6..8].try_into().unwrap());
        if let Some(before) = store.objects.insert((kind, object.object_id.clone()), object.value.clone()) {
          assert_eq!(before, object.value);
        }
      }
      expected.remove(0);
      if expected.is_empty() {
        assert!(plan.root_object_id().is_none());
        assert_eq!(plan.node_count(), 0);
      } else {
        let (independent, independent_nodes) = oracle(algorithm, &expected, 0);
        assert_eq!(plan.root_object_id(), Some(independent.object_id.as_slice()));
        assert_eq!(plan.node_count(), independent_nodes);
        current_root = plan.root_object_id().unwrap().to_vec();
        nodes = plan.node_count();
        current_bounds = SemanticCatalogTraversalBoundsV1::new(plan.record_count(), nodes).unwrap();
      }
      drop(plan);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
    for (identity, bytes) in initial_objects {
      assert_eq!(store.objects[&identity], bytes);
    }
    // An earlier checkpoint still selects its earlier immutable first entry.
    let original = SemanticCatalogReaderV1::new(algorithm, &store)
      .with_first_record(&initial_root, initial_bounds, &|| false, |record| Ok((record.record_kind, record.owner_key.to_vec())))
      .unwrap();
    let bindings: Vec<_> = (0..40).map(|index| binding(algorithm, index)).collect();
    let first = bindings.iter().min_by_key(|binding| lookup(algorithm, binding)).unwrap();
    assert_eq!(original, (first.kind, first.owner.clone()));
  }
}

#[test]
fn first_record_descends_compressed_prefixes_without_using_raw_owner_order() {
  for algorithm in ALGORITHMS {
    let mut groups = BTreeMap::new();
    let pair = (0..1024)
      .find_map(|index| {
        let next = binding(algorithm, index);
        let first = lookup(algorithm, &next)[0];
        groups
          .insert(first, next.clone())
          .filter(|previous| lookup(algorithm, previous)[1] != lookup(algorithm, &next)[1])
          .map(|previous| [previous, next])
      })
      .unwrap();
    let mut store = Store::default();
    let (root, nodes) = install_nodes(algorithm, &pair, 0, &mut store);
    let bounds = SemanticCatalogTraversalBoundsV1::new(2, nodes).unwrap();
    let expected = pair.iter().min_by_key(|binding| lookup(algorithm, binding)).unwrap();
    let actual = SemanticCatalogReaderV1::new(algorithm, &store)
      .with_first_record(&root, bounds, &|| false, |record| Ok((record.record_kind, record.owner_key.to_vec())))
      .unwrap();
    assert_eq!(actual, (expected.kind, expected.owner.clone()));
    assert_eq!(store.reads.borrow().len(), 4);
  }
}
