use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::durability::{rename_durable, sync_parent_dir};
use aeordb::engine::native_durability::{
  NativeDurabilityErrorClass, NativeOperationSupport, PlatformFileIdentityDescriptorV1, durable_replace_native, platform_file_identity,
  preallocate_file, probe_native_durability, sync_directory_native, sync_file_all_native, sync_file_data_native,
};
use aeordb::engine::v4::control_store::{ControlStoreReadV1, ControlStoreSlotsV1, select_control_store_read};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1, decode_system_control, system_control_path};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/system-control-v1")
}

fn fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
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
  assert!(report.read_back_verified);
  for support in [
    &report.capabilities.data_barrier,
    &report.capabilities.file_barrier,
    &report.capabilities.parent_directory_sync,
    &report.capabilities.durable_replace,
    &report.capabilities.preallocation,
    &report.capabilities.stable_file_identity,
  ] {
    assert_eq!(support, &NativeOperationSupport::Supported);
  }
  assert_eq!(report.identity_before_rename, report.identity_after_rename);
  assert_eq!(report.replaced_identity, report.identity_after_rename);
  assert_ne!(report.destination_identity_before_replace, report.replaced_identity);
  assert_eq!(report.identity_before_rename.unwrap().to_bytes().len(), PlatformFileIdentityDescriptorV1::ENCODED_LENGTH);
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
  sync_directory_native(temp.path()).unwrap();
  let before = platform_file_identity(&original).unwrap();

  fs::rename(&original, &renamed).unwrap();
  sync_directory_native(temp.path()).unwrap();
  assert_eq!(before, platform_file_identity(&renamed).unwrap());

  let mut file = File::create(&replacement).unwrap();
  file.write_all(b"new payload").unwrap();
  sync_file_all_native(&file).unwrap();
  drop(file);
  durable_replace_native(&replacement, &renamed).unwrap();
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
  sync_parent_dir(&source).unwrap();
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
