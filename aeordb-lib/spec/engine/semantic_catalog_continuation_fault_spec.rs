//! Every observed publication/read boundary must preserve the old checkpoint.
use super::*;
use super::restart_spec::{checkpoint, native_configuration};
use aeordb::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_progress_v1;

#[derive(Clone, Copy, Debug)]
enum Step {
  Upsert,
  Replace,
  Exclude,
  Prune,
  PruneLast,
  Finish,
}

fn fixture(step: Step, registry: &CompiledParserRegistryV1, memory: &MemoryCoordinator) -> (Store, Vec<u8>, u64) {
  let algorithm = ALGORITHMS[0];
  let mut store = Store::new(algorithm);
  let base = compile_semantic_catalog_v1(
    request(algorithm, 2),
    registry,
    ["/left", "/right"].into_iter().map(|owner| Ok(native_configuration(algorithm, registry, memory, owner))),
    &mut store,
    memory,
    &|| false,
  )
  .unwrap();
  let expected = match step {
    Step::Upsert => 3,
    Step::Replace | Step::Finish => 2,
    Step::Exclude => 1,
    Step::Prune | Step::PruneLast => 0,
  };
  let mut work =
    SemanticCatalogContinuationV1::from_complete(request(algorithm, expected), &base, registry, &store, memory, &|| false).unwrap();
  if matches!(step, Step::Exclude | Step::Prune | Step::PruneLast) {
    work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/left".into()), &mut store).unwrap();
  }
  if matches!(step, Step::Prune | Step::PruneLast) {
    work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/right".into()), &mut store).unwrap();
  }
  if matches!(step, Step::Prune | Step::PruneLast | Step::Finish) {
    work = work.finish_configurations(&mut store).unwrap();
  }
  if matches!(step, Step::PruneLast) {
    for _ in 0..3 {
      work = work.prune_one(&mut store).unwrap();
    }
  }
  let bytes = checkpoint(&work, base.semantic_state(), expected, algorithm);
  (store, bytes, expected)
}

fn execute(
  step: Step,
  work: SemanticCatalogContinuationV1<'_>,
  store: &mut Store,
  configuration: Option<CompiledIndexConfigurationV1>,
) -> Result<(), SemanticCatalogCompilationErrorV1> {
  match step {
    Step::Upsert | Step::Replace => work.apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration.unwrap()), store).map(drop),
    Step::Exclude => work.finish_configurations(store).map(drop),
    Step::Prune | Step::PruneLast => work.prune_one(store).map(drop),
    Step::Finish => work.finish(store).map(drop),
  }
}

fn staged_input(step: Step, registry: &CompiledParserRegistryV1, memory: &MemoryCoordinator) -> Option<CompiledIndexConfigurationV1> {
  match step {
    Step::Upsert => Some(native_configuration(ALGORITHMS[0], registry, memory, "/added")),
    Step::Replace => Some(configuration(ALGORITHMS[0], registry, memory, "/left").unwrap()),
    _ => None,
  }
}

fn copy_store(source: &Store) -> Store {
  let mut result = Store::new(source.algorithm);
  result.objects = source.objects.clone();
  result
}

fn open<'a>(
  bytes: &[u8],
  expected: u64,
  registry: &'a CompiledParserRegistryV1,
  store: &Store,
  memory: &'a MemoryCoordinator,
  cancelled: &'a dyn Fn() -> bool,
) -> SemanticCatalogContinuationV1<'a> {
  let request = request(ALGORITHMS[0], expected);
  let progress = admit_semantic_catalog_progress_v1(request, bytes, registry, store, memory, &|| false).unwrap();
  SemanticCatalogContinuationV1::from_progress(request, progress, registry, store, memory, cancelled).unwrap()
}

fn fault_boundaries(step: Step) {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let (original, bytes, expected) = fixture(step, &registry, &memory);
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut baseline = copy_store(&original);
  let work = open(&bytes, expected, &registry, &baseline, &memory, &|| false);
  baseline.present_reads.set(0);
  baseline.writes = 0;
  execute(step, work, &mut baseline, staged_input(step, &registry, &memory)).unwrap();
  let reads = baseline.present_reads.get();
  let writes = baseline.writes;
  assert!(reads > 0, "every step inspects its immutable inputs");
  if matches!(step, Step::PruneLast) {
    assert_eq!(writes, 0, "last dependency removal reuses the registry leaf and empties candidates without new objects");
  } else {
    assert!(writes > 0, "this fixture must also exercise publication");
  }
  for fault in [ReadFault::Unavailable, ReadFault::Missing, ReadFault::Corrupt, ReadFault::Resource] {
    for at in 1..=reads {
      let mut store = copy_store(&original);
      let work = open(&bytes, expected, &registry, &store, &memory, &|| false);
      store.present_reads.set(0);
      store.writes = 0;
      store.read_fault = Some((at, fault));
      let error = execute(step, work, &mut store, staged_input(step, &registry, &memory)).expect_err("each observed read matters");
      let class = match fault {
        ReadFault::Unavailable => SemanticCatalogReadErrorClassV1::Unavailable,
        ReadFault::Resource => SemanticCatalogReadErrorClassV1::ResourceLimit,
        _ => SemanticCatalogReadErrorClassV1::Corrupt,
      };
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == class), "{step:?}/{fault:?}/{at}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      store.read_fault = None;
      let retry = open(&bytes, expected, &registry, &store, &memory, &|| false);
      execute(step, retry, &mut store, staged_input(step, &registry, &memory)).unwrap();
      for (key, value) in &original.objects {
        assert_eq!(store.objects.get(key), Some(value));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
  }
  for after_durable_write in [false, true] {
    for at in 1..=writes {
      let mut store = copy_store(&original);
      let work = open(&bytes, expected, &registry, &store, &memory, &|| false);
      store.writes = 0;
      store.write_fault = Some((at, after_durable_write));
      let error = execute(step, work, &mut store, staged_input(step, &registry, &memory)).expect_err("each publication must finish");
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Unavailable)
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      store.write_fault = None;
      let retry = open(&bytes, expected, &registry, &store, &memory, &|| false);
      execute(step, retry, &mut store, staged_input(step, &registry, &memory)).unwrap();
      for (key, value) in &original.objects {
        assert_eq!(store.objects.get(key), Some(value));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
  }
}

#[test]
fn catalog_continuation_upsert_refuses_every_read_and_publication_failure() {
  fault_boundaries(Step::Upsert);
}
#[test]
fn catalog_continuation_replace_refuses_every_read_and_publication_failure() {
  fault_boundaries(Step::Replace);
}
#[test]
fn catalog_continuation_live_exclusion_refuses_every_read_and_publication_failure() {
  fault_boundaries(Step::Exclude);
}
#[test]
fn catalog_continuation_single_prune_refuses_every_read_and_publication_failure() {
  fault_boundaries(Step::Prune);
}
#[test]
fn catalog_continuation_last_prune_reuses_existing_roots_and_refuses_every_read_failure() {
  fault_boundaries(Step::PruneLast);
}
#[test]
fn catalog_continuation_finish_refuses_every_read_and_publication_failure() {
  fault_boundaries(Step::Finish);
}

#[test]
fn catalog_continuation_steps_release_at_cancellation_boundaries_and_each_publication_pressure_point() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  for step in [Step::Upsert, Step::Replace, Step::Exclude, Step::Prune, Step::PruneLast, Step::Finish] {
    let (original, bytes, expected) = fixture(step, &registry, &memory);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let calls = Cell::new(0usize);
    let stop = Cell::new(usize::MAX);
    let cancelled = || {
      calls.set(calls.get() + 1);
      calls.get() >= stop.get()
    };
    let mut baseline = copy_store(&original);
    let work = open(&bytes, expected, &registry, &baseline, &memory, &cancelled);
    calls.set(0);
    baseline.writes = 0;
    execute(step, work, &mut baseline, staged_input(step, &registry, &memory)).unwrap();
    let checks = calls.get();
    let writes = baseline.writes;
    assert!(checks > 0);
    // Cover beginning, interior and final checks without quadratic repetition
    // of every inner tree-byte check. Publication pressure remains exhaustive.
    let mut points = vec![1, (checks / 4).max(1), (checks / 2).max(1), (checks * 3 / 4).max(1), checks];
    points.sort_unstable();
    points.dedup();
    for at in points {
      stop.set(usize::MAX);
      let mut store = copy_store(&original);
      let work = open(&bytes, expected, &registry, &store, &memory, &cancelled);
      calls.set(0);
      stop.set(at);
      let error = execute(step, work, &mut store, staged_input(step, &registry, &memory)).expect_err("every cancellation check matters");
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Cancelled)
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    for at in 1..=writes {
      let mut store = copy_store(&original);
      let work = open(&bytes, expected, &registry, &store, &memory, &|| false);
      store.writes = 0;
      store.pressure_after_write = Some((at, memory.clone()));
      let error = execute(step, work, &mut store, staged_input(step, &registry, &memory))
        .expect_err("pressure after publication must refuse the step");
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
      );
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      store.pressure_after_write = None;
      let retry = open(&bytes, expected, &registry, &store, &memory, &|| false);
      execute(step, retry, &mut store, staged_input(step, &registry, &memory)).unwrap();
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
  }
}
