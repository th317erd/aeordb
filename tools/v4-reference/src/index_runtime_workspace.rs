use crate::core::HashProfile;

const MANIFEST_LENGTH: usize = 208;
const MANIFEST_BODY_LENGTH: usize = 204;
const OBJECT_HEADER_LENGTH: usize = 184;
const OBJECT_MAX_LENGTH: usize = 512 * 1_024 * 1_024;

#[derive(Clone, Copy)]
pub enum IndexRuntimeWorkspaceFormat {
  ManifestV1,
  ObjectV1,
}

impl IndexRuntimeWorkspaceFormat {
  pub fn id(self) -> &'static str {
    match self {
      Self::ManifestV1 => "index-runtime-workspace-manifest-v1",
      Self::ObjectV1 => "index-runtime-workspace-object-v1",
    }
  }

  pub fn family(self) -> &'static str {
    match self {
      Self::ManifestV1 => "IndexRuntimeWorkspaceManifestV1",
      Self::ObjectV1 => "IndexRuntimeWorkspaceObjectV1",
    }
  }
}

#[derive(Clone)]
pub struct IndexRuntimeWorkspaceFixtureCase {
  pub id: &'static str,
  pub format: IndexRuntimeWorkspaceFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

pub fn fixture_cases() -> Vec<IndexRuntimeWorkspaceFixtureCase> {
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let runtime_payload = build_runtime_batch_payload(profile);
    let runtime_v2_payload = build_runtime_batch_payload_v2(profile);
    let task_payload = build_producer_task_payload(profile);
    let runtime_object = build_object(profile, 1, 9, 2, 40, 41, &runtime_payload);
    let runtime_v2_object = build_object(profile, 1, 11, 5, 40, 41, &runtime_v2_payload);
    let task_object = build_object(profile, 2, 10, 1, 42, 42, &task_payload);
    let manifest = build_manifest(1, &runtime_object);
    let profile_name = profile.label();

    cases.push(IndexRuntimeWorkspaceFixtureCase {
      id: leak(format!("aiwm-{profile_name}-runtime-head-valid")),
      format: IndexRuntimeWorkspaceFormat::ManifestV1,
      profile,
      expected: "index-workspace:manifest:sequence=1:objects=1",
      relation: Some("selects:runtime-batch-object"),
      canonical_key: None,
      bytes: manifest,
    });
    cases.push(IndexRuntimeWorkspaceFixtureCase {
      id: leak(format!("aiwo-{profile_name}-runtime-batch-valid")),
      format: IndexRuntimeWorkspaceFormat::ObjectV1,
      profile,
      expected: "index-workspace:object:runtime-batch:sequence=9:records=2:publications=40..41",
      relation: Some("listed-by:index-runtime-workspace-manifest-v1"),
      canonical_key: None,
      bytes: runtime_object,
    });
    cases.push(IndexRuntimeWorkspaceFixtureCase {
      id: leak(format!("aiwo-{profile_name}-runtime-batch-v2-valid")),
      format: IndexRuntimeWorkspaceFormat::ObjectV1,
      profile,
      expected: "index-workspace:object:runtime-batch:sequence=11:records=5:publications=40..41",
      relation: Some("successor-payload:index-runtime-workspace-object-v1"),
      canonical_key: None,
      bytes: runtime_v2_object,
    });
    cases.push(IndexRuntimeWorkspaceFixtureCase {
      id: leak(format!("aiwo-{profile_name}-producer-task-valid")),
      format: IndexRuntimeWorkspaceFormat::ObjectV1,
      profile,
      expected: "index-workspace:object:producer-task:sequence=10:records=1:publications=42..42",
      relation: Some("listed-by:index-runtime-workspace-manifest-v1"),
      canonical_key: None,
      bytes: task_object,
    });
  }
  cases
}

pub fn observe(format: IndexRuntimeWorkspaceFormat, profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  let observed = match format {
    IndexRuntimeWorkspaceFormat::ManifestV1 => decode_manifest(bytes),
    IndexRuntimeWorkspaceFormat::ObjectV1 => decode_object(profile, bytes),
  };
  (observed.unwrap_or_else(|error| format!("error:{error}")), None)
}

pub fn annotation_lines(format: IndexRuntimeWorkspaceFormat, bytes: &[u8]) -> Vec<String> {
  match format {
    IndexRuntimeWorkspaceFormat::ManifestV1 => vec![
      "manifest +0x000 len 16: AIWM framing and zero flags".to_string(),
      "manifest +0x010 len 64: database/destination/workspace/runtime identities".to_string(),
      "manifest +0x050 len 40: sequence and previous-manifest BLAKE3 digest".to_string(),
      "manifest +0x078 len 52: object kind/id/BLAKE3 digest".to_string(),
      "manifest +0x0ac len 32: object/cumulative byte and count closure".to_string(),
      "manifest +0x0cc len 4: CRC-32/ISO-HDLC".to_string(),
    ],
    IndexRuntimeWorkspaceFormat::ObjectV1 => vec![
      "object +0x000 len 24: AIWO framing, kind, database hash algorithm, zero flags".to_string(),
      "object +0x018 len 80: database/destination/workspace/runtime/object identities".to_string(),
      "object +0x068 len 48: sequence/time/payload/count/publication range".to_string(),
      "object +0x098 len 32: payload BLAKE3 digest".to_string(),
      format!("object +0x0b8 len {}: exact payload", bytes.len().saturating_sub(OBJECT_HEADER_LENGTH + 4)),
      format!("object +0x{:03x} len 4: CRC-32/ISO-HDLC", bytes.len().saturating_sub(4)),
    ],
  }
}

fn build_manifest(sequence: u64, object: &[u8]) -> Vec<u8> {
  let mut bytes = vec![0u8; MANIFEST_LENGTH];
  bytes[..4].copy_from_slice(b"AIWM");
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, MANIFEST_BODY_LENGTH as u16);
  put_u32(&mut bytes, 8, MANIFEST_LENGTH as u32);
  fill(&mut bytes[16..32], 0x11);
  fill(&mut bytes[32..48], 0x22);
  fill(&mut bytes[48..64], 0x33);
  fill(&mut bytes[64..80], 0x44);
  put_u64(&mut bytes, 80, sequence);
  put_u16(&mut bytes, 120, 1);
  fill(&mut bytes[124..140], 0x55);
  bytes[140..172].copy_from_slice(blake3::hash(object).as_bytes());
  put_u64(&mut bytes, 172, object.len() as u64);
  put_u64(&mut bytes, 180, 1);
  put_u64(&mut bytes, 188, object.len() as u64);
  put_u64(&mut bytes, 196, 1_725_000_000_123);
  let checksum = crc32fast::hash(&bytes[..MANIFEST_BODY_LENGTH]);
  put_u32(&mut bytes, MANIFEST_BODY_LENGTH, checksum);
  bytes
}

fn build_object(
  profile: HashProfile,
  kind: u16,
  sequence: u64,
  records: u64,
  minimum_publication: u64,
  maximum_publication: u64,
  payload: &[u8],
) -> Vec<u8> {
  let payload_end = OBJECT_HEADER_LENGTH + payload.len();
  let mut bytes = vec![0u8; payload_end + 4];
  bytes[..4].copy_from_slice(b"AIWO");
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, kind);
  put_u16(&mut bytes, 8, OBJECT_HEADER_LENGTH as u16);
  put_u16(&mut bytes, 10, profile.algorithm_id());
  put_u64(&mut bytes, 12, (payload_end + 4) as u64);
  fill(&mut bytes[24..40], 0x11);
  fill(&mut bytes[40..56], 0x22);
  fill(&mut bytes[56..72], 0x33);
  fill(&mut bytes[72..88], 0x44);
  fill(&mut bytes[88..104], 0x55);
  put_u64(&mut bytes, 104, sequence);
  put_u64(&mut bytes, 112, 1_725_000_000_123);
  put_u64(&mut bytes, 120, payload.len() as u64);
  put_u64(&mut bytes, 128, records);
  put_u64(&mut bytes, 136, minimum_publication);
  put_u64(&mut bytes, 144, maximum_publication);
  bytes[152..184].copy_from_slice(blake3::hash(payload).as_bytes());
  bytes[OBJECT_HEADER_LENGTH..payload_end].copy_from_slice(payload);
  let checksum = crc32fast::hash(&bytes[..payload_end]);
  put_u32(&mut bytes, payload_end, checksum);
  bytes
}

fn build_runtime_batch_payload(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let record_length = 12 + h;
  let frame_length = 40 + 3 * h + 12;
  let mut bytes = vec![0u8; 64 + 2 * frame_length];
  bytes[..4].copy_from_slice(b"AIRB");
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, 64);
  let total_length = bytes.len() as u64;
  put_u64(&mut bytes, 8, total_length);
  fill(&mut bytes[16..32], 0x77);
  put_u64(&mut bytes, 32, 1);
  put_u16(&mut bytes, 40, 4);
  put_u32(&mut bytes, 44, 2);

  for (ordinal, (index_byte, file_byte, operation_byte, publication_sequence)) in
    [(0usize, (0x21, 0x31, 0x11, 40u64)), (1, (0x22, 0x32, 0x12, 41))]
  {
    let start = 64 + ordinal * frame_length;
    put_u32(&mut bytes, start, frame_length as u32);
    bytes[start + 4] = 2;
    put_u16(&mut bytes, start + 6, h as u16);
    put_u64(&mut bytes, start + 8, publication_sequence);
    fill(&mut bytes[start + 16..start + 32], operation_byte);
    put_u32(&mut bytes, start + 32, h as u32);
    put_u32(&mut bytes, start + 36, record_length as u32);
    let index_start = start + 40;
    let order_start = index_start + h;
    let record_start = order_start + h;
    fill(&mut bytes[index_start..order_start], index_byte);
    fill(&mut bytes[order_start..record_start], file_byte);
    put_u64(&mut bytes, record_start + 4, ordinal as u64 + 3);
    fill(&mut bytes[record_start + 12..record_start + record_length], file_byte);
  }
  bytes
}

fn build_runtime_batch_payload_v2(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let reverse_record_length = 12 + h;
  let mutation_frame_length = 40 + 3 * h + 12;
  let transition_frame_length = 48 + h;
  let mutation_count = 2usize;
  let transition_count = 3usize;
  let transitions_start = 64 + mutation_count * mutation_frame_length;
  let mut bytes = vec![0u8; transitions_start + transition_count * transition_frame_length];
  bytes[..4].copy_from_slice(b"AIRB");
  put_u16(&mut bytes, 4, 2);
  put_u16(&mut bytes, 6, 64);
  let total_length = bytes.len() as u64;
  put_u64(&mut bytes, 8, total_length);
  fill(&mut bytes[16..32], 0x77);
  put_u64(&mut bytes, 32, 2);
  put_u16(&mut bytes, 40, 4);
  put_u32(&mut bytes, 44, mutation_count as u32);
  put_u32(&mut bytes, 48, transition_count as u32);

  for (ordinal, (file_byte, operation_byte, publication_sequence, operation_kind)) in
    [(0usize, (0x31, 0x11, 40u64, 2u8)), (1, (0x32, 0x12, 41, 1))]
  {
    let start = 64 + ordinal * mutation_frame_length;
    put_u32(&mut bytes, start, mutation_frame_length as u32);
    bytes[start + 4] = 2;
    bytes[start + 5] = operation_kind;
    put_u16(&mut bytes, start + 6, h as u16);
    put_u64(&mut bytes, start + 8, publication_sequence);
    fill(&mut bytes[start + 16..start + 32], operation_byte);
    put_u32(&mut bytes, start + 32, h as u32);
    put_u32(&mut bytes, start + 36, reverse_record_length as u32);
    let index_start = start + 40;
    let order_start = index_start + h;
    let record_start = order_start + h;
    fill(&mut bytes[index_start..order_start], 0x21);
    fill(&mut bytes[order_start..record_start], file_byte);
    put_u64(&mut bytes, record_start + 4, 3);
    fill(&mut bytes[record_start + 12..record_start + reverse_record_length], file_byte);
  }

  for (ordinal, (owner_byte, owner_class, flags, operation_byte)) in
    [(0usize, (0x21, 1u8, 0b0011u8, 0x12)), (1, (0x22, 2, 0b0010, 0x22)), (2, (0x23, 3, 0b0110, 0x23))]
  {
    let start = transitions_start + ordinal * transition_frame_length;
    put_u32(&mut bytes, start, transition_frame_length as u32);
    bytes[start + 4] = owner_class;
    bytes[start + 5] = flags;
    put_u16(&mut bytes, start + 6, h as u16);
    put_u64(&mut bytes, start + 8, 41);
    fill(&mut bytes[start + 16..start + 32], operation_byte);
    put_u64(&mut bytes, start + 32, 3);
    fill(&mut bytes[start + 48..start + 48 + h], owner_byte);
  }
  bytes
}

fn build_producer_task_payload(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let header_length = 56 + 4 * h;
  let mut bytes = vec![0u8; header_length];
  bytes[..4].copy_from_slice(b"AITK");
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, header_length as u16);
  put_u64(&mut bytes, 8, header_length as u64);
  fill(&mut bytes[16..32], 0x71);
  put_u16(&mut bytes, 32, 1);
  put_u16(&mut bytes, 34, 1);
  put_u64(&mut bytes, 36, 42);
  put_u16(&mut bytes, 44, h as u16);
  for (ordinal, value) in [0x61, 0x62, 0x63, 0x64].into_iter().enumerate() {
    let start = 56 + ordinal * h;
    fill(&mut bytes[start..start + h], value);
  }
  bytes
}

fn decode_manifest(bytes: &[u8]) -> Result<String, &'static str> {
  if bytes.len() != MANIFEST_LENGTH || &bytes[..4] != b"AIWM" || read_u16(bytes, 4)? != 1 {
    return Err("manifest_framing");
  }
  if read_u16(bytes, 6)? as usize != MANIFEST_BODY_LENGTH
    || read_u32(bytes, 8)? as usize != MANIFEST_LENGTH
    || bytes[12..16].iter().any(|byte| *byte != 0)
    || bytes[122..124].iter().any(|byte| *byte != 0)
    || read_u32(bytes, MANIFEST_BODY_LENGTH)? != crc32fast::hash(&bytes[..MANIFEST_BODY_LENGTH])
  {
    return Err("manifest_integrity");
  }
  let sequence = read_u64(bytes, 80)?;
  let count = read_u64(bytes, 180)?;
  if bytes[16..80].chunks(16).any(all_zero)
    || sequence == 0
    || all_zero(&bytes[140..172])
    || read_u64(bytes, 172)? == 0
    || count == 0
    || read_u64(bytes, 188)? < read_u64(bytes, 172)?
    || read_u64(bytes, 196)? == 0
    || !matches!(read_u16(bytes, 120)?, 1 | 2)
    || all_zero(&bytes[124..140])
    || (all_zero(&bytes[88..120]) != (sequence == 1))
  {
    return Err("manifest_fields");
  }
  Ok(format!("index-workspace:manifest:sequence={sequence}:objects={count}"))
}

fn decode_object(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  if bytes.len() < OBJECT_HEADER_LENGTH + 5 || bytes.len() > OBJECT_MAX_LENGTH || &bytes[..4] != b"AIWO" {
    return Err("object_framing");
  }
  let kind = read_u16(bytes, 6)?;
  let payload_length = usize::try_from(read_u64(bytes, 120)?).map_err(|_| "object_length")?;
  let payload_end = OBJECT_HEADER_LENGTH.checked_add(payload_length).ok_or("object_length")?;
  if read_u16(bytes, 4)? != 1
    || !matches!(kind, 1 | 2)
    || read_u16(bytes, 8)? as usize != OBJECT_HEADER_LENGTH
    || read_u16(bytes, 10)? != profile.algorithm_id()
    || read_u64(bytes, 12)? as usize != bytes.len()
    || bytes[20..24].iter().any(|byte| *byte != 0)
    || payload_end.checked_add(4) != Some(bytes.len())
    || read_u32(bytes, payload_end)? != crc32fast::hash(&bytes[..payload_end])
  {
    return Err("object_integrity");
  }
  let payload = &bytes[OBJECT_HEADER_LENGTH..payload_end];
  let sequence = read_u64(bytes, 104)?;
  let records = read_u64(bytes, 128)?;
  let minimum = read_u64(bytes, 136)?;
  let maximum = read_u64(bytes, 144)?;
  if bytes[24..104].chunks(16).any(all_zero)
    || sequence == 0
    || read_u64(bytes, 112)? == 0
    || payload.is_empty()
    || records == 0
    || minimum == 0
    || maximum < minimum
    || blake3::hash(payload).as_bytes() != &bytes[152..184]
  {
    return Err("object_fields");
  }
  let (payload_records, payload_minimum, payload_maximum) =
    if kind == 1 { decode_runtime_batch_payload(profile, payload)? } else { decode_producer_task_payload(profile, payload)? };
  if records != payload_records || minimum != payload_minimum || maximum != payload_maximum {
    return Err("object_payload_closure");
  }
  let kind_name = if kind == 1 { "runtime-batch" } else { "producer-task" };
  Ok(format!("index-workspace:object:{kind_name}:sequence={sequence}:records={records}:publications={minimum}..{maximum}"))
}

fn decode_runtime_batch_payload(profile: HashProfile, bytes: &[u8]) -> Result<(u64, u64, u64), &'static str> {
  if bytes.len() < 64 || &bytes[..4] != b"AIRB" || read_u16(bytes, 6)? != 64 {
    return Err("runtime_payload_framing");
  }
  match read_u16(bytes, 4)? {
    1 => decode_runtime_batch_payload_v1(profile, bytes),
    2 => decode_runtime_batch_payload_v2(profile, bytes),
    _ => Err("runtime_payload_version"),
  }
}

fn decode_runtime_batch_payload_v1(profile: HashProfile, bytes: &[u8]) -> Result<(u64, u64, u64), &'static str> {
  if bytes.len() < 104 {
    return Err("runtime_payload_framing");
  }
  let count = read_u32(bytes, 44)? as usize;
  if read_u64(bytes, 8)? as usize != bytes.len()
    || all_zero(&bytes[16..32])
    || read_u64(bytes, 32)? == 0
    || !(1..=5).contains(&read_u16(bytes, 40)?)
    || bytes[42..44].iter().chain(bytes[48..64].iter()).any(|byte| *byte != 0)
    || count == 0
  {
    return Err("runtime_payload_header");
  }
  let h = profile.width();
  let mut cursor = 64usize;
  let mut previous_key: Option<Vec<u8>> = None;
  let mut minimum = u64::MAX;
  let mut maximum = 0u64;
  for _ in 0..count {
    if cursor.checked_add(40).is_none_or(|fixed_end| fixed_end > bytes.len()) {
      return Err("runtime_frame_truncated");
    }
    let frame_length = read_u32(bytes, cursor)? as usize;
    let end = cursor.checked_add(frame_length).ok_or("runtime_frame_overflow")?;
    let index_length = read_u16(bytes, cursor + 6)? as usize;
    let order_length = read_u32(bytes, cursor + 32)? as usize;
    let record_length = read_u32(bytes, cursor + 36)? as usize;
    if end > bytes.len()
      || bytes[cursor + 4] != 2
      || bytes[cursor + 5] != 0
      || index_length != h
      || order_length != h
      || record_length != 12 + h
      || frame_length != 40 + index_length + order_length + record_length
      || read_u64(bytes, cursor + 8)? == 0
      || all_zero(&bytes[cursor + 16..cursor + 32])
    {
      return Err("runtime_frame_fields");
    }
    let index_start = cursor + 40;
    let order_start = index_start + h;
    let record_start = order_start + h;
    let index_id = &bytes[index_start..order_start];
    let order_key = &bytes[order_start..record_start];
    let record = &bytes[record_start..end];
    if all_zero(index_id)
      || all_zero(order_key)
      || record[..4].iter().any(|byte| *byte != 0)
      || read_u64(record, 4)? == 0
      || &record[12..] != order_key
    {
      return Err("runtime_frame_record");
    }
    let mut key = Vec::with_capacity(1 + 2 * h);
    key.extend_from_slice(index_id);
    key.push(2);
    key.extend_from_slice(order_key);
    if previous_key.as_ref().is_some_and(|previous| previous.as_slice() >= key.as_slice()) {
      return Err("runtime_frame_order");
    }
    previous_key = Some(key);
    let publication = read_u64(bytes, cursor + 8)?;
    minimum = minimum.min(publication);
    maximum = maximum.max(publication);
    cursor = end;
  }
  if cursor != bytes.len() {
    return Err("runtime_payload_trailing");
  }
  Ok((count as u64, minimum, maximum))
}

fn decode_runtime_batch_payload_v2(profile: HashProfile, bytes: &[u8]) -> Result<(u64, u64, u64), &'static str> {
  let mutation_count = read_u32(bytes, 44)? as usize;
  let transition_count = read_u32(bytes, 48)? as usize;
  if read_u64(bytes, 8)? as usize != bytes.len()
    || all_zero(&bytes[16..32])
    || read_u64(bytes, 32)? == 0
    || !(1..=5).contains(&read_u16(bytes, 40)?)
    || bytes[42..44].iter().chain(bytes[52..64].iter()).any(|byte| *byte != 0)
    || transition_count == 0
    || mutation_count.checked_add(transition_count).is_none_or(|count| count > 1_048_576)
  {
    return Err("runtime_v2_payload_header");
  }
  let h = profile.width();
  let minimum_mutation_bytes = mutation_count.checked_mul(40 + h + 2).ok_or("runtime_v2_mutation_count_overflow")?;
  let minimum_transition_bytes = transition_count.checked_mul(48 + h).ok_or("runtime_v2_transition_count_overflow")?;
  if minimum_mutation_bytes.checked_add(minimum_transition_bytes).is_none_or(|minimum| minimum > bytes.len() - 64) {
    return Err("runtime_v2_count_amplification");
  }
  let mut cursor = 64usize;
  let mut previous_mutation_key: Option<Vec<u8>> = None;
  let mut minimum = u64::MAX;
  let mut maximum = 0u64;
  let mut mutations = Vec::with_capacity(mutation_count);
  for _ in 0..mutation_count {
    if cursor.checked_add(40).is_none_or(|fixed_end| fixed_end > bytes.len()) {
      return Err("runtime_v2_mutation_truncated");
    }
    let frame_length = read_u32(bytes, cursor)? as usize;
    let end = cursor.checked_add(frame_length).ok_or("runtime_v2_mutation_overflow")?;
    let operation_kind = bytes[cursor + 5];
    let index_length = read_u16(bytes, cursor + 6)? as usize;
    let order_length = read_u32(bytes, cursor + 32)? as usize;
    let record_length = read_u32(bytes, cursor + 36)? as usize;
    if end > bytes.len()
      || bytes[cursor + 4] != 2
      || !matches!(operation_kind, 1 | 2)
      || index_length != h
      || order_length != h
      || record_length != 12 + h
      || frame_length != 40 + index_length + order_length + record_length
      || read_u64(bytes, cursor + 8)? == 0
      || all_zero(&bytes[cursor + 16..cursor + 32])
    {
      return Err("runtime_v2_mutation_fields");
    }
    let index_start = cursor + 40;
    let order_start = index_start + h;
    let record_start = order_start + h;
    let index_id = &bytes[index_start..order_start];
    let order_key = &bytes[order_start..record_start];
    let record = &bytes[record_start..end];
    let document_ordinal = read_u64(record, 4)?;
    if all_zero(index_id)
      || all_zero(order_key)
      || record[..4].iter().any(|byte| *byte != 0)
      || document_ordinal == 0
      || &record[12..] != order_key
    {
      return Err("runtime_v2_mutation_record");
    }
    let mut key = Vec::with_capacity(1 + 2 * h);
    key.extend_from_slice(index_id);
    key.push(bytes[cursor + 4]);
    key.extend_from_slice(order_key);
    if previous_mutation_key.as_ref().is_some_and(|previous| previous.as_slice() >= key.as_slice()) {
      return Err("runtime_v2_mutation_order");
    }
    previous_mutation_key = Some(key);
    let publication = read_u64(bytes, cursor + 8)?;
    minimum = minimum.min(publication);
    maximum = maximum.max(publication);
    mutations.push((index_id.to_vec(), document_ordinal, publication));
    cursor = end;
  }

  let mut previous_transition_key: Option<Vec<u8>> = None;
  let mut transitions = Vec::with_capacity(transition_count);
  for _ in 0..transition_count {
    if cursor.checked_add(48).is_none_or(|fixed_end| fixed_end > bytes.len()) {
      return Err("runtime_v2_transition_truncated");
    }
    let frame_length = read_u32(bytes, cursor)? as usize;
    let end = cursor.checked_add(frame_length).ok_or("runtime_v2_transition_overflow")?;
    let owner_class = bytes[cursor + 4];
    let flags = bytes[cursor + 5];
    let owner_length = read_u16(bytes, cursor + 6)? as usize;
    let publication = read_u64(bytes, cursor + 8)?;
    let document_ordinal = read_u64(bytes, cursor + 32)?;
    let owner_start = cursor + 48;
    if end > bytes.len()
      || frame_length != 48 + h
      || !matches!(owner_class, 1..=3)
      || flags & !0x0f != 0
      || owner_length != h
      || publication == 0
      || all_zero(&bytes[cursor + 16..cursor + 32])
      || document_ordinal == 0
      || bytes[cursor + 40..cursor + 48].iter().any(|byte| *byte != 0)
    {
      return Err("runtime_v2_transition_fields");
    }
    let before_live = flags & 1 != 0;
    let after_live = flags & 2 != 0;
    let before_unindexable = flags & 4 != 0;
    let after_unindexable = flags & 8 != 0;
    if (before_live && before_unindexable)
      || (after_live && after_unindexable)
      || (owner_class == 1 && (before_unindexable || after_unindexable))
    {
      return Err("runtime_v2_transition_state");
    }
    let owner_id = &bytes[owner_start..end];
    if all_zero(owner_id) {
      return Err("runtime_v2_transition_owner");
    }
    let mut key = Vec::with_capacity(h + 8);
    key.extend_from_slice(owner_id);
    key.extend_from_slice(&document_ordinal.to_be_bytes());
    if previous_transition_key.as_ref().is_some_and(|previous| previous.as_slice() >= key.as_slice()) {
      return Err("runtime_v2_transition_order");
    }
    previous_transition_key = Some(key);
    minimum = minimum.min(publication);
    maximum = maximum.max(publication);
    transitions.push((owner_id.to_vec(), owner_class, document_ordinal, publication));
    cursor = end;
  }
  if cursor != bytes.len() {
    return Err("runtime_v2_payload_trailing");
  }
  for (owner_id, document_ordinal, publication) in mutations {
    let Some((_, owner_class, _, transition_publication)) = transitions
      .iter()
      .find(|(transition_owner, _, transition_ordinal, _)| transition_owner == &owner_id && *transition_ordinal == document_ordinal)
    else {
      return Err("runtime_v2_mutation_transition_missing");
    };
    if *owner_class != 1 || publication > *transition_publication {
      return Err("runtime_v2_mutation_transition_sequence");
    }
  }
  Ok(((mutation_count + transition_count) as u64, minimum, maximum))
}

fn decode_producer_task_payload(profile: HashProfile, bytes: &[u8]) -> Result<(u64, u64, u64), &'static str> {
  let h = profile.width();
  let header_length = 56 + 4 * h;
  if bytes.len() < header_length
    || &bytes[..4] != b"AITK"
    || read_u16(bytes, 4)? != 1
    || read_u16(bytes, 6)? as usize != header_length
    || read_u64(bytes, 8)? as usize != bytes.len()
    || all_zero(&bytes[16..32])
    || !(1..=9).contains(&read_u16(bytes, 32)?)
    || !matches!(read_u16(bytes, 34)?, 1 | 2)
    || read_u64(bytes, 36)? == 0
    || read_u16(bytes, 44)? as usize != h
    || bytes[46..48].iter().chain(bytes[52..56].iter()).any(|byte| *byte != 0)
    || header_length.checked_add(read_u32(bytes, 48)? as usize) != Some(bytes.len())
  {
    return Err("producer_payload_header");
  }
  let kind = read_u16(bytes, 32)?;
  let flags = read_u16(bytes, 34)?;
  let before = &bytes[56..56 + h];
  let after = &bytes[56 + h..56 + 2 * h];
  let semantic = &bytes[56 + 2 * h..56 + 3 * h];
  let journal = &bytes[56 + 3 * h..header_length];
  let scope = &bytes[header_length..];
  if all_zero(before) || all_zero(after) || all_zero(semantic) {
    return Err("producer_payload_roots");
  }
  if matches!(kind, 1 | 2) {
    if flags != 1 || before == after || all_zero(journal) || !scope.is_empty() {
      return Err("producer_payload_journal_closure");
    }
  } else if flags != 2
    || before != after
    || !all_zero(journal)
    || scope.is_empty()
    || scope[0] != b'/'
    || std::str::from_utf8(scope).is_err()
  {
    return Err("producer_payload_scope_closure");
  }
  let publication = read_u64(bytes, 36)?;
  Ok((1, publication, publication))
}

fn fill(bytes: &mut [u8], value: u8) {
  bytes.fill(value);
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
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

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}
