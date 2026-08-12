use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, RootLifecycleSupportPublicationRequestV1,
  RootRetirementAuthorityRecheckErrorV1, RootRetirementAuthorityRecheckRequestV1, RootRetirementAuthoritySnapshotV1,
  RootRetirementAuthorityVerifierV1, RootRetirementPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::gc::{GcActiveControlWriteV1, encode_gc_active_control};
use aeordb::engine::v4::gc_lifecycle::{
  RootCandidateRecordWriteV1, RootExpiryManifestWriteV1, RootExpiryRecordWriteV1, RootLifecycleManifestWriteV1,
  RootLifecycleSupportClosureBuilderV1, RootLifecycleSupportLimitsV1, RootRetirementCommitWriteV1, decode_root_expiry_manifest_v1,
  decode_root_lifecycle_manifest_v1, decode_root_retirement_commit_v1, encode_root_candidate_record_v1, encode_root_expiry_manifest_v1,
  encode_root_expiry_record_v1, encode_root_lifecycle_manifest_v1, encode_root_retirement_commit_v1,
};
use aeordb::engine::v4::gc_root_transition::RootRetirementIntentV1;
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcPhysicalHintV1, GcStateArtifactV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, GcStatePageWriteV1,
  RootExpiryStateV1, decode_gc_state_artifact, encode_gc_state_directory_v1, encode_gc_state_page_v1,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::read_view::RootReadPinCoordinatorV1;
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31; 16];

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  let algorithm_name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("root lifecycle closure fixtures cover both frozen hash widths"),
  };
  fs::read(fixture_root().join(format!("agca-{algorithm_name}-{name}.bin"))).unwrap()
}

fn support_limits(maximum_support_artifacts: u64) -> RootLifecycleSupportLimitsV1 {
  RootLifecycleSupportLimitsV1 { maximum_candidate_records: 1, maximum_expiry_records: 2, maximum_support_artifacts }
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let entry = entry.unwrap();
    if entry.file_type().unwrap().is_dir() {
      rust_sources(&entry.path(), sources);
    } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
      sources.push(entry.path());
    }
  }
}

struct PreparedRetirementSupportV1 {
  retirement: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  expiry_page: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  expiry_directory: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  expiry_manifest: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  lifecycle_manifest: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  lifecycle_control: aeordb::engine::v4::gc::EncodedGcActiveControlV1,
}

fn prepare_retirement_support(algorithm: HashAlgorithm) -> PreparedRetirementSupportV1 {
  let namespace_root_hash = digest_parts(algorithm, &[b"retired namespace root"]);
  let admission_commit_payload_hash = digest_parts(algorithm, &[b"root admission commit"]);
  let prior_lifecycle_manifest_hash = digest_parts(algorithm, &[b"prior lifecycle manifest"]);
  prepare_retirement_support_for(algorithm, &namespace_root_hash, &admission_commit_payload_hash, &prior_lifecycle_manifest_hash)
}

fn prepare_retirement_support_for(
  algorithm: HashAlgorithm,
  namespace_root_hash: &[u8],
  admission_commit_payload_hash: &[u8],
  prior_lifecycle_manifest_hash: &[u8],
) -> PreparedRetirementSupportV1 {
  let authority_root_set_digest = digest_parts(algorithm, &[b"complete authority roots"]);
  let committed_at_ms = 1_700_000_100_000;
  let retirement = encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1 {
    hash_algorithm: algorithm,
    database_id: &DATABASE_ID,
    namespace_root_hash: &namespace_root_hash,
    retirement_id: &[0x81; 16],
    committed_at_ms,
    pending_since_ms: committed_at_ms - 86_400_000,
    grace_at_pending_ms: 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    prior_lifecycle_manifest_hash,
    authority_root_set_digest: &authority_root_set_digest,
    admission_commit_payload_hash: &admission_commit_payload_hash,
  })
  .unwrap();
  let expiry_record = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: &namespace_root_hash,
    retired_at_ms: committed_at_ms,
    last_pending_since_ms: committed_at_ms - 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    state: RootExpiryStateV1::LogicallyRetired,
    retirement_commit_hash: &retirement.key,
    root_object_reclaim_proof_hash: None,
    evidence_expires_at_ms: None,
  })
  .unwrap();
  let expiry_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &DATABASE_ID,
    catalog_id: &[0x71; 16],
    generation: 6,
    page_id: 1,
    records: &[&expiry_record],
  })
  .unwrap();
  let GcStateArtifactV1::Page(decoded_page) = decode_gc_state_artifact(&expiry_page.value, algorithm).unwrap() else {
    unreachable!();
  };
  let entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: decoded_page.lower_fence,
    upper_fence: decoded_page.upper_fence,
    child_hash: &expiry_page.key,
    child_generation: decoded_page.generation,
    live_count: u64::from(decoded_page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: decoded_page.logical_bytes,
    minimum_page_id: decoded_page.page_id,
    maximum_page_id: decoded_page.page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let expiry_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &DATABASE_ID,
    catalog_id: decoded_page.catalog_id,
    generation: 6,
    level: 0,
    entries: &entries,
  })
  .unwrap();
  let logical_bytes = u64::try_from(expiry_record.len()).unwrap();
  let expiry_manifest = encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &DATABASE_ID,
    generation: 6,
    retention_ms: 30 * 24 * 60 * 60 * 1_000,
    optional_byte_budget: 256 * 1024 * 1024,
    directory_root_hash: Some(&expiry_directory.key),
    next_page_id: 2,
    record_count: 1,
    logical_bytes,
    mandatory_count: 1,
    mandatory_bytes: logical_bytes,
    optional_count: 0,
    optional_bytes: 0,
    oldest_retired_at_ms: Some(committed_at_ms),
    newest_retired_at_ms: Some(committed_at_ms),
  })
  .unwrap();
  let lifecycle_manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &DATABASE_ID,
    generation: 6,
    published_at_ms: committed_at_ms + 1,
    source_complete_mark_generation: 5,
    authority_root_set_digest: &authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: Some(&expiry_manifest.key),
    next_page_id: 1,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: 1,
    candidate_bytes: 0,
    expiry_bytes: logical_bytes,
  })
  .unwrap();
  let lifecycle_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: aeordb::engine::v4::gc::GcArtifactKindV1::RootLifecycleActiveControl,
    hash_algorithm: algorithm,
    database_id: &DATABASE_ID,
    slot: 0,
    sequence: 1,
    generation: 6,
    target_manifest_hash: &lifecycle_manifest.key,
  })
  .unwrap();
  PreparedRetirementSupportV1 { retirement, expiry_page, expiry_directory, expiry_manifest, lifecycle_manifest, lifecycle_control }
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

fn publisher() -> (tempfile::TempDir, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("root-retirement.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let kv_block_length = initial_block_size() as u64;
  let header = initial_header(algorithm, kv_block_length);
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
  (directory, V4FirstAuthorityPublisher::new(kv, coordinator).unwrap())
}

struct StaticAuthorityVerifierV1 {
  called: bool,
  target_is_authoritative: bool,
  authority_root_set_digest: Vec<u8>,
}

impl RootRetirementAuthorityVerifierV1 for StaticAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    _request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    self.called = true;
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: self.target_is_authoritative,
      authority_root_set_digest: self.authority_root_set_digest.clone(),
    })
  }
}

fn first_authority_request() -> FirstAuthorityPublicationRequestV1 {
  let algorithm = HashAlgorithm::Blake3_256;
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
    typed_closure_digest: digest_parts(algorithm, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn root_candidate_page() -> aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let root_hash = digest_parts(algorithm, &[b"retiring root"]);
  let authority_digest = digest_parts(algorithm, &[b"authority roots"]);
  let admission_hash = digest_parts(algorithm, &[b"admission commit"]);
  let row = encode_root_candidate_record_v1(&RootCandidateRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: &root_hash,
    reason: 1,
    pending_since_ms: 1_700_000_000_000,
    first_unreachable_generation: 4,
    last_confirmed_unreachable_generation: 5,
    grace_at_pending_ms: 86_400_000,
    authority_root_set_digest: &authority_digest,
    admission_commit_payload_hash: &admission_hash,
  })
  .unwrap();
  encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootCandidates,
    database_id: &DATABASE_ID,
    catalog_id: &[0x71; 16],
    generation: 6,
    page_id: 1,
    records: &[&row],
  })
  .unwrap()
}

#[test]
fn support_publication_is_bounded_to_exact_lifecycle_pages_and_directories() {
  let (_directory, publisher) = publisher();
  publisher.publish(&first_authority_request()).unwrap();
  let page = root_candidate_page();

  let first = publisher
    .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifact: &page,
      publication_timestamp_ms: 1_700_000_100_000,
    })
    .unwrap();
  let retry = publisher
    .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifact: &page,
      publication_timestamp_ms: 1_700_000_100_000,
    })
    .unwrap();

  assert_eq!(first.artifact_key, page.key);
  assert_eq!(retry, first);
  assert!(publisher.locator(&page.key).unwrap().is_some());
}

#[test]
fn support_publication_rejects_wrong_identity_and_authority_artifacts_without_mutation() {
  let (_directory, publisher) = publisher();
  publisher.publish(&first_authority_request()).unwrap();
  let page = root_candidate_page();
  let before = publisher.observe().unwrap();

  let wrong_database = publisher
    .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
      database_id: &[0x32; 16],
      artifact: &page,
      publication_timestamp_ms: 1_700_000_100_000,
    })
    .unwrap_err();
  assert_eq!(wrong_database.code(), "root_lifecycle_support_identity");

  let mut wrong_key = page.clone();
  wrong_key.key.fill(0x91);
  let key_error = publisher
    .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifact: &wrong_key,
      publication_timestamp_ms: 1_700_000_100_000,
    })
    .unwrap_err();
  assert_eq!(key_error.code(), "root_lifecycle_support_identity");

  let algorithm = HashAlgorithm::Blake3_256;
  let retirement = encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1 {
    hash_algorithm: algorithm,
    database_id: &DATABASE_ID,
    namespace_root_hash: &digest_parts(algorithm, &[b"retiring root"]),
    retirement_id: &[0x81; 16],
    committed_at_ms: 1_700_000_100_000,
    pending_since_ms: 1_700_000_100_000,
    grace_at_pending_ms: 0,
    final_mark_generation: 5,
    reason: 1,
    prior_lifecycle_manifest_hash: &digest_parts(algorithm, &[b"prior lifecycle"]),
    authority_root_set_digest: &digest_parts(algorithm, &[b"authority roots"]),
    admission_commit_payload_hash: &digest_parts(algorithm, &[b"admission commit"]),
  })
  .unwrap();
  let kind_error = publisher
    .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifact: &retirement,
      publication_timestamp_ms: 1_700_000_100_000,
    })
    .unwrap_err();
  assert_eq!(kind_error.code(), "root_lifecycle_support_kind");

  assert_eq!(publisher.observe().unwrap(), before);
  assert!(publisher.locator(&page.key).unwrap().is_none());
  assert!(publisher.locator(&retirement.key).unwrap().is_none());
}

#[test]
fn lifecycle_support_closure_requires_exact_child_before_parent_graph_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
    let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
    let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
    let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
    let candidate_page = fixture(algorithm, "root-candidate-page-valid");
    let candidate_directory = fixture(algorithm, "root-candidates-directory-valid");
    let expiry_page = fixture(algorithm, "root-expiry-page-valid");
    let expiry_directory = fixture(algorithm, "root-expiry-directory-valid");
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap());
    let mut builder =
      RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &memory).unwrap();

    builder.observe_encoded(&candidate_page).unwrap();
    builder.observe_encoded(&candidate_directory).unwrap();
    builder.observe_encoded(&expiry_page).unwrap();
    builder.observe_encoded(&expiry_directory).unwrap();
    let closure = builder.finish().unwrap();

    assert_eq!(closure.hash_algorithm(), algorithm);
    assert_eq!(closure.database_id(), lifecycle.database_id);
    assert_eq!(closure.lifecycle_manifest_hash(), lifecycle.key);
    assert_eq!(closure.expiry_manifest_hash(), Some(expiry.key.as_slice()));
    assert_eq!(closure.candidate_directory_hash(), lifecycle.candidate_directory_hash);
    assert_eq!(closure.expiry_directory_hash(), expiry.directory_root_hash);
    assert_eq!(closure.lifecycle_generation(), lifecycle.generation);
    assert_eq!(closure.source_complete_mark_generation(), lifecycle.source_complete_mark_generation);
    assert_eq!(closure.support_artifact_count(), 4);
  }
}

#[test]
fn lifecycle_support_closure_binds_the_exact_retirement_commit_and_mandatory_expiry_row() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let prepared = prepare_retirement_support(algorithm);
    let lifecycle = decode_root_lifecycle_manifest_v1(&prepared.lifecycle_manifest.value, algorithm).unwrap();
    let expiry = decode_root_expiry_manifest_v1(&prepared.expiry_manifest.value, algorithm).unwrap();
    let retirement = decode_root_retirement_commit_v1(&prepared.retirement.value, algorithm).unwrap();
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap());
    let mut builder = RootLifecycleSupportClosureBuilderV1::new_for_retirement(
      &lifecycle,
      &expiry,
      &retirement,
      algorithm,
      &cancellation,
      RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
      &memory,
    )
    .unwrap();

    builder.observe_encoded(&prepared.expiry_page.value).unwrap();
    builder.observe_encoded(&prepared.expiry_directory.value).unwrap();
    let closure = builder.finish().unwrap();
    assert_eq!(closure.retirement_commit_hash(), Some(prepared.retirement.key.as_slice()));

    let mut wrong_retirement = retirement.clone();
    wrong_retirement.authority_root_set_digest = &prepared.retirement.key;
    let error = match RootLifecycleSupportClosureBuilderV1::new_for_retirement(
      &lifecycle,
      &expiry,
      &wrong_retirement,
      algorithm,
      &cancellation,
      RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
      &memory,
    ) {
      Ok(_) => panic!("mismatched retirement evidence unexpectedly produced a closure builder"),
      Err(error) => error,
    };
    assert_eq!(error.code(), "root_lifecycle_support_retirement_closure");
  }
}

#[test]
fn lifecycle_support_closure_latches_order_limit_kind_and_cancellation_failures() {
  let algorithm = HashAlgorithm::Blake3_256;
  let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
  let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
  let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
  let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
  let candidate_page = fixture(algorithm, "root-candidate-page-valid");
  let candidate_directory = fixture(algorithm, "root-candidates-directory-valid");
  let expiry_page = fixture(algorithm, "root-expiry-page-valid");
  let unsupported_page = fixture(algorithm, "candidate-page-valid");
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap());

  let cancellation = CancellationToken::new();
  let mut out_of_order =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &memory).unwrap();
  assert_eq!(out_of_order.observe_encoded(&candidate_directory).unwrap_err().code(), "root_lifecycle_support_artifact_order");
  assert_eq!(out_of_order.observe_encoded(&candidate_page).unwrap_err().code(), "root_lifecycle_support_failed");

  let cancellation = CancellationToken::new();
  let mut bounded =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(1), &memory).unwrap();
  bounded.observe_encoded(&candidate_page).unwrap();
  assert_eq!(bounded.observe_encoded(&candidate_directory).unwrap_err().code(), "root_lifecycle_support_artifact_limit");

  let cancellation = CancellationToken::new();
  let mut wrong_kind =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &memory).unwrap();
  assert_eq!(wrong_kind.observe_encoded(&unsupported_page).unwrap_err().code(), "root_lifecycle_support_artifact_kind");

  let cancellation = CancellationToken::new();
  let mut canceled =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &memory).unwrap();
  cancellation.cancel();
  assert_eq!(canceled.observe_encoded(&candidate_page).unwrap_err().code(), "root_lifecycle_support_canceled");
  assert_eq!(canceled.observe_encoded(&candidate_page).unwrap_err().code(), "root_lifecycle_support_failed");

  let cancellation = CancellationToken::new();
  let mut incomplete =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &memory).unwrap();
  incomplete.observe_encoded(&candidate_page).unwrap();
  incomplete.observe_encoded(&expiry_page).unwrap();
  assert_eq!(incomplete.finish().unwrap_err().code(), "root_lifecycle_support_manifest_closure");
}

#[test]
fn lifecycle_support_closure_rejects_descriptor_substitution_and_memory_pressure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
  let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
  let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
  let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
  let candidate_page_bytes = fixture(algorithm, "root-candidate-page-valid");
  let candidate_directory_bytes = fixture(algorithm, "root-candidates-directory-valid");
  let GcStateArtifactV1::Directory(candidate_directory) = decode_gc_state_artifact(&candidate_directory_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let descriptor = &candidate_directory.entries[0];
  let substituted_hash = digest_parts(algorithm, &[b"substituted child"]);
  let substituted_entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: descriptor.lower_fence,
    upper_fence: descriptor.upper_fence,
    child_hash: &substituted_hash,
    child_generation: descriptor.child_generation,
    live_count: descriptor.live_count,
    tombstone_count: descriptor.tombstone_count,
    page_count: descriptor.page_count,
    logical_bytes: descriptor.logical_bytes,
    minimum_page_id: descriptor.minimum_page_id,
    maximum_page_id: descriptor.maximum_page_id,
    physical_hint: descriptor.physical_hint,
  }];
  let substituted_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: candidate_directory.role,
    database_id: candidate_directory.database_id,
    catalog_id: candidate_directory.catalog_id,
    generation: candidate_directory.generation,
    level: candidate_directory.level,
    entries: &substituted_entries,
  })
  .unwrap();

  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap());
  let mut substituted =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &memory).unwrap();
  substituted.observe_encoded(&candidate_page_bytes).unwrap();
  assert_eq!(substituted.observe_encoded(&substituted_directory.value).unwrap_err().code(), "root_lifecycle_support_directory_closure",);

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  let mut constrained =
    RootLifecycleSupportClosureBuilderV1::new(&lifecycle, Some(&expiry), algorithm, &cancellation, support_limits(4), &constrained_memory)
      .unwrap();
  assert_eq!(constrained.observe_encoded(&candidate_page_bytes).unwrap_err().code(), "root_lifecycle_support_memory");
  assert_eq!(constrained.observe_encoded(&candidate_page_bytes).unwrap_err().code(), "root_lifecycle_support_failed");
}

#[test]
fn guarded_retirement_rejects_the_current_head_before_publishing_authority() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, mut publisher) = publisher();
  let first_authority = publisher.publish(&first_authority_request()).unwrap();
  let admission_commit_payload_hash = digest_parts(algorithm, &[&first_authority.admission_control]);
  let prior_lifecycle_manifest_hash = digest_parts(algorithm, &[b"prior lifecycle manifest"]);
  let prepared = prepare_retirement_support_for(
    algorithm,
    &first_authority.namespace_root.root_hash,
    &admission_commit_payload_hash,
    &prior_lifecycle_manifest_hash,
  );
  for artifact in [&prepared.expiry_page, &prepared.expiry_directory] {
    publisher
      .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
        database_id: &DATABASE_ID,
        artifact,
        publication_timestamp_ms: 1_700_000_100_001,
      })
      .unwrap();
  }
  let lifecycle = decode_root_lifecycle_manifest_v1(&prepared.lifecycle_manifest.value, algorithm).unwrap();
  let expiry = decode_root_expiry_manifest_v1(&prepared.expiry_manifest.value, algorithm).unwrap();
  let retirement = decode_root_retirement_commit_v1(&prepared.retirement.value, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
  let mut closure_builder = RootLifecycleSupportClosureBuilderV1::new_for_retirement(
    &lifecycle,
    &expiry,
    &retirement,
    algorithm,
    &cancellation,
    RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
    &memory,
  )
  .unwrap();
  closure_builder.observe_encoded(&prepared.expiry_page.value).unwrap();
  closure_builder.observe_encoded(&prepared.expiry_directory.value).unwrap();
  let support_closure = closure_builder.finish().unwrap();
  let intent = RootRetirementIntentV1 {
    namespace_root_hash: retirement.namespace_root_hash.to_vec(),
    committed_at_ms: retirement.committed_at_ms,
    pending_since_ms: retirement.pending_since_ms,
    grace_at_pending_ms: retirement.grace_at_pending_ms,
    final_mark_generation: retirement.final_mark_generation,
    reason: retirement.reason,
    prior_lifecycle_manifest_hash: retirement.prior_lifecycle_manifest_hash.to_vec(),
    authority_root_set_digest: retirement.authority_root_set_digest.to_vec(),
    admission_commit_payload_hash: retirement.admission_commit_payload_hash.to_vec(),
  };
  let pin_coordinator = RootReadPinCoordinatorV1::new(memory.clone(), algorithm, 16, 16).unwrap();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    DATABASE_ID,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(8, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut authority_verifier = StaticAuthorityVerifierV1 {
    called: false,
    target_is_authoritative: false,
    authority_root_set_digest: intent.authority_root_set_digest.clone(),
  };
  let before = publisher.observe().unwrap();

  let error = publisher
    .publish_root_retirement(
      RootRetirementPublicationRequestV1 {
        hash_algorithm: algorithm,
        intent: &intent,
        support_closure: &support_closure,
        retirement_commit: &prepared.retirement,
        expiry_manifest: &prepared.expiry_manifest,
        lifecycle_manifest: &prepared.lifecycle_manifest,
        lifecycle_control: &prepared.lifecycle_control,
        publication_timestamp_ms: 1_700_000_100_001,
        monotonic_now_ms: 1_700_000_100_001,
        cancellation: &cancellation,
        pin_coordinator: &pin_coordinator,
      },
      &mut authority_verifier,
      &mut retirement_owner,
    )
    .unwrap_err();

  assert_eq!(error.code(), "root_retirement_current_head");
  assert!(error.committed_receipt().is_none());
  assert!(!authority_verifier.called);
  assert_eq!(publisher.observe().unwrap(), before);
  assert!(publisher.locator(&prepared.retirement.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_none());
}

#[test]
fn root_retirement_publication_remains_confined_to_disconnected_first_authority() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let owner = source_root.join("engine/v4/first_authority.rs");
  let symbols = [
    "publish_root_retirement",
    "RootRetirementPublicationRequestV1",
    "publish_root_reclaim",
    "RootReclaimPublicationRequestV1",
    "RootRetirementAuthorityVerifierV1",
    "publish_root_lifecycle_support_artifact",
  ];
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let mut violations = Vec::new();
  for path in sources {
    if path == owner {
      continue;
    }
    let source = fs::read_to_string(&path).unwrap();
    for symbol in symbols {
      if source.contains(symbol) {
        violations.push((path.strip_prefix(&source_root).unwrap().to_owned(), symbol));
      }
    }
  }
  assert!(violations.is_empty(), "root-retirement publication escaped disconnected first-authority ownership: {violations:?}");
}
