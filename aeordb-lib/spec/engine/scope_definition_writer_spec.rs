//! Canonical ScopeDefinitionV1 writers; source configuration normalization is separate.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::scope::{ScopeDefinitionWriteV1, ScopeMatchingMode, decode_scope_definition, encode_scope_definition};
use sha2::Digest;

fn fixture(profile: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/scope-definition-v1/ascp-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn independent_identity(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  let mut input = b"aeordb.index.scope-definition.v1\0".to_vec();
  input.extend_from_slice(bytes);
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(&input).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(&input).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(&input).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(&input).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(&input).to_vec(),
  }
}

#[test]
fn scope_writer_matches_every_frozen_scope_fixture_without_using_the_reader_as_an_oracle() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for name in ["root-direct", "normalized-glob", "maximum-length"] {
      let expected = fixture(profile, name);
      // Read the independent fixture's documented offsets, not production decoding.
      let owner_length = u32::from_le_bytes(expected[32..36].try_into().unwrap()) as usize;
      let owner_path = std::str::from_utf8(&expected[64..64 + owner_length]).unwrap();
      let glob = if name == "root-direct" { None } else { Some(std::str::from_utf8(&expected[64 + owner_length..]).unwrap()) };
      let mode = if glob.is_some() { ScopeMatchingMode::RelativePathGlob } else { ScopeMatchingMode::DirectChildren };
      let encoded = encode_scope_definition(ScopeDefinitionWriteV1 { owner_path, glob, mode }, algorithm).unwrap();
      assert_eq!(encoded.value, expected, "{profile}/{name}");
      assert_eq!(encoded.scope_id, independent_identity(algorithm, &expected), "{profile}/{name}");
    }
  }
}

#[test]
fn scope_writer_uses_database_hash_only_for_identity_not_definition_bytes() {
  let expected = fixture("blake3-256", "root-direct");
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let encoded =
      encode_scope_definition(ScopeDefinitionWriteV1 { owner_path: "/", glob: None, mode: ScopeMatchingMode::DirectChildren }, algorithm)
        .unwrap();
    assert_eq!(encoded.value, expected);
    assert_eq!(encoded.scope_id, independent_identity(algorithm, &expected));
    assert_eq!(decode_scope_definition(&encoded.value, algorithm).unwrap().scope_id, encoded.scope_id);
  }
}

#[test]
fn scope_writer_rejects_noncanonical_owner_paths_instead_of_silently_normalizing() {
  for owner_path in ["", "relative", " /docs", "/docs ", "/docs/", "//docs", "/a//b", "/a/./b", "/a/../b", "/a\0b"] {
    let error = encode_scope_definition(
      ScopeDefinitionWriteV1 { owner_path, glob: None, mode: ScopeMatchingMode::DirectChildren },
      HashAlgorithm::Blake3_256,
    )
    .unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "{owner_path:?}");
  }
}

#[test]
fn scope_writer_rejects_noncanonical_globs_and_mismatched_mode_presence() {
  for glob in ["", "/x", "x/", "a//b", ".", "..", "a/./b", "a/../b", "x\0y"] {
    assert!(
      encode_scope_definition(
        ScopeDefinitionWriteV1 { owner_path: "/", glob: Some(glob), mode: ScopeMatchingMode::RelativePathGlob },
        HashAlgorithm::Blake3_256
      )
      .is_err(),
      "{glob:?}"
    );
  }
  for glob in [Some(""), Some("*")] {
    assert!(encode_scope_definition(
      ScopeDefinitionWriteV1 { owner_path: "/", glob, mode: ScopeMatchingMode::DirectChildren },
      HashAlgorithm::Blake3_256
    )
    .is_err());
  }
  assert!(encode_scope_definition(
    ScopeDefinitionWriteV1 { owner_path: "/", glob: None, mode: ScopeMatchingMode::RelativePathGlob },
    HashAlgorithm::Blake3_256
  )
  .is_err());
}

#[test]
fn scope_writer_enforces_the_combined_byte_boundary_including_multibyte_utf8() {
  for glob in [None, Some("*")] {
    let maximum_owner_length = 65_536 - 64 - glob.map_or(0, str::len);
    let owner_path = format!("/{}", "a".repeat(maximum_owner_length - 1));
    let mode = if glob.is_some() { ScopeMatchingMode::RelativePathGlob } else { ScopeMatchingMode::DirectChildren };
    let encoded = encode_scope_definition(ScopeDefinitionWriteV1 { owner_path: &owner_path, glob, mode }, HashAlgorithm::Sha512).unwrap();
    assert_eq!(encoded.value.len(), 65_536);
    for extra in ["a", "é"] {
      let oversized = format!("{owner_path}{extra}");
      let error =
        encode_scope_definition(ScopeDefinitionWriteV1 { owner_path: &oversized, glob, mode }, HashAlgorithm::Sha512).unwrap_err();
      assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
    }
  }
}

#[test]
fn scope_writer_preserves_case_unicode_and_glob_literals_in_identity() {
  let mut identities = std::collections::BTreeSet::new();
  for owner_path in ["/docs", "/Docs", "/é", "/e\u{301}"] {
    for glob in ["?", "*", "**", "[a]", "\\a"] {
      let encoded = encode_scope_definition(
        ScopeDefinitionWriteV1 { owner_path, glob: Some(glob), mode: ScopeMatchingMode::RelativePathGlob },
        HashAlgorithm::Blake3_256,
      )
      .unwrap();
      assert!(identities.insert(encoded.scope_id.clone()));
      let decoded = decode_scope_definition(&encoded.value, HashAlgorithm::Blake3_256).unwrap();
      assert_eq!(decoded.owner_path, owner_path);
      assert_eq!(decoded.glob, Some(glob));
    }
  }
}
