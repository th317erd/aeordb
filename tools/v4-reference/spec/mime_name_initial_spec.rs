//! Independent-reader proof of Round 9 / RFC 6838 section 4.2 initials.
use crate::{core::HashProfile, parser};

#[test]
fn corrected_mime_requires_an_alphanumeric_initial_but_legacy_matches_remain_exact() {
  for (profile, name) in [(HashProfile::Blake3_256, "blake3-256"), (HashProfile::Sha512, "sha512")] {
    for legacy in [false, true] {
      let suffix = if legacy { "automatic-legacy" } else { "automatic" };
      let original = std::fs::read(format!(
        "{}/../../aeordb-lib/spec/fixtures/v4/parser-resolution-plan-v1/aprp-{name}-{suffix}-valid.bin",
        env!("CARGO_MANIFEST_DIR")
      ))
      .unwrap();
      let length = u32::from_le_bytes(original[64..68].try_into().unwrap()) as usize;
      let slash = original[80..80 + length].iter().position(|byte| *byte == b'/').unwrap();
      for initial in b"!#$&^_.+-" {
        for offset in [80, 80 + slash + 1] {
          let mut bytes = original.clone();
          bytes[offset] = *initial;
          let observed = parser::observe(profile, &bytes).0;
          assert_eq!(!observed.starts_with("error:"), legacy, "{name}/{legacy}/{initial}/{offset}: {observed}");
        }
      }
      for initial in b"ab09" {
        for offset in [80, 80 + slash + 1] {
          let mut bytes = original.clone();
          bytes[offset] = *initial;
          assert!(!parser::observe(profile, &bytes).0.starts_with("error:"), "valid initial: {name}/{legacy}/{initial}/{offset}");
        }
      }
    }
  }
}
