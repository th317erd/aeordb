use sha2::{Digest, Sha512};

const ENTITY_MAGIC: u32 = 0x0ae0_12db;
const MAX_ENTITY_VERSION: u8 = 1;
const DIRECTORY_ENTRY_TYPE: u8 = 0x03;
const INITIAL_CAPABILITIES: &[u8; 32] = &[
  0x7f, 0x00, 0x6c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashProfile {
  Blake3_256,
  Sha512,
}

impl HashProfile {
  pub fn algorithm_id(self) -> u16 {
    match self {
      Self::Blake3_256 => 0x0001,
      Self::Sha512 => 0x0003,
    }
  }

  pub fn width(self) -> usize {
    match self {
      Self::Blake3_256 => 32,
      Self::Sha512 => 64,
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      Self::Blake3_256 => "blake3-256",
      Self::Sha512 => "sha512",
    }
  }

  pub fn digest(self, bytes: &[u8]) -> Vec<u8> {
    match self {
      Self::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
      Self::Sha512 => Sha512::digest(bytes).to_vec(),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreFormat {
  WholeEntityV1,
  DirectoryIndexV1,
  SemanticObjectV1,
}

impl CoreFormat {
  pub fn id(self) -> &'static str {
    match self {
      Self::WholeEntityV1 => "whole-entity-v1",
      Self::DirectoryIndexV1 => "directory-index-v1",
      Self::SemanticObjectV1 => "semantic-object-v1",
    }
  }

  pub fn family(self) -> &'static str {
    match self {
      Self::WholeEntityV1 => "WholeEntityV1",
      Self::DirectoryIndexV1 => "DirectoryIndexV1",
      Self::SemanticObjectV1 => "SemanticObjectV1",
    }
  }
}

#[derive(Clone)]
pub struct CoreFixtureCase {
  pub id: &'static str,
  pub format: CoreFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

pub fn fixture_cases() -> Vec<CoreFixtureCase> {
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let directory = build_namespace_root(profile);
    let directory_key = directory_key(profile, &directory);
    let entity = build_entity(profile, 1, DIRECTORY_ENTRY_TYPE, &directory_key, &directory, 11);

    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "entity-blake3-256-directory-root-valid",
        HashProfile::Sha512 => "entity-sha512-directory-root-valid",
      },
      format: CoreFormat::WholeEntityV1,
      profile,
      expected: "entity:version=1:entry-type=0x03",
      relation: Some("wraps:namespace-root-valid"),
      canonical_key: Some(hex::encode(&directory_key)),
      bytes: entity,
    });
    let legacy_directory = Vec::new();
    let legacy_directory_key = legacy_directory_key(profile, &legacy_directory);
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "entity-blake3-256-directory-tree-v0-empty-valid",
        HashProfile::Sha512 => "entity-sha512-directory-tree-v0-empty-valid",
      },
      format: CoreFormat::WholeEntityV1,
      profile,
      expected: "entity:version=0:entry-type=0x03",
      relation: Some("migrated:directory-tree-v0"),
      canonical_key: Some(hex::encode(&legacy_directory_key)),
      bytes: build_entity(profile, 0, DIRECTORY_ENTRY_TYPE, &legacy_directory_key, &legacy_directory, 10),
    });
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "adir-blake3-256-namespace-root-valid",
        HashProfile::Sha512 => "adir-sha512-namespace-root-valid",
      },
      format: CoreFormat::DirectoryIndexV1,
      profile,
      expected: "directory:namespace-root",
      relation: Some("wrapped-by:whole-entity-v1"),
      canonical_key: Some(hex::encode(directory_key)),
      bytes: directory,
    });

    let definition = build_definition_object(profile);
    let definition_id = semantic_object_id(profile, 0x0004, &definition);
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "asem-blake3-256-definition-valid",
        HashProfile::Sha512 => "asem-sha512-definition-valid",
      },
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: "semantic:definition:class=2",
      relation: None,
      canonical_key: Some(hex::encode(definition_id)),
      bytes: definition,
    });

    let leaf = build_catalog_leaf(profile, b"/.aeordb-config/parsers.json");
    let leaf_id = semantic_object_id(profile, 0x0002, &leaf);
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "asem-blake3-256-catalog-leaf-valid",
        HashProfile::Sha512 => "asem-sha512-catalog-leaf-valid",
      },
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: "semantic:catalog-leaf:records=1",
      relation: Some("references:definition-valid"),
      canonical_key: Some(hex::encode(leaf_id)),
      bytes: leaf,
    });

    let internal = build_catalog_internal(profile);
    let internal_id = semantic_object_id(profile, 0x0003, &internal);
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "asem-blake3-256-catalog-internal-valid",
        HashProfile::Sha512 => "asem-sha512-catalog-internal-valid",
      },
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: "semantic:catalog-internal:children=2",
      relation: Some("references:two-catalog-leaves"),
      canonical_key: Some(hex::encode(&internal_id)),
      bytes: internal,
    });

    let state = build_semantic_state(profile, Some(&internal_id));
    let state_id = semantic_object_id(profile, 0x0001, &state);
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "asem-blake3-256-state-complete",
        HashProfile::Sha512 => "asem-sha512-state-complete",
      },
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: "semantic:state:complete",
      relation: Some("references:catalog-internal-valid"),
      canonical_key: Some(hex::encode(state_id)),
      bytes: state,
    });

    let content_only = build_semantic_state(profile, None);
    let content_only_id = semantic_object_id(profile, 0x0001, &content_only);
    cases.push(CoreFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "asem-blake3-256-state-content-only",
        HashProfile::Sha512 => "asem-sha512-state-content-only",
      },
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: "semantic:state:content-only:reason=1",
      relation: Some("empty-catalog"),
      canonical_key: Some(hex::encode(content_only_id)),
      bytes: content_only,
    });
  }
  cases
}

pub fn observe(format: CoreFormat, profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  let result = match format {
    CoreFormat::WholeEntityV1 => decode_entity(profile, bytes),
    CoreFormat::DirectoryIndexV1 => decode_directory(profile, bytes),
    CoreFormat::SemanticObjectV1 => decode_semantic_object(profile, bytes),
  };
  match result {
    Ok((summary, key)) => (summary, key.map(hex::encode)),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(format: CoreFormat, profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  match format {
    CoreFormat::WholeEntityV1 => entity_annotation_lines(profile),
    CoreFormat::DirectoryIndexV1 => directory_annotation_lines(profile),
    CoreFormat::SemanticObjectV1 => semantic_annotation_lines(profile, bytes),
  }
}

fn build_entity(profile: HashProfile, entity_version: u8, entry_type: u8, key: &[u8], value: &[u8], write_sequence: u64) -> Vec<u8> {
  let header_length = 77 + profile.width();
  let total_length = header_length + key.len() + value.len();
  let mut entity = vec![0u8; total_length];
  put_u32(&mut entity, 0, ENTITY_MAGIC);
  entity[4] = entity_version;
  entity[5] = entry_type;
  put_u16(&mut entity, 6, header_length as u16);
  put_u32(&mut entity, 8, total_length as u32);
  entity[12] = 0;
  put_u16(&mut entity, 13, profile.algorithm_id());
  entity[15] = 0;
  entity[16] = 0;
  put_u32(&mut entity, 17, key.len() as u32);
  put_u32(&mut entity, 21, value.len() as u32);
  put_u64(&mut entity, 25, 1_700_000_000_000);
  put_u64(&mut entity, 33, write_sequence);

  let mut integrity_preimage = Vec::with_capacity(22 + key.len() + value.len());
  integrity_preimage.extend_from_slice(b"aeordb-entry-v1\0");
  integrity_preimage.push(entity_version);
  integrity_preimage.push(entry_type);
  integrity_preimage.push(0);
  integrity_preimage.extend_from_slice(&profile.algorithm_id().to_le_bytes());
  integrity_preimage.push(0);
  integrity_preimage.push(0);
  integrity_preimage.extend_from_slice(&(key.len() as u32).to_le_bytes());
  integrity_preimage.extend_from_slice(&(value.len() as u32).to_le_bytes());
  integrity_preimage.extend_from_slice(key);
  integrity_preimage.extend_from_slice(value);
  let integrity = profile.digest(&integrity_preimage);
  entity[41..41 + profile.width()].copy_from_slice(&integrity);

  let crc_offset = header_length - 4;
  let crc = crc32fast::hash(&entity[..crc_offset]);
  put_u32(&mut entity, crc_offset, crc);
  entity[header_length..header_length + key.len()].copy_from_slice(key);
  entity[header_length + key.len()..].copy_from_slice(value);
  entity
}

fn decode_entity(profile: HashProfile, entity: &[u8]) -> Result<(String, Option<Vec<u8>>), &'static str> {
  if entity.len() < 12 {
    return Err("truncated_prefix");
  }
  if read_u32(entity, 0)? != ENTITY_MAGIC || entity[4] > MAX_ENTITY_VERSION {
    return Err("magic_or_version");
  }
  let entity_version = entity[4];
  let entry_type = entity[5];
  if entry_type == DIRECTORY_ENTRY_TYPE && !matches!(entity_version, 0 | 1) {
    return Err("type_version");
  }
  let header_length = read_u16(entity, 6)? as usize;
  if header_length != 77 + profile.width() || header_length > 4_096 || entity.len() < header_length {
    return Err("header_length");
  }
  if read_u32(entity, 8)? as usize != entity.len() {
    return Err("total_length");
  }
  if entity[12] != 0 || read_u16(entity, 13)? != profile.algorithm_id() || entity[15] != 0 || entity[16] != 0 {
    return Err("flags_or_algorithms");
  }
  let key_length = read_u32(entity, 17)? as usize;
  let value_length = read_u32(entity, 21)? as usize;
  if header_length.checked_add(key_length).and_then(|length| length.checked_add(value_length)) != Some(entity.len()) {
    return Err("length_arithmetic");
  }
  if read_u64(entity, 33)? == 0 {
    return Err("zero_write_sequence");
  }
  if entity[41 + profile.width()..header_length - 4].iter().any(|byte| *byte != 0) {
    return Err("reserved_nonzero");
  }
  if read_u32(entity, header_length - 4)? != crc32fast::hash(&entity[..header_length - 4]) {
    return Err("header_crc_mismatch");
  }
  let key = &entity[header_length..header_length + key_length];
  let value = &entity[header_length + key_length..];
  let mut integrity_preimage = Vec::with_capacity(22 + key.len() + value.len());
  integrity_preimage.extend_from_slice(b"aeordb-entry-v1\0");
  integrity_preimage.extend_from_slice(&entity[4..6]);
  integrity_preimage.push(entity[12]);
  integrity_preimage.extend_from_slice(&entity[13..17]);
  integrity_preimage.extend_from_slice(&entity[17..25]);
  integrity_preimage.extend_from_slice(key);
  integrity_preimage.extend_from_slice(value);
  if entity[41..41 + profile.width()] != profile.digest(&integrity_preimage) {
    return Err("integrity_hash_mismatch");
  }
  if entry_type == DIRECTORY_ENTRY_TYPE {
    let expected_key = if entity_version == 0 { legacy_directory_key(profile, value) } else { directory_key(profile, value) };
    if key != expected_key {
      return Err("directory_key_mismatch");
    }
  }
  Ok((format!("entity:version={entity_version}:entry-type=0x{entry_type:02x}"), Some(key.to_vec())))
}

fn build_namespace_root(profile: HashProfile) -> Vec<u8> {
  let body_length = 72 + 2 * profile.width();
  let total_length = 32 + body_length + 4;
  let mut value = vec![0u8; total_length];
  value[0..4].copy_from_slice(b"ADIR");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, 0x0003);
  put_u16(&mut value, 8, 32);
  put_u32(&mut value, 12, total_length as u32);
  put_u32(&mut value, 16, body_length as u32);
  value[36..68].copy_from_slice(INITIAL_CAPABILITIES);
  put_u16(&mut value, 68, 1);
  put_u16(&mut value, 70, 1);
  fill_sequence(&mut value[72..72 + profile.width()], 0x10);
  fill_sequence(&mut value[72 + profile.width()..72 + 2 * profile.width()], 0x80);
  write_trailing_crc(&mut value);
  value
}

fn decode_directory(profile: HashProfile, value: &[u8]) -> Result<(String, Option<Vec<u8>>), &'static str> {
  let body_length = 72 + 2 * profile.width();
  let expected_length = 32 + body_length + 4;
  if value.len() != expected_length {
    return Err("directory_length");
  }
  if &value[0..4] != b"ADIR" || read_u16(value, 4)? != 1 || read_u16(value, 6)? != 0x0003 || read_u16(value, 8)? != 32 {
    return Err("directory_envelope");
  }
  if read_u16(value, 10)? != 0
    || read_u32(value, 12)? as usize != value.len()
    || read_u32(value, 16)? as usize != body_length
    || read_u32(value, 20)? != 0
    || value[24..32].iter().any(|byte| *byte != 0)
  {
    return Err("directory_lengths_or_reserved");
  }
  verify_trailing_crc(value)?;
  if read_u32(value, 32)? != 0 || value[36 + 3..68].iter().any(|byte| *byte != 0) || read_u16(value, 68)? != 1 || read_u16(value, 70)? != 1
  {
    return Err("namespace_root_metadata");
  }
  if value[72..72 + profile.width()].iter().all(|byte| *byte == 0)
    || value[72 + profile.width()..72 + 2 * profile.width()].iter().all(|byte| *byte == 0)
    || value[72 + 2 * profile.width()..value.len() - 4].iter().any(|byte| *byte != 0)
  {
    return Err("namespace_root_edges_or_reserved");
  }
  Ok(("directory:namespace-root".to_string(), Some(directory_key(profile, value))))
}

fn directory_key(profile: HashProfile, value: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(42 + value.len());
  preimage.extend_from_slice(b"aeordb.directory-index.immutable.v1\0");
  preimage.extend_from_slice(&0x0003u16.to_le_bytes());
  preimage.extend_from_slice(value);
  profile.digest(&preimage)
}

fn legacy_directory_key(profile: HashProfile, value: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(5 + value.len());
  preimage.extend_from_slice(b"dirc:");
  preimage.extend_from_slice(value);
  profile.digest(&preimage)
}

fn build_definition_object(profile: HashProfile) -> Vec<u8> {
  let definition = b"\x01\x00canonical-parser-registry";
  let mut semantic_preimage = Vec::new();
  semantic_preimage.extend_from_slice(b"aeordb.semantic.parser-registry-projection.v1\0");
  semantic_preimage.extend_from_slice(definition);
  let semantic_id = profile.digest(&semantic_preimage);
  let mut body = vec![0u8; 16 + profile.width() + definition.len()];
  put_u16(&mut body, 0, 0x0002);
  put_u16(&mut body, 2, 1);
  body[8..8 + profile.width()].copy_from_slice(&semantic_id);
  put_u32(&mut body, 8 + profile.width(), definition.len() as u32);
  body[16 + profile.width()..].copy_from_slice(definition);
  build_semantic_envelope(0x0004, 1, body)
}

fn build_catalog_leaf(profile: HashProfile, path: &[u8]) -> Vec<u8> {
  let definition = build_definition_object(profile);
  let definition_id = semantic_object_id(profile, 0x0004, &definition);
  let semantic_id = definition_semantic_id(profile, &definition);
  let mut owner_key = Vec::with_capacity(2 + path.len());
  owner_key.extend_from_slice(&0x0002u16.to_le_bytes());
  owner_key.extend_from_slice(path);
  let lookup_digest = semantic_catalog_lookup_digest(profile, 0x0002, &owner_key);

  let record_length = 8 + 2 * profile.width() + owner_key.len();
  let mut record = vec![0u8; record_length];
  put_u16(&mut record, 0, 0x0002);
  put_u32(&mut record, 4, owner_key.len() as u32);
  record[8..8 + profile.width()].copy_from_slice(&semantic_id);
  record[8 + profile.width()..8 + 2 * profile.width()].copy_from_slice(&definition_id);
  record[8 + 2 * profile.width()..].copy_from_slice(&owner_key);

  let mut body = vec![0u8; 16 + profile.width() + record.len()];
  put_u32(&mut body, 4, 1);
  body[8..8 + profile.width()].copy_from_slice(&lookup_digest);
  put_u32(&mut body, 8 + profile.width(), record.len() as u32);
  body[16 + profile.width()..].copy_from_slice(&record);
  build_semantic_envelope(0x0002, 1, body)
}

fn build_catalog_internal(profile: HashProfile) -> Vec<u8> {
  let left = build_catalog_leaf(profile, b"/.aeordb-config/indexes.json");
  let right = build_catalog_leaf(profile, b"/.aeordb-config/parsers.json");
  let mut children = [
    (catalog_leaf_lookup_digest(profile, &left), semantic_object_id(profile, 0x0002, &left)),
    (catalog_leaf_lookup_digest(profile, &right), semantic_object_id(profile, 0x0002, &right)),
  ];
  let prefix_length = common_prefix_length(&children[0].0, &children[1].0);
  children.sort_by_key(|(digest, _)| digest[prefix_length]);
  let child_length = 12 + profile.width();
  let mut body = vec![0u8; 20 + prefix_length + 2 * child_length];
  put_u16(&mut body, 6, prefix_length as u16);
  put_u16(&mut body, 8, 2);
  put_u64(&mut body, 12, 2);
  body[20..20 + prefix_length].copy_from_slice(&children[0].0[..prefix_length]);
  for (index, (digest, object_id)) in children.iter().enumerate() {
    let offset = 20 + prefix_length + index * child_length;
    body[offset] = digest[prefix_length];
    put_u64(&mut body, offset + 4, 1);
    body[offset + 12..offset + 12 + profile.width()].copy_from_slice(object_id);
  }
  build_semantic_envelope(0x0003, 2, body)
}

fn build_semantic_state(profile: HashProfile, catalog_root: Option<&[u8]>) -> Vec<u8> {
  let mut body = vec![0u8; 112 + 3 * profile.width()];
  body[4..36].copy_from_slice(INITIAL_CAPABILITIES);
  put_u16(&mut body, 36, 1);
  put_u16(&mut body, 38, 1);
  put_u16(&mut body, 40, 1);
  match catalog_root {
    Some(root) => {
      body[44] = 1;
      let compiler = profile.digest(b"aeordb-v4-reference-compiler-profile-v1");
      let registry = profile.digest(b"aeordb-v4-reference-semantic-registry-v1");
      body[48..48 + profile.width()].copy_from_slice(&compiler);
      body[48 + profile.width()..48 + 2 * profile.width()].copy_from_slice(&registry);
      body[48 + 2 * profile.width()..48 + 3 * profile.width()].copy_from_slice(root);
      put_u64(&mut body, 48 + 3 * profile.width(), 2);
      put_u64(&mut body, 56 + 3 * profile.width(), 3);
      put_u64(&mut body, 64 + 3 * profile.width(), 1);
      build_semantic_envelope(0x0001, 2, body)
    }
    None => {
      put_u32(&mut body, 0, 1);
      put_u16(&mut body, 42, 1);
      build_semantic_envelope(0x0001, 0, body)
    }
  }
}

fn build_semantic_envelope(kind: u16, item_count: u64, body: Vec<u8>) -> Vec<u8> {
  let total_length = 32 + body.len() + 4;
  let mut object = vec![0u8; total_length];
  object[0..4].copy_from_slice(b"ASEM");
  put_u16(&mut object, 4, 1);
  put_u16(&mut object, 6, kind);
  put_u16(&mut object, 8, 32);
  put_u32(&mut object, 12, total_length as u32);
  put_u32(&mut object, 16, body.len() as u32);
  put_u64(&mut object, 20, item_count);
  object[32..32 + body.len()].copy_from_slice(&body);
  write_trailing_crc(&mut object);
  object
}

fn decode_semantic_object(profile: HashProfile, object: &[u8]) -> Result<(String, Option<Vec<u8>>), &'static str> {
  if object.len() < 36 {
    return Err("semantic_truncated");
  }
  if &object[0..4] != b"ASEM" || read_u16(object, 4)? != 1 || read_u16(object, 8)? != 32 {
    return Err("semantic_envelope");
  }
  if read_u16(object, 10)? != 0
    || read_u32(object, 12)? as usize != object.len()
    || read_u32(object, 16)? as usize != object.len() - 36
    || object[28..32].iter().any(|byte| *byte != 0)
  {
    return Err("semantic_lengths_or_reserved");
  }
  verify_trailing_crc(object)?;
  let kind = read_u16(object, 6)?;
  let item_count = read_u64(object, 20)?;
  let body = &object[32..object.len() - 4];
  let summary = match kind {
    0x0001 => decode_semantic_state(profile, body, item_count)?,
    0x0002 => decode_catalog_leaf(profile, body, item_count)?,
    0x0003 => decode_catalog_internal(profile, body, item_count)?,
    0x0004 => decode_definition(profile, body, item_count)?,
    _ => return Err("semantic_kind"),
  };
  Ok((summary, Some(semantic_object_id(profile, kind, object))))
}

fn decode_semantic_state(profile: HashProfile, body: &[u8], item_count: u64) -> Result<String, &'static str> {
  if body.len() != 112 + 3 * profile.width() {
    return Err("semantic_state_length");
  }
  let flags = read_u32(body, 0)?;
  if flags & !1 != 0
    || body[4 + 3..36].iter().any(|byte| *byte != 0)
    || read_u16(body, 36)? != 1
    || read_u16(body, 38)? != 1
    || read_u16(body, 40)? != 1
    || body[44] > 1
    || body[45..48].iter().any(|byte| *byte != 0)
    || body[80 + 3 * profile.width()..].iter().any(|byte| *byte != 0)
  {
    return Err("semantic_state_metadata");
  }
  let reason = read_u16(body, 42)?;
  let hashes = &body[48..48 + 3 * profile.width()];
  let counts = [
    read_u64(body, 48 + 3 * profile.width())?,
    read_u64(body, 56 + 3 * profile.width())?,
    read_u64(body, 64 + 3 * profile.width())?,
    read_u64(body, 72 + 3 * profile.width())?,
  ];
  if flags == 0 {
    if reason != 0
      || body[44] != 1
      || hashes.chunks(profile.width()).any(|hash| hash.iter().all(|byte| *byte == 0))
      || counts[0] != item_count
      || counts[0] == 0
      || counts[1] == 0
    {
      return Err("semantic_state_complete_invariant");
    }
    Ok("semantic:state:complete".to_string())
  } else {
    if !(1..=3).contains(&reason)
      || body[44] != 0
      || hashes.iter().any(|byte| *byte != 0)
      || counts.iter().any(|count| *count != 0)
      || item_count != 0
    {
      return Err("semantic_state_content_only_invariant");
    }
    Ok(format!("semantic:state:content-only:reason={reason}"))
  }
}

fn decode_definition(profile: HashProfile, body: &[u8], item_count: u64) -> Result<String, &'static str> {
  if body.len() < 16 + profile.width() || item_count != 1 {
    return Err("semantic_definition_length");
  }
  let class = read_u16(body, 0)?;
  if !(1..=7).contains(&class)
    || read_u16(body, 2)? != 1
    || read_u32(body, 4)? != 0
    || body[8..8 + profile.width()].iter().all(|byte| *byte == 0)
  {
    return Err("semantic_definition_metadata");
  }
  let definition_length = read_u32(body, 8 + profile.width())? as usize;
  if body[12 + profile.width()..16 + profile.width()].iter().any(|byte| *byte != 0)
    || 16 + profile.width() + definition_length != body.len()
  {
    return Err("semantic_definition_body");
  }
  Ok(format!("semantic:definition:class={class}"))
}

fn decode_catalog_leaf(profile: HashProfile, body: &[u8], item_count: u64) -> Result<String, &'static str> {
  if body.len() < 16 + profile.width() || read_u32(body, 0)? != 0 {
    return Err("catalog_leaf_length_or_flags");
  }
  let record_count = read_u32(body, 4)? as usize;
  if record_count == 0 || record_count > 4_096 || item_count != record_count as u64 {
    return Err("catalog_leaf_count");
  }
  let lookup_digest = &body[8..8 + profile.width()];
  let records_length = read_u32(body, 8 + profile.width())? as usize;
  if body[12 + profile.width()..16 + profile.width()].iter().any(|byte| *byte != 0) || 16 + profile.width() + records_length != body.len() {
    return Err("catalog_leaf_records_length");
  }
  let mut cursor = 16 + profile.width();
  let mut previous: Option<(u16, Vec<u8>)> = None;
  for _ in 0..record_count {
    if cursor + 8 + 2 * profile.width() > body.len() {
      return Err("catalog_leaf_record_truncated");
    }
    let kind = read_u16(body, cursor)?;
    let flags = read_u16(body, cursor + 2)?;
    let key_length = read_u32(body, cursor + 4)? as usize;
    let record_length =
      8usize.checked_add(2 * profile.width()).and_then(|length| length.checked_add(key_length)).ok_or("catalog_leaf_record_overflow")?;
    if !(1..=7).contains(&kind) || flags != 0 || cursor + record_length > body.len() {
      return Err("catalog_leaf_record_metadata");
    }
    if body[cursor + 8..cursor + 8 + 2 * profile.width()].chunks(profile.width()).any(|hash| hash.iter().all(|byte| *byte == 0)) {
      return Err("catalog_leaf_zero_hash");
    }
    let owner_key = body[cursor + 8 + 2 * profile.width()..cursor + record_length].to_vec();
    if semantic_catalog_lookup_digest(profile, kind, &owner_key) != lookup_digest {
      return Err("catalog_leaf_lookup_digest");
    }
    if previous.as_ref().is_some_and(|prior| prior >= &(kind, owner_key.clone())) {
      return Err("catalog_leaf_order");
    }
    previous = Some((kind, owner_key));
    cursor += record_length;
  }
  if cursor != body.len() {
    return Err("catalog_leaf_trailing");
  }
  Ok(format!("semantic:catalog-leaf:records={record_count}"))
}

fn decode_catalog_internal(profile: HashProfile, body: &[u8], item_count: u64) -> Result<String, &'static str> {
  if body.len() < 20 || read_u32(body, 0)? != 0 {
    return Err("catalog_internal_length_or_flags");
  }
  let depth = read_u16(body, 4)? as usize;
  let prefix_length = read_u16(body, 6)? as usize;
  let child_count = read_u16(body, 8)? as usize;
  if !(2..=256).contains(&child_count)
    || item_count != child_count as u64
    || read_u16(body, 10)? != 0
    || depth.checked_add(prefix_length).is_none_or(|next| next >= profile.width())
  {
    return Err("catalog_internal_metadata");
  }
  let child_length = 12 + profile.width();
  if 20usize.checked_add(prefix_length).and_then(|length| length.checked_add(child_count * child_length)) != Some(body.len()) {
    return Err("catalog_internal_body_length");
  }
  let mut sum = 0u64;
  let mut previous_edge = None;
  for index in 0..child_count {
    let offset = 20 + prefix_length + index * child_length;
    let edge = body[offset];
    if previous_edge.is_some_and(|previous| previous >= edge)
      || body[offset + 1] != 0
      || read_u16(body, offset + 2)? != 0
      || body[offset + 12..offset + child_length].iter().all(|byte| *byte == 0)
    {
      return Err("catalog_internal_child");
    }
    let count = read_u64(body, offset + 4)?;
    if count == 0 {
      return Err("catalog_internal_zero_count");
    }
    sum = sum.checked_add(count).ok_or("catalog_internal_count_overflow")?;
    previous_edge = Some(edge);
  }
  if sum != read_u64(body, 12)? {
    return Err("catalog_internal_subtree_count");
  }
  Ok(format!("semantic:catalog-internal:children={child_count}"))
}

fn semantic_object_id(profile: HashProfile, kind: u16, object: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(40 + object.len());
  preimage.extend_from_slice(b"aeordb.semantic-object.immutable.v1\0");
  preimage.extend_from_slice(&kind.to_le_bytes());
  preimage.extend_from_slice(object);
  profile.digest(&preimage)
}

fn semantic_catalog_lookup_digest(profile: HashProfile, kind: u16, owner_key: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(34 + owner_key.len());
  preimage.extend_from_slice(b"aeordb.semantic-catalog-key.v1\0");
  preimage.extend_from_slice(&kind.to_le_bytes());
  preimage.extend_from_slice(owner_key);
  profile.digest(&preimage)
}

fn definition_semantic_id(profile: HashProfile, object: &[u8]) -> Vec<u8> {
  object[40..40 + profile.width()].to_vec()
}

fn catalog_leaf_lookup_digest(profile: HashProfile, object: &[u8]) -> Vec<u8> {
  object[40..40 + profile.width()].to_vec()
}

fn common_prefix_length(left: &[u8], right: &[u8]) -> usize {
  left.iter().zip(right).take_while(|(left, right)| left == right).count()
}

fn entity_annotation_lines(profile: HashProfile) -> Vec<String> {
  let h = profile.width();
  vec![
    "field +0x000 len 4: magic".to_string(),
    "field +0x004 len 1: entity_version".to_string(),
    "field +0x005 len 1: entry_type".to_string(),
    "field +0x006 len 2: allocated_header_length".to_string(),
    "field +0x008 len 4: total_length".to_string(),
    "field +0x00c len 1: flags".to_string(),
    "field +0x00d len 2: hash_algorithm".to_string(),
    "field +0x00f len 1: compression_algorithm".to_string(),
    "field +0x010 len 1: encryption_algorithm".to_string(),
    "field +0x011 len 4: key_length".to_string(),
    "field +0x015 len 4: value_length".to_string(),
    "field +0x019 len 8: timestamp_ms".to_string(),
    "field +0x021 len 8: write_sequence".to_string(),
    format!("field +0x029 len {h}: integrity_hash"),
    format!("field +0x{:03x} len 32: reserved", 41 + h),
    format!("field +0x{:03x} len 4: header_crc32", 73 + h),
    format!("field +0x{:03x}: key then stored_value", 77 + h),
  ]
}

fn directory_annotation_lines(profile: HashProfile) -> Vec<String> {
  let h = profile.width();
  vec![
    "envelope +0x000 len 32: ADIR common envelope".to_string(),
    "body +0x000 len 4: root_flags".to_string(),
    "body +0x004 len 32: required_reader_capabilities".to_string(),
    "body +0x024 len 2: namespace_tree_codec".to_string(),
    "body +0x026 len 2: semantic_state_codec".to_string(),
    format!("body +0x028 len {h}: namespace_tree_root"),
    format!("body +0x{:03x} len {h}: semantic_state_root", 40 + h),
    format!("body +0x{:03x} len 32: reserved", 40 + 2 * h),
    format!("value +0x{:03x} len 4: directory_crc32", 104 + 2 * h),
  ]
}

fn semantic_annotation_lines(profile: HashProfile, object: &[u8]) -> Vec<String> {
  let h = profile.width();
  let kind = read_u16(object, 6).unwrap_or(0);
  let mut lines = vec!["envelope +0x000 len 32: ASEM common envelope".to_string(), format!("envelope kind: 0x{kind:04x}")];
  match kind {
    1 => {
      lines.push("body +0x000: SemanticStateRootV1".to_string());
      lines.push(format!("body length: {}", 112 + 3 * h));
    }
    2 => lines.push("body +0x000: SemanticCatalogLeafV1".to_string()),
    3 => lines.push("body +0x000: SemanticCatalogInternalV1".to_string()),
    4 => lines.push("body +0x000: SemanticDefinitionRecordV1".to_string()),
    _ => lines.push("body +0x000: unknown semantic kind".to_string()),
  }
  lines.push(format!("object +0x{:03x} len 4: object_crc32", object.len().saturating_sub(4)));
  lines
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
  fn core_fixture_cases_match_expected_results_and_keys() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.format, case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn entity_prefix_exposes_both_lengths() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let value = build_namespace_root(profile);
      let key = directory_key(profile, &value);
      let entity = build_entity(profile, 1, DIRECTORY_ENTRY_TYPE, &key, &value, 1);
      assert_eq!(read_u16(&entity, 6).unwrap() as usize, 77 + profile.width());
      assert_eq!(read_u32(&entity, 8).unwrap() as usize, entity.len());
    }
  }

  #[test]
  fn every_core_fixture_byte_is_integrity_protected() {
    for case in fixture_cases() {
      for index in 0..case.bytes.len() {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 0x01;
        let (observed, _) = observe(case.format, case.profile, &mutated);
        assert!(observed.starts_with("error:"), "fixture {} byte {index} unexpectedly produced {observed}", case.id);
      }
    }
  }
}
