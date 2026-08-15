use crate::core::HashProfile;

const IDENTITY_DOMAIN: &[u8] = b"aeordb.migration-capture-manifest.v1\0";
const FIXED_PREFIX_LENGTH: usize = 176;
const HASH_COUNT: usize = 7;
const SOURCE_AUTHORITY_DIGEST_LENGTH: usize = 32;
const RESERVED_LENGTH: usize = 64;
const CRC_LENGTH: usize = 4;

#[derive(Clone, Copy)]
pub enum MigrationCaptureFormat {
  ManifestV1,
}

impl MigrationCaptureFormat {
  pub fn id(self) -> &'static str {
    "migration-capture-v1"
  }

  pub fn family(self) -> &'static str {
    "MigrationCaptureManifestV1"
  }
}

#[derive(Clone)]
pub struct MigrationCaptureFixtureCase {
  pub id: &'static str,
  pub format: MigrationCaptureFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

pub fn fixture_cases() -> Vec<MigrationCaptureFixtureCase> {
  [HashProfile::Blake3_256, HashProfile::Sha512]
    .into_iter()
    .flat_map(|profile| {
      [false, true].map(|initial| {
        let bytes = build_manifest(profile, initial);
        let mut identity_preimage = Vec::with_capacity(IDENTITY_DOMAIN.len() + bytes.len());
        identity_preimage.extend_from_slice(IDENTITY_DOMAIN);
        identity_preimage.extend_from_slice(&bytes);
        MigrationCaptureFixtureCase {
          id: match (profile, initial) {
            (HashProfile::Blake3_256, false) => "amcm-blake3-256-capturing-valid",
            (HashProfile::Sha512, false) => "amcm-sha512-capturing-valid",
            (HashProfile::Blake3_256, true) => "amcm-blake3-256-initial-empty-valid",
            (HashProfile::Sha512, true) => "amcm-sha512-initial-empty-valid",
          },
          format: MigrationCaptureFormat::ManifestV1,
          profile,
          expected: if initial {
            "migration:capture:state=capturing:checkpoint=1:segments=0:captured=0:observed=0"
          } else {
            "migration:capture:state=capturing:checkpoint=3:segments=2:captured=110:observed=110"
          },
          relation: Some("selects:external-migration-capture-segment-chain"),
          canonical_key: Some(hex::encode(profile.digest(&identity_preimage))),
          bytes,
        }
      })
    })
    .collect()
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  let observed = decode_manifest(profile, bytes).unwrap_or_else(|error| format!("error:{error}"));
  let mut identity_preimage = Vec::with_capacity(IDENTITY_DOMAIN.len() + bytes.len());
  identity_preimage.extend_from_slice(IDENTITY_DOMAIN);
  identity_preimage.extend_from_slice(bytes);
  (observed, Some(hex::encode(profile.digest(&identity_preimage))))
}

pub fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  vec![
    "manifest +0x000 len 24: AMCM envelope, state, flags, and complete length".to_string(),
    "manifest +0x018 len 64: logical database, migration, source, and destination identities".to_string(),
    "manifest +0x058 len 88: fence, checkpoint, time, publication, and segment closure".to_string(),
    format!("manifest +0x0b0 len {}: seven selected-profile closure hashes", HASH_COUNT * profile.width()),
    format!("manifest +0x{:03x} len 32: source authority digest", 176 + HASH_COUNT * profile.width()),
    format!("manifest +0x{:03x} len 64: future reserve", 208 + HASH_COUNT * profile.width()),
    format!("manifest +0x{:03x} len 4: crc32", bytes.len().saturating_sub(4)),
  ]
}

fn build_manifest(profile: HashProfile, initial: bool) -> Vec<u8> {
  let expected = manifest_length(profile);
  let mut bytes = Vec::with_capacity(expected);
  bytes.extend_from_slice(b"AMCM");
  put_u16_vec(&mut bytes, 1);
  put_u16_vec(&mut bytes, profile.algorithm_id());
  put_u16_vec(&mut bytes, 1);
  put_u16_vec(&mut bytes, 0);
  put_u32_vec(&mut bytes, 0);
  put_u32_vec(&mut bytes, expected as u32);
  put_u32_vec(&mut bytes, 0);
  append_sequence(&mut bytes, 0x10, 16);
  append_sequence(&mut bytes, 0x20, 16);
  append_sequence(&mut bytes, 0x30, 16);
  append_sequence(&mut bytes, 0x40, 16);
  for value in [9, 2, if initial { 1 } else { 3 }] {
    put_u64_vec(&mut bytes, value);
  }
  put_i64_vec(&mut bytes, 1_700_000_000_000);
  put_i64_vec(&mut bytes, 1_700_000_001_000);
  let closure = if initial { [0, 0, 0, 0, 0, 0] } else { [110, 110, 4, 5, 2, 2_048] };
  for value in closure {
    put_u64_vec(&mut bytes, value);
  }
  append_sequence(&mut bytes, 0x50, profile.width());
  append_sequence(&mut bytes, if initial { 0x50 } else { 0x60 }, profile.width());
  if initial {
    bytes.resize(bytes.len() + 2 * profile.width(), 0);
  } else {
    append_sequence(&mut bytes, 0x70, profile.width());
    append_sequence(&mut bytes, 0x80, profile.width());
  }
  append_sequence(&mut bytes, 0x90, profile.width());
  append_sequence(&mut bytes, 0xa0, profile.width());
  bytes.resize(bytes.len() + profile.width(), 0);
  append_sequence(&mut bytes, 0xb0, SOURCE_AUTHORITY_DIGEST_LENGTH);
  bytes.resize(expected - CRC_LENGTH, 0);
  let crc = crc32fast::hash(&bytes);
  put_u32_vec(&mut bytes, crc);
  assert_eq!(bytes.len(), expected);
  bytes
}

fn decode_manifest(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  let expected = manifest_length(profile);
  if bytes.len() != expected {
    return Err("length");
  }
  if read_u32(bytes, expected - CRC_LENGTH)? != crc32fast::hash(&bytes[..expected - CRC_LENGTH]) {
    return Err("crc");
  }
  if &bytes[..4] != b"AMCM" || read_u16(bytes, 4)? != 1 {
    return Err("magic_or_version");
  }
  if read_u16(bytes, 6)? != profile.algorithm_id() {
    return Err("hash_algorithm");
  }
  if read_u16(bytes, 8)? != 1 || read_u16(bytes, 10)? != 0 || read_u32(bytes, 12)? != 0 {
    return Err("state_flags_or_reserve");
  }
  if read_u32(bytes, 16)? as usize != expected || read_u32(bytes, 20)? != 0 {
    return Err("length_or_reserve");
  }
  if bytes[24..88].chunks_exact(16).any(|identity| identity.iter().all(|byte| *byte == 0)) {
    return Err("zero_identity");
  }
  if [read_u64(bytes, 88)?, read_u64(bytes, 96)?, read_u64(bytes, 104)?].contains(&0) {
    return Err("zero_generation");
  }
  if read_i64(bytes, 112)? < 0 || read_i64(bytes, 120)? < read_i64(bytes, 112)? {
    return Err("time");
  }
  let checkpoint = read_u64(bytes, 104)?;
  let captured = read_u64(bytes, 128)?;
  let observed = read_u64(bytes, 136)?;
  let first = read_u64(bytes, 144)?;
  let last = read_u64(bytes, 152)?;
  let count = read_u64(bytes, 160)?;
  let stored_bytes = read_u64(bytes, 168)?;
  let source_root_before = &bytes[FIXED_PREFIX_LENGTH..FIXED_PREFIX_LENGTH + profile.width()];
  let source_root_after = &bytes[FIXED_PREFIX_LENGTH + profile.width()..FIXED_PREFIX_LENGTH + 2 * profile.width()];
  let segment_head = &bytes[FIXED_PREFIX_LENGTH + 2 * profile.width()..FIXED_PREFIX_LENGTH + 3 * profile.width()];
  let previous_manifest = &bytes[FIXED_PREFIX_LENGTH + 3 * profile.width()..FIXED_PREFIX_LENGTH + 4 * profile.width()];
  let config = &bytes[FIXED_PREFIX_LENGTH + 4 * profile.width()..FIXED_PREFIX_LENGTH + 5 * profile.width()];
  let registry = &bytes[FIXED_PREFIX_LENGTH + 5 * profile.width()..FIXED_PREFIX_LENGTH + 6 * profile.width()];
  let failure = &bytes[FIXED_PREFIX_LENGTH + 6 * profile.width()..FIXED_PREFIX_LENGTH + 7 * profile.width()];
  let empty_segments = count == 0
    && captured == 0
    && observed == 0
    && first == 0
    && last == 0
    && stored_bytes == 0
    && all_zero(segment_head)
    && source_root_before == source_root_after;
  let populated_segments = count != 0
    && captured != 0
    && captured == observed
    && first != 0
    && last.checked_sub(first).and_then(|span| span.checked_add(1)) == Some(count)
    && stored_bytes >= count
    && !all_zero(segment_head);
  if !(empty_segments || populated_segments)
    || (checkpoint == 1) != all_zero(previous_manifest)
    || all_zero(source_root_before)
    || all_zero(source_root_after)
    || all_zero(config)
    || all_zero(registry)
    || !all_zero(failure)
  {
    return Err("sequence_or_segment_closure");
  }
  let hashes_end = FIXED_PREFIX_LENGTH + HASH_COUNT * profile.width();
  if bytes[hashes_end..hashes_end + SOURCE_AUTHORITY_DIGEST_LENGTH].iter().all(|byte| *byte == 0)
    || bytes[hashes_end + SOURCE_AUTHORITY_DIGEST_LENGTH..expected - CRC_LENGTH].iter().any(|byte| *byte != 0)
  {
    return Err("hash_or_reserve_closure");
  }
  Ok(format!("migration:capture:state=capturing:checkpoint={}:segments={count}:captured={captured}:observed={observed}", checkpoint))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn manifest_length(profile: HashProfile) -> usize {
  FIXED_PREFIX_LENGTH + HASH_COUNT * profile.width() + SOURCE_AUTHORITY_DIGEST_LENGTH + RESERVED_LENGTH + CRC_LENGTH
}

fn append_sequence(bytes: &mut Vec<u8>, first: u8, length: usize) {
  bytes.extend((0..length).map(|offset| first.wrapping_add(offset as u8)));
}

fn put_u16_vec(bytes: &mut Vec<u8>, value: u16) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u32_vec(bytes: &mut Vec<u8>, value: u32) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64_vec(bytes: &mut Vec<u8>, value: u64) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_i64_vec(bytes: &mut Vec<u8>, value: i64) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  Ok(u16::from_le_bytes(bytes.get(offset..offset + 2).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  Ok(u32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  Ok(u64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, &'static str> {
  Ok(i64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}
