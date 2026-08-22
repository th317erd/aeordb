use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::first_authority::{LoadedIndexActivePointerPairV1, LoadedIndexActivePointerV1};
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, EncodedImmutableIndexArtifactV1, FieldIndexManifestBodyV1, FieldNvtManifestBodyV1,
  CoverageVersionV1, IndexManifestBodyV1, IndexManifestWriteV1, ScopeCatalogManifestBodyV1, ValueStoreManifestBodyV1,
  decode_index_manifest, encode_active_pointer, encode_index_manifest,
};
use aeordb::engine::v4::index_coverage_planner::{
  IndexCoverageGenerationHealthV1, IndexCoveragePlanningRequestV1, IndexCoveragePlanV1, IndexSemanticQueryAvailabilityV1,
  plan_selected_index_coverage_v1,
};
use aeordb::engine::v4::index_coverage_registry::{
  IndexCoverageNvtStatusV1, IndexCoverageNvtUnavailableReasonV1, IndexCoverageRegistryErrorV1, IndexCoverageRegistryOptionsV1,
  IndexCoverageRegistryOwnerKindV1, IndexCoverageRegistryOwnerRequestV1, IndexCoverageRegistrySelectionV1,
  IndexCoverageRegistrySourceErrorV1, IndexCoverageRegistrySourceV1, IndexCoverageRegistryUnavailableReasonV1, IndexCoverageRegistryV1,
  field_definition_fingerprint, field_dependency_fingerprint,
};
use tokio_util::sync::CancellationToken;

fn memory(limit: u64) -> Arc<MemoryCoordinator> {
  Arc::new(MemoryCoordinator::new(MemoryPolicy::new(limit, limit + 4 * 1_024 * 1_024, 1, 2 * 1_024 * 1_024).unwrap()))
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn profile_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("registry fixtures cover the two frozen v4 hash widths"),
  }
}

fn fixture_bytes(algorithm: HashAlgorithm, suffix: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("aidx-{}-{suffix}", profile_name(algorithm)))).unwrap()
}

struct ManifestChain {
  scope: EncodedImmutableIndexArtifactV1,
  value: EncodedImmutableIndexArtifactV1,
  field: EncodedImmutableIndexArtifactV1,
  nvt: EncodedImmutableIndexArtifactV1,
  scope_owner: Vec<u8>,
  field_owner: Vec<u8>,
}

impl ManifestChain {
  fn new(algorithm: HashAlgorithm) -> Self {
    let scope_fixture_bytes = fixture_bytes(algorithm, "scope-catalog-manifest-empty.bin");
    let scope_fixture = decode_index_manifest(&scope_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(scope_body) = scope_fixture.details else {
      panic!("scope fixture kind");
    };
    let coverage_root = scope_body.coverage.source_namespace_root.to_vec();
    let coverage_epoch = scope_body.coverage.coverage_epoch_id.to_vec();
    let coverage = CoverageVersionV1 {
      source_namespace_root: &coverage_root,
      coverage_epoch_id: &coverage_epoch,
      coverage_publication_sequence: scope_body.coverage.coverage_publication_sequence.max(1),
    };
    let scope = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: scope_fixture.generation,
      owner_id: scope_fixture.owner_id,
      body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 { coverage: coverage.clone(), ..scope_body }),
    })
    .unwrap();

    let value_fixture_bytes = fixture_bytes(algorithm, "value-store-manifest-empty.bin");
    let value_fixture = decode_index_manifest(&value_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(value_body) = value_fixture.details else {
      panic!("value fixture kind");
    };
    let value = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: value_fixture.generation,
      owner_id: value_fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        coverage: coverage.clone(),
        scope_catalog_manifest: &scope.key,
        ..value_body
      }),
    })
    .unwrap();

    let field_fixture_bytes = fixture_bytes(algorithm, "field-index-manifest-empty.bin");
    let field_fixture = decode_index_manifest(&field_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(field_body) = field_fixture.details else {
      panic!("field fixture kind");
    };
    let source_root = coverage.source_namespace_root.to_vec();
    let field_generation = field_fixture.generation;
    let field = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: field_generation,
      owner_id: field_fixture.owner_id,
      body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 { coverage, value_store_manifest: &value.key, ..field_body }),
    })
    .unwrap();

    let nvt_fixture_bytes = fixture_bytes(algorithm, "field-nvt-manifest-empty.bin");
    let nvt_fixture = decode_index_manifest(&nvt_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::FieldNvt(nvt_body) = nvt_fixture.details else {
      panic!("NVT fixture kind");
    };
    let nvt = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: nvt_fixture.generation,
      owner_id: field_fixture.owner_id,
      body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
        basis_posting_generation: field_generation,
        basis_source_head_hash: &source_root,
        ..nvt_body
      }),
    })
    .unwrap();

    Self { scope_owner: scope_fixture.owner_id.to_vec(), field_owner: field_fixture.owner_id.to_vec(), scope, value, field, nvt }
  }

  fn scope_successor(&self, algorithm: HashAlgorithm) -> EncodedImmutableIndexArtifactV1 {
    let manifest = decode_index_manifest(&self.scope.value, algorithm).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(body) = manifest.details else {
      panic!("scope manifest kind");
    };
    let source_root = vec![0xa7; algorithm.hash_length()];
    let coverage_epoch = body.coverage.coverage_epoch_id.to_vec();
    encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: manifest.generation.checked_add(1).unwrap(),
      owner_id: manifest.owner_id,
      body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 {
        coverage: CoverageVersionV1 {
          source_namespace_root: &source_root,
          coverage_epoch_id: &coverage_epoch,
          coverage_publication_sequence: body.coverage.coverage_publication_sequence.checked_add(1).unwrap(),
        },
        ..body
      }),
    })
    .unwrap()
  }
}

#[derive(Default)]
struct FakeSource {
  algorithm: Option<HashAlgorithm>,
  database_id: [u8; 16],
  pairs: BTreeMap<(u16, Vec<u8>), VecDeque<Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistrySourceErrorV1>>>,
  artifacts: BTreeMap<Vec<u8>, Result<Option<Vec<u8>>, IndexCoverageRegistrySourceErrorV1>>,
  loaded_artifacts: Vec<Vec<u8>>,
  cancel_on_artifact: Option<(Vec<u8>, CancellationToken)>,
}

struct ReentrantSource {
  registry: Arc<IndexCoverageRegistryV1>,
  delegate: FakeSource,
  nested_refresh_busy: Option<bool>,
}

impl IndexCoverageRegistrySourceV1 for ReentrantSource {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.delegate.hash_algorithm()
  }

  fn database_id(&self) -> [u8; 16] {
    self.delegate.database_id()
  }

  fn load_active_pointer_pair(
    &mut self,
    kind: ActivePointerKindV1,
    owner_id: &[u8],
  ) -> Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistrySourceErrorV1> {
    if self.nested_refresh_busy.is_none() {
      let mut nested = FakeSource::new(self.hash_algorithm(), self.database_id());
      self.nested_refresh_busy =
        Some(matches!(self.registry.refresh(&mut nested, &[], &CancellationToken::new()), Err(IndexCoverageRegistryErrorV1::RefreshBusy)));
    }
    self.delegate.load_active_pointer_pair(kind, owner_id)
  }

  fn load_artifact_bounded(
    &mut self,
    key: &[u8],
    maximum_value_length: usize,
  ) -> Result<Option<Vec<u8>>, IndexCoverageRegistrySourceErrorV1> {
    self.delegate.load_artifact_bounded(key, maximum_value_length)
  }
}

impl FakeSource {
  fn new(algorithm: HashAlgorithm, database_id: [u8; 16]) -> Self {
    Self { algorithm: Some(algorithm), database_id, ..Self::default() }
  }

  fn insert_artifact(&mut self, artifact: &EncodedImmutableIndexArtifactV1) {
    self.artifacts.insert(artifact.key.clone(), Ok(Some(artifact.value.clone())));
  }

  fn set_stable_pair(&mut self, kind: ActivePointerKindV1, artifact: &EncodedImmutableIndexArtifactV1, repair_required: bool) {
    let pair = self.pointer_pair(kind, artifact, 7, repair_required);
    let owner_id = pair.selected.as_ref().unwrap().owner_id.clone();
    self.pairs.insert((kind.id(), owner_id), VecDeque::from([Ok(pair.clone()), Ok(pair)]));
  }

  fn pointer_pair(
    &self,
    kind: ActivePointerKindV1,
    artifact: &EncodedImmutableIndexArtifactV1,
    pointer_sequence: u64,
    repair_required: bool,
  ) -> LoadedIndexActivePointerPairV1 {
    let manifest = decode_index_manifest(&artifact.value, self.algorithm.unwrap()).unwrap();
    let pointer = encode_active_pointer(&ActivePointerWriteV1 {
      kind,
      hash_algorithm: self.algorithm.unwrap(),
      generation: manifest.generation,
      owner_id: manifest.owner_id,
      slot: 0,
      sequence: pointer_sequence,
      target_manifest_hash: &artifact.key,
    })
    .unwrap();
    let selected = LoadedIndexActivePointerV1 {
      kind,
      generation: manifest.generation,
      owner_id: manifest.owner_id.to_vec(),
      selected_slot: 0,
      pointer_sequence,
      target_manifest_hash: artifact.key.clone(),
      write_sequence: 19,
      bytes: pointer.value,
    };
    LoadedIndexActivePointerPairV1 {
      slots: [Some(selected.clone()), None],
      selected: Some(selected),
      repair_required,
      structurally_invalid_slots: [false, false],
      closure_invalid_slots: [false, false],
    }
  }

  fn set_pair_responses(
    &mut self,
    kind: ActivePointerKindV1,
    owner_id: &[u8],
    responses: impl IntoIterator<Item = Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistrySourceErrorV1>>,
  ) {
    self.pairs.insert((kind.id(), owner_id.to_vec()), responses.into_iter().collect());
  }
}

impl IndexCoverageRegistrySourceV1 for FakeSource {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.algorithm.unwrap()
  }

  fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  fn load_active_pointer_pair(
    &mut self,
    kind: ActivePointerKindV1,
    owner_id: &[u8],
  ) -> Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistrySourceErrorV1> {
    self.pairs.get_mut(&(kind.id(), owner_id.to_vec())).and_then(VecDeque::pop_front).unwrap_or_else(|| {
      Ok(LoadedIndexActivePointerPairV1 {
        slots: [None, None],
        selected: None,
        repair_required: false,
        structurally_invalid_slots: [false, false],
        closure_invalid_slots: [false, false],
      })
    })
  }

  fn load_artifact_bounded(
    &mut self,
    key: &[u8],
    _maximum_value_length: usize,
  ) -> Result<Option<Vec<u8>>, IndexCoverageRegistrySourceErrorV1> {
    self.loaded_artifacts.push(key.to_vec());
    if let Some((cancel_key, cancellation)) = &self.cancel_on_artifact {
      if cancel_key == key {
        cancellation.cancel();
      }
    }
    self.artifacts.get(key).cloned().unwrap_or(Ok(None))
  }
}

fn request(
  kind: IndexCoverageRegistryOwnerKindV1,
  owner_id: Vec<u8>,
  health: IndexCoverageGenerationHealthV1,
) -> IndexCoverageRegistryOwnerRequestV1 {
  IndexCoverageRegistryOwnerRequestV1::new(kind, owner_id, health).unwrap()
}

fn registry(algorithm: HashAlgorithm, database_id: [u8; 16], memory: Arc<MemoryCoordinator>) -> IndexCoverageRegistryV1 {
  IndexCoverageRegistryV1::new(algorithm, database_id, IndexCoverageRegistryOptionsV1::new(8, 64 * 1_024).unwrap(), memory).unwrap()
}

#[test]
fn an_empty_registry_is_bounded_and_carries_one_database_authority() {
  let database_id = [0x11; 16];
  let registry = IndexCoverageRegistryV1::new(
    HashAlgorithm::Blake3_256,
    database_id,
    IndexCoverageRegistryOptionsV1::new(16, 64 * 1_024).unwrap(),
    memory(16 * 1_024 * 1_024),
  )
  .unwrap();
  let snapshot = registry.snapshot().unwrap();
  assert_eq!(snapshot.hash_algorithm(), HashAlgorithm::Blake3_256);
  assert_eq!(snapshot.database_id(), database_id);
  assert_eq!(snapshot.len(), 0);
  assert!(snapshot.is_empty());
  assert!(snapshot.retained_bytes() <= 64 * 1_024);
}

#[test]
fn missing_corrupt_and_repair_required_pairs_remain_explicit_without_inventing_coverage() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let registry = registry(algorithm, database_id, memory(16 * 1_024 * 1_024));

  let mut missing_source = FakeSource::new(algorithm, database_id);
  let missing = registry
    .refresh(
      &mut missing_source,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner.clone(), IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap();
  assert_eq!(
    missing.entries()[0].selection(),
    &IndexCoverageRegistrySelectionV1::Unavailable(IndexCoverageRegistryUnavailableReasonV1::NoSelectedGeneration)
  );

  let corrupt_pair = LoadedIndexActivePointerPairV1 {
    slots: [None, None],
    selected: None,
    repair_required: false,
    structurally_invalid_slots: [true, false],
    closure_invalid_slots: [false, true],
  };
  let mut corrupt_source = FakeSource::new(algorithm, database_id);
  corrupt_source.set_pair_responses(ActivePointerKindV1::ScopeCatalog, &chain.scope_owner, [Ok(corrupt_pair.clone()), Ok(corrupt_pair)]);
  let corrupt = registry
    .refresh(
      &mut corrupt_source,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner.clone(), IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap();
  assert_eq!(
    corrupt.entries()[0].selection(),
    &IndexCoverageRegistrySelectionV1::Unavailable(IndexCoverageRegistryUnavailableReasonV1::CorruptSelection)
  );

  let mut repair_source = FakeSource::new(algorithm, database_id);
  repair_source.insert_artifact(&chain.scope);
  repair_source.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, true);
  let degraded = registry
    .refresh(
      &mut repair_source,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner, IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap();
  let IndexCoverageRegistrySelectionV1::Selected(generation) = degraded.entries()[0].selection() else {
    panic!("repairable selected pair remains readable");
  };
  assert_eq!(generation.health(), IndexCoverageGenerationHealthV1::Degraded);
}

#[test]
fn pointer_change_or_corrupt_transitive_closure_never_replaces_the_prior_snapshot() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let coordinator = memory(16 * 1_024 * 1_024);
  let coverage_registry = registry(algorithm, database_id, Arc::clone(&coordinator));
  let prior = coverage_registry.snapshot().unwrap();
  let baseline_bytes = coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;

  let mut moved = FakeSource::new(algorithm, database_id);
  moved.insert_artifact(&chain.scope);
  let first = moved.pointer_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, 7, false);
  let second = moved.pointer_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, 8, false);
  moved.set_pair_responses(ActivePointerKindV1::ScopeCatalog, &chain.scope_owner, [Ok(first), Ok(second)]);
  let error = coverage_registry
    .refresh(
      &mut moved,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner.clone(), IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap_err();
  assert!(matches!(error, IndexCoverageRegistryErrorV1::SelectionChanged));
  assert!(Arc::ptr_eq(&prior, &coverage_registry.snapshot().unwrap()));
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, baseline_bytes);

  let mut missing_dependency = FakeSource::new(algorithm, database_id);
  missing_dependency.insert_artifact(&chain.field);
  missing_dependency.insert_artifact(&chain.value);
  missing_dependency.set_stable_pair(ActivePointerKindV1::FieldIndex, &chain.field, false);
  let error = coverage_registry
    .refresh(
      &mut missing_dependency,
      &[request(IndexCoverageRegistryOwnerKindV1::FieldIndex, chain.field_owner, IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap_err();
  assert!(matches!(error, IndexCoverageRegistryErrorV1::Corrupt { .. }));
  assert!(Arc::ptr_eq(&prior, &coverage_registry.snapshot().unwrap()));
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, baseline_bytes);
}

#[test]
fn malformed_pair_shape_and_zero_coverage_sequence_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let registry = registry(algorithm, database_id, memory(16 * 1_024 * 1_024));

  let mut dishonest_source = FakeSource::new(algorithm, database_id);
  let selected = dishonest_source.pointer_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, 7, false);
  let dishonest_pair = LoadedIndexActivePointerPairV1 { selected: None, ..selected };
  dishonest_source.set_pair_responses(
    ActivePointerKindV1::ScopeCatalog,
    &chain.scope_owner,
    [Ok(dishonest_pair.clone()), Ok(dishonest_pair)],
  );
  assert!(matches!(
    registry
      .refresh(
        &mut dishonest_source,
        &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner, IndexCoverageGenerationHealthV1::Healthy,)],
        &CancellationToken::new(),
      )
      .unwrap_err(),
    IndexCoverageRegistryErrorV1::Corrupt { .. }
  ));

  let zero_bytes = fixture_bytes(algorithm, "scope-catalog-manifest-empty.bin");
  let zero_manifest = decode_index_manifest(&zero_bytes, algorithm).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(zero_body) = &zero_manifest.details else {
    panic!("scope fixture kind");
  };
  assert_eq!(zero_body.coverage.coverage_publication_sequence, 0);
  let zero_owner = zero_manifest.owner_id.to_vec();
  let zero_key = zero_manifest.key.clone();
  let zero_artifact = EncodedImmutableIndexArtifactV1 { key: zero_key, value: zero_bytes };
  let mut zero_source = FakeSource::new(algorithm, database_id);
  zero_source.insert_artifact(&zero_artifact);
  zero_source.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &zero_artifact, false);
  assert!(matches!(
    registry
      .refresh(
        &mut zero_source,
        &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, zero_owner, IndexCoverageGenerationHealthV1::Healthy,)],
        &CancellationToken::new(),
      )
      .unwrap_err(),
    IndexCoverageRegistryErrorV1::Corrupt { .. }
  ));
}

#[test]
fn refresh_is_single_owner_and_never_allows_an_older_build_to_race_a_newer_snapshot() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let registry = Arc::new(registry(algorithm, database_id, memory(16 * 1_024 * 1_024)));
  let mut delegate = FakeSource::new(algorithm, database_id);
  delegate.insert_artifact(&chain.scope);
  delegate.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, false);
  let mut source = ReentrantSource { registry: Arc::clone(&registry), delegate, nested_refresh_busy: None };
  let snapshot = registry
    .refresh(
      &mut source,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner, IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap();
  assert_eq!(source.nested_refresh_busy, Some(true));
  assert_eq!(snapshot.len(), 1);
  assert!(Arc::ptr_eq(&snapshot, &registry.snapshot().unwrap()));
}

#[test]
fn cancellation_and_memory_pressure_release_every_provisional_byte_without_swapping() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let coordinator = memory(16 * 1_024 * 1_024);
  let coverage_registry = registry(algorithm, database_id, Arc::clone(&coordinator));
  let prior = coverage_registry.snapshot().unwrap();
  let baseline_bytes = coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
  let cancellation = CancellationToken::new();
  let mut source = FakeSource::new(algorithm, database_id);
  for artifact in [&chain.scope, &chain.value, &chain.field, &chain.nvt] {
    source.insert_artifact(artifact);
  }
  source.set_stable_pair(ActivePointerKindV1::FieldIndex, &chain.field, false);
  source.set_stable_pair(ActivePointerKindV1::FieldNvt, &chain.nvt, false);
  source.cancel_on_artifact = Some((chain.nvt.key.clone(), cancellation.clone()));
  let error = coverage_registry
    .refresh(
      &mut source,
      &[request(IndexCoverageRegistryOwnerKindV1::FieldIndex, chain.field_owner.clone(), IndexCoverageGenerationHealthV1::Healthy)],
      &cancellation,
    )
    .unwrap_err();
  assert!(matches!(error, IndexCoverageRegistryErrorV1::Cancelled));
  assert!(Arc::ptr_eq(&prior, &coverage_registry.snapshot().unwrap()));
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, baseline_bytes);

  let constrained_memory = memory(512 * 1_024);
  let constrained = registry(algorithm, database_id, Arc::clone(&constrained_memory));
  let constrained_prior = constrained.snapshot().unwrap();
  let mut constrained_source = FakeSource::new(algorithm, database_id);
  constrained_source.insert_artifact(&chain.scope);
  constrained_source.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, false);
  let error = constrained
    .refresh(
      &mut constrained_source,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner, IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap_err();
  assert!(matches!(error, IndexCoverageRegistryErrorV1::Memory(_)));
  assert!(Arc::ptr_eq(&constrained_prior, &constrained.snapshot().unwrap()));
}

#[test]
fn malformed_request_sets_fail_before_source_io() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let registry = IndexCoverageRegistryV1::new(
    algorithm,
    database_id,
    IndexCoverageRegistryOptionsV1::new(1, 64 * 1_024).unwrap(),
    memory(16 * 1_024 * 1_024),
  )
  .unwrap();
  let scope = request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner.clone(), IndexCoverageGenerationHealthV1::Healthy);
  let field = request(IndexCoverageRegistryOwnerKindV1::FieldIndex, chain.field_owner, IndexCoverageGenerationHealthV1::Healthy);
  for requests in [vec![scope.clone(), scope.clone()], vec![field, scope]] {
    let mut source = FakeSource::new(algorithm, database_id);
    let error = registry.refresh(&mut source, &requests, &CancellationToken::new()).unwrap_err();
    assert!(matches!(error, IndexCoverageRegistryErrorV1::Invalid { .. }));
    assert!(source.loaded_artifacts.is_empty());
    assert!(source.pairs.is_empty());
  }
  let mut source = FakeSource::new(algorithm, database_id);
  let wrong_width = request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, vec![0x44; 64], IndexCoverageGenerationHealthV1::Healthy);
  assert!(matches!(
    registry.refresh(&mut source, &[wrong_width], &CancellationToken::new()).unwrap_err(),
    IndexCoverageRegistryErrorV1::Invalid { .. }
  ));
}

#[test]
fn nvt_absence_staleness_or_source_failure_cannot_remove_field_coverage() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let request = request(IndexCoverageRegistryOwnerKindV1::FieldIndex, chain.field_owner.clone(), IndexCoverageGenerationHealthV1::Healthy);
  let registry = registry(algorithm, database_id, memory(16 * 1_024 * 1_024));

  let mut absent = FakeSource::new(algorithm, database_id);
  for artifact in [&chain.scope, &chain.value, &chain.field] {
    absent.insert_artifact(artifact);
  }
  absent.set_stable_pair(ActivePointerKindV1::FieldIndex, &chain.field, false);
  let snapshot = registry.refresh(&mut absent, &[request.clone()], &CancellationToken::new()).unwrap();
  assert!(matches!(snapshot.entries()[0].selection(), IndexCoverageRegistrySelectionV1::Selected(_)));
  assert_eq!(snapshot.entries()[0].nvt_status(), &IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::Absent));

  let nvt_manifest = decode_index_manifest(&chain.nvt.value, algorithm).unwrap();
  let IndexManifestBodyV1::FieldNvt(nvt_body) = nvt_manifest.details else {
    panic!("NVT fixture kind");
  };
  let stale_nvt = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: algorithm,
    generation: nvt_manifest.generation,
    owner_id: nvt_manifest.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
      basis_posting_generation: nvt_body.basis_posting_generation + 1,
      ..nvt_body
    }),
  })
  .unwrap();
  let mut stale = FakeSource::new(algorithm, database_id);
  for artifact in [&chain.scope, &chain.value, &chain.field] {
    stale.insert_artifact(artifact);
  }
  stale.insert_artifact(&stale_nvt);
  stale.set_stable_pair(ActivePointerKindV1::FieldIndex, &chain.field, false);
  stale.set_stable_pair(ActivePointerKindV1::FieldNvt, &stale_nvt, false);
  let snapshot = registry.refresh(&mut stale, &[request.clone()], &CancellationToken::new()).unwrap();
  assert!(matches!(snapshot.entries()[0].selection(), IndexCoverageRegistrySelectionV1::Selected(_)));
  assert_eq!(
    snapshot.entries()[0].nvt_status(),
    &IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::StalePostingGeneration)
  );

  let mut unavailable = FakeSource::new(algorithm, database_id);
  for artifact in [&chain.scope, &chain.value, &chain.field] {
    unavailable.insert_artifact(artifact);
  }
  unavailable.set_stable_pair(ActivePointerKindV1::FieldIndex, &chain.field, false);
  unavailable.set_pair_responses(
    ActivePointerKindV1::FieldNvt,
    &chain.field_owner,
    [Err(IndexCoverageRegistrySourceErrorV1::unavailable("nvt_io", "injected NVT read failure"))],
  );
  let snapshot = registry.refresh(&mut unavailable, &[request], &CancellationToken::new()).unwrap();
  assert!(matches!(snapshot.entries()[0].selection(), IndexCoverageRegistrySelectionV1::Selected(_)));
  assert_eq!(
    snapshot.entries()[0].nvt_status(),
    &IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::SourceUnavailable)
  );
}

#[test]
fn registry_architecture_delegates_selection_and_never_loads_or_publishes_index_pages() {
  let source = include_str!("../../src/engine/v4/index_coverage_registry.rs");
  let planner = include_str!("../../src/engine/v4/index_coverage_planner.rs");
  let partial = include_str!("../../src/engine/v4/index_partial_acceleration.rs");
  assert_eq!(source.matches(".load_index_active_pointer_pair(").count(), 1);
  assert_eq!(planner.matches("pub fn plan_selected_index_coverage_v1").count(), 1);
  assert_eq!(partial.matches("pub fn execute_partial_index_acceleration_v1").count(), 1);
  assert!(!source.contains("publish_index_active_pointer"));
  assert!(!source.contains("encode_active_pointer"));
  assert!(!source.contains("decode_ordered_page"));
  assert!(!source.contains("decode_artifact_directory"));
  assert!(!source.contains("execute_partial_index_acceleration_v1"));
  assert!(!planner.contains("IndexCoverageRegistryV1"));
  assert!(!partial.contains("IndexCoverageRegistryV1"));
  assert!(!source.contains("std::thread::spawn"));
  assert!(!source.contains("tokio::spawn"));
}

#[test]
fn selected_generation_replacement_retains_in_flight_readers_and_reconstructs_after_restart_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let database_id = [0x11; 16];
    let chain = ManifestChain::new(algorithm);
    let successor = chain.scope_successor(algorithm);
    let coordinator = memory(16 * 1_024 * 1_024);
    let coverage_registry = registry(algorithm, database_id, Arc::clone(&coordinator));
    let requests =
      [request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner.clone(), IndexCoverageGenerationHealthV1::Healthy)];

    let mut initial_source = FakeSource::new(algorithm, database_id);
    initial_source.insert_artifact(&chain.scope);
    initial_source.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, false);
    let in_flight = coverage_registry.refresh(&mut initial_source, &requests, &CancellationToken::new()).unwrap();
    let IndexCoverageRegistrySelectionV1::Selected(initial) = in_flight.entries()[0].selection() else {
      panic!("initial selected generation");
    };

    let mut successor_source = FakeSource::new(algorithm, database_id);
    successor_source.insert_artifact(&successor);
    let successor_pair = successor_source.pointer_pair(ActivePointerKindV1::ScopeCatalog, &successor, 8, false);
    successor_source.set_pair_responses(
      ActivePointerKindV1::ScopeCatalog,
      &chain.scope_owner,
      [Ok(successor_pair.clone()), Ok(successor_pair)],
    );
    let current = coverage_registry.refresh(&mut successor_source, &requests, &CancellationToken::new()).unwrap();
    let IndexCoverageRegistrySelectionV1::Selected(replacement) = current.entries()[0].selection() else {
      panic!("replacement selected generation");
    };
    assert_eq!(initial.manifest_hash(), chain.scope.key);
    assert_eq!(replacement.manifest_hash(), successor.key);
    assert_eq!(replacement.generation(), initial.generation() + 1);
    assert_ne!(replacement.source_namespace_root(), initial.source_namespace_root());
    let overlap = coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
    assert!(overlap >= in_flight.retained_bytes() + current.retained_bytes());
    let expected_entries = current.entries().to_vec();
    drop(in_flight);
    assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, current.retained_bytes());

    let restart_coordinator = memory(16 * 1_024 * 1_024);
    let reopened = registry(algorithm, database_id, restart_coordinator);
    let mut restart_source = FakeSource::new(algorithm, database_id);
    restart_source.insert_artifact(&successor);
    let restart_pair = restart_source.pointer_pair(ActivePointerKindV1::ScopeCatalog, &successor, 8, false);
    restart_source.set_pair_responses(ActivePointerKindV1::ScopeCatalog, &chain.scope_owner, [Ok(restart_pair.clone()), Ok(restart_pair)]);
    let reconstructed = reopened.refresh(&mut restart_source, &requests, &CancellationToken::new()).unwrap();
    assert_eq!(reconstructed.entries(), expected_entries);
  }
}

#[test]
fn in_flight_snapshots_retain_their_generation_and_memory_until_last_release() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x11; 16];
  let chain = ManifestChain::new(algorithm);
  let memory = memory(16 * 1_024 * 1_024);
  let registry = registry(algorithm, database_id, Arc::clone(&memory));
  let old = registry.snapshot().unwrap();
  let old_bytes = old.retained_bytes();
  let mut source = FakeSource::new(algorithm, database_id);
  source.insert_artifact(&chain.scope);
  source.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, false);
  let current = registry
    .refresh(
      &mut source,
      &[request(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, chain.scope_owner, IndexCoverageGenerationHealthV1::Healthy)],
      &CancellationToken::new(),
    )
    .unwrap();
  let overlap = memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
  assert!(overlap >= old_bytes + current.retained_bytes());
  assert!(old.is_empty());
  assert_eq!(current.len(), 1);
  drop(old);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, current.retained_bytes());
}

#[test]
fn selected_scope_field_and_compatible_nvt_metadata_load_at_both_hash_widths_without_pages() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let database_id = [0x11; 16];
    let chain = ManifestChain::new(algorithm);
    let mut source = FakeSource::new(algorithm, database_id);
    for artifact in [&chain.scope, &chain.value, &chain.field, &chain.nvt] {
      source.insert_artifact(artifact);
    }
    source.set_stable_pair(ActivePointerKindV1::ScopeCatalog, &chain.scope, false);
    source.set_stable_pair(ActivePointerKindV1::FieldIndex, &chain.field, false);
    source.set_stable_pair(ActivePointerKindV1::FieldNvt, &chain.nvt, false);

    let memory = memory(16 * 1_024 * 1_024);
    let registry = IndexCoverageRegistryV1::new(
      algorithm,
      database_id,
      IndexCoverageRegistryOptionsV1::new(8, 64 * 1_024).unwrap(),
      Arc::clone(&memory),
    )
    .unwrap();
    let requests = [
      IndexCoverageRegistryOwnerRequestV1::new(
        IndexCoverageRegistryOwnerKindV1::ScopeCatalog,
        chain.scope_owner.clone(),
        IndexCoverageGenerationHealthV1::Healthy,
      )
      .unwrap(),
      IndexCoverageRegistryOwnerRequestV1::new(
        IndexCoverageRegistryOwnerKindV1::FieldIndex,
        chain.field_owner.clone(),
        IndexCoverageGenerationHealthV1::Healthy,
      )
      .unwrap(),
    ];
    let snapshot = registry.refresh(&mut source, &requests, &CancellationToken::new()).unwrap();
    assert_eq!(snapshot.len(), 2);
    let field = snapshot.entry(IndexCoverageRegistryOwnerKindV1::FieldIndex, &chain.field_owner).unwrap();
    let IndexCoverageRegistrySelectionV1::Selected(selected) = field.selection() else {
      panic!("field generation must be selected");
    };
    assert_eq!(selected.manifest_hash(), chain.field.key);
    assert_eq!(selected.owner_id(), chain.field_owner);
    assert!(selected.definition_fingerprint().iter().any(|byte| *byte != 0));
    assert!(selected.dependency_fingerprint().iter().any(|byte| *byte != 0));
    let field_manifest = decode_index_manifest(&chain.field.value, algorithm).unwrap();
    let value_manifest = decode_index_manifest(&chain.value.value, algorithm).unwrap();
    let scope_manifest = decode_index_manifest(&chain.scope.value, algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(field_body) = field_manifest.details else {
      panic!("field manifest kind");
    };
    assert_eq!(selected.definition_fingerprint(), field_definition_fingerprint(algorithm, field_body.field_index_definition));
    assert_eq!(
      selected.dependency_fingerprint(),
      field_dependency_fingerprint(algorithm, scope_manifest.owner_id, value_manifest.owner_id)
    );
    assert!(matches!(field.nvt_status(), IndexCoverageNvtStatusV1::Usable(_)));

    let generation = selected.as_planning_generation();
    let plan = plan_selected_index_coverage_v1(&IndexCoveragePlanningRequestV1 {
      hash_algorithm: algorithm,
      requested_namespace_root: generation.source_namespace_root,
      requested_publication_sequence: generation.coverage_publication_sequence,
      required_owner_id: selected.owner_id(),
      required_definition_fingerprint: selected.definition_fingerprint(),
      required_dependency_fingerprint: selected.dependency_fingerprint(),
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      selected_generation: Some(generation),
    })
    .unwrap();
    let IndexCoveragePlanV1::Complete { generation } = plan else {
      panic!("exact-root selected generation must plan complete coverage");
    };
    assert_eq!(generation.manifest_hash, selected.manifest_hash());
    assert!(snapshot.retained_bytes() <= 64 * 1_024);
    assert!(source
      .loaded_artifacts
      .iter()
      .all(|key| { [&chain.scope.key, &chain.value.key, &chain.field.key, &chain.nvt.key].contains(&key) }));
    let clean = memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
    assert!(clean >= snapshot.retained_bytes());
  }
}
