use std::cmp::Ordering;

use crate::core::HashProfile;

const TABLE_HEADER_LENGTH: usize = 32;
const RECORD_HEADER_LENGTH: usize = 96;
const TABLE_MAX_LENGTH: usize = 256 * 1_024;
const MAX_RECORDS: usize = 1_024;

#[derive(Clone, Copy)]
pub enum DependencyFormat {
  DependencyTableV1,
}

impl DependencyFormat {
  pub fn id(self) -> &'static str {
    "dependency-table-v1"
  }

  pub fn family(self) -> &'static str {
    "DependencyTableV1"
  }
}

#[derive(Clone)]
pub struct DependencyFixtureCase {
  pub id: &'static str,
  pub format: DependencyFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone)]
struct DependencyRecord {
  kind: u16,
  role: u16,
  flags: u32,
  abi: u16,
  executor_profile: u16,
  fingerprint_semantics: u16,
  artifact_kind: u16,
  artifact_length: u64,
  fingerprint: [u8; 32],
  dependency_id: String,
  version: String,
}

pub fn fixture_cases() -> Vec<DependencyFixtureCase> {
  let native = native_record();
  let wasm = wasm_record();
  let mut cases = Vec::with_capacity(6);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (suffix, records, expected, relation) in [
      ("empty", Vec::new(), "dependencies:records=0", Some("canonical-empty-table")),
      ("native-parser-resolution", vec![native.clone()], "dependencies:records=1", Some("native:parser-resolution")),
      ("wasm-mapper", vec![wasm.clone()], "dependencies:records=1", Some("wasm:mapper-binary-canonical-v1")),
    ] {
      cases.push(DependencyFixtureCase {
        id: fixture_id(profile, suffix),
        format: DependencyFormat::DependencyTableV1,
        profile,
        expected,
        relation,
        canonical_key: None,
        bytes: build_table(&records).expect("fixture dependency table must encode"),
      });
    }
  }
  cases
}

pub fn observe(_profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_table(bytes) {
    Ok(count) => (format!("dependencies:records={count}"), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(bytes: &[u8]) -> Vec<String> {
  let count = read_u32(bytes, 16).unwrap_or(0);
  vec![
    "table +0x000 len 32: ADPT header".to_string(),
    format!("table record_count: {count}"),
    format!("table +0x020 len {}: canonical dependency records", bytes.len().saturating_sub(TABLE_HEADER_LENGTH)),
  ]
}

fn native_record() -> DependencyRecord {
  DependencyRecord {
    kind: 2,
    role: 3,
    flags: 0,
    abi: 0,
    executor_profile: 1,
    fingerprint_semantics: 2,
    artifact_kind: 0,
    artifact_length: 0,
    fingerprint: digest32(b"aeordb-native-parser-resolution-v1-conformance"),
    dependency_id: "/org/aeordev/aeordb/native/parser-resolution-v1".to_string(),
    version: "1.0.0".to_string(),
  }
}

fn wasm_record() -> DependencyRecord {
  DependencyRecord {
    kind: 1,
    role: 2,
    flags: 0x04,
    abi: 4,
    executor_profile: 2,
    fingerprint_semantics: 1,
    artifact_kind: 1,
    artifact_length: 4_096,
    fingerprint: digest32(b"fixture-wasm-mapper-module-v1"),
    dependency_id: "/org/aeordev/aeordb/plugins/fixture-mapper".to_string(),
    version: "1.0.0".to_string(),
  }
}

fn build_table(records: &[DependencyRecord]) -> Result<Vec<u8>, &'static str> {
  if records.len() > MAX_RECORDS {
    return Err("dependency_record_count");
  }
  for pair in records.windows(2) {
    if compare_records(&pair[0], &pair[1]) != Ordering::Less {
      return Err("dependency_record_order");
    }
  }
  let mut body = Vec::new();
  for record in records {
    let encoded = build_record(record)?;
    if TABLE_HEADER_LENGTH + body.len() + encoded.len() > TABLE_MAX_LENGTH {
      return Err("dependency_table_oversize");
    }
    body.extend_from_slice(&encoded);
  }
  let mut value = vec![0u8; TABLE_HEADER_LENGTH];
  value[0..4].copy_from_slice(b"ADPT");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, TABLE_HEADER_LENGTH as u16);
  put_u32(&mut value, 8, (TABLE_HEADER_LENGTH + body.len()) as u32);
  put_u32(&mut value, 16, records.len() as u32);
  put_u32(&mut value, 20, body.len() as u32);
  value.extend_from_slice(&body);
  Ok(value)
}

fn build_record(record: &DependencyRecord) -> Result<Vec<u8>, &'static str> {
  validate_record_fields(record)?;
  let total_length = RECORD_HEADER_LENGTH + record.dependency_id.len() + record.version.len();
  let mut value = vec![0u8; total_length];
  put_u32(&mut value, 0, total_length as u32);
  put_u16(&mut value, 4, record.kind);
  put_u16(&mut value, 6, record.role);
  put_u32(&mut value, 8, record.flags);
  put_u16(&mut value, 12, record.abi);
  put_u16(&mut value, 14, record.executor_profile);
  put_u16(&mut value, 16, record.fingerprint_semantics);
  put_u16(&mut value, 18, record.artifact_kind);
  put_u32(&mut value, 20, record.dependency_id.len() as u32);
  put_u32(&mut value, 24, record.version.len() as u32);
  put_u64(&mut value, 32, record.artifact_length);
  value[40..72].copy_from_slice(&record.fingerprint);
  value[96..96 + record.dependency_id.len()].copy_from_slice(record.dependency_id.as_bytes());
  value[96 + record.dependency_id.len()..].copy_from_slice(record.version.as_bytes());
  Ok(value)
}

fn decode_table(value: &[u8]) -> Result<usize, &'static str> {
  if value.len() < TABLE_HEADER_LENGTH || value.len() > TABLE_MAX_LENGTH {
    return Err("dependency_table_length");
  }
  if &value[0..4] != b"ADPT" || read_u16(value, 4)? != 1 || read_u16(value, 6)? as usize != TABLE_HEADER_LENGTH {
    return Err("dependency_table_envelope");
  }
  if read_u32(value, 8)? as usize != value.len()
    || read_u32(value, 12)? != 0
    || read_u32(value, 20)? as usize != value.len() - TABLE_HEADER_LENGTH
    || value[24..32].iter().any(|byte| *byte != 0)
  {
    return Err("dependency_table_metadata");
  }
  let count = read_u32(value, 16)? as usize;
  if count > MAX_RECORDS {
    return Err("dependency_record_count");
  }
  let mut cursor = TABLE_HEADER_LENGTH;
  let mut previous: Option<DependencyRecord> = None;
  for _ in 0..count {
    let (record, next) = decode_record(value, cursor)?;
    if previous.as_ref().is_some_and(|previous| compare_records(previous, &record) != Ordering::Less) {
      return Err("dependency_record_order");
    }
    previous = Some(record);
    cursor = next;
  }
  if cursor != value.len() {
    return Err("dependency_record_count_mismatch");
  }
  Ok(count)
}

fn decode_record(value: &[u8], start: usize) -> Result<(DependencyRecord, usize), &'static str> {
  let header_end = start.checked_add(RECORD_HEADER_LENGTH).ok_or("dependency_length_overflow")?;
  if header_end > value.len() {
    return Err("dependency_record_truncated");
  }
  let total_length = read_u32(value, start)? as usize;
  let end = start.checked_add(total_length).ok_or("dependency_length_overflow")?;
  let id_length = read_u32(value, start + 20)? as usize;
  let version_length = read_u32(value, start + 24)? as usize;
  if total_length != RECORD_HEADER_LENGTH + id_length + version_length
    || end > value.len()
    || id_length == 0
    || id_length > 4_096
    || version_length > 256
  {
    return Err("dependency_record_length");
  }
  if read_u32(value, start + 28)? != 0 || value[start + 72..header_end].iter().any(|byte| *byte != 0) {
    return Err("dependency_record_reserved");
  }
  let id_end = header_end + id_length;
  let dependency_id = std::str::from_utf8(&value[header_end..id_end]).map_err(|_| "dependency_id_utf8")?.to_string();
  let version = std::str::from_utf8(&value[id_end..end]).map_err(|_| "dependency_version_utf8")?.to_string();
  let mut fingerprint = [0u8; 32];
  fingerprint.copy_from_slice(&value[start + 40..start + 72]);
  let record = DependencyRecord {
    kind: read_u16(value, start + 4)?,
    role: read_u16(value, start + 6)?,
    flags: read_u32(value, start + 8)?,
    abi: read_u16(value, start + 12)?,
    executor_profile: read_u16(value, start + 14)?,
    fingerprint_semantics: read_u16(value, start + 16)?,
    artifact_kind: read_u16(value, start + 18)?,
    artifact_length: read_u64(value, start + 32)?,
    fingerprint,
    dependency_id,
    version,
  };
  validate_record_fields(&record)?;
  Ok((record, end))
}

fn validate_record_fields(record: &DependencyRecord) -> Result<(), &'static str> {
  if record.flags & !0x07 != 0 || record.fingerprint.iter().all(|byte| *byte == 0) {
    return Err("dependency_flags_or_fingerprint");
  }
  let version_absent = record.flags & 0x01 != 0;
  let opaque_id = record.flags & 0x02 != 0;
  let artifact_required = record.flags & 0x04 != 0;
  if version_absent != record.version.is_empty() || (!version_absent && !is_canonical_semver(&record.version)) {
    return Err("dependency_version");
  }
  if !opaque_id && !is_canonical_dependency_id(&record.dependency_id) {
    return Err("dependency_id");
  }
  match record.kind {
    1 => {
      let corrected = matches!((record.role, record.abi, record.executor_profile), (1, 3, 2) | (2, 4, 2));
      let legacy = matches!((record.role, record.abi, record.executor_profile), (1, 1, 3) | (2, 2, 3));
      if (!corrected && !legacy)
        || record.fingerprint_semantics != 1
        || record.artifact_kind != 1
        || record.artifact_length == 0
        || !artifact_required
      {
        return Err("dependency_wasm_contract");
      }
    }
    2 => {
      if !matches!(record.role, 1 | 3 | 4)
        || record.abi != 0
        || record.executor_profile != 1
        || record.fingerprint_semantics != 2
        || record.artifact_kind != 0
        || record.artifact_length != 0
        || record.flags != 0
      {
        return Err("dependency_native_contract");
      }
    }
    _ => return Err("dependency_kind"),
  }
  Ok(())
}

fn compare_records(left: &DependencyRecord, right: &DependencyRecord) -> Ordering {
  (
    left.kind,
    left.role,
    left.dependency_id.as_bytes(),
    u8::from(!left.version.is_empty()),
    left.version.as_bytes(),
    left.abi,
    left.executor_profile,
    left.fingerprint_semantics,
    left.fingerprint,
    left.artifact_kind,
    left.artifact_length,
    left.flags,
  )
    .cmp(&(
      right.kind,
      right.role,
      right.dependency_id.as_bytes(),
      u8::from(!right.version.is_empty()),
      right.version.as_bytes(),
      right.abi,
      right.executor_profile,
      right.fingerprint_semantics,
      right.fingerprint,
      right.artifact_kind,
      right.artifact_length,
      right.flags,
    ))
}

fn is_canonical_dependency_id(value: &str) -> bool {
  value.starts_with('/')
    && value.len() <= 4_096
    && value.split('/').skip(1).all(|segment| {
      !segment.is_empty() && !matches!(segment, "." | "..") && !segment.chars().any(|character| character == '\0' || character.is_control())
    })
}

fn is_canonical_semver(value: &str) -> bool {
  let core = value.split_once(['-', '+']).map_or(value, |(core, _)| core);
  let mut parts = core.split('.');
  let valid_number =
    |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) && (part == "0" || !part.starts_with('0'));
  valid_number(parts.next().unwrap_or(""))
    && valid_number(parts.next().unwrap_or(""))
    && valid_number(parts.next().unwrap_or(""))
    && parts.next().is_none()
    && !value.ends_with(['-', '+', '.'])
}

fn digest32(value: &[u8]) -> [u8; 32] {
  *blake3::hash(value).as_bytes()
}

fn fixture_id(profile: HashProfile, suffix: &str) -> &'static str {
  match (profile, suffix) {
    (HashProfile::Blake3_256, "empty") => "adpt-blake3-256-empty-valid",
    (HashProfile::Blake3_256, "native-parser-resolution") => "adpt-blake3-256-native-parser-resolution-valid",
    (HashProfile::Blake3_256, "wasm-mapper") => "adpt-blake3-256-wasm-mapper-valid",
    (HashProfile::Sha512, "empty") => "adpt-sha512-empty-valid",
    (HashProfile::Sha512, "native-parser-resolution") => "adpt-sha512-native-parser-resolution-valid",
    (HashProfile::Sha512, "wasm-mapper") => "adpt-sha512-wasm-mapper-valid",
    _ => unreachable!("fixture suffixes are fixed"),
  }
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
  Ok(u16::from_le_bytes(bytes.get(offset..offset + 2).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  Ok(u32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  Ok(u64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn dependency_fixtures_match_expected_counts() {
    for case in fixture_cases() {
      assert_eq!(observe(case.profile, &case.bytes).0, case.expected, "fixture {}", case.id);
    }
  }

  #[test]
  fn dependency_order_and_duplicates_fail_closed() {
    let native = native_record();
    let wasm = wasm_record();
    assert!(build_table(&[wasm.clone(), native.clone()]).is_ok());
    assert_eq!(build_table(&[native.clone(), wasm.clone()]).err(), Some("dependency_record_order"));
    assert_eq!(build_table(&[wasm.clone(), wasm]).err(), Some("dependency_record_order"));
  }

  #[test]
  fn malformed_dependency_contexts_are_rejected() {
    let mut wasm = wasm_record();
    wasm.flags = 0;
    assert_eq!(build_record(&wasm).err(), Some("dependency_wasm_contract"));
    let mut native = native_record();
    native.artifact_length = 1;
    assert_eq!(build_record(&native).err(), Some("dependency_native_contract"));
    native = native_record();
    native.version = "01.0.0".to_string();
    assert_eq!(build_record(&native).err(), Some("dependency_version"));
    native = native_record();
    native.dependency_id = "/bad//id".to_string();
    assert_eq!(build_record(&native).err(), Some("dependency_id"));
  }

  #[test]
  fn table_decoder_rejects_counts_lengths_reserve_and_trailing_records() {
    let empty = build_table(&[]).unwrap();
    let mut count = empty.clone();
    put_u32(&mut count, 16, 1);
    assert!(decode_table(&count).is_err());
    let mut reserve = empty.clone();
    reserve[24] = 1;
    assert_eq!(decode_table(&reserve).err(), Some("dependency_table_metadata"));
    let mut trailing = empty;
    trailing.push(0);
    assert_eq!(decode_table(&trailing).err(), Some("dependency_table_metadata"));
  }
}
