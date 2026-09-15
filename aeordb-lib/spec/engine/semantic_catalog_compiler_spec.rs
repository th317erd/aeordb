//! Streaming composition of complete configurations, never namespace activation.
#[path = "../helpers/semantic_catalog_oracle.rs"]
mod catalog_oracle;

use std::cell::Cell;
use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::{DependencyRecordV1, decode_dependency_record_bytes};
use aeordb::engine::v4::index_configuration_compiler::{
  CompiledIndexConfigurationV1, IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use aeordb::engine::v4::namespace::{EncodedSemanticObjectV1, SemanticAvailabilityV1, decode_semantic_object};
use aeordb::engine::v4::parser_registry_compiler::{
  CompiledParserRegistryV1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
  compile_parser_registry_v1,
};
use aeordb::engine::v4::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1,
  SemanticCatalogTraversalBoundsV1,
};
use aeordb::engine::v4::semantic_catalog_compiler::{
  CompiledSemanticCatalogV1, SemanticCatalogCompilationErrorV1, SemanticCatalogCompilationRequestV1, SemanticCatalogStagingStoreV1,
  compile_semantic_catalog_v1,
};
use aeordb::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
use aeordb::engine::v4::system_family::embedded_system_family_registry;

const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];
const EMPTY: &[u8] = br#"{"$v":1,"indexes":[]}"#;

struct Snapshot;
impl ParserAliasSnapshotV1 for Snapshot {
  fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}
impl IndexConfigurationAliasSnapshotV1 for Snapshot {
  fn resolve_mapper_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(None)
  }
}

struct Store {
  algorithm: HashAlgorithm,
  objects: BTreeMap<(u16, Vec<u8>), Vec<u8>>,
  reads: Cell<usize>,
  writes: usize,
  fail_write: bool,
  present_reads: Cell<usize>,
  read_fault: Option<(usize, ReadFault)>,
  write_fault: Option<(usize, bool)>,
  pressure_after_write: Option<(usize, MemoryCoordinator)>,
}

#[derive(Clone, Copy, Debug)]
enum ReadFault {
  Unavailable,
  Missing,
  Corrupt,
  Resource,
}
impl Store {
  fn new(algorithm: HashAlgorithm) -> Self {
    Self {
      algorithm,
      objects: BTreeMap::new(),
      reads: Cell::new(0),
      writes: 0,
      fail_write: false,
      present_reads: Cell::new(0),
      read_fault: None,
      write_fault: None,
      pressure_after_write: None,
    }
  }
}
impl SemanticCatalogObjectSourceV1 for Store {
  fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    self.reads.set(self.reads.get() + 1);
    let mut bytes = self.objects.get(&(kind, identity.to_vec())).cloned();
    if let Some(value) = &mut bytes {
      let count = self.present_reads.get() + 1;
      self.present_reads.set(count);
      if let Some((at, fault)) = self.read_fault.filter(|(at, _)| *at == count) {
        assert_eq!(at, count);
        match fault {
          ReadFault::Unavailable => return Err(SemanticCatalogReadErrorV1::unavailable("test_staging_read", "read failure")),
          ReadFault::Missing => return Ok(None),
          ReadFault::Corrupt => value[0] ^= 1,
          ReadFault::Resource => return Err(SemanticCatalogReadErrorV1::resource("test_staging_read", "read allocation refused")),
        }
      }
    }
    Ok(bytes)
  }
}
impl SemanticCatalogStagingStoreV1 for Store {
  fn publish_semantic_objects(&mut self, objects: &[EncodedSemanticObjectV1]) -> Result<(), SemanticCatalogReadErrorV1> {
    self.writes += 1;
    if self.fail_write || self.write_fault == Some((self.writes, false)) {
      return Err(SemanticCatalogReadErrorV1::unavailable("test_staging_write", "injected write failure"));
    }
    for object in objects {
      let decoded = decode_semantic_object(&object.value, self.algorithm).unwrap();
      assert_eq!(decoded.object_id, object.object_id);
      let old = self.objects.insert((decoded.kind_id, object.object_id.clone()), object.value.clone());
      if let Some(old) = old {
        assert_eq!(old, object.value, "immutable identity was overwritten");
      }
    }
    if self.write_fault == Some((self.writes, true)) {
      return Err(SemanticCatalogReadErrorV1::unavailable("test_staging_write", "failure after durable write"));
    }
    if let Some((at, memory)) = &self.pressure_after_write {
      if *at == self.writes {
        memory.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..Default::default() }).unwrap();
      }
    }
    Ok(())
  }
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 32 << 20).unwrap())
}
fn registry(algorithm: HashAlgorithm, memory: &MemoryCoordinator) -> CompiledParserRegistryV1 {
  compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &Snapshot,
    memory,
    &|| false,
  )
  .unwrap()
}
fn configuration(
  algorithm: HashAlgorithm,
  registry: &CompiledParserRegistryV1,
  memory: &MemoryCoordinator,
  owner_path: &str,
) -> Result<CompiledIndexConfigurationV1, SemanticCompilationErrorV1> {
  compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: EMPTY,
      owner_path,
      registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &Snapshot,
    memory,
    &|| false,
  )
}
fn request(algorithm: HashAlgorithm, count: u64) -> SemanticCatalogCompilationRequestV1 {
  SemanticCatalogCompilationRequestV1 {
    hash_algorithm: algorithm,
    expected_configuration_count: count,
    required_capabilities: [0; 32],
    maximum_workspace_bytes: 64 << 20,
  }
}
fn failure(result: Result<CompiledSemanticCatalogV1, SemanticCatalogCompilationErrorV1>) -> SemanticCatalogCompilationErrorV1 {
  match result {
    Err(error) => error,
    Ok(_) => panic!("incomplete staging returned a complete candidate"),
  }
}

#[test]
fn complete_fieldless_catalogs_bind_current_profiles_and_exact_counts_for_every_hash() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configurations = ["/", "/nested"].into_iter().map(|path| configuration(algorithm, &registry, &memory, path));
    let result = compile_semantic_catalog_v1(request(algorithm, 2), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete {
      compiler_fingerprint,
      semantic_registry_fingerprint,
      catalog_root,
      catalog_record_count,
      catalog_node_count,
      definition_count,
      dependency_count,
    } = state.availability
    else {
      panic!("fresh compilation must not be content-only")
    };
    assert_eq!(compiler_fingerprint, semantic_compiler_fingerprint_v1(algorithm));
    assert_eq!(semantic_registry_fingerprint, embedded_system_family_registry(algorithm).unwrap().semantic_projection_fingerprint);
    assert_eq!((catalog_record_count, definition_count, dependency_count), (5, 5, 0));
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    let mut bindings = Vec::new();
    let stats = reader
      .walk_catalog(
        &catalog_root,
        SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap(),
        &|| false,
        |record| {
          reader.with_definition(record, &|| false, |_| Ok(()))?;
          bindings.push(catalog_oracle::Binding {
            kind: record.record_kind,
            owner: record.owner_key.to_vec(),
            semantic: record.semantic_id.to_vec(),
            definition: record.definition_object_id.to_vec(),
          });
          Ok(())
        },
      )
      .unwrap();
    assert_eq!(stats.class_counts, [0, 2, 1, 2, 0, 0, 0, 0]);
    let (independent, independent_nodes) = catalog_oracle::oracle(algorithm, &bindings, 0);
    assert_eq!(catalog_root, independent.object_id);
    assert_eq!(catalog_node_count, independent_nodes);
    assert_eq!(store.objects[&(3, catalog_root)], independent.value);
    assert_eq!(result.configuration_count(), 2);
    drop(result);
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn counted_stream_releases_each_configuration_before_requesting_the_next() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let before_next = Cell::new(None);
  let configurations = (0..64).map(|index| {
    let retained = memory.snapshot().unwrap().reserved_bytes;
    match before_next.get() {
      None => before_next.set(Some(retained)),
      Some(expected) => assert_eq!(retained, expected, "previous configuration or path remained resident"),
    }
    configuration(algorithm, &registry, &memory, &format!("/scope/{index}"))
  });
  let result = compile_semantic_catalog_v1(request(algorithm, 64), &registry, configurations, &mut store, &memory, &|| false).unwrap();
  assert_eq!(result.configuration_count(), 64);
  drop(result);
  drop(registry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn source_order_does_not_change_the_catalog_or_complete_semantic_state() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut outputs = Vec::new();
    for owners in [["/", "/a", "/a/nested"], ["/a/nested", "/", "/a"]] {
      let mut store = Store::new(algorithm);
      let configurations = owners.into_iter().map(|path| configuration(algorithm, &registry, &memory, path));
      let result = compile_semantic_catalog_v1(request(algorithm, 3), &registry, configurations, &mut store, &memory, &|| false).unwrap();
      outputs.push(result.semantic_state().clone());
    }
    assert_eq!(outputs[0], outputs[1]);
  }
}

#[test]
fn duplicate_configuration_owners_do_not_leave_duplicate_or_stale_bindings() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let mut store = Store::new(algorithm);
  let configurations = ["/a", "/a"].into_iter().map(|path| configuration(algorithm, &registry, &memory, path));
  let error = failure(compile_semantic_catalog_v1(request(algorithm, 2), &registry, configurations, &mut store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_duplicate_configuration", .. }));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
}

#[test]
fn incomplete_or_excess_configuration_streams_never_return_a_complete_candidate() {
  let algorithm = ALGORITHMS[0];
  for expected in [0, 2] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let mut store = Store::new(algorithm);
    let configurations = std::iter::once_with(|| configuration(algorithm, &registry, &memory, "/"));
    assert!(matches!(
      failure(compile_semantic_catalog_v1(request(algorithm, expected), &registry, configurations, &mut store, &memory, &|| false)),
      SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_configuration_count", .. }
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
}

#[test]
fn configuration_source_failure_keeps_its_original_typed_error() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let mut store = Store::new(algorithm);
  let configurations = std::iter::once(Err(SemanticCompilationErrorV1::Operational { path: "test", message: "source read failed".into() }));
  assert!(matches!(
    failure(compile_semantic_catalog_v1(request(algorithm, 1), &registry, configurations, &mut store, &memory, &|| false)),
    SemanticCatalogCompilationErrorV1::Input(SemanticCompilationErrorV1::Operational { path: "test", .. })
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
}

#[test]
fn cancelled_or_unadmitted_compilation_does_not_enumerate_sources_or_touch_storage() {
  let algorithm = ALGORITHMS[0];
  for cancelled in [true, false] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let mut store = Store::new(algorithm);
    let configurations = std::iter::from_fn(|| -> Option<Result<CompiledIndexConfigurationV1, SemanticCompilationErrorV1>> {
      panic!("source access before admission")
    });
    let mut request = request(algorithm, 1);
    if !cancelled {
      request.maximum_workspace_bytes = 1;
    }
    let error = failure(compile_semantic_catalog_v1(request, &registry, configurations, &mut store, &memory, &|| cancelled));
    let expected = if cancelled { SemanticCatalogReadErrorClassV1::Cancelled } else { SemanticCatalogReadErrorClassV1::ResourceLimit };
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == expected));
    assert_eq!((store.reads.get(), store.writes), (0, 0));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
}

#[test]
fn failed_object_publication_remains_unavailable_and_releases_the_compiler_lease() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let mut store = Store::new(algorithm);
  store.fail_write = true;
  let error = failure(compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false));
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::Unavailable && source.code() == "test_staging_write")
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  assert!(store.objects.is_empty());
}

struct AvailableSnapshot;
fn dependency(role: u16) -> DependencyRecordV1<'static> {
  DependencyRecordV1 {
    kind: 1,
    role,
    flags: 4,
    abi: role + 2,
    executor_profile: 2,
    fingerprint_semantics: 1,
    artifact_kind: 1,
    artifact_length: 123,
    fingerprint: [0x42; 32],
    dependency_id: "/org/example/shared",
    version: "1.2.3",
  }
}
impl ParserAliasSnapshotV1 for AvailableSnapshot {
  fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(Some(dependency(1)))
  }
}
impl IndexConfigurationAliasSnapshotV1 for AvailableSnapshot {
  fn resolve_mapper_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(Some(dependency(2)))
  }
}

#[test]
fn shared_dependencies_deduplicate_across_scopes_without_conflating_same_artifact_roles() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let mapper: &[u8] = br#"{"$v":1,"parser":"p","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#;
    let native: &[u8] = br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#;
    let configurations = [("/", mapper), ("/nested", mapper), ("/native", native)].into_iter().map(|(owner_path, source)| {
      compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source,
          owner_path,
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 128 << 20,
        },
        &AvailableSnapshot,
        &memory,
        &|| false,
      )
    });
    let result = compile_semantic_catalog_v1(request(algorithm, 3), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete {
      catalog_root, catalog_record_count, catalog_node_count, definition_count, dependency_count, ..
    } = state.availability
    else {
      panic!("complete state")
    };
    assert_eq!((catalog_record_count, definition_count, dependency_count), (19, 19, 6));
    let reader = SemanticCatalogReaderV1::new(algorithm, &store);
    let mut roles = Vec::new();
    let stats = reader
      .walk_catalog(
        &catalog_root,
        SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap(),
        &|| false,
        |record| {
          reader.with_definition(record, &|| false, |bytes| {
            if record.record_kind == 6 {
              let dependency = decode_dependency_record_bytes(bytes).unwrap();
              assert_eq!(dependency.fingerprint, [0x42; 32]);
              assert_eq!(record.owner_key.len(), algorithm.hash_length());
              assert_eq!(record.owner_key, record.semantic_id);
              roles.push((dependency.role, record.owner_key.to_vec()));
            }
            Ok(())
          })
        },
      )
      .unwrap();
    assert_eq!(stats.class_counts, [0, 3, 1, 3, 3, 3, 2, 4]);
    roles.sort();
    assert_eq!((roles[0].0, roles[1].0), (1, 2));
    assert_ne!(roles[0].1, roles[1].1);
    drop(result);
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn parser_registry_dependency_is_retained_even_without_any_configured_fields() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(br#"{"$v":1,"parsers":{"text/plain":"p"}}"#),
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &AvailableSnapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let mut store = Store::new(algorithm);
    let result = compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
    let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete { catalog_record_count, definition_count, dependency_count, .. } = state.availability else {
      panic!("complete")
    };
    assert_eq!((catalog_record_count, definition_count, dependency_count), (2, 2, 1));
  }
}

fn small_catalog(
  registry: &CompiledParserRegistryV1,
  store: &mut Store,
  memory: &MemoryCoordinator,
  cancellation: &dyn Fn() -> bool,
) -> Result<CompiledSemanticCatalogV1, SemanticCatalogCompilationErrorV1> {
  let algorithm = store.algorithm;
  let inputs = ["/", "/nested"].into_iter().map(|path| configuration(algorithm, registry, memory, path));
  compile_semantic_catalog_v1(request(algorithm, 2), registry, inputs, store, memory, cancellation)
}

#[test]
fn every_publication_failure_before_or_after_durable_write_preserves_deterministic_retry() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let mut baseline = Store::new(ALGORITHMS[0]);
  let expected = small_catalog(&registry, &mut baseline, &memory, &|| false).unwrap().semantic_state().clone();
  assert!(baseline.writes > 5);
  for at in 1..=baseline.writes {
    for after in [false, true] {
      let mut store = Store::new(ALGORITHMS[0]);
      store.write_fault = Some((at, after));
      let error = failure(small_catalog(&registry, &mut store, &memory, &|| false));
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source)
        if source.class() == SemanticCatalogReadErrorClassV1::Unavailable && source.code() == "test_staging_write"));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      store.write_fault = None;
      let retry = small_catalog(&registry, &mut store, &memory, &|| false).unwrap();
      assert_eq!(retry.semantic_state(), &expected, "write {at}, after={after}");
    }
  }
}

#[test]
fn every_present_object_read_failure_is_detected_and_can_retry_without_losing_error_class() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let mut baseline = Store::new(ALGORITHMS[0]);
  let expected = small_catalog(&registry, &mut baseline, &memory, &|| false).unwrap().semantic_state().clone();
  assert!(baseline.present_reads.get() > baseline.writes);
  for at in 1..=baseline.present_reads.get() {
    for fault in [ReadFault::Unavailable, ReadFault::Missing, ReadFault::Corrupt, ReadFault::Resource] {
      let mut store = Store::new(ALGORITHMS[0]);
      store.read_fault = Some((at, fault));
      let error = failure(small_catalog(&registry, &mut store, &memory, &|| false));
      let expected_class = match fault {
        ReadFault::Unavailable => SemanticCatalogReadErrorClassV1::Unavailable,
        ReadFault::Resource => SemanticCatalogReadErrorClassV1::ResourceLimit,
        ReadFault::Missing | ReadFault::Corrupt => SemanticCatalogReadErrorClassV1::Corrupt,
      };
      assert!(
        matches!(&error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == expected_class),
        "read {at} {fault:?}: {error}"
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      store.read_fault = None;
      assert_eq!(small_catalog(&registry, &mut store, &memory, &|| false).unwrap().semantic_state(), &expected);
    }
  }
}

#[test]
fn cancellation_at_every_compiler_check_returns_no_candidate_and_releases_all_leases() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let checks = Cell::new(0usize);
  let mut baseline = Store::new(ALGORITHMS[0]);
  let expected = small_catalog(&registry, &mut baseline, &memory, &|| {
    checks.set(checks.get() + 1);
    false
  })
  .unwrap()
  .semantic_state()
  .clone();
  assert!(checks.get() > 100);
  for at in 1..=checks.get() {
    let mut store = Store::new(ALGORITHMS[0]);
    let observed = Cell::new(0usize);
    let error = failure(small_catalog(&registry, &mut store, &memory, &|| {
      observed.set(observed.get() + 1);
      observed.get() >= at
    }));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::Cancelled)
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    assert_eq!(small_catalog(&registry, &mut store, &memory, &|| false).unwrap().semantic_state(), &expected);
  }
}

#[test]
fn revoked_host_admission_after_every_publication_stops_and_retries_cleanly() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let mut baseline = Store::new(ALGORITHMS[0]);
  let expected = small_catalog(&registry, &mut baseline, &memory, &|| false).unwrap().semantic_state().clone();
  for at in 1..=baseline.writes {
    let mut store = Store::new(ALGORITHMS[0]);
    store.pressure_after_write = Some((at, memory.clone()));
    let error = failure(small_catalog(&registry, &mut store, &memory, &|| false));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
    );
    assert_eq!(store.writes, at);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    store.pressure_after_write = None;
    assert_eq!(small_catalog(&registry, &mut store, &memory, &|| false).unwrap().semantic_state(), &expected);
  }
}

#[test]
fn unavailable_global_admission_does_not_enumerate_or_access_storage() {
  let compiler_memory = memory();
  let registry = registry(ALGORITHMS[0], &compiler_memory);
  for task_memory in [MemoryCoordinator::without_policy(), MemoryCoordinator::new(MemoryPolicy::new(1, 2, 1, 1).unwrap())] {
    let mut store = Store::new(ALGORITHMS[0]);
    let inputs = std::iter::from_fn(|| -> Option<Result<CompiledIndexConfigurationV1, SemanticCompilationErrorV1>> {
      panic!("unadmitted source access")
    });
    let error = failure(compile_semantic_catalog_v1(request(ALGORITHMS[0], 1), &registry, inputs, &mut store, &task_memory, &|| false));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
    );
    assert_eq!((store.reads.get(), store.writes), (0, 0));
    assert_eq!(task_memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn unsupported_capabilities_are_rejected_before_source_or_storage_access() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let mut store = Store::new(ALGORITHMS[0]);
  let mut requested = request(ALGORITHMS[0], 0);
  requested.required_capabilities[31] = 0x80;
  let error = failure(compile_semantic_catalog_v1(requested, &registry, std::iter::empty(), &mut store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_capabilities", .. }));
  assert_eq!((store.reads.get(), store.writes), (0, 0));
}

#[test]
fn mismatched_registry_hash_is_rejected_before_publishing_any_object() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    for selected in ALGORITHMS.into_iter().filter(|selected| *selected != algorithm) {
      let mut store = Store::new(selected);
      let error = failure(compile_semantic_catalog_v1(request(selected, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false));
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { .. } | SemanticCatalogCompilationErrorV1::Catalog(_)));
      assert_eq!((store.reads.get(), store.writes), (0, 0));
    }
  }
}

#[test]
fn a_configuration_from_another_captured_registry_cannot_form_a_complete_catalog() {
  let memory = memory();
  let original = registry(ALGORITHMS[0], &memory);
  let replacement = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: Some(br#"{"$v":1,"parsers":{"text/plain":"p"}}"#),
      hash_algorithm: ALGORITHMS[0],
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &AvailableSnapshot,
    &memory,
    &|| false,
  )
  .unwrap();
  let mut store = Store::new(ALGORITHMS[0]);
  let input = configuration(ALGORITHMS[0], &original, &memory, "/");
  let error = failure(compile_semantic_catalog_v1(request(ALGORITHMS[0], 1), &replacement, [input], &mut store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_configuration_registry", .. }));
}

#[test]
fn conflicting_configurations_at_normalized_alias_paths_are_rejected() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  let mut store = Store::new(ALGORITHMS[0]);
  let inputs = [
    configuration(ALGORITHMS[0], &registry, &memory, "/a"),
    compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source: br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#,
        owner_path: "/a/",
        registry: &registry,
        hash_algorithm: ALGORITHMS[0],
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      },
      &Snapshot,
      &memory,
      &|| false,
    ),
  ];
  let error = failure(compile_semantic_catalog_v1(request(ALGORITHMS[0], 2), &registry, inputs, &mut store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_duplicate_configuration", .. }));
  drop(registry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn scope_limit_precedes_the_larger_catalog_control_owner_limit() {
  let memory = memory();
  let registry = registry(ALGORITHMS[0], &memory);
  // Frozen ASCP has a64-byte fixed header and a64KiB total cap. Its maximum
  // path is smaller than the catalog control key's separate65537-byte cap.
  let maximum_owner_length = 65_536 - 64;
  for length in [maximum_owner_length, maximum_owner_length + 1, 65_535] {
    let owner = format!("/{}", "a".repeat(length - 1));
    let mut store = Store::new(ALGORITHMS[0]);
    let input = configuration(ALGORITHMS[0], &registry, &memory, &owner);
    let result = compile_semantic_catalog_v1(request(ALGORITHMS[0], 1), &registry, [input], &mut store, &memory, &|| false);
    if length == maximum_owner_length {
      assert_eq!(result.unwrap().configuration_count(), 1);
    } else {
      assert!(matches!(
        failure(result),
        SemanticCatalogCompilationErrorV1::Input(SemanticCompilationErrorV1::Resource { message, .. })
          if message.contains("scope_exceeds_cap")
      ));
    }
  }
}

#[path = "semantic_catalog_update_spec.rs"]
mod update_spec;
