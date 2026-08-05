use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::core::HashProfile;
use crate::definitions;
use crate::index::{
  build_immutable_value, decode_immutable_value, fill_sequence, immutable_key, put_u16, put_u32, put_u64, read_u16, read_u32, read_u64,
  IndexFixtureCase, IndexFormat,
};

const DIRECTORY_KIND: u16 = 0x0020;
const POSTING_PAGE_KIND: u16 = 0x0030;
const VALUE_PAGE_KIND: u16 = 0x0031;
const SCOPE_PAGE_KIND: u16 = 0x0033;
const STATE_PAGE_KIND: u16 = 0x0034;
const MAX_ARTIFACT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_EVIDENCE_LENGTH: usize = 4 * 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryRole {
  ScopeOrdinal,
  ScopeReverse,
  Value,
  ValueDocumentState,
  Posting,
  IndexDocumentState,
  NvtTile,
}

impl DirectoryRole {
  fn id(self) -> u8 {
    match self {
      Self::ScopeOrdinal => 1,
      Self::ScopeReverse => 2,
      Self::Value => 3,
      Self::ValueDocumentState => 4,
      Self::Posting => 5,
      Self::IndexDocumentState => 6,
      Self::NvtTile => 7,
    }
  }

  fn from_id(value: u8) -> Option<Self> {
    match value {
      1 => Some(Self::ScopeOrdinal),
      2 => Some(Self::ScopeReverse),
      3 => Some(Self::Value),
      4 => Some(Self::ValueDocumentState),
      5 => Some(Self::Posting),
      6 => Some(Self::IndexDocumentState),
      7 => Some(Self::NvtTile),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::ScopeOrdinal => "scope-ordinal",
      Self::ScopeReverse => "scope-reverse",
      Self::Value => "value",
      Self::ValueDocumentState => "value-document-state",
      Self::Posting => "posting",
      Self::IndexDocumentState => "index-document-state",
      Self::NvtTile => "nvt-tile",
    }
  }

  fn owner_class(self) -> u8 {
    match self {
      Self::ScopeOrdinal | Self::ScopeReverse => 1,
      Self::Value | Self::ValueDocumentState => 2,
      Self::Posting | Self::IndexDocumentState | Self::NvtTile => 3,
    }
  }

  fn key_codec(self) -> u16 {
    match self {
      Self::ScopeOrdinal | Self::ValueDocumentState | Self::IndexDocumentState => 1,
      Self::ScopeReverse => 2,
      Self::Value => 3,
      Self::Posting => 4,
      Self::NvtTile => 5,
    }
  }

  fn child_kind(self) -> u16 {
    match self {
      Self::ScopeOrdinal | Self::ScopeReverse => SCOPE_PAGE_KIND,
      Self::Value => VALUE_PAGE_KIND,
      Self::ValueDocumentState | Self::IndexDocumentState => STATE_PAGE_KIND,
      Self::Posting => POSTING_PAGE_KIND,
      Self::NvtTile => 0x0032,
    }
  }

  fn uses_page_id(self) -> bool {
    !matches!(self, Self::ScopeOrdinal | Self::ScopeReverse | Self::NvtTile)
  }
}

#[derive(Clone)]
struct SamplePage {
  role: DirectoryRole,
  owner: Vec<u8>,
  owner_class: u8,
  page_id: u64,
  generation: u64,
  lower: Vec<u8>,
  upper: Vec<u8>,
  live: u64,
  tombstones: u64,
  logical_bytes: u64,
  bytes: Vec<u8>,
}

pub(crate) fn fixture_cases() -> Vec<IndexFixtureCase> {
  let mut cases = Vec::with_capacity(28);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let pages = sample_pages(profile);
    for page in &pages {
      let decoded = decode_page(profile, &page.bytes).expect("sample ordered page must decode");
      cases.push(IndexFixtureCase {
        id: leak(format!("aidx-{}-{}-page-valid", profile.label(), page.role.name())),
        format: IndexFormat::IndexArtifactV1,
        profile,
        expected: page_expected(page.role, decoded.page_id, decoded.record_count),
        relation: Some(page_relation(page.role)),
        canonical_key: Some(hex::encode(decoded.key)),
        bytes: page.bytes.clone(),
      });

      let directory = build_leaf_directory(profile, page);
      let decoded = decode_directory(profile, &directory).expect("sample leaf directory must decode");
      cases.push(IndexFixtureCase {
        id: leak(format!("aidx-{}-{}-directory-leaf-valid", profile.label(), page.role.name())),
        format: IndexFormat::IndexArtifactV1,
        profile,
        expected: directory_expected(
          page.role,
          decoded.level,
          decoded.entry_count,
          decoded.live,
          decoded.pages,
          decoded.lower.len(),
          decoded.upper.len(),
        ),
        relation: Some(directory_relation(page.role, "leaf")),
        canonical_key: Some(hex::encode(decoded.key)),
        bytes: directory.clone(),
      });

      if page.role == DirectoryRole::Posting {
        let internal = build_internal_directory(profile, page, &directory);
        let decoded = decode_directory(profile, &internal).expect("sample internal directory must decode");
        cases.push(IndexFixtureCase {
          id: leak(format!("aidx-{}-posting-directory-internal-valid", profile.label())),
          format: IndexFormat::IndexArtifactV1,
          profile,
          expected: directory_expected(
            page.role,
            decoded.level,
            decoded.entry_count,
            decoded.live,
            decoded.pages,
            decoded.lower.len(),
            decoded.upper.len(),
          ),
          relation: Some("directory:PostingPageV1:internal-over-leaf"),
          canonical_key: Some(hex::encode(decoded.key)),
          bytes: internal,
        });
      }
    }
  }
  cases
}

pub(crate) fn nvt_directory_fixture(
  profile: HashProfile,
  owner: &[u8],
  tile_start_cell: u64,
  generation: u64,
  populated_entries: u32,
  logical_bytes: u64,
  tile_bytes: &[u8],
) -> IndexFixtureCase {
  assert_ne!(populated_entries, 0);
  let fence = tile_start_cell.to_le_bytes().to_vec();
  let page = SamplePage {
    role: DirectoryRole::NvtTile,
    owner: owner.to_vec(),
    owner_class: 3,
    page_id: 0,
    generation,
    lower: fence.clone(),
    upper: fence,
    live: u64::from(populated_entries),
    tombstones: 0,
    logical_bytes,
    bytes: tile_bytes.to_vec(),
  };
  let bytes = build_leaf_directory(profile, &page);
  let decoded = decode_directory(profile, &bytes).expect("sample NVT directory must decode");
  IndexFixtureCase {
    id: leak(format!("aidx-{}-nvt-tile-directory-leaf-valid", profile.label())),
    format: IndexFormat::IndexArtifactV1,
    profile,
    expected: directory_expected(
      page.role,
      decoded.level,
      decoded.entry_count,
      decoded.live,
      decoded.pages,
      decoded.lower.len(),
      decoded.upper.len(),
    ),
    relation: Some("directory:NvtTileV1:sparse-hint-only"),
    canonical_key: Some(hex::encode(decoded.key)),
    bytes,
  }
}

pub(crate) fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match read_u16(bytes, 6) {
    Ok(DIRECTORY_KIND) => match decode_directory(profile, bytes) {
      Ok(directory) => (
        directory_expected(
          directory.role,
          directory.level,
          directory.entry_count,
          directory.live,
          directory.pages,
          directory.lower.len(),
          directory.upper.len(),
        )
        .to_string(),
        Some(hex::encode(directory.key)),
      ),
      Err(error) => (format!("error:{error}"), None),
    },
    Ok(POSTING_PAGE_KIND | VALUE_PAGE_KIND | SCOPE_PAGE_KIND | STATE_PAGE_KIND) => match decode_page(profile, bytes) {
      Ok(page) => (page_expected(page.role, page.page_id, page.record_count).to_string(), Some(hex::encode(page.key))),
      Err(error) => (format!("error:{error}"), None),
    },
    Ok(_) => ("error:index_ordered_artifact_kind".to_string(), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub(crate) fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let kind = read_u16(bytes, 6).unwrap_or(0);
  let identity_length = read_u16(bytes, 16).unwrap_or(0);
  let body_length = read_u32(bytes, 20).unwrap_or(0);
  vec![
    "envelope +0x000 len 32: AIDX common envelope".to_string(),
    format!("envelope artifact_kind: 0x{kind:04x}"),
    format!("identity +0x000 len {identity_length}: exact role-specific identity (H={})", profile.width()),
    format!("body +0x000 len {body_length}: exact directory/page body"),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

fn sample_pages(profile: HashProfile) -> Vec<SamplePage> {
  let scope_definition = definitions::sample_scope_definition();
  let scope_id = definitions::scope_id(profile, &scope_definition);
  let value_definition = crate::value_store::sample_value_store_definition_for_scope(profile, &scope_id);
  let value_store_id = crate::value_store::value_store_id_bytes(profile, &value_definition);
  let field_definition = crate::field_index::sample_field_index_definition_for_value_store(profile, &value_store_id);
  let index_id = crate::field_index::index_id(profile, &field_definition);

  let scope_ordinal = build_scope_page(profile, &scope_id, DirectoryRole::ScopeOrdinal);
  let scope_reverse = build_scope_page(profile, &scope_id, DirectoryRole::ScopeReverse);
  validate_scope_catalog_pair(profile, &scope_ordinal.bytes, &scope_reverse.bytes).expect("sample scope catalog directions agree");
  vec![
    scope_ordinal,
    scope_reverse,
    build_value_page(profile, &value_store_id),
    build_state_page(profile, &value_store_id, DirectoryRole::ValueDocumentState),
    build_posting_page(profile, &index_id),
    build_state_page(profile, &index_id, DirectoryRole::IndexDocumentState),
  ]
}

fn build_scope_page(profile: HashProfile, owner: &[u8], role: DirectoryRole) -> SamplePage {
  let h = profile.width();
  let mut rows = Vec::new();
  for (ordinal, path) in [(1u64, "/workspace/docs/a.md"), (2, "/workspace/docs/b.md")] {
    let file_key = definitions::file_key(profile, path).expect("canonical sample path");
    let mut revision = vec![0u8; h];
    fill_sequence(&mut revision, 0x70 + ordinal as u8);
    rows.push((ordinal, path, file_key, revision));
  }

  let records = if role == DirectoryRole::ScopeOrdinal {
    rows
      .iter()
      .map(|(ordinal, path, file_key, revision)| {
        let mut record = vec![0u8; 16 + 2 * h + path.len()];
        put_u32(&mut record, 4, path.len() as u32);
        put_u64(&mut record, 8, *ordinal);
        record[16..16 + h].copy_from_slice(file_key);
        record[16 + h..16 + 2 * h].copy_from_slice(revision);
        record[16 + 2 * h..].copy_from_slice(path.as_bytes());
        record
      })
      .collect::<Vec<_>>()
  } else {
    rows.sort_by(|left, right| left.2.cmp(&right.2));
    rows
      .iter()
      .map(|(ordinal, _, file_key, _)| {
        let mut record = vec![0u8; 12 + h];
        put_u64(&mut record, 4, *ordinal);
        record[12..].copy_from_slice(file_key);
        record
      })
      .collect::<Vec<_>>()
  };
  let keys = records
    .iter()
    .map(|record| if role == DirectoryRole::ScopeOrdinal { record[8..16].to_vec() } else { record[12..12 + h].to_vec() })
    .collect::<Vec<_>>();
  let (body, logical_bytes) = build_ordered_page_body(role, &records, &keys, None);
  let mut identity = Vec::with_capacity(h + 1 + h.max(8));
  identity.extend_from_slice(owner);
  identity.push(role.id());
  identity.extend_from_slice(&keys[0]);
  let generation = 0x3301 + u64::from(role.id());
  let bytes = build_immutable_value(SCOPE_PAGE_KIND, generation, &identity, &body);
  SamplePage {
    role,
    owner: owner.to_vec(),
    owner_class: 1,
    page_id: 0,
    generation,
    lower: keys[0].clone(),
    upper: keys[1].clone(),
    live: records.len() as u64,
    tombstones: 0,
    logical_bytes,
    bytes,
  }
}

fn canonical_utf8(value: &str) -> Vec<u8> {
  let mut bytes = Vec::with_capacity(5 + value.len());
  bytes.push(0x07);
  bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
  bytes.extend_from_slice(value.as_bytes());
  bytes
}

fn build_value_page(profile: HashProfile, owner: &[u8]) -> SamplePage {
  let h = profile.width();
  let mut records = Vec::new();
  for (ordinal, value) in [(1u64, "alpha"), (2, "beta")] {
    let value = canonical_utf8(value);
    let mut record = vec![0u8; 24 + h + value.len()];
    put_u32(&mut record, 4, value.len() as u32);
    put_u64(&mut record, 8, ordinal);
    put_u32(&mut record, 16, 0);
    fill_sequence(&mut record[24..24 + h], 0x80 + ordinal as u8);
    record[24 + h..].copy_from_slice(&value);
    records.push(record);
  }
  let keys = records
    .iter()
    .map(|record| {
      let mut key = Vec::with_capacity(12);
      key.extend_from_slice(&record[8..16]);
      key.extend_from_slice(&record[16..20]);
      key
    })
    .collect::<Vec<_>>();
  let (body, logical_bytes) = build_ordered_page_body(DirectoryRole::Value, &records, &keys, None);
  build_id_page(profile, DirectoryRole::Value, owner, 0x3101, 201, PagePayload { body, keys, live: records.len() as u64, logical_bytes })
}

fn typed_exact_key(source_value: &[u8]) -> Vec<u8> {
  let mut input = Vec::with_capacity(38 + source_value.len());
  input.extend_from_slice(b"aeordb.typed-exact-posting.v1\0");
  input.extend_from_slice(source_value);
  let digest = blake3::hash(&input);
  let mut key = Vec::with_capacity(33);
  key.push(source_value[0]);
  key.extend_from_slice(digest.as_bytes());
  key
}

fn exact_coordinate(key: &[u8]) -> u64 {
  let mut input = Vec::with_capacity(39 + key.len());
  input.extend_from_slice(b"aeordb.index.exact-coordinate.v1\0");
  input.extend_from_slice(key);
  u64::from_be_bytes(blake3::hash(&input).as_bytes()[..8].try_into().unwrap())
}

fn posting_position(record: &[u8]) -> Vec<u8> {
  let key_length = read_u32(record, 4).expect("sample posting key length") as usize;
  let mut key = Vec::with_capacity(24 + key_length);
  key.extend_from_slice(&record[8..16]);
  key.extend_from_slice(&record[32..32 + key_length]);
  key.extend_from_slice(&record[16..32]);
  key
}

fn build_posting_page(profile: HashProfile, owner: &[u8]) -> SamplePage {
  let mut records = Vec::new();
  for (ordinal, value) in [(1u64, "alpha"), (2, "beta")] {
    let key = typed_exact_key(&canonical_utf8(value));
    let mut record = vec![0u8; 32 + key.len()];
    put_u32(&mut record, 4, key.len() as u32);
    put_u64(&mut record, 8, exact_coordinate(&key));
    put_u64(&mut record, 16, ordinal);
    record[32..].copy_from_slice(&key);
    records.push(record);
  }
  records.sort_by(|left, right| compare_posting_records(left, right).expect("sample posting comparison"));
  let keys = records.iter().map(|record| posting_position(record)).collect::<Vec<_>>();
  let coordinates = (read_u64(&records[0], 8).unwrap(), read_u64(records.last().unwrap(), 8).unwrap());
  let (body, logical_bytes) = build_ordered_page_body(DirectoryRole::Posting, &records, &keys, Some(coordinates));
  build_id_page(profile, DirectoryRole::Posting, owner, 0x3001, 301, PagePayload { body, keys, live: records.len() as u64, logical_bytes })
}

fn build_state_page(profile: HashProfile, owner: &[u8], role: DirectoryRole) -> SamplePage {
  let h = profile.width();
  let (stage, reason) = if role == DirectoryRole::ValueDocumentState { (2u8, 0x0005u16) } else { (5u8, 0x000cu16) };
  let evidence = canonical_utf8("limit");
  let mut records = Vec::new();
  for ordinal in [1u64, 2] {
    let mut record = vec![0u8; 48 + h + evidence.len()];
    record[1] = stage;
    put_u16(&mut record, 2, reason);
    put_u32(&mut record, 4, evidence.len() as u32);
    put_u64(&mut record, 8, ordinal);
    fill_sequence(&mut record[16..16 + h], 0xa0 + ordinal as u8);
    put_u64(&mut record, 16 + h, 1);
    put_u64(&mut record, 24 + h, 64);
    put_u64(&mut record, 32 + h, 128);
    record[48 + h..].copy_from_slice(&evidence);
    records.push(record);
  }
  let keys = records.iter().map(|record| record[8..16].to_vec()).collect::<Vec<_>>();
  let (body, logical_bytes) = build_ordered_page_body(role, &records, &keys, None);
  build_id_page(
    profile,
    role,
    owner,
    0x3400 + u64::from(role.id()),
    401 + u64::from(role.id()),
    PagePayload { body, keys, live: records.len() as u64, logical_bytes },
  )
}

struct PagePayload {
  body: Vec<u8>,
  keys: Vec<Vec<u8>>,
  live: u64,
  logical_bytes: u64,
}

fn build_id_page(
  profile: HashProfile,
  role: DirectoryRole,
  owner: &[u8],
  generation: u64,
  page_id: u64,
  payload: PagePayload,
) -> SamplePage {
  let mut identity = Vec::with_capacity(profile.width() + 16);
  identity.extend_from_slice(owner);
  if matches!(role, DirectoryRole::ValueDocumentState | DirectoryRole::IndexDocumentState) {
    identity.push(role.owner_class());
    identity.extend_from_slice(&[0u8; 7]);
  }
  identity.extend_from_slice(&page_id.to_le_bytes());
  let bytes = build_immutable_value(role.child_kind(), generation, &identity, &payload.body);
  SamplePage {
    role,
    owner: owner.to_vec(),
    owner_class: role.owner_class(),
    page_id,
    generation,
    lower: payload.keys.first().unwrap().clone(),
    upper: payload.keys.last().unwrap().clone(),
    live: payload.live,
    tombstones: 0,
    logical_bytes: payload.logical_bytes,
    bytes,
  }
}

fn build_ordered_page_body(role: DirectoryRole, records: &[Vec<u8>], keys: &[Vec<u8>], coordinates: Option<(u64, u64)>) -> (Vec<u8>, u64) {
  assert_eq!(records.len(), keys.len());
  let records_length: usize = records.iter().map(Vec::len).sum();
  let logical_live_bytes = records.iter().filter(|record| record[0] & 1 == 0).map(Vec::len).sum::<usize>();
  let lower = keys.first().expect("nonempty page");
  let upper = keys.last().expect("nonempty page");
  let mut body = vec![0u8; 96 + lower.len() + upper.len() + records_length];
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, role.key_codec());
  put_u32(&mut body, 24, lower.len() as u32);
  put_u32(&mut body, 28, upper.len() as u32);
  put_u32(&mut body, 32, records.len() as u32);
  put_u32(&mut body, 36, records.iter().filter(|record| record[0] & 1 == 0).count() as u32);
  put_u32(&mut body, 40, records.iter().filter(|record| record[0] & 1 != 0).count() as u32);
  put_u64(&mut body, 48, records_length as u64);
  put_u64(&mut body, 56, logical_live_bytes as u64);
  if let Some((minimum, maximum)) = coordinates {
    put_u64(&mut body, 64, minimum);
    put_u64(&mut body, 72, maximum);
  }
  let mut cursor = 96;
  body[cursor..cursor + lower.len()].copy_from_slice(lower);
  cursor += lower.len();
  body[cursor..cursor + upper.len()].copy_from_slice(upper);
  cursor += upper.len();
  for record in records {
    body[cursor..cursor + record.len()].copy_from_slice(record);
    cursor += record.len();
  }
  (body, logical_live_bytes as u64)
}

fn build_leaf_directory(profile: HashProfile, page: &SamplePage) -> Vec<u8> {
  let h = profile.width();
  let fixed = 72 + h;
  let entries_length = fixed + page.lower.len() + page.upper.len();
  let mut body = vec![0u8; 80 + page.lower.len() + page.upper.len() + entries_length];
  put_u16(&mut body, 2, page.role.key_codec());
  put_u32(&mut body, 4, 1);
  put_u32(&mut body, 16, page.lower.len() as u32);
  put_u32(&mut body, 20, page.upper.len() as u32);
  put_u64(&mut body, 24, page.live);
  put_u64(&mut body, 32, page.tombstones);
  put_u64(&mut body, 40, 1);
  put_u64(&mut body, 48, page.logical_bytes);
  if page.role.uses_page_id() {
    put_u64(&mut body, 56, page.page_id);
    put_u64(&mut body, 64, page.page_id);
  }
  put_u32(&mut body, 72, entries_length as u32);
  let mut cursor = 80;
  body[cursor..cursor + page.lower.len()].copy_from_slice(&page.lower);
  cursor += page.lower.len();
  body[cursor..cursor + page.upper.len()].copy_from_slice(&page.upper);
  cursor += page.upper.len();
  put_u32(&mut body, cursor, page.lower.len() as u32);
  put_u32(&mut body, cursor + 4, page.upper.len() as u32);
  put_u64(&mut body, cursor + 8, page.page_id);
  let child_key = immutable_key(profile, page.role.child_kind(), &page.bytes);
  body[cursor + 16..cursor + 16 + h].copy_from_slice(&child_key);
  let fields = cursor + 16 + h;
  put_u64(&mut body, fields, page.generation);
  put_u64(&mut body, fields + 8, page.live);
  put_u64(&mut body, fields + 16, page.tombstones);
  put_u64(&mut body, fields + 24, page.logical_bytes);
  cursor += fixed;
  body[cursor..cursor + page.lower.len()].copy_from_slice(&page.lower);
  cursor += page.lower.len();
  body[cursor..cursor + page.upper.len()].copy_from_slice(&page.upper);
  let mut identity = Vec::with_capacity(h + 2);
  identity.extend_from_slice(&page.owner);
  identity.push(page.owner_class);
  identity.push(page.role.id());
  build_immutable_value(DIRECTORY_KIND, page.generation + 10, &identity, &body)
}

fn build_internal_directory(profile: HashProfile, page: &SamplePage, child: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let fixed = 88 + h;
  let entries_length = fixed + page.lower.len() + page.upper.len();
  let mut body = vec![0u8; 80 + page.lower.len() + page.upper.len() + entries_length];
  put_u16(&mut body, 0, 1);
  put_u16(&mut body, 2, page.role.key_codec());
  put_u32(&mut body, 4, 1);
  put_u32(&mut body, 16, page.lower.len() as u32);
  put_u32(&mut body, 20, page.upper.len() as u32);
  put_u64(&mut body, 24, page.live);
  put_u64(&mut body, 32, page.tombstones);
  put_u64(&mut body, 40, 1);
  put_u64(&mut body, 48, page.logical_bytes);
  put_u64(&mut body, 56, page.page_id);
  put_u64(&mut body, 64, page.page_id);
  put_u32(&mut body, 72, entries_length as u32);
  let mut cursor = 80;
  body[cursor..cursor + page.lower.len()].copy_from_slice(&page.lower);
  cursor += page.lower.len();
  body[cursor..cursor + page.upper.len()].copy_from_slice(&page.upper);
  cursor += page.upper.len();
  put_u32(&mut body, cursor, page.lower.len() as u32);
  put_u32(&mut body, cursor + 4, page.upper.len() as u32);
  let child_key = immutable_key(profile, DIRECTORY_KIND, child);
  body[cursor + 8..cursor + 8 + h].copy_from_slice(&child_key);
  let fields = cursor + 8 + h;
  put_u64(&mut body, fields, page.generation + 10);
  put_u64(&mut body, fields + 8, page.live);
  put_u64(&mut body, fields + 16, page.tombstones);
  put_u64(&mut body, fields + 24, 1);
  put_u64(&mut body, fields + 32, page.logical_bytes);
  put_u64(&mut body, fields + 40, page.page_id);
  put_u64(&mut body, fields + 48, page.page_id);
  cursor += fixed;
  body[cursor..cursor + page.lower.len()].copy_from_slice(&page.lower);
  cursor += page.lower.len();
  body[cursor..cursor + page.upper.len()].copy_from_slice(&page.upper);
  let mut identity = Vec::with_capacity(h + 2);
  identity.extend_from_slice(&page.owner);
  identity.push(page.owner_class);
  identity.push(page.role.id());
  build_immutable_value(DIRECTORY_KIND, page.generation + 20, &identity, &body)
}

#[derive(Debug)]
struct DecodedDirectory {
  role: DirectoryRole,
  level: u16,
  entry_count: u32,
  lower: Vec<u8>,
  upper: Vec<u8>,
  live: u64,
  pages: u64,
  key: Vec<u8>,
}

#[derive(Debug)]
struct DirectoryEntry {
  lower: Vec<u8>,
  upper: Vec<u8>,
  live: u64,
  tombstones: u64,
  pages: u64,
  logical_bytes: u64,
  minimum_page_id: u64,
  maximum_page_id: u64,
}

fn decode_directory(profile: HashProfile, bytes: &[u8]) -> Result<DecodedDirectory, &'static str> {
  let artifact = decode_immutable_value(profile, bytes, MAX_ARTIFACT_LENGTH)?;
  if artifact.kind != DIRECTORY_KIND {
    return Err("artifact_directory_kind");
  }
  let h = profile.width();
  if artifact.identity.len() != h + 2 || artifact.identity[..h].iter().all(|byte| *byte == 0) {
    return Err("artifact_directory_identity");
  }
  let owner_class = artifact.identity[h];
  let role = DirectoryRole::from_id(artifact.identity[h + 1]).ok_or("artifact_directory_role")?;
  if owner_class != role.owner_class() {
    return Err("artifact_directory_owner_class");
  }
  let body = artifact.body;
  if body.len() < 80 {
    return Err("artifact_directory_body_length");
  }
  let level = read_u16(body, 0)?;
  let entry_count = read_u32(body, 4)?;
  let lower_length = read_u32(body, 16)? as usize;
  let upper_length = read_u32(body, 20)? as usize;
  let entries_length = read_u32(body, 72)? as usize;
  if level > 15
    || read_u16(body, 2)? != role.key_codec()
    || entry_count == 0
    || entry_count > 65_536
    || read_u32(body, 8)? != 0
    || read_u32(body, 12)? != 0
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
    || read_u32(body, 76)? != 0
    || 80usize
      .checked_add(lower_length)
      .and_then(|length| length.checked_add(upper_length))
      .and_then(|length| length.checked_add(entries_length))
      != Some(body.len())
  {
    return Err("artifact_directory_header");
  }
  let lower = body[80..80 + lower_length].to_vec();
  let upper = body[80 + lower_length..80 + lower_length + upper_length].to_vec();
  validate_key(profile, role, &lower)?;
  validate_key(profile, role, &upper)?;
  if compare_keys(profile, role, &lower, &upper)? == Ordering::Greater {
    return Err("artifact_directory_fence_order");
  }
  let mut cursor = 80 + lower_length + upper_length;
  let entries_end = cursor + entries_length;
  let mut entries = Vec::new();
  for _ in 0..entry_count {
    let entry = if level == 0 {
      decode_leaf_descriptor(profile, role, artifact.generation, body, &mut cursor, entries_end)?
    } else {
      decode_internal_descriptor(profile, role, artifact.generation, body, &mut cursor, entries_end)?
    };
    if entries.last().is_some_and(|previous: &DirectoryEntry| {
      compare_keys(profile, role, &previous.upper, &entry.lower).is_ok_and(|ordering| ordering != Ordering::Less)
    }) {
      return Err("artifact_directory_overlap");
    }
    entries.push(entry);
  }
  if cursor != entries_end
    || entries.first().map(|entry| entry.lower.as_slice()) != Some(lower.as_slice())
    || entries.last().map(|entry| entry.upper.as_slice()) != Some(upper.as_slice())
  {
    return Err("artifact_directory_entries_or_fences");
  }
  let live = checked_sum(entries.iter().map(|entry| entry.live))?;
  let tombstones = checked_sum(entries.iter().map(|entry| entry.tombstones))?;
  let pages = checked_sum(entries.iter().map(|entry| entry.pages))?;
  let logical_bytes = checked_sum(entries.iter().map(|entry| entry.logical_bytes))?;
  let minimum_page_id = entries.iter().map(|entry| entry.minimum_page_id).min().ok_or("artifact_directory_empty")?;
  let maximum_page_id = entries.iter().map(|entry| entry.maximum_page_id).max().ok_or("artifact_directory_empty")?;
  if read_u64(body, 24)? != live
    || read_u64(body, 32)? != tombstones
    || read_u64(body, 40)? != pages
    || read_u64(body, 48)? != logical_bytes
    || read_u64(body, 56)? != minimum_page_id
    || read_u64(body, 64)? != maximum_page_id
    || pages == 0
  {
    return Err("artifact_directory_aggregate");
  }
  Ok(DecodedDirectory { role, level, entry_count, lower, upper, live, pages, key: artifact.key })
}

fn decode_leaf_descriptor(
  profile: HashProfile,
  role: DirectoryRole,
  parent_generation: u64,
  body: &[u8],
  cursor: &mut usize,
  end: usize,
) -> Result<DirectoryEntry, &'static str> {
  let h = profile.width();
  let fixed = 72 + h;
  if cursor.checked_add(fixed).is_none_or(|next| next > end) {
    return Err("artifact_directory_leaf_truncated");
  }
  let start = *cursor;
  let lower_length = read_u32(body, start)? as usize;
  let upper_length = read_u32(body, start + 4)? as usize;
  let page_id = read_u64(body, start + 8)?;
  let child_hash = body[start + 16..start + 16 + h].to_vec();
  let fields = start + 16 + h;
  let child_generation = read_u64(body, fields)?;
  let live = read_u64(body, fields + 8)?;
  let tombstones = read_u64(body, fields + 16)?;
  let logical_bytes = read_u64(body, fields + 24)?;
  validate_physical_hints(body, fields + 32)?;
  let key_start = start + fixed;
  let next =
    key_start.checked_add(lower_length).and_then(|value| value.checked_add(upper_length)).ok_or("artifact_directory_leaf_overflow")?;
  if lower_length == 0 || upper_length == 0 || lower_length > MAX_KEY_LENGTH || upper_length > MAX_KEY_LENGTH || next > end {
    return Err("artifact_directory_leaf_length");
  }
  if child_hash.iter().all(|byte| *byte == 0)
    || child_generation == 0
    || child_generation > parent_generation
    || live.checked_add(tombstones).is_none_or(|count| count == 0)
    || logical_bytes == 0
  {
    return Err("artifact_directory_leaf_semantics");
  }
  if role.uses_page_id() == (page_id == 0) {
    return Err("artifact_directory_page_id");
  }
  let lower = body[key_start..key_start + lower_length].to_vec();
  let upper = body[key_start + lower_length..next].to_vec();
  validate_descriptor_keys(profile, role, &lower, &upper)?;
  *cursor = next;
  Ok(DirectoryEntry { lower, upper, live, tombstones, pages: 1, logical_bytes, minimum_page_id: page_id, maximum_page_id: page_id })
}

fn decode_internal_descriptor(
  profile: HashProfile,
  role: DirectoryRole,
  parent_generation: u64,
  body: &[u8],
  cursor: &mut usize,
  end: usize,
) -> Result<DirectoryEntry, &'static str> {
  let h = profile.width();
  let fixed = 88 + h;
  if cursor.checked_add(fixed).is_none_or(|next| next > end) {
    return Err("artifact_directory_internal_truncated");
  }
  let start = *cursor;
  let lower_length = read_u32(body, start)? as usize;
  let upper_length = read_u32(body, start + 4)? as usize;
  let child_hash = body[start + 8..start + 8 + h].to_vec();
  let fields = start + 8 + h;
  let child_generation = read_u64(body, fields)?;
  let live = read_u64(body, fields + 8)?;
  let tombstones = read_u64(body, fields + 16)?;
  let pages = read_u64(body, fields + 24)?;
  let logical_bytes = read_u64(body, fields + 32)?;
  let minimum_page_id = read_u64(body, fields + 40)?;
  let maximum_page_id = read_u64(body, fields + 48)?;
  validate_physical_hints(body, fields + 56)?;
  let key_start = start + fixed;
  let next =
    key_start.checked_add(lower_length).and_then(|value| value.checked_add(upper_length)).ok_or("artifact_directory_internal_overflow")?;
  if lower_length == 0 || upper_length == 0 || lower_length > MAX_KEY_LENGTH || upper_length > MAX_KEY_LENGTH || next > end {
    return Err("artifact_directory_internal_length");
  }
  if child_hash.iter().all(|byte| *byte == 0)
    || child_generation == 0
    || child_generation > parent_generation
    || live.checked_add(tombstones).is_none_or(|count| count == 0)
    || pages == 0
    || logical_bytes == 0
    || minimum_page_id > maximum_page_id
    || (role.uses_page_id() && minimum_page_id == 0)
    || (!role.uses_page_id() && (minimum_page_id != 0 || maximum_page_id != 0))
  {
    return Err("artifact_directory_internal_semantics");
  }
  let lower = body[key_start..key_start + lower_length].to_vec();
  let upper = body[key_start + lower_length..next].to_vec();
  validate_descriptor_keys(profile, role, &lower, &upper)?;
  *cursor = next;
  Ok(DirectoryEntry { lower, upper, live, tombstones, pages, logical_bytes, minimum_page_id, maximum_page_id })
}

fn validate_physical_hints(body: &[u8], offset: usize) -> Result<(), &'static str> {
  read_u64(body, offset)?;
  read_u32(body, offset + 8)?;
  let reserved = read_u32(body, offset + 12)?;
  read_u64(body, offset + 16)?;
  if reserved != 0 {
    return Err("artifact_directory_physical_hint");
  }
  Ok(())
}

#[cfg(test)]
fn physical_hint_matches(body: &[u8], offset: usize, wal_offset: u64, total_length: u32, write_sequence: u64) -> bool {
  total_length != 0
    && read_u64(body, offset).ok() == Some(wal_offset)
    && read_u32(body, offset + 8).ok() == Some(total_length)
    && read_u32(body, offset + 12).ok() == Some(0)
    && read_u64(body, offset + 16).ok() == Some(write_sequence)
}

fn validate_descriptor_keys(profile: HashProfile, role: DirectoryRole, lower: &[u8], upper: &[u8]) -> Result<(), &'static str> {
  validate_key(profile, role, lower)?;
  validate_key(profile, role, upper)?;
  if compare_keys(profile, role, lower, upper)? == Ordering::Greater {
    return Err("artifact_directory_descriptor_order");
  }
  Ok(())
}

fn checked_sum(mut values: impl Iterator<Item = u64>) -> Result<u64, &'static str> {
  values.try_fold(0u64, |total, value| total.checked_add(value).ok_or("artifact_directory_aggregate_overflow"))
}

#[derive(Debug)]
struct DecodedPage {
  role: DirectoryRole,
  page_id: u64,
  record_count: u32,
  key: Vec<u8>,
}

fn decode_page(profile: HashProfile, bytes: &[u8]) -> Result<DecodedPage, &'static str> {
  let artifact = decode_immutable_value(profile, bytes, MAX_ARTIFACT_LENGTH)?;
  let h = profile.width();
  let (role, page_id) = match artifact.kind {
    POSTING_PAGE_KIND => decode_id_page_identity(profile, artifact.identity, DirectoryRole::Posting)?,
    VALUE_PAGE_KIND => decode_id_page_identity(profile, artifact.identity, DirectoryRole::Value)?,
    STATE_PAGE_KIND => decode_state_page_identity(profile, artifact.identity)?,
    SCOPE_PAGE_KIND => decode_scope_page_identity(profile, artifact.identity)?,
    _ => return Err("ordered_page_kind"),
  };
  if artifact.identity[..h].iter().all(|byte| *byte == 0) {
    return Err("ordered_page_owner");
  }
  let body = artifact.body;
  if body.len() < 96 {
    return Err("ordered_page_body_length");
  }
  let lower_length = read_u32(body, 24)? as usize;
  let upper_length = read_u32(body, 28)? as usize;
  let record_count = read_u32(body, 32)?;
  let live = read_u32(body, 36)?;
  let tombstones = read_u32(body, 40)?;
  let records_length = usize::try_from(read_u64(body, 48)?).map_err(|_| "ordered_page_records_length")?;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != role.key_codec()
    || (role != DirectoryRole::Posting && (read_u64(body, 8)? != 0 || read_u64(body, 16)? != 0))
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
    || record_count == 0
    || live.checked_add(tombstones) != Some(record_count)
    || read_u32(body, 44)? != 0
    || body[80..96].iter().any(|byte| *byte != 0)
    || 96usize
      .checked_add(lower_length)
      .and_then(|length| length.checked_add(upper_length))
      .and_then(|length| length.checked_add(records_length))
      != Some(body.len())
  {
    return Err("ordered_page_header");
  }
  let lower = body[96..96 + lower_length].to_vec();
  let upper = body[96 + lower_length..96 + lower_length + upper_length].to_vec();
  validate_descriptor_keys(profile, role, &lower, &upper)?;
  let records = &body[96 + lower_length + upper_length..];
  let decoded_records = decode_records(profile, role, records, record_count as usize)?;
  if decoded_records.first().map(|record| record.key.as_slice()) != Some(lower.as_slice())
    || decoded_records.last().map(|record| record.key.as_slice()) != Some(upper.as_slice())
  {
    return Err("ordered_page_fence");
  }
  let decoded_live = decoded_records.iter().filter(|record| !record.tombstone).count() as u32;
  let logical_live_bytes = decoded_records
    .iter()
    .filter(|record| !record.tombstone)
    .try_fold(0u64, |total, record| total.checked_add(record.length as u64).ok_or("ordered_page_logical_overflow"))?;
  if decoded_live != live || record_count - decoded_live != tombstones || read_u64(body, 56)? != logical_live_bytes {
    return Err("ordered_page_counts");
  }
  for pair in decoded_records.windows(2) {
    if compare_keys(profile, role, &pair[0].key, &pair[1].key)? != Ordering::Less {
      return Err("ordered_page_record_order");
    }
  }
  if role == DirectoryRole::Posting {
    let minimum = decoded_records.first().unwrap().coordinate;
    let maximum = decoded_records.last().unwrap().coordinate;
    if read_u64(body, 64)? != minimum || read_u64(body, 72)? != maximum || minimum > maximum {
      return Err("posting_page_coordinate_range");
    }
  } else if read_u64(body, 64)? != 0 || read_u64(body, 72)? != 0 {
    return Err("ordered_page_coordinate_reserved");
  }
  if artifact.kind == SCOPE_PAGE_KIND {
    let identity_fence = &artifact.identity[h + 1..];
    if identity_fence != lower {
      return Err("scope_page_identity_fence");
    }
  }
  Ok(DecodedPage { role, page_id, record_count, key: artifact.key })
}

fn decode_id_page_identity(profile: HashProfile, identity: &[u8], role: DirectoryRole) -> Result<(DirectoryRole, u64), &'static str> {
  let h = profile.width();
  if identity.len() != h + 8 {
    return Err("ordered_page_identity_length");
  }
  let page_id = read_u64(identity, h)?;
  if page_id == 0 {
    return Err("ordered_page_id");
  }
  Ok((role, page_id))
}

fn decode_state_page_identity(profile: HashProfile, identity: &[u8]) -> Result<(DirectoryRole, u64), &'static str> {
  let h = profile.width();
  if identity.len() != h + 16 || identity[h + 1..h + 8].iter().any(|byte| *byte != 0) {
    return Err("state_page_identity_length_or_reserve");
  }
  let role = match identity[h] {
    2 => DirectoryRole::ValueDocumentState,
    3 => DirectoryRole::IndexDocumentState,
    _ => return Err("state_page_owner_class"),
  };
  let page_id = read_u64(identity, h + 8)?;
  if page_id == 0 {
    return Err("state_page_id");
  }
  Ok((role, page_id))
}

fn decode_scope_page_identity(profile: HashProfile, identity: &[u8]) -> Result<(DirectoryRole, u64), &'static str> {
  let h = profile.width();
  if identity.len() < h + 1 {
    return Err("scope_page_identity_length");
  }
  let role = match identity[h] {
    1 if identity.len() == h + 9 => DirectoryRole::ScopeOrdinal,
    2 if identity.len() == 1 + 2 * h => DirectoryRole::ScopeReverse,
    _ => return Err("scope_page_role_or_identity"),
  };
  Ok((role, 0))
}

struct DecodedRecord {
  key: Vec<u8>,
  tombstone: bool,
  length: usize,
  coordinate: u64,
  document_ordinal: u64,
  file_key: Option<Vec<u8>>,
}

fn decode_records(profile: HashProfile, role: DirectoryRole, bytes: &[u8], count: usize) -> Result<Vec<DecodedRecord>, &'static str> {
  let mut cursor = 0usize;
  let mut records = Vec::new();
  for _ in 0..count {
    let record = match role {
      DirectoryRole::Posting => decode_posting_record(bytes, &mut cursor)?,
      DirectoryRole::Value => decode_value_record(profile, bytes, &mut cursor)?,
      DirectoryRole::ScopeOrdinal => decode_scope_ordinal_record(profile, bytes, &mut cursor)?,
      DirectoryRole::ScopeReverse => decode_scope_reverse_record(profile, bytes, &mut cursor)?,
      DirectoryRole::ValueDocumentState | DirectoryRole::IndexDocumentState => decode_state_record(profile, role, bytes, &mut cursor)?,
      DirectoryRole::NvtTile => return Err("nvt_tile_is_not_an_ordered_page"),
    };
    records.push(record);
  }
  if cursor != bytes.len() {
    return Err("ordered_page_record_count");
  }
  Ok(records)
}

fn record_flags(bytes: &[u8], cursor: usize) -> Result<bool, &'static str> {
  let flags = *bytes.get(cursor).ok_or("ordered_record_truncated")?;
  if flags & !1 != 0 || bytes.get(cursor + 1..cursor + 4).is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0)) {
    return Err("ordered_record_flags_or_reserve");
  }
  Ok(flags & 1 != 0)
}

fn decode_posting_record(bytes: &[u8], cursor: &mut usize) -> Result<DecodedRecord, &'static str> {
  let start = *cursor;
  let tombstone = record_flags(bytes, start)?;
  let key_length = read_u32(bytes, start + 4)? as usize;
  if key_length == 0 || key_length > MAX_KEY_LENGTH {
    return Err("posting_record_key_length");
  }
  let end = start.checked_add(32).and_then(|value| value.checked_add(key_length)).ok_or("posting_record_overflow")?;
  if end > bytes.len() {
    return Err("posting_record_truncated");
  }
  let coordinate = read_u64(bytes, start + 8)?;
  let document_ordinal = read_u64(bytes, start + 16)?;
  let key = posting_position(&bytes[start..end]);
  *cursor = end;
  Ok(DecodedRecord { key, tombstone, length: end - start, coordinate, document_ordinal, file_key: None })
}

fn decode_value_record(profile: HashProfile, bytes: &[u8], cursor: &mut usize) -> Result<DecodedRecord, &'static str> {
  let h = profile.width();
  let start = *cursor;
  let tombstone = record_flags(bytes, start)?;
  let value_length = read_u32(bytes, start + 4)? as usize;
  let end = start.checked_add(24 + h).and_then(|value| value.checked_add(value_length)).ok_or("value_record_overflow")?;
  if end > bytes.len() || bytes[start + 20..start + 24].iter().any(|byte| *byte != 0) {
    return Err("value_record_truncated_or_reserve");
  }
  let revision = &bytes[start + 24..start + 24 + h];
  if revision.iter().all(|byte| *byte == 0) || tombstone != (value_length == 0) {
    return Err("value_record_revision_or_tombstone");
  }
  if !tombstone {
    crate::config::validate_source_value(&bytes[start + 24 + h..end]).map_err(|_| "value_record_canonical_value")?;
  }
  let mut key = Vec::with_capacity(12);
  key.extend_from_slice(&bytes[start + 8..start + 20]);
  let document_ordinal = read_u64(bytes, start + 8)?;
  *cursor = end;
  Ok(DecodedRecord { key, tombstone, length: end - start, coordinate: 0, document_ordinal, file_key: None })
}

fn decode_scope_ordinal_record(profile: HashProfile, bytes: &[u8], cursor: &mut usize) -> Result<DecodedRecord, &'static str> {
  let h = profile.width();
  let start = *cursor;
  let tombstone = record_flags(bytes, start)?;
  let path_length = read_u32(bytes, start + 4)? as usize;
  if path_length == 0 || path_length > MAX_KEY_LENGTH {
    return Err("scope_ordinal_path_length");
  }
  let end = start.checked_add(16 + 2 * h).and_then(|value| value.checked_add(path_length)).ok_or("scope_ordinal_overflow")?;
  if end > bytes.len() {
    return Err("scope_ordinal_truncated");
  }
  let path = std::str::from_utf8(&bytes[start + 16 + 2 * h..end]).map_err(|_| "scope_ordinal_path_utf8")?;
  if !definitions::is_canonical_absolute_path(path)
    || definitions::file_key(profile, path).map_err(|_| "scope_ordinal_path")? != bytes[start + 16..start + 16 + h]
    || bytes[start + 16 + h..start + 16 + 2 * h].iter().all(|byte| *byte == 0)
  {
    return Err("scope_ordinal_identity");
  }
  let document_ordinal = read_u64(bytes, start + 8)?;
  let key = bytes[start + 8..start + 16].to_vec();
  let file_key = bytes[start + 16..start + 16 + h].to_vec();
  *cursor = end;
  Ok(DecodedRecord { key, tombstone, length: end - start, coordinate: 0, document_ordinal, file_key: Some(file_key) })
}

fn decode_scope_reverse_record(profile: HashProfile, bytes: &[u8], cursor: &mut usize) -> Result<DecodedRecord, &'static str> {
  let h = profile.width();
  let start = *cursor;
  let tombstone = record_flags(bytes, start)?;
  let end = start.checked_add(12 + h).ok_or("scope_reverse_overflow")?;
  if end > bytes.len() || tombstone || bytes[start + 12..end].iter().all(|byte| *byte == 0) {
    return Err("scope_reverse_record");
  }
  let document_ordinal = read_u64(bytes, start + 4)?;
  let key = bytes[start + 12..end].to_vec();
  *cursor = end;
  Ok(DecodedRecord { key: key.clone(), tombstone: false, length: end - start, coordinate: 0, document_ordinal, file_key: Some(key) })
}

fn decode_state_record(profile: HashProfile, role: DirectoryRole, bytes: &[u8], cursor: &mut usize) -> Result<DecodedRecord, &'static str> {
  let h = profile.width();
  let start = *cursor;
  let tombstone = *bytes.get(start).ok_or("state_record_truncated")? & 1 != 0;
  if *bytes.get(start).ok_or("state_record_truncated")? & !1 != 0 {
    return Err("state_record_flags");
  }
  let stage = *bytes.get(start + 1).ok_or("state_record_truncated")?;
  let reason = read_u16(bytes, start + 2)?;
  let evidence_length = read_u32(bytes, start + 4)? as usize;
  let end = start.checked_add(48 + h).and_then(|value| value.checked_add(evidence_length)).ok_or("state_record_overflow")?;
  if end > bytes.len()
    || evidence_length > MAX_EVIDENCE_LENGTH
    || bytes[start + 16..start + 16 + h].iter().all(|byte| *byte == 0)
    || read_u32(bytes, start + 44 + h)? != 0
    || !valid_state_reason(role, stage, reason)
  {
    return Err("state_record_semantics");
  }
  if evidence_length == 0 || crate::config::validate(&bytes[start + 48 + h..end]).is_err() {
    return Err("state_record_evidence");
  }
  let key = bytes[start + 8..start + 16].to_vec();
  let document_ordinal = read_u64(bytes, start + 8)?;
  *cursor = end;
  Ok(DecodedRecord { key, tombstone, length: end - start, coordinate: 0, document_ordinal, file_key: None })
}

fn validate_scope_catalog_pair(profile: HashProfile, ordinal_page: &[u8], reverse_page: &[u8]) -> Result<(), &'static str> {
  let ordinal_artifact = decode_immutable_value(profile, ordinal_page, MAX_ARTIFACT_LENGTH)?;
  let reverse_artifact = decode_immutable_value(profile, reverse_page, MAX_ARTIFACT_LENGTH)?;
  let h = profile.width();
  if ordinal_artifact.kind != SCOPE_PAGE_KIND
    || reverse_artifact.kind != SCOPE_PAGE_KIND
    || ordinal_artifact.identity[..h] != reverse_artifact.identity[..h]
  {
    return Err("scope_catalog_pair_owner");
  }
  decode_page(profile, ordinal_page)?;
  decode_page(profile, reverse_page)?;
  let ordinal_records = page_records(profile, DirectoryRole::ScopeOrdinal, ordinal_artifact.body)?;
  let reverse_records = page_records(profile, DirectoryRole::ScopeReverse, reverse_artifact.body)?;
  let mut ordinal_live = BTreeMap::new();
  for record in ordinal_records.into_iter().filter(|record| !record.tombstone) {
    if ordinal_live.insert(record.file_key.expect("scope ordinal record has FileKey"), record.document_ordinal).is_some() {
      return Err("scope_catalog_pair_duplicate_file_key");
    }
  }
  let mut reverse_live = BTreeMap::new();
  for record in reverse_records {
    if reverse_live.insert(record.file_key.expect("scope reverse record has FileKey"), record.document_ordinal).is_some() {
      return Err("scope_catalog_pair_duplicate_file_key");
    }
  }
  if ordinal_live != reverse_live {
    return Err("scope_catalog_pair_bijection");
  }
  Ok(())
}

fn page_records(profile: HashProfile, role: DirectoryRole, body: &[u8]) -> Result<Vec<DecodedRecord>, &'static str> {
  let lower_length = read_u32(body, 24)? as usize;
  let upper_length = read_u32(body, 28)? as usize;
  let record_count = read_u32(body, 32)? as usize;
  let start = 96usize.checked_add(lower_length).and_then(|value| value.checked_add(upper_length)).ok_or("ordered_page_record_offset")?;
  decode_records(profile, role, body.get(start..).ok_or("ordered_page_record_offset")?, record_count)
}

fn valid_state_reason(role: DirectoryRole, stage: u8, reason: u16) -> bool {
  match role {
    DirectoryRole::ValueDocumentState => {
      matches!((stage, reason), (1, 0x0001..=0x0003) | (2, 0x0005..=0x0008) | (3, 0x0002 | 0x0004 | 0x0007 | 0x0008) | (4, 0x0007..=0x000b))
    }
    DirectoryRole::IndexDocumentState => matches!((stage, reason), (5, 0x0009..=0x000c | 0x000e | 0x000f) | (6, 0x0002 | 0x000d..=0x000f)),
    _ => false,
  }
}

fn validate_key(profile: HashProfile, role: DirectoryRole, key: &[u8]) -> Result<(), &'static str> {
  let valid = match role {
    DirectoryRole::ScopeOrdinal | DirectoryRole::ValueDocumentState | DirectoryRole::IndexDocumentState => key.len() == 8,
    DirectoryRole::ScopeReverse => key.len() == profile.width(),
    DirectoryRole::Value => key.len() == 12,
    DirectoryRole::Posting => key.len() >= 25,
    DirectoryRole::NvtTile => key.len() == 8,
  };
  if !valid {
    return Err("artifact_key_codec_length");
  }
  Ok(())
}

fn compare_keys(profile: HashProfile, role: DirectoryRole, left: &[u8], right: &[u8]) -> Result<Ordering, &'static str> {
  validate_key(profile, role, left)?;
  validate_key(profile, role, right)?;
  Ok(match role {
    DirectoryRole::ScopeOrdinal | DirectoryRole::ValueDocumentState | DirectoryRole::IndexDocumentState | DirectoryRole::NvtTile => {
      read_u64(left, 0)?.cmp(&read_u64(right, 0)?)
    }
    DirectoryRole::ScopeReverse => left.cmp(right),
    DirectoryRole::Value => {
      read_u64(left, 0)?.cmp(&read_u64(right, 0)?).then_with(|| read_u32(left, 8).unwrap().cmp(&read_u32(right, 8).unwrap()))
    }
    DirectoryRole::Posting => compare_posting_positions(left, right)?,
  })
}

fn compare_posting_positions(left: &[u8], right: &[u8]) -> Result<Ordering, &'static str> {
  let left_key_end = left.len() - 16;
  let right_key_end = right.len() - 16;
  Ok(
    read_u64(left, 0)?
      .cmp(&read_u64(right, 0)?)
      .then_with(|| left[8..left_key_end].cmp(&right[8..right_key_end]))
      .then_with(|| read_u64(left, left_key_end).unwrap().cmp(&read_u64(right, right_key_end).unwrap()))
      .then_with(|| read_u32(left, left_key_end + 8).unwrap().cmp(&read_u32(right, right_key_end + 8).unwrap()))
      .then_with(|| read_u32(left, left_key_end + 12).unwrap().cmp(&read_u32(right, right_key_end + 12).unwrap())),
  )
}

fn compare_posting_records(left: &[u8], right: &[u8]) -> Result<Ordering, &'static str> {
  compare_posting_positions(&posting_position(left), &posting_position(right))
}

fn directory_expected(
  role: DirectoryRole,
  level: u16,
  entries: u32,
  live: u64,
  pages: u64,
  lower_length: usize,
  upper_length: usize,
) -> &'static str {
  leak(format!(
    "index:directory:{}:level={level}:entries={entries}:live={live}:pages={pages}:fences={lower_length}/{upper_length}",
    role.name()
  ))
}

fn page_expected(role: DirectoryRole, page_id: u64, records: u32) -> &'static str {
  let value = match role {
    DirectoryRole::ScopeOrdinal => format!("index:page:scope-catalog:ordinal:records={records}"),
    DirectoryRole::ScopeReverse => format!("index:page:scope-catalog:reverse:records={records}"),
    DirectoryRole::Value => format!("index:page:value:page-id={page_id}:records={records}"),
    DirectoryRole::ValueDocumentState => format!("index:page:document-state:value-store:page-id={page_id}:records={records}"),
    DirectoryRole::Posting => format!("index:page:posting:page-id={page_id}:records={records}"),
    DirectoryRole::IndexDocumentState => format!("index:page:document-state:index:page-id={page_id}:records={records}"),
    DirectoryRole::NvtTile => unreachable!("NVT tiles use their own body"),
  };
  leak(value)
}

fn page_relation(role: DirectoryRole) -> &'static str {
  match role {
    DirectoryRole::ScopeOrdinal => "page:ScopeCatalogPageV1:ordinal-map",
    DirectoryRole::ScopeReverse => "page:ScopeCatalogPageV1:reverse-map",
    DirectoryRole::Value => "page:ValuePageV1",
    DirectoryRole::ValueDocumentState => "page:DocumentStatePageV1:ValueStoreId",
    DirectoryRole::Posting => "page:PostingPageV1",
    DirectoryRole::IndexDocumentState => "page:DocumentStatePageV1:IndexId",
    DirectoryRole::NvtTile => unreachable!("NVT tiles use their own body"),
  }
}

fn directory_relation(role: DirectoryRole, level: &str) -> &'static str {
  leak(format!("directory:{}:{level}", role.name()))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::index::write_trailing_crc;

  #[test]
  fn ordered_artifact_fixtures_decode_with_exact_keys() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn directory_leaf_references_the_exact_role_page_and_internal_child() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      for page in sample_pages(profile) {
        let leaf_bytes = build_leaf_directory(profile, &page);
        let leaf = decode_directory(profile, &leaf_bytes).unwrap();
        let leaf_artifact = decode_immutable_value(profile, &leaf_bytes, MAX_ARTIFACT_LENGTH).unwrap();
        let expected_page_hash = immutable_key(profile, page.role.child_kind(), &page.bytes);
        let h = profile.width();
        let lower_length = read_u32(leaf_artifact.body, 16).unwrap() as usize;
        let upper_length = read_u32(leaf_artifact.body, 20).unwrap() as usize;
        let descriptor = 80 + lower_length + upper_length;
        assert_eq!(&leaf_artifact.body[descriptor + 16..descriptor + 16 + h], expected_page_hash);
        assert_eq!(leaf.lower, page.lower);
        assert_eq!(leaf.upper, page.upper);

        if page.role == DirectoryRole::Posting {
          let internal_bytes = build_internal_directory(profile, &page, &leaf_bytes);
          let internal = decode_directory(profile, &internal_bytes).unwrap();
          let internal_artifact = decode_immutable_value(profile, &internal_bytes, MAX_ARTIFACT_LENGTH).unwrap();
          let descriptor = 80 + page.lower.len() + page.upper.len();
          assert_eq!(&internal_artifact.body[descriptor + 8..descriptor + 8 + h], leaf_artifact.key);
          assert_eq!(internal.live, leaf.live);
          assert_eq!(internal.pages, leaf.pages);
        }
      }
    }
  }

  #[test]
  fn directory_and_page_semantic_corruption_fails_after_crc_repair() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let pages = sample_pages(profile);
      let posting = pages.iter().find(|page| page.role == DirectoryRole::Posting).unwrap();
      let directory = build_leaf_directory(profile, posting);
      let body_offset = 32 + profile.width() + 2;

      for offset in [body_offset + 2, body_offset + 24, body_offset + 40, body_offset + 72] {
        let mut changed = directory.clone();
        changed[offset] ^= 1;
        write_trailing_crc(&mut changed);
        assert!(decode_directory(profile, &changed).is_err(), "directory offset {offset} accepted");
      }

      let mut changed = posting.bytes.clone();
      let posting_body = 32 + profile.width() + 8;
      changed[posting_body + 36] ^= 1;
      write_trailing_crc(&mut changed);
      assert!(decode_page(profile, &changed).is_err());

      let scope = pages.iter().find(|page| page.role == DirectoryRole::ScopeOrdinal).unwrap();
      let mut changed = scope.bytes.clone();
      let scope_identity_length = profile.width() + 9;
      let scope_body = 32 + scope_identity_length;
      let lower_length = read_u32(&changed, scope_body + 24).unwrap() as usize;
      let upper_length = read_u32(&changed, scope_body + 28).unwrap() as usize;
      let first_record = scope_body + 96 + lower_length + upper_length;
      changed[first_record + 16] ^= 1;
      write_trailing_crc(&mut changed);
      assert!(decode_page(profile, &changed).is_err());

      let state = pages.iter().find(|page| page.role == DirectoryRole::ValueDocumentState).unwrap();
      let mut changed = state.bytes.clone();
      let state_body = 32 + profile.width() + 16;
      let lower_length = read_u32(&changed, state_body + 24).unwrap() as usize;
      let upper_length = read_u32(&changed, state_body + 28).unwrap() as usize;
      let first_record = state_body + 96 + lower_length + upper_length;
      changed[first_record + 1] = 5;
      write_trailing_crc(&mut changed);
      assert!(decode_page(profile, &changed).is_err());
    }
  }

  #[test]
  fn future_child_generation_fails_but_partial_physical_hints_only_disable_coalescing() {
    let profile = HashProfile::Blake3_256;
    let posting = sample_pages(profile).into_iter().find(|page| page.role == DirectoryRole::Posting).unwrap();
    let baseline = build_leaf_directory(profile, &posting);
    let artifact = decode_immutable_value(profile, &baseline, MAX_ARTIFACT_LENGTH).unwrap();
    let body_offset = artifact.body.as_ptr() as usize - baseline.as_ptr() as usize;
    let descriptor = body_offset + 80 + posting.lower.len() + posting.upper.len();
    let fields = descriptor + 16 + profile.width();

    let mut future = baseline.clone();
    put_u64(&mut future, fields, posting.generation + 100);
    write_trailing_crc(&mut future);
    assert!(decode_directory(profile, &future).is_err());

    let mut partial_hint = baseline;
    put_u64(&mut partial_hint, fields + 32, 1234);
    write_trailing_crc(&mut partial_hint);
    assert!(decode_directory(profile, &partial_hint).is_ok());
    let hint_offset = fields + 32;
    assert!(!physical_hint_matches(&partial_hint, hint_offset, 1234, 0, 0));

    put_u32(&mut partial_hint, hint_offset + 8, 4096);
    put_u64(&mut partial_hint, hint_offset + 16, 77);
    write_trailing_crc(&mut partial_hint);
    assert!(decode_directory(profile, &partial_hint).is_ok());
    assert!(physical_hint_matches(&partial_hint, hint_offset, 1234, 4096, 77));
    assert!(!physical_hint_matches(&partial_hint, hint_offset, 1235, 4096, 77));
  }

  #[test]
  fn source_value_codec_accepts_typed_small_u64_and_larger_values_than_config_codec() {
    let mut small_u64 = vec![0x05];
    small_u64.extend_from_slice(&8u32.to_le_bytes());
    small_u64.extend_from_slice(&1u64.to_le_bytes());
    assert!(crate::config::validate(&small_u64).is_err());
    assert!(crate::config::validate_source_value(&small_u64).is_ok());

    let payload = vec![b'x'; 128 * 1_024];
    let mut source = vec![0x08];
    source.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    source.extend_from_slice(&payload);
    assert!(crate::config::validate(&source).is_err());
    assert!(crate::config::validate_source_value(&source).is_ok());
  }

  #[test]
  fn scope_catalog_directions_require_an_exact_live_bijection() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let pages = sample_pages(profile);
      let ordinal = &pages.iter().find(|page| page.role == DirectoryRole::ScopeOrdinal).unwrap().bytes;
      let reverse = &pages.iter().find(|page| page.role == DirectoryRole::ScopeReverse).unwrap().bytes;
      assert!(validate_scope_catalog_pair(profile, ordinal, reverse).is_ok());

      let mut changed = reverse.clone();
      let body_offset = 32 + 1 + 2 * profile.width();
      let lower_length = read_u32(&changed, body_offset + 24).unwrap() as usize;
      let upper_length = read_u32(&changed, body_offset + 28).unwrap() as usize;
      let first_record = body_offset + 96 + lower_length + upper_length;
      put_u64(&mut changed, first_record + 4, 99);
      write_trailing_crc(&mut changed);
      assert!(decode_page(profile, &changed).is_ok());
      assert_eq!(validate_scope_catalog_pair(profile, ordinal, &changed), Err("scope_catalog_pair_bijection"));
    }
  }

  #[test]
  fn role_comparators_decode_structural_little_endian_keys_instead_of_sorting_raw_bytes() {
    let profile = HashProfile::Blake3_256;
    let low = 255u64.to_le_bytes();
    let high = 256u64.to_le_bytes();
    assert_eq!(low.as_slice().cmp(high.as_slice()), Ordering::Greater);
    assert_eq!(compare_keys(profile, DirectoryRole::ScopeOrdinal, &low, &high), Ok(Ordering::Less));

    let mut value_low = low.to_vec();
    value_low.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut value_high = high.to_vec();
    value_high.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(compare_keys(profile, DirectoryRole::Value, &value_low, &value_high), Ok(Ordering::Less));

    let mut posting_low = low.to_vec();
    posting_low.push(b'z');
    posting_low.extend_from_slice(&u64::MAX.to_le_bytes());
    posting_low.extend_from_slice(&u32::MAX.to_le_bytes());
    posting_low.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut posting_high = high.to_vec();
    posting_high.push(b'a');
    posting_high.extend_from_slice(&0u64.to_le_bytes());
    posting_high.extend_from_slice(&0u32.to_le_bytes());
    posting_high.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(compare_keys(profile, DirectoryRole::Posting, &posting_low, &posting_high), Ok(Ordering::Less));
  }
}
