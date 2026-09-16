//! Literal Round9 layout builders: no production serializer supplies test bytes.
use aeordb::engine::v4::plugin_identity::{decode_plugin_alias_v1, decode_plugin_manifest_payload_v1, PluginManifestRoleV1};

pub fn alias_path(alias: &str) -> String {
  format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(alias.as_bytes()).to_hex())
}

pub fn alias_bytes(alias: &str, id: &str, name: &str, version: Option<&str>, author: Option<&str>, opaque: bool) -> Vec<u8> {
  let mut bytes = vec![0; 128];
  bytes[..4].copy_from_slice(b"APAL");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&128u16.to_le_bytes());
  let flags = u32::from(version.is_none()) | (u32::from(author.is_none()) << 1) | (u32::from(opaque) << 2);
  bytes[12..16].copy_from_slice(&flags.to_le_bytes());
  for (offset, text) in [(16, alias), (20, id), (24, name), (28, version.unwrap_or("")), (32, author.unwrap_or(""))] {
    bytes[offset..offset + 4].copy_from_slice(&(text.len() as u32).to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
  }
  bytes[36..38].copy_from_slice(&1u16.to_le_bytes());
  bytes[38..40].copy_from_slice(&1u16.to_le_bytes());
  bytes[40..72].copy_from_slice(blake3::hash(b"independent raw module fixture").as_bytes());
  bytes[72..80].copy_from_slice(&30u64.to_le_bytes());
  bytes[80..88].copy_from_slice(&(-7i64).to_le_bytes());
  bytes[88..96].copy_from_slice(&13i64.to_le_bytes());
  let length = (bytes.len() + 4) as u32;
  bytes[8..12].copy_from_slice(&length.to_le_bytes());
  bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
  bytes
}

pub fn manifest_bytes(id: &str, name: &str, version: &str, author: &str, roles: &[(u16, u16)]) -> Vec<u8> {
  let mut bytes = vec![0; 64];
  bytes[..4].copy_from_slice(b"APWM");
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&64u16.to_le_bytes());
  for (offset, text) in [(16, id), (20, name), (24, version), (28, author)] {
    bytes[offset..offset + 4].copy_from_slice(&(text.len() as u32).to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
  }
  bytes[32..34].copy_from_slice(&(roles.len() as u16).to_le_bytes());
  for &(role, abi) in roles {
    bytes.extend_from_slice(&role.to_le_bytes());
    bytes.extend_from_slice(&abi.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
  }
  let length = bytes.len() as u32;
  bytes[8..12].copy_from_slice(&length.to_le_bytes());
  bytes
}

fn reseal(bytes: &mut [u8]) {
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn corrected_alias_borrows_exact_metadata_and_checks_its_canonical_name_key() {
  let bytes = alias_bytes("parse/é", "/org/example/parser", "Fixture", Some("1.2.3-rc.1+build.02"), Some("Author"), false);
  let value = decode_plugin_alias_v1(&bytes, &alias_path("parse/é")).unwrap();
  assert_eq!(bytes.len(), 132 + "parse/é".len() + 19 + 7 + 19 + 6);
  assert_eq!(value.alias, "parse/é");
  assert_eq!(value.plugin_id, "/org/example/parser");
  assert_eq!(value.name, "Fixture");
  assert_eq!(value.version, Some("1.2.3-rc.1+build.02"));
  assert_eq!(value.author, Some("Author"));
  assert_eq!(value.flags, 0);
  assert_eq!(value.artifact_fingerprint, blake3::hash(b"independent raw module fixture").as_bytes());
  assert_eq!(value.artifact_length, 30);
  assert_eq!((value.created_at_ms, value.updated_at_ms), (-7, 13));
  assert_eq!(value.alias.as_ptr(), bytes[128..].as_ptr());
  assert_eq!(value.artifact_fingerprint.as_ptr(), bytes[40..].as_ptr());
  assert!(decode_plugin_alias_v1(&bytes, &alias_path("parse/other")).is_err());
  assert!(decode_plugin_alias_v1(&bytes, &alias_path("parse/é").to_uppercase()).is_err());
}

#[test]
fn legacy_metadata_absence_and_opaque_identity_are_explicit_not_corrected_defaults() {
  for version in [None, Some("0.0.0")] {
    for author in [None, Some("Author")] {
      let bytes = alias_bytes("old", "old-plugin-id", "Old", version, author, true);
      let value = decode_plugin_alias_v1(&bytes, &alias_path("old")).unwrap();
      assert_eq!(value.plugin_id, "old-plugin-id");
      assert_eq!(value.version, version);
      assert_eq!(value.author, author);
      assert_ne!(value.flags & 4, 0);
    }
  }
  let bytes = alias_bytes("old", "old-plugin-id", "Old", None, None, false);
  assert!(decode_plugin_alias_v1(&bytes, &alias_path("old")).is_err());
}

#[test]
fn manifest_borrows_strings_and_canonical_role_abi_pairs() {
  for roles in [vec![(1, 3)], vec![(2, 4)], vec![(1, 3), (2, 4)]] {
    let bytes = manifest_bytes("/org/example/parser", "Fixture", "1.0.0", "", &roles);
    let value = decode_plugin_manifest_payload_v1(&bytes).unwrap();
    assert_eq!(bytes.len(), 64 + 19 + 7 + 5 + roles.len() * 8);
    assert_eq!(value.plugin_id, "/org/example/parser");
    assert_eq!(value.name, "Fixture");
    assert_eq!(value.version, "1.0.0");
    assert_eq!(value.author, None);
    assert_eq!(value.plugin_id.as_ptr(), bytes[64..].as_ptr());
    assert_eq!(value.roles().collect::<Vec<_>>(), roles.iter().map(|&(role, abi)| PluginManifestRoleV1 { role, abi }).collect::<Vec<_>>());
  }
}

#[test]
fn both_formats_refuse_every_truncation_trailing_byte_and_invalid_envelope() {
  let alias = alias_bytes("a", "/i", "n", Some("1.0.0"), None, false);
  let manifest = manifest_bytes("/i", "n", "1.0.0", "", &[(1, 3)]);
  for end in 0..alias.len() {
    assert!(decode_plugin_alias_v1(&alias[..end], &alias_path("a")).is_err(), "alias truncation {end}");
  }
  for end in 0..manifest.len() {
    assert!(decode_plugin_manifest_payload_v1(&manifest[..end]).is_err(), "manifest truncation {end}");
  }
  for offset in [0, 4, 6, 8] {
    let mut bytes = alias.clone();
    bytes[offset] ^= 1;
    reseal(&mut bytes);
    assert!(decode_plugin_alias_v1(&bytes, &alias_path("a")).is_err());
    let mut bytes = manifest.clone();
    bytes[offset] ^= 1;
    assert!(decode_plugin_manifest_payload_v1(&bytes).is_err());
  }
  let mut alias = alias;
  alias.push(0);
  assert!(decode_plugin_alias_v1(&alias, &alias_path("a")).is_err());
  let mut manifest = manifest;
  manifest.push(0);
  assert!(decode_plugin_manifest_payload_v1(&manifest).is_err());
}

#[test]
fn canonical_versions_match_independent_semver_including_prerelease_build_and_u64_limits() {
  let versions = [
    "0.0.0",
    "1.2.3",
    "1.0.0-rc.1+build.02",
    "1.0.0--",
    "18446744073709551615.0.0",
    "1.0.0+01",
    "",
    "1.0",
    "v1.0.0",
    "01.0.0",
    "1.0.0-01",
    "1.0.0-",
    "1.0.0+",
    "1.0.0-a..b",
    "1.0.0+a+b",
    "1.0.0-α",
    "1.0.0 ",
    "18446744073709551616.0.0",
  ];
  for version in versions {
    let expected = semver::Version::parse(version).is_ok_and(|parsed| parsed.to_string() == version);
    let manifest = manifest_bytes("/i", "n", version, "", &[(1, 3)]);
    let alias = alias_bytes("a", "/i", "n", Some(version), None, false);
    assert_eq!(decode_plugin_manifest_payload_v1(&manifest).is_ok(), expected, "manifest {version:?}");
    assert_eq!(decode_plugin_alias_v1(&alias, &alias_path("a")).is_ok(), expected, "alias {version:?}");
  }
}

#[test]
fn borrowed_version_grammar_matches_semver_across_ascii_edits_and_numeric_boundaries() {
  let seeds = ["1.2.3", "0.0.0-a.1+build.02", "18446744073709551615.0.0", "1.0.0-999999999999999999999999"];
  for seed in seeds {
    for position in 0..=seed.len() {
      for byte in 0..=127u8 {
        let mut version = seed.as_bytes().to_vec();
        version.insert(position, byte);
        let version = std::str::from_utf8(&version).unwrap();
        let expected = semver::Version::parse(version).is_ok_and(|parsed| parsed.to_string() == version);
        let bytes = manifest_bytes("/i", "n", version, "", &[(1, 3)]);
        assert_eq!(decode_plugin_manifest_payload_v1(&bytes).is_ok(), expected, "{version:?}");
      }
    }
  }
}

#[test]
fn bounds_flags_presence_reserves_and_role_registry_do_not_follow_untrusted_counts() {
  let alias = alias_bytes("a", "/i", "n", Some("1.0.0"), None, false);
  for offset in [12, 16, 20, 24, 28, 32, 36, 38, 72, 96, 127] {
    let mut bytes = alias.clone();
    bytes[offset] = 0xff;
    if offset == 72 {
      bytes[72..80].copy_from_slice(&((64u64 << 20) + 1).to_le_bytes());
    }
    reseal(&mut bytes);
    assert!(decode_plugin_alias_v1(&bytes, &alias_path("a")).is_err(), "alias offset {offset}");
  }
  for roles in [vec![], vec![(1, 4)], vec![(2, 3)], vec![(3, 3)], vec![(1, 3), (1, 3)], vec![(2, 4), (1, 3)]] {
    let bytes = manifest_bytes("/i", "n", "1.0.0", "", &roles);
    assert!(decode_plugin_manifest_payload_v1(&bytes).is_err(), "roles {roles:?}");
  }
  let manifest = manifest_bytes("/i", "n", "1.0.0", "", &[(1, 3)]);
  for offset in [12, 16, 20, 24, 28, 32, 34, 63, manifest.len() - 1] {
    let mut bytes = manifest.clone();
    bytes[offset] = 0xff;
    assert!(decode_plugin_manifest_payload_v1(&bytes).is_err(), "manifest offset {offset}");
  }
}

#[test]
fn maximum_metadata_is_borrowed_while_each_component_rejects_one_extra_byte() {
  let alias = "a".repeat(4096);
  let id = format!("/{}", "i".repeat(4095));
  let name = "n".repeat(4096);
  let version = format!("1.0.0+{}", "b".repeat(250));
  let author = "u".repeat(4096);
  let path = alias_path(&alias);
  let bytes = alias_bytes(&alias, &id, &name, Some(&version), Some(&author), false);
  assert_eq!(bytes.len(), 16_772);
  let decoded = decode_plugin_alias_v1(&bytes, &path).unwrap();
  assert_eq!(decoded.author, Some(author.as_str()));
  assert_eq!(decoded.alias.as_ptr(), bytes[128..].as_ptr());
  let manifest = manifest_bytes(&id, &name, &version, &author, &[(1, 3), (2, 4)]);
  let decoded = decode_plugin_manifest_payload_v1(&manifest).unwrap();
  assert_eq!(decoded.author, Some(author.as_str()));
  assert_eq!(decoded.roles().len(), 2);
  for at in 0..5 {
    let mut fields = [alias.clone(), id.clone(), name.clone(), version.clone(), author.clone()];
    fields[at].push('x');
    let bytes = alias_bytes(&fields[0], &fields[1], &fields[2], Some(&fields[3]), Some(&fields[4]), false);
    assert!(decode_plugin_alias_v1(&bytes, &alias_path(&fields[0])).is_err(), "alias component {at}");
    if at != 0 {
      let bytes = manifest_bytes(&fields[1], &fields[2], &fields[3], &fields[4], &[(1, 3)]);
      assert!(decode_plugin_manifest_payload_v1(&bytes).is_err(), "manifest component {at}");
    }
  }
}

#[test]
fn exact_alias_bytes_and_signed_timestamps_are_not_normalized() {
  for alias in [" Mixed/Case ", "é", "/a/b"] {
    let mut bytes = alias_bytes(alias, "/i", "Display é", Some("1.0.0"), Some("Author"), false);
    bytes[80..88].copy_from_slice(&i64::MIN.to_le_bytes());
    bytes[88..96].copy_from_slice(&i64::MAX.to_le_bytes());
    reseal(&mut bytes);
    let decoded = decode_plugin_alias_v1(&bytes, &alias_path(alias)).unwrap();
    assert_eq!(decoded.alias, alias);
    assert_eq!(decoded.created_at_ms, i64::MIN);
    assert_eq!(decoded.updated_at_ms, i64::MAX);
  }
}

#[test]
fn alias_checksums_key_syntax_and_metadata_encoding_are_checked_independently() {
  let original = alias_bytes("a", "/i", "n", Some("1.0.0"), None, false);
  let path = alias_path("a");
  for position in [40, original.len() - 1] {
    let mut bytes = original.clone();
    bytes[position] ^= 1;
    assert!(decode_plugin_alias_v1(&bytes, &path).is_err());
  }
  for path in [
    format!("{path}x"),
    path[..path.len() - 1].to_string(),
    format!("/.aeordb-system/plugin-aliases/{}", "z".repeat(64)),
    format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(b"a").to_hex().to_string().to_uppercase()),
  ] {
    assert!(decode_plugin_alias_v1(&original, &path).is_err());
  }
  for alias in ["", "bad\0name", "bad\nname", "bad\u{85}name"] {
    let bytes = alias_bytes(alias, "/i", "n", Some("1.0.0"), None, false);
    assert!(decode_plugin_alias_v1(&bytes, &alias_path(alias)).is_err(), "alias {alias:?}");
  }
  for id in ["", "/", "relative", "/a/", "/a//b", "/a/../b", "/a\n"] {
    let alias = alias_bytes("a", id, "n", Some("1.0.0"), None, false);
    assert!(decode_plugin_alias_v1(&alias, &path).is_err(), "ID {id:?}");
    let manifest = manifest_bytes(id, "n", "1.0.0", "", &[(1, 3)]);
    assert!(decode_plugin_manifest_payload_v1(&manifest).is_err(), "ID {id:?}");
  }
  for offset in [128, 129, 131, 132] {
    let mut bytes = original.clone();
    bytes[offset] = 0xff;
    reseal(&mut bytes);
    assert!(decode_plugin_alias_v1(&bytes, &path).is_err(), "UTF-8 at {offset}");
  }
}

#[test]
fn alias_presence_and_artifact_boundaries_are_checked_with_valid_checksums() {
  let path = alias_path("a");
  for (version, author) in [(Some("1.0.0"), Some("Author")), (None, None)] {
    let original = alias_bytes("a", "/i", "n", version, author, false);
    for bit in [1, 2] {
      let mut bytes = original.clone();
      bytes[12] ^= bit;
      reseal(&mut bytes);
      assert_eq!(decode_plugin_alias_v1(&bytes, &path).unwrap_err().code(), "plugin_alias_presence");
    }
    for length in [0, 1, 64u64 << 20, (64u64 << 20) + 1, u64::MAX] {
      let mut bytes = original.clone();
      bytes[72..80].copy_from_slice(&length.to_le_bytes());
      reseal(&mut bytes);
      let result = decode_plugin_alias_v1(&bytes, &path);
      if (1..=(64 << 20)).contains(&length) {
        assert_eq!(result.unwrap().artifact_length, length);
      } else {
        assert_eq!(result.unwrap_err().code(), "plugin_alias_artifact_length");
      }
    }
  }
}

#[test]
fn every_text_field_checks_utf8_while_display_bytes_remain_exact() {
  let alias = alias_bytes("a", "/i", "n", Some("1.0.0"), Some("u"), false);
  for offset in [128, 129, 131, 132, 137] {
    let mut bytes = alias.clone();
    bytes[offset] = 0xff;
    reseal(&mut bytes);
    assert_eq!(decode_plugin_alias_v1(&bytes, &alias_path("a")).unwrap_err().code(), "plugin_identity_utf8");
  }
  let manifest = manifest_bytes("/i", "n", "1.0.0", "u", &[(1, 3)]);
  for offset in [64, 66, 67, 72] {
    let mut bytes = manifest.clone();
    bytes[offset] = 0xff;
    assert_eq!(decode_plugin_manifest_payload_v1(&bytes).unwrap_err().code(), "plugin_identity_utf8");
  }
  let display = " \n\0é ";
  let mut alias = alias_bytes("a", "/i", display, Some("1.0.0"), Some(display), false);
  alias[80..88].copy_from_slice(&i64::MAX.to_le_bytes());
  alias[88..96].copy_from_slice(&i64::MIN.to_le_bytes());
  reseal(&mut alias);
  let decoded = decode_plugin_alias_v1(&alias, &alias_path("a")).unwrap();
  assert_eq!(decoded.name, display);
  assert_eq!(decoded.author, Some(display));
  assert_eq!((decoded.created_at_ms, decoded.updated_at_ms), (i64::MAX, i64::MIN));
  let manifest = manifest_bytes("/i", display, "1.0.0", display, &[(1, 3)]);
  let decoded = decode_plugin_manifest_payload_v1(&manifest).unwrap();
  assert_eq!(decoded.name, display);
  assert_eq!(decoded.author, Some(display));
}
