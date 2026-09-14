//! Independent reference acceptance of ratified, previously misread contracts.
use crate::{core::HashProfile, dependency, selector, value_store};

fn key_selector(length: usize) -> Vec<u8> {
  let mut bytes = vec![0; length];
  bytes[..2].copy_from_slice(&1u16.to_le_bytes());
  bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
  bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
  bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
  bytes[16..18].copy_from_slice(&1u16.to_le_bytes());
  bytes[32] = 1;
  bytes[36..40].copy_from_slice(&((length - 40) as u32).to_le_bytes());
  bytes[40..].fill(b'k');
  bytes
}

fn unsigned32(bytes: &[u8], offset: usize) -> usize {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn fixture(profile: HashProfile, suffix: &str) -> Vec<u8> {
  std::fs::read(format!(
    "{}/../../aeordb-lib/spec/fixtures/v4/value-store-definition-v1/avst-{}-{suffix}-valid.bin",
    env!("CARGO_MANIFEST_DIR"),
    profile.label(),
  ))
  .unwrap()
}

#[test]
fn reference_selector_uses_the_ratified_64_kib_cap() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for length in [4096, 4097, 65536, 65537] {
      let observed = selector::observe(profile, &key_selector(length)).0;
      assert_eq!(observed.starts_with("selector:"), length <= 65536, "{length}: {observed}");
    }
  }
}

#[test]
fn reference_value_store_accepts_a_complete_64_kib_selector_child() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let original = fixture(profile, "json-corrected");
    let fixed = 32 + profile.width();
    let start = fixed + 80 + unsigned32(&original, fixed);
    let end = start + unsigned32(&original, fixed + 4);
    let mut bytes = original[..start].to_vec();
    bytes.extend_from_slice(&key_selector(65536));
    bytes.extend_from_slice(&original[end..]);
    let total = bytes.len() as u32;
    bytes[8..12].copy_from_slice(&total.to_le_bytes());
    bytes[fixed + 4..fixed + 8].copy_from_slice(&65536u32.to_le_bytes());
    assert!(!value_store::observe(profile, &bytes).0.starts_with("error:"));
  }
}

#[test]
fn reference_always_missing_requires_none_and_no_content_input() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let old = fixture(profile, "always-missing-legacy");
    let fixed = 32 + profile.width();
    let parser_start = fixed + 80 + unsigned32(&old, fixed) + unsigned32(&old, fixed + 4);
    let metadata = fixture(profile, "metadata-hash-corrected");
    let none_start = fixed + 80 + unsigned32(&metadata, fixed) + unsigned32(&metadata, fixed + 4);
    // Metadata's canonical none/empty children are already independently frozen.
    let mut bytes = old[..parser_start].to_vec();
    bytes.extend_from_slice(&metadata[none_start..]);
    let total = bytes.len() as u32;
    bytes[8..12].copy_from_slice(&total.to_le_bytes());
    bytes[fixed + 8..fixed + 12].copy_from_slice(&48u32.to_le_bytes());
    bytes[fixed + 12..fixed + 16].copy_from_slice(&32u32.to_le_bytes());
    bytes[fixed + 56..fixed + 64].fill(0);
    assert_eq!(value_store::observe(profile, &bytes).0, "value-store:field=legacy_missing:selector=4:dependencies=0");
    assert!(value_store::observe(profile, &old).0.starts_with("error:"));
  }
}

#[test]
fn reference_corrected_dependency_records_reject_migration_identity_flags() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let original = std::fs::read(format!(
      "{}/../../aeordb-lib/spec/fixtures/v4/dependency-table-v1/adpt-{}-wasm-mapper-valid.bin",
      env!("CARGO_MANIFEST_DIR"),
      profile.label(),
    ))
    .unwrap();
    for (abi, executor, valid) in
      [(4u16, 2u16, false), (4, u16::MAX, false), (u16::MAX, 2, false), (2, 3, true), (u16::MAX, u16::MAX, true)]
    {
      let mut bytes = original.clone();
      bytes[40..44].copy_from_slice(&6u32.to_le_bytes());
      bytes[44..46].copy_from_slice(&abi.to_le_bytes());
      bytes[46..48].copy_from_slice(&executor.to_le_bytes());
      assert_eq!(!dependency::observe(profile, &bytes).0.starts_with("error:"), valid, "ABI={abi}, executor={executor}");
    }
  }
}

#[test]
fn reference_corrected_closure_rejects_unknown_executor_migration_flags() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let mut bytes = fixture(profile, "mapper-corrected");
    let fixed = 32 + profile.width();
    let table = fixed + 80 + unsigned32(&bytes, fixed) + unsigned32(&bytes, fixed + 4) + unsigned32(&bytes, fixed + 8);
    let record = table + 32;
    bytes[record + 8..record + 12].copy_from_slice(&6u32.to_le_bytes());
    bytes[record + 12..record + 14].copy_from_slice(&u16::MAX.to_le_bytes());
    bytes[record + 14..record + 16].copy_from_slice(&u16::MAX.to_le_bytes());
    assert!(!dependency::observe(profile, &bytes[table..]).0.starts_with("error:"));
    assert!(value_store::observe(profile, &bytes).0.starts_with("error:"));
  }
}
