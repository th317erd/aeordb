use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::index_batch_application::{
  FrozenIndexCompactionApplicationOutcomeV1, IndexArtifactCompactionApplicationRequestV1, IndexBatchArtifactOverlayLimitsV1,
  apply_index_artifact_compaction_v1,
};
use aeordb::engine::v4::index_copy_on_write::{ArtifactDirectoryPathV1, default_index_directory_layout_v1, default_index_page_layout_v1};
use aeordb::engine::v4::index_manifest::{FieldIndexManifestBodyV1, IndexManifestBodyV1, ValueStoreManifestBodyV1};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, encode_artifact_directory, encode_ordered_page, encode_posting_record,
};
use aeordb::engine::v4::index_artifact::{IndexManifestWriteV1, decode_index_manifest, encode_index_manifest};
use aeordb::engine::v4::index_record::{CanonicalValueRecordV1, encode_canonical_value_record};

const COORDINATOR_ID: [u8; 16] = [0x41; 16];

#[test]
fn eligible_field_window_becomes_one_publication_ready_frozen_plan_at_both_hash_widths() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fixture_manifest(hash_algorithm, "field-index");
    let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(fixture_body) = &fixture.details else {
      panic!("fixture is a field-index manifest");
    };
    let left = posting_page(hash_algorithm, fixture.owner_id, fixture.generation, 10, 0, 30, 1);
    let right = posting_page(hash_algorithm, fixture.owner_id, fixture.generation, 30, 10, 0, 2);
    let root = posting_root(hash_algorithm, fixture.owner_id, fixture.generation, &[&left, &right]);
    let root_decoded = decode_artifact_directory(&root, hash_algorithm).unwrap();
    let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm,
      generation: fixture.generation,
      owner_id: fixture.owner_id,
      body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
        posting_directory_root: Some(&root_decoded.key),
        first_page_id: 10,
        last_page_id: 30,
        next_page_id: 40,
        posting_page_count: 2,
        live_posting_count: 2,
        posting_document_count: 2,
        posting_tombstone_count: 0,
        live_canonical_posting_bytes: decoded_live_bytes(hash_algorithm, &left) + decoded_live_bytes(hash_algorithm, &right),
        ..fixture_body.clone()
      }),
    })
    .unwrap();
    let left_decoded = decode_ordered_page(&left, hash_algorithm).unwrap();
    let right_decoded = decode_ordered_page(&right, hash_algorithm).unwrap();
    let root_path = [root.as_slice()];
    let paths = [
      ArtifactDirectoryPathV1 { source_page_key: &left_decoded.key, directories: &root_path },
      ArtifactDirectoryPathV1 { source_page_key: &right_decoded.key, directories: &root_path },
    ];
    let source_pages = [left.as_slice(), right.as_slice()];

    let outcome = apply_index_artifact_compaction_v1(
      &IndexArtifactCompactionApplicationRequestV1 {
        hash_algorithm,
        coordinator_id: COORDINATOR_ID,
        batch_id: 11,
        attempt_id: 1,
        generation: fixture.generation + 1,
        source_manifest: &source_manifest.value,
        dependent_field_manifests: &[],
        role: OrderedIndexRoleV1::Posting,
        source_pages: &source_pages,
        previous_posting_page: None,
        next_posting_page: None,
        paths: &paths,
        tombstone_drop_proof: None,
        overlay_limits: IndexBatchArtifactOverlayLimitsV1::default(),
        page_layout: default_index_page_layout_v1(),
        directory_layout: default_index_directory_layout_v1(),
      },
      &|| false,
    )
    .unwrap();
    let FrozenIndexCompactionApplicationOutcomeV1::Publication(plan) = outcome else {
      panic!("eligible pair must produce a publication plan");
    };

    assert_eq!(plan.coordinator_id(), COORDINATOR_ID);
    assert_eq!((plan.batch_id(), plan.attempt_id(), plan.generation()), (11, 1, fixture.generation + 1));
    assert_eq!(plan.owner_plans().len(), 1);
    assert_eq!(plan.prepared_artifacts().len(), 2);
    let successor = decode_index_manifest(&plan.owner_plans()[0].successor_manifest().value, hash_algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(successor) = successor.details else {
      panic!("successor is a field-index manifest");
    };
    assert_eq!(successor.coverage, fixture_body.coverage);
    assert_ne!(successor.posting_directory_root, Some(root_decoded.key.as_slice()));
    assert_eq!((successor.posting_page_count, successor.live_posting_count, successor.posting_tombstone_count), (1, 2, 0));
    assert_eq!((successor.first_page_id, successor.last_page_id, successor.next_page_id), (10, 10, 40));
  }
}

#[test]
fn ineligible_single_page_is_unchanged_without_a_publication_plan() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let fixture = fixture_manifest(hash_algorithm, "field-index");
  let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(fixture_body) = &fixture.details else {
    panic!("fixture is a field-index manifest");
  };
  let page = posting_page(hash_algorithm, fixture.owner_id, fixture.generation, 10, 0, 0, 1);
  let root = posting_root(hash_algorithm, fixture.owner_id, fixture.generation, &[&page]);
  let root_decoded = decode_artifact_directory(&root, hash_algorithm).unwrap();
  let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: fixture.generation,
    owner_id: fixture.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
      posting_directory_root: Some(&root_decoded.key),
      first_page_id: 10,
      last_page_id: 10,
      next_page_id: 20,
      posting_page_count: 1,
      live_posting_count: 1,
      posting_document_count: 1,
      posting_tombstone_count: 0,
      live_canonical_posting_bytes: decoded_live_bytes(hash_algorithm, &page),
      ..fixture_body.clone()
    }),
  })
  .unwrap();
  let decoded = decode_ordered_page(&page, hash_algorithm).unwrap();
  let root_path = [root.as_slice()];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &decoded.key, directories: &root_path }];
  let source_pages = [page.as_slice()];

  let outcome = apply_index_artifact_compaction_v1(
    &IndexArtifactCompactionApplicationRequestV1 {
      hash_algorithm,
      coordinator_id: COORDINATOR_ID,
      batch_id: 12,
      attempt_id: 1,
      generation: fixture.generation + 1,
      source_manifest: &source_manifest.value,
      dependent_field_manifests: &[],
      role: OrderedIndexRoleV1::Posting,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      paths: &paths,
      tombstone_drop_proof: None,
      overlay_limits: IndexBatchArtifactOverlayLimitsV1::default(),
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    },
    &|| false,
  )
  .unwrap();

  assert!(matches!(outcome, FrozenIndexCompactionApplicationOutcomeV1::Unchanged));
}

#[test]
fn value_store_compaction_carries_its_dependent_field_selector_parent_before_child() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let value_fixture = fixture_manifest(hash_algorithm, "value-store");
  let value_fixture = decode_index_manifest(&value_fixture, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ValueStore(value_body) = &value_fixture.details else {
    panic!("fixture is a value-store manifest");
  };
  let left = value_page(hash_algorithm, value_fixture.owner_id, value_fixture.generation, 10, 1);
  let right = value_page(hash_algorithm, value_fixture.owner_id, value_fixture.generation, 30, 2);
  let root = ordered_root(hash_algorithm, OrderedIndexRoleV1::Value, value_fixture.owner_id, value_fixture.generation, &[&left, &right]);
  let root_decoded = decode_artifact_directory(&root, hash_algorithm).unwrap();
  let source_value = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: value_fixture.generation,
    owner_id: value_fixture.owner_id,
    body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
      value_directory_root: Some(&root_decoded.key),
      next_page_id: 40,
      value_page_count: 2,
      value_document_count: 2,
      live_value_count: 2,
      value_tombstone_count: 0,
      live_canonical_value_bytes: decoded_live_bytes(hash_algorithm, &left) + decoded_live_bytes(hash_algorithm, &right),
      ..value_body.clone()
    }),
  })
  .unwrap();
  let field_fixture = fixture_manifest(hash_algorithm, "field-index");
  let field_fixture = decode_index_manifest(&field_fixture, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(field_body) = &field_fixture.details else {
    panic!("fixture is a field-index manifest");
  };
  let source_field = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: field_fixture.generation,
    owner_id: field_fixture.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 { value_store_manifest: &source_value.key, ..field_body.clone() }),
  })
  .unwrap();
  let left_decoded = decode_ordered_page(&left, hash_algorithm).unwrap();
  let right_decoded = decode_ordered_page(&right, hash_algorithm).unwrap();
  let root_path = [root.as_slice()];
  let paths = [
    ArtifactDirectoryPathV1 { source_page_key: &left_decoded.key, directories: &root_path },
    ArtifactDirectoryPathV1 { source_page_key: &right_decoded.key, directories: &root_path },
  ];
  let source_pages = [left.as_slice(), right.as_slice()];
  let dependent_fields = [source_field.value.as_slice()];

  let request = IndexArtifactCompactionApplicationRequestV1 {
    hash_algorithm,
    coordinator_id: COORDINATOR_ID,
    batch_id: 13,
    attempt_id: 1,
    generation: value_fixture.generation.max(field_fixture.generation) + 1,
    source_manifest: &source_value.value,
    dependent_field_manifests: &dependent_fields,
    role: OrderedIndexRoleV1::Value,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    paths: &paths,
    tombstone_drop_proof: None,
    overlay_limits: IndexBatchArtifactOverlayLimitsV1::default(),
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  };
  let outcome = apply_index_artifact_compaction_v1(&request, &|| false).unwrap();
  let FrozenIndexCompactionApplicationOutcomeV1::Publication(plan) = outcome else {
    panic!("eligible ValueStore pair must publish through a dependent FieldIndex");
  };

  assert_eq!(plan.owner_plans().len(), 2);
  let value_successor = decode_index_manifest(&plan.owner_plans()[0].successor_manifest().value, hash_algorithm).unwrap();
  let field_successor = decode_index_manifest(&plan.owner_plans()[1].successor_manifest().value, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ValueStore(value_successor_body) = value_successor.details else {
    panic!("first successor is the compacted ValueStore");
  };
  let IndexManifestBodyV1::FieldIndex(field_successor_body) = field_successor.details else {
    panic!("second successor is the dependent FieldIndex");
  };
  assert_eq!((value_successor_body.value_page_count, value_successor_body.live_value_count), (1, 2));
  assert_eq!(field_successor_body.value_store_manifest, value_successor.key);
  assert_eq!(field_successor_body.coverage, value_successor_body.coverage);
  assert!(plan.owner_plans()[0].dependency_range().len() > 0);
  assert!(plan.owner_plans()[1].dependency_range().is_empty());

  let duplicate_fields = [source_field.value.as_slice(), source_field.value.as_slice()];
  let mut duplicate_request = request;
  duplicate_request.dependent_field_manifests = &duplicate_fields;
  assert_eq!(apply_index_artifact_compaction_v1(&duplicate_request, &|| false).unwrap_err().code(), "index_batch_manifest_closure");
}

#[test]
fn value_store_without_a_dependent_selector_refuses_before_planning_output() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let value_fixture = fixture_manifest(hash_algorithm, "value-store");
  let value_fixture = decode_index_manifest(&value_fixture, hash_algorithm).unwrap();
  let page = value_page(hash_algorithm, value_fixture.owner_id, value_fixture.generation, 10, 1);
  let root = ordered_root(hash_algorithm, OrderedIndexRoleV1::Value, value_fixture.owner_id, value_fixture.generation, &[&page]);
  let decoded = decode_ordered_page(&page, hash_algorithm).unwrap();
  let root_path = [root.as_slice()];
  let paths = [ArtifactDirectoryPathV1 { source_page_key: &decoded.key, directories: &root_path }];
  let source_pages = [page.as_slice()];

  let error = apply_index_artifact_compaction_v1(
    &IndexArtifactCompactionApplicationRequestV1 {
      hash_algorithm,
      coordinator_id: COORDINATOR_ID,
      batch_id: 14,
      attempt_id: 1,
      generation: value_fixture.generation + 1,
      source_manifest: &value_fixture_bytes(hash_algorithm),
      dependent_field_manifests: &[],
      role: OrderedIndexRoleV1::Value,
      source_pages: &source_pages,
      previous_posting_page: None,
      next_posting_page: None,
      paths: &paths,
      tombstone_drop_proof: None,
      overlay_limits: IndexBatchArtifactOverlayLimitsV1::default(),
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    },
    &|| false,
  )
  .unwrap_err();

  assert_eq!(error.code(), "index_batch_manifest_closure");
}

#[test]
fn compaction_refuses_cancellation_at_initial_and_post_page_planning_boundaries() {
  with_eligible_field_compaction(HashAlgorithm::Blake3_256, |request| {
    let initial = apply_index_artifact_compaction_v1(request, &|| true).unwrap_err();
    assert_eq!(initial.code(), "index_batch_cancelled");

    let calls = Cell::new(0usize);
    let after_page_plan = apply_index_artifact_compaction_v1(request, &|| {
      let next = calls.get() + 1;
      calls.set(next);
      next >= 2
    })
    .unwrap_err();
    assert_eq!(after_page_plan.code(), "index_batch_cancelled");
    assert_eq!(calls.get(), 2);
  });
}

#[test]
fn compaction_refuses_identity_role_generation_and_overlay_pressure_without_a_plan() {
  with_eligible_field_compaction(HashAlgorithm::Blake3_256, |request| {
    let mut invalid_identity = *request;
    invalid_identity.coordinator_id = [0; 16];
    assert_eq!(apply_index_artifact_compaction_v1(&invalid_identity, &|| false).unwrap_err().code(), "index_batch_invalid_limits");

    let mut invalid_role = *request;
    invalid_role.role = OrderedIndexRoleV1::NvtTile;
    assert_eq!(apply_index_artifact_compaction_v1(&invalid_role, &|| false).unwrap_err().code(), "index_batch_invalid_limits");

    let mut stale_generation = *request;
    stale_generation.generation -= 1;
    assert_eq!(apply_index_artifact_compaction_v1(&stale_generation, &|| false).unwrap_err().code(), "index_batch_manifest_closure");

    let mut count_pressure = *request;
    count_pressure.overlay_limits = IndexBatchArtifactOverlayLimitsV1::new(1, 64 * 1_024 * 1_024).unwrap();
    assert_eq!(apply_index_artifact_compaction_v1(&count_pressure, &|| false).unwrap_err().code(), "index_batch_overlay_count");

    let mut byte_pressure = *request;
    byte_pressure.overlay_limits = IndexBatchArtifactOverlayLimitsV1::new(4_095, 1).unwrap();
    assert_eq!(apply_index_artifact_compaction_v1(&byte_pressure, &|| false).unwrap_err().code(), "index_batch_overlay_bytes");
  });
}

#[test]
fn non_value_compaction_refuses_an_ad_hoc_dependent_selector() {
  with_eligible_field_compaction(HashAlgorithm::Blake3_256, |request| {
    let dependents = [request.source_manifest];
    let mut invalid = *request;
    invalid.dependent_field_manifests = &dependents;
    assert_eq!(apply_index_artifact_compaction_v1(&invalid, &|| false).unwrap_err().code(), "index_batch_manifest_closure");
  });
}

fn with_eligible_field_compaction(hash_algorithm: HashAlgorithm, test: impl FnOnce(&IndexArtifactCompactionApplicationRequestV1<'_>)) {
  let fixture = fixture_manifest(hash_algorithm, "field-index");
  let fixture = decode_index_manifest(&fixture, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(fixture_body) = &fixture.details else {
    panic!("fixture is a field-index manifest");
  };
  let left = posting_page(hash_algorithm, fixture.owner_id, fixture.generation, 10, 0, 30, 1);
  let right = posting_page(hash_algorithm, fixture.owner_id, fixture.generation, 30, 10, 0, 2);
  let root = posting_root(hash_algorithm, fixture.owner_id, fixture.generation, &[&left, &right]);
  let root_decoded = decode_artifact_directory(&root, hash_algorithm).unwrap();
  let source_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: fixture.generation,
    owner_id: fixture.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
      posting_directory_root: Some(&root_decoded.key),
      first_page_id: 10,
      last_page_id: 30,
      next_page_id: 40,
      posting_page_count: 2,
      live_posting_count: 2,
      posting_document_count: 2,
      posting_tombstone_count: 0,
      live_canonical_posting_bytes: decoded_live_bytes(hash_algorithm, &left) + decoded_live_bytes(hash_algorithm, &right),
      ..fixture_body.clone()
    }),
  })
  .unwrap();
  let left_decoded = decode_ordered_page(&left, hash_algorithm).unwrap();
  let right_decoded = decode_ordered_page(&right, hash_algorithm).unwrap();
  let root_path = [root.as_slice()];
  let paths = [
    ArtifactDirectoryPathV1 { source_page_key: &left_decoded.key, directories: &root_path },
    ArtifactDirectoryPathV1 { source_page_key: &right_decoded.key, directories: &root_path },
  ];
  let source_pages = [left.as_slice(), right.as_slice()];
  test(&IndexArtifactCompactionApplicationRequestV1 {
    hash_algorithm,
    coordinator_id: COORDINATOR_ID,
    batch_id: 20,
    attempt_id: 1,
    generation: fixture.generation + 1,
    source_manifest: &source_manifest.value,
    dependent_field_manifests: &[],
    role: OrderedIndexRoleV1::Posting,
    source_pages: &source_pages,
    previous_posting_page: None,
    next_posting_page: None,
    paths: &paths,
    tombstone_drop_proof: None,
    overlay_limits: IndexBatchArtifactOverlayLimitsV1::default(),
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  });
}

fn posting_page(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  page_id: u64,
  previous_page_id: u64,
  next_page_id: u64,
  coordinate: u64,
) -> Vec<u8> {
  let record = encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate,
    document_ordinal: coordinate,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &coordinate.to_le_bytes(),
  })
  .unwrap();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation,
    page_id,
    previous_page_id,
    next_page_id,
    records: &[&record],
  })
  .unwrap()
  .value
}

fn value_page(hash_algorithm: HashAlgorithm, owner_id: &[u8], generation: u64, page_id: u64, ordinal: u64) -> Vec<u8> {
  let revision = vec![u8::try_from(ordinal).unwrap(); hash_algorithm.hash_length()];
  let canonical = encode_canonical_value(&CanonicalConfigValueV1::Unsigned(ordinal), CanonicalValueBounds::SOURCE_VALUE).unwrap();
  let record = encode_canonical_value_record(
    &CanonicalValueRecordV1 {
      tombstone: false,
      document_ordinal: ordinal,
      source_value_ordinal: 0,
      record_revision_hash: &revision,
      canonical_value: Some(&canonical),
    },
    hash_algorithm,
  )
  .unwrap();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Value,
    owner_id,
    generation,
    page_id,
    previous_page_id: 0,
    next_page_id: 0,
    records: &[&record],
  })
  .unwrap()
  .value
}

fn posting_root(hash_algorithm: HashAlgorithm, owner_id: &[u8], generation: u64, pages: &[&[u8]]) -> Vec<u8> {
  ordered_root(hash_algorithm, OrderedIndexRoleV1::Posting, owner_id, generation, pages)
}

fn ordered_root(hash_algorithm: HashAlgorithm, role: OrderedIndexRoleV1, owner_id: &[u8], generation: u64, pages: &[&[u8]]) -> Vec<u8> {
  let pages = pages.iter().map(|page| decode_ordered_page(page, hash_algorithm).unwrap()).collect::<Vec<_>>();
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
  encode_artifact_directory(&ArtifactDirectoryWriteV1 { hash_algorithm, role, owner_id, generation, level: 0, entries: &entries })
    .unwrap()
    .value
}

fn decoded_live_bytes(hash_algorithm: HashAlgorithm, page: &[u8]) -> u64 {
  decode_ordered_page(page, hash_algorithm).unwrap().logical_live_bytes
}

fn fixture_manifest(hash_algorithm: HashAlgorithm, kind: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("aidx-{}-{kind}-manifest-populated.bin", profile_name(hash_algorithm)))).unwrap()
}

fn value_fixture_bytes(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  fixture_manifest(hash_algorithm, "value-store")
}

fn profile_name(hash_algorithm: HashAlgorithm) -> &'static str {
  match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("compaction application tests use frozen v4 hash profiles"),
  }
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}
