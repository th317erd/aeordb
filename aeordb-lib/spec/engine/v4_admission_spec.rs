use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::admission::{
  AdmissionModeV1, BinaryCapabilityProfileV1, CapabilitySetV1, PeerCapabilityAdmissionV1, PeerCapabilityViewV1, PhysicalIdentityEvidenceV1,
  V4AdmissionError, V4AdmissionResult, admit_peer_capabilities_v4, admit_v4_header,
};
use aeordb::engine::v4::database_header::{DatabaseHeaderV4, SelectedDatabaseHeaderV4, decode_header_region};
use aeordb::engine::v4::system_family::{
  SystemFamilyClassificationV1, SystemFamilySubjectV1, classify_system_family, decode_system_family_registry,
  require_complete_system_family,
};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn header(name: &str) -> SelectedDatabaseHeaderV4 {
  decode_header_region(&fs::read(fixture_root().join("database-header-v4").join(name)).unwrap()).unwrap()
}

fn all_capabilities() -> CapabilitySetV1 {
  CapabilitySetV1::from_bits(0..24).unwrap()
}

fn full_profile() -> BinaryCapabilityProfileV1 {
  BinaryCapabilityProfileV1::new(all_capabilities(), all_capabilities())
}

fn identity_evidence(header: &DatabaseHeaderV4) -> PhysicalIdentityEvidenceV1 {
  PhysicalIdentityEvidenceV1::ExistingInstance {
    physical_instance_id: header.physical_instance_id,
    previous_writer_fence_epoch: header.writer_fence_epoch - 1,
  }
}

#[test]
fn selected_v4_headers_require_exact_capabilities_and_embedded_registry() {
  for name in ["header-blake3-256-valid-ab.bin", "header-sha512-valid-ab.bin"] {
    let selected = header(name);
    let admitted = admit_v4_header(&selected, AdmissionModeV1::SemanticReadOnly, BinaryCapabilityProfileV1::current(), None).unwrap();
    let V4AdmissionResult::SemanticReadOnly(read) = admitted else {
      panic!("expected semantic read-only admission")
    };
    assert_eq!(read.registry.bytes.len(), 3_155);
    assert_eq!(read.registry.operational_fingerprint, selected.header.system_family_registry_fingerprint);
    assert_eq!(read.selected_slot, selected.selected_slot);
  }

  let registry_bytes = fs::read(fixture_root().join("../system-family-registry-v1.bin")).unwrap();
  let mut selected = header("header-blake3-256-valid-ab.bin");
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    selected.header.hash_algorithm = algorithm;
    selected.header.system_family_registry_fingerprint =
      decode_system_family_registry(&registry_bytes, algorithm).unwrap().operational_fingerprint;
    assert!(matches!(
      admit_v4_header(&selected, AdmissionModeV1::SemanticReadOnly, BinaryCapabilityProfileV1::current(), None).unwrap(),
      V4AdmissionResult::SemanticReadOnly(_)
    ));
  }
}

#[test]
fn diagnostics_can_inspect_but_never_semantically_admit_unsupported_headers() {
  let mut selected = header("header-blake3-256-valid-ab.bin");
  selected.header.required_reader_capabilities = CapabilitySetV1::from_bytes(selected.header.required_reader_capabilities)
    .unwrap()
    .union(CapabilitySetV1::from_bits([23]).unwrap())
    .into_bytes();
  let profile = BinaryCapabilityProfileV1::new(CapabilitySetV1::v4_baseline(), CapabilitySetV1::empty());

  let error = admit_v4_header(&selected, AdmissionModeV1::SemanticReadOnly, profile, None).unwrap_err();
  assert_eq!(error.code(), "missing_reader_capabilities");
  assert_eq!(error.capability_bits(), &[23]);

  let diagnostic = admit_v4_header(&selected, AdmissionModeV1::DiagnosticRaw, profile, None).unwrap();
  let V4AdmissionResult::DiagnosticRaw(diagnostic) = diagnostic else {
    panic!("diagnostic mode exposed semantic authority")
  };
  assert_eq!(diagnostic.issues.len(), 1);
  assert_eq!(diagnostic.issues[0].code(), "missing_reader_capabilities");
  assert!(!diagnostic.mutation_allowed());

  let clean = header("header-blake3-256-valid-ab.bin");
  let diagnostic = admit_v4_header(&clean, AdmissionModeV1::DiagnosticRaw, full_profile(), None).unwrap();
  let V4AdmissionResult::DiagnosticRaw(diagnostic) = diagnostic else {
    panic!("diagnostic mode exposed semantic authority")
  };
  assert!(diagnostic.issues.is_empty());
  assert!(!diagnostic.mutation_allowed());
}

#[test]
fn v4_baseline_capabilities_are_required_in_both_stored_floors() {
  let selected = header("header-blake3-256-valid-ab.bin");
  let mut missing_reader = selected.clone();
  missing_reader.header.required_reader_capabilities = CapabilitySetV1::from_bits([1, 2, 3, 4, 5, 6, 18, 19, 21, 22]).unwrap().into_bytes();
  assert_eq!(
    admit_v4_header(&missing_reader, AdmissionModeV1::SemanticReadOnly, full_profile(), None).unwrap_err().code(),
    "missing_baseline_reader_capabilities"
  );

  let mut missing_writer = selected.clone();
  missing_writer.header.required_writer_capabilities = CapabilitySetV1::from_bits([0, 1, 2, 3, 4, 5, 6, 18, 19, 21]).unwrap().into_bytes();
  assert_eq!(
    admit_v4_header(&missing_writer, AdmissionModeV1::SemanticReadOnly, full_profile(), None).unwrap_err().code(),
    "missing_baseline_writer_capabilities"
  );

  let diagnostic = admit_v4_header(&missing_writer, AdmissionModeV1::DiagnosticRaw, full_profile(), None).unwrap();
  let V4AdmissionResult::DiagnosticRaw(diagnostic) = diagnostic else {
    panic!("invalid baseline gained semantic authority")
  };
  assert_eq!(diagnostic.issues[0].code(), "missing_baseline_writer_capabilities");
  assert_eq!(diagnostic.issues[0].capability_bits(), &[22]);
}

#[test]
fn registry_version_and_fingerprint_drift_fail_before_semantic_or_write_authority() {
  let selected = header("header-blake3-256-valid-ab.bin");
  let mut bad_version = selected.clone();
  bad_version.header.system_family_registry_version = 2;
  assert_eq!(
    admit_v4_header(&bad_version, AdmissionModeV1::SemanticReadOnly, full_profile(), None).unwrap_err().code(),
    "unsupported_system_family_registry"
  );

  let mut bad_fingerprint = selected.clone();
  bad_fingerprint.header.system_family_registry_fingerprint[0] ^= 0xff;
  assert_eq!(
    admit_v4_header(&bad_fingerprint, AdmissionModeV1::Writable, full_profile(), Some(identity_evidence(&bad_fingerprint.header)))
      .unwrap_err()
      .code(),
    "system_family_registry_fingerprint_mismatch"
  );
  let diagnostic = admit_v4_header(&bad_fingerprint, AdmissionModeV1::DiagnosticRaw, full_profile(), None).unwrap();
  let V4AdmissionResult::DiagnosticRaw(diagnostic) = diagnostic else {
    panic!("registry drift gained semantic authority")
  };
  assert_eq!(diagnostic.issues[0].code(), "system_family_registry_fingerprint_mismatch");
}

#[test]
fn current_binary_advertises_only_proven_shadow_writers_and_refuses_v4_authority() {
  let profile = BinaryCapabilityProfileV1::current();
  assert_eq!(profile.supported_reader_capabilities.bits(), (0..24).collect::<Vec<_>>());
  assert_eq!(profile.supported_writer_capabilities.bits(), vec![0, 4]);
  for bit in 0..24 {
    assert_eq!(profile.supported_writer_capabilities.contains(bit), [0, 4].contains(&bit), "writer capability bit {bit}");
  }
  assert!(!profile.supported_writer_capabilities.contains(7), "immutable-only IndexArtifactV1 support is not the complete writer family");
  assert!(!profile.supported_writer_capabilities.contains(12), "immutable-only GcArtifactV1 support is not the complete writer family");

  let selected = header("header-blake3-256-valid-ab.bin");
  let error = admit_v4_header(&selected, AdmissionModeV1::Writable, profile, Some(identity_evidence(&selected.header))).unwrap_err();
  assert_eq!(error.code(), "missing_writer_capabilities");
  assert_eq!(error.capability_bits(), &[1, 2, 3, 5, 6, 18, 19, 21, 22]);

  let mut source = PeerCapabilityViewV1::from_selected(&selected, profile);
  source.physical_instance_id[0] ^= 1;
  let destination = PeerCapabilityViewV1::from_selected(&selected, profile);
  let error = admit_peer_capabilities_v4(&source, &destination).unwrap_err();
  assert_eq!(error.code(), "peer_destination_writer_capability_mismatch");

  let mut extra_writer = selected.clone();
  extra_writer.header.required_writer_capabilities = CapabilitySetV1::from_bytes(extra_writer.header.required_writer_capabilities)
    .unwrap()
    .union(CapabilitySetV1::from_bits([23]).unwrap())
    .into_bytes();
  let profile = BinaryCapabilityProfileV1::new(all_capabilities(), CapabilitySetV1::from_bits(0..23).unwrap());
  let error =
    admit_v4_header(&extra_writer, AdmissionModeV1::Writable, profile, Some(identity_evidence(&extra_writer.header))).unwrap_err();
  assert_eq!(error.code(), "missing_writer_capabilities");
  assert_eq!(error.capability_bits(), &[23]);
}

#[test]
fn current_capability_profile_has_no_production_admission_caller() {
  fn rust_sources(path: PathBuf) -> String {
    let mut source = String::new();
    for entry in fs::read_dir(path).unwrap() {
      let entry = entry.unwrap();
      if entry.file_type().unwrap().is_dir() {
        source.push_str(&rust_sources(entry.path()));
      } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
        source.push_str(&fs::read_to_string(entry.path()).unwrap());
      }
    }
    source
  }

  let production = rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
  assert_eq!(production.matches("admit_v4_header(").count(), 1);
  assert_eq!(production.matches("BinaryCapabilityProfileV1::current(").count(), 0);
}

#[test]
fn writable_admission_requires_a_hard_fenced_existing_or_adopted_identity() {
  let selected = header("header-blake3-256-valid-ab.bin");
  assert_eq!(
    admit_v4_header(&selected, AdmissionModeV1::Writable, full_profile(), None).unwrap_err().code(),
    "physical_identity_evidence_required"
  );

  let stale = PhysicalIdentityEvidenceV1::ExistingInstance {
    physical_instance_id: selected.header.physical_instance_id,
    previous_writer_fence_epoch: selected.header.writer_fence_epoch,
  };
  assert_eq!(
    admit_v4_header(&selected, AdmissionModeV1::Writable, full_profile(), Some(stale)).unwrap_err().code(),
    "writer_fence_not_advanced"
  );

  let wrong_existing = PhysicalIdentityEvidenceV1::ExistingInstance {
    physical_instance_id: [0x55; 16],
    previous_writer_fence_epoch: selected.header.writer_fence_epoch - 1,
  };
  assert_eq!(
    admit_v4_header(&selected, AdmissionModeV1::Writable, full_profile(), Some(wrong_existing)).unwrap_err().code(),
    "physical_instance_identity_mismatch"
  );

  let copied = PhysicalIdentityEvidenceV1::AdoptedCopy {
    source_physical_instance_id: selected.header.physical_instance_id,
    source_writer_fence_epoch: selected.header.writer_fence_epoch - 1,
  };
  assert_eq!(
    admit_v4_header(&selected, AdmissionModeV1::Writable, full_profile(), Some(copied)).unwrap_err().code(),
    "clone_physical_identity_not_adopted"
  );

  let copied_with_stale_fence = PhysicalIdentityEvidenceV1::AdoptedCopy {
    source_physical_instance_id: [0x55; 16],
    source_writer_fence_epoch: selected.header.writer_fence_epoch,
  };
  assert_eq!(
    admit_v4_header(&selected, AdmissionModeV1::Writable, full_profile(), Some(copied_with_stale_fence)).unwrap_err().code(),
    "writer_fence_not_advanced"
  );

  let adopted = header("header-blake3-256-adopted-physical-id.bin");
  let evidence = PhysicalIdentityEvidenceV1::AdoptedCopy {
    source_physical_instance_id: selected.header.physical_instance_id,
    source_writer_fence_epoch: selected.header.writer_fence_epoch,
  };
  let admitted = admit_v4_header(&adopted, AdmissionModeV1::Writable, full_profile(), Some(evidence)).unwrap();
  assert!(matches!(admitted, V4AdmissionResult::Writable(_)));
}

#[test]
fn writable_admission_rejects_degraded_header_redundancy() {
  let degraded = header("header-blake3-256-one-valid-slot.bin");
  assert!(degraded.redundancy_degraded);

  let error = admit_v4_header(&degraded, AdmissionModeV1::Writable, full_profile(), Some(identity_evidence(&degraded.header))).unwrap_err();
  assert_eq!(error.code(), "writable_header_redundancy_degraded");

  assert!(matches!(
    admit_v4_header(&degraded, AdmissionModeV1::SemanticReadOnly, BinaryCapabilityProfileV1::current(), None).unwrap(),
    V4AdmissionResult::SemanticReadOnly(_)
  ));
}

#[test]
fn system_family_classification_follows_frozen_specificity_and_unknown_rules() {
  let selected = header("header-blake3-256-valid-ab.bin");
  let admission = admit_v4_header(&selected, AdmissionModeV1::SemanticReadOnly, BinaryCapabilityProfileV1::current(), None).unwrap();
  let V4AdmissionResult::SemanticReadOnly(read) = admission else {
    unreachable!()
  };

  let cases = [
    (SystemFamilySubjectV1::Path("/.aeordb-config/indexes.json"), Some(0x0001)),
    (SystemFamilySubjectV1::Path("/docs/.aeordb-config/indexes.json"), Some(0x0002)),
    (SystemFamilySubjectV1::Path("/docs/.aeordb-config/custom.json"), Some(0x0008)),
    (SystemFamilySubjectV1::Path("/docs/.aeordb-permissions"), Some(0x0019)),
    (SystemFamilySubjectV1::Path("/docs/.aeordb-config/archive/.aeordb-indexes/postings/page.bin"), Some(0x0060)),
    (SystemFamilySubjectV1::Path("/.aeordb-system/controls/v1/index-registry/a.ctrl"), Some(0x0043)),
    (SystemFamilySubjectV1::EntryType(0x0005), Some(0x0040)),
    (SystemFamilySubjectV1::ControlTag(9), Some(0x0056)),
    (SystemFamilySubjectV1::ExternalWorkspaceKind(4), Some(0x0071)),
    (SystemFamilySubjectV1::KvKey(b"aeordb.task.v1\0task-1"), Some(0x0042)),
    (SystemFamilySubjectV1::Path("/docs/readme.md"), None),
  ];
  for (subject, expected) in cases {
    let actual = classify_system_family(&read.registry, subject).unwrap();
    assert_eq!(actual.family_id(), expected);
  }

  let unknown = classify_system_family(&read.registry, SystemFamilySubjectV1::Path("/docs/.aeordb-unknown/value")).unwrap();
  assert_eq!(unknown, SystemFamilyClassificationV1::UnknownProtected);
  assert_eq!(require_complete_system_family(unknown, "logical backup").unwrap_err().code(), "unknown_protected_system_family");
  assert_eq!(
    classify_system_family(&read.registry, SystemFamilySubjectV1::Path("/docs/")).unwrap_err().code(),
    "system_family_matcher_exact_shape"
  );
  assert_eq!(
    classify_system_family(&read.registry, SystemFamilySubjectV1::Path("/.aeordb-indexes/page.bin")).unwrap().family_id(),
    Some(0x0060)
  );
  assert_eq!(
    classify_system_family(&read.registry, SystemFamilySubjectV1::Path("/.aeordb-logs/index.log")).unwrap().family_id(),
    Some(0x0061)
  );
  assert_eq!(
    classify_system_family(&read.registry, SystemFamilySubjectV1::EntryType(0xffff)).unwrap(),
    SystemFamilyClassificationV1::Ordinary
  );
  for subject in [
    SystemFamilySubjectV1::KvKey(b"aeordb.future.v1\0state"),
    SystemFamilySubjectV1::ControlTag(0xffff),
    SystemFamilySubjectV1::ExternalWorkspaceKind(0xffff),
  ] {
    assert_eq!(classify_system_family(&read.registry, subject).unwrap(), SystemFamilyClassificationV1::UnknownProtected);
  }
  assert_eq!(
    classify_system_family(&read.registry, SystemFamilySubjectV1::KvKey(b"ordinary-path-key")).unwrap(),
    SystemFamilyClassificationV1::Ordinary
  );
  for malformed in ["relative", "/docs//file", "/docs/../file", "/docs/./file"] {
    assert!(classify_system_family(&read.registry, SystemFamilySubjectV1::Path(malformed)).is_err());
  }
}

fn capability_view(selected: &SelectedDatabaseHeaderV4, profile: BinaryCapabilityProfileV1) -> PeerCapabilityViewV1 {
  PeerCapabilityViewV1::from_selected(selected, profile)
}

#[test]
fn peer_admission_requires_matching_identity_registry_and_destination_capability_floors() {
  let source = header("header-blake3-256-valid-ab.bin");
  let mut destination = header("header-blake3-256-adopted-physical-id.bin");
  destination.header.database_id = source.header.database_id;
  destination.header.required_reader_capabilities = source.header.required_reader_capabilities;
  destination.header.required_writer_capabilities = source.header.required_writer_capabilities;
  let source_hello = capability_view(&source, full_profile());
  let destination_hello = capability_view(&destination, full_profile());
  assert_eq!(admit_peer_capabilities_v4(&source_hello, &destination_hello).unwrap(), PeerCapabilityAdmissionV1::Compatible);

  let mut cases: Vec<(PeerCapabilityViewV1, &str)> = Vec::new();
  let mut wrong_database = destination_hello.clone();
  wrong_database.database_id[0] ^= 1;
  cases.push((wrong_database, "peer_database_identity_mismatch"));
  let mut same_physical = destination_hello.clone();
  same_physical.physical_instance_id = source_hello.physical_instance_id;
  cases.push((same_physical, "peer_physical_identity_collision"));
  let mut wrong_hash = destination_hello.clone();
  wrong_hash.hash_algorithm = HashAlgorithm::Sha512;
  cases.push((wrong_hash, "peer_hash_algorithm_mismatch"));
  let mut wrong_registry = destination_hello.clone();
  wrong_registry.system_family_registry_fingerprint[0] ^= 1;
  cases.push((wrong_registry, "peer_system_family_registry_mismatch"));
  let mut unreadable_source = source_hello.clone();
  unreadable_source.supported_reader_capabilities = CapabilitySetV1::empty();
  assert_eq!(
    admit_peer_capabilities_v4(&unreadable_source, &destination_hello).unwrap_err().code(),
    "peer_source_reader_capability_mismatch"
  );
  let mut unwritable_destination = destination_hello.clone();
  unwritable_destination.supported_writer_capabilities = CapabilitySetV1::empty();
  cases.push((unwritable_destination, "peer_destination_writer_capability_mismatch"));
  let mut unreadable_destination = destination_hello.clone();
  unreadable_destination.supported_reader_capabilities = CapabilitySetV1::empty();
  cases.push((unreadable_destination, "peer_destination_reader_capability_mismatch"));
  let mut floor_missing = destination_hello.clone();
  floor_missing.required_writer_capabilities = CapabilitySetV1::empty();
  cases.push((floor_missing, "peer_capability_floor_invalid"));

  let mut source_extra = source_hello.clone();
  source_extra.required_reader_capabilities = source_extra.required_reader_capabilities.union(CapabilitySetV1::from_bits([23]).unwrap());
  assert_eq!(
    admit_peer_capabilities_v4(&source_extra, &destination_hello).unwrap_err().code(),
    "peer_destination_stored_reader_floor_mismatch"
  );
  let mut destination_reader_ready = destination_hello.clone();
  destination_reader_ready.required_reader_capabilities =
    destination_reader_ready.required_reader_capabilities.union(CapabilitySetV1::from_bits([23]).unwrap());
  assert_eq!(
    admit_peer_capabilities_v4(&source_extra, &destination_reader_ready).unwrap_err().code(),
    "peer_destination_stored_writer_floor_mismatch"
  );

  let mut matching_wrong_source = source_hello.clone();
  let mut matching_wrong_destination = destination_hello.clone();
  matching_wrong_source.system_family_registry_fingerprint[0] ^= 1;
  matching_wrong_destination.system_family_registry_fingerprint[0] ^= 1;
  assert_eq!(
    admit_peer_capabilities_v4(&matching_wrong_source, &matching_wrong_destination).unwrap_err().code(),
    "peer_system_family_registry_mismatch"
  );

  let mut invalid_identity = destination_hello.clone();
  invalid_identity.selected_header_sequence = 0;
  assert_eq!(admit_peer_capabilities_v4(&source_hello, &invalid_identity).unwrap_err().code(), "peer_identity_or_sequence_invalid");

  for (candidate, code) in cases {
    assert_eq!(admit_peer_capabilities_v4(&source_hello, &candidate).unwrap_err().code(), code);
  }
}

#[test]
fn capability_sets_reject_reserved_bits_and_report_names() {
  let error = CapabilitySetV1::from_bits([24]).unwrap_err();
  assert_eq!(error.code(), "unknown_capability_bit");
  let error = V4AdmissionError::missing_reader(CapabilitySetV1::from_bits([3, 23]).unwrap());
  assert_eq!(error.capability_bits(), &[3, 23]);
  assert_eq!(error.capability_names(), vec!["SystemFamilyRegistryV1", "DurableTaskPinV1"]);
}
