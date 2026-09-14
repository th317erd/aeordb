//! Round 9 retention checks against the independent reference implementation.
use crate::{core::HashProfile, dependency, value_store};

fn fixture(family: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/../../aeordb-lib/spec/fixtures/v4/{family}/{name}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn u32_at(bytes: &[u8], offset: usize) -> usize {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

#[test]
fn independent_dependency_reader_retains_unknown_executors_but_not_invalid_kinds() {
  for (profile, name) in [(HashProfile::Blake3_256, "blake3-256"), (HashProfile::Sha512, "sha512")] {
    for suffix in ["native-parser-resolution", "wasm-mapper"] {
      let original = fixture("dependency-table-v1", &format!("adpt-{name}-{suffix}-valid"));
      for offset in [44, 46] {
        let mut bytes = original.clone();
        bytes[offset..offset + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(dependency::observe(profile, &bytes).0, "dependencies:records=1");
      }
      let mut invalid = original;
      invalid[36..38].copy_from_slice(&u16::MAX.to_le_bytes());
      assert!(dependency::observe(profile, &invalid).0.starts_with("error:"));
    }
  }
}

#[test]
fn independent_value_store_reader_retains_known_kind_unknown_profile_closures() {
  for (profile, name) in [(HashProfile::Blake3_256, "blake3-256"), (HashProfile::Sha512, "sha512")] {
    for suffix in ["json-corrected", "json-legacy", "mapper-corrected", "mapper-legacy"] {
      let original = fixture("value-store-definition-v1", &format!("avst-{name}-{suffix}-valid"));
      let fixed = 32 + profile.width();
      let table = fixed + 80 + u32_at(&original, fixed) + u32_at(&original, fixed + 4) + u32_at(&original, fixed + 8);
      assert_eq!(&original[table..table + 4], b"ADPT");
      let mut cursor = table + 32;
      for _ in 0..u32_at(&original, table + 16) {
        for offset in [12, 14] {
          let mut bytes = original.clone();
          bytes[cursor + offset..cursor + offset + 2].copy_from_slice(&u16::MAX.to_le_bytes());
          let observed = value_store::observe(profile, &bytes).0;
          assert!(!observed.starts_with("error:"), "{name}/{suffix}/{cursor}/{offset}: {observed}");
        }
        cursor += u32_at(&original, cursor);
      }
      assert_eq!(cursor, original.len());
    }
  }
}
