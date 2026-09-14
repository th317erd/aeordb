//! Binding-tree COW proof; definition closure/semantic compilation are owned by
//! the caller. The whole-set oracle below is test-only and encodes its own bytes.
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::namespace::{EncodedSemanticObjectV1, SemanticCatalogRecordV1};
use aeordb::engine::v4::semantic_catalog::{SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1, SemanticCatalogReadErrorV1};
use aeordb::engine::v4::semantic_catalog_mutation::{
  SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1, plan_semantic_catalog_mutation_v1,
};
use sha2::Digest;

const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];
const WORKSPACE: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Binding {
  kind: u16,
  owner: Vec<u8>,
  semantic: Vec<u8>,
  definition: Vec<u8>,
}

impl Binding {
  fn new(algorithm: HashAlgorithm, index: usize) -> Self {
    Self {
      kind: 2,
      owner: [b"\x02\x00".as_slice(), format!("/controls/{index}.json").as_bytes()].concat(),
      semantic: vec![0x11; algorithm.hash_length()],
      definition: vec![0x22; algorithm.hash_length()],
    }
  }

  fn record(&self) -> SemanticCatalogRecordV1<'_> {
    SemanticCatalogRecordV1 {
      record_kind: self.kind,
      owner_key: &self.owner,
      semantic_id: &self.semantic,
      definition_object_id: &self.definition,
    }
  }
}

#[derive(Default)]
struct Objects {
  bytes: BTreeMap<(u16, Vec<u8>), Vec<u8>>,
  reads: RefCell<Vec<(u16, Vec<u8>)>>,
  fail: Cell<bool>,
  cancelled: Cell<bool>,
  cancel_on_read: Cell<bool>,
}

impl SemanticCatalogObjectSourceV1 for Objects {
  fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    assert!(matches!(kind, 2 | 3), "COW may not fetch unrelated definitions");
    self.reads.borrow_mut().push((kind, identity.to_vec()));
    if self.cancel_on_read.get() {
      self.cancelled.set(true);
    }
    if self.fail.get() {
      return Err(SemanticCatalogReadErrorV1::unavailable("injected_source_failure", "test source unavailable"));
    }
    Ok(self.bytes.get(&(kind, identity.to_vec())).cloned())
  }
}

fn digest(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(bytes).to_vec(),
  }
}

fn lookup(algorithm: HashAlgorithm, binding: &Binding) -> Vec<u8> {
  digest(algorithm, &[b"aeordb.semantic-catalog-key.v1\0".as_slice(), &binding.kind.to_le_bytes(), &binding.owner].concat())
}

fn envelope(algorithm: HashAlgorithm, kind: u16, count: u64, body: Vec<u8>) -> EncodedSemanticObjectV1 {
  let mut bytes = vec![0; 32];
  bytes[..4].copy_from_slice(b"ASEM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&kind.to_le_bytes());
  bytes[8..10].copy_from_slice(&32u16.to_le_bytes());
  bytes[12..16].copy_from_slice(&((36 + body.len()) as u32).to_le_bytes());
  bytes[16..20].copy_from_slice(&(body.len() as u32).to_le_bytes());
  bytes[20..28].copy_from_slice(&count.to_le_bytes());
  bytes.extend_from_slice(&body);
  bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
  let object_id = digest(algorithm, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &kind.to_le_bytes(), &bytes].concat());
  EncodedSemanticObjectV1 { object_id, value: bytes }
}

fn oracle(algorithm: HashAlgorithm, bindings: &[Binding], depth: usize) -> (EncodedSemanticObjectV1, u64) {
  assert!(!bindings.is_empty());
  let width = algorithm.hash_length();
  if bindings.len() == 1 {
    let binding = &bindings[0];
    let record_length = 8 + 2 * width + binding.owner.len();
    let mut body = vec![0; 16 + width];
    body[4..8].copy_from_slice(&1u32.to_le_bytes());
    body[8..8 + width].copy_from_slice(&lookup(algorithm, binding));
    body[8 + width..12 + width].copy_from_slice(&(record_length as u32).to_le_bytes());
    body.extend_from_slice(&binding.kind.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&(binding.owner.len() as u32).to_le_bytes());
    body.extend_from_slice(&binding.semantic);
    body.extend_from_slice(&binding.definition);
    body.extend_from_slice(&binding.owner);
    return (envelope(algorithm, 2, 1, body), 1);
  }
  let first = lookup(algorithm, &bindings[0]);
  let branch =
    (depth..width).find(|position| bindings.iter().any(|binding| lookup(algorithm, binding)[*position] != first[*position])).unwrap();
  let mut groups: BTreeMap<u8, Vec<Binding>> = BTreeMap::new();
  for binding in bindings {
    groups.entry(lookup(algorithm, binding)[branch]).or_default().push(binding.clone());
  }
  let mut body = vec![0; 20];
  body[4..6].copy_from_slice(&(depth as u16).to_le_bytes());
  body[6..8].copy_from_slice(&((branch - depth) as u16).to_le_bytes());
  body[8..10].copy_from_slice(&(groups.len() as u16).to_le_bytes());
  body[12..20].copy_from_slice(&(bindings.len() as u64).to_le_bytes());
  body.extend_from_slice(&first[depth..branch]);
  let children = groups.len();
  let mut nodes = 1;
  for (edge, group) in groups {
    let (child, child_nodes) = oracle(algorithm, &group, branch + 1);
    nodes += child_nodes;
    body.extend_from_slice(&[edge, 0, 0, 0]);
    body.extend_from_slice(&(group.len() as u64).to_le_bytes());
    body.extend_from_slice(&child.object_id);
  }
  (envelope(algorithm, 3, children as u64, body), nodes)
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap())
}

#[derive(Default)]
struct Catalog {
  root: Option<Vec<u8>>,
  records: u64,
  nodes: u64,
  objects: Objects,
}

impl Catalog {
  fn snapshot(&self) -> SemanticCatalogSnapshotV1<'_> {
    SemanticCatalogSnapshotV1 { root_object_id: self.root.as_deref(), record_count: self.records, node_count: self.nodes }
  }

  fn apply(&mut self, algorithm: HashAlgorithm, mutation: SemanticCatalogMutationV1<'_>, expected: &[Binding]) -> bool {
    self.objects.reads.borrow_mut().clear();
    let memory = memory();
    let plan = plan_semantic_catalog_mutation_v1(
      SemanticCatalogMutationRequestV1 {
        hash_algorithm: algorithm,
        snapshot: self.snapshot(),
        mutation,
        maximum_workspace_bytes: WORKSPACE,
      },
      &self.objects,
      &memory,
      &|| false,
    )
    .unwrap();
    assert!(memory.snapshot().unwrap().reserved_bytes > 0, "result must retain its output reservation");
    assert!(self.objects.reads.borrow().len() <= 2 * (algorithm.hash_length() + 2));
    assert!(plan.objects().len() <= algorithm.hash_length() + 3);
    assert_eq!(plan.record_count(), expected.len() as u64);
    if expected.is_empty() {
      assert!(plan.root_object_id().is_none());
      assert_eq!(plan.node_count(), 0);
    } else {
      let (expected_root, expected_nodes) = oracle(algorithm, expected, 0);
      assert_eq!(plan.root_object_id(), Some(expected_root.object_id.as_slice()));
      assert_eq!(plan.node_count(), expected_nodes);
      if let Some(root) = plan.objects().iter().find(|object| object.object_id == expected_root.object_id) {
        assert_eq!(root.value, expected_root.value);
      }
    }
    let unchanged = plan.is_unchanged();
    for object in plan.objects() {
      let kind = u16::from_le_bytes(object.value[6..8].try_into().unwrap());
      let previous = self.objects.bytes.insert((kind, object.object_id.clone()), object.value.clone());
      if let Some(previous) = previous {
        assert_eq!(previous, object.value);
      }
    }
    self.root = plan.root_object_id().map(<[u8]>::to_vec);
    self.records = plan.record_count();
    self.nodes = plan.node_count();
    drop(plan);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    unchanged
  }
}

#[test]
fn catalog_mutation_matches_independent_canonical_tree_after_every_insert_and_remove_for_all_hashes() {
  for algorithm in ALGORITHMS {
    let bindings: Vec<_> = (0..40).map(|index| Binding::new(algorithm, index)).collect();
    let mut final_root = None;
    for order in [(0..40).collect::<Vec<_>>(), (0..40).rev().collect(), (0..40).map(|index| (index * 17) % 40).collect()] {
      let mut catalog = Catalog::default();
      let mut current = Vec::new();
      for index in order.iter().copied() {
        current.push(bindings[index].clone());
        assert!(!catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(bindings[index].record()), &current));
      }
      if let Some(previous) = &final_root {
        assert_eq!(&catalog.root, previous);
      } else {
        final_root = Some(catalog.root.clone());
      }
      for index in order.into_iter().rev() {
        current.retain(|binding| binding.owner != bindings[index].owner);
        assert!(!catalog.apply(
          algorithm,
          SemanticCatalogMutationV1::Remove { record_kind: 2, owner_key: &bindings[index].owner },
          &current
        ));
      }
      assert!(catalog.root.is_none());
    }
  }
}

#[test]
fn identical_upsert_and_missing_removal_do_not_create_objects_or_change_root() {
  for algorithm in ALGORITHMS {
    let mut catalog = Catalog::default();
    let binding = Binding::new(algorithm, 0);
    let missing = Binding::new(algorithm, 1);
    assert!(catalog.apply(algorithm, SemanticCatalogMutationV1::Remove { record_kind: 2, owner_key: &missing.owner }, &[]));
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(binding.record()), std::slice::from_ref(&binding));
    let original = catalog.objects.bytes.clone();
    assert!(catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(binding.record()), std::slice::from_ref(&binding)));
    assert!(catalog.apply(
      algorithm,
      SemanticCatalogMutationV1::Remove { record_kind: 2, owner_key: &missing.owner },
      std::slice::from_ref(&binding)
    ));
    assert_eq!(catalog.objects.bytes, original);
  }
}

#[test]
fn replacing_projection_changes_its_leaf_and_ancestors_without_changing_record_or_node_counts() {
  for algorithm in ALGORITHMS {
    let mut catalog = Catalog::default();
    let mut bindings: Vec<_> = (0..12).map(|index| Binding::new(algorithm, index)).collect();
    for index in 0..bindings.len() {
      catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(bindings[index].record()), &bindings[..=index]);
    }
    let previous = catalog.root.clone();
    let nodes = catalog.nodes;
    bindings[4].semantic.fill(0x44);
    bindings[4].definition.fill(0x55);
    assert!(!catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(bindings[4].record()), &bindings));
    assert_ne!(catalog.root, previous);
    assert_eq!(catalog.nodes, nodes);
  }
}

fn shared_prefix_bindings(algorithm: HashAlgorithm) -> (Binding, Binding, Binding) {
  let mut seen = BTreeMap::new();
  for index in 0..=256 {
    let binding = Binding::new(algorithm, index);
    let edge = lookup(algorithm, &binding)[0];
    if let Some(previous) = seen.insert(edge, binding.clone()) {
      let outside =
        (257..1024).map(|index| Binding::new(algorithm, index)).find(|candidate| lookup(algorithm, candidate)[0] != edge).unwrap();
      return (previous, binding, outside);
    }
  }
  panic!("257 keys must share one of256 first bytes");
}

#[test]
fn compressed_internal_prefix_splits_then_collapses_to_the_original_canonical_root() {
  for algorithm in ALGORITHMS {
    let (first, second, outside) = shared_prefix_bindings(algorithm);
    let mut catalog = Catalog::default();
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(first.record()), std::slice::from_ref(&first));
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(second.record()), &[first.clone(), second.clone()]);
    let original = catalog.root.clone();
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(outside.record()), &[first.clone(), second.clone(), outside.clone()]);
    catalog.apply(algorithm, SemanticCatalogMutationV1::Remove { record_kind: 2, owner_key: &outside.owner }, &[first, second]);
    assert_eq!(catalog.root, original);
  }
}

#[test]
fn all_seven_binding_classes_follow_the_same_canonical_mutation_path() {
  for algorithm in ALGORITHMS {
    let mut catalog = Catalog::default();
    let mut bindings = Vec::new();
    for class in 1..=7 {
      let mut binding = Binding::new(algorithm, usize::from(class));
      binding.kind = class;
      if class <= 2 {
        binding.owner[..2].copy_from_slice(&class.to_le_bytes());
      } else {
        binding.owner = vec![class as u8; algorithm.hash_length()];
        binding.semantic = binding.owner.clone();
      }
      bindings.push(binding.clone());
      catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(binding.record()), &bindings);
    }
  }
}

#[test]
fn replacement_reads_only_the_selected_path_even_when_unrelated_branches_are_unavailable() {
  let algorithm = HashAlgorithm::Blake3_256;
  let first = Binding::new(algorithm, 0);
  let edge = lookup(algorithm, &first)[0];
  let second = (1..1024).map(|index| Binding::new(algorithm, index)).find(|binding| lookup(algorithm, binding)[0] != edge).unwrap();
  let mut catalog = Catalog::default();
  catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(first.record()), std::slice::from_ref(&first));
  let selected_leaf = catalog.root.clone().unwrap();
  catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(second.record()), &[first.clone(), second.clone()]);
  let root = catalog.root.clone().unwrap();
  // The initial captured tree is valid. This test source denies reads of
  // unrelated objects; the planner must not attempt whole-tree validation.
  catalog.objects.bytes.retain(|(_, identity), _| identity == &selected_leaf || identity == &root);
  let mut changed = first;
  changed.semantic.fill(0x33);
  changed.definition.fill(0x44);
  catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(changed.record()), &[changed.clone(), second]);
  assert_eq!(catalog.objects.reads.borrow().len(), 4);
  assert!(catalog.objects.reads.borrow().iter().all(|(_, identity)| identity == &root || identity == &selected_leaf));
}

fn request<'a>(snapshot: SemanticCatalogSnapshotV1<'a>, mutation: SemanticCatalogMutationV1<'a>) -> SemanticCatalogMutationRequestV1<'a> {
  SemanticCatalogMutationRequestV1 { hash_algorithm: HashAlgorithm::Blake3_256, snapshot, mutation, maximum_workspace_bytes: WORKSPACE }
}

fn failed_plan(
  request: SemanticCatalogMutationRequestV1<'_>,
  objects: &Objects,
  memory: &MemoryCoordinator,
  cancelled: &dyn Fn() -> bool,
) -> SemanticCatalogReadErrorV1 {
  let before = memory.snapshot().unwrap().reserved_bytes;
  let error = plan_semantic_catalog_mutation_v1(request, objects, memory, cancelled).err().expect("invalid request must fail");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before, "failed plan leaked a reservation");
  error
}

#[test]
fn malformed_snapshot_presence_width_zero_identity_and_counts_fail_without_io() {
  let objects = Objects::default();
  let memory = memory();
  let binding = Binding::new(HashAlgorithm::Blake3_256, 0);
  let bad_snapshots = [
    (None, 1, 0),
    (None, 0, 1),
    (Some(vec![1; 31]), 1, 1),
    (Some(vec![0; 32]), 1, 1),
    (Some(vec![1; 32]), 0, 1),
    (Some(vec![1; 32]), 1, 0),
    (Some(vec![1; 32]), 1, u64::MAX),
  ];
  for (root, records, nodes) in bad_snapshots {
    let snapshot = SemanticCatalogSnapshotV1 { root_object_id: root.as_deref(), record_count: records, node_count: nodes };
    assert_eq!(
      failed_plan(request(snapshot, SemanticCatalogMutationV1::Upsert(binding.record())), &objects, &memory, &|| false).class(),
      SemanticCatalogReadErrorClassV1::Corrupt
    );
  }
  assert!(objects.reads.borrow().is_empty());
}

#[test]
fn malformed_binding_and_removal_keys_fail_before_io() {
  let catalog = Catalog::default();
  let memory = memory();
  let valid = Binding::new(HashAlgorithm::Blake3_256, 0);
  for kind in [0, 8, u16::MAX] {
    let mut binding = valid.clone();
    binding.kind = kind;
    assert_eq!(
      failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
        .class(),
      SemanticCatalogReadErrorClassV1::Corrupt
    );
  }
  for owner in [vec![], b"\x02\x00relative".to_vec(), b"\x02\x00/a/../b".to_vec(), vec![1; 65538]] {
    let mut binding = valid.clone();
    binding.owner = owner;
    assert!(failed_plan(
      request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())),
      &catalog.objects,
      &memory,
      &|| false
    )
    .code()
    .contains("owner"));
    assert!(failed_plan(
      request(catalog.snapshot(), SemanticCatalogMutationV1::Remove { record_kind: 2, owner_key: &binding.owner }),
      &catalog.objects,
      &memory,
      &|| false
    )
    .code()
    .contains("owner"));
  }
  for class in 3..=7 {
    let binding = Binding { kind: class, owner: vec![1; 32], semantic: vec![2; 32], definition: vec![3; 32] };
    assert_eq!(
      failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
        .class(),
      SemanticCatalogReadErrorClassV1::Corrupt
    );
  }
  for semantic in [vec![], vec![0; 32], vec![1; 31], vec![1; 64]] {
    let mut binding = valid.clone();
    binding.semantic = semantic;
    assert_eq!(
      failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
        .class(),
      SemanticCatalogReadErrorClassV1::Corrupt
    );
  }
  assert!(catalog.objects.reads.borrow().is_empty());
}

#[test]
fn workspace_limit_and_shared_memory_contention_fail_before_source_access_without_leaking() {
  let catalog = Catalog::default();
  let binding = Binding::new(HashAlgorithm::Blake3_256, 0);
  let memory = memory();
  let mut operation = request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record()));
  for limit in [0, 1, 65536] {
    operation.maximum_workspace_bytes = limit;
    assert_eq!(failed_plan(operation, &catalog.objects, &memory, &|| false).class(), SemanticCatalogReadErrorClassV1::ResourceLimit);
  }
  operation.maximum_workspace_bytes = WORKSPACE;
  let busy = memory.reserve(MemoryOwner::Task, 119 * 1024 * 1024, AdmissionClass::Workload).unwrap();
  assert_eq!(failed_plan(operation, &catalog.objects, &memory, &|| false).class(), SemanticCatalogReadErrorClassV1::ResourceLimit);
  drop(busy);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert!(catalog.objects.reads.borrow().is_empty());
}

#[test]
fn cancellation_before_and_during_reads_returns_cancelled_without_output_or_leaked_memory() {
  let algorithm = HashAlgorithm::Blake3_256;
  let binding = Binding::new(algorithm, 0);
  let mut catalog = Catalog::default();
  catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(binding.record()), std::slice::from_ref(&binding));
  let memory = memory();
  let operation = request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record()));
  assert_eq!(failed_plan(operation, &catalog.objects, &memory, &|| true).class(), SemanticCatalogReadErrorClassV1::Cancelled);
  assert!(catalog.objects.reads.borrow().is_empty());
  catalog.objects.cancel_on_read.set(true);
  assert_eq!(
    failed_plan(operation, &catalog.objects, &memory, &|| catalog.objects.cancelled.get()).class(),
    SemanticCatalogReadErrorClassV1::Cancelled
  );
  assert_eq!(catalog.objects.reads.borrow().len(), 1);
}

#[test]
fn missing_and_ambiguous_nodes_and_source_failures_never_become_empty_catalogs() {
  let algorithm = HashAlgorithm::Blake3_256;
  let binding = Binding::new(algorithm, 0);
  let mut catalog = Catalog::default();
  catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(binding.record()), std::slice::from_ref(&binding));
  let memory = memory();
  catalog.objects.fail.set(true);
  assert_eq!(
    failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
      .code(),
    "injected_source_failure"
  );
  catalog.objects.fail.set(false);
  let identity = catalog.root.clone().unwrap();
  let leaf = catalog.objects.bytes.get(&(2, identity.clone())).unwrap().clone();
  catalog.objects.bytes.insert((3, identity.clone()), leaf);
  assert_eq!(
    failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
      .class(),
    SemanticCatalogReadErrorClassV1::Corrupt
  );
  catalog.objects.bytes.clear();
  assert_eq!(
    failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
      .class(),
    SemanticCatalogReadErrorClassV1::Corrupt
  );
}

#[test]
fn damaged_truncated_oversized_wrong_kind_and_wrong_identity_node_bytes_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let binding = Binding::new(algorithm, 0);
  let original = oracle(algorithm, std::slice::from_ref(&binding), 0).0;
  let mut corrupt = original.value.clone();
  corrupt[32] ^= 1;
  let other = oracle(algorithm, &[Binding::new(algorithm, 1)], 0).0;
  for bytes in [corrupt, original.value[..original.value.len() - 1].to_vec(), vec![0; 1048577], other.value] {
    let mut objects = Objects::default();
    objects.bytes.insert((2, original.object_id.clone()), bytes);
    let snapshot = SemanticCatalogSnapshotV1 { root_object_id: Some(&original.object_id), record_count: 1, node_count: 1 };
    assert_eq!(
      failed_plan(request(snapshot, SemanticCatalogMutationV1::Upsert(binding.record())), &objects, &memory(), &|| false).class(),
      SemanticCatalogReadErrorClassV1::Corrupt
    );
  }
  let mut wrong_kind = Objects::default();
  wrong_kind.bytes.insert((3, original.object_id.clone()), original.value);
  let snapshot = SemanticCatalogSnapshotV1 { root_object_id: Some(&original.object_id), record_count: 1, node_count: 1 };
  assert_eq!(
    failed_plan(request(snapshot, SemanticCatalogMutationV1::Upsert(binding.record())), &wrong_kind, &memory(), &|| false).class(),
    SemanticCatalogReadErrorClassV1::Corrupt
  );
}

fn replace_root_bytes(catalog: &mut Catalog, mut bytes: Vec<u8>) {
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
  let identity = digest(HashAlgorithm::Blake3_256, &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &bytes[6..8], &bytes].concat());
  let kind = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
  catalog.objects.bytes.insert((kind, identity.clone()), bytes);
  catalog.root = Some(identity);
}

#[test]
fn valid_checksums_cannot_hide_root_depth_or_child_prefix_and_record_count_mismatches() {
  let algorithm = HashAlgorithm::Blake3_256;
  let first = Binding::new(algorithm, 0);
  let edge = lookup(algorithm, &first)[0];
  let second = (1..1024).map(|index| Binding::new(algorithm, index)).find(|binding| lookup(algorithm, binding)[0] != edge).unwrap();
  for damage in 0..4 {
    let mut catalog = Catalog::default();
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(first.record()), std::slice::from_ref(&first));
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(second.record()), &[first.clone(), second.clone()]);
    let mut bytes = catalog.objects.bytes.get(&(3, catalog.root.clone().unwrap())).unwrap().clone();
    let child = if bytes[52] == edge { 52 } else { 96 };
    match damage {
      0 => bytes[36..38].copy_from_slice(&1u16.to_le_bytes()),
      1 => {
        let other = oracle(algorithm, std::slice::from_ref(&second), 0).0;
        bytes[child + 12..child + 44].copy_from_slice(&other.object_id);
      }
      2 => {
        bytes[child + 4..child + 12].copy_from_slice(&2u64.to_le_bytes());
        bytes[44..52].copy_from_slice(&3u64.to_le_bytes());
        catalog.records = 3;
      }
      3 => catalog.records = 3,
      _ => unreachable!(),
    }
    replace_root_bytes(&mut catalog, bytes);
    assert_eq!(
      failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(first.record())), &catalog.objects, &memory(), &|| false)
        .class(),
      SemanticCatalogReadErrorClassV1::Corrupt,
      "damage {damage}"
    );
  }
}

#[test]
fn new_catalog_nodes_are_ordered_after_every_referenced_child() {
  for algorithm in ALGORITHMS {
    let (first, second, outside) = shared_prefix_bindings(algorithm);
    let mut catalog = Catalog::default();
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(first.record()), std::slice::from_ref(&first));
    catalog.apply(algorithm, SemanticCatalogMutationV1::Upsert(second.record()), &[first.clone(), second.clone()]);
    let memory = memory();
    let plan = plan_semantic_catalog_mutation_v1(
      SemanticCatalogMutationRequestV1 {
        hash_algorithm: algorithm,
        snapshot: catalog.snapshot(),
        mutation: SemanticCatalogMutationV1::Upsert(outside.record()),
        maximum_workspace_bytes: WORKSPACE,
      },
      &catalog.objects,
      &memory,
      &|| false,
    )
    .unwrap();
    let mut available: std::collections::BTreeSet<_> = catalog.objects.bytes.keys().map(|(_, identity)| identity.clone()).collect();
    for object in plan.objects() {
      let kind = u16::from_le_bytes(object.value[6..8].try_into().unwrap());
      if kind == 3 {
        let prefix_length = u16::from_le_bytes(object.value[38..40].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(object.value[40..42].try_into().unwrap()) as usize;
        for index in 0..count {
          let offset = 52 + prefix_length + index * (12 + algorithm.hash_length());
          assert!(
            available.contains(&object.value[offset + 12..offset + 12 + algorithm.hash_length()]),
            "parent precedes an unpublished child"
          );
        }
      }
      available.insert(object.object_id.clone());
    }
    assert_eq!(plan.source_root_object_id(), catalog.root.as_deref());
    assert!(available.contains(plan.root_object_id().unwrap()));
  }
}

#[test]
fn no_policy_and_mid_operation_pressure_do_not_bypass_memory_admission() {
  use aeordb::engine::memory_coordinator::HostMemorySample;
  let memory = MemoryCoordinator::without_policy();
  let catalog = Catalog::default();
  let binding = Binding::new(HashAlgorithm::Blake3_256, 0);
  assert_eq!(
    failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &|| false)
      .class(),
    SemanticCatalogReadErrorClassV1::ResourceLimit
  );
  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap());
  let mut catalog = Catalog::default();
  catalog.apply(HashAlgorithm::Blake3_256, SemanticCatalogMutationV1::Upsert(binding.record()), std::slice::from_ref(&binding));
  let before = catalog.objects.bytes.clone();
  let calls = Cell::new(0);
  let cancelled = || {
    calls.set(calls.get() + 1);
    if calls.get() == 2 {
      memory
        .update_host_sample(HostMemorySample { rss_bytes: 200 * 1024 * 1024, host_available_bytes: Some(1024), ..Default::default() })
        .unwrap();
    }
    false
  };
  assert_eq!(
    failed_plan(request(catalog.snapshot(), SemanticCatalogMutationV1::Upsert(binding.record())), &catalog.objects, &memory, &cancelled)
      .class(),
    SemanticCatalogReadErrorClassV1::ResourceLimit
  );
  assert_eq!(catalog.objects.bytes, before);
}

#[test]
fn deleting_a_zero_semantic_definition_owner_is_not_accepted_as_a_missing_key() {
  let catalog = Catalog::default();
  let memory = memory();
  for class in 3..=7 {
    let operation = request(catalog.snapshot(), SemanticCatalogMutationV1::Remove { record_kind: class, owner_key: &[0; 32] });
    assert_eq!(failed_plan(operation, &catalog.objects, &memory, &|| false).class(), SemanticCatalogReadErrorClassV1::Corrupt);
  }
  assert!(catalog.objects.reads.borrow().is_empty());
}
