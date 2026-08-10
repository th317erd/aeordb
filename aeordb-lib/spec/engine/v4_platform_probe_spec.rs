use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use aeordb::engine::EngineError;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::durability::{rename_durable, sync_parent_dir};
use aeordb::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityErrorClass, NativeDurabilityMechanism, NativeDurabilityOperation, NativeOperationSupport,
  PlatformFileIdentityDescriptorV1, durable_replace_native, platform_file_identity, preallocate_file, probe_native_durability,
  sync_directory_native, sync_file_all_native, sync_file_data_native,
};
use aeordb::engine::v4::control_store::{ControlStoreReadV1, ControlStoreSlotsV1, select_control_store_read};
use aeordb::engine::v4::config_value::{
  CanonicalConfigValueV1, CanonicalValueBounds, canonical_value_to_json, canonicalize_json, decode_canonical_value, encode_canonical_value,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::system_control::{
  ConfigDiagnosticsBodyV1, ConfigLKGBodyV1, ConfigurationKindV1, SystemControlKindV1, SystemControlSlotV1, decode_config_diagnostics_body,
  decode_config_lkg_body, decode_durability_latch_body, decode_emergency_spill_catalog_body, decode_system_control,
  encode_config_diagnostics_control, encode_config_lkg_control, encode_durability_latch_control, encode_emergency_spill_catalog_control,
  system_control_path,
};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/system-control-v1")
}

fn fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
}

fn canonical_fixture(name: &str) -> Vec<u8> {
  fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/canonical-config-value-v1").join(name)).unwrap()
}

fn repair_crc(bytes: &mut [u8]) {
  let offset = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..offset]);
  bytes[offset..].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn native_probe_proves_barriers_preallocation_replace_identity_and_readback() {
  let temp = tempfile::tempdir().unwrap();
  let report = probe_native_durability(temp.path()).unwrap();

  assert!(!report.filesystem.kind.is_empty());
  for support in [
    &report.capabilities.data_barrier,
    &report.capabilities.file_barrier,
    &report.capabilities.preallocation,
    &report.capabilities.stable_file_identity,
  ] {
    assert_eq!(support, &NativeOperationSupport::Supported);
  }
  assert_eq!(report.identity_before_rename, report.identity_after_rename);
  assert_eq!(report.identity_before_rename.unwrap().to_bytes().len(), PlatformFileIdentityDescriptorV1::ENCODED_LENGTH);

  #[cfg(unix)]
  {
    assert!(report.read_back_verified);
    assert_eq!(report.capabilities.parent_directory_sync, NativeOperationSupport::Supported);
    assert_eq!(report.capabilities.durable_replace, NativeOperationSupport::Supported);
    assert_eq!(report.replaced_identity, report.identity_after_rename);
    assert_ne!(report.destination_identity_before_replace, report.replaced_identity);
  }

  #[cfg(windows)]
  match &report.capabilities.parent_directory_sync {
    NativeOperationSupport::Supported => {
      assert_eq!(report.capabilities.durable_replace, NativeOperationSupport::Supported);
      assert!(report.read_back_verified);
      assert_eq!(report.replaced_identity, report.identity_after_rename);
      assert_ne!(report.destination_identity_before_replace, report.replaced_identity);
    }
    NativeOperationSupport::Unsupported { reason } => {
      assert!(!reason.is_empty());
      assert!(matches!(report.capabilities.durable_replace, NativeOperationSupport::Unsupported { .. }));
      assert!(!report.read_back_verified);
      assert!(report.replaced_identity.is_none());
    }
  }

  #[cfg(target_os = "linux")]
  assert_eq!(
    report.mechanisms,
    aeordb::engine::native_durability::NativeDurabilityMechanisms {
      data_barrier: Some(NativeDurabilityMechanism::UnixFdatasync),
      file_barrier: Some(NativeDurabilityMechanism::UnixFsync),
      parent_directory_sync: Some(NativeDurabilityMechanism::UnixFsync),
      durable_replace: Some(NativeDurabilityMechanism::UnixRenameAndDirectoryFsync),
    }
  );

  #[cfg(target_os = "macos")]
  {
    assert_eq!(report.mechanisms.data_barrier, Some(NativeDurabilityMechanism::AppleBarrierFsync));
    assert!(matches!(
      report.mechanisms.file_barrier,
      Some(NativeDurabilityMechanism::AppleFullFsync | NativeDurabilityMechanism::AppleFsyncFallback)
    ));
    assert_eq!(report.mechanisms.parent_directory_sync, Some(NativeDurabilityMechanism::UnixFsync));
    assert_eq!(report.mechanisms.durable_replace, Some(NativeDurabilityMechanism::UnixRenameAndDirectoryFsync));
  }

  #[cfg(windows)]
  {
    assert_eq!(report.mechanisms.data_barrier, Some(NativeDurabilityMechanism::WindowsFlushFileBuffers));
    assert_eq!(report.mechanisms.file_barrier, Some(NativeDurabilityMechanism::WindowsFlushFileBuffers));
    assert!(matches!(report.mechanisms.parent_directory_sync, Some(NativeDurabilityMechanism::WindowsDirectoryFlushFileBuffers) | None));
    if report.capabilities.durable_replace == NativeOperationSupport::Supported {
      assert!(matches!(
        report.mechanisms.durable_replace,
        Some(NativeDurabilityMechanism::WindowsReplaceFileAndFlush | NativeDurabilityMechanism::WindowsMoveFileExWriteThrough)
      ));
    }
  }
}

#[test]
fn windows_replace_never_passes_the_documented_unsupported_replacefile_flag() {
  let source = include_str!("../../src/engine/native_durability.rs");
  assert!(!source.contains("REPLACEFILE_WRITE_THROUGH"));
}

#[test]
fn native_unsupported_os_results_remain_typed_and_never_become_warning_success() {
  #[cfg(unix)]
  let raw_error = libc::ENOSYS;
  #[cfg(windows)]
  let raw_error = 50;

  let error = NativeDurabilityError::from_io(NativeDurabilityOperation::ParentDirectorySync, io::Error::from_raw_os_error(raw_error));
  assert_eq!(error.class(), NativeDurabilityErrorClass::Unsupported);
  assert_eq!(error.operation(), NativeDurabilityOperation::ParentDirectorySync);
  assert_eq!(error.raw_os_error(), Some(raw_error));
}

#[test]
fn native_primitives_preserve_identity_across_rename_and_replace_contents_durably() {
  let temp = tempfile::tempdir().unwrap();
  let original = temp.path().join("original.bin");
  let renamed = temp.path().join("renamed.bin");
  let replacement = temp.path().join("replacement.bin");

  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&original).unwrap();
  preallocate_file(&file, 64 * 1024).unwrap();
  file.write_all(b"old payload").unwrap();
  sync_file_data_native(&file).unwrap();
  sync_file_all_native(&file).unwrap();
  #[cfg(unix)]
  sync_directory_native(temp.path()).unwrap();
  #[cfg(windows)]
  if let Err(error) = sync_directory_native(temp.path()) {
    assert_eq!(error.class(), NativeDurabilityErrorClass::Unsupported);
    assert_eq!(fs::read(&original).unwrap(), b"old payload");
    return;
  }
  let before = platform_file_identity(&original).unwrap();

  fs::rename(&original, &renamed).unwrap();
  #[cfg(unix)]
  sync_directory_native(temp.path()).unwrap();
  #[cfg(windows)]
  sync_directory_native(temp.path()).unwrap();
  assert_eq!(before, platform_file_identity(&renamed).unwrap());

  let mut file = File::create(&replacement).unwrap();
  file.write_all(b"new payload").unwrap();
  sync_file_all_native(&file).unwrap();
  drop(file);
  #[cfg(unix)]
  durable_replace_native(&replacement, &renamed).unwrap();
  #[cfg(windows)]
  if let Err(error) = durable_replace_native(&replacement, &renamed) {
    assert_eq!(error.class(), NativeDurabilityErrorClass::Unsupported);
    assert_eq!(fs::read(&renamed).unwrap(), b"old payload");
    assert_eq!(fs::read(&replacement).unwrap(), b"new payload");
    return;
  }
  assert_eq!(fs::read(&renamed).unwrap(), b"new payload");
  assert_ne!(before, platform_file_identity(&renamed).unwrap());
  assert!(!replacement.exists());
}

#[test]
fn legacy_durability_helpers_delegate_to_the_proven_native_path() {
  let temp = tempfile::tempdir().unwrap();
  let source = temp.path().join("legacy-source.bin");
  let destination = temp.path().join("legacy-destination.bin");
  fs::write(&source, b"replacement").unwrap();
  fs::write(&destination, b"old").unwrap();
  #[cfg(unix)]
  sync_parent_dir(&source).unwrap();
  #[cfg(windows)]
  if let Err(error) = sync_parent_dir(&source) {
    let EngineError::DurabilityFailure(message) = error else {
      panic!("unexpected parent-sync error: {error}")
    };
    assert!(message.contains("Unsupported"));
    assert_eq!(fs::read(&source).unwrap(), b"replacement");
    assert_eq!(fs::read(&destination).unwrap(), b"old");
    return;
  }
  rename_durable(&source, &destination).unwrap();
  assert_eq!(fs::read(&destination).unwrap(), b"replacement");
  assert!(!source.exists());
}

#[test]
fn platform_file_identity_descriptor_has_the_frozen_exact_layout() {
  let descriptor = PlatformFileIdentityDescriptorV1 {
    platform: 2,
    schema: 1,
    flags: 3,
    volume_identity: [0x11; 16],
    file_identity: [0x22; 16],
    birth_identity: [0x33; 16],
  };
  let bytes = descriptor.to_bytes();
  assert_eq!(&bytes[..8], &[2, 0, 1, 0, 3, 0, 0, 0]);
  assert_eq!(&bytes[8..24], &[0x11; 16]);
  assert_eq!(&bytes[24..40], &[0x22; 16]);
  assert_eq!(&bytes[40..56], &[0x33; 16]);
}

#[test]
fn native_failures_are_typed_and_never_reported_as_success() {
  let temp = tempfile::tempdir().unwrap();
  let missing = temp.path().join("missing");
  let error = sync_directory_native(&missing).unwrap_err();
  assert_eq!(error.class(), NativeDurabilityErrorClass::Io);
  assert!(!error.is_unsupported());

  let source = temp.path().join("missing-source");
  let destination = temp.path().join("destination");
  assert_eq!(durable_replace_native(&source, &destination).unwrap_err().class(), NativeDurabilityErrorClass::Io);
  assert_eq!(platform_file_identity(&missing).unwrap_err().class(), NativeDurabilityErrorClass::Io);
  let file = File::create(temp.path().join("zero-length.bin")).unwrap();
  assert_eq!(preallocate_file(&file, 0).unwrap_err().class(), NativeDurabilityErrorClass::InvalidInput);
  assert_eq!(durable_replace_native(&destination, &destination).unwrap_err().class(), NativeDurabilityErrorClass::InvalidInput);
}

#[test]
fn native_probes_are_isolated_when_run_concurrently_in_one_directory() {
  let temp = tempfile::tempdir().unwrap();
  std::thread::scope(|scope| {
    let handles: Vec<_> = (0..8).map(|_| scope.spawn(|| probe_native_durability(temp.path()))).collect();
    for handle in handles {
      let report = handle.join().unwrap().unwrap();
      assert!(report.read_back_verified);
    }
  });
}

#[test]
fn control_store_selects_absent_mutable_torn_and_ambiguous_states() {
  let mut a = fixture("control-blake3-256-index-degraded-valid.bin");
  let decoded = decode_system_control(&a, HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = decoded.database_id.try_into().unwrap();
  let identity = decoded.identity.clone();

  let absent = select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::IndexDegraded,
    database_id,
    &identity,
    ControlStoreSlotsV1::default(),
  )
  .unwrap();
  assert!(matches!(absent, ControlStoreReadV1::Absent));

  let only_a = select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::IndexDegraded,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: Some(&a), b: None, immutable: None },
  )
  .unwrap();
  let ControlStoreReadV1::Mutable(only_a) = only_a else {
    panic!("mutable slot did not select mutable control")
  };
  assert_eq!(only_a.selected_slot, SystemControlSlotV1::A);
  assert!(only_a.redundancy_degraded);

  let mut b = a.clone();
  b[16..24].copy_from_slice(&8u64.to_le_bytes());
  repair_crc(&mut b);
  let selected = select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::IndexDegraded,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: Some(&a), b: Some(&b), immutable: None },
  )
  .unwrap();
  let ControlStoreReadV1::Mutable(selected) = selected else {
    panic!("mutable pair did not select mutable control")
  };
  assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
  assert_eq!(selected.control.sequence, 8);

  b[40] ^= 1;
  let selected = select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::IndexDegraded,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: Some(&a), b: Some(&b), immutable: None },
  )
  .unwrap();
  let ControlStoreReadV1::Mutable(selected) = selected else {
    panic!("torn pair did not select mutable control")
  };
  assert_eq!(selected.selected_slot, SystemControlSlotV1::A);
  assert!(selected.redundancy_degraded);

  let changed_at = 32 + 24 + 32;
  let changed = i64::from_le_bytes(a[changed_at..changed_at + 8].try_into().unwrap()) + 1;
  a[changed_at..changed_at + 8].copy_from_slice(&changed.to_le_bytes());
  repair_crc(&mut a);
  b = fixture("control-blake3-256-index-degraded-valid.bin");
  assert_eq!(
    select_control_store_read(
      HashAlgorithm::Blake3_256,
      SystemControlKindV1::IndexDegraded,
      database_id,
      &identity,
      ControlStoreSlotsV1 { a: Some(&a), b: Some(&b), immutable: None },
    )
    .unwrap_err()
    .class(),
    MalformedInputClass::AmbiguousEqualSequenceSelector
  );
}

#[test]
fn control_store_verifies_expected_identity_kind_database_and_slot_family() {
  let mutable = fixture("control-blake3-256-index-degraded-valid.bin");
  let decoded = decode_system_control(&mutable, HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = decoded.database_id.try_into().unwrap();
  let identity = decoded.identity.clone();

  let mut wrong_database = database_id;
  wrong_database[0] ^= 1;
  assert_eq!(
    select_control_store_read(
      HashAlgorithm::Blake3_256,
      SystemControlKindV1::IndexDegraded,
      wrong_database,
      &identity,
      ControlStoreSlotsV1 { a: Some(&mutable), b: None, immutable: None },
    )
    .unwrap_err()
    .class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert!(select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::DurabilityLatch,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: Some(&mutable), b: None, immutable: None },
  )
  .is_err());
  assert!(select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::IndexDegraded,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: None, b: None, immutable: Some(&mutable) },
  )
  .is_err());
  assert_eq!(system_control_path(SystemControlKindV1::IndexDegraded, &identity, SystemControlSlotV1::A).unwrap(), decoded.canonical_path());
  assert!(system_control_path(SystemControlKindV1::IndexDegraded, &identity, SystemControlSlotV1::Immutable).is_err());
  assert_eq!(
    select_control_store_read(
      HashAlgorithm::Blake3_256,
      SystemControlKindV1::IndexDegraded,
      database_id,
      &[0; 4_097],
      ControlStoreSlotsV1::default(),
    )
    .unwrap_err()
    .class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn control_store_selects_and_verifies_immutable_i_slots() {
  let immutable = fixture("control-blake3-256-root-admission-commit-valid.bin");
  let decoded = decode_system_control(&immutable, HashAlgorithm::Blake3_256).unwrap();
  let database_id: [u8; 16] = decoded.database_id.try_into().unwrap();
  let identity = decoded.identity.clone();

  let selected = select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::RootAdmissionCommit,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: None, b: None, immutable: Some(&immutable) },
  )
  .unwrap();
  let ControlStoreReadV1::Immutable(selected) = selected else {
    panic!("immutable I slot did not select immutable control")
  };
  assert_eq!(selected.sequence, 1);
  assert!(selected.canonical_path().ends_with("/i.ctrl"));

  assert!(select_control_store_read(
    HashAlgorithm::Blake3_256,
    SystemControlKindV1::RootAdmissionCommit,
    database_id,
    &identity,
    ControlStoreSlotsV1 { a: Some(&immutable), b: None, immutable: None },
  )
  .is_err());
}

#[test]
fn production_durability_latch_encoder_matches_independent_fixtures() {
  for (algorithm, fixture_name) in [
    (HashAlgorithm::Blake3_256, "control-blake3-256-durability-latch-valid.bin"),
    (HashAlgorithm::Sha512, "control-sha512-durability-latch-valid.bin"),
  ] {
    let expected = fixture(fixture_name);
    let control = decode_system_control(&expected, algorithm).unwrap();
    let latch = decode_durability_latch_body(control.body, algorithm).unwrap();

    assert_eq!(latch.latch_generation, 2);
    assert_eq!(latch.first_failure_at_ms, 1_700_000_015_000);
    assert_eq!(latch.latest_failure_at_ms, 1_700_000_016_000);
    assert_eq!(latch.os_error_code, 28);
    assert_eq!(encode_durability_latch_control(control.sequence, &latch, algorithm).unwrap(), expected);
  }
}

#[test]
fn production_spill_catalog_encoder_matches_independent_fixtures() {
  for (algorithm, fixture_name) in [
    (HashAlgorithm::Blake3_256, "control-blake3-256-emergency-spill-catalog-valid.bin"),
    (HashAlgorithm::Sha512, "control-sha512-emergency-spill-catalog-valid.bin"),
  ] {
    let expected = fixture(fixture_name);
    let control = decode_system_control(&expected, algorithm).unwrap();
    let catalog = decode_emergency_spill_catalog_body(control.body, algorithm).unwrap();

    assert_eq!(catalog.catalog_generation, 1);
    assert_eq!(catalog.rows.len(), 1);
    assert_eq!(catalog.rows[0].file_length, 4_096);
    assert_eq!(catalog.rows[0].native_path, b"/var/lib/aeordb/spill/hot-tail-0001.bin");
    assert_eq!(encode_emergency_spill_catalog_control(control.sequence, &catalog, algorithm).unwrap(), expected);
  }
}

#[test]
fn production_configuration_control_encoders_match_independent_fixtures() {
  for (algorithm, fixture_prefix) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for (configuration_kind, family) in [(ConfigurationKindV1::Lifecycle, "lifecycle"), (ConfigurationKindV1::Runtime, "runtime")] {
      let lkg_expected = fixture(&format!("control-{fixture_prefix}-{family}-lkg-valid.bin"));
      let lkg_control = decode_system_control(&lkg_expected, algorithm).unwrap();
      let lkg = decode_config_lkg_body(lkg_control.body, algorithm).unwrap();
      assert_eq!(lkg.configuration_kind, configuration_kind);
      assert_eq!(lkg.configuration_schema, 1);
      assert_eq!(lkg.activated_at_ms, 1_700_000_003_000);
      assert_eq!(encode_config_lkg_control(lkg_control.sequence, &lkg, algorithm).unwrap(), lkg_expected);

      let diagnostics_expected = fixture(&format!("control-{fixture_prefix}-{family}-diagnostics-valid.bin"));
      let diagnostics_control = decode_system_control(&diagnostics_expected, algorithm).unwrap();
      let diagnostics = decode_config_diagnostics_body(diagnostics_control.body, algorithm).unwrap();
      assert_eq!(diagnostics.configuration_kind, configuration_kind);
      assert_eq!(diagnostics.aggregate_state, 1);
      assert_eq!(diagnostics.observed_at_ms, 1_700_000_004_000);
      assert_eq!(encode_config_diagnostics_control(diagnostics_control.sequence, &diagnostics, algorithm).unwrap(), diagnostics_expected);
    }
  }
}

#[test]
fn production_configuration_control_encoders_reject_invalid_closure() {
  let lkg_bytes = fixture("control-blake3-256-runtime-lkg-valid.bin");
  let lkg_control = decode_system_control(&lkg_bytes, HashAlgorithm::Blake3_256).unwrap();
  let lkg = decode_config_lkg_body(lkg_control.body, HashAlgorithm::Blake3_256).unwrap();

  let mut invalid = ConfigLKGBodyV1 { activated_at_ms: -1, ..lkg.clone() };
  assert_eq!(
    encode_config_lkg_control(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  invalid = ConfigLKGBodyV1 { configuration_schema: 0, ..lkg.clone() };
  assert_eq!(
    encode_config_lkg_control(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );
  invalid = ConfigLKGBodyV1 { source_namespace_root: vec![0; 31], ..lkg.clone() };
  assert_eq!(
    encode_config_lkg_control(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
  invalid = ConfigLKGBodyV1 { canonical_config: vec![0xff], ..lkg };
  assert_eq!(
    encode_config_lkg_control(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );

  let diagnostics_bytes = fixture("control-blake3-256-lifecycle-diagnostics-valid.bin");
  let diagnostics_control = decode_system_control(&diagnostics_bytes, HashAlgorithm::Blake3_256).unwrap();
  let diagnostics = decode_config_diagnostics_body(diagnostics_control.body, HashAlgorithm::Blake3_256).unwrap();
  let invalid = ConfigDiagnosticsBodyV1 { aggregate_state: 0, ..diagnostics.clone() };
  assert_eq!(
    encode_config_diagnostics_control(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::UnknownTypeKindOrEnum
  );
  let invalid = ConfigDiagnosticsBodyV1 { detail: vec![0xff], ..diagnostics };
  assert_eq!(
    encode_config_diagnostics_control(1, &invalid, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::TruncationOrTrailingBytes
  );
}

#[test]
fn production_canonical_value_codec_matches_every_independent_fixture() {
  for hash_name in ["blake3-256", "sha512"] {
    for case in ["all-tags", "maximum-string", "numeric-boundaries"] {
      let expected = canonical_fixture(&format!("config-{hash_name}-{case}-valid.bin"));
      let decoded = decode_canonical_value(&expected, CanonicalValueBounds::CONFIG).unwrap();
      assert_eq!(encode_canonical_value(&decoded, CanonicalValueBounds::CONFIG).unwrap(), expected);
    }
  }
}

#[test]
fn strict_json_canonicalization_matches_lkg_payloads_and_rejects_noncanonical_input() {
  for (fixture_name, json) in [
    ("control-blake3-256-lifecycle-lkg-valid.bin", br#"{"garbage_collection":{"enabled":false}}"#.as_slice()),
    ("control-blake3-256-runtime-lkg-valid.bin", br#"{"memory":{"maximum_bytes":8589934592}}"#.as_slice()),
  ] {
    let expected = fixture(fixture_name);
    let control = decode_system_control(&expected, HashAlgorithm::Blake3_256).unwrap();
    let lkg = decode_config_lkg_body(control.body, HashAlgorithm::Blake3_256).unwrap();
    assert_eq!(canonicalize_json(json, CanonicalValueBounds::AUDIT_VALUE).unwrap(), lkg.canonical_config);
  }

  assert!(canonicalize_json(br#"{"a":1,"a":2}"#, CanonicalValueBounds::CONFIG).is_err());
  assert!(canonicalize_json(br#"{"a":-0.0}"#, CanonicalValueBounds::CONFIG).is_err());
  assert!(canonicalize_json(br#"{"a":1} trailing"#, CanonicalValueBounds::CONFIG).is_err());
  assert_eq!(
    canonicalize_json(&vec![b' '; CanonicalValueBounds::CONFIG.maximum_value_length + 1], CanonicalValueBounds::CONFIG)
      .unwrap_err()
      .class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn canonical_config_recovers_bounded_strict_json_without_changing_policy_identity() {
  let json = br#"{"lifecycle":{"enabled":true,"limits":[0,9223372036854775808]},"name":"line\nvalue"}"#;
  let canonical = canonicalize_json(json, CanonicalValueBounds::AUDIT_VALUE).unwrap();

  let recovered = canonical_value_to_json(&canonical, CanonicalValueBounds::AUDIT_VALUE, 1_048_576).unwrap();

  assert_eq!(canonicalize_json(&recovered, CanonicalValueBounds::AUDIT_VALUE).unwrap(), canonical);
}

#[test]
fn canonical_config_json_recovery_rejects_non_json_bytes_and_output_amplification() {
  let bytes = encode_canonical_value(&CanonicalConfigValueV1::Bytes(vec![1, 2, 3]), CanonicalValueBounds::AUDIT_VALUE).unwrap();
  assert!(canonical_value_to_json(&bytes, CanonicalValueBounds::AUDIT_VALUE, 1_048_576).is_err());

  let escaped = encode_canonical_value(
    &CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::String("\0".repeat(64)); 16]),
    CanonicalValueBounds::AUDIT_VALUE,
  )
  .unwrap();
  let error = canonical_value_to_json(&escaped, CanonicalValueBounds::AUDIT_VALUE, 128).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn production_control_encoders_reject_invalid_sequences_widths_and_state_closure() {
  let latch_bytes = fixture("control-blake3-256-durability-latch-valid.bin");
  let latch_control = decode_system_control(&latch_bytes, HashAlgorithm::Blake3_256).unwrap();
  let mut latch = decode_durability_latch_body(latch_control.body, HashAlgorithm::Blake3_256).unwrap();

  assert_eq!(
    encode_durability_latch_control(0, &latch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  latch.latest_failure_at_ms = latch.first_failure_at_ms - 1;
  assert_eq!(
    encode_durability_latch_control(1, &latch, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let spill_bytes = fixture("control-blake3-256-emergency-spill-catalog-valid.bin");
  let spill_control = decode_system_control(&spill_bytes, HashAlgorithm::Blake3_256).unwrap();
  let mut catalog = decode_emergency_spill_catalog_body(spill_control.body, HashAlgorithm::Blake3_256).unwrap();
  catalog.rows[0].complete_file_digest.pop();
  assert_eq!(
    encode_emergency_spill_catalog_control(1, &catalog, HashAlgorithm::Blake3_256).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
}
