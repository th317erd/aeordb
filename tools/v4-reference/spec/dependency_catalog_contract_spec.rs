//! Independent reference checks for the owner-approved Round 16 catalog rule.
use super::{CoreFormat, HashProfile, observe};

fn record(profile: HashProfile, class: u16) -> Vec<u8> {
  let suffix = if class == 6 { "wasm-mapper" } else { "native-parser-resolution" };
  let path = format!(
    "{}/../../aeordb-lib/spec/fixtures/v4/dependency-table-v1/adpt-{}-{suffix}-valid.bin",
    env!("CARGO_MANIFEST_DIR"),
    profile.label()
  );
  std::fs::read(path).unwrap()[32..].to_vec()
}

fn identity(profile: HashProfile, class: u16, bytes: &[u8]) -> Vec<u8> {
  let domain: &[u8] = if class == 6 {
    b"aeordb.semantic.executable-dependency-definition.v1\0"
  } else {
    b"aeordb.semantic.native-dependency-definition.v1\0"
  };
  profile.digest(&[domain, bytes].concat())
}

fn envelope(kind: u16, body: &[u8]) -> Vec<u8> {
  let mut bytes = Vec::new();
  bytes.extend_from_slice(b"ASEM");
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&kind.to_le_bytes());
  bytes.extend_from_slice(&32u16.to_le_bytes());
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes.extend_from_slice(&((36 + body.len()) as u32).to_le_bytes());
  bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&1u64.to_le_bytes());
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes.extend_from_slice(body);
  let checksum = crc32fast::hash(&bytes);
  bytes.extend_from_slice(&checksum.to_le_bytes());
  bytes
}

fn definition(profile: HashProfile, class: u16, payload: &[u8], semantic_id: &[u8]) -> Vec<u8> {
  assert_eq!(semantic_id.len(), profile.width());
  let mut body = Vec::new();
  body.extend_from_slice(&class.to_le_bytes());
  body.extend_from_slice(&1u16.to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(semantic_id);
  body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(payload);
  envelope(4, &body)
}

fn leaf(profile: HashProfile, class: u16, semantic_id: &[u8], owner: &[u8]) -> Vec<u8> {
  let lookup = profile.digest(&[b"aeordb.semantic-catalog-key.v1\0".as_slice(), &class.to_le_bytes(), owner].concat());
  let mut body = Vec::new();
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(&1u32.to_le_bytes());
  body.extend_from_slice(&lookup);
  body.extend_from_slice(&((8 + 2 * profile.width() + owner.len()) as u32).to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(&class.to_le_bytes());
  body.extend_from_slice(&0u16.to_le_bytes());
  body.extend_from_slice(&(owner.len() as u32).to_le_bytes());
  body.extend_from_slice(semantic_id);
  body.extend_from_slice(&vec![0x71; profile.width()]);
  body.extend_from_slice(owner);
  envelope(2, &body)
}

fn accepted(profile: HashProfile, bytes: &[u8]) -> bool {
  !observe(CoreFormat::SemanticObjectV1, profile, bytes).0.starts_with("error:")
}

#[test]
fn reference_accepts_complete_dependency_keys_at_both_database_widths() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [6, 7] {
      let payload = record(profile, class);
      let semantic_id = identity(profile, class, &payload);
      assert!(accepted(profile, &definition(profile, class, &payload, &semantic_id)));
      assert!(accepted(profile, &leaf(profile, class, &semantic_id, &semantic_id)));
    }
  }
}

#[test]
fn reference_rejects_wrong_owner_identity_including_raw_artifact_fingerprints() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [6, 7] {
      let payload = record(profile, class);
      let semantic_id = identity(profile, class, &payload);
      let mut mismatched = semantic_id.clone();
      mismatched[0] ^= 1;
      for owner in [mismatched, payload[40..72].to_vec()] {
        assert!(!accepted(profile, &leaf(profile, class, &semantic_id, &owner)), "{profile:?}, class {class}");
      }
    }
  }
}

#[test]
fn reference_rejects_dependency_keys_with_non_database_width() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [6, 7] {
      let semantic_id = identity(profile, class, &record(profile, class));
      for length in [0, 1, profile.width() - 1, profile.width() + 1, 65] {
        assert!(!accepted(profile, &leaf(profile, class, &semantic_id, &vec![0x42; length])), "{profile:?}, length {length}");
      }
    }
  }
}

#[test]
fn reference_recomputes_dependency_semantic_ids_with_the_correct_class_domain() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [6, 7] {
      let payload = record(profile, class);
      let mut stale = identity(profile, class, &payload);
      stale[0] ^= 1;
      let wrong_class = identity(profile, if class == 6 { 7 } else { 6 }, &payload);
      for semantic_id in [stale, wrong_class] {
        assert!(!accepted(profile, &definition(profile, class, &payload, &semantic_id)), "{profile:?}, class {class}");
      }
    }
  }
}

#[test]
fn reference_rejects_correctly_hashed_malformed_and_wrong_class_dependency_bytes() {
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for class in [6, 7] {
      let mut reserved = record(profile, class);
      reserved[72] = 1;
      for payload in [Vec::new(), reserved, record(profile, if class == 6 { 7 } else { 6 })] {
        let semantic_id = identity(profile, class, &payload);
        assert!(!accepted(profile, &definition(profile, class, &payload, &semantic_id)), "{profile:?}, class {class}");
      }
    }
  }
}
