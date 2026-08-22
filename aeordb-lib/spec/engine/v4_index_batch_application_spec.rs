use std::collections::BTreeMap;
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::{EncodedImmutableIndexArtifactV1, IndexManifestWriteV1, decode_index_manifest, encode_index_manifest};
use aeordb::engine::v4::index_batch_application::{
  INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1, IndexBatchApplicationErrorV1, IndexBatchArtifactOverlayLimitsV1, IndexBatchArtifactReadErrorV1,
  IndexBatchArtifactSourceV1, IndexManifestSuccessorRequestV1, OrderedPagePathLookupLimitsV1, OrderedPagePathLookupRequestV1,
  SparseIndexArtifactOverlayV1, load_ordered_page_path_v1, synthesize_successor_index_manifest_v1,
};
use aeordb::engine::v4::index_coordinator::{
  FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexGroupMutationRequestV1,
  IndexMembershipOwnerClassV1, IndexMembershipStateV1, IndexMembershipTransitionRequestV1, IndexMutationGroupRequestV1,
  IndexMutationOperationV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_copy_on_write::{
  ArtifactDirectoryMutationRequestV1, ArtifactDirectoryPathV1, IndexCopyOnWriteBootstrapRequestV1, IndexCopyOnWriteClosureRequestV1,
  IndexCopyOnWriteClosureSummaryV1, OrderedPageMutationKindV1, OrderedPageMutationRequestV1, bootstrap_ordered_index_v1,
  default_index_directory_layout_v1, default_index_page_layout_v1, mutate_ordered_page_v1, rewrite_artifact_directory_paths_v1,
  validate_index_copy_on_write_closure_v1,
};
use aeordb::engine::v4::index_manifest::{
  CoverageVersionV1, FieldIndexManifestBodyV1, IndexManifestBodyV1, ScopeCatalogManifestBodyV1, ValueStoreManifestBodyV1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, decode_ordered_record, encode_artifact_directory, encode_ordered_page,
  encode_posting_record, ordered_record_order_key,
};
use aeordb::engine::v4::index_record::{
  CanonicalValueRecordV1, DocumentStateOwnerV1, DocumentStateRecordV1, ScopeDocumentRecordV1, ScopeReverseRecordV1,
  encode_canonical_value_record, encode_document_state_record, encode_scope_document_record, encode_scope_reverse_record,
};

fn owner(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  vec![0x71; hash_algorithm.hash_length()]
}

fn fixture_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn fixture_manifest(hash_algorithm: HashAlgorithm, kind: &str) -> Vec<u8> {
  let profile = match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("manifest successor tests use the independently frozen hash-width profiles"),
  };
  fs::read(fixture_root().join(format!("aidx-{profile}-{kind}-manifest-populated.bin"))).unwrap()
}

fn transition_only_batch(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  owner_class: IndexMembershipOwnerClassV1,
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
) -> FrozenIndexBatchV1 {
  let hard_limit = 1_000_000;
  let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, hard_limit, 1, 299_999).unwrap());
  let mut coordinator =
    IndexCoordinatorV1::new([0x41; 16], hash_algorithm, memory, IndexCoordinatorOptionsV1::new(500_000, 1, 1_000, 500_000).unwrap(), 1_000)
      .unwrap();
  coordinator
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: IndexMembershipTransitionRequestV1 {
          owner_id,
          owner_class,
          publication_sequence: 9,
          operation_id: [0x49; 16],
          document_ordinal: 7,
          before,
          after,
        },
        mutations: &[],
      },
      1_001,
    )
    .unwrap();
  coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap()
}

fn mutation_batch(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  owner_class: IndexMembershipOwnerClassV1,
  role: OrderedIndexRoleV1,
  encoded_record: &[u8],
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
) -> FrozenIndexBatchV1 {
  let hard_limit = 1_000_000;
  let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, hard_limit, 1, 299_999).unwrap());
  let mut coordinator =
    IndexCoordinatorV1::new([0x42; 16], hash_algorithm, memory, IndexCoordinatorOptionsV1::new(500_000, 1, 1_000, 500_000).unwrap(), 1_000)
      .unwrap();
  let mutation = [IndexGroupMutationRequestV1 {
    operation: IndexMutationOperationV1::Upsert,
    mutation: IndexMutationRequestV1 { index_id: owner_id, role, publication_sequence: 9, operation_id: [0x59; 16], encoded_record },
  }];
  coordinator
    .admit_group(
      IndexMutationGroupRequestV1 {
        transition: IndexMembershipTransitionRequestV1 {
          owner_id,
          owner_class,
          publication_sequence: 9,
          operation_id: [0x59; 16],
          document_ordinal: 7,
          before,
          after,
        },
        mutations: &mutation,
      },
      1_001,
    )
    .unwrap();
  coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap()
}

fn ordered_leaf_directory(hash_algorithm: HashAlgorithm, page: &EncodedImmutableIndexArtifactV1) -> EncodedImmutableIndexArtifactV1 {
  let page = decode_ordered_page(&page.value, hash_algorithm).unwrap();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: page.role,
    owner_id: page.owner_id,
    generation: page.generation,
    level: 0,
    entries: &[ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &page.key,
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap()
}

fn cow_summary(
  hash_algorithm: HashAlgorithm,
  source_page: &EncodedImmutableIndexArtifactV1,
  source_root: &EncodedImmutableIndexArtifactV1,
  successor_generation: u64,
  initial_next_page_id: u64,
  mutation: OrderedPageMutationKindV1<'_>,
) -> IndexCopyOnWriteClosureSummaryV1 {
  let page_plan = mutate_ordered_page_v1(&OrderedPageMutationRequestV1 {
    hash_algorithm,
    source_page: &source_page.value,
    next_posting_page: None,
    generation: successor_generation,
    next_page_id: initial_next_page_id,
    mutation,
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  let directories = [source_root.value.as_slice()];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &source_page.key, directories: &directories }];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: successor_generation,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  let source_pages = [source_page.value.as_slice()];
  let applied_mutations = [mutation];
  validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm,
    generation: successor_generation,
    initial_next_page_id,
    applied_mutations: Some(&applied_mutations),
    source_pages: &source_pages,
    paths: &paths,
    page_plan: &page_plan,
    directory_plan: &directory_plan,
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  })
  .unwrap()
}

fn posting_record(coordinate: u64) -> Vec<u8> {
  encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate,
    document_ordinal: coordinate,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &coordinate.to_le_bytes(),
  })
  .unwrap()
}

fn posting_order_key(hash_algorithm: HashAlgorithm, coordinate: u64) -> Vec<u8> {
  let record = posting_record(coordinate);
  ordered_record_order_key(&decode_ordered_record(&record, hash_algorithm, OrderedIndexRoleV1::Posting).unwrap()).unwrap()
}

fn posting_page(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  coordinate: u64,
  page_id: u64,
  previous: u64,
  next: u64,
) -> EncodedImmutableIndexArtifactV1 {
  let record = posting_record(coordinate);
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 7,
    page_id,
    previous_page_id: previous,
    next_page_id: next,
    records: &[&record],
  })
  .unwrap()
}

fn leaf_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  pages: &[&EncodedImmutableIndexArtifactV1],
) -> EncodedImmutableIndexArtifactV1 {
  let pages = pages.iter().map(|artifact| decode_ordered_page(&artifact.value, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let entries = pages
    .iter()
    .map(|page| ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &page.key,
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 7,
    level: 0,
    entries: &entries,
  })
  .unwrap()
}

fn parent_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  child: &EncodedImmutableIndexArtifactV1,
  live_count_delta: u64,
) -> EncodedImmutableIndexArtifactV1 {
  internal_directory(hash_algorithm, owner_id, &[(child, live_count_delta)])
}

fn internal_directory(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  children: &[(&EncodedImmutableIndexArtifactV1, u64)],
) -> EncodedImmutableIndexArtifactV1 {
  let children = children
    .iter()
    .map(|(artifact, live_count_delta)| (decode_artifact_directory(&artifact.value, hash_algorithm).unwrap(), *live_count_delta))
    .collect::<Vec<_>>();
  let entries = children
    .iter()
    .map(|(child, live_count_delta)| ArtifactDirectoryEntryWriteV1 {
      lower_fence: child.lower_fence,
      upper_fence: child.upper_fence,
      child_hash: &child.key,
      child_generation: child.generation,
      live_count: child.live_count + live_count_delta,
      tombstone_count: child.tombstone_count,
      page_count: child.page_count,
      logical_bytes: child.logical_bytes,
      minimum_page_id: child.minimum_page_id,
      maximum_page_id: child.maximum_page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation: 8,
    level: children[0].0.level + 1,
    entries: &entries,
  })
  .unwrap()
}

#[derive(Default)]
struct CountingSource {
  values: BTreeMap<Vec<u8>, Vec<u8>>,
  reads: Vec<Vec<u8>>,
  failure: Option<IndexBatchArtifactReadErrorV1>,
}

impl CountingSource {
  fn insert(&mut self, artifact: &EncodedImmutableIndexArtifactV1) {
    self.values.insert(artifact.key.clone(), artifact.value.clone());
  }
}

impl IndexBatchArtifactSourceV1 for CountingSource {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<Vec<u8>, IndexBatchArtifactReadErrorV1> {
    self.reads.push(key.to_vec());
    if let Some(error) = self.failure.take() {
      return Err(error);
    }
    let value = self.values.get(key).ok_or(IndexBatchArtifactReadErrorV1::Missing)?;
    if value.len() > maximum_bytes {
      return Err(IndexBatchArtifactReadErrorV1::ResourcePressure("source value exceeds the supplied read bound".to_string()));
    }
    Ok(value.clone())
  }
}

#[test]
fn manifest_successor_preserves_count_neutral_transition_only_state_and_rejects_unclosed_count_changes() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let source = fixture_manifest(hash_algorithm, "value-store");
    let original = source.clone();
    let decoded = decode_index_manifest(&source, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(body) = &decoded.details else {
      panic!("fixture is a value-store manifest");
    };
    let count_neutral = transition_only_batch(
      hash_algorithm,
      decoded.owner_id,
      IndexMembershipOwnerClassV1::ValueStore,
      IndexMembershipStateV1 { live: false, unindexable: false },
      IndexMembershipStateV1 { live: false, unindexable: false },
    );
    let source_namespace_root = vec![0xc1; hash_algorithm.hash_length()];
    let coverage_epoch_id = vec![0xc2; 16];
    let coverage = CoverageVersionV1 {
      source_namespace_root: &source_namespace_root,
      coverage_epoch_id: &coverage_epoch_id,
      coverage_publication_sequence: 9,
    };
    let successor_generation = decoded.generation.checked_add(1).unwrap();
    let successor = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source,
        generation: successor_generation,
        parent_manifest_key: Some(body.scope_catalog_manifest),
        coverage: coverage.clone(),
        next_document_ordinal: None,
        mutations: count_neutral.records(),
        transitions: count_neutral.transitions(),
        role_summaries: &[],
      },
      &|| false,
    )
    .unwrap();
    let successor = decode_index_manifest(&successor.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(successor_body) = successor.details else {
      panic!("successor is a value-store manifest");
    };
    assert_eq!(successor.generation, successor_generation);
    assert_eq!(successor.owner_id, decoded.owner_id);
    assert_eq!(successor_body.coverage, coverage);
    assert_eq!(successor_body.scope_catalog_manifest, body.scope_catalog_manifest);
    assert_eq!(successor_body.value_directory_root, body.value_directory_root);
    assert_eq!(successor_body.document_state_directory_root, body.document_state_directory_root);
    assert_eq!(successor_body.next_page_id, body.next_page_id);
    assert_eq!(successor_body.value_page_count, body.value_page_count);
    assert_eq!(successor_body.state_page_count, body.state_page_count);
    assert_eq!(successor_body.value_document_count, body.value_document_count);
    assert_eq!(successor_body.unindexable_document_count, body.unindexable_document_count);
    assert_eq!(successor_body.live_value_count, body.live_value_count);
    assert_eq!(successor_body.value_tombstone_count, body.value_tombstone_count);
    assert_eq!(successor_body.state_tombstone_count, body.state_tombstone_count);
    assert_eq!(successor_body.live_canonical_value_bytes, body.live_canonical_value_bytes);
    assert_eq!(successor_body.required_reader_capabilities, body.required_reader_capabilities);
    assert_eq!(successor_body.value_store_definition, body.value_store_definition);
    assert_eq!(source, original, "manifest synthesis mutated its immutable source bytes");

    let unclosed = transition_only_batch(
      hash_algorithm,
      decoded.owner_id,
      IndexMembershipOwnerClassV1::ValueStore,
      IndexMembershipStateV1 { live: false, unindexable: false },
      IndexMembershipStateV1 { live: false, unindexable: true },
    );
    let error = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source,
        generation: successor_generation,
        parent_manifest_key: Some(body.scope_catalog_manifest),
        coverage,
        next_document_ordinal: None,
        mutations: unclosed.records(),
        transitions: unclosed.transitions(),
        role_summaries: &[],
      },
      &|| false,
    )
    .unwrap_err();
    assert!(
      matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
    );
  }
}

#[test]
fn manifest_successor_accepts_absent_root_bootstrap_and_rejects_it_for_a_present_root() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "value-store");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(fixture_body) = &fixture.details else {
      panic!("fixture is a value-store manifest");
    };
    let source_generation = fixture.generation;
    let successor_generation = source_generation.checked_add(1).unwrap();
    let source_next_page_id = fixture_body.next_page_id;
    let absent_source = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: source_generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        value_directory_root: None,
        next_page_id: source_next_page_id,
        value_page_count: 0,
        value_document_count: 0,
        live_value_count: 0,
        value_tombstone_count: 0,
        live_canonical_value_bytes: 0,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let source = decode_index_manifest(&absent_source.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(source_body) = &source.details else {
      panic!("source is a value-store manifest");
    };

    let canonical_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("first".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let record_revision_hash = vec![0xe1; hash_algorithm.hash_length()];
    let inserted = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 7,
        source_value_ordinal: 0,
        record_revision_hash: &record_revision_hash,
        canonical_value: Some(&canonical_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let mutations = [OrderedPageMutationKindV1::UpsertLive(&inserted)];
    let bootstrap = bootstrap_ordered_index_v1(&IndexCopyOnWriteBootstrapRequestV1 {
      hash_algorithm,
      owner_id: source.owner_id,
      role: OrderedIndexRoleV1::Value,
      generation: successor_generation,
      initial_next_page_id: source_next_page_id,
      mutations: &mutations,
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    })
    .unwrap();
    let batch = mutation_batch(
      hash_algorithm,
      source.owner_id,
      IndexMembershipOwnerClassV1::ValueStore,
      OrderedIndexRoleV1::Value,
      &inserted,
      IndexMembershipStateV1 { live: false, unindexable: false },
      IndexMembershipStateV1 { live: true, unindexable: false },
    );
    let namespace_root = vec![0xe2; hash_algorithm.hash_length()];
    let epoch = vec![0xe3; 16];
    let coverage =
      CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 };
    let summaries = [bootstrap.summary.clone()];
    let successor = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &absent_source.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage: coverage.clone(),
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &summaries,
      },
      &|| false,
    )
    .unwrap();
    let successor = decode_index_manifest(&successor.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(successor_body) = successor.details else {
      panic!("successor is a value-store manifest");
    };
    assert_eq!(successor_body.value_directory_root, bootstrap.summary.root_key.as_deref());
    assert_eq!(successor_body.next_page_id, bootstrap.summary.next_page_id);
    assert_eq!(successor_body.value_page_count, bootstrap.summary.page_count);
    assert_eq!(successor_body.value_document_count, 1);
    assert_eq!(successor_body.live_value_count, bootstrap.summary.live_count);
    assert_eq!(successor_body.value_tombstone_count, 0);
    assert_eq!(successor_body.live_canonical_value_bytes, bootstrap.summary.logical_bytes);

    let present_root = vec![0xe4; hash_algorithm.hash_length()];
    let present_source = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: source_generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        value_directory_root: Some(&present_root),
        next_page_id: source_next_page_id,
        value_page_count: 1,
        value_document_count: 1,
        live_value_count: 1,
        value_tombstone_count: 0,
        live_canonical_value_bytes: 1,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let error = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &present_source.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage,
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &summaries,
      },
      &|| false,
    )
    .unwrap_err();
    assert!(
      matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
    );
  }
}

#[test]
fn manifest_successor_applies_value_membership_and_exact_cow_aggregates() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "value-store");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(fixture_body) = &fixture.details else {
      panic!("fixture is a value-store manifest");
    };
    let source_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("source".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let source_record = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 2,
        source_value_ordinal: 0,
        record_revision_hash: &vec![0xa1; hash_algorithm.hash_length()],
        canonical_value: Some(&source_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let source_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Value,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 1,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_record],
    })
    .unwrap();
    let source_root = ordered_leaf_directory(hash_algorithm, &source_page);
    let source_page_summary = decode_ordered_page(&source_page.value, hash_algorithm).unwrap();
    let source_next_page_id = 10;
    let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        value_directory_root: Some(&source_root.key),
        next_page_id: source_next_page_id,
        value_page_count: 1,
        value_document_count: 1,
        live_value_count: 1,
        value_tombstone_count: 0,
        live_canonical_value_bytes: source_page_summary.logical_live_bytes,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let source = decode_index_manifest(&source_manifest.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(source_body) = &source.details else {
      panic!("source is a value-store manifest");
    };
    let revision = vec![0xa2; hash_algorithm.hash_length()];
    let inserted_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("inserted".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let inserted = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 7,
        source_value_ordinal: 0,
        record_revision_hash: &revision,
        canonical_value: Some(&inserted_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let successor_generation = source.generation.checked_add(1).unwrap();
    let summary = cow_summary(
      hash_algorithm,
      &source_page,
      &source_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&inserted),
    );
    let batch = mutation_batch(
      hash_algorithm,
      source.owner_id,
      IndexMembershipOwnerClassV1::ValueStore,
      OrderedIndexRoleV1::Value,
      &inserted,
      IndexMembershipStateV1 { live: false, unindexable: false },
      IndexMembershipStateV1 { live: true, unindexable: false },
    );
    let namespace_root = vec![0xd1; hash_algorithm.hash_length()];
    let epoch = vec![0xd2; 16];
    let summaries = [summary.clone()];
    let successor = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &summaries,
      },
      &|| false,
    )
    .unwrap();
    let successor = decode_index_manifest(&successor.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(successor_body) = successor.details else {
      panic!("successor is a value-store manifest");
    };
    assert_eq!(successor_body.value_directory_root, summary.root_key.as_deref());
    assert_eq!(successor_body.value_page_count, summary.page_count);
    assert_eq!(successor_body.value_document_count, 2);
    assert_eq!(successor_body.live_value_count, summary.live_count);
    assert_eq!(successor_body.value_tombstone_count, summary.tombstone_count);
    assert_eq!(successor_body.live_canonical_value_bytes, summary.logical_bytes);
    assert_eq!(successor_body.next_page_id, summary.next_page_id);
    assert_eq!(successor_body.document_state_directory_root, source_body.document_state_directory_root);
    assert_eq!(successor_body.unindexable_document_count, source_body.unindexable_document_count);

    let prior_document_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("prior".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let prior_document_record = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 7,
        source_value_ordinal: 0,
        record_revision_hash: &vec![0xa5; hash_algorithm.hash_length()],
        canonical_value: Some(&prior_document_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let contradictory_source_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Value,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 1,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_record, &prior_document_record],
    })
    .unwrap();
    let contradictory_source_root = ordered_leaf_directory(hash_algorithm, &contradictory_source_page);
    let contradictory_page = decode_ordered_page(&contradictory_source_page.value, hash_algorithm).unwrap();
    let contradictory_source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        value_directory_root: Some(&contradictory_source_root.key),
        next_page_id: source_next_page_id,
        value_page_count: 1,
        value_document_count: 2,
        live_value_count: 2,
        value_tombstone_count: 0,
        live_canonical_value_bytes: contradictory_page.logical_live_bytes,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let contradictory_summary = cow_summary(
      hash_algorithm,
      &contradictory_source_page,
      &contradictory_source_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&inserted),
    );
    let contradictory_batch = mutation_batch(
      hash_algorithm,
      source.owner_id,
      IndexMembershipOwnerClassV1::ValueStore,
      OrderedIndexRoleV1::Value,
      &inserted,
      IndexMembershipStateV1 { live: true, unindexable: false },
      IndexMembershipStateV1 { live: false, unindexable: false },
    );
    let error = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &contradictory_source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: None,
        mutations: contradictory_batch.records(),
        transitions: contradictory_batch.transitions(),
        role_summaries: &[contradictory_summary],
      },
      &|| false,
    )
    .unwrap_err();
    assert!(
      matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
    );

    let wrong_generation_summary = cow_summary(
      hash_algorithm,
      &source_page,
      &source_root,
      successor_generation.checked_add(1).unwrap(),
      source_next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&inserted),
    );
    let alternate_source_record = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 3,
        source_value_ordinal: 0,
        record_revision_hash: &vec![0xa3; hash_algorithm.hash_length()],
        canonical_value: Some(&source_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let alternate_source_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Value,
      owner_id: source.owner_id,
      generation: source.generation,
      page_id: 1,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&alternate_source_record],
    })
    .unwrap();
    let alternate_source_root = ordered_leaf_directory(hash_algorithm, &alternate_source_page);
    let wrong_source_summary = cow_summary(
      hash_algorithm,
      &alternate_source_page,
      &alternate_source_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&inserted),
    );
    let other_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("other".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let other_inserted = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 8,
        source_value_ordinal: 0,
        record_revision_hash: &vec![0xa4; hash_algorithm.hash_length()],
        canonical_value: Some(&other_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let wrong_mutation_summary = cow_summary(
      hash_algorithm,
      &source_page,
      &source_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&other_inserted),
    );
    assert_ne!(summary.mutation_commitment, wrong_mutation_summary.mutation_commitment);
    for (label, invalid_summary) in
      [("generation", wrong_generation_summary), ("source", wrong_source_summary), ("mutation", wrong_mutation_summary)]
    {
      let invalid_summaries = [invalid_summary];
      let result = synthesize_successor_index_manifest_v1(
        &IndexManifestSuccessorRequestV1 {
          hash_algorithm,
          source_manifest: &source_manifest.value,
          generation: successor_generation,
          parent_manifest_key: Some(source_body.scope_catalog_manifest),
          coverage: CoverageVersionV1 {
            source_namespace_root: &namespace_root,
            coverage_epoch_id: &epoch,
            coverage_publication_sequence: 9,
          },
          next_document_ordinal: None,
          mutations: batch.records(),
          transitions: batch.transitions(),
          role_summaries: &invalid_summaries,
        },
        &|| false,
      );
      let error = match result {
        Ok(_) => panic!("{label} summary unexpectedly matched the frozen batch"),
        Err(error) => error,
      };
      assert!(
        matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
      );
    }
  }
}

#[test]
fn manifest_successor_rejects_roles_borrowed_across_membership_transitions() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "value-store");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(fixture_body) = &fixture.details else {
      panic!("fixture is a value-store manifest");
    };
    let revision = vec![0xb1; hash_algorithm.hash_length()];
    let source_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("source".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let source_record = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 7,
        source_value_ordinal: 0,
        record_revision_hash: &revision,
        canonical_value: Some(&source_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let source_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Value,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 1,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_record],
    })
    .unwrap();
    let source_root = ordered_leaf_directory(hash_algorithm, &source_page);
    let source_page_summary = decode_ordered_page(&source_page.value, hash_algorithm).unwrap();
    let source_next_page_id = 10;
    let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        value_directory_root: Some(&source_root.key),
        next_page_id: source_next_page_id,
        value_page_count: 1,
        value_document_count: 1,
        live_value_count: 1,
        value_tombstone_count: 0,
        live_canonical_value_bytes: source_page_summary.logical_live_bytes,
        ..fixture_body.clone()
      }),
    })
    .unwrap();

    let inserted_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("inserted".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let inserted = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 8,
        source_value_ordinal: 0,
        record_revision_hash: &vec![0xb2; hash_algorithm.hash_length()],
        canonical_value: Some(&inserted_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let successor_generation = fixture.generation.checked_add(1).unwrap();
    let summary = cow_summary(
      hash_algorithm,
      &source_page,
      &source_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&inserted),
    );

    let hard_limit = 1_000_000;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, hard_limit, 1, 299_999).unwrap());
    let mut coordinator = IndexCoordinatorV1::new(
      [0x43; 16],
      hash_algorithm,
      memory,
      IndexCoordinatorOptionsV1::new(500_000, 1, 1_000, 500_000).unwrap(),
      1_000,
    )
    .unwrap();
    coordinator
      .admit_group(
        IndexMutationGroupRequestV1 {
          transition: IndexMembershipTransitionRequestV1 {
            owner_id: fixture.owner_id,
            owner_class: IndexMembershipOwnerClassV1::ValueStore,
            publication_sequence: 9,
            operation_id: [0x61; 16],
            document_ordinal: 7,
            before: IndexMembershipStateV1 { live: true, unindexable: false },
            after: IndexMembershipStateV1 { live: false, unindexable: false },
          },
          mutations: &[],
        },
        1_001,
      )
      .unwrap();
    let mutations = [IndexGroupMutationRequestV1 {
      operation: IndexMutationOperationV1::Upsert,
      mutation: IndexMutationRequestV1 {
        index_id: fixture.owner_id,
        role: OrderedIndexRoleV1::Value,
        publication_sequence: 9,
        operation_id: [0x62; 16],
        encoded_record: &inserted,
      },
    }];
    coordinator
      .admit_group(
        IndexMutationGroupRequestV1 {
          transition: IndexMembershipTransitionRequestV1 {
            owner_id: fixture.owner_id,
            owner_class: IndexMembershipOwnerClassV1::ValueStore,
            publication_sequence: 9,
            operation_id: [0x62; 16],
            document_ordinal: 8,
            before: IndexMembershipStateV1 { live: false, unindexable: false },
            after: IndexMembershipStateV1 { live: true, unindexable: false },
          },
          mutations: &mutations,
        },
        1_002,
      )
      .unwrap();
    let batch = coordinator.begin_flush(1_003, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
    let source = decode_index_manifest(&source_manifest.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(source_body) = &source.details else {
      panic!("source is a value-store manifest");
    };
    let namespace_root = vec![0xd3; hash_algorithm.hash_length()];
    let epoch = vec![0xd4; 16];
    let error = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &[summary],
      },
      &|| false,
    )
    .unwrap_err();
    assert!(
      matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
    );
  }
}

#[test]
fn manifest_successor_moves_value_membership_to_state_with_one_page_id_chain() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "value-store");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(fixture_body) = &fixture.details else {
      panic!("fixture is a value-store manifest");
    };
    let source_revision = vec![0xc1; hash_algorithm.hash_length()];
    let successor_revision = vec![0xc2; hash_algorithm.hash_length()];
    let source_value =
      encode_canonical_value(&CanonicalConfigValueV1::String("source".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let source_value_record = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: false,
        document_ordinal: 7,
        source_value_ordinal: 0,
        record_revision_hash: &source_revision,
        canonical_value: Some(&source_value),
      },
      hash_algorithm,
    )
    .unwrap();
    let source_value_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Value,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 1,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_value_record],
    })
    .unwrap();
    let source_value_root = ordered_leaf_directory(hash_algorithm, &source_value_page);
    let source_value_page_summary = decode_ordered_page(&source_value_page.value, hash_algorithm).unwrap();

    let evidence =
      encode_canonical_value(&CanonicalConfigValueV1::String("value could not be normalized".to_string()), CanonicalValueBounds::CONFIG)
        .unwrap();
    let source_state_record = encode_document_state_record(
      &DocumentStateRecordV1 {
        tombstone: true,
        stage: 1,
        reason: 0x0001,
        document_ordinal: 7,
        record_revision_hash: &source_revision,
        observed_value_count: 1,
        observed_canonical_bytes: source_value.len() as u64,
        observed_work_units: 1,
        dependency_ordinal: 0,
        evidence: &evidence,
      },
      DocumentStateOwnerV1::ValueStore,
      hash_algorithm,
    )
    .unwrap();
    let source_state_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::ValueDocumentState,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 2,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_state_record],
    })
    .unwrap();
    let source_state_root = ordered_leaf_directory(hash_algorithm, &source_state_page);
    let source_next_page_id = 10;
    let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        value_directory_root: Some(&source_value_root.key),
        document_state_directory_root: Some(&source_state_root.key),
        next_page_id: source_next_page_id,
        value_page_count: 1,
        state_page_count: 1,
        value_document_count: 1,
        unindexable_document_count: 0,
        live_value_count: 1,
        value_tombstone_count: 0,
        state_tombstone_count: 1,
        live_canonical_value_bytes: source_value_page_summary.logical_live_bytes,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let source_manifest_before = source_manifest.value.clone();
    let source = decode_index_manifest(&source_manifest.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(source_body) = &source.details else {
      panic!("source is a value-store manifest");
    };

    let tombstone_value = encode_canonical_value_record(
      &CanonicalValueRecordV1 {
        tombstone: true,
        document_ordinal: 7,
        source_value_ordinal: 0,
        record_revision_hash: &successor_revision,
        canonical_value: None,
      },
      hash_algorithm,
    )
    .unwrap();
    let live_state = encode_document_state_record(
      &DocumentStateRecordV1 {
        tombstone: false,
        stage: 1,
        reason: 0x0001,
        document_ordinal: 7,
        record_revision_hash: &successor_revision,
        observed_value_count: 1,
        observed_canonical_bytes: source_value.len() as u64,
        observed_work_units: 2,
        dependency_ordinal: 0,
        evidence: &evidence,
      },
      DocumentStateOwnerV1::ValueStore,
      hash_algorithm,
    )
    .unwrap();
    let successor_generation = source.generation.checked_add(1).unwrap();
    let value_summary = cow_summary(
      hash_algorithm,
      &source_value_page,
      &source_value_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::TombstoneExisting(&tombstone_value),
    );
    let state_summary = cow_summary(
      hash_algorithm,
      &source_state_page,
      &source_state_root,
      successor_generation,
      value_summary.next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&live_state),
    );
    let wrong_state_summary = cow_summary(
      hash_algorithm,
      &source_state_page,
      &source_state_root,
      successor_generation,
      value_summary.next_page_id.checked_add(1).unwrap(),
      OrderedPageMutationKindV1::UpsertLive(&live_state),
    );

    let hard_limit = 1_000_000;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, hard_limit, 1, 299_999).unwrap());
    let mut coordinator = IndexCoordinatorV1::new(
      [0x44; 16],
      hash_algorithm,
      memory,
      IndexCoordinatorOptionsV1::new(500_000, 1, 1_000, 500_000).unwrap(),
      1_000,
    )
    .unwrap();
    let mutations = [
      IndexGroupMutationRequestV1 {
        operation: IndexMutationOperationV1::Upsert,
        mutation: IndexMutationRequestV1 {
          index_id: source.owner_id,
          role: OrderedIndexRoleV1::Value,
          publication_sequence: 9,
          operation_id: [0x63; 16],
          encoded_record: &tombstone_value,
        },
      },
      IndexGroupMutationRequestV1 {
        operation: IndexMutationOperationV1::Upsert,
        mutation: IndexMutationRequestV1 {
          index_id: source.owner_id,
          role: OrderedIndexRoleV1::ValueDocumentState,
          publication_sequence: 9,
          operation_id: [0x63; 16],
          encoded_record: &live_state,
        },
      },
    ];
    coordinator
      .admit_group(
        IndexMutationGroupRequestV1 {
          transition: IndexMembershipTransitionRequestV1 {
            owner_id: source.owner_id,
            owner_class: IndexMembershipOwnerClassV1::ValueStore,
            publication_sequence: 9,
            operation_id: [0x63; 16],
            document_ordinal: 7,
            before: IndexMembershipStateV1 { live: true, unindexable: false },
            after: IndexMembershipStateV1 { live: false, unindexable: true },
          },
          mutations: &mutations,
        },
        1_001,
      )
      .unwrap();
    let batch = coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
    let namespace_root = vec![0xd5; hash_algorithm.hash_length()];
    let epoch = vec![0xd6; 16];
    let summaries = [value_summary.clone(), state_summary.clone()];
    let successor = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &summaries,
      },
      &|| false,
    )
    .unwrap();
    let successor = decode_index_manifest(&successor.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(successor_body) = successor.details else {
      panic!("successor is a value-store manifest");
    };
    assert_eq!(value_summary.initial_next_page_id, source_next_page_id);
    assert_eq!(state_summary.initial_next_page_id, value_summary.next_page_id);
    assert_eq!(successor_body.next_page_id, state_summary.next_page_id);
    assert_eq!(successor_body.value_directory_root, value_summary.root_key.as_deref());
    assert_eq!(successor_body.document_state_directory_root, state_summary.root_key.as_deref());
    assert_eq!(successor_body.value_page_count, value_summary.page_count);
    assert_eq!(successor_body.state_page_count, state_summary.page_count);
    assert_eq!(successor_body.value_document_count, 0);
    assert_eq!(successor_body.unindexable_document_count, 1);
    assert_eq!(successor_body.live_value_count, 0);
    assert_eq!(successor_body.value_tombstone_count, 1);
    assert_eq!(successor_body.state_tombstone_count, 0);
    assert_eq!(successor_body.live_canonical_value_bytes, 0);
    assert_eq!(source_manifest.value, source_manifest_before, "successor synthesis mutated its immutable source manifest");

    let wrong_summaries = [value_summary.clone(), wrong_state_summary];
    let error = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.scope_catalog_manifest),
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &wrong_summaries,
      },
      &|| false,
    )
    .unwrap_err();
    assert!(
      matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
    );
    let invalid_summary_sets = vec![
      vec![value_summary.clone()],
      vec![state_summary.clone(), value_summary.clone()],
      vec![value_summary.clone(), value_summary.clone(), state_summary.clone()],
      Vec::new(),
    ];
    for invalid_summaries in invalid_summary_sets {
      let error = synthesize_successor_index_manifest_v1(
        &IndexManifestSuccessorRequestV1 {
          hash_algorithm,
          source_manifest: &source_manifest.value,
          generation: successor_generation,
          parent_manifest_key: Some(source_body.scope_catalog_manifest),
          coverage: CoverageVersionV1 {
            source_namespace_root: &namespace_root,
            coverage_epoch_id: &epoch,
            coverage_publication_sequence: 9,
          },
          next_document_ordinal: None,
          mutations: batch.records(),
          transitions: batch.transitions(),
          role_summaries: &invalid_summaries,
        },
        &|| false,
      )
      .unwrap_err();
      assert!(
        matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
      );
    }
  }
}

#[test]
fn manifest_successor_removes_scope_reverse_and_retains_ordinal_tombstone() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "scope-catalog");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(fixture_body) = &fixture.details else {
      panic!("fixture is a scope-catalog manifest");
    };
    let path = "/docs/readme.md";
    let file_key = digest_parts(hash_algorithm, &[b"file:", path.as_bytes()]);
    let source_revision = vec![0xd1; hash_algorithm.hash_length()];
    let successor_revision = vec![0xd2; hash_algorithm.hash_length()];
    let source_ordinal_record = encode_scope_document_record(
      &ScopeDocumentRecordV1 { tombstone: false, document_ordinal: 7, file_key: &file_key, record_revision_hash: &source_revision, path },
      hash_algorithm,
    )
    .unwrap();
    let source_reverse_record =
      encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: 7, file_key: &file_key }, hash_algorithm).unwrap();
    let source_ordinal_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::ScopeOrdinal,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 0,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_ordinal_record],
    })
    .unwrap();
    let source_reverse_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::ScopeReverse,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 0,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_reverse_record],
    })
    .unwrap();
    let source_ordinal_root = ordered_leaf_directory(hash_algorithm, &source_ordinal_page);
    let source_reverse_root = ordered_leaf_directory(hash_algorithm, &source_reverse_page);
    let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 {
        next_document_ordinal: 8,
        ordinal_directory_root: Some(&source_ordinal_root.key),
        reverse_directory_root: Some(&source_reverse_root.key),
        live_document_count: 1,
        retained_tombstone_count: 0,
        ordinal_page_count: 1,
        reverse_page_count: 1,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let source_before = source_manifest.value.clone();
    let source = decode_index_manifest(&source_manifest.value, hash_algorithm).unwrap();

    let tombstone_ordinal = encode_scope_document_record(
      &ScopeDocumentRecordV1 { tombstone: true, document_ordinal: 7, file_key: &file_key, record_revision_hash: &successor_revision, path },
      hash_algorithm,
    )
    .unwrap();
    let successor_generation = source.generation.checked_add(1).unwrap();
    let ordinal_summary = cow_summary(
      hash_algorithm,
      &source_ordinal_page,
      &source_ordinal_root,
      successor_generation,
      0,
      OrderedPageMutationKindV1::TombstoneExisting(&tombstone_ordinal),
    );
    let reverse_summary = cow_summary(
      hash_algorithm,
      &source_reverse_page,
      &source_reverse_root,
      successor_generation,
      0,
      OrderedPageMutationKindV1::RemoveExisting(&source_reverse_record),
    );
    assert!(reverse_summary.root_key.is_none(), "removing the only reverse row must retire its root");

    let hard_limit = 1_000_000;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, hard_limit, 1, 299_999).unwrap());
    let mut coordinator = IndexCoordinatorV1::new(
      [0x45; 16],
      hash_algorithm,
      memory,
      IndexCoordinatorOptionsV1::new(500_000, 1, 1_000, 500_000).unwrap(),
      1_000,
    )
    .unwrap();
    let mutations = [
      IndexGroupMutationRequestV1 {
        operation: IndexMutationOperationV1::Upsert,
        mutation: IndexMutationRequestV1 {
          index_id: source.owner_id,
          role: OrderedIndexRoleV1::ScopeOrdinal,
          publication_sequence: 9,
          operation_id: [0x64; 16],
          encoded_record: &tombstone_ordinal,
        },
      },
      IndexGroupMutationRequestV1 {
        operation: IndexMutationOperationV1::RemoveExisting,
        mutation: IndexMutationRequestV1 {
          index_id: source.owner_id,
          role: OrderedIndexRoleV1::ScopeReverse,
          publication_sequence: 9,
          operation_id: [0x64; 16],
          encoded_record: &source_reverse_record,
        },
      },
    ];
    coordinator
      .admit_group(
        IndexMutationGroupRequestV1 {
          transition: IndexMembershipTransitionRequestV1 {
            owner_id: source.owner_id,
            owner_class: IndexMembershipOwnerClassV1::ScopeCatalog,
            publication_sequence: 9,
            operation_id: [0x64; 16],
            document_ordinal: 7,
            before: IndexMembershipStateV1 { live: true, unindexable: false },
            after: IndexMembershipStateV1 { live: false, unindexable: false },
          },
          mutations: &mutations,
        },
        1_001,
      )
      .unwrap();
    let batch = coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
    let namespace_root = vec![0xd7; hash_algorithm.hash_length()];
    let epoch = vec![0xd8; 16];
    let summaries = [ordinal_summary.clone(), reverse_summary.clone()];
    let successor = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: None,
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: Some(8),
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &summaries,
      },
      &|| false,
    )
    .unwrap();
    let successor = decode_index_manifest(&successor.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(successor_body) = successor.details else {
      panic!("successor is a scope-catalog manifest");
    };
    assert_eq!(successor_body.next_document_ordinal, 8);
    assert_eq!(successor_body.live_document_count, 0);
    assert_eq!(successor_body.ordinal_directory_root, ordinal_summary.root_key.as_deref());
    assert_eq!(successor_body.reverse_directory_root, None);
    assert_eq!(successor_body.retained_tombstone_count, 1);
    assert_eq!(successor_body.ordinal_page_count, 1);
    assert_eq!(successor_body.reverse_page_count, 0);
    assert_eq!(source_manifest.value, source_before, "successor synthesis mutated its immutable source manifest");

    for (parent_manifest_key, next_document_ordinal) in [(Some(namespace_root.as_slice()), Some(8)), (None, None), (None, Some(7))] {
      let error = synthesize_successor_index_manifest_v1(
        &IndexManifestSuccessorRequestV1 {
          hash_algorithm,
          source_manifest: &source_manifest.value,
          generation: successor_generation,
          parent_manifest_key,
          coverage: CoverageVersionV1 {
            source_namespace_root: &namespace_root,
            coverage_epoch_id: &epoch,
            coverage_publication_sequence: 9,
          },
          next_document_ordinal,
          mutations: batch.records(),
          transitions: batch.transitions(),
          role_summaries: &summaries,
        },
        &|| false,
      )
      .unwrap_err();
      assert!(
        matches!(error, IndexBatchApplicationErrorV1::Malformed(error) if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch)
      );
    }
  }
}

#[test]
fn manifest_successor_moves_field_membership_to_state_with_exact_page_ids() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "field-index");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(fixture_body) = &fixture.details else {
      panic!("fixture is a field-index manifest");
    };
    let posting_key = 7u64.to_le_bytes();
    let source_posting_record = encode_posting_record(&PostingRecordV1 {
      tombstone: false,
      coordinate: 7,
      document_ordinal: 7,
      source_value_ordinal: 0,
      expansion_ordinal: 0,
      posting_key: &posting_key,
    })
    .unwrap();
    let source_posting_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Posting,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 1,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_posting_record],
    })
    .unwrap();
    let source_posting_root = ordered_leaf_directory(hash_algorithm, &source_posting_page);
    let source_posting_summary = decode_ordered_page(&source_posting_page.value, hash_algorithm).unwrap();
    let source_revision = vec![0xe1; hash_algorithm.hash_length()];
    let successor_revision = vec![0xe2; hash_algorithm.hash_length()];
    let evidence = encode_canonical_value(
      &CanonicalConfigValueV1::String("converter could not emit a posting".to_string()),
      CanonicalValueBounds::CONFIG,
    )
    .unwrap();
    let source_state_record = encode_document_state_record(
      &DocumentStateRecordV1 {
        tombstone: true,
        stage: 5,
        reason: 0x0009,
        document_ordinal: 7,
        record_revision_hash: &source_revision,
        observed_value_count: 1,
        observed_canonical_bytes: source_posting_record.len() as u64,
        observed_work_units: 1,
        dependency_ordinal: 0,
        evidence: &evidence,
      },
      DocumentStateOwnerV1::FieldIndex,
      hash_algorithm,
    )
    .unwrap();
    let source_state_page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::IndexDocumentState,
      owner_id: fixture.owner_id,
      generation: fixture.generation,
      page_id: 2,
      previous_page_id: 0,
      next_page_id: 0,
      records: &[&source_state_record],
    })
    .unwrap();
    let source_state_root = ordered_leaf_directory(hash_algorithm, &source_state_page);
    let source_next_page_id = 10;
    let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
        posting_directory_root: Some(&source_posting_root.key),
        document_state_directory_root: Some(&source_state_root.key),
        first_page_id: 1,
        last_page_id: 1,
        next_page_id: source_next_page_id,
        posting_page_count: 1,
        state_page_count: 1,
        live_posting_count: 1,
        posting_tombstone_count: 0,
        posting_document_count: 1,
        unindexable_document_count: 0,
        state_tombstone_count: 1,
        live_canonical_posting_bytes: source_posting_summary.logical_live_bytes,
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let source_before = source_manifest.value.clone();
    let source = decode_index_manifest(&source_manifest.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(source_body) = &source.details else {
      panic!("source is a field-index manifest");
    };

    let tombstone_posting = encode_posting_record(&PostingRecordV1 {
      tombstone: true,
      coordinate: 7,
      document_ordinal: 7,
      source_value_ordinal: 0,
      expansion_ordinal: 0,
      posting_key: &posting_key,
    })
    .unwrap();
    let live_state = encode_document_state_record(
      &DocumentStateRecordV1 {
        tombstone: false,
        stage: 5,
        reason: 0x0009,
        document_ordinal: 7,
        record_revision_hash: &successor_revision,
        observed_value_count: 1,
        observed_canonical_bytes: source_posting_record.len() as u64,
        observed_work_units: 2,
        dependency_ordinal: 0,
        evidence: &evidence,
      },
      DocumentStateOwnerV1::FieldIndex,
      hash_algorithm,
    )
    .unwrap();
    let successor_generation = source.generation.checked_add(1).unwrap();
    let posting_summary = cow_summary(
      hash_algorithm,
      &source_posting_page,
      &source_posting_root,
      successor_generation,
      source_next_page_id,
      OrderedPageMutationKindV1::TombstoneExisting(&tombstone_posting),
    );
    let state_summary = cow_summary(
      hash_algorithm,
      &source_state_page,
      &source_state_root,
      successor_generation,
      posting_summary.next_page_id,
      OrderedPageMutationKindV1::UpsertLive(&live_state),
    );

    let hard_limit = 1_000_000;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, hard_limit, 1, 299_999).unwrap());
    let mut coordinator = IndexCoordinatorV1::new(
      [0x46; 16],
      hash_algorithm,
      memory,
      IndexCoordinatorOptionsV1::new(500_000, 1, 1_000, 500_000).unwrap(),
      1_000,
    )
    .unwrap();
    let mutations = [
      IndexGroupMutationRequestV1 {
        operation: IndexMutationOperationV1::Upsert,
        mutation: IndexMutationRequestV1 {
          index_id: source.owner_id,
          role: OrderedIndexRoleV1::Posting,
          publication_sequence: 9,
          operation_id: [0x65; 16],
          encoded_record: &tombstone_posting,
        },
      },
      IndexGroupMutationRequestV1 {
        operation: IndexMutationOperationV1::Upsert,
        mutation: IndexMutationRequestV1 {
          index_id: source.owner_id,
          role: OrderedIndexRoleV1::IndexDocumentState,
          publication_sequence: 9,
          operation_id: [0x65; 16],
          encoded_record: &live_state,
        },
      },
    ];
    coordinator
      .admit_group(
        IndexMutationGroupRequestV1 {
          transition: IndexMembershipTransitionRequestV1 {
            owner_id: source.owner_id,
            owner_class: IndexMembershipOwnerClassV1::FieldIndex,
            publication_sequence: 9,
            operation_id: [0x65; 16],
            document_ordinal: 7,
            before: IndexMembershipStateV1 { live: true, unindexable: false },
            after: IndexMembershipStateV1 { live: false, unindexable: true },
          },
          mutations: &mutations,
        },
        1_001,
      )
      .unwrap();
    let batch = coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
    let namespace_root = vec![0xd9; hash_algorithm.hash_length()];
    let epoch = vec![0xda; 16];
    let summaries = [posting_summary.clone(), state_summary.clone()];
    let successor = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm,
        source_manifest: &source_manifest.value,
        generation: successor_generation,
        parent_manifest_key: Some(source_body.value_store_manifest),
        coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
        next_document_ordinal: None,
        mutations: batch.records(),
        transitions: batch.transitions(),
        role_summaries: &summaries,
      },
      &|| false,
    )
    .unwrap();
    let successor = decode_index_manifest(&successor.value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(successor_body) = successor.details else {
      panic!("successor is a field-index manifest");
    };
    assert_eq!(successor_body.posting_directory_root, posting_summary.root_key.as_deref());
    assert_eq!(successor_body.document_state_directory_root, state_summary.root_key.as_deref());
    assert_eq!(successor_body.first_page_id, posting_summary.minimum_page_id);
    assert_eq!(successor_body.last_page_id, posting_summary.maximum_page_id);
    assert_eq!(successor_body.next_page_id, state_summary.next_page_id);
    assert_eq!(successor_body.posting_page_count, posting_summary.page_count);
    assert_eq!(successor_body.state_page_count, state_summary.page_count);
    assert_eq!(successor_body.live_posting_count, 0);
    assert_eq!(successor_body.posting_tombstone_count, 1);
    assert_eq!(successor_body.posting_document_count, 0);
    assert_eq!(successor_body.unindexable_document_count, 1);
    assert_eq!(successor_body.state_tombstone_count, 0);
    assert_eq!(successor_body.live_canonical_posting_bytes, 0);
    assert_eq!(source_manifest.value, source_before, "successor synthesis mutated its immutable source manifest");
  }
}

#[test]
fn manifest_successor_rejects_invalid_authority_boundaries_before_encoding() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let source = fixture_manifest(hash_algorithm, "value-store");
    let decoded = decode_index_manifest(&source, hash_algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(body) = &decoded.details else {
      panic!("fixture is a value-store manifest");
    };
    let batch = transition_only_batch(
      hash_algorithm,
      decoded.owner_id,
      IndexMembershipOwnerClassV1::ValueStore,
      IndexMembershipStateV1 { live: false, unindexable: false },
      IndexMembershipStateV1 { live: false, unindexable: false },
    );
    let namespace_root = vec![0xeb; hash_algorithm.hash_length()];
    let epoch = vec![0xec; 16];
    let base = IndexManifestSuccessorRequestV1 {
      hash_algorithm,
      source_manifest: &source,
      generation: decoded.generation.checked_add(1).unwrap(),
      parent_manifest_key: Some(body.scope_catalog_manifest),
      coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
      next_document_ordinal: None,
      mutations: batch.records(),
      transitions: batch.transitions(),
      role_summaries: &[],
    };
    assert_eq!(synthesize_successor_index_manifest_v1(&base, &|| true).unwrap_err(), IndexBatchApplicationErrorV1::Cancelled);
    let cancellation_checks = Cell::new(0u8);
    assert_eq!(
      synthesize_successor_index_manifest_v1(&base, &|| {
        let observed = cancellation_checks.get();
        cancellation_checks.set(observed + 1);
        observed >= 1
      })
      .unwrap_err(),
      IndexBatchApplicationErrorV1::Cancelled
    );
    assert_eq!(cancellation_checks.get(), 2, "successor synthesis did not honor its post-validation cancellation boundary");

    let mut invalid = base.clone();
    invalid.generation = decoded.generation;
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
    invalid = base.clone();
    invalid.parent_manifest_key = None;
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
    invalid = base.clone();
    invalid.next_document_ordinal = Some(8);
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
    invalid = base.clone();
    invalid.coverage.source_namespace_root = &[];
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
    invalid = base.clone();
    invalid.coverage.coverage_epoch_id = &namespace_root;
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
    invalid = base.clone();
    invalid.coverage.coverage_publication_sequence = 0;
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
    invalid = base.clone();
    invalid.coverage.coverage_publication_sequence = 8;
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));

    let nvt = fixture_manifest(hash_algorithm, "field-nvt");
    invalid = base.clone();
    invalid.source_manifest = &nvt;
    assert!(matches!(
      synthesize_successor_index_manifest_v1(&invalid, &|| false).unwrap_err(),
      IndexBatchApplicationErrorV1::Malformed(error)
        if error.class() == aeordb::engine::v4::reader::MalformedInputClass::CrossRecordClosureMismatch
    ));
  }
}

#[test]
fn sparse_lookup_loads_only_the_selected_path_and_required_posting_successor() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let owner_id = owner(hash_algorithm);
    let first = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
    let second = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 3);
    let third = posting_page(hash_algorithm, &owner_id, 30, 3, 2, 0);
    let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &second, &third]);
    let mut source = CountingSource::default();
    for artifact in [&root, &first, &second, &third] {
      source.insert(artifact);
    }
    let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

    let loaded = load_ordered_page_path_v1(
      &OrderedPagePathLookupRequestV1 {
        hash_algorithm,
        root_key: &root.key,
        owner_id: &owner_id,
        role: OrderedIndexRoleV1::Posting,
        order_key: &posting_order_key(hash_algorithm, 10),
        load_posting_successor: true,
        limits: OrderedPagePathLookupLimitsV1::default(),
      },
      &overlay,
      &mut source,
      &|| false,
    )
    .unwrap();

    assert_eq!(loaded.directory_count(), 1);
    assert_eq!(decode_ordered_page(loaded.page(), hash_algorithm).unwrap().page_id, 1);
    assert_eq!(decode_ordered_page(loaded.next_posting_page().unwrap(), hash_algorithm).unwrap().page_id, 2);
    assert_eq!(loaded.next_directory_count(), 1);
    assert_eq!(source.reads, vec![root.key.clone(), first.key.clone(), second.key.clone()]);
    assert!(!source.reads.contains(&third.key));

    source.reads.clear();
    let without_successor = load_ordered_page_path_v1(
      &OrderedPagePathLookupRequestV1 {
        hash_algorithm,
        root_key: &root.key,
        owner_id: &owner_id,
        role: OrderedIndexRoleV1::Posting,
        order_key: &posting_order_key(hash_algorithm, 10),
        load_posting_successor: false,
        limits: OrderedPagePathLookupLimitsV1::default(),
      },
      &overlay,
      &mut source,
      &|| false,
    )
    .unwrap();
    assert!(without_successor.next_posting_page().is_none());
    assert_eq!(source.reads, vec![root.key.clone(), first.key.clone()]);
  }
}

#[test]
fn sparse_lookup_reads_successor_artifacts_from_the_overlay_without_source_io() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
  let second = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&first, &second]);
  let mut overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();
  for artifact in [first, second, root.clone()] {
    assert!(overlay.insert(artifact).unwrap());
  }
  let mut source = CountingSource::default();

  let loaded = load_ordered_page_path_v1(
    &OrderedPagePathLookupRequestV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      order_key: &posting_order_key(hash_algorithm, 10),
      load_posting_successor: true,
      limits: OrderedPagePathLookupLimitsV1::default(),
    },
    &overlay,
    &mut source,
    &|| false,
  )
  .unwrap();

  assert_eq!(decode_ordered_page(loaded.page(), hash_algorithm).unwrap().page_id, 1);
  assert_eq!(decode_ordered_page(loaded.next_posting_page().unwrap(), hash_algorithm).unwrap().page_id, 2);
  assert!(source.reads.is_empty());
  assert_eq!(overlay.artifact_count(), 3);
  assert_eq!(overlay.prepared_artifacts().count(), 3);
}

#[test]
fn sparse_lookup_finds_a_posting_successor_across_internal_directory_branches() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let first = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
  let second = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 0);
  let left = leaf_directory(hash_algorithm, &owner_id, &[&first]);
  let right = leaf_directory(hash_algorithm, &owner_id, &[&second]);
  let root = internal_directory(hash_algorithm, &owner_id, &[(&left, 0), (&right, 0)]);
  let mut source = CountingSource::default();
  for artifact in [&root, &left, &right, &first, &second] {
    source.insert(artifact);
  }
  let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

  let loaded = load_ordered_page_path_v1(
    &OrderedPagePathLookupRequestV1 {
      hash_algorithm,
      root_key: &root.key,
      owner_id: &owner_id,
      role: OrderedIndexRoleV1::Posting,
      order_key: &posting_order_key(hash_algorithm, 10),
      load_posting_successor: true,
      limits: OrderedPagePathLookupLimitsV1::default(),
    },
    &overlay,
    &mut source,
    &|| false,
  )
  .unwrap();

  assert_eq!(loaded.directory_count(), 2);
  assert_eq!(loaded.next_directory_count(), 2);
  assert_eq!(decode_ordered_page(loaded.next_posting_page().unwrap(), hash_algorithm).unwrap().page_id, 2);
  assert_eq!(source.reads, vec![root.key.clone(), left.key.clone(), first.key.clone(), right.key.clone(), second.key.clone()]);
}

#[test]
fn sparse_lookup_and_overlay_fail_closed_on_missing_pressure_corruption_cancellation_and_caps() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let page = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 0);
  let root = leaf_directory(hash_algorithm, &owner_id, &[&page]);
  let request = OrderedPagePathLookupRequestV1 {
    hash_algorithm,
    root_key: &root.key,
    owner_id: &owner_id,
    role: OrderedIndexRoleV1::Posting,
    order_key: &posting_order_key(hash_algorithm, 10),
    load_posting_successor: true,
    limits: OrderedPagePathLookupLimitsV1::default(),
  };
  let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

  let missing = load_ordered_page_path_v1(&request, &overlay, &mut CountingSource::default(), &|| false).unwrap_err();
  assert_eq!(missing.code(), "index_batch_artifact_missing");

  let mut pressure =
    CountingSource { failure: Some(IndexBatchArtifactReadErrorV1::ResourcePressure("budget".to_string())), ..Default::default() };
  assert_eq!(load_ordered_page_path_v1(&request, &overlay, &mut pressure, &|| false).unwrap_err().code(), "index_batch_source_pressure");

  let mut corrupt = CountingSource::default();
  corrupt.values.insert(root.key.clone(), page.value.clone());
  assert!(matches!(
    load_ordered_page_path_v1(&request, &overlay, &mut corrupt, &|| false).unwrap_err(),
    IndexBatchApplicationErrorV1::Malformed(_)
  ));

  let mismatched_parent = parent_directory(hash_algorithm, &owner_id, &root, 1);
  let mut mismatched_source = CountingSource::default();
  for artifact in [&mismatched_parent, &root, &page] {
    mismatched_source.insert(artifact);
  }
  let mismatched_request = OrderedPagePathLookupRequestV1 { root_key: &mismatched_parent.key, ..request };
  assert_eq!(
    load_ordered_page_path_v1(&mismatched_request, &overlay, &mut mismatched_source, &|| false).unwrap_err().code(),
    "index_batch_path_closure"
  );
  assert_eq!(
    load_ordered_page_path_v1(&request, &overlay, &mut CountingSource::default(), &|| true).unwrap_err().code(),
    "index_batch_cancelled"
  );

  let limits = IndexBatchArtifactOverlayLimitsV1::new(1, 4 * 1_024 * 1_024).unwrap();
  let mut bounded = SparseIndexArtifactOverlayV1::new(hash_algorithm, limits).unwrap();
  assert!(bounded.insert(page).unwrap());
  assert_eq!(bounded.insert(root).unwrap_err().code(), "index_batch_overlay_count");
  assert_eq!(bounded.artifact_count(), 1);
}

#[test]
fn sparse_lookup_rejects_source_classes_link_corruption_depth_and_byte_limits() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let terminal = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 0);
  let unexpected = posting_page(hash_algorithm, &owner_id, 20, 2, 1, 0);
  let terminal_root = leaf_directory(hash_algorithm, &owner_id, &[&terminal, &unexpected]);
  let order_key = posting_order_key(hash_algorithm, 10);
  let request = OrderedPagePathLookupRequestV1 {
    hash_algorithm,
    root_key: &terminal_root.key,
    owner_id: &owner_id,
    role: OrderedIndexRoleV1::Posting,
    order_key: &order_key,
    load_posting_successor: true,
    limits: OrderedPagePathLookupLimitsV1::default(),
  };
  let overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();

  for (failure, code) in [
    (IndexBatchArtifactReadErrorV1::Cancelled, "index_batch_cancelled"),
    (IndexBatchArtifactReadErrorV1::Operational("disk".to_string()), "index_batch_source_operational"),
  ] {
    let mut source = CountingSource { failure: Some(failure), ..Default::default() };
    assert_eq!(load_ordered_page_path_v1(&request, &overlay, &mut source, &|| false).unwrap_err().code(), code);
  }

  let mut terminal_source = CountingSource::default();
  for artifact in [&terminal_root, &terminal, &unexpected] {
    terminal_source.insert(artifact);
  }
  assert_eq!(
    load_ordered_page_path_v1(&request, &overlay, &mut terminal_source, &|| false).unwrap_err().code(),
    "index_batch_path_closure"
  );

  let linked = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 2);
  let linked_leaf = leaf_directory(hash_algorithm, &owner_id, &[&linked]);
  let linked_root = parent_directory(hash_algorithm, &owner_id, &linked_leaf, 0);
  let mut linked_source = CountingSource::default();
  for artifact in [&linked_root, &linked_leaf, &linked] {
    linked_source.insert(artifact);
  }
  let shallow = OrderedPagePathLookupRequestV1 {
    root_key: &linked_root.key,
    limits: OrderedPagePathLookupLimitsV1::new(1, INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1).unwrap(),
    ..request
  };
  assert_eq!(load_ordered_page_path_v1(&shallow, &overlay, &mut linked_source, &|| false).unwrap_err().code(), "index_batch_path_depth");

  let tiny_input = OrderedPagePathLookupRequestV1 { limits: OrderedPagePathLookupLimitsV1::new(16, 1).unwrap(), ..request };
  let mut tiny_source = CountingSource::default();
  tiny_source.insert(&terminal_root);
  assert_eq!(
    load_ordered_page_path_v1(&tiny_input, &overlay, &mut tiny_source, &|| false).unwrap_err().code(),
    "index_batch_source_pressure"
  );
}

#[test]
fn sparse_overlay_validates_limits_identity_idempotence_and_retained_bytes() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = owner(hash_algorithm);
  let page = posting_page(hash_algorithm, &owner_id, 10, 1, 0, 0);
  assert_eq!(IndexBatchArtifactOverlayLimitsV1::new(0, 1).unwrap_err().code(), "index_batch_invalid_limits");
  assert_eq!(OrderedPagePathLookupLimitsV1::new(0, 1).unwrap_err().code(), "index_batch_invalid_limits");

  let too_small = IndexBatchArtifactOverlayLimitsV1::new(1, page.value.len()).unwrap();
  let mut bounded = SparseIndexArtifactOverlayV1::new(hash_algorithm, too_small).unwrap();
  assert_eq!(bounded.insert(page.clone()).unwrap_err().code(), "index_batch_overlay_bytes");
  assert_eq!(bounded.artifact_count(), 0);

  let mut overlay = SparseIndexArtifactOverlayV1::new(hash_algorithm, IndexBatchArtifactOverlayLimitsV1::default()).unwrap();
  assert!(overlay.insert(page.clone()).unwrap());
  assert!(!overlay.insert(page.clone()).unwrap());
  assert_eq!(overlay.artifact_count(), 1);

  let mut forged = page;
  forged.key[0] ^= 0xff;
  assert!(matches!(overlay.insert(forged).unwrap_err(), IndexBatchApplicationErrorV1::Malformed(_)));
}
