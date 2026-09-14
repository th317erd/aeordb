//! Independent Round 10/16 catalog owner-key boundary checks.
//! Expected bytes are assembled here, never by an AeorDB production encoder.
use super::{CoreFormat, HashProfile, observe};

fn leaf(profile: HashProfile, class: u16, owner: &[u8]) -> Vec<u8> {
  let width = profile.width();
  let record_length = 8 + 2 * width + owner.len();
  let body_length = 16 + width + record_length;
  let mut bytes = vec![0; 36 + body_length];
  let total_length = bytes.len() as u32;
  bytes[..4].copy_from_slice(b"ASEM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&2u16.to_le_bytes());
  bytes[8..10].copy_from_slice(&32u16.to_le_bytes());
  bytes[12..16].copy_from_slice(&total_length.to_le_bytes());
  bytes[16..20].copy_from_slice(&(body_length as u32).to_le_bytes());
  bytes[20..28].copy_from_slice(&1u64.to_le_bytes());
  bytes[36..40].copy_from_slice(&1u32.to_le_bytes());
  let lookup = profile.digest(&[b"aeordb.semantic-catalog-key.v1\0".as_slice(), &class.to_le_bytes(), owner].concat());
  bytes[40..40 + width].copy_from_slice(&lookup);
  bytes[40 + width..44 + width].copy_from_slice(&(record_length as u32).to_le_bytes());
  let record = 48 + width;
  bytes[record..record + 2].copy_from_slice(&class.to_le_bytes());
  bytes[record + 4..record + 8].copy_from_slice(&(owner.len() as u32).to_le_bytes());
  bytes[record + 8..record + 8 + width].fill(0x43);
  if owner.len() == width && (3..=7).contains(&class) {
    bytes[record + 8..record + 8 + width].copy_from_slice(owner);
  }
  bytes[record + 8 + width..record + 8 + 2 * width].fill(0x71);
  bytes[record + 8 + 2 * width..record + record_length].copy_from_slice(owner);
  let checksum_offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..checksum_offset]);
  bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
  bytes
}

fn control_owner(kind: u16, path: &[u8]) -> Vec<u8> {
  [&kind.to_le_bytes(), path].concat()
}

fn accepted(profile: HashProfile, class: u16, owner: &[u8]) -> bool {
  observe(CoreFormat::SemanticObjectV1, profile, &leaf(profile, class, owner)).0 == "semantic:catalog-leaf:records=1"
}

#[test]
fn reference_catalog_accepts_all_seven_classes_at_both_database_widths() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in 1..=7 {
      let owner = if class <= 2 { control_owner(1, b"/.aeordb-config/parsers.json") } else { vec![0x43; profile.width()] };
      assert!(accepted(profile, class, &owner), "{profile:?}, class {class}");
    }
  }
}

#[test]
fn reference_catalog_control_owner_admits_root_unicode_and_internal_spaces() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [1, 2] {
      for kind in [1, 256, u16::MAX] {
        for path in ["/", "/資料/λ.json", "/folder name/config.json", "/literal\\backslash"] {
          assert!(accepted(profile, class, &control_owner(kind, path.as_bytes())), "{profile:?}, class {class}, {path:?}");
        }
      }
    }
  }
}

#[test]
fn reference_catalog_control_owner_enforces_byte_length_not_character_count() {
  let maximum_ascii = format!("/{}", "a".repeat(65_534));
  let maximum_unicode = format!("/{}", "λ".repeat(32_767));
  assert_eq!(maximum_ascii.len(), 65_535);
  assert_eq!(maximum_unicode.len(), 65_535);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [1, 2] {
      for path in [&maximum_ascii, &maximum_unicode] {
        assert!(accepted(profile, class, &control_owner(1, path.as_bytes())));
        let oversized = format!("{path}x");
        assert!(!accepted(profile, class, &control_owner(1, oversized.as_bytes())), "{profile:?}, class {class}");
      }
    }
  }
}

#[test]
fn reference_catalog_control_owner_rejects_missing_kind_or_path() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [1, 2] {
      for owner in [Vec::new(), vec![1], vec![1, 0], control_owner(0, b"/")] {
        assert!(!accepted(profile, class, &owner), "{profile:?}, class {class}, owner {owner:?}");
      }
    }
  }
}

#[test]
fn reference_catalog_control_owner_rejects_noncanonical_paths_without_normalizing_them() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [1, 2] {
      for path in ["relative", "//", "/folder/", "/a//b", "/./a", "/a/../b", " /a", "/a ", "/a\0b", "/a\n", "/a\u{2003}"] {
        assert!(!accepted(profile, class, &control_owner(1, path.as_bytes())), "{profile:?}, class {class}, path {path:?}");
      }
    }
  }
}

#[test]
fn reference_catalog_control_owner_rejects_invalid_utf8() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [1, 2] {
      for path in [&b"/\xff"[..], &b"/\xc0\xaf"[..], &b"/\xed\xa0\x80"[..], &b"/\xe2\x82"[..]] {
        assert!(!accepted(profile, class, &control_owner(1, path)), "{profile:?}, class {class}");
      }
    }
  }
}

#[test]
fn reference_catalog_scope_value_and_field_owner_keys_have_exact_selected_hash_width() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in 3..=5 {
      for length in [0, 1, profile.width() - 1, profile.width() + 1, 65, 65_537, 65_538] {
        assert!(!accepted(profile, class, &vec![0x43; length]), "{profile:?}, class {class}, length {length}");
      }
    }
  }
}

#[test]
fn reference_catalog_dependency_width_and_identity_guards_remain_strict() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [6, 7] {
      for length in [0, 1, profile.width() - 1, profile.width() + 1, 65_538] {
        assert!(!accepted(profile, class, &vec![0x43; length]));
      }
      let mut bytes = leaf(profile, class, &vec![0x43; profile.width()]);
      bytes[56 + profile.width()] ^= 1;
      let checksum_offset = bytes.len() - 4;
      let checksum = crc32fast::hash(&bytes[..checksum_offset]);
      bytes[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
      assert_eq!(observe(CoreFormat::SemanticObjectV1, profile, &bytes).0, "error:catalog_leaf_dependency_identity");
    }
  }
}

#[test]
fn reference_catalog_unregistered_classes_remain_rejected() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [0, 8, u16::MAX] {
      assert!(!accepted(profile, class, &vec![0x43; profile.width()]));
    }
  }
}
