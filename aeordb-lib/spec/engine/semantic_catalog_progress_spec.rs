use super::*;

#[path = "semantic_catalog_progress_boundary_spec.rs"]
mod boundary;
use aeordb::engine::v4::semantic_catalog_compiler::{AdmittedSemanticCatalogProgressV1, admit_semantic_catalog_progress_v1};
use aeordb::engine::v4::semantic_mutation_control::{SemanticMutationPhaseV1, decode_semantic_mutation_checkpoint};

struct CheckpointFixture {
  bytes: Vec<u8>,
  root: Vec<u8>,
  records: u64,
  nodes: u64,
  pruning_root: Option<Vec<u8>>,
  pruning_records: u64,
  pruning_nodes: u64,
}

fn checkpoint(
  store: &mut Store,
  baseline: &EncodedSemanticObjectV1,
  retained: &[catalog_oracle::Binding],
  candidates: &[catalog_oracle::Binding],
  phase: u16,
  current: u64,
  expected: u64,
) -> CheckpointFixture {
  let algorithm = store.algorithm;
  let width = algorithm.hash_length();
  let (root, nodes) = install_oracle(store, retained, 0);
  let pruning = if candidates.is_empty() { None } else { Some(install_oracle(store, candidates, 0)) };
  let state = decode_semantic_object(&baseline.value, algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { compiler_fingerprint, semantic_registry_fingerprint, .. } = state.availability else {
    panic!("fixture base must be complete");
  };
  // Independent ASMC envelope/body, not the production checkpoint encoder.
  // All fields use the ratified offsets; catalog nodes use the separate oracle.
  let mut bytes = vec![0; 32 + 168 + 9 * width + 4];
  let total = bytes.len();
  bytes[..4].copy_from_slice(b"ASMC");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&32u16.to_le_bytes());
  bytes[8..12].copy_from_slice(&(total as u32).to_le_bytes());
  bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
  bytes[24..28].copy_from_slice(&((total - 36) as u32).to_le_bytes());
  let body = &mut bytes[32..total - 4];
  body[..16].fill(1);
  body[16..32].fill(2);
  body[40..56].fill(3);
  for offset in [32, 56, 64, 72, 136] {
    body[offset..offset + 8].copy_from_slice(&1u64.to_le_bytes());
  }
  body[80..88].copy_from_slice(&100u64.to_le_bytes());
  body[88..90].copy_from_slice(&phase.to_le_bytes());
  let dependencies = retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).count() as u64;
  let pruning_nodes = pruning.as_ref().map_or(0, |(_, nodes)| *nodes);
  for (offset, value) in [
    (96, expected),
    (104, current),
    (112, retained.len() as u64),
    (120, nodes),
    (128, dependencies),
    (152, candidates.len() as u64),
    (160, pruning_nodes),
  ] {
    body[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
  }
  for slot in [0, 1, 8] {
    body[168 + slot * width..168 + (slot + 1) * width].fill(slot as u8 + 1);
  }
  for (slot, hash) in [(2, root.as_slice()), (6, compiler_fingerprint.as_slice()), (7, semantic_registry_fingerprint.as_slice())] {
    body[168 + slot * width..168 + (slot + 1) * width].copy_from_slice(hash);
  }
  if let Some((root, _)) = &pruning {
    body[168 + 3 * width..168 + 4 * width].copy_from_slice(root);
  }
  let crc = crc32fast::hash(&bytes[..total - 4]);
  bytes[total - 4..].copy_from_slice(&crc.to_le_bytes());
  decode_semantic_mutation_checkpoint(&bytes, algorithm).expect("independent checkpoint must be a valid input");
  CheckpointFixture {
    bytes,
    root,
    records: retained.len() as u64,
    nodes,
    pruning_root: pruning.map(|(root, _)| root),
    pruning_records: candidates.len() as u64,
    pruning_nodes,
  }
}

fn assert_snapshot(progress: &AdmittedSemanticCatalogProgressV1, fixture: &CheckpointFixture) {
  let catalog = progress.catalog();
  assert_eq!(catalog.root_object_id, Some(fixture.root.as_slice()));
  assert_eq!((catalog.record_count, catalog.node_count), (fixture.records, fixture.nodes));
  let pruning = progress.pruning_candidates();
  assert_eq!(pruning.root_object_id, fixture.pruning_root.as_deref());
  assert_eq!((pruning.record_count, pruning.node_count), (fixture.pruning_records, fixture.pruning_nodes));
}

#[test]
fn partial_catalog_admission_accepts_reachable_compiling_progress_without_final_state() {
  for algorithm in ALGORITHMS {
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
    let fixture = checkpoint(&mut store, &original, &retained, &[], 2, 1, 3);
    store.objects.remove(&(1, original.object_id.clone()));
    assert!(!store.objects.keys().any(|(kind, _)| *kind == 1));
    let before = store.objects.clone();
    let writes = store.writes;
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let progress = admit_semantic_catalog_progress_v1(request(algorithm, 3), &fixture.bytes, &registry, &store, &memory, &|| false)
      .expect("a valid partial catalog needs a distinct admission proof, not a completed state");
    assert_snapshot(&progress, &fixture);
    assert_eq!(progress.hash_algorithm(), algorithm);
    assert_eq!(progress.phase(), SemanticMutationPhaseV1::Compiling);
    assert_eq!(progress.configuration_count(), 1);
    assert_eq!(progress.dependency_count(), retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).count() as u64);
    assert!(memory.snapshot().unwrap().reserved_bytes > reserved);
    assert_eq!(store.objects, before);
    assert_eq!(store.writes, writes);
    drop(progress);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
}

#[test]
fn partial_catalog_admission_accepts_exact_orphan_dependencies_in_compiling_and_pruning() {
  for algorithm in ALGORITHMS {
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
    let retained: Vec<_> = bindings(&store, &original).into_iter().filter(|binding| matches!(binding.kind, 2 | 6 | 7)).collect();
    let candidates: Vec<_> = retained.iter().filter(|binding| matches!(binding.kind, 6 | 7)).cloned().collect();
    assert!(!candidates.is_empty());
    // Complete admission must remain strict for these same dependency orphans.
    let incomplete = replace_bindings(&mut store, &original, &retained);
    let error = failure(admit_semantic_catalog_v1(request(algorithm, 0), &incomplete.object_id, &registry, &store, &memory, &|| false));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_orphan"));
    for (phase, expected, typed_phase) in [(2, 1, SemanticMutationPhaseV1::Compiling), (3, 0, SemanticMutationPhaseV1::Pruning)] {
      let fixture = checkpoint(&mut store, &original, &retained, &candidates, phase, 0, expected);
      let before = store.objects.clone();
      let writes = store.writes;
      let reserved = memory.snapshot().unwrap().reserved_bytes;
      let progress =
        admit_semantic_catalog_progress_v1(request(algorithm, expected), &fixture.bytes, &registry, &store, &memory, &|| false)
          .expect("exact candidate dependencies are valid unfinished compiler work");
      assert_snapshot(&progress, &fixture);
      assert_eq!(progress.phase(), typed_phase);
      assert_eq!(progress.configuration_count(), 0);
      assert_eq!(progress.dependency_count(), candidates.len() as u64);
      assert_eq!(store.objects, before);
      assert_eq!(store.writes, writes);
      drop(progress);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    }
  }
}
