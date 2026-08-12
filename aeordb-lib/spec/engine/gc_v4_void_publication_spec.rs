use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::first_authority::{
  VoidCatalogPublicationRequestV1, VoidCatalogPublicationReceiptV1, VoidCatalogSupportPublicationReceiptV1,
  VoidCatalogSupportPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_void::{SweepVoidArtifactV1, decode_sweep_void_artifact};
use aeordb::engine::v4::gc_void_publication::{
  VoidCatalogClosureLimitsV1, VoidCatalogClosureValidatorV1, VoidCatalogPublicationAuthorityV1, VoidCatalogPublicationErrorV1,
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

fn publish_void_support(
  publisher: &V4FirstAuthorityPublisher,
  request: VoidCatalogSupportPublicationRequestV1<'_>,
) -> Result<VoidCatalogSupportPublicationReceiptV1, aeordb::engine::v4::first_authority::FirstAuthorityPublicationErrorV1> {
  publisher.publish_void_catalog_support_artifact(request)
}

fn publish_void_catalog(
  publisher: &mut V4FirstAuthorityPublisher,
  request: VoidCatalogPublicationRequestV1<'_>,
  authority: &mut dyn VoidCatalogPublicationAuthorityV1,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
) -> Result<VoidCatalogPublicationReceiptV1, VoidCatalogPublicationErrorV1> {
  publisher.publish_void_catalog(request, authority, retirement_owner)
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
fn complete_void_closures_stream_at_both_frozen_hash_widths() {
  for profile in ["blake3-256", "sha512"] {
    let algorithm = algorithm(profile);
    let source_manifest_bytes = fixture(profile, "void-catalog-source");
    let SweepVoidArtifactV1::VoidCatalog(source_manifest) = decode_sweep_void_artifact(&source_manifest_bytes, algorithm).unwrap() else {
      panic!("expected source Void catalog")
    };
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut source = VoidCatalogClosureValidatorV1::new(
      &source_manifest,
      algorithm,
      cancellation.clone(),
      VoidCatalogClosureLimitsV1 { maximum_support_artifacts: 8 },
      &memory,
    )
    .unwrap();
    source.observe_encoded(&fixture(profile, "void-extent-page-source")).unwrap();
    source.observe_encoded(&fixture(profile, "void-free-directory-source")).unwrap();
    let source = source.finish().unwrap();
    assert_eq!(source.support_artifact_count, 2);
    assert_eq!(source.free_extent_count, 2);
    assert_eq!(source.free_bytes, source_manifest.free_bytes);
    assert_eq!(source.outstanding_claim_count, 0);

    let outstanding_manifest_bytes = fixture(profile, "void-catalog-outstanding");
    let SweepVoidArtifactV1::VoidCatalog(outstanding_manifest) =
      decode_sweep_void_artifact(&outstanding_manifest_bytes, algorithm).unwrap()
    else {
      panic!("expected outstanding-claim Void catalog")
    };
    let mut outstanding = VoidCatalogClosureValidatorV1::new(
      &outstanding_manifest,
      algorithm,
      cancellation,
      VoidCatalogClosureLimitsV1 { maximum_support_artifacts: 8 },
      &memory,
    )
    .unwrap();
    outstanding.observe_encoded(&fixture(profile, "void-extent-page-remaining")).unwrap();
    outstanding.observe_encoded(&fixture(profile, "void-free-directory-remaining")).unwrap();
    outstanding.observe_encoded(&fixture(profile, "void-claim")).unwrap();
    outstanding.observe_encoded(&fixture(profile, "void-claims-directory")).unwrap();
    let outstanding = outstanding.finish().unwrap();
    assert_eq!(outstanding.support_artifact_count, 4);
    assert_eq!(outstanding.free_extent_count, outstanding_manifest.free_count);
    assert_eq!(outstanding.free_bytes, outstanding_manifest.free_bytes);
    assert_eq!(outstanding.outstanding_claim_count, outstanding_manifest.claim_count);
    assert_eq!(outstanding.claimed_bytes, outstanding_manifest.claimed_bytes);
  }
}

#[test]
fn void_closure_cancellation_and_limits_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("blake3-256", "void-catalog-source");
  let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&manifest_bytes, algorithm).unwrap() else {
    panic!("expected source Void catalog")
  };
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut canceled = VoidCatalogClosureValidatorV1::new(
    &manifest,
    algorithm,
    cancellation.clone(),
    VoidCatalogClosureLimitsV1 { maximum_support_artifacts: 8 },
    &memory,
  )
  .unwrap();
  cancellation.cancel();
  assert_eq!(canceled.observe_encoded(&fixture("blake3-256", "void-extent-page-source")).unwrap_err().code(), "void_closure_canceled");
  assert_eq!(canceled.finish().unwrap_err().code(), "void_closure_failed");

  let mut limited = VoidCatalogClosureValidatorV1::new(
    &manifest,
    algorithm,
    CancellationToken::new(),
    VoidCatalogClosureLimitsV1 { maximum_support_artifacts: 1 },
    &memory,
  )
  .unwrap();
  limited.observe_encoded(&fixture("blake3-256", "void-extent-page-source")).unwrap();
  assert_eq!(
    limited.observe_encoded(&fixture("blake3-256", "void-free-directory-source")).unwrap_err().code(),
    "void_closure_artifact_limit"
  );
  assert_eq!(limited.finish().unwrap_err().code(), "void_closure_failed");
}

#[test]
fn void_catalog_publication_has_one_disconnected_selector_last_owner() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut source_files = Vec::new();
  collect_rust_files(&source_root, &mut source_files);

  let support_owners: Vec<_> = source_files
    .iter()
    .filter(|path| fs::read_to_string(path).unwrap().contains("pub fn publish_void_catalog_support_artifact("))
    .collect();
  assert_eq!(support_owners.len(), 1);
  assert!(support_owners[0].ends_with("engine/v4/first_authority.rs"));

  let selector_owners: Vec<_> =
    source_files.iter().filter(|path| fs::read_to_string(path).unwrap().contains("pub fn publish_void_catalog(")).collect();
  assert_eq!(selector_owners.len(), 1);
  assert!(selector_owners[0].ends_with("engine/v4/first_authority.rs"));

  let source = fs::read_to_string(selector_owners[0]).unwrap();
  let method_start = source.find("pub fn publish_void_catalog(").unwrap();
  let method_tail = &source[method_start..];
  let method_end = method_tail.find("\n  ///").unwrap_or(method_tail.len());
  let method = &method_tail[..method_end];
  assert!(method.contains("verify_void_catalog_support_is_durable"));
  assert!(method.contains("recheck_void_catalog_publication_authority"));
  assert!(method.contains("publish_gc_active_control_locked"));
  assert!(method.find("publish_immutable_gc_artifact_locked").unwrap() < method.find("publish_gc_active_control_locked").unwrap());

  for forbidden in ["VoidManager", "replace_all", "find_void", "run_gc", "server::", "StorageEngine"] {
    assert!(!method.contains(forbidden), "P4-7b must not activate live {forbidden}");
  }

  let support_pointer: fn(
    &V4FirstAuthorityPublisher,
    VoidCatalogSupportPublicationRequestV1<'_>,
  ) -> Result<
    VoidCatalogSupportPublicationReceiptV1,
    aeordb::engine::v4::first_authority::FirstAuthorityPublicationErrorV1,
  > = publish_void_support;
  let selector_pointer: fn(
    &mut V4FirstAuthorityPublisher,
    VoidCatalogPublicationRequestV1<'_>,
    &mut dyn VoidCatalogPublicationAuthorityV1,
    &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<VoidCatalogPublicationReceiptV1, VoidCatalogPublicationErrorV1> = publish_void_catalog;
  assert_eq!(std::mem::size_of_val(&support_pointer), std::mem::size_of::<usize>());
  assert_eq!(std::mem::size_of_val(&selector_pointer), std::mem::size_of::<usize>());
}

#[test]
fn p4_7b_public_contract_exposes_no_allocator_or_claim_permit() {
  let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/first_authority.rs")).unwrap();
  let receipt_start = source.find("pub struct VoidCatalogPublicationReceiptV1").unwrap();
  let receipt_tail = &source[receipt_start..];
  let receipt_end = receipt_tail.find("\n}").unwrap();
  let receipt = &receipt_tail[..receipt_end];
  for forbidden in ["Allocator", "AllocationPermit", "VoidClaim", "claimed_extents", "reusable_extents"] {
    assert!(!receipt.contains(forbidden), "publication receipt must not expose {forbidden}");
  }
}
