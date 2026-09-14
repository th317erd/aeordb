//! Bounded lookup and staging traversal over independent Patricia bytes.
#[path = "../helpers/semantic_catalog_oracle.rs"]
mod catalog_oracle;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1 as Class, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1,
  SemanticCatalogTraversalBoundsV1, walk_semantic_catalog_with_mutable_source_v1,
};
use catalog_oracle::{Binding, digest, lookup, oracle};

const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];

#[derive(Default)]
struct Store {
  objects: BTreeMap<(u16, Vec<u8>), Vec<u8>>,
  reads: RefCell<Vec<(u16, Vec<u8>)>>,
  failure: Option<(usize, Class)>,
}

impl SemanticCatalogObjectSourceV1 for Store {
  fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    self.reads.borrow_mut().push((kind, identity.to_vec()));
    if let Some((at, class)) = self.failure {
      if self.reads.borrow().len() == at {
        let error = match class {
          Class::Cancelled => SemanticCatalogReadErrorV1::cancelled("test_source", "cancelled source"),
          Class::Unavailable => SemanticCatalogReadErrorV1::unavailable("test_source", "unavailable source"),
          Class::ResourceLimit => SemanticCatalogReadErrorV1::resource("test_source", "refused source"),
          Class::Corrupt => SemanticCatalogReadErrorV1::corrupt("test_source", "corrupt source"),
        };
        return Err(error);
      }
    }
    Ok(self.objects.get(&(kind, identity.to_vec())).cloned())
  }
}

fn binding(algorithm: HashAlgorithm, index: usize) -> Binding {
  let kind = 1 + (index % 7) as u16;
  let semantic = digest(algorithm, format!("semantic {index}").as_bytes());
  let owner =
    if kind <= 2 { [kind.to_le_bytes().as_slice(), format!("/scope/{index}/control.json").as_bytes()].concat() } else { semantic.clone() };
  Binding { kind, owner, semantic, definition: digest(algorithm, format!("definition {index}").as_bytes()) }
}

fn install_nodes(algorithm: HashAlgorithm, bindings: &[Binding], depth: usize, store: &mut Store) -> (Vec<u8>, u64) {
  let (object, nodes) = oracle(algorithm, bindings, depth);
  let kind = u16::from_le_bytes(object.value[6..8].try_into().unwrap());
  store.objects.insert((kind, object.object_id.clone()), object.value);
  if bindings.len() > 1 {
    let first = lookup(algorithm, &bindings[0]);
    let branch = (depth..algorithm.hash_length())
      .find(|position| bindings.iter().any(|binding| lookup(algorithm, binding)[*position] != first[*position]))
      .unwrap();
    let mut groups: BTreeMap<u8, Vec<Binding>> = BTreeMap::new();
    for binding in bindings {
      groups.entry(lookup(algorithm, binding)[branch]).or_default().push(binding.clone());
    }
    for group in groups.into_values() {
      install_nodes(algorithm, &group, branch + 1, store);
    }
  }
  (object.object_id, nodes)
}

fn fixture(algorithm: HashAlgorithm, count: usize) -> (Store, Vec<Binding>, Vec<u8>, SemanticCatalogTraversalBoundsV1) {
  let bindings: Vec<_> = (0..count).map(|index| binding(algorithm, index)).collect();
  let mut store = Store::default();
  let (root, nodes) = install_nodes(algorithm, &bindings, 0, &mut store);
  (store, bindings, root, SemanticCatalogTraversalBoundsV1::new(count as u64, nodes).unwrap())
}

#[test]
fn point_lookup_returns_exact_full_keys_through_one_bounded_path_for_all_hashes() {
  for algorithm in ALGORITHMS {
    let (store, bindings, root, bounds) = fixture(algorithm, 96);
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    for binding in &bindings {
      store.reads.borrow_mut().clear();
      let found = reader
        .with_record(&root, bounds, binding.kind, &binding.owner, &|| false, |record| {
          assert_eq!(record.record_kind, binding.kind);
          assert_eq!(record.owner_key, binding.owner);
          Ok((record.semantic_id.to_vec(), record.definition_object_id.to_vec()))
        })
        .unwrap();
      assert_eq!(found, Some((binding.semantic.clone(), binding.definition.clone())));
      let reads = store.reads.borrow();
      assert!(reads.len() <= 2 * (algorithm.hash_length() + 1));
      assert!(reads.len() < store.objects.len(), "lookup scanned the whole catalog");
      assert!(reads.iter().all(|(kind, _)| matches!(kind, 2 | 3)), "lookup read definitions");
    }
  }
}

#[test]
fn absent_keys_never_call_the_visitor_or_scan_unrelated_subtrees() {
  for algorithm in ALGORITHMS {
    for count in [1, 96] {
      let (store, _, root, bounds) = fixture(algorithm, count);
      let reader = SemanticCatalogReaderV1::new(algorithm, &store);
      for index in 1000..1032 {
        let key = binding(algorithm, index);
        store.reads.borrow_mut().clear();
        let found = reader
          .with_record(&root, bounds, key.kind, &key.owner, &|| false, |_| -> Result<(), SemanticCatalogReadErrorV1> {
            panic!("absent key visited");
          })
          .unwrap();
        assert_eq!(found, None);
        assert!(store.reads.borrow().len() <= 2 * (algorithm.hash_length() + 1));
      }
    }
  }
}

#[test]
fn invalid_lookup_root_kind_owner_and_cancelled_requests_do_not_read_storage() {
  let algorithm = ALGORITHMS[0];
  let (store, bindings, root, bounds) = fixture(algorithm, 2);
  let key = &bindings[0];
  let reader = SemanticCatalogReaderV1::new(algorithm, &store);
  for (candidate_root, kind, owner, cancelled) in [
    (vec![0; 32], key.kind, key.owner.clone(), false),
    (vec![1; 31], key.kind, key.owner.clone(), false),
    (root.clone(), 0, key.owner.clone(), false),
    (root.clone(), 8, vec![1; 32], false),
    (root.clone(), 1, b"\x01\x00/a/../b".to_vec(), false),
    (root.clone(), 3, vec![1; 31], false),
    (root.clone(), 6, vec![0; 32], false),
    (root.clone(), key.kind, key.owner.clone(), true),
  ] {
    let error = reader.with_record(&candidate_root, bounds, kind, &owner, &|| cancelled, |_| Ok(())).unwrap_err();
    assert_eq!(error.class(), if cancelled { Class::Cancelled } else { Class::Corrupt });
    assert!(store.reads.borrow().is_empty());
  }
}

#[test]
fn point_lookup_preserves_source_error_identity_at_every_read_boundary() {
  let algorithm = ALGORITHMS[0];
  let (mut store, bindings, root, bounds) = fixture(algorithm, 96);
  let key = &bindings[0];
  SemanticCatalogReaderV1::new(algorithm, &store).with_record(&root, bounds, key.kind, &key.owner, &|| false, |_| Ok(())).unwrap();
  let reads = store.reads.borrow().len();
  for at in 1..=reads {
    for class in [Class::Cancelled, Class::Unavailable, Class::ResourceLimit, Class::Corrupt] {
      store.reads.borrow_mut().clear();
      store.failure = Some((at, class));
      let error = SemanticCatalogReaderV1::new(algorithm, &store)
        .with_record(&root, bounds, key.kind, &key.owner, &|| false, |_| Ok(()))
        .unwrap_err();
      assert_eq!(error.class(), class);
      assert_eq!(error.code(), "test_source");
    }
  }
}

#[test]
fn point_lookup_rejects_missing_ambiguous_corrupt_wrong_kind_and_wrong_identity_nodes() {
  let algorithm = ALGORITHMS[0];
  for mode in 0..5 {
    let (mut store, bindings, root, bounds) = fixture(algorithm, 2);
    let original = store.objects.remove(&(3, root.clone())).unwrap();
    match mode {
      0 => {}
      1 => {
        store.objects.insert((2, root.clone()), original.clone());
        store.objects.insert((3, root.clone()), original);
      }
      2 => {
        let mut corrupt = original;
        corrupt[0] ^= 1;
        store.objects.insert((3, root.clone()), corrupt);
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
    let key = &bindings[0];
    let error =
      SemanticCatalogReaderV1::new(algorithm, &store).with_record(&root, bounds, key.kind, &key.owner, &|| false, |_| Ok(())).unwrap_err();
    assert_eq!(error.class(), Class::Corrupt, "mode {mode}");
    if mode == 4 {
      assert_eq!(error.code(), "semantic_catalog_identity", "wrong identity must not be masked by wrong kind");
    }
  }
}

#[test]
fn point_lookup_rejects_captured_count_mismatch_and_preserves_visitor_failures() {
  let algorithm = ALGORITHMS[0];
  let (store, bindings, root, bounds) = fixture(algorithm, 2);
  let reader = SemanticCatalogReaderV1::new(algorithm, &store);
  let key = &bindings[0];
  for invalid in [SemanticCatalogTraversalBoundsV1::new(3, 3).unwrap(), SemanticCatalogTraversalBoundsV1::new(2, 1).unwrap()] {
    assert_eq!(reader.with_record(&root, invalid, key.kind, &key.owner, &|| false, |_| Ok(())).unwrap_err().class(), Class::Corrupt);
  }
  let error = reader
    .with_record(&root, bounds, key.kind, &key.owner, &|| false, |_| -> Result<(), SemanticCatalogReadErrorV1> {
      Err(SemanticCatalogReadErrorV1::resource("test_visitor", "refused output"))
    })
    .unwrap_err();
  assert_eq!((error.class(), error.code()), (Class::ResourceLimit, "test_visitor"));
}

#[test]
fn point_lookup_cancellation_at_every_check_and_after_visitor_returns_no_result() {
  let algorithm = ALGORITHMS[0];
  let (store, bindings, root, bounds) = fixture(algorithm, 96);
  let reader = SemanticCatalogReaderV1::new(algorithm, &store);
  let key = &bindings[0];
  let checks = Cell::new(0);
  reader
    .with_record(
      &root,
      bounds,
      key.kind,
      &key.owner,
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
      .with_record(
        &root,
        bounds,
        key.kind,
        &key.owner,
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
    .with_record(&root, bounds, key.kind, &key.owner, &|| cancelled.get(), |_| {
      cancelled.set(true);
      Ok(())
    })
    .unwrap_err();
  assert_eq!(error.class(), Class::Cancelled);
}

#[test]
fn mutable_source_walk_stages_unselected_objects_without_changing_the_captured_traversal() {
  for algorithm in ALGORITHMS {
    let (mut store, bindings, root, bounds) = fixture(algorithm, 96);
    let before = store.objects.clone();
    let mut visited = Vec::new();
    let stats = walk_semantic_catalog_with_mutable_source_v1(algorithm, &mut store, &root, bounds, &|| false, |record, source| {
      visited.push((record.record_kind, record.owner_key.to_vec()));
      let staged = oracle(algorithm, &[binding(algorithm, 1000 + visited.len())], 0).0;
      assert!(source.objects.insert((2, staged.object_id), staged.value).is_none());
      Ok(())
    })
    .unwrap();
    let mut expected: Vec<_> = bindings.iter().map(|binding| (binding.kind, binding.owner.clone())).collect();
    expected.sort();
    visited.sort();
    assert_eq!(visited, expected);
    assert_eq!(stats.records, 96);
    assert_eq!(stats.nodes as usize, before.len());
    for (key, value) in before {
      assert_eq!(store.objects[&key], value);
    }
    assert_eq!(store.reads.borrow().len(), stats.nodes as usize * 2);
  }
}

#[test]
fn mutable_source_walk_cancellation_and_visitor_failure_do_not_claim_completion() {
  let algorithm = ALGORITHMS[0];
  for mode in 0..3 {
    let (mut store, _, root, bounds) = fixture(algorithm, 1);
    let cancelled = Cell::new(mode == 0);
    let visits = Cell::new(0);
    let error = walk_semantic_catalog_with_mutable_source_v1(algorithm, &mut store, &root, bounds, &|| cancelled.get(), |_, _| {
      visits.set(visits.get() + 1);
      if mode == 2 {
        return Err(SemanticCatalogReadErrorV1::unavailable("test_visitor", "sink failed"));
      }
      cancelled.set(true);
      Ok(())
    })
    .unwrap_err();
    assert_eq!(error.class(), if mode == 2 { Class::Unavailable } else { Class::Cancelled });
    if mode == 0 {
      assert_eq!(visits.get(), 0);
      assert!(store.reads.borrow().is_empty());
    }
  }
}

#[test]
fn compressed_prefix_and_missing_edge_absence_stop_at_the_root_for_every_hash() {
  for algorithm in ALGORITHMS {
    let mut by_first_byte = BTreeMap::new();
    let pair = (0..1024)
      .find_map(|index| {
        let candidate = binding(algorithm, index);
        let first = lookup(algorithm, &candidate)[0];
        by_first_byte
          .insert(first, candidate.clone())
          .filter(|previous| lookup(algorithm, previous)[1] != lookup(algorithm, &candidate)[1])
          .map(|previous| [previous, candidate])
      })
      .expect("bounded corpus must contain a first-byte collision");
    let first = lookup(algorithm, &pair[0]);
    let second = lookup(algorithm, &pair[1]);
    let branch = (0..algorithm.hash_length()).find(|position| first[*position] != second[*position]).unwrap();
    assert_eq!(branch, 1);
    let mut store = Store::default();
    let (root, nodes) = install_nodes(algorithm, &pair, 0, &mut store);
    let bounds = SemanticCatalogTraversalBoundsV1::new(2, nodes).unwrap();
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    for missing_edge in [false, true] {
      let absent = (2000..100_000)
        .map(|index| binding(algorithm, index))
        .find(|candidate| {
          let hash = lookup(algorithm, candidate);
          if missing_edge {
            hash[..branch] == first[..branch] && hash[branch] != first[branch] && hash[branch] != second[branch]
          } else {
            hash[..branch] != first[..branch]
          }
        })
        .expect("bounded corpus must contain the selected absence branch");
      store.reads.borrow_mut().clear();
      let result = reader
        .with_record(&root, bounds, absent.kind, &absent.owner, &|| false, |_| -> Result<(), SemanticCatalogReadErrorV1> {
          panic!("absent branch visited");
        })
        .unwrap();
      assert_eq!(result, None);
      assert_eq!(store.reads.borrow().len(), 2, "absence read beyond its proving root");
    }
  }
}

#[test]
fn checksum_valid_swapped_children_fail_parent_prefix_closure_in_both_read_paths() {
  for algorithm in ALGORITHMS {
    let (mut store, bindings, root, bounds) = fixture(algorithm, 2);
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
    let forged_root = digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &3u16.to_le_bytes(), &bytes].concat());
    store.objects.insert((3, forged_root.clone()), bytes);
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    let key = &bindings[0];
    let point = reader.with_record(&forged_root, bounds, key.kind, &key.owner, &|| false, |_| Ok(()));
    assert_eq!(point.unwrap_err().code(), "semantic_catalog_leaf_closure");
    let traversal = walk_semantic_catalog_with_mutable_source_v1(algorithm, &mut store, &forged_root, bounds, &|| false, |_, _| Ok(()));
    assert_eq!(traversal.unwrap_err().code(), "semantic_catalog_leaf_closure");
  }
}
