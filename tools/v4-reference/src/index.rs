use crate::core::HashProfile;
use crate::{definitions, field_index, value_store};

const AIDX_HEADER_LENGTH: usize = 32;
const MAX_MANIFEST_LENGTH: usize = 1_048_576;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManifestKind {
  FieldIndex,
  FieldNvt,
  ScopeCatalog,
  ValueStore,
}

impl ManifestKind {
  fn id(self) -> u16 {
    match self {
      Self::FieldIndex => 0x0010,
      Self::FieldNvt => 0x0011,
      Self::ScopeCatalog => 0x0012,
      Self::ValueStore => 0x0013,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::FieldIndex => "field-index",
      Self::FieldNvt => "field-nvt",
      Self::ScopeCatalog => "scope-catalog",
      Self::ValueStore => "value-store",
    }
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      0x0010 => Some(Self::FieldIndex),
      0x0011 => Some(Self::FieldNvt),
      0x0012 => Some(Self::ScopeCatalog),
      0x0013 => Some(Self::ValueStore),
      _ => None,
    }
  }
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
  let mut cases = Vec::with_capacity(28);
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
    for (kind, populated, bytes) in manifest_fixture_graph(profile) {
      let decoded = decode_manifest(profile, &bytes).expect("fixture manifest must decode");
      cases.push(IndexFixtureCase {
        id: manifest_fixture_id(profile, kind, populated),
        format: IndexFormat::IndexArtifactV1,
        profile,
        expected: manifest_expected(kind, decoded.generation, populated),
        relation: Some(manifest_relation(kind, populated)),
        canonical_key: Some(hex::encode(decoded.key)),
        bytes,
      });
    }
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match read_u16(bytes, 6) {
    Ok(0x0001..=0x0003) => match decode_pointer(profile, bytes) {
      Ok(pointer) => (
        format!("index:pointer:{}:slot-{}:sequence={}", pointer.kind.name(), if pointer.slot == 0 { 'a' } else { 'b' }, pointer.sequence),
        Some(hex::encode(pointer.key)),
      ),
      Err(error) => (format!("error:{error}"), None),
    },
    Ok(0x0010..=0x0013) => match decode_manifest(profile, bytes) {
      Ok(manifest) => (
        format!(
          "index:manifest:{}:generation={}:roots={}",
          manifest.kind.name(),
          manifest.generation,
          if manifest.populated { "populated" } else { "empty" }
        ),
        Some(hex::encode(manifest.key)),
      ),
      Err(error) => (format!("error:{error}"), None),
    },
    Ok(_) => ("error:index_artifact_kind".to_string(), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let kind = read_u16(bytes, 6).unwrap_or(0);
  let h = profile.width();
  if ManifestKind::from_id(kind).is_some() {
    return vec![
      "envelope +0x000 len 32: AIDX common envelope".to_string(),
      format!("envelope artifact_kind: 0x{kind:04x}"),
      format!("identity +0x000 len {h}: semantic owner ID"),
      format!("identity +0x{h:03x} len 8: manifest generation"),
      format!("body +0x000 len {}: exact immutable manifest body", bytes.len().saturating_sub(44 + h)),
      format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
    ];
  }
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

fn manifest_fixture_graph(profile: HashProfile) -> Vec<(ManifestKind, bool, Vec<u8>)> {
  let scope_definition = definitions::sample_scope_definition();
  let scope_id = definitions::scope_id(profile, &scope_definition);
  let scope_empty = build_scope_manifest(profile, &scope_id, 0x1201, false, &scope_definition);
  let scope_populated = build_scope_manifest(profile, &scope_id, 0x1202, true, &scope_definition);

  let value_definition = value_store::sample_value_store_definition_for_scope(profile, &scope_id);
  let value_store_id = value_store::value_store_id_bytes(profile, &value_definition);
  let value_empty = build_value_manifest(
    profile,
    &value_store_id,
    0x1301,
    false,
    &immutable_key(profile, ManifestKind::ScopeCatalog.id(), &scope_empty),
    &value_definition,
  );
  let value_populated = build_value_manifest(
    profile,
    &value_store_id,
    0x1302,
    true,
    &immutable_key(profile, ManifestKind::ScopeCatalog.id(), &scope_populated),
    &value_definition,
  );

  let field_definition = field_index::sample_field_index_definition_for_value_store(profile, &value_store_id);
  let index_id = field_index::index_id(profile, &field_definition);
  let field_empty = build_field_manifest(
    profile,
    &index_id,
    0x1001,
    false,
    &immutable_key(profile, ManifestKind::ValueStore.id(), &value_empty),
    &field_definition,
  );
  let field_populated = build_field_manifest(
    profile,
    &index_id,
    0x1002,
    true,
    &immutable_key(profile, ManifestKind::ValueStore.id(), &value_populated),
    &field_definition,
  );
  let nvt_empty = build_nvt_manifest(profile, &index_id, 0x1101, false);
  let nvt_populated = build_nvt_manifest(profile, &index_id, 0x1102, true);

  vec![
    (ManifestKind::ScopeCatalog, false, scope_empty),
    (ManifestKind::ScopeCatalog, true, scope_populated),
    (ManifestKind::ValueStore, false, value_empty),
    (ManifestKind::ValueStore, true, value_populated),
    (ManifestKind::FieldIndex, false, field_empty),
    (ManifestKind::FieldIndex, true, field_populated),
    (ManifestKind::FieldNvt, false, nvt_empty),
    (ManifestKind::FieldNvt, true, nvt_populated),
  ]
}

fn build_scope_manifest(profile: HashProfile, owner: &[u8], generation: u64, populated: bool, definition: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let mut body = vec![0u8; 112 + 3 * h + definition.len()];
  write_correctness_prefix(&mut body, profile, definition.len(), populated);
  put_u16(&mut body, 64 + h, 1);
  put_u16(&mut body, 66 + h, 1);
  body[68 + h] = if populated { 0x03 } else { 0 };
  put_u64(&mut body, 72 + h, if populated { 43 } else { 1 });
  if populated {
    fill_sequence(&mut body[80 + h..80 + 2 * h], 0x31);
    fill_sequence(&mut body[80 + 2 * h..80 + 3 * h], 0x51);
    put_u64(&mut body, 80 + 3 * h, 40);
    put_u64(&mut body, 88 + 3 * h, 2);
    put_u64(&mut body, 96 + 3 * h, 3);
    put_u64(&mut body, 104 + 3 * h, 2);
  }
  body[112 + 3 * h..].copy_from_slice(definition);
  build_immutable_artifact(profile, ManifestKind::ScopeCatalog, owner, generation, &body)
}

fn build_value_manifest(
  profile: HashProfile,
  owner: &[u8],
  generation: u64,
  populated: bool,
  scope_manifest: &[u8],
  definition: &[u8],
) -> Vec<u8> {
  let h = profile.width();
  let mut body = vec![0u8; 144 + 4 * h + definition.len()];
  write_correctness_prefix(&mut body, profile, definition.len(), populated);
  put_u16(&mut body, 64 + h, 1);
  put_u16(&mut body, 66 + h, 1);
  put_u16(&mut body, 68 + h, 1);
  body[70 + h] = if populated { 0x03 } else { 0 };
  body[72 + h..72 + 2 * h].copy_from_slice(scope_manifest);
  if populated {
    fill_sequence(&mut body[72 + 2 * h..72 + 3 * h], 0x61);
    fill_sequence(&mut body[72 + 3 * h..72 + 4 * h], 0x71);
  }
  put_u64(&mut body, 72 + 4 * h, if populated { 12 } else { 1 });
  if populated {
    for (offset, value) in [(80, 4), (88, 2), (96, 30), (104, 2), (112, 55), (120, 5), (128, 1), (136, 8_192)] {
      put_u64(&mut body, offset + 4 * h, value);
    }
  }
  body[144 + 4 * h..].copy_from_slice(definition);
  build_immutable_artifact(profile, ManifestKind::ValueStore, owner, generation, &body)
}

fn build_field_manifest(
  profile: HashProfile,
  owner: &[u8],
  generation: u64,
  populated: bool,
  value_manifest: &[u8],
  definition: &[u8],
) -> Vec<u8> {
  let h = profile.width();
  let mut body = vec![0u8; 160 + 4 * h + definition.len()];
  write_correctness_prefix(&mut body, profile, definition.len(), populated);
  put_u16(&mut body, 64 + h, 1);
  put_u16(&mut body, 66 + h, 1);
  put_u16(&mut body, 68 + h, 1);
  body[70 + h] = if populated { 0x03 } else { 0 };
  body[72 + h..72 + 2 * h].copy_from_slice(value_manifest);
  if populated {
    fill_sequence(&mut body[72 + 2 * h..72 + 3 * h], 0x81);
    fill_sequence(&mut body[72 + 3 * h..72 + 4 * h], 0x91);
    put_u64(&mut body, 72 + 4 * h, 2);
    put_u64(&mut body, 80 + 4 * h, 9);
  }
  put_u64(&mut body, 88 + 4 * h, if populated { 20 } else { 1 });
  if populated {
    for (offset, value) in [(96, 8), (104, 2), (112, 1_024), (120, 64), (128, 30), (136, 2), (144, 1), (152, 32_768)] {
      put_u64(&mut body, offset + 4 * h, value);
    }
  }
  body[160 + 4 * h..].copy_from_slice(definition);
  build_immutable_artifact(profile, ManifestKind::FieldIndex, owner, generation, &body)
}

fn build_nvt_manifest(profile: HashProfile, owner: &[u8], generation: u64, populated: bool) -> Vec<u8> {
  let h = profile.width();
  let mut body = vec![0u8; 88 + 2 * h];
  write_capabilities(&mut body[4..36], &[7, 11]);
  put_u16(&mut body, 36, 1);
  put_u16(&mut body, 38, 1);
  put_u32(&mut body, 40, 4_096);
  body[44] = if populated { 1 } else { 0 };
  put_u64(&mut body, 48, 65_536);
  put_u64(&mut body, 56, 0x1002);
  fill_sequence(&mut body[64..64 + h], 0xa1);
  if populated {
    fill_sequence(&mut body[64 + h..64 + 2 * h], 0xb1);
    put_u64(&mut body, 64 + 2 * h, 4);
    put_u64(&mut body, 72 + 2 * h, 100);
    put_u64(&mut body, 80 + 2 * h, 1_024);
  }
  build_immutable_artifact(profile, ManifestKind::FieldNvt, owner, generation, &body)
}

fn write_correctness_prefix(body: &mut [u8], profile: HashProfile, definition_length: usize, populated: bool) {
  write_capabilities(&mut body[4..36], &[7, 8, 9, 10]);
  put_u32(body, 36, definition_length as u32);
  fill_sequence(&mut body[40..40 + profile.width()], if populated { 0xd1 } else { 0xc1 });
  fill_sequence(&mut body[40 + profile.width()..56 + profile.width()], if populated { 0xe1 } else { 0xe0 });
  put_u64(body, 56 + profile.width(), if populated { 99 } else { 0 });
}

fn write_capabilities(bytes: &mut [u8], bits: &[usize]) {
  for bit in bits {
    bytes[bit / 8] |= 1 << (bit % 8);
  }
}

fn build_immutable_artifact(profile: HashProfile, kind: ManifestKind, owner: &[u8], generation: u64, body: &[u8]) -> Vec<u8> {
  let h = profile.width();
  assert_eq!(owner.len(), h);
  let identity_length = h + 8;
  let total_length = AIDX_HEADER_LENGTH + identity_length + body.len() + 4;
  let mut value = vec![0u8; total_length];
  value[0..4].copy_from_slice(b"AIDX");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, kind.id());
  put_u16(&mut value, 8, AIDX_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, total_length as u32);
  put_u16(&mut value, 16, identity_length as u16);
  put_u32(&mut value, 20, body.len() as u32);
  put_u64(&mut value, 24, generation);
  value[32..32 + h].copy_from_slice(owner);
  put_u64(&mut value, 32 + h, generation);
  value[40 + h..40 + h + body.len()].copy_from_slice(body);
  write_trailing_crc(&mut value);
  value
}

fn immutable_key(profile: HashProfile, kind: u16, value: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(44 + value.len());
  preimage.extend_from_slice(b"aeordb.index-artifact.immutable.v1\0");
  preimage.extend_from_slice(&kind.to_le_bytes());
  preimage.extend_from_slice(value);
  profile.digest(&preimage)
}

#[derive(Debug)]
struct DecodedManifest {
  kind: ManifestKind,
  generation: u64,
  populated: bool,
  key: Vec<u8>,
}

fn decode_manifest(profile: HashProfile, value: &[u8]) -> Result<DecodedManifest, &'static str> {
  if value.len() > MAX_MANIFEST_LENGTH {
    return Err("index_manifest_length");
  }
  let h = profile.width();
  if value.len() < 44 + h {
    return Err("index_manifest_length");
  }
  if &value[0..4] != b"AIDX" || read_u16(value, 4)? != 1 || read_u16(value, 8)? != AIDX_HEADER_LENGTH as u16 {
    return Err("index_envelope");
  }
  let kind = ManifestKind::from_id(read_u16(value, 6)?).ok_or("index_manifest_kind")?;
  let identity_length = read_u16(value, 16)? as usize;
  let body_length = read_u32(value, 20)? as usize;
  if read_u16(value, 10)? != 0
    || read_u32(value, 12)? as usize != value.len()
    || identity_length != h + 8
    || read_u16(value, 18)? != 0
    || 32usize.checked_add(identity_length).and_then(|length| length.checked_add(body_length)).and_then(|length| length.checked_add(4))
      != Some(value.len())
  {
    return Err("index_envelope_metadata");
  }
  verify_trailing_crc(value)?;
  let generation = read_u64(value, 24)?;
  let owner = &value[32..32 + h];
  if generation == 0 || read_u64(value, 32 + h)? != generation || owner.iter().all(|byte| *byte == 0) {
    return Err("index_manifest_identity");
  }
  let body = &value[40 + h..value.len() - 4];
  let populated = match kind {
    ManifestKind::ScopeCatalog => decode_scope_manifest_body(profile, owner, body)?,
    ManifestKind::ValueStore => decode_value_manifest_body(profile, owner, body)?,
    ManifestKind::FieldIndex => decode_field_manifest_body(profile, owner, body)?,
    ManifestKind::FieldNvt => decode_nvt_manifest_body(profile, body)?,
  };
  Ok(DecodedManifest { kind, generation, populated, key: immutable_key(profile, kind.id(), value) })
}

fn validate_capabilities(bytes: &[u8]) -> Result<(), &'static str> {
  if bytes.len() != 32 || bytes[3..].iter().any(|byte| *byte != 0) {
    return Err("index_manifest_capability");
  }
  Ok(())
}

fn decode_correctness_prefix(profile: HashProfile, body: &[u8], definition_start: usize) -> Result<&[u8], &'static str> {
  let h = profile.width();
  if body.len() < definition_start || read_u32(body, 0)? != 0 {
    return Err("index_manifest_body_length");
  }
  validate_capabilities(body.get(4..36).ok_or("index_manifest_body_length")?)?;
  if body[40..40 + h].iter().all(|byte| *byte == 0) || body[40 + h..56 + h].iter().all(|byte| *byte == 0) {
    return Err("index_manifest_coverage");
  }
  let definition_length = read_u32(body, 36)? as usize;
  if definition_start.checked_add(definition_length) != Some(body.len()) {
    return Err("index_manifest_definition_length");
  }
  Ok(&body[definition_start..])
}

fn validate_root(presence: u8, bit: u8, root: &[u8]) -> Result<bool, &'static str> {
  let set = presence & bit != 0;
  if set == root.iter().all(|byte| *byte == 0) {
    return Err("index_manifest_root_presence");
  }
  Ok(set)
}

fn decode_scope_manifest_body(profile: HashProfile, owner: &[u8], body: &[u8]) -> Result<bool, &'static str> {
  let h = profile.width();
  let definition_start = 112 + 3 * h;
  let definition = decode_correctness_prefix(profile, body, definition_start)?;
  if definition.len() > 65_536
    || read_u16(body, 64 + h)? != 1
    || read_u16(body, 66 + h)? != 1
    || body[69 + h..72 + h].iter().any(|b| *b != 0)
  {
    return Err("scope_manifest_codec_or_reserve");
  }
  let presence = body[68 + h];
  if presence & !0x03 != 0 || read_u64(body, 72 + h)? == 0 {
    return Err("scope_manifest_presence_or_ordinal");
  }
  let ordinal = validate_root(presence, 1, &body[80 + h..80 + 2 * h])?;
  let reverse = validate_root(presence, 2, &body[80 + 2 * h..80 + 3 * h])?;
  let live = read_u64(body, 80 + 3 * h)?;
  let tombstones = read_u64(body, 88 + 3 * h)?;
  let ordinal_pages = read_u64(body, 96 + 3 * h)?;
  let reverse_pages = read_u64(body, 104 + 3 * h)?;
  if (!ordinal && (live != 0 || tombstones != 0 || ordinal_pages != 0))
    || (ordinal && ordinal_pages == 0)
    || (!reverse && (live != 0 || reverse_pages != 0))
    || (reverse && (live == 0 || reverse_pages == 0))
  {
    return Err("scope_manifest_count");
  }
  definitions::validate_scope_definition(definition).map_err(|_| "scope_manifest_definition")?;
  if definitions::scope_id(profile, definition) != owner {
    return Err("scope_manifest_owner");
  }
  Ok(ordinal || reverse)
}

fn decode_value_manifest_body(profile: HashProfile, owner: &[u8], body: &[u8]) -> Result<bool, &'static str> {
  let h = profile.width();
  let definition_start = 144 + 4 * h;
  let definition = decode_correctness_prefix(profile, body, definition_start)?;
  if definition.len() > 512 * 1_024
    || [64 + h, 66 + h, 68 + h].iter().any(|offset| read_u16(body, *offset).ok() != Some(1))
    || body[71 + h] != 0
    || body[72 + h..72 + 2 * h].iter().all(|byte| *byte == 0)
    || read_u64(body, 72 + 4 * h)? == 0
  {
    return Err("value_manifest_codec_reference_or_highwater");
  }
  let presence = body[70 + h];
  if presence & !0x03 != 0 {
    return Err("value_manifest_presence");
  }
  let values = validate_root(presence, 1, &body[72 + 2 * h..72 + 3 * h])?;
  let states = validate_root(presence, 2, &body[72 + 3 * h..72 + 4 * h])?;
  let value_counts = [80, 96, 112, 120, 136].map(|offset| read_u64(body, offset + 4 * h)).into_iter().collect::<Result<Vec<_>, _>>()?;
  let state_counts = [88, 104, 128].map(|offset| read_u64(body, offset + 4 * h)).into_iter().collect::<Result<Vec<_>, _>>()?;
  if (!values && value_counts.iter().any(|count| *count != 0))
    || (values && (value_counts[0] == 0 || value_counts[1] == 0 || value_counts[2] == 0))
    || (!states && state_counts.iter().any(|count| *count != 0))
    || (states && (state_counts[0] == 0 || state_counts[1] == 0))
  {
    return Err("value_manifest_count");
  }
  value_store::validate_value_store_definition(profile, definition).map_err(|_| "value_manifest_definition")?;
  if value_store::value_store_id_bytes(profile, definition) != owner {
    return Err("value_manifest_owner");
  }
  Ok(values || states)
}

fn decode_field_manifest_body(profile: HashProfile, owner: &[u8], body: &[u8]) -> Result<bool, &'static str> {
  let h = profile.width();
  let definition_start = 160 + 4 * h;
  let definition = decode_correctness_prefix(profile, body, definition_start)?;
  if definition.len() > 256 * 1_024
    || [64 + h, 66 + h, 68 + h].iter().any(|offset| read_u16(body, *offset).ok() != Some(1))
    || body[71 + h] != 0
    || body[72 + h..72 + 2 * h].iter().all(|byte| *byte == 0)
    || read_u64(body, 88 + 4 * h)? == 0
  {
    return Err("field_manifest_codec_reference_or_highwater");
  }
  let presence = body[70 + h];
  if presence & !0x03 != 0 {
    return Err("field_manifest_presence");
  }
  let postings = validate_root(presence, 1, &body[72 + 2 * h..72 + 3 * h])?;
  let states = validate_root(presence, 2, &body[72 + 3 * h..72 + 4 * h])?;
  let first = read_u64(body, 72 + 4 * h)?;
  let last = read_u64(body, 80 + 4 * h)?;
  let next = read_u64(body, 88 + 4 * h)?;
  let posting_counts = [96, 112, 120, 128, 152].map(|offset| read_u64(body, offset + 4 * h)).into_iter().collect::<Result<Vec<_>, _>>()?;
  let state_counts = [104, 136, 144].map(|offset| read_u64(body, offset + 4 * h)).into_iter().collect::<Result<Vec<_>, _>>()?;
  if (!postings && (first != 0 || last != 0 || posting_counts.iter().any(|count| *count != 0)))
    || (postings
      && (first == 0
        || last == 0
        || first > last
        || next <= last
        || posting_counts[0] == 0
        || posting_counts[1] == 0
        || posting_counts[3] == 0))
    || (!states && state_counts.iter().any(|count| *count != 0))
    || (states && (state_counts[0] == 0 || state_counts[1] == 0))
  {
    return Err("field_manifest_count");
  }
  field_index::validate_field_index_definition(profile, definition).map_err(|_| "field_manifest_definition")?;
  if field_index::index_id(profile, definition) != owner {
    return Err("field_manifest_owner");
  }
  Ok(postings || states)
}

fn decode_nvt_manifest_body(profile: HashProfile, body: &[u8]) -> Result<bool, &'static str> {
  let h = profile.width();
  if body.len() != 88 + 2 * h || read_u32(body, 0)? != 0 {
    return Err("nvt_manifest_length_or_flags");
  }
  validate_capabilities(&body[4..36])?;
  let tile_cells = read_u32(body, 40)?;
  let resolution = read_u64(body, 48)?;
  if read_u16(body, 36)? != 1
    || read_u16(body, 38)? != 1
    || tile_cells == 0
    || !tile_cells.is_power_of_two()
    || body[45..48].iter().any(|byte| *byte != 0)
    || resolution == 0
    || u64::from(tile_cells) > resolution
    || resolution % u64::from(tile_cells) != 0
    || read_u64(body, 56)? == 0
    || body[64..64 + h].iter().all(|byte| *byte == 0)
  {
    return Err("nvt_manifest_semantics");
  }
  let presence = body[44];
  if presence & !1 != 0 {
    return Err("nvt_manifest_presence");
  }
  let tiles = validate_root(presence, 1, &body[64 + h..64 + 2 * h])?;
  let tile_count = read_u64(body, 64 + 2 * h)?;
  let populated = read_u64(body, 72 + 2 * h)?;
  if tile_count > resolution / u64::from(tile_cells)
    || populated > resolution
    || (!tiles && (tile_count != 0 || populated != 0))
    || (tiles && (tile_count == 0 || populated == 0))
  {
    return Err("nvt_manifest_count");
  }
  Ok(tiles)
}

fn manifest_fixture_id(profile: HashProfile, kind: ManifestKind, populated: bool) -> &'static str {
  Box::leak(format!("aidx-{}-{}-manifest-{}", profile.label(), kind.name(), if populated { "populated" } else { "empty" }).into_boxed_str())
}

fn manifest_expected(kind: ManifestKind, generation: u64, populated: bool) -> &'static str {
  Box::leak(
    format!("index:manifest:{}:generation={generation}:roots={}", kind.name(), if populated { "populated" } else { "empty" })
      .into_boxed_str(),
  )
}

fn manifest_relation(kind: ManifestKind, populated: bool) -> &'static str {
  match (kind, populated) {
    (ManifestKind::ScopeCatalog, false) => "manifest:ScopeCatalogV1:empty",
    (ManifestKind::ScopeCatalog, true) => "manifest:ScopeCatalogV1:populated",
    (ManifestKind::ValueStore, false) => "manifest:ValueStoreV1:empty",
    (ManifestKind::ValueStore, true) => "manifest:ValueStoreV1:populated",
    (ManifestKind::FieldIndex, false) => "manifest:FieldIndexV1:empty",
    (ManifestKind::FieldIndex, true) => "manifest:FieldIndexV1:populated",
    (ManifestKind::FieldNvt, false) => "manifest:FieldNvtV1:empty",
    (ManifestKind::FieldNvt, true) => "manifest:FieldNvtV1:populated",
  }
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
  fn artifact_fixtures_match_results_and_keys() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn every_artifact_fixture_byte_is_integrity_protected() {
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

  #[test]
  fn manifest_presence_counts_capabilities_and_identity_fail_closed() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      for (kind, populated, bytes) in manifest_fixture_graph(profile) {
        let body_start = 40 + h;

        let mut generation = bytes.clone();
        put_u64(&mut generation, 32 + h, read_u64(&bytes, 32 + h).unwrap() + 1);
        write_trailing_crc(&mut generation);
        assert_eq!(decode_manifest(profile, &generation).err(), Some("index_manifest_identity"));

        let mut capability = bytes.clone();
        capability[body_start + 4 + 3] = 1;
        write_trailing_crc(&mut capability);
        assert_eq!(decode_manifest(profile, &capability).err(), Some("index_manifest_capability"));

        let presence_offset = body_start
          + match kind {
            ManifestKind::ScopeCatalog => 68 + h,
            ManifestKind::ValueStore | ManifestKind::FieldIndex => 70 + h,
            ManifestKind::FieldNvt => 44,
          };
        let mut presence = bytes.clone();
        presence[presence_offset] = if populated { 0 } else { 1 };
        write_trailing_crc(&mut presence);
        assert!(matches!(
          decode_manifest(profile, &presence).err(),
          Some(
            "index_manifest_root_presence"
              | "scope_manifest_count"
              | "value_manifest_count"
              | "field_manifest_count"
              | "nvt_manifest_count"
          )
        ));
      }
    }
  }

  #[test]
  fn correctness_manifest_owner_is_recomputed_from_embedded_definition() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      for (kind, _, bytes) in manifest_fixture_graph(profile) {
        if kind == ManifestKind::FieldNvt {
          continue;
        }
        let mut owner = bytes;
        owner[32] ^= 1;
        write_trailing_crc(&mut owner);
        assert!(matches!(
          decode_manifest(profile, &owner).err(),
          Some("scope_manifest_owner" | "value_manifest_owner" | "field_manifest_owner")
        ));
      }
    }
  }

  #[test]
  fn fixture_manifest_graph_references_exact_prior_closures_and_coverage() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let graph = manifest_fixture_graph(profile);
      for populated in [false, true] {
        let scope = graph.iter().find(|(kind, state, _)| *kind == ManifestKind::ScopeCatalog && *state == populated).unwrap();
        let value = graph.iter().find(|(kind, state, _)| *kind == ManifestKind::ValueStore && *state == populated).unwrap();
        let field = graph.iter().find(|(kind, state, _)| *kind == ManifestKind::FieldIndex && *state == populated).unwrap();
        let scope_body = &scope.2[40 + h..scope.2.len() - 4];
        let value_body = &value.2[40 + h..value.2.len() - 4];
        let field_body = &field.2[40 + h..field.2.len() - 4];
        assert_eq!(&value_body[72 + h..72 + 2 * h], immutable_key(profile, ManifestKind::ScopeCatalog.id(), &scope.2));
        assert_eq!(&field_body[72 + h..72 + 2 * h], immutable_key(profile, ManifestKind::ValueStore.id(), &value.2));
        assert_eq!(&scope_body[40..64 + h], &value_body[40..64 + h]);
        assert_eq!(&value_body[40..64 + h], &field_body[40..64 + h]);
        assert_eq!(&value_body[144 + 4 * h + 32..144 + 5 * h + 32], &scope.2[32..32 + h]);
        assert_eq!(&field_body[160 + 4 * h + 32..160 + 5 * h + 32], &value.2[32..32 + h]);
      }
    }
  }
}
