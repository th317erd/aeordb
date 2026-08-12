use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::first_authority::{V4FirstAuthorityPublisher, VoidClaimAdmissionPermitV1, VoidClaimAdmissionRequestV1};
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcPhysicalHintV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, encode_gc_state_directory_v1,
};
use aeordb::engine::v4::gc_void::{
  SweepVoidArtifactV1, VoidCatalogManifestWriteV1, VoidClaimExtentV1, VoidClaimWriteV1, VoidExtentPageWriteV1, VoidExtentRecordV1,
  decode_sweep_void_artifact, encode_void_catalog_manifest_v1, encode_void_claim_v1, encode_void_extent_page_v1,
};
use aeordb::engine::v4::gc_void_claim::{
  VoidClaimAdmissionAuthorityV1, VoidClaimAdmissionErrorV1, VoidClaimTransitionLimitsV1, VoidClaimTransitionValidatorV1,
};
use aeordb::engine::v4::gc_retirement::RetirementJournalOwnerV1;
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(profile: &str, name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{profile}-{name}.bin"))).unwrap()
}

fn algorithm(profile: &str) -> HashAlgorithm {
  match profile {
    "blake3-256" => HashAlgorithm::Blake3_256,
    "sha512" => HashAlgorithm::Sha512,
    _ => panic!("unexpected fixture profile {profile}"),
  }
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap())
}

fn admit_void_claim(
  publisher: &mut V4FirstAuthorityPublisher,
  request: VoidClaimAdmissionRequestV1<'_>,
  authority: &mut dyn VoidClaimAdmissionAuthorityV1,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
) -> Result<VoidClaimAdmissionPermitV1, VoidClaimAdmissionErrorV1> {
  publisher.admit_void_claim(request, authority, retirement_owner)
}

fn collect_rust_files(root: &Path, output: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      collect_rust_files(&path, output);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      output.push(path);
    }
  }
}

#[test]
fn frozen_source_to_outstanding_claim_transition_validates_at_both_hash_widths() {
  for profile in ["blake3-256", "sha512"] {
    let algorithm = algorithm(profile);
    let source_bytes = fixture(profile, "void-catalog-source");
    let result_bytes = fixture(profile, "void-catalog-outstanding");
    let claim_bytes = fixture(profile, "void-claim");
    let source = decode_sweep_void_artifact(&source_bytes, algorithm).unwrap();
    let result = decode_sweep_void_artifact(&result_bytes, algorithm).unwrap();
    let claim = decode_sweep_void_artifact(&claim_bytes, algorithm).unwrap();
    let memory = memory_coordinator();
    let mut validator = VoidClaimTransitionValidatorV1::new(
      &source,
      &result,
      &claim,
      CancellationToken::new(),
      VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
      &memory,
    )
    .unwrap();

    validator.observe_source_encoded(&fixture(profile, "void-extent-page-source")).unwrap();
    validator.observe_source_encoded(&fixture(profile, "void-free-directory-source")).unwrap();
    validator.finish_source().unwrap();
    validator.observe_result_encoded(&fixture(profile, "void-extent-page-remaining")).unwrap();
    validator.observe_result_encoded(&fixture(profile, "void-free-directory-remaining")).unwrap();
    validator.observe_result_encoded(&claim_bytes).unwrap();
    validator.observe_result_encoded(&fixture(profile, "void-claims-directory")).unwrap();
    let summary = validator.finish().unwrap();

    assert_eq!(summary.claim_key, claim.key());
    assert_eq!(summary.source_manifest_key, source.key());
    assert_eq!(summary.result_manifest_key, result.key());
    assert_eq!(summary.claimed_extent_count, 1);
    assert_eq!(summary.claimed_bytes, 4_097);
  }
}

#[test]
fn partial_claim_splits_free_extent_and_preserves_every_prior_claim() {
  let profile = "blake3-256";
  let algorithm = algorithm(profile);
  let source_bytes = fixture(profile, "void-catalog-outstanding");
  let source_free_page_bytes = fixture(profile, "void-extent-page-remaining");
  let existing_claim_bytes = fixture(profile, "void-claim");
  let SweepVoidArtifactV1::VoidCatalog(source_manifest) = decode_sweep_void_artifact(&source_bytes, algorithm).unwrap() else {
    panic!("expected outstanding source catalog")
  };
  let SweepVoidArtifactV1::VoidExtentPage(source_page) = decode_sweep_void_artifact(&source_free_page_bytes, algorithm).unwrap() else {
    panic!("expected remaining free page")
  };
  let source_extent = source_page.extent_records().unwrap().next().unwrap().unwrap();
  let database_id: [u8; 16] = source_manifest.database_id.try_into().unwrap();
  let claim_id = [0xf1; 16];
  let requesting_boot_id = [0xb1; 16];
  let requesting_task_or_batch_id = [0xc1; 16];
  let claimed_offset = source_extent.offset + 100;
  let claimed_length = 100;
  let claim = encode_void_claim_v1(&VoidClaimWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    claim_id: &claim_id,
    generation: 3,
    created_at_ms: source_manifest.published_at_ms + 1,
    requesting_boot_id: &requesting_boot_id,
    requesting_task_or_batch_id: &requesting_task_or_batch_id,
    source_manifest_hash: &source_manifest.key,
    extents: &[VoidClaimExtentV1 {
      offset: claimed_offset,
      length: claimed_length,
      origin_sweep_proposal_hash: source_extent.origin_sweep_proposal_hash,
    }],
  })
  .unwrap();
  let result_catalog_id = [0xd1; 16];
  let result_free_extents = [
    VoidExtentRecordV1 { length: 100, ..source_extent },
    VoidExtentRecordV1 { offset: claimed_offset + u64::from(claimed_length), length: source_extent.length - 200, ..source_extent },
  ];
  let result_free_page = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    catalog_id: &result_catalog_id,
    generation: 3,
    page_id: source_manifest.next_page_id,
    extents: &result_free_extents,
  })
  .unwrap();
  let lower_free_fence = result_free_extents[0].offset.to_le_bytes();
  let upper_free_fence = result_free_extents[1].offset.to_le_bytes();
  let result_free_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::FreeExtents,
    database_id: &database_id,
    catalog_id: &result_catalog_id,
    generation: 3,
    level: 0,
    entries: &[GcStateDirectoryEntryWriteV1 {
      lower_fence: &lower_free_fence,
      upper_fence: &upper_free_fence,
      child_hash: &result_free_page.key,
      child_generation: 3,
      live_count: 2,
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: source_manifest.free_bytes - u64::from(claimed_length),
      minimum_page_id: source_manifest.next_page_id,
      maximum_page_id: source_manifest.next_page_id,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let SweepVoidArtifactV1::VoidClaim(existing_claim) = decode_sweep_void_artifact(&existing_claim_bytes, algorithm).unwrap() else {
    panic!("expected existing claim")
  };
  let result_claim_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::Claims,
    database_id: &database_id,
    catalog_id: &[0xe1; 16],
    generation: 3,
    level: 0,
    entries: &[
      GcStateDirectoryEntryWriteV1 {
        lower_fence: existing_claim.claim_id,
        upper_fence: existing_claim.claim_id,
        child_hash: &existing_claim.key,
        child_generation: existing_claim.generation,
        live_count: 1,
        tombstone_count: 0,
        page_count: 0,
        logical_bytes: existing_claim.stored_length,
        minimum_page_id: 0,
        maximum_page_id: 0,
        physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
      },
      GcStateDirectoryEntryWriteV1 {
        lower_fence: &claim_id,
        upper_fence: &claim_id,
        child_hash: &claim.key,
        child_generation: 3,
        live_count: 1,
        tombstone_count: 0,
        page_count: 0,
        logical_bytes: u64::try_from(claim.value.len()).unwrap(),
        minimum_page_id: 0,
        maximum_page_id: 0,
        physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
      },
    ],
  })
  .unwrap();
  let result_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: 3,
    published_at_ms: source_manifest.published_at_ms + 2,
    free_root: Some(&result_free_directory.key),
    claim_root: Some(&result_claim_directory.key),
    next_page_id: source_manifest.next_page_id + 1,
    free_count: 2,
    free_bytes: source_manifest.free_bytes - u64::from(claimed_length),
    claim_count: source_manifest.claim_count + 1,
    claimed_bytes: source_manifest.claimed_bytes + u64::from(claimed_length),
    previous_control_sequence: 2,
  })
  .unwrap();
  let source = SweepVoidArtifactV1::VoidCatalog(source_manifest);
  let result = decode_sweep_void_artifact(&result_manifest.value, algorithm).unwrap();
  let claim_artifact = decode_sweep_void_artifact(&claim.value, algorithm).unwrap();
  let memory = memory_coordinator();
  let mut validator = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &claim_artifact,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  validator.observe_source_encoded(&source_free_page_bytes).unwrap();
  validator.observe_source_encoded(&fixture(profile, "void-free-directory-remaining")).unwrap();
  validator.observe_source_encoded(&existing_claim_bytes).unwrap();
  validator.observe_source_encoded(&fixture(profile, "void-claims-directory")).unwrap();
  validator.finish_source().unwrap();
  validator.observe_result_encoded(&result_free_page.value).unwrap();
  validator.observe_result_encoded(&result_free_directory.value).unwrap();
  validator.observe_result_encoded(&existing_claim_bytes).unwrap();
  validator.observe_result_encoded(&claim.value).unwrap();
  validator.observe_result_encoded(&result_claim_directory.value).unwrap();
  let summary = validator.finish().unwrap();
  assert_eq!(summary.claimed_extent_count, 1);
  assert_eq!(summary.claimed_bytes, 100);
  assert_eq!(summary.result_closure.free_extent_count, 2);
  assert_eq!(summary.result_closure.outstanding_claim_count, 2);
}

#[test]
fn claim_transition_cancellation_limits_and_substitution_fail_closed() {
  let profile = "blake3-256";
  let algorithm = algorithm(profile);
  let source_bytes = fixture(profile, "void-catalog-source");
  let result_bytes = fixture(profile, "void-catalog-outstanding");
  let claim_bytes = fixture(profile, "void-claim");
  let source = decode_sweep_void_artifact(&source_bytes, algorithm).unwrap();
  let result = decode_sweep_void_artifact(&result_bytes, algorithm).unwrap();
  let claim = decode_sweep_void_artifact(&claim_bytes, algorithm).unwrap();
  let memory = memory_coordinator();

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  assert_eq!(
    VoidClaimTransitionValidatorV1::new(
      &source,
      &result,
      &claim,
      CancellationToken::new(),
      VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
      &constrained_memory,
    )
    .unwrap_err()
    .code(),
    "void_claim_transition_memory"
  );

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    VoidClaimTransitionValidatorV1::new(
      &source,
      &result,
      &claim,
      canceled,
      VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
      &memory,
    )
    .unwrap_err()
    .code(),
    "void_claim_transition_canceled"
  );

  let cancellation = CancellationToken::new();
  let mut interrupted = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &claim,
    cancellation.clone(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  interrupted.observe_source_encoded(&fixture(profile, "void-extent-page-source")).unwrap();
  cancellation.cancel();
  assert_eq!(
    interrupted.observe_source_encoded(&fixture(profile, "void-free-directory-source")).unwrap_err().code(),
    "void_claim_transition_canceled"
  );
  assert_eq!(interrupted.finish_source().unwrap_err().code(), "void_claim_transition_failed");

  let mut limited = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &claim,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 1 },
    &memory,
  )
  .unwrap();
  limited.observe_source_encoded(&fixture(profile, "void-extent-page-source")).unwrap();
  assert_eq!(
    limited.observe_source_encoded(&fixture(profile, "void-free-directory-source")).unwrap_err().code(),
    "void_closure_artifact_limit"
  );
  assert_eq!(limited.finish_source().unwrap_err().code(), "void_claim_transition_failed");

  let mut unavailable = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &claim,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  assert_eq!(unavailable.finish_source().unwrap_err().code(), "void_claim_transition_unavailable");

  let mut wrong_phase = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &claim,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  assert_eq!(
    wrong_phase.observe_result_encoded(&fixture(profile, "void-extent-page-remaining")).unwrap_err().code(),
    "void_claim_transition_phase"
  );
  assert_eq!(wrong_phase.finish_source().unwrap_err().code(), "void_claim_transition_failed");

  let mut substituted = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &claim,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  substituted.observe_source_encoded(&fixture(profile, "void-extent-page-source")).unwrap();
  substituted.observe_source_encoded(&fixture(profile, "void-free-directory-source")).unwrap();
  substituted.finish_source().unwrap();
  substituted.observe_result_encoded(&fixture(profile, "void-extent-page-source")).unwrap();
  substituted.observe_result_encoded(&fixture(profile, "void-free-directory-source")).unwrap();
  substituted.observe_result_encoded(&claim_bytes).unwrap();
  substituted.observe_result_encoded(&fixture(profile, "void-claims-directory")).unwrap();
  assert!(matches!(
    substituted.finish().unwrap_err().code(),
    "void_closure_root_mismatch" | "void_closure_manifest_totals" | "void_claim_transition_result"
  ));

  let SweepVoidArtifactV1::VoidCatalog(source_manifest) = &source else {
    panic!("expected source catalog")
  };
  let source_page_bytes = fixture(profile, "void-extent-page-source");
  let SweepVoidArtifactV1::VoidExtentPage(source_page) = decode_sweep_void_artifact(&source_page_bytes, algorithm).unwrap() else {
    panic!("expected source extent page")
  };
  let source_extent = source_page.extent_records().unwrap().next().unwrap().unwrap();
  let database_id: [u8; 16] = source_manifest.database_id.try_into().unwrap();
  let wrong_provenance = vec![0x99; algorithm.hash_length()];
  let substituted_claim = encode_void_claim_v1(&VoidClaimWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    claim_id: &[0x71; 16],
    generation: source_manifest.generation + 1,
    created_at_ms: source_manifest.published_at_ms + 1,
    requesting_boot_id: &[0x72; 16],
    requesting_task_or_batch_id: &[0x73; 16],
    source_manifest_hash: &source_manifest.key,
    extents: &[VoidClaimExtentV1 {
      offset: source_extent.offset,
      length: source_extent.length,
      origin_sweep_proposal_hash: &wrong_provenance,
    }],
  })
  .unwrap();
  let substituted_claim_artifact = decode_sweep_void_artifact(&substituted_claim.value, algorithm).unwrap();
  let mut bad_provenance = VoidClaimTransitionValidatorV1::new(
    &source,
    &result,
    &substituted_claim_artifact,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  assert_eq!(bad_provenance.observe_source_encoded(&source_page_bytes).unwrap_err().code(), "void_claim_transition_unavailable");

  let overlapping_claim = encode_void_claim_v1(&VoidClaimWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    claim_id: &[0x74; 16],
    generation: source_manifest.generation + 1,
    created_at_ms: source_manifest.published_at_ms + 1,
    requesting_boot_id: &[0x75; 16],
    requesting_task_or_batch_id: &[0x76; 16],
    source_manifest_hash: &source_manifest.key,
    extents: &[
      VoidClaimExtentV1 { offset: source_extent.offset, length: 100, origin_sweep_proposal_hash: source_extent.origin_sweep_proposal_hash },
      VoidClaimExtentV1 {
        offset: source_extent.offset + 50,
        length: 100,
        origin_sweep_proposal_hash: source_extent.origin_sweep_proposal_hash,
      },
    ],
  });
  assert!(overlapping_claim.is_err());

  let outstanding_bytes = fixture(profile, "void-catalog-outstanding");
  let outstanding_free_bytes = fixture(profile, "void-extent-page-remaining");
  let existing_claim_bytes = fixture(profile, "void-claim");
  let SweepVoidArtifactV1::VoidCatalog(outstanding_manifest) = decode_sweep_void_artifact(&outstanding_bytes, algorithm).unwrap() else {
    panic!("expected outstanding catalog")
  };
  let SweepVoidArtifactV1::VoidExtentPage(outstanding_page) = decode_sweep_void_artifact(&outstanding_free_bytes, algorithm).unwrap()
  else {
    panic!("expected outstanding free page")
  };
  let outstanding_extent = outstanding_page.extent_records().unwrap().next().unwrap().unwrap();
  let SweepVoidArtifactV1::VoidClaim(existing_claim) = decode_sweep_void_artifact(&existing_claim_bytes, algorithm).unwrap() else {
    panic!("expected existing claim")
  };
  let duplicate_claim = encode_void_claim_v1(&VoidClaimWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    claim_id: existing_claim.claim_id.try_into().unwrap(),
    generation: outstanding_manifest.generation + 1,
    created_at_ms: outstanding_manifest.published_at_ms + 1,
    requesting_boot_id: &[0x77; 16],
    requesting_task_or_batch_id: &[0x78; 16],
    source_manifest_hash: &outstanding_manifest.key,
    extents: &[VoidClaimExtentV1 {
      offset: outstanding_extent.offset,
      length: outstanding_extent.length,
      origin_sweep_proposal_hash: outstanding_extent.origin_sweep_proposal_hash,
    }],
  })
  .unwrap();
  let duplicate_result = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: outstanding_manifest.generation + 1,
    published_at_ms: outstanding_manifest.published_at_ms + 2,
    free_root: None,
    claim_root: None,
    next_page_id: outstanding_manifest.next_page_id,
    free_count: 0,
    free_bytes: 0,
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: 2,
  })
  .unwrap();
  let outstanding = decode_sweep_void_artifact(&outstanding_bytes, algorithm).unwrap();
  let duplicate_result_artifact = decode_sweep_void_artifact(&duplicate_result.value, algorithm).unwrap();
  let duplicate_claim_artifact = decode_sweep_void_artifact(&duplicate_claim.value, algorithm).unwrap();
  let mut duplicate = VoidClaimTransitionValidatorV1::new(
    &outstanding,
    &duplicate_result_artifact,
    &duplicate_claim_artifact,
    CancellationToken::new(),
    VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 8 },
    &memory,
  )
  .unwrap();
  duplicate.observe_source_encoded(&outstanding_free_bytes).unwrap();
  assert_eq!(duplicate.observe_source_encoded(&existing_claim_bytes).unwrap_err().code(), "void_claim_transition_duplicate_claim");
}

#[test]
fn claim_admission_has_one_disconnected_selector_last_owner_and_private_permit() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut source_files = Vec::new();
  collect_rust_files(&source_root, &mut source_files);
  let owners: Vec<_> = source_files.iter().filter(|path| fs::read_to_string(path).unwrap().contains("pub fn admit_void_claim(")).collect();
  assert_eq!(owners.len(), 1);
  assert!(owners[0].ends_with("engine/v4/first_authority.rs"));

  let source = fs::read_to_string(owners[0]).unwrap();
  let method_start = source.find("pub fn admit_void_claim(").unwrap();
  let method_tail = &source[method_start..];
  let method_end = method_tail.find("\n  ///").unwrap_or(method_tail.len());
  let method = &method_tail[..method_end];
  assert!(method.contains("verify_void_claim_transition_is_durable"));
  assert!(method.contains("recheck_void_claim_admission_authority"));
  assert!(method.contains("publish_gc_active_control_locked"));
  assert!(method.find("publish_immutable_gc_artifact_locked").unwrap() < method.find("publish_gc_active_control_locked").unwrap());
  for forbidden in ["VoidManager", "replace_all", "find_void", "run_gc", "server::", "StorageEngine"] {
    assert!(!method.contains(forbidden), "P4-7c must not activate live {forbidden}");
  }

  let support_method_start = source.find("pub fn publish_void_catalog_support_artifact(").unwrap();
  let support_method_tail = &source[support_method_start..];
  let support_method_end = support_method_tail.find("\n  ///").unwrap_or(support_method_tail.len());
  let support_method = &support_method_tail[..support_method_end];
  assert!(support_method.contains("VoidClaim(claim)"));
  assert!(support_method.contains("rejects claim publication outside claim admission"));

  let permit_start = source.find("pub struct VoidClaimAdmissionPermitV1").unwrap();
  let permit_tail = &source[permit_start..];
  let permit_end = permit_tail.find("\n}").unwrap();
  let permit = &permit_tail[..permit_end];
  assert!(!permit.lines().skip(1).any(|line| line.trim_start().starts_with("pub ")), "claim permit fields must remain private");

  let admission_pointer: fn(
    &mut V4FirstAuthorityPublisher,
    VoidClaimAdmissionRequestV1<'_>,
    &mut dyn VoidClaimAdmissionAuthorityV1,
    &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<VoidClaimAdmissionPermitV1, VoidClaimAdmissionErrorV1> = admit_void_claim;
  assert_eq!(std::mem::size_of_val(&admission_pointer), std::mem::size_of::<usize>());
}
