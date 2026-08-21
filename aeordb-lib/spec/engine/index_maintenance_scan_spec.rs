use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_maintenance_scan::{
  IndexMaintenanceScanDocumentV1, IndexMaintenanceScanErrorV1, IndexMaintenanceScanLimitsV1, IndexMaintenanceScanPageV1,
  IndexMaintenanceScanReadErrorClassV1, IndexMaintenanceScanReadErrorV1, IndexMaintenanceScanReadV1, IndexMaintenanceScanRequestV1,
  IndexProducerServiceModeV1, derive_index_maintenance_document_operation_id_v1, index_producer_service_mode_v1,
  validate_index_maintenance_scan_page_v1,
};
use aeordb::engine::v4::index_producer_coordinator::IndexProducerTaskKindV1;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"maintenance-scan:", label])
}

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit_bytes - 1_024, hard_limit_bytes, 1, 512).unwrap())
}

fn file(path: &str, content: u8) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 32,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_001,
    metadata: vec![content; 8],
    content_hash: vec![content; 32],
    chunk_hashes: vec![vec![content.wrapping_add(1); 32]],
  }
}

fn request<'a>(root: &'a [u8], resume_after: Option<&'a str>, limits: IndexMaintenanceScanLimitsV1) -> IndexMaintenanceScanRequestV1<'a> {
  IndexMaintenanceScanRequestV1 { namespace_root: root, scope: "/docs", resume_after, limits, is_cancelled: &|| false }
}

#[test]
fn task_kind_dispatch_separates_journals_scans_retirement_and_compaction() {
  for kind in [IndexProducerTaskKindV1::MutationWindow, IndexProducerTaskKindV1::Reconcile] {
    assert_eq!(index_producer_service_mode_v1(kind), IndexProducerServiceModeV1::JournalTransition);
  }
  for kind in [
    IndexProducerTaskKindV1::Build,
    IndexProducerTaskKindV1::Rebuild,
    IndexProducerTaskKindV1::Repair,
    IndexProducerTaskKindV1::ExplicitMutation,
    IndexProducerTaskKindV1::LegacyMigration,
  ] {
    assert_eq!(index_producer_service_mode_v1(kind), IndexProducerServiceModeV1::AuthoritativeUpsertScan);
  }
  assert_eq!(index_producer_service_mode_v1(IndexProducerTaskKindV1::Retire), IndexProducerServiceModeV1::AuthoritativeRetirementScan);
  assert_eq!(index_producer_service_mode_v1(IndexProducerTaskKindV1::Compact), IndexProducerServiceModeV1::ArtifactCompaction);
}

#[test]
fn maintenance_resume_state_does_not_mutate_the_frozen_producer_task_payload() {
  let payload_source = include_str!("../../src/engine/v4/index_runtime_workspace_payload.rs");
  assert!(payload_source.contains("const PRODUCER_TASK_SCHEMA_VERSION: u16 = 1;"));
  assert!(!payload_source.contains("resume_after"));
  assert!(!payload_source.contains("IndexMaintenanceScan"));
}

#[test]
fn scan_limits_are_nonzero_and_bound_cursor_growth() {
  assert!(IndexMaintenanceScanLimitsV1::new(0, 1, 1).is_err());
  assert!(IndexMaintenanceScanLimitsV1::new(1, 0, 1).is_err());
  assert!(IndexMaintenanceScanLimitsV1::new(1, 1, 0).is_err());
  let limits = IndexMaintenanceScanLimitsV1::new(8, 64 * 1_024, 4 * 1_024).unwrap();
  assert_eq!(limits.maximum_documents(), 8);
  assert_eq!(limits.maximum_retained_bytes(), 64 * 1_024);
  assert_eq!(limits.maximum_path_bytes(), 4 * 1_024);
}

#[test]
fn every_database_hash_profile_supports_root_revision_and_document_identity_validation() {
  let limits = IndexMaintenanceScanLimitsV1::new(1, 64 * 1_024, 4 * 1_024).unwrap();
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let root = aeordb::engine::v4::hash::digest_parts(algorithm, &[b"maintenance-scan:root"]);
    let revision = aeordb::engine::v4::hash::digest_parts(algorithm, &[b"maintenance-scan:revision"]);
    let request = request(&root, None, limits);
    let page = IndexMaintenanceScanPageV1 {
      documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: revision.clone(), file_record: file("/docs/a.json", 1) }],
      next_resume_after: None,
      complete: true,
      retained_bytes: 4 * 1_024,
    };
    validate_index_maintenance_scan_page_v1(algorithm, &request, &page).unwrap();
    assert_ne!(
      derive_index_maintenance_document_operation_id_v1(
        algorithm,
        [0x31; 16],
        IndexProducerTaskKindV1::Rebuild,
        &root,
        &revision,
        "/docs/a.json",
      )
      .unwrap(),
      [0; 16]
    );
  }
}

#[test]
fn scan_source_failures_preserve_stable_class_code_and_context() {
  let cases = [
    (
      IndexMaintenanceScanReadErrorV1::cancelled("cancelled", "shutdown requested"),
      IndexMaintenanceScanReadErrorClassV1::Cancelled,
      "cancelled",
      "shutdown requested",
    ),
    (
      IndexMaintenanceScanReadErrorV1::retryable("memory_pressure", "task budget refused"),
      IndexMaintenanceScanReadErrorClassV1::Retryable,
      "memory_pressure",
      "task budget refused",
    ),
    (
      IndexMaintenanceScanReadErrorV1::corrupt("invalid_root", "directory descriptor disagrees"),
      IndexMaintenanceScanReadErrorClassV1::Corrupt,
      "invalid_root",
      "directory descriptor disagrees",
    ),
  ];
  for (error, class, code, context) in cases {
    assert_eq!(error.class(), class);
    assert_eq!(error.code(), code);
    assert_eq!(error.context(), context);
  }
}

#[test]
fn document_operation_identity_is_stable_per_exact_parent_root_revision_kind_and_path() {
  let parent = [0x31; 16];
  let root = hash(b"root");
  let revision = hash(b"revision");
  let expected = derive_index_maintenance_document_operation_id_v1(
    ALGORITHM,
    parent,
    IndexProducerTaskKindV1::Rebuild,
    &root,
    &revision,
    "/docs/a.json",
  )
  .unwrap();
  assert_eq!(hex::encode(expected), "1ed70d379756b5eb7071e53cf4969938");
  assert_eq!(
    expected,
    derive_index_maintenance_document_operation_id_v1(
      ALGORITHM,
      parent,
      IndexProducerTaskKindV1::Rebuild,
      &root,
      &revision,
      "/docs/a.json",
    )
    .unwrap()
  );
  let changed = [
    derive_index_maintenance_document_operation_id_v1(
      ALGORITHM,
      [0x32; 16],
      IndexProducerTaskKindV1::Rebuild,
      &root,
      &revision,
      "/docs/a.json",
    )
    .unwrap(),
    derive_index_maintenance_document_operation_id_v1(ALGORITHM, parent, IndexProducerTaskKindV1::Build, &root, &revision, "/docs/a.json")
      .unwrap(),
    derive_index_maintenance_document_operation_id_v1(
      ALGORITHM,
      parent,
      IndexProducerTaskKindV1::Rebuild,
      &hash(b"other-root"),
      &revision,
      "/docs/a.json",
    )
    .unwrap(),
    derive_index_maintenance_document_operation_id_v1(
      ALGORITHM,
      parent,
      IndexProducerTaskKindV1::Rebuild,
      &root,
      &hash(b"other-revision"),
      "/docs/a.json",
    )
    .unwrap(),
    derive_index_maintenance_document_operation_id_v1(
      ALGORITHM,
      parent,
      IndexProducerTaskKindV1::Rebuild,
      &root,
      &revision,
      "/docs/b.json",
    )
    .unwrap(),
  ];
  assert!(changed.into_iter().all(|operation_id| operation_id != expected));
  assert!(derive_index_maintenance_document_operation_id_v1(
    ALGORITHM,
    parent,
    IndexProducerTaskKindV1::Compact,
    &root,
    &revision,
    "/docs/a.json",
  )
  .is_err());
  assert!(matches!(
    derive_index_maintenance_document_operation_id_v1(
      ALGORITHM,
      parent,
      IndexProducerTaskKindV1::Rebuild,
      &root,
      &revision,
      "docs/not-canonical.json",
    ),
    Err(IndexMaintenanceScanErrorV1::InvalidRequest(_))
  ));
}

#[test]
fn a_valid_page_is_strictly_ordered_scoped_and_resumes_after_its_last_path() {
  let root = hash(b"root");
  let limits = IndexMaintenanceScanLimitsV1::new(2, 64 * 1_024, 4 * 1_024).unwrap();
  let request = request(&root, Some("/docs/a.json"), limits);
  let page = IndexMaintenanceScanPageV1 {
    documents: vec![
      IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"b"), file_record: file("/docs/b.json", 2) },
      IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"c"), file_record: file("/docs/c.json", 3) },
    ],
    next_resume_after: Some("/docs/c.json".to_string()),
    complete: false,
    retained_bytes: 4 * 1_024,
  };
  validate_index_maintenance_scan_page_v1(ALGORITHM, &request, &page).unwrap();
}

#[test]
fn malformed_unsorted_out_of_scope_and_nonadvancing_pages_fail_closed() {
  let root = hash(b"root");
  let limits = IndexMaintenanceScanLimitsV1::new(4, 64 * 1_024, 4 * 1_024).unwrap();
  let request = request(&root, Some("/docs/a.json"), limits);
  let cases = [
    IndexMaintenanceScanPageV1 {
      documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: file("/docs/a.json", 1) }],
      next_resume_after: Some("/docs/a.json".to_string()),
      complete: false,
      retained_bytes: 4 * 1_024,
    },
    IndexMaintenanceScanPageV1 {
      documents: vec![
        IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"c"), file_record: file("/docs/c.json", 3) },
        IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"b"), file_record: file("/docs/b.json", 2) },
      ],
      next_resume_after: Some("/docs/b.json".to_string()),
      complete: false,
      retained_bytes: 4 * 1_024,
    },
    IndexMaintenanceScanPageV1 {
      documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"elsewhere"), file_record: file("/elsewhere/a.json", 4) }],
      next_resume_after: Some("/elsewhere/a.json".to_string()),
      complete: false,
      retained_bytes: 4 * 1_024,
    },
  ];
  for page in cases {
    assert!(validate_index_maintenance_scan_page_v1(ALGORITHM, &request, &page).is_err());
  }

  let empty_incomplete = IndexMaintenanceScanPageV1 {
    documents: Vec::new(),
    next_resume_after: Some("/docs/a.json".to_string()),
    complete: false,
    retained_bytes: 0,
  };
  assert!(validate_index_maintenance_scan_page_v1(ALGORITHM, &request, &empty_incomplete).is_err());
}

#[test]
fn cancellation_identity_limits_and_reservation_mismatch_fail_before_consumption() {
  let root = hash(b"root");
  let limits = IndexMaintenanceScanLimitsV1::new(1, 1_024, 64).unwrap();
  let cancelled =
    IndexMaintenanceScanRequestV1 { namespace_root: &root, scope: "/docs", resume_after: None, limits, is_cancelled: &|| true };
  let page = IndexMaintenanceScanPageV1 { documents: Vec::new(), next_resume_after: None, complete: true, retained_bytes: 0 };
  assert!(validate_index_maintenance_scan_page_v1(ALGORITHM, &cancelled, &page).is_err());

  let wrong_root =
    IndexMaintenanceScanRequestV1 { namespace_root: &[1; 31], scope: "/docs", resume_after: None, limits, is_cancelled: &|| false };
  assert!(matches!(
    validate_index_maintenance_scan_page_v1(ALGORITHM, &wrong_root, &page),
    Err(IndexMaintenanceScanErrorV1::InvalidRequest(_))
  ));
  let invalid_scope =
    IndexMaintenanceScanRequestV1 { namespace_root: &root, scope: "docs", resume_after: None, limits, is_cancelled: &|| false };
  assert!(matches!(
    validate_index_maintenance_scan_page_v1(ALGORITHM, &invalid_scope, &page),
    Err(IndexMaintenanceScanErrorV1::InvalidRequest(_))
  ));
  let oversized_cursor = IndexMaintenanceScanRequestV1 {
    namespace_root: &root,
    scope: "/docs",
    resume_after: Some("/docs/this-path-is-deliberately-longer-than-sixty-four-bytes-to-hit-the-boundary.json"),
    limits,
    is_cancelled: &|| false,
  };
  assert!(validate_index_maintenance_scan_page_v1(ALGORITHM, &oversized_cursor, &page).is_err());

  let request = request(&root, None, limits);
  let too_many = IndexMaintenanceScanPageV1 {
    documents: vec![
      IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: file("/docs/a.json", 1) },
      IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"b"), file_record: file("/docs/b.json", 2) },
    ],
    next_resume_after: None,
    complete: true,
    retained_bytes: 1_024,
  };
  assert!(validate_index_maintenance_scan_page_v1(ALGORITHM, &request, &too_many).is_err());
  let underreported = IndexMaintenanceScanPageV1 {
    documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: file("/docs/a.json", 1) }],
    next_resume_after: None,
    complete: true,
    retained_bytes: 1,
  };
  assert!(validate_index_maintenance_scan_page_v1(ALGORITHM, &request, &underreported).is_err());

  let mut overallocated = file("/docs/a.json", 1);
  overallocated.metadata.reserve_exact(8 * 1_024);
  let capacity_underreported = IndexMaintenanceScanPageV1 {
    documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: overallocated }],
    next_resume_after: None,
    complete: true,
    retained_bytes: 1_024,
  };
  assert!(matches!(
    validate_index_maintenance_scan_page_v1(ALGORITHM, &request, &capacity_underreported),
    Err(IndexMaintenanceScanErrorV1::InvalidPage(_))
  ));

  let coordinator = memory(128 * 1_024);
  let page = IndexMaintenanceScanPageV1 {
    documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: file("/docs/a.json", 1) }],
    next_resume_after: None,
    complete: true,
    retained_bytes: 512,
  };
  let wrong_owner = coordinator.reserve(MemoryOwner::Query, 512, AdmissionClass::Workload).unwrap();
  assert!(IndexMaintenanceScanReadV1::new(ALGORITHM, &request, page.clone(), wrong_owner).is_err());
  let too_small = coordinator.reserve(MemoryOwner::Task, 256, AdmissionClass::Workload).unwrap();
  assert!(IndexMaintenanceScanReadV1::new(ALGORITHM, &request, page, too_small).is_err());
  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  assert_eq!(snapshot.owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn completed_pages_have_no_cursor_and_page_memory_is_retained_until_consumed() {
  let root = hash(b"root");
  let limits = IndexMaintenanceScanLimitsV1::new(2, 64 * 1_024, 4 * 1_024).unwrap();
  let request = request(&root, None, limits);
  let coordinator = memory(128 * 1_024);
  let reservation = coordinator.reserve(MemoryOwner::Task, 4 * 1_024, AdmissionClass::Workload).unwrap();
  let page = IndexMaintenanceScanPageV1 {
    documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: file("/docs/a.json", 1) }],
    next_resume_after: None,
    complete: true,
    retained_bytes: 4 * 1_024,
  };
  let read = IndexMaintenanceScanReadV1::new(ALGORITHM, &request, page, reservation).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 4 * 1_024);
  assert!(read.page().complete);
  drop(read);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);

  let reservation = coordinator.reserve(MemoryOwner::Task, 4 * 1_024, AdmissionClass::Workload).unwrap();
  let invalid = IndexMaintenanceScanPageV1 {
    documents: vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"a"), file_record: file("/docs/a.json", 1) }],
    next_resume_after: Some("/docs/a.json".to_string()),
    complete: true,
    retained_bytes: 4 * 1_024,
  };
  assert!(IndexMaintenanceScanReadV1::new(ALGORITHM, &request, invalid, reservation).is_err());
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}
