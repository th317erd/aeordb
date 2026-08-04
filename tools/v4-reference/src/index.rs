use crate::core::HashProfile;

const AIDX_HEADER_LENGTH: usize = 32;

#[derive(Clone, Copy)]
pub enum IndexFormat {
  IndexArtifactV1,
}

impl IndexFormat {
  pub fn id(self) -> &'static str {
    "index-artifact-v1"
  }

  pub fn family(self) -> &'static str {
    "IndexArtifactV1"
  }
}

#[derive(Clone)]
pub struct IndexFixtureCase {
  pub id: &'static str,
  pub format: IndexFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
enum PointerKind {
  FieldIndex,
  FieldNvt,
  ScopeCatalog,
}

impl PointerKind {
  fn id(self) -> u16 {
    match self {
      Self::FieldIndex => 0x0001,
      Self::FieldNvt => 0x0002,
      Self::ScopeCatalog => 0x0003,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::FieldIndex => "field-index",
      Self::FieldNvt => "field-nvt",
      Self::ScopeCatalog => "scope-catalog",
    }
  }

  fn relation(self) -> &'static str {
    match self {
      Self::FieldIndex => "targets:FieldIndexManifestV1",
      Self::FieldNvt => "targets:FieldNvtManifestV1",
      Self::ScopeCatalog => "targets:ScopeCatalogManifestV1",
    }
  }
}

pub fn fixture_cases() -> Vec<IndexFixtureCase> {
  let mut cases = Vec::with_capacity(12);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for kind in [PointerKind::FieldIndex, PointerKind::FieldNvt, PointerKind::ScopeCatalog] {
      for slot in [0u8, 1u8] {
        let sequence = if slot == 0 { 1 } else { u64::MAX };
        let generation = 700 + u64::from(kind.id());
        let bytes = build_pointer(profile, kind, slot, sequence, generation);
        let key = pointer_key(profile, kind.id(), pointer_identity(profile, kind, slot));
        cases.push(IndexFixtureCase {
          id: fixture_id(profile, kind, slot),
          format: IndexFormat::IndexArtifactV1,
          profile,
          expected: expected_result(kind, slot),
          relation: Some(kind.relation()),
          canonical_key: Some(hex::encode(key)),
          bytes,
        });
      }
    }
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_pointer(profile, bytes) {
    Ok(pointer) => (
      format!("index:pointer:{}:slot-{}:sequence={}", pointer.kind.name(), if pointer.slot == 0 { 'a' } else { 'b' }, pointer.sequence),
      Some(hex::encode(pointer.key)),
    ),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let kind = read_u16(bytes, 6).unwrap_or(0);
  let h = profile.width();
  vec![
    "envelope +0x000 len 32: AIDX common envelope".to_string(),
    format!("envelope artifact_kind: 0x{kind:04x}"),
    format!("identity +0x000 len {h}: owner_id"),
    format!("identity +0x{h:03x} len 1: slot"),
    "body +0x000 len 8: pointer_sequence".to_string(),
    format!("body +0x008 len {h}: target_manifest_hash"),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

fn build_pointer(profile: HashProfile, kind: PointerKind, slot: u8, sequence: u64, generation: u64) -> Vec<u8> {
  let identity = pointer_identity(profile, kind, slot);
  let body_length = 8 + profile.width();
  let total_length = AIDX_HEADER_LENGTH + identity.len() + body_length + 4;
  let mut value = vec![0u8; total_length];
  value[0..4].copy_from_slice(b"AIDX");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, kind.id());
  put_u16(&mut value, 8, AIDX_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, total_length as u32);
  put_u16(&mut value, 16, identity.len() as u16);
  put_u32(&mut value, 20, body_length as u32);
  put_u64(&mut value, 24, generation);
  value[AIDX_HEADER_LENGTH..AIDX_HEADER_LENGTH + identity.len()].copy_from_slice(&identity);
  let body_offset = AIDX_HEADER_LENGTH + identity.len();
  put_u64(&mut value, body_offset, sequence);
  fill_sequence(&mut value[body_offset + 8..body_offset + 8 + profile.width()], 0x80u8.wrapping_add(kind.id() as u8));
  write_trailing_crc(&mut value);
  value
}

fn pointer_identity(profile: HashProfile, kind: PointerKind, slot: u8) -> Vec<u8> {
  let mut identity = vec![0u8; profile.width() + 1];
  fill_sequence(&mut identity[..profile.width()], 0x20u8.wrapping_add(kind.id() as u8 * 0x10));
  identity[profile.width()] = slot;
  identity
}

fn pointer_key(profile: HashProfile, kind: u16, identity: Vec<u8>) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(42 + identity.len());
  preimage.extend_from_slice(b"aeordb.index-artifact.pointer.v1\0");
  preimage.extend_from_slice(&kind.to_le_bytes());
  preimage.extend_from_slice(&identity);
  profile.digest(&preimage)
}

struct DecodedPointer {
  kind: PointerKind,
  slot: u8,
  sequence: u64,
  #[cfg(test)]
  target: Vec<u8>,
  key: Vec<u8>,
}

fn decode_pointer(profile: HashProfile, value: &[u8]) -> Result<DecodedPointer, &'static str> {
  let expected_length = 45 + 2 * profile.width();
  if value.len() != expected_length {
    return Err("index_pointer_length");
  }
  if &value[0..4] != b"AIDX" || read_u16(value, 4)? != 1 || read_u16(value, 8)? != AIDX_HEADER_LENGTH as u16 {
    return Err("index_envelope");
  }
  let kind = match read_u16(value, 6)? {
    0x0001 => PointerKind::FieldIndex,
    0x0002 => PointerKind::FieldNvt,
    0x0003 => PointerKind::ScopeCatalog,
    _ => return Err("index_pointer_kind"),
  };
  if read_u16(value, 10)? != 0
    || read_u32(value, 12)? as usize != value.len()
    || read_u16(value, 16)? as usize != profile.width() + 1
    || read_u16(value, 18)? != 0
    || read_u32(value, 20)? as usize != 8 + profile.width()
    || read_u64(value, 24)? == 0
  {
    return Err("index_envelope_metadata");
  }
  verify_trailing_crc(value)?;
  let identity = &value[32..33 + profile.width()];
  if identity[..profile.width()].iter().all(|byte| *byte == 0) || identity[profile.width()] > 1 {
    return Err("index_pointer_identity");
  }
  let body_offset = 33 + profile.width();
  let sequence = read_u64(value, body_offset)?;
  let target = value[body_offset + 8..body_offset + 8 + profile.width()].to_vec();
  if sequence == 0 || target.iter().all(|byte| *byte == 0) {
    return Err("index_pointer_body");
  }
  Ok(DecodedPointer {
    kind,
    slot: identity[profile.width()],
    sequence,
    #[cfg(test)]
    target,
    key: pointer_key(profile, kind.id(), identity.to_vec()),
  })
}

fn fixture_id(profile: HashProfile, kind: PointerKind, slot: u8) -> &'static str {
  match (profile, kind, slot) {
    (HashProfile::Blake3_256, PointerKind::FieldIndex, 0) => "aidx-blake3-256-field-index-pointer-a",
    (HashProfile::Blake3_256, PointerKind::FieldIndex, 1) => "aidx-blake3-256-field-index-pointer-b-max-sequence",
    (HashProfile::Blake3_256, PointerKind::FieldNvt, 0) => "aidx-blake3-256-field-nvt-pointer-a",
    (HashProfile::Blake3_256, PointerKind::FieldNvt, 1) => "aidx-blake3-256-field-nvt-pointer-b-max-sequence",
    (HashProfile::Blake3_256, PointerKind::ScopeCatalog, 0) => "aidx-blake3-256-scope-catalog-pointer-a",
    (HashProfile::Blake3_256, PointerKind::ScopeCatalog, 1) => "aidx-blake3-256-scope-catalog-pointer-b-max-sequence",
    (HashProfile::Sha512, PointerKind::FieldIndex, 0) => "aidx-sha512-field-index-pointer-a",
    (HashProfile::Sha512, PointerKind::FieldIndex, 1) => "aidx-sha512-field-index-pointer-b-max-sequence",
    (HashProfile::Sha512, PointerKind::FieldNvt, 0) => "aidx-sha512-field-nvt-pointer-a",
    (HashProfile::Sha512, PointerKind::FieldNvt, 1) => "aidx-sha512-field-nvt-pointer-b-max-sequence",
    (HashProfile::Sha512, PointerKind::ScopeCatalog, 0) => "aidx-sha512-scope-catalog-pointer-a",
    (HashProfile::Sha512, PointerKind::ScopeCatalog, 1) => "aidx-sha512-scope-catalog-pointer-b-max-sequence",
    _ => unreachable!("fixture slots are canonical booleans"),
  }
}

fn expected_result(kind: PointerKind, slot: u8) -> &'static str {
  match (kind, slot) {
    (PointerKind::FieldIndex, 0) => "index:pointer:field-index:slot-a:sequence=1",
    (PointerKind::FieldIndex, 1) => "index:pointer:field-index:slot-b:sequence=18446744073709551615",
    (PointerKind::FieldNvt, 0) => "index:pointer:field-nvt:slot-a:sequence=1",
    (PointerKind::FieldNvt, 1) => "index:pointer:field-nvt:slot-b:sequence=18446744073709551615",
    (PointerKind::ScopeCatalog, 0) => "index:pointer:scope-catalog:slot-a:sequence=1",
    (PointerKind::ScopeCatalog, 1) => "index:pointer:scope-catalog:slot-b:sequence=18446744073709551615",
    _ => unreachable!("fixture slots are canonical booleans"),
  }
}

#[cfg(test)]
fn select_pointer_pair(left: &DecodedPointer, right: &DecodedPointer) -> Result<u8, &'static str> {
  if left.sequence > right.sequence {
    Ok(left.slot)
  } else if right.sequence > left.sequence {
    Ok(right.slot)
  } else if left.target == right.target {
    Ok(0)
  } else {
    Err("ambiguous_equal_sequence")
  }
}

fn fill_sequence(bytes: &mut [u8], start: u8) {
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(index as u8);
  }
}

fn write_trailing_crc(bytes: &mut [u8]) {
  let crc_offset = bytes.len() - 4;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  put_u32(bytes, crc_offset, crc);
}

fn verify_trailing_crc(bytes: &[u8]) -> Result<(), &'static str> {
  if bytes.len() < 4 || read_u32(bytes, bytes.len() - 4)? != crc32fast::hash(&bytes[..bytes.len() - 4]) {
    return Err("crc_mismatch");
  }
  Ok(())
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let raw = bytes.get(offset..offset + 2).ok_or("truncated")?;
  Ok(u16::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let raw = bytes.get(offset..offset + 4).ok_or("truncated")?;
  Ok(u32::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  let raw = bytes.get(offset..offset + 8).ok_or("truncated")?;
  Ok(u64::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pointer_fixtures_match_results_and_keys() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn every_pointer_fixture_byte_is_integrity_protected() {
    for case in fixture_cases() {
      for index in 0..case.bytes.len() {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 0x01;
        let (observed, _) = observe(case.profile, &mutated);
        assert!(observed.starts_with("error:"), "fixture {} byte {index} unexpectedly produced {observed}", case.id);
      }
    }
  }

  #[test]
  fn pointer_pair_selection_is_deterministic() {
    let profile = HashProfile::Blake3_256;
    let low = decode_pointer(profile, &build_pointer(profile, PointerKind::FieldIndex, 0, 1, 7)).unwrap();
    let high = decode_pointer(profile, &build_pointer(profile, PointerKind::FieldIndex, 1, 2, 8)).unwrap();
    assert_eq!(select_pointer_pair(&low, &high), Ok(1));

    let equal_a = decode_pointer(profile, &build_pointer(profile, PointerKind::FieldIndex, 0, 3, 9)).unwrap();
    let mut equal_b = decode_pointer(profile, &build_pointer(profile, PointerKind::FieldIndex, 1, 3, 9)).unwrap();
    equal_b.target = equal_a.target.clone();
    assert_eq!(select_pointer_pair(&equal_a, &equal_b), Ok(0));

    let mut conflict = decode_pointer(profile, &build_pointer(profile, PointerKind::FieldIndex, 1, 3, 9)).unwrap();
    conflict.target[0] ^= 0x01;
    assert_eq!(select_pointer_pair(&equal_a, &conflict), Err("ambiguous_equal_sequence"));
  }
}
