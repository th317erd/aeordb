use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::{IndexManifestWriteV1, decode_index_manifest, encode_index_manifest};
use aeordb::engine::v4::index_batch_application::{
  FrozenIndexBatchApplicationPlanV1, FrozenIndexBatchApplicationRequestV1, FrozenIndexOwnerSourceV1, IndexBatchArtifactOverlayLimitsV1,
  IndexBatchArtifactReadErrorV1, IndexBatchArtifactSourceV1, OrderedPagePathLookupLimitsV1, apply_frozen_index_batch_v1,
};
use aeordb::engine::v4::index_coordinator::{
  FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexGroupMutationRequestV1,
  IndexMembershipOwnerClassV1, IndexMembershipStateV1, IndexMembershipTransitionRequestV1, IndexMutationGroupRequestV1,
  IndexMutationOperationV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_copy_on_write::{default_index_directory_layout_v1, default_index_page_layout_v1};
use aeordb::engine::v4::index_generation_authority::{FrozenIndexGenerationPublicationRequestV1, publish_frozen_index_application_v1};
use aeordb::engine::v4::index_generation_publication::{
  INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1, IndexGenerationPublicationLimitsV1,
  IndexGenerationPublicationModeV1,
};
use aeordb::engine::v4::index_manifest::{CoverageVersionV1, FieldIndexManifestBodyV1, IndexManifestBodyV1, ValueStoreManifestBodyV1};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, ScopeReverseRecordV1, encode_scope_document_record, encode_scope_reverse_record};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const DATABASE_ID: [u8; 16] = [0x31; 16];

#[test]
fn complete_application_publishes_scope_value_field_closure_and_retries_without_moving_pointers() {
  let plan = complete_application_plan(ALGORITHM);
  let (_directory, path, publisher) = create_publisher(ALGORITHM);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);
  let request = publication_request(&plan, IndexGenerationPublicationModeV1::Soft);

  let first = publish_frozen_index_application_v1(&publisher, &mut retirement, request, &|| false).unwrap();
  assert_eq!(first.manifest_count, 3);
  assert_eq!(first.pointer_receipts.len(), 2);
  assert!(first.pointer_receipts.iter().all(|receipt| !receipt.idempotent));
  let after_first = publisher.observe().unwrap();

  let retry = publish_frozen_index_application_v1(&publisher, &mut retirement, request, &|| false).unwrap();
  assert_eq!(retry.pointer_receipts.len(), 2);
  assert!(retry.pointer_receipts.iter().all(|receipt| receipt.idempotent));
  assert_eq!(publisher.observe().unwrap(), after_first);

  drop(publisher);
  let reopened = reopen(&path);
  for receipt in &first.pointer_receipts {
    let pair = reopened.load_index_active_pointer_pair(&DATABASE_ID, receipt.kind, &receipt.owner_id).unwrap();
    let selected = pair.selected.unwrap();
    assert_eq!(selected.bytes, receipt.pointer_bytes);
    assert_eq!(selected.target_manifest_hash, receipt.manifest_key);
  }
}

#[test]
fn hard_application_uses_two_real_ordered_barriers_for_every_selected_pointer() {
  let plan = complete_application_plan(ALGORITHM);
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);

  let receipt = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&plan, IndexGenerationPublicationModeV1::Hard),
    &|| false,
  )
  .unwrap();
  let immutable = receipt.immutable_barrier_sequence.unwrap();
  let pointer = receipt.pointer_barrier_sequence.unwrap();
  assert!(pointer > immutable);
  assert!(receipt
    .pointer_receipts
    .iter()
    .all(|owner| { owner.immutable_barrier_sequence == Some(immutable) && owner.pointer_barrier_sequence == Some(pointer) }));

  let retry = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&plan, IndexGenerationPublicationModeV1::Hard),
    &|| false,
  )
  .unwrap();
  assert!(retry.pointer_receipts.iter().all(|owner| owner.idempotent));
  assert!(retry.immutable_barrier_sequence.unwrap() > pointer);
  assert!(retry.pointer_barrier_sequence.unwrap() > retry.immutable_barrier_sequence.unwrap());
}

#[test]
fn dependency_bearing_scope_generation_publishes_pages_directories_manifest_and_pointer() {
  let plan = scope_insert_application_plan(ALGORITHM);
  assert!(plan.prepared_artifacts().len() >= 4);
  let expected_artifacts = plan.prepared_artifacts().map(|artifact| (artifact.key.clone(), artifact.value.clone())).collect::<Vec<_>>();
  let (_directory, path, publisher) = create_publisher(ALGORITHM);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);

  let receipt = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&plan, IndexGenerationPublicationModeV1::Soft),
    &|| false,
  )
  .unwrap();
  assert_eq!(receipt.artifact_count, expected_artifacts.len());
  assert_eq!(receipt.manifest_count, 1);
  assert_eq!(receipt.pointer_receipts.len(), 1);
  assert_eq!(receipt.pointer_receipts[0].dependency_count, expected_artifacts.len());
  for (key, value) in &expected_artifacts {
    assert_eq!(publisher.load_index_artifact(key, u64::try_from(value.len()).unwrap()).unwrap().unwrap(), *value);
  }

  drop(publisher);
  let reopened = reopen(&path);
  for (key, value) in &expected_artifacts {
    assert_eq!(reopened.load_index_artifact(key, u64::try_from(value.len()).unwrap()).unwrap().unwrap(), *value);
  }
}

#[test]
fn sha512_application_publishes_and_reopens_the_exact_complete_closure() {
  let plan = complete_application_plan(HashAlgorithm::Sha512);
  let (_directory, path, publisher) = create_publisher(HashAlgorithm::Sha512);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(HashAlgorithm::Sha512, &cancellation, &memory);
  let receipt = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request_for(&plan, HashAlgorithm::Sha512, IndexGenerationPublicationModeV1::Soft),
    &|| false,
  )
  .unwrap();
  assert_eq!(receipt.pointer_receipts.len(), 2);
  assert!(receipt.pointer_receipts.iter().all(|owner| {
    owner.pointer_key.len() == HashAlgorithm::Sha512.hash_length() && owner.manifest_key.len() == HashAlgorithm::Sha512.hash_length()
  }));

  drop(publisher);
  let reopened = reopen(&path);
  for owner in &receipt.pointer_receipts {
    let selected = reopened.load_index_active_pointer_pair(&DATABASE_ID, owner.kind, &owner.owner_id).unwrap().selected.unwrap();
    assert_eq!(selected.bytes, owner.pointer_bytes);
  }
}

#[test]
fn foreign_database_identity_refuses_before_any_authority_mutation() {
  const FOREIGN_DATABASE_ID: [u8; 16] = [0x99; 16];
  let plan = complete_application_plan(ALGORITHM);
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let before = publisher.observe().unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);
  let request = FrozenIndexGenerationPublicationRequestV1 {
    database_id: &FOREIGN_DATABASE_ID,
    ..publication_request(&plan, IndexGenerationPublicationModeV1::Soft)
  };

  let error = publish_frozen_index_application_v1(&publisher, &mut retirement, request, &|| false).unwrap_err();
  assert_eq!(error.code(), "index_generation_database_identity");
  assert_eq!(publisher.observe().unwrap(), before);
}

#[test]
fn invalid_timestamps_and_initial_cancellation_refuse_before_any_authority_mutation() {
  let plan = complete_application_plan(ALGORITHM);
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let before = publisher.observe().unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);

  for request in [
    FrozenIndexGenerationPublicationRequestV1 {
      publication_timestamp_ms: 0,
      ..publication_request(&plan, IndexGenerationPublicationModeV1::Soft)
    },
    FrozenIndexGenerationPublicationRequestV1 { monotonic_now_ms: 0, ..publication_request(&plan, IndexGenerationPublicationModeV1::Soft) },
  ] {
    let error = publish_frozen_index_application_v1(&publisher, &mut retirement, request, &|| false).unwrap_err();
    assert_eq!(error.code(), "index_generation_publication_request");
    assert_eq!(publisher.observe().unwrap(), before);
  }

  let error = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&plan, IndexGenerationPublicationModeV1::Soft),
    &|| true,
  )
  .unwrap_err();
  assert_eq!(error.code(), "index_generation_cancelled");
  assert_eq!(
    error.failure_boundary(),
    aeordb::engine::v4::index_generation_publication::IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained
  );
  assert_eq!(publisher.observe().unwrap(), before);
}

#[test]
fn unreferenced_value_store_refuses_before_any_authority_mutation() {
  let plan = value_only_application_plan(ALGORITHM);
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let before = publisher.observe().unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);

  let error = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&plan, IndexGenerationPublicationModeV1::Soft),
    &|| false,
  )
  .unwrap_err();
  assert_eq!(error.code(), "index_generation_unreferenced_value_store");
  assert_eq!(publisher.observe().unwrap(), before);
}

#[test]
fn a_plan_built_from_a_superseded_selected_manifest_refuses_before_pointer_mutation() {
  let selected_plan = complete_application_plan(ALGORITHM);
  let stale_plan = scope_insert_application_plan(ALGORITHM);
  let (_directory, _path, publisher) = create_publisher(ALGORITHM);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);
  publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&selected_plan, IndexGenerationPublicationModeV1::Soft),
    &|| false,
  )
  .unwrap();
  let before = publisher.observe().unwrap();
  let stale_owner = &stale_plan.owner_plans()[0];
  let selected = publisher
    .load_index_active_pointer_pair(
      &DATABASE_ID,
      aeordb::engine::v4::index_artifact::ActivePointerKindV1::ScopeCatalog,
      stale_owner.owner_id(),
    )
    .unwrap()
    .selected
    .unwrap();
  assert_ne!(selected.target_manifest_hash, stale_owner.source_manifest_key());
  assert_ne!(selected.target_manifest_hash, stale_owner.successor_manifest().key);

  let error = publish_frozen_index_application_v1(
    &publisher,
    &mut retirement,
    publication_request(&stale_plan, IndexGenerationPublicationModeV1::Soft),
    &|| false,
  )
  .unwrap_err();

  assert_eq!(error.code(), "index_generation_source_superseded");
  assert_eq!(
    error.failure_boundary(),
    aeordb::engine::v4::index_generation_publication::IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained
  );
  assert_eq!(publisher.observe().unwrap(), before);
}

#[test]
fn every_cancellable_physical_prefix_reopens_and_converges_on_exact_retry() {
  let mut saw_prior_authority = false;
  let mut saw_partial_successor = false;
  let mut saw_complete = false;
  for cancel_after in 0usize..32 {
    let plan = complete_application_plan(ALGORITHM);
    let (_directory, path, publisher) = create_publisher(ALGORITHM);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let mut retirement = retirement_owner(ALGORITHM, &cancellation, &memory);
    let calls = Cell::new(0usize);
    let result = publish_frozen_index_application_v1(
      &publisher,
      &mut retirement,
      publication_request(&plan, IndexGenerationPublicationModeV1::Soft),
      &|| {
        let call = calls.get();
        calls.set(call + 1);
        call >= cancel_after
      },
    );
    drop(publisher);

    let reopened = reopen(&path);
    let selected_count = selected_pointer_count(&reopened, &plan);
    match result {
      Ok(_) => {
        saw_complete = true;
        assert_eq!(selected_count, 2);
      }
      Err(error) => {
        saw_prior_authority |= selected_count == 0;
        saw_partial_successor |= selected_count == 1;
        assert_eq!(error.code(), "index_generation_cancelled");
        assert_eq!(
          error.failure_boundary(),
          if selected_count == 0 {
            aeordb::engine::v4::index_generation_publication::IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained
          } else {
            aeordb::engine::v4::index_generation_publication::IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible
          }
        );
        let mut retry_retirement = retirement_owner(ALGORITHM, &cancellation, &memory);
        let retry = publish_frozen_index_application_v1(
          &reopened,
          &mut retry_retirement,
          publication_request(&plan, IndexGenerationPublicationModeV1::Soft),
          &|| false,
        )
        .unwrap();
        assert_eq!(retry.pointer_receipts.len(), 2);
        assert_eq!(selected_pointer_count(&reopened, &plan), 2);
      }
    }
  }
  assert!(saw_prior_authority);
  assert!(saw_partial_successor);
  assert!(saw_complete);
}

fn publication_request<'a>(
  plan: &'a FrozenIndexBatchApplicationPlanV1,
  mode: IndexGenerationPublicationModeV1,
) -> FrozenIndexGenerationPublicationRequestV1<'a> {
  publication_request_for(plan, ALGORITHM, mode)
}

fn publication_request_for<'a>(
  plan: &'a FrozenIndexBatchApplicationPlanV1,
  hash_algorithm: HashAlgorithm,
  mode: IndexGenerationPublicationModeV1,
) -> FrozenIndexGenerationPublicationRequestV1<'a> {
  FrozenIndexGenerationPublicationRequestV1 {
    database_id: &DATABASE_ID,
    hash_algorithm,
    plan,
    mode,
    limits: IndexGenerationPublicationLimitsV1::new(INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1)
      .unwrap(),
    publication_timestamp_ms: 1_700_000_000_300,
    monotonic_now_ms: 1_700_000_000_300,
  }
}

fn scope_insert_application_plan(hash_algorithm: HashAlgorithm) -> FrozenIndexBatchApplicationPlanV1 {
  let source_manifest = fixture_manifest(hash_algorithm, "scope-catalog");
  let source = decode_index_manifest(&source_manifest, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(source_body) = &source.details else {
    panic!("fixture is a scope-catalog manifest");
  };
  let path = "/docs/readme.md";
  let file_key = digest_parts(hash_algorithm, &[b"file:", path.as_bytes()]);
  let revision = vec![0xa7; hash_algorithm.hash_length()];
  let ordinal = encode_scope_document_record(
    &ScopeDocumentRecordV1 { tombstone: false, document_ordinal: 7, file_key: &file_key, record_revision_hash: &revision, path },
    hash_algorithm,
  )
  .unwrap();
  let reverse = encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: 7, file_key: &file_key }, hash_algorithm).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, 1_000_000, 1, 299_999).unwrap());
  let mut coordinator =
    IndexCoordinatorV1::new([0x45; 16], hash_algorithm, memory, IndexCoordinatorOptionsV1::new(500_000, 2, 1_000, 500_000).unwrap(), 1_000)
      .unwrap();
  let mutations = [
    IndexGroupMutationRequestV1 {
      operation: IndexMutationOperationV1::Upsert,
      mutation: IndexMutationRequestV1 {
        index_id: source.owner_id,
        role: OrderedIndexRoleV1::ScopeOrdinal,
        publication_sequence: 9,
        operation_id: [0x64; 16],
        encoded_record: &ordinal,
      },
    },
    IndexGroupMutationRequestV1 {
      operation: IndexMutationOperationV1::Upsert,
      mutation: IndexMutationRequestV1 {
        index_id: source.owner_id,
        role: OrderedIndexRoleV1::ScopeReverse,
        publication_sequence: 9,
        operation_id: [0x64; 16],
        encoded_record: &reverse,
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
          before: IndexMembershipStateV1 { live: false, unindexable: false },
          after: IndexMembershipStateV1 { live: true, unindexable: false },
        },
        mutations: &mutations,
      },
      1_001,
    )
    .unwrap();
  let batch = coordinator.begin_flush(1_002, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  let owner_sources =
    [FrozenIndexOwnerSourceV1 { source_manifest: &source_manifest, next_document_ordinal: Some(source_body.next_document_ordinal.max(8)) }];
  apply_plan(hash_algorithm, source.generation + 1, &batch, &owner_sources)
}

fn selected_pointer_count(publisher: &V4FirstAuthorityPublisher, plan: &FrozenIndexBatchApplicationPlanV1) -> usize {
  plan
    .owner_plans()
    .iter()
    .filter_map(|owner| match owner.owner_class() {
      IndexMembershipOwnerClassV1::ScopeCatalog => Some(aeordb::engine::v4::index_artifact::ActivePointerKindV1::ScopeCatalog),
      IndexMembershipOwnerClassV1::FieldIndex => Some(aeordb::engine::v4::index_artifact::ActivePointerKindV1::FieldIndex),
      IndexMembershipOwnerClassV1::ValueStore => None,
    })
    .filter(|kind| publisher.load_index_active_pointer_pair(&DATABASE_ID, *kind, plan_owner_id(plan, *kind)).unwrap().selected.is_some())
    .count()
}

fn plan_owner_id(plan: &FrozenIndexBatchApplicationPlanV1, kind: aeordb::engine::v4::index_artifact::ActivePointerKindV1) -> &[u8] {
  plan
    .owner_plans()
    .iter()
    .find(|owner| {
      matches!(
        (owner.owner_class(), kind),
        (IndexMembershipOwnerClassV1::ScopeCatalog, aeordb::engine::v4::index_artifact::ActivePointerKindV1::ScopeCatalog)
          | (IndexMembershipOwnerClassV1::FieldIndex, aeordb::engine::v4::index_artifact::ActivePointerKindV1::FieldIndex)
      )
    })
    .unwrap()
    .owner_id()
}

fn complete_application_plan(hash_algorithm: HashAlgorithm) -> FrozenIndexBatchApplicationPlanV1 {
  let scope_source = fixture_manifest(hash_algorithm, "scope-catalog");
  let scope = decode_index_manifest(&scope_source, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(scope_body) = &scope.details else {
    panic!("fixture is a scope-catalog manifest");
  };
  let value_fixture = fixture_manifest(hash_algorithm, "value-store");
  let value_fixture = decode_index_manifest(&value_fixture, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ValueStore(value_body) = &value_fixture.details else {
    panic!("fixture is a value-store manifest");
  };
  let value_source = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: value_fixture.generation,
    owner_id: value_fixture.owner_id,
    body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 { scope_catalog_manifest: &scope.key, ..value_body.clone() }),
  })
  .unwrap();
  let field_fixture = fixture_manifest(hash_algorithm, "field-index");
  let field_fixture = decode_index_manifest(&field_fixture, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(field_body) = &field_fixture.details else {
    panic!("fixture is a field-index manifest");
  };
  let field_source = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: field_fixture.generation,
    owner_id: field_fixture.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 { value_store_manifest: &value_source.key, ..field_body.clone() }),
  })
  .unwrap();
  let owners = [
    (scope.owner_id, IndexMembershipOwnerClassV1::ScopeCatalog),
    (value_fixture.owner_id, IndexMembershipOwnerClassV1::ValueStore),
    (field_fixture.owner_id, IndexMembershipOwnerClassV1::FieldIndex),
  ];
  let batch = transition_only_batch_many(hash_algorithm, &owners);
  let mut owner_sources = vec![
    FrozenIndexOwnerSourceV1 { source_manifest: &scope_source, next_document_ordinal: Some(scope_body.next_document_ordinal.max(8)) },
    FrozenIndexOwnerSourceV1 { source_manifest: &value_source.value, next_document_ordinal: None },
    FrozenIndexOwnerSourceV1 { source_manifest: &field_source.value, next_document_ordinal: None },
  ];
  owner_sources.sort_unstable_by(|left, right| {
    decode_index_manifest(left.source_manifest, hash_algorithm)
      .unwrap()
      .owner_id
      .cmp(decode_index_manifest(right.source_manifest, hash_algorithm).unwrap().owner_id)
  });
  let generation = scope.generation.max(value_fixture.generation).max(field_fixture.generation) + 1;
  apply_plan(hash_algorithm, generation, &batch, &owner_sources)
}

fn value_only_application_plan(hash_algorithm: HashAlgorithm) -> FrozenIndexBatchApplicationPlanV1 {
  let source = fixture_manifest(hash_algorithm, "value-store");
  let manifest = decode_index_manifest(&source, hash_algorithm).unwrap();
  let owners = [(manifest.owner_id, IndexMembershipOwnerClassV1::ValueStore)];
  let batch = transition_only_batch_many(hash_algorithm, &owners);
  let owner_sources = [FrozenIndexOwnerSourceV1 { source_manifest: &source, next_document_ordinal: None }];
  apply_plan(hash_algorithm, manifest.generation + 1, &batch, &owner_sources)
}

fn apply_plan(
  hash_algorithm: HashAlgorithm,
  generation: u64,
  batch: &FrozenIndexBatchV1,
  owner_sources: &[FrozenIndexOwnerSourceV1<'_>],
) -> FrozenIndexBatchApplicationPlanV1 {
  let namespace_root = vec![0xfa; hash_algorithm.hash_length()];
  let epoch = [0xfb; 16];
  let mut source = MissingSource;
  apply_frozen_index_batch_v1(
    &FrozenIndexBatchApplicationRequestV1 {
      hash_algorithm,
      generation,
      coverage: CoverageVersionV1 { source_namespace_root: &namespace_root, coverage_epoch_id: &epoch, coverage_publication_sequence: 9 },
      batch,
      owner_sources,
      overlay_limits: IndexBatchArtifactOverlayLimitsV1::default(),
      path_limits: OrderedPagePathLookupLimitsV1::default(),
      page_layout: default_index_page_layout_v1(),
      directory_layout: default_index_directory_layout_v1(),
    },
    &mut source,
    &|| false,
  )
  .unwrap()
}

fn transition_only_batch_many(hash_algorithm: HashAlgorithm, owners: &[(&[u8], IndexMembershipOwnerClassV1)]) -> FrozenIndexBatchV1 {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(700_000, 1_000_000, 1, 299_999).unwrap());
  let mut coordinator = IndexCoordinatorV1::new(
    [0x44; 16],
    hash_algorithm,
    memory,
    IndexCoordinatorOptionsV1::new(500_000, 100, 1_000, 500_000).unwrap(),
    1_000,
  )
  .unwrap();
  for (index, (owner_id, owner_class)) in owners.iter().enumerate() {
    coordinator
      .admit_group(
        IndexMutationGroupRequestV1 {
          transition: IndexMembershipTransitionRequestV1 {
            owner_id,
            owner_class: *owner_class,
            publication_sequence: 9,
            operation_id: [u8::try_from(index).unwrap() + 0x70; 16],
            document_ordinal: 7,
            before: IndexMembershipStateV1 { live: false, unindexable: false },
            after: IndexMembershipStateV1 { live: false, unindexable: false },
          },
          mutations: &[],
        },
        1_001 + u64::try_from(index).unwrap(),
      )
      .unwrap();
  }
  coordinator.begin_flush(1_100, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap()
}

struct MissingSource;

impl IndexBatchArtifactSourceV1 for MissingSource {
  fn read_immutable_artifact(&mut self, _key: &[u8], _maximum_bytes: usize) -> Result<Vec<u8>, IndexBatchArtifactReadErrorV1> {
    Err(IndexBatchArtifactReadErrorV1::Missing)
  }
}

fn fixture_manifest(hash_algorithm: HashAlgorithm, kind: &str) -> Vec<u8> {
  let profile = match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("generation authority tests use independent hash-width fixtures"),
  };
  fs::read(fixture_root().join(format!("aidx-{profile}-{kind}-manifest-empty.bin"))).unwrap()
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn create_publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("index-generation-authority.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header(algorithm, initial_block_size());
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .unwrap();
  file.sync_all().unwrap();
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator).unwrap();
  publisher.publish(&first_authority_request(algorithm)).unwrap();
  (directory, path, publisher)
}

fn reopen(path: &Path) -> V4FirstAuthorityPublisher {
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  let observation = aeordb::engine::v4::header_publication::observe_database_header_v4(&file).unwrap();
  let header = &observation.selected.header;
  let hot_tail = read_hot_tail_checked(&mut file, header.hot_tail_offset, header.hash_algorithm.hash_length()).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::open_with_coordinator(
    file.try_clone().unwrap(),
    header.hash_algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    header.kv_block_stage as usize,
    hot_tail.writes,
    hot_tail.voids,
    header.kv_block_version,
    coordinator.clone(),
  )
  .unwrap();
  V4FirstAuthorityPublisher::new(kv, coordinator).unwrap()
}

fn initial_header(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: DATABASE_ID,
    write_sequence_high_water: 1,
    required_reader_capabilities: [0; 32],
    kv_block_offset: DATABASE_HEADER_V4_DATA_OFFSET,
    kv_block_length,
    kv_block_version: DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    kv_block_stage: 0,
    resize_in_progress: false,
    resize_target_stage: 0,
    nvt_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    nvt_length: 0,
    nvt_version: 1,
    backup_type: 0,
    hot_tail_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    buffer_kvs_offset: 0,
    buffer_nvt_offset: 0,
    entry_count: 0,
    head_hash: vec![0; algorithm.hash_length()],
    base_hash: vec![0; algorithm.hash_length()],
    target_hash: vec![0; algorithm.hash_length()],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; algorithm.hash_length()],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  }
}

fn first_authority_request(algorithm: HashAlgorithm) -> FirstAuthorityPublicationRequestV1 {
  FirstAuthorityPublicationRequestV1 {
    database_id: DATABASE_ID,
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state: encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
      },
      algorithm,
    )
    .unwrap(),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed index-generation closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn retirement_owner(algorithm: HashAlgorithm, cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    DATABASE_ID,
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}
