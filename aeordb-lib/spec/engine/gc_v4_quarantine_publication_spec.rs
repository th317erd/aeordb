use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, PhysicalQuarantineSupportPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc::{EncodedImmutableGcArtifactV1, PhysicalIncarnationV1};
use aeordb::engine::v4::gc_quarantine::{
  CandidateDeltaOperationV1, CandidateDeltaRecordWriteV1, CandidateDeltaWriteV1, PhysicalQuarantineCandidateClassV1,
  PhysicalQuarantineCandidateWriteV1, QuarantineClosureLimitsV1, QuarantineClosureValidatorV1, QuarantineManifestWriteV1,
  decode_quarantine_manifest_v1, encode_candidate_delta_v1, encode_physical_quarantine_candidate_v1, encode_quarantine_manifest_v1,
};
use aeordb::engine::v4::gc_quarantine_publication::{
  PhysicalQuarantinePublicationQualificationRequestV1, qualify_physical_quarantine_publication_v1,
};
use aeordb::engine::v4::gc_quarantine_transition::{
  PhysicalQuarantineObservationV1, PhysicalQuarantineReachabilityV1, PhysicalQuarantineTransitionContextV1,
  PhysicalQuarantineTransitionModelV1, PhysicalQuarantineTransitionV1,
};
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcStateArtifactV1, GcStateManifestV1, GcStatePageWriteV1, decode_gc_state_artifact, encode_gc_state_page_v1,
};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::DiskKVStore;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40];

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
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

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("publication qualification covers both frozen GC hash widths"),
  }
}

fn sequence(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn capabilities() -> [u8; 32] {
  let mut value = [0u8; 32];
  for capability in [12usize, 13, 15, 17] {
    value[capability / 8] |= 1 << (capability % 8);
  }
  value
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap())
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
  let path = directory.path().join("quarantine-publication.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let kv_block_length = initial_block_size();
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

fn lifecycle_bytes(algorithm: HashAlgorithm) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-root-lifecycle-manifest-populated.bin", algorithm_name(algorithm)))).unwrap()
}

fn lifecycle_manifest(bytes: &[u8], algorithm: HashAlgorithm) -> GcStateManifestV1<'_> {
  let GcStateArtifactV1::Manifest(manifest) = decode_gc_state_artifact(bytes, algorithm).unwrap() else {
    panic!("the lifecycle fixture must decode as a manifest")
  };
  manifest
}

struct BasisV1 {
  authority: Vec<u8>,
  semantic: Vec<u8>,
  layout: Vec<u8>,
  mark: Vec<u8>,
}

impl BasisV1 {
  fn new(algorithm: HashAlgorithm) -> Self {
    let width = algorithm.hash_length();
    Self { authority: sequence(width, 0x51), semantic: sequence(width, 0x71), layout: sequence(width, 0x91), mark: sequence(width, 0xb1) }
  }
}

fn manifest_bytes(
  algorithm: HashAlgorithm,
  generation: u64,
  completed_at_ms: u64,
  basis: &BasisV1,
  lifecycle_hash: &[u8],
  counts: (u64, u64),
  delta_hashes: &[u8],
) -> Vec<u8> {
  let (candidate_count, eligible_count) = counts;
  let required_capabilities = capabilities();
  let record_bytes = u64::try_from(52 + 2 * algorithm.hash_length()).unwrap();
  encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: DATABASE_ID,
    mark_generation: generation,
    completed_at_ms,
    required_capabilities: &required_capabilities,
    authority_root_set_digest: &basis.authority,
    semantic_state_digest: &basis.semantic,
    kv_layout_fingerprint: &basis.layout,
    mark_result_digest: &basis.mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: lifecycle_hash,
    candidate_count,
    candidate_bytes: candidate_count * record_bytes,
    eligible_count_hint: eligible_count,
    eligible_bytes_hint: eligible_count * record_bytes,
    next_candidate_page_id: 1,
    delta_hashes,
  })
  .unwrap()
  .value
}

#[test]
fn completed_transition_qualifies_only_its_exact_incremental_delta_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let lifecycle_bytes = lifecycle_bytes(algorithm);
    let lifecycle = lifecycle_manifest(&lifecycle_bytes, algorithm);
    let prior_basis = BasisV1::new(algorithm);
    let prior_bytes = manifest_bytes(algorithm, 100, 1_000, &prior_basis, &lifecycle.key, (0, 0), &[]);
    let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
    let next_basis = BasisV1 {
      authority: sequence(algorithm.hash_length(), 0x52),
      semantic: sequence(algorithm.hash_length(), 0x72),
      layout: sequence(algorithm.hash_length(), 0x92),
      mark: sequence(algorithm.hash_length(), 0xb2),
    };
    let cancellation = CancellationToken::new();
    let mut model = PhysicalQuarantineTransitionModelV1::new(
      PhysicalQuarantineTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_manifest: &prior,
        mark_generation: 101,
        completed_at_ms: 2_000,
        current_configured_grace_ms: 60_000,
        authority_root_set_digest: &next_basis.authority,
        semantic_state_digest: &next_basis.semantic,
        kv_layout_fingerprint: &next_basis.layout,
        mark_result_digest: &next_basis.mark,
        captured_root_lifecycle_manifest: &lifecycle.key,
        maximum_incarnations: 1,
        maximum_candidates: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        mark_authority_healthy: true,
        physical_inventory_healthy: true,
        root_lifecycle_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let logical_key = sequence(algorithm.hash_length(), 0x11);
    let integrity = sequence(algorithm.hash_length(), 0x31);
    let transition = model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: PhysicalIncarnationV1 {
          logical_key: &logical_key,
          integrity_or_legacy_digest: &integrity,
          wal_offset: 4_096,
          write_sequence: 7,
          entity_length: 512,
          entry_type: 1,
          entity_version: 1,
        },
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
        },
      })
      .unwrap();
    let PhysicalQuarantineTransitionV1::CandidateStarted(candidate) = transition else {
      panic!("the first unreachable mark must emit one candidate set")
    };
    let records = [candidate.as_delta_write_request()];
    let delta = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
      hash_algorithm: algorithm,
      database_id: DATABASE_ID,
      mark_generation: 101,
      delta_ordinal: 1,
      previous_delta_hash: None,
      records: &records,
    })
    .unwrap();
    let transition_permit = model.finish_for_publication().unwrap();
    let next_bytes = manifest_bytes(algorithm, 101, 2_000, &next_basis, &lifecycle.key, (1, 0), &delta.key);
    let next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
    let mut closure = QuarantineClosureValidatorV1::new(
      &next,
      None,
      &lifecycle,
      algorithm,
      cancellation.clone(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: 4 },
      &memory_coordinator(),
    )
    .unwrap();
    closure.observe_delta(&delta.value).unwrap();
    let closure = closure.finish().unwrap();
    let permit = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition_permit,
      appended_delta: Some(&delta.value),
      cancellation: &cancellation,
    })
    .unwrap();
    assert_eq!(permit.next_manifest_hash(), next.key);
    assert_eq!(permit.prior_manifest_hash(), prior.key);
    assert_eq!((permit.mutation_count(), permit.resulting_candidate_count(), permit.eligible_count()), (1, 1, 0));

    let substituted_record = CandidateDeltaRecordWriteV1 {
      operation: records[0].operation,
      candidate: aeordb::engine::v4::gc_quarantine::PhysicalQuarantineCandidateWriteV1 {
        class: PhysicalQuarantineCandidateClassV1::ExpiredDerivedArtifact,
        ..records[0].candidate
      },
    };
    let substituted = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
      hash_algorithm: algorithm,
      database_id: DATABASE_ID,
      mark_generation: 101,
      delta_ordinal: 1,
      previous_delta_hash: None,
      records: &[substituted_record],
    })
    .unwrap();
    let substituted_manifest_bytes = manifest_bytes(algorithm, 101, 2_000, &next_basis, &lifecycle.key, (1, 0), &substituted.key);
    let substituted_manifest = decode_quarantine_manifest_v1(&substituted_manifest_bytes, algorithm).unwrap();
    let mut substituted_closure = QuarantineClosureValidatorV1::new(
      &substituted_manifest,
      None,
      &lifecycle,
      algorithm,
      cancellation.clone(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: 4 },
      &memory_coordinator(),
    )
    .unwrap();
    substituted_closure.observe_delta(&substituted.value).unwrap();
    let substituted_closure = substituted_closure.finish().unwrap();
    assert_eq!(
      qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
        prior_manifest: &prior,
        next_manifest: &substituted_manifest,
        support_closure: &substituted_closure,
        transition: &transition_permit,
        appended_delta: Some(&substituted.value),
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "quarantine_publication_mutations",
    );
  }
}

#[test]
fn omitted_delta_aggregate_drift_and_cancellation_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let lifecycle_bytes = lifecycle_bytes(algorithm);
  let lifecycle = lifecycle_manifest(&lifecycle_bytes, algorithm);
  let basis = BasisV1::new(algorithm);
  let prior_bytes = manifest_bytes(algorithm, 200, 1_000, &basis, &lifecycle.key, (0, 0), &[]);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let model = PhysicalQuarantineTransitionModelV1::new(
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_manifest: &prior,
      mark_generation: 201,
      completed_at_ms: 2_000,
      current_configured_grace_ms: 0,
      authority_root_set_digest: &basis.authority,
      semantic_state_digest: &basis.semantic,
      kv_layout_fingerprint: &basis.layout,
      mark_result_digest: &basis.mark,
      captured_root_lifecycle_manifest: &lifecycle.key,
      maximum_incarnations: 1,
      maximum_candidates: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      mark_authority_healthy: true,
      physical_inventory_healthy: true,
      root_lifecycle_healthy: true,
    },
    &cancellation,
  )
  .unwrap();
  let transition = model.finish_for_publication().unwrap();
  let next_bytes = manifest_bytes(algorithm, 201, 2_000, &basis, &lifecycle.key, (0, 0), &[]);
  let mut next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
  let closure = QuarantineClosureValidatorV1::new(
    &next,
    None,
    &lifecycle,
    algorithm,
    cancellation.clone(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 1 },
    &memory_coordinator(),
  )
  .unwrap()
  .finish()
  .unwrap();
  next.candidate_count = 1;
  assert_eq!(
    qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition,
      appended_delta: None,
      cancellation: &cancellation,
    })
    .unwrap_err()
    .code(),
    "quarantine_publication_closure",
  );

  let next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition,
      appended_delta: None,
      cancellation: &canceled,
    })
    .unwrap_err()
    .code(),
    "quarantine_publication_canceled",
  );
}

#[test]
fn support_publication_accepts_only_quarantine_pages_directories_and_deltas_without_selecting_authority() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, publisher) = publisher();
  publisher.publish(&first_authority_request()).unwrap();
  let before = publisher.observe().unwrap();
  let logical_key = digest_parts(algorithm, &[b"physical candidate logical key"]);
  let integrity = digest_parts(algorithm, &[b"physical candidate integrity"]);
  let candidate = PhysicalQuarantineCandidateWriteV1 {
    hash_algorithm: algorithm,
    incarnation: PhysicalIncarnationV1 {
      logical_key: &logical_key,
      integrity_or_legacy_digest: &integrity,
      wal_offset: 4_096,
      write_sequence: 7,
      entity_length: 512,
      entry_type: 1,
      entity_version: 1,
    },
    class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
    pending_since_ms: 1_700_000_000_000,
    first_unreachable_generation: 5,
    grace_at_pending_ms: 86_400_000,
  };
  let row = encode_physical_quarantine_candidate_v1(&candidate).unwrap();
  let page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::Candidates,
    database_id: &DATABASE_ID,
    catalog_id: &[0x71; 16],
    generation: 6,
    page_id: 1,
    records: &[&row],
  })
  .unwrap();
  let delta_record = CandidateDeltaRecordWriteV1 { operation: CandidateDeltaOperationV1::Set, candidate };
  let delta = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
    hash_algorithm: algorithm,
    database_id: DATABASE_ID,
    mark_generation: 6,
    delta_ordinal: 1,
    previous_delta_hash: None,
    records: &[delta_record],
  })
  .unwrap();
  let directory_bytes = fs::read(fixture_root().join("agca-blake3-256-candidates-directory-valid.bin")).unwrap();
  let directory_key = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap().key().to_vec();
  let candidate_directory = EncodedImmutableGcArtifactV1 { key: directory_key, value: directory_bytes };

  for artifact in [&page, &candidate_directory, &delta] {
    let first = publisher
      .publish_physical_quarantine_support_artifact(PhysicalQuarantineSupportPublicationRequestV1 {
        database_id: &DATABASE_ID,
        artifact,
        publication_timestamp_ms: 1_700_000_100_000,
      })
      .unwrap();
    let retry = publisher
      .publish_physical_quarantine_support_artifact(PhysicalQuarantineSupportPublicationRequestV1 {
        database_id: &DATABASE_ID,
        artifact,
        publication_timestamp_ms: 1_700_000_100_000,
      })
      .unwrap();
    assert_eq!(first, retry);
    assert_eq!(first.artifact_key, artifact.key);
    assert!(publisher.locator(&artifact.key).unwrap().is_some());
  }
  assert_eq!(publisher.observe().unwrap().selected.header.head_hash, before.selected.header.head_hash);

  let wrong_database_id = [0x99; 16];
  let error = publisher
    .publish_physical_quarantine_support_artifact(PhysicalQuarantineSupportPublicationRequestV1 {
      database_id: &wrong_database_id,
      artifact: &page,
      publication_timestamp_ms: 1_700_000_100_001,
    })
    .unwrap_err();
  assert_eq!(error.code(), "quarantine_support_identity");

  let lifecycle_bytes = lifecycle_bytes(algorithm);
  let lifecycle = lifecycle_manifest(&lifecycle_bytes, algorithm);
  let lifecycle_artifact = EncodedImmutableGcArtifactV1 { key: lifecycle.key.clone(), value: lifecycle_bytes };
  let error = publisher
    .publish_physical_quarantine_support_artifact(PhysicalQuarantineSupportPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifact: &lifecycle_artifact,
      publication_timestamp_ms: 1_700_000_100_001,
    })
    .unwrap_err();
  assert_eq!(error.code(), "quarantine_support_kind");
  assert!(publisher.locator(&lifecycle_artifact.key).unwrap().is_none());

  let root_page_bytes = fs::read(fixture_root().join("agca-blake3-256-root-candidate-page-valid.bin")).unwrap();
  let root_page_key = decode_gc_state_artifact(&root_page_bytes, algorithm).unwrap().key().to_vec();
  let root_page = EncodedImmutableGcArtifactV1 { key: root_page_key, value: root_page_bytes };
  let error = publisher
    .publish_physical_quarantine_support_artifact(PhysicalQuarantineSupportPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifact: &root_page,
      publication_timestamp_ms: 1_700_000_100_002,
    })
    .unwrap_err();
  assert_eq!(error.code(), "quarantine_support_kind");
  assert!(publisher.locator(&root_page.key).unwrap().is_none());
}

#[test]
fn quarantine_finalizer_remains_confined_to_disconnected_first_authority_without_physical_removal() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let mut finalizer_owners =
    sources.iter().filter(|path| fs::read_to_string(path).unwrap().contains("publish_physical_quarantine(")).cloned().collect::<Vec<_>>();
  finalizer_owners.sort();
  assert_eq!(finalizer_owners, vec![source_root.join("engine/v4/first_authority.rs")]);

  let authority_source = fs::read_to_string(&finalizer_owners[0]).unwrap();
  for forbidden in ["VoidManager", "remove_entry", "remove_locator", "run_gc", "server::", "DirectoryOps", "StorageEngine"] {
    assert!(!authority_source.contains(forbidden), "quarantine finalizer authority unexpectedly references {forbidden}");
  }
}
