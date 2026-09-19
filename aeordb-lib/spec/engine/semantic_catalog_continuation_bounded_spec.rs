//! Numeric per-step read bounds and memory ownership, separate from admission.
use super::*;
use super::restart_spec::{checkpoint, native_configuration};
use aeordb::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_progress_v1;

#[test]
fn catalog_continuation_fieldless_steps_do_not_rescan_a_growing_base() {
  let algorithm = ALGORITHMS[0];
  let mut reads = Vec::new();
  for count in [32, 128] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let base = compile_semantic_catalog_v1(
      request(algorithm, count),
      &registry,
      (0..count).map(|index| configuration(algorithm, &registry, &memory, &format!("/existing/{index}"))),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let before = store.reads.get();
    let work =
      SemanticCatalogContinuationV1::from_complete(request(algorithm, count + 1), &base, &registry, &store, &memory, &|| false).unwrap();
    let work = work
      .apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration(algorithm, &registry, &memory, "/added").unwrap()), &mut store)
      .unwrap();
    reads.push(store.reads.get() - before);
    let before = store.reads.get();
    let work = work.finish_configurations(&mut store).unwrap();
    assert_eq!(store.reads.get(), before, "no nominated dependencies means no exclusion scan");
    let result = work.finish(&mut store).unwrap();
    assert_eq!(store.reads.get() - before, 1, "finish reads only its new state back");
    assert_eq!(result.configuration_count(), count + 1);
  }
  assert!(reads[1] <= reads[0] * 2 + 32, "fieldless step scanned the growing base: {reads:?}");
}

#[test]
fn catalog_continuation_each_pruning_step_uses_paths_not_a_growing_catalog_scan() {
  let algorithm = ALGORITHMS[0];
  let mut maximum_reads = Vec::new();
  for count in [32, 128] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configurations = (0..count)
      .map(|index| configuration(algorithm, &registry, &memory, &format!("/existing/{index}")))
      .chain(std::iter::once_with(|| Ok(native_configuration(algorithm, &registry, &memory, "/native"))));
    let base =
      compile_semantic_catalog_v1(request(algorithm, count + 1), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let work = SemanticCatalogContinuationV1::from_complete(request(algorithm, count), &base, &registry, &store, &memory, &|| false)
      .unwrap()
      .apply(SemanticCatalogConfigurationMutationV1::Remove("/native".into()), &mut store)
      .unwrap();
    let mut work = work.finish_configurations(&mut store).unwrap();
    let mut maximum = 0;
    for remaining in (0..4).rev() {
      let before = store.reads.get();
      work = work.prune_one(&mut store).unwrap();
      let reads = store.reads.get() - before;
      maximum = maximum.max(reads);
      assert!(reads <= 16 * algorithm.hash_length() + 32, "single-path read envelope exceeded: {reads}");
      assert_eq!(work.pruning_candidates().record_count, remaining);
    }
    maximum_reads.push(maximum);
    let result = work.finish(&mut store).unwrap();
    assert_eq!(result.configuration_count(), count);
  }
  assert!(maximum_reads[1] <= maximum_reads[0] * 2 + 32, "prune step rescanned the growing base: {maximum_reads:?}");
}

#[test]
fn catalog_continuation_consumed_inputs_do_not_accumulate_memory_between_steps() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut work = SemanticCatalogContinuationV1::start(request(algorithm, 64), &registry, &mut store, &memory, &|| false).unwrap();
  let active = memory.snapshot().unwrap().reserved_bytes;
  for index in 0..64 {
    let configuration = configuration(algorithm, &registry, &memory, &format!("/config/{index}")).unwrap();
    assert!(memory.snapshot().unwrap().reserved_bytes > active);
    work = work.apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration), &mut store).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, active);
  }
  let result = work.finish_configurations(&mut store).unwrap().finish(&mut store).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, active);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
}

#[test]
fn catalog_continuation_resume_cannot_transfer_an_old_lease_to_a_new_memory_budget() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let next_memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 32 << 20).unwrap());
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let original =
    compile_semantic_catalog_v1(request(algorithm, 0), &registry, [], &mut store, &memory, &|| false).unwrap().semantic_state().clone();
  let work = SemanticCatalogContinuationV1::start(request(algorithm, 1), &registry, &mut store, &memory, &|| false).unwrap();
  let bytes = checkpoint(&work, &original, 1, algorithm);
  drop(work);
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for old_pressure in [false, true] {
    let admitted = admit_semantic_catalog_progress_v1(request(algorithm, 1), &bytes, &registry, &store, &memory, &|| false).unwrap();
    let pressured = if old_pressure { &memory } else { &next_memory };
    pressured.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..Default::default() }).unwrap();
    let before = (store.reads.get(), store.writes);
    let error = SemanticCatalogContinuationV1::from_progress(request(algorithm, 1), admitted, &registry, &store, &next_memory, &|| false)
      .err()
      .expect("both old and new admission boundaries remain live");
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
    );
    pressured.update_host_sample(HostMemorySample::default()).unwrap();
    assert_eq!((store.reads.get(), store.writes), before);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(next_memory.snapshot().unwrap().reserved_bytes, 0);
  }
  let admitted = admit_semantic_catalog_progress_v1(request(algorithm, 1), &bytes, &registry, &store, &memory, &|| false).unwrap();
  let work =
    SemanticCatalogContinuationV1::from_progress(request(algorithm, 1), admitted, &registry, &store, &next_memory, &|| false).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(next_memory.snapshot().unwrap().reserved_bytes, 32 << 20);
  drop(work);
  assert_eq!(next_memory.snapshot().unwrap().reserved_bytes, 0);
}
