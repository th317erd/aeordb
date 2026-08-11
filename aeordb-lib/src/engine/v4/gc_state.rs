use std::cmp::Ordering;

use super::gc::{
  GcArtifactKindV1, PhysicalIncarnationV1, decode_gc_artifact_envelope, decode_physical_incarnation, immutable_gc_artifact_key, u16_at,
  u32_at, u64_at,
};
use super::contract_generated::root_retirement_reason_v1;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAX_MANIFEST_LENGTH: usize = 1_024 * 1_024;
const MAX_PAGE_LENGTH: usize = 16 * 1_024 * 1_024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_DELTAS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum GcDirectoryRoleV1 {
  Candidates = 1,
  RootExpiry = 2,
  PhysicalInventory = 3,
  RootCandidates = 8,
}

impl GcDirectoryRoleV1 {
  pub fn directory_name(self) -> &'static str {
    match self {
      Self::Candidates => "candidates",
      Self::RootExpiry => "root-expiry",
      Self::PhysicalInventory => "physical-inventory",
      Self::RootCandidates => "root-candidates",
    }
  }

  pub fn page_name(self) -> &'static str {
    match self {
      Self::Candidates => "candidate",
      Self::RootExpiry => "root-expiry",
      Self::PhysicalInventory => "physical-inventory",
      Self::RootCandidates => "root-candidate",
    }
  }

  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Candidates),
      2 => Ok(Self::RootExpiry),
      3 => Ok(Self::PhysicalInventory),
      8 => Ok(Self::RootCandidates),
      _ => Err(kind_error("gc_directory_role", format!("unknown GC directory role {value}"))),
    }
  }
}

#[derive(Debug, Clone)]
pub struct GcStatePageV1<'a> {
  pub role: GcDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub record_count: u32,
  pub logical_bytes: u64,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub records: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GcStateDirectoryV1<'a> {
  pub role: GcDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub record_count: u64,
  pub logical_bytes: u64,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub child_hash: &'a [u8],
  pub child_generation: u64,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GcStateManifestV1<'a> {
  pub kind: GcArtifactKindV1,
  pub database_id: &'a [u8],
  pub generation: u64,
  pub populated: bool,
  pub record_count: u64,
  pub secondary_count: u64,
  pub primary_root: &'a [u8],
  pub secondary_root: Option<&'a [u8]>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum GcStateArtifactV1<'a> {
  Page(GcStatePageV1<'a>),
  Directory(GcStateDirectoryV1<'a>),
  CandidateDelta { record_count: u32, key: Vec<u8> },
  Manifest(GcStateManifestV1<'a>),
  RootRetirementCommit { mark_generation: u64, key: Vec<u8> },
  RootObjectReclaimProof { incarnation_count: u64, receipt_count: u64, key: Vec<u8> },
  RetirementJournal { record_count: u32, key: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCandidateRecordV1<'a> {
  pub namespace_root_hash: &'a [u8],
  pub reason: u16,
  pub pending_since_ms: i64,
  pub first_unreachable_generation: u64,
  pub last_confirmed_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  pub admission_commit_payload_hash: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootExpiryStateV1 {
  LogicallyRetired,
  PhysicallyReclaimed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryRecordV1<'a> {
  pub namespace_root_hash: &'a [u8],
  pub retired_at_ms: i64,
  pub last_pending_since_ms: i64,
  pub final_mark_generation: u64,
  pub reason: u16,
  pub state: RootExpiryStateV1,
  pub retirement_commit_hash: &'a [u8],
  pub root_object_reclaim_proof_hash: Option<&'a [u8]>,
  pub evidence_expires_at_ms: Option<i64>,
}

impl GcStateArtifactV1<'_> {
  pub fn key(&self) -> &[u8] {
    match self {
      Self::Page(value) => &value.key,
      Self::Directory(value) => &value.key,
      Self::CandidateDelta { key, .. }
      | Self::RootRetirementCommit { key, .. }
      | Self::RootObjectReclaimProof { key, .. }
      | Self::RetirementJournal { key, .. } => key,
      Self::Manifest(value) => &value.key,
    }
  }

  pub fn summary(&self) -> String {
    match self {
      Self::Page(page) => format!("gc:page:{}:page={}:records={}", page.role.page_name(), page.page_id, page.record_count),
      Self::Directory(directory) => {
        format!("gc:directory:{}:level=0:records={}", directory.role.directory_name(), directory.record_count)
      }
      Self::CandidateDelta { record_count, .. } => format!("gc:delta:candidate:records={record_count}"),
      Self::Manifest(manifest) => manifest_summary(manifest),
      Self::RootRetirementCommit { mark_generation, .. } => format!("gc:commit:root-retirement:mark={mark_generation}"),
      Self::RootObjectReclaimProof { incarnation_count, receipt_count, .. } => {
        format!("gc:proof:root-object-reclaim:incarnations={incarnation_count}:receipts={receipt_count}")
      }
      Self::RetirementJournal { record_count, .. } => format!("gc:journal:retirement:records={record_count}"),
    }
  }
}

pub fn decode_gc_state_artifact(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<GcStateArtifactV1<'_>> {
  let hinted_kind = u16_at(bytes, 6).ok().and_then(GcArtifactKindV1::from_u16);
  let hard_cap = match hinted_kind {
    Some(GcArtifactKindV1::GcArtifactDirectoryNode) => MAX_DIRECTORY_LENGTH,
    Some(
      GcArtifactKindV1::CandidatePage
      | GcArtifactKindV1::RootExpiryPage
      | GcArtifactKindV1::PhysicalInventoryPage
      | GcArtifactKindV1::RootCandidatePage
      | GcArtifactKindV1::RetirementJournalSegment,
    ) => MAX_PAGE_LENGTH,
    Some(
      GcArtifactKindV1::RootExpiryCatalogManifest
      | GcArtifactKindV1::PhysicalInventoryManifest
      | GcArtifactKindV1::QuarantineManifest
      | GcArtifactKindV1::RootLifecycleManifest
      | GcArtifactKindV1::RootRetirementCommit
      | GcArtifactKindV1::RootObjectReclaimProof,
    ) => MAX_MANIFEST_LENGTH,
    _ => super::gc::MAX_GC_ARTIFACT_LENGTH,
  };
  if bytes.len() > hard_cap {
    return Err(amplification_error("gc_state_artifact_length", bytes.len(), hard_cap));
  }
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let key = immutable_gc_artifact_key(algorithm, envelope.kind, bytes);
  match envelope.kind {
    GcArtifactKindV1::CandidatePage
    | GcArtifactKindV1::RootExpiryPage
    | GcArtifactKindV1::PhysicalInventoryPage
    | GcArtifactKindV1::RootCandidatePage => decode_page(bytes, algorithm, key).map(GcStateArtifactV1::Page),
    GcArtifactKindV1::GcArtifactDirectoryNode => decode_directory(bytes, algorithm, key).map(GcStateArtifactV1::Directory),
    GcArtifactKindV1::CandidateDelta => decode_candidate_delta(bytes, algorithm, key),
    GcArtifactKindV1::RootExpiryCatalogManifest
    | GcArtifactKindV1::PhysicalInventoryManifest
    | GcArtifactKindV1::QuarantineManifest
    | GcArtifactKindV1::RootLifecycleManifest => decode_manifest(bytes, algorithm, key).map(GcStateArtifactV1::Manifest),
    GcArtifactKindV1::RootRetirementCommit => decode_root_retirement_commit(bytes, algorithm, key),
    GcArtifactKindV1::RootObjectReclaimProof => decode_root_object_reclaim_proof(bytes, algorithm, key),
    GcArtifactKindV1::RetirementJournalSegment => decode_retirement_journal(bytes, algorithm, key),
    _ => Err(kind_error("gc_state_kind", format!("{} is not a lifecycle/inventory artifact", envelope.kind.name()))),
  }
}

pub fn validate_gc_directory_page(directory: &GcStateDirectoryV1<'_>, page: &GcStatePageV1<'_>) -> FormatResult<()> {
  if directory.role != page.role
    || directory.database_id != page.database_id
    || directory.catalog_id != page.catalog_id
    || directory.page_id != page.page_id
    || directory.child_hash != page.key
    || directory.child_generation != page.generation
    || directory.record_count != u64::from(page.record_count)
    || directory.logical_bytes != page.logical_bytes
    || directory.lower_fence != page.lower_fence
    || directory.upper_fence != page.upper_fence
  {
    return Err(closure_error("gc_directory_page_closure", "GC directory descriptor does not match its immutable page"));
  }
  Ok(())
}

fn decode_page(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStatePageV1<'_>> {
  if bytes.len() > MAX_PAGE_LENGTH {
    return Err(amplification_error("gc_page_length", bytes.len(), MAX_PAGE_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let role = match artifact.kind {
    GcArtifactKindV1::CandidatePage => GcDirectoryRoleV1::Candidates,
    GcArtifactKindV1::RootExpiryPage => GcDirectoryRoleV1::RootExpiry,
    GcArtifactKindV1::PhysicalInventoryPage => GcDirectoryRoleV1::PhysicalInventory,
    GcArtifactKindV1::RootCandidatePage => GcDirectoryRoleV1::RootCandidates,
    _ => return Err(kind_error("gc_page_kind", "artifact is not a GC state page")),
  };
  if artifact.identity.len() != 42 {
    return Err(closure_error("gc_page_identity", "GC page identity must be 42 bytes"));
  }
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  if all_zero(database_id) || all_zero(catalog_id) || u16_at(artifact.identity, 32)? != role as u16 {
    return Err(identity_error("gc_page_identity", "GC page database/catalog/role identity is invalid"));
  }
  let page_id = u64_at(artifact.identity, 34)?;
  let body = artifact.body;
  if page_id == 0 || body.len() < 64 {
    return Err(identity_error("gc_page_identity_or_length", "GC page ID is zero or body is truncated"));
  }
  if u32_at(body, 0)? != 0 || body[40..64].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("gc_page_header", "GC page reserve fields must be zero"));
  }
  let lower_length = usize::try_from(u32_at(body, 8)?).map_err(|_| length_error("GC page lower-fence length"))?;
  let upper_length = usize::try_from(u32_at(body, 12)?).map_err(|_| length_error("GC page upper-fence length"))?;
  let record_count = u32_at(body, 16)?;
  let records_length = usize::try_from(u64_at(body, 24)?).map_err(|_| length_error("GC page records length"))?;
  if u16_at(body, 4)? != 1
    || u16_at(body, 6)? != role as u16
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
    || record_count == 0
    || u32_at(body, 20)? != record_count
    || u64_at(body, 32)? != records_length as u64
  {
    return Err(closure_error("gc_page_header", "GC page codec, role, fences, counts, or logical length is invalid"));
  }
  let records_start = 64usize
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("GC page fence end overflow"))?;
  if records_start.checked_add(records_length) != Some(body.len()) {
    return Err(trailing_error("gc_page_header", "GC page lengths do not consume body exactly"));
  }
  let lower_fence = &body[64..64 + lower_length];
  let upper_fence = &body[64 + lower_length..records_start];
  compare_fences(algorithm, role, lower_fence, upper_fence)?;
  let records = &body[records_start..];
  let row_length = row_length(algorithm, role);
  let expected_records_length = usize::try_from(record_count)
    .ok()
    .and_then(|count| count.checked_mul(row_length))
    .ok_or_else(|| length_error("GC page record-count multiplication overflow"))?;
  if expected_records_length != records.len() {
    return Err(closure_error("gc_page_records_length", "GC page record count does not match fixed records length"));
  }
  let mut previous: Option<&[u8]> = None;
  for row in records.chunks_exact(row_length) {
    validate_row(algorithm, role, row, false)?;
    if let Some(prior) = previous {
      if compare_rows(algorithm, role, prior, row)? != Ordering::Less {
        return Err(order_error("gc_page_record_order", "GC page rows are not strictly ordered"));
      }
    }
    previous = Some(row);
  }
  let first = records.get(..row_length).ok_or_else(|| trailing_error("gc_page_lower", "GC page has no first row"))?;
  let last = records.get(records.len() - row_length..).ok_or_else(|| trailing_error("gc_page_upper", "GC page has no last row"))?;
  if !row_key_equals_fence(algorithm, role, first, lower_fence)? || !row_key_equals_fence(algorithm, role, last, upper_fence)? {
    return Err(closure_error("gc_page_fences", "GC page fences do not match first/last record keys"));
  }
  Ok(GcStatePageV1 {
    role,
    database_id,
    catalog_id,
    generation: artifact.generation,
    page_id,
    record_count,
    logical_bytes: records_length as u64,
    lower_fence,
    upper_fence,
    records,
    key,
  })
}

fn decode_directory(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateDirectoryV1<'_>> {
  if bytes.len() > MAX_DIRECTORY_LENGTH {
    return Err(amplification_error("gc_directory_length", bytes.len(), MAX_DIRECTORY_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::GcArtifactDirectoryNode || artifact.identity.len() != 34 {
    return Err(closure_error("gc_directory_identity", "GC directory kind/identity length is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  let role = GcDirectoryRoleV1::from_u16(u16_at(artifact.identity, 32)?)?;
  if all_zero(database_id) || all_zero(catalog_id) || artifact.body.len() < 80 {
    return Err(identity_error("gc_directory_identity", "GC directory database/catalog identity is zero or body is truncated"));
  }
  let body = artifact.body;
  if u16_at(body, 0)? != 0 || u32_at(body, 8)? != 0 || u32_at(body, 12)? != 0 || u32_at(body, 76)? != 0 {
    return Err(reserved_error("gc_directory_header", "GC directory reserve fields must be zero"));
  }
  let lower_length = usize::try_from(u32_at(body, 16)?).map_err(|_| length_error("GC directory lower length"))?;
  let upper_length = usize::try_from(u32_at(body, 20)?).map_err(|_| length_error("GC directory upper length"))?;
  let entries_length = usize::try_from(u32_at(body, 72)?).map_err(|_| length_error("GC directory entries length"))?;
  if u16_at(body, 2)? != role as u16
    || u32_at(body, 4)? != 1
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
  {
    return Err(closure_error("gc_directory_header", "GC directory role, count, or fence length is invalid"));
  }
  let descriptor_start = 80usize
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("GC directory descriptor start overflow"))?;
  if descriptor_start.checked_add(entries_length) != Some(body.len()) {
    return Err(trailing_error("gc_directory_header", "GC directory lengths do not consume body exactly"));
  }
  let lower_fence = &body[80..80 + lower_length];
  let upper_fence = &body[80 + lower_length..descriptor_start];
  compare_fences(algorithm, role, lower_fence, upper_fence)?;
  let fixed = 72usize.checked_add(algorithm.hash_length()).ok_or_else(|| length_error("GC descriptor width overflow"))?;
  if entries_length != fixed + lower_length + upper_length
    || usize::try_from(u32_at(body, descriptor_start)?).ok() != Some(lower_length)
    || usize::try_from(u32_at(body, descriptor_start + 4)?).ok() != Some(upper_length)
  {
    return Err(closure_error("gc_directory_descriptor_length", "GC directory descriptor length is invalid"));
  }
  let page_id = u64_at(body, descriptor_start + 8)?;
  let h = algorithm.hash_length();
  let child_hash = &body[descriptor_start + 16..descriptor_start + 16 + h];
  let fields = descriptor_start + 16 + h;
  let child_generation = u64_at(body, fields)?;
  let record_count = u64_at(body, fields + 8)?;
  let tombstones = u64_at(body, fields + 16)?;
  let logical_bytes = u64_at(body, fields + 24)?;
  let repeated_fences = descriptor_start + fixed;
  if page_id == 0
    || all_zero(child_hash)
    || child_generation == 0
    || child_generation > artifact.generation
    || record_count == 0
    || tombstones != 0
    || logical_bytes == 0
    || body[fields + 32..fields + 56].iter().any(|byte| *byte != 0)
  {
    return Err(closure_error("gc_directory_descriptor", "GC directory child descriptor fields are invalid"));
  }
  if body[repeated_fences..repeated_fences + lower_length] != *lower_fence
    || body[repeated_fences + lower_length..] != *upper_fence
    || u64_at(body, 24)? != record_count
    || u64_at(body, 32)? != 0
    || u64_at(body, 40)? != 1
    || u64_at(body, 48)? != logical_bytes
    || u64_at(body, 56)? != page_id
    || u64_at(body, 64)? != page_id
  {
    return Err(closure_error("gc_directory_aggregate", "GC directory aggregate or repeated fences disagree"));
  }
  Ok(GcStateDirectoryV1 {
    role,
    database_id,
    catalog_id,
    generation: artifact.generation,
    page_id,
    record_count,
    logical_bytes,
    lower_fence,
    upper_fence,
    child_hash,
    child_generation,
    key,
  })
}

fn decode_candidate_delta(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::CandidateDelta
    || artifact.identity.len() != 28
    || all_zero(&artifact.identity[..16])
    || u64_at(artifact.identity, 16)? != artifact.generation
    || u32_at(artifact.identity, 24)? == 0
    || artifact.body.len() < 16 + h
  {
    return Err(identity_error("candidate_delta_identity", "candidate delta identity/body is invalid"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != 0 || body[16..16 + h].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("candidate_delta_header", "candidate delta reserve fields must be zero"));
  }
  let count = u32_at(body, 8)?;
  let records_length = usize::try_from(u32_at(body, 12)?).map_err(|_| length_error("candidate delta records length"))?;
  if u16_at(body, 4)? != 1 || count == 0 || 16usize.checked_add(h).and_then(|value| value.checked_add(records_length)) != Some(body.len()) {
    return Err(closure_error("candidate_delta_header", "candidate delta codec/count/length is invalid"));
  }
  let row_length = 52 + 2 * h;
  let framed_row_length = 4usize.checked_add(row_length).ok_or_else(|| length_error("candidate delta row width overflow"))?;
  if usize::try_from(count).ok().and_then(|value| value.checked_mul(framed_row_length)) != Some(records_length) {
    return Err(closure_error("candidate_delta_count", "candidate delta count does not match records length"));
  }
  let records = &body[16 + h..];
  let mut previous: Option<&[u8]> = None;
  for framed in records.chunks_exact(framed_row_length) {
    let operation = framed[0];
    if !matches!(operation, 1 | 2) {
      return Err(kind_error("candidate_delta_operation", format!("unknown candidate delta operation {operation}")));
    }
    if framed[1..4].iter().any(|byte| *byte != 0) {
      return Err(reserved_error("candidate_delta_operation", "candidate delta operation reserve bytes must be zero"));
    }
    let row = &framed[4..];
    validate_candidate_row(algorithm, row, operation == 2)?;
    if let Some(prior) = previous {
      if compare_physical_rows(algorithm, prior, row)? != Ordering::Less {
        return Err(order_error("candidate_delta_order", "candidate delta records are not strictly ordered"));
      }
    }
    previous = Some(row);
  }
  Ok(GcStateArtifactV1::CandidateDelta { record_count: count, key })
}

fn decode_manifest(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateManifestV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("gc_manifest_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.identity.len() != 24 || all_zero(&artifact.identity[..16]) || u64_at(artifact.identity, 16)? != artifact.generation {
    return Err(identity_error("gc_manifest_identity", "GC manifest database/generation identity is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let generation = artifact.generation;
  let (populated, record_count, secondary_count, primary_root, secondary_root) = match artifact.kind {
    GcArtifactKindV1::RootExpiryCatalogManifest => decode_root_expiry_manifest_body(artifact.body, algorithm)?,
    GcArtifactKindV1::RootLifecycleManifest => decode_root_lifecycle_manifest_body(artifact.body, algorithm, generation)?,
    GcArtifactKindV1::PhysicalInventoryManifest => decode_inventory_manifest_body(artifact.body, algorithm, generation)?,
    GcArtifactKindV1::QuarantineManifest => decode_quarantine_manifest_body(artifact.body, algorithm, generation)?,
    _ => return Err(kind_error("gc_manifest_kind", "artifact is not a GC lifecycle manifest")),
  };
  Ok(GcStateManifestV1 {
    kind: artifact.kind,
    database_id,
    generation,
    populated,
    record_count,
    secondary_count,
    primary_root,
    secondary_root,
    key,
  })
}

type ManifestBody<'a> = (bool, u64, u64, &'a [u8], Option<&'a [u8]>);

fn decode_root_expiry_manifest_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() != 124 + h || u32_at(body, 0)? != 0 || !valid_capabilities(&body[4..36], &[12, 17]) {
    return Err(closure_error("root_expiry_manifest_header", "root-expiry manifest shape/capabilities are invalid"));
  }
  if u64_at(body, 36)? == 0 || u64_at(body, 44)? == 0 {
    return Err(identity_error("root_expiry_manifest_header", "root-expiry policy values are zero"));
  }
  let root = &body[52..52 + h];
  let next = u64_at(body, 52 + h)?;
  let count = u64_at(body, 60 + h)?;
  let logical_bytes = u64_at(body, 68 + h)?;
  let mandatory_count = u64_at(body, 76 + h)?;
  let mandatory_bytes = u64_at(body, 84 + h)?;
  let optional_count = u64_at(body, 92 + h)?;
  let optional_bytes = u64_at(body, 100 + h)?;
  let oldest = i64_at(body, 108 + h)?;
  let newest = i64_at(body, 116 + h)?;
  let populated = !all_zero(root);
  if next == 0
    || mandatory_count.checked_add(optional_count) != Some(count)
    || mandatory_bytes.checked_add(optional_bytes) != Some(logical_bytes)
    || populated != (count != 0)
    || populated != (logical_bytes != 0)
    || (if populated { oldest <= 0 || newest <= 0 } else { oldest != 0 || newest != 0 })
    || oldest > newest
  {
    return Err(closure_error("root_expiry_manifest_state", "root-expiry manifest counts/times/root disagree"));
  }
  Ok((populated, count, mandatory_count, root, None))
}

fn decode_root_lifecycle_manifest_body(body: &[u8], algorithm: HashAlgorithm, generation: u64) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() != 108 + 3 * h
    || u32_at(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 17])
    || u64_at(body, 36)? != generation
    || i64_at(body, 44)? <= 0
    || u64_at(body, 52)? == 0
    || all_zero(&body[60..60 + h])
  {
    return Err(closure_error("root_lifecycle_manifest_header", "root-lifecycle manifest header/semantics are invalid"));
  }
  let candidate_root = &body[60 + h..60 + 2 * h];
  let expiry_root = &body[60 + 2 * h..60 + 3 * h];
  let next_page = u64_at(body, 60 + 3 * h)?;
  let candidate_count = u64_at(body, 68 + 3 * h)?;
  let pending_count = u64_at(body, 76 + 3 * h)?;
  let retired_count = u64_at(body, 84 + 3 * h)?;
  let candidate_bytes = u64_at(body, 92 + 3 * h)?;
  let expiry_bytes = u64_at(body, 100 + 3 * h)?;
  if next_page == 0
    || candidate_count != pending_count
    || !all_zero(candidate_root) != (candidate_count != 0)
    || !all_zero(expiry_root) != (retired_count != 0)
    || (candidate_count == 0) != (candidate_bytes == 0)
    || (retired_count == 0) != (expiry_bytes == 0)
  {
    return Err(closure_error("root_lifecycle_manifest_state", "root-lifecycle roots/counts/bytes disagree"));
  }
  Ok((candidate_count != 0 || retired_count != 0, candidate_count, retired_count, candidate_root, Some(expiry_root)))
}

fn decode_inventory_manifest_body(body: &[u8], algorithm: HashAlgorithm, generation: u64) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() != 132 + 2 * h
    || u32_at(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 13])
    || u64_at(body, 36)? != generation
    || u64_at(body, 44)? == 0
    || all_zero(&body[52..52 + h])
    || u64_at(body, 52 + h)? == 0
    || u64_at(body, 60 + h)? == 0
    || u64_at(body, 68 + h)? > u64_at(body, 60 + h)?
    || u64_at(body, 76 + 2 * h)? == 0
  {
    return Err(closure_error("inventory_manifest_header", "inventory manifest header/capture state is invalid"));
  }
  let root = &body[76 + h..76 + 2 * h];
  let mut count = 0u64;
  for index in 0..5 {
    count = count.checked_add(u64_at(body, 84 + 2 * h + index * 8)?).ok_or_else(|| length_error("inventory count overflow"))?;
  }
  let logical_bytes = u64_at(body, 124 + 2 * h)?;
  let populated = !all_zero(root);
  if populated != (count != 0) || populated != (logical_bytes != 0) {
    return Err(closure_error("inventory_manifest_state", "inventory root/count/bytes disagree"));
  }
  Ok((populated, count, 0, root, None))
}

fn decode_quarantine_manifest_body(body: &[u8], algorithm: HashAlgorithm, generation: u64) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() < 100 + 6 * h || body.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("quarantine_manifest_length", body.len(), MAX_MANIFEST_LENGTH));
  }
  if u32_at(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 13, 15, 17])
    || u64_at(body, 36)? != generation
    || u64_at(body, 44)? == 0
    || (0..4).any(|index| all_zero(&body[52 + index * h..52 + (index + 1) * h]))
  {
    return Err(closure_error("quarantine_manifest_header", "quarantine manifest capture authority is invalid"));
  }
  let root = &body[52 + 4 * h..52 + 5 * h];
  let captured_lifecycle = &body[52 + 5 * h..52 + 6 * h];
  if all_zero(captured_lifecycle) {
    return Err(identity_error("quarantine_manifest_header", "captured root-lifecycle manifest is zero"));
  }
  let delta_count = usize::try_from(u32_at(body, 52 + 6 * h)?).map_err(|_| length_error("quarantine delta count"))?;
  let count = u64_at(body, 60 + 6 * h)?;
  let bytes_count = u64_at(body, 68 + 6 * h)?;
  let eligible_count = u64_at(body, 76 + 6 * h)?;
  let eligible_bytes = u64_at(body, 84 + 6 * h)?;
  let next_page = u64_at(body, 92 + 6 * h)?;
  let deltas_start = 100 + 6 * h;
  if delta_count > MAX_DELTAS {
    return Err(amplification_error("quarantine_delta_count", delta_count, MAX_DELTAS));
  }
  if deltas_start.checked_add(delta_count.checked_mul(h).ok_or_else(|| length_error("quarantine delta bytes overflow"))?)
    != Some(body.len())
    || body[56 + 6 * h..60 + 6 * h].iter().any(|byte| *byte != 0)
  {
    return Err(reserved_error("quarantine_manifest_formula", "quarantine delta framing/reserve is invalid"));
  }
  if body[deltas_start..].chunks_exact(h).any(all_zero)
    || next_page == 0
    || eligible_count > count
    || eligible_bytes > bytes_count
    || (count != 0) != (bytes_count != 0)
    || (eligible_count == 0) != (eligible_bytes == 0)
    || (count != 0 && all_zero(root) && delta_count == 0)
  {
    return Err(closure_error("quarantine_manifest_state", "quarantine roots/deltas/counts/bytes disagree"));
  }
  Ok((count != 0, count, eligible_count, root, Some(captured_lifecycle)))
}

fn decode_root_retirement_commit(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("root_retirement_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::RootRetirementCommit
    || artifact.identity.len() != 32 + h
    || all_zero(&artifact.identity[..16])
    || all_zero(&artifact.identity[16..16 + h])
    || all_zero(&artifact.identity[16 + h..])
    || artifact.body.len() != 72 + 4 * h
  {
    return Err(identity_error("root_retirement_shape", "root-retirement identity/body shape is invalid"));
  }
  let body = artifact.body;
  if body[..32 + h] != *artifact.identity {
    return Err(closure_error("root_retirement_identity", "root-retirement body does not repeat identity"));
  }
  if u16_at(body, 66 + h)? != 0 || u32_at(body, 68 + h)? != 0 {
    return Err(reserved_error("root_retirement_fields", "root-retirement reserve fields must be zero"));
  }
  let committed = i64_at(body, 32 + h)?;
  let pending = i64_at(body, 40 + h)?;
  let grace = u64_at(body, 48 + h)?;
  let grace = i64::try_from(grace).map_err(|_| length_error("root-retirement grace exceeds i64"))?;
  let eligible_at = pending.checked_add(grace).ok_or_else(|| length_error("root-retirement eligibility overflows i64"))?;
  let mark_generation = u64_at(body, 56 + h)?;
  if committed <= 0
    || pending <= 0
    || committed < eligible_at
    || mark_generation == 0
    || mark_generation != artifact.generation
    || u16_at(body, 64 + h)? == 0
    || (1..=3).any(|index| all_zero(&body[72 + index * h..72 + (index + 1) * h]))
  {
    return Err(closure_error("root_retirement_fields", "root-retirement timing, generation, reason, or evidence is invalid"));
  }
  Ok(GcStateArtifactV1::RootRetirementCommit { mark_generation, key })
}

fn decode_root_object_reclaim_proof(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("root_reclaim_proof_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::RootObjectReclaimProof
    || artifact.identity.len() != 32 + h
    || all_zero(&artifact.identity[..16])
    || all_zero(&artifact.identity[16..16 + h])
    || all_zero(&artifact.identity[16 + h..])
    || artifact.body.len() != 40 + 6 * h
  {
    return Err(identity_error("root_reclaim_proof_shape", "root reclaim proof identity/body shape is invalid"));
  }
  let body = artifact.body;
  if body[..16] != artifact.identity[..16] || body[16..16 + h] != artifact.identity[16..16 + h] {
    return Err(closure_error("root_reclaim_proof_identity", "root reclaim proof body does not repeat database/root identity"));
  }
  let incarnation_count = u64_at(body, 24 + 4 * h)?;
  let receipt_count = u64_at(body, 32 + 5 * h)?;
  if all_zero(&body[16 + h..16 + 2 * h])
    || i64_at(body, 16 + 2 * h)? <= 0
    || all_zero(&body[24 + 2 * h..24 + 3 * h])
    || all_zero(&body[24 + 3 * h..24 + 4 * h])
    || incarnation_count == 0
    || all_zero(&body[32 + 4 * h..32 + 5 * h])
    || receipt_count == 0
    || all_zero(&body[40 + 5 * h..])
  {
    return Err(closure_error("root_reclaim_proof_fields", "root reclaim proof evidence/counts are invalid"));
  }
  Ok(GcStateArtifactV1::RootObjectReclaimProof { incarnation_count, receipt_count, key })
}

fn decode_retirement_journal(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  if bytes.len() > MAX_PAGE_LENGTH {
    return Err(amplification_error("retirement_journal_length", bytes.len(), MAX_PAGE_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::RetirementJournalSegment
    || artifact.identity.len() != 24
    || all_zero(&artifact.identity[..16])
    || u64_at(artifact.identity, 16)? == 0
    || artifact.body.len() < 32 + h
  {
    return Err(identity_error("retirement_journal_identity", "retirement journal identity/body is invalid"));
  }
  let body = artifact.body;
  let flags = u32_at(body, 0)?;
  if flags & !1 != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("retirement_journal_header", "retirement journal flags/reserve are invalid"));
  }
  let first = u64_at(body, 8)?;
  let last = u64_at(body, 16)?;
  let count = u32_at(body, 24)?;
  let records_length = usize::try_from(u32_at(body, 28)?).map_err(|_| length_error("retirement records length"))?;
  if u16_at(body, 4)? != 1
    || first == 0
    || first > last
    || count == 0
    || 32usize.checked_add(h).and_then(|value| value.checked_add(records_length)) != Some(body.len())
    || ((flags & 1 != 0) != all_zero(&body[32..32 + h]))
  {
    return Err(closure_error("retirement_journal_header", "retirement journal codec/range/reset/length is invalid"));
  }
  let record_length = 72 + 4 * h;
  if usize::try_from(count).ok().and_then(|value| value.checked_mul(record_length)) != Some(records_length) {
    return Err(closure_error("retirement_journal_count", "retirement record count does not match length"));
  }
  let records = &body[32 + h..];
  let mut first_observed = None;
  let mut previous: Option<(u64, &[u8])> = None;
  for record in records.chunks_exact(record_length) {
    if usize::try_from(u32_at(record, 0)?).ok() != Some(record_length) || u16_at(record, 6)? != 0 {
      return Err(reserved_error("retirement_record_length", "retirement record length/reserve is invalid"));
    }
    let reason = u16_at(record, 4)?;
    let sequence = u64_at(record, 8)?;
    let retired_at = u64_at(record, 16)?;
    let physical_length = 24 + 2 * h;
    let old_bytes = &record[24..24 + physical_length];
    let replacement_bytes = &record[24 + physical_length..];
    let old = decode_physical_incarnation(old_bytes, algorithm)?;
    let replacement = decode_physical_incarnation(replacement_bytes, algorithm)?;
    if !(1..=5).contains(&reason) || sequence == 0 || retired_at == 0 || old == replacement {
      return Err(closure_error("retirement_record_fields", "retirement record reason/time/incarnations are invalid"));
    }
    if let Some((prior_sequence, prior_old)) = previous {
      if prior_sequence > sequence
        || (prior_sequence == sequence && compare_physical_bytes(algorithm, prior_old, old_bytes)? != Ordering::Less)
      {
        return Err(order_error("retirement_record_order", "retirement records are not canonically ordered"));
      }
    }
    first_observed.get_or_insert(sequence);
    previous = Some((sequence, old_bytes));
  }
  if first_observed != Some(first) || previous.map(|(sequence, _)| sequence) != Some(last) {
    return Err(closure_error("retirement_journal_order", "retirement journal first/last sequence disagrees"));
  }
  Ok(GcStateArtifactV1::RetirementJournal { record_count: count, key })
}

fn validate_row(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, row: &[u8], clear: bool) -> FormatResult<()> {
  match role {
    GcDirectoryRoleV1::Candidates => validate_candidate_row(algorithm, row, clear),
    GcDirectoryRoleV1::RootExpiry => validate_root_expiry_row(algorithm, row),
    GcDirectoryRoleV1::PhysicalInventory => validate_inventory_row(algorithm, row),
    GcDirectoryRoleV1::RootCandidates => validate_root_candidate_row(algorithm, row),
  }
}

fn validate_candidate_row(algorithm: HashAlgorithm, row: &[u8], clear: bool) -> FormatResult<()> {
  let physical_length = 24 + 2 * algorithm.hash_length();
  if row.len() != 52 + 2 * algorithm.hash_length() {
    return Err(trailing_error("candidate_row_length", "candidate row has wrong fixed length"));
  }
  decode_physical_incarnation(&row[..physical_length], algorithm)?;
  let class = u16_at(row, physical_length)?;
  if !(1..=7).contains(&class) {
    return Err(kind_error("candidate_row_class", format!("unknown candidate class {class}")));
  }
  if u16_at(row, physical_length + 2)? != 0 {
    return Err(reserved_error("candidate_row_flags", "candidate row reserve must be zero"));
  }
  let pending = u64_at(row, physical_length + 4)?;
  let first_generation = u64_at(row, physical_length + 12)?;
  let grace = u64_at(row, physical_length + 20)?;
  if (clear && (pending != 0 || first_generation != 0 || grace != 0)) || (!clear && (pending == 0 || first_generation == 0)) {
    return Err(closure_error("candidate_row_state", "candidate set/clear state is invalid"));
  }
  Ok(())
}

fn validate_root_expiry_row(algorithm: HashAlgorithm, row: &[u8]) -> FormatResult<()> {
  decode_root_expiry_record_v1(row, algorithm).map(|_| ())
}

pub fn decode_root_expiry_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootExpiryRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  if row.len() != 40 + 3 * hash_width {
    return Err(trailing_error("root_expiry_row", "root-expiry row has wrong fixed length"));
  }
  if row[hash_width + 28..hash_width + 32].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("root_expiry_row", "root-expiry row reserve must be zero"));
  }
  let retired_at_ms = i64_at(row, hash_width)?;
  let last_pending_since_ms = i64_at(row, hash_width + 8)?;
  let final_mark_generation = u64_at(row, hash_width + 16)?;
  let reason = u16_at(row, hash_width + 24)?;
  let state = row[hash_width + 26];
  let root_object_reclaim_proof = &row[hash_width + 32 + hash_width..hash_width + 32 + 2 * hash_width];
  let evidence_expires_at_ms = i64_at(row, hash_width + 32 + 2 * hash_width)?;
  if all_zero(&row[..hash_width])
    || retired_at_ms <= 0
    || last_pending_since_ms <= 0
    || last_pending_since_ms > retired_at_ms
    || final_mark_generation == 0
    || !matches!(state, 1 | 2)
    || all_zero(&row[hash_width + 32..hash_width + 32 + hash_width])
  {
    return Err(identity_error("root_expiry_row", "root-expiry identity/times/state are invalid"));
  }
  if !matches!(reason, root_retirement_reason_v1::ORDINARY_GC_UNREACHABLE | root_retirement_reason_v1::EXPLICIT_OPERATOR_RETIREMENT) {
    return Err(kind_error("root_expiry_reason", "root-expiry reason is outside RootRetirementReasonV1"));
  }
  let (state, root_object_reclaim_proof_hash, evidence_expires_at_ms) = match state {
    1 if row[hash_width + 27] == 0 && all_zero(root_object_reclaim_proof) && evidence_expires_at_ms == 0 => {
      (RootExpiryStateV1::LogicallyRetired, None, None)
    }
    2 if row[hash_width + 27] == 1 && !all_zero(root_object_reclaim_proof) && evidence_expires_at_ms >= retired_at_ms => {
      (RootExpiryStateV1::PhysicallyReclaimed, Some(root_object_reclaim_proof), Some(evidence_expires_at_ms))
    }
    _ => return Err(closure_error("root_expiry_row_state", "root-expiry pending/reclaimed evidence is inconsistent")),
  };
  Ok(RootExpiryRecordV1 {
    namespace_root_hash: &row[..hash_width],
    retired_at_ms,
    last_pending_since_ms,
    final_mark_generation,
    reason,
    state,
    retirement_commit_hash: &row[hash_width + 32..hash_width + 32 + hash_width],
    root_object_reclaim_proof_hash,
    evidence_expires_at_ms,
  })
}

fn validate_root_candidate_row(algorithm: HashAlgorithm, row: &[u8]) -> FormatResult<()> {
  decode_root_candidate_record_v1(row, algorithm).map(|_| ())
}

pub fn decode_root_candidate_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootCandidateRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  if row.len() != 36 + 3 * hash_width {
    return Err(trailing_error("root_candidate_row", "root-candidate row has wrong fixed length"));
  }
  if row[hash_width + 1] != 0 {
    return Err(reserved_error("root_candidate_row", "root-candidate reserve must be zero"));
  }
  let reason = u16_at(row, hash_width + 2)?;
  let pending_since_ms = i64_at(row, hash_width + 4)?;
  let first_unreachable_generation = u64_at(row, hash_width + 12)?;
  let last_confirmed_unreachable_generation = u64_at(row, hash_width + 20)?;
  let grace_at_pending_ms = u64_at(row, hash_width + 28)?;
  if all_zero(&row[..hash_width])
    || row[hash_width] != 1
    || pending_since_ms <= 0
    || first_unreachable_generation == 0
    || last_confirmed_unreachable_generation < first_unreachable_generation
    || all_zero(&row[hash_width + 36..hash_width + 36 + hash_width])
    || all_zero(&row[hash_width + 36 + hash_width..])
  {
    return Err(identity_error("root_candidate_row", "root-candidate state/identity/evidence is invalid"));
  }
  if !matches!(reason, root_retirement_reason_v1::ORDINARY_GC_UNREACHABLE | root_retirement_reason_v1::EXPLICIT_OPERATOR_RETIREMENT) {
    return Err(kind_error("root_candidate_reason", "root-candidate reason is outside RootRetirementReasonV1"));
  }
  Ok(RootCandidateRecordV1 {
    namespace_root_hash: &row[..hash_width],
    reason,
    pending_since_ms,
    first_unreachable_generation,
    last_confirmed_unreachable_generation,
    grace_at_pending_ms,
    authority_root_set_digest: &row[hash_width + 36..hash_width + 36 + hash_width],
    admission_commit_payload_hash: &row[hash_width + 36 + hash_width..],
  })
}

fn validate_inventory_row(algorithm: HashAlgorithm, row: &[u8]) -> FormatResult<()> {
  let h = algorithm.hash_length();
  let physical_length = 24 + 2 * h;
  if row.len() != 68 + 5 * h {
    return Err(trailing_error("inventory_row_length", "inventory row has wrong fixed length"));
  }
  decode_physical_incarnation(&row[..physical_length], algorithm)?;
  let state = row[physical_length];
  let reason = row[physical_length + 1];
  let flags = u16_at(row, physical_length + 2)?;
  if !(1..=5).contains(&state) || flags & !3 != 0 || (state == 1) != (reason == 0) {
    return Err(kind_error("inventory_row_state", "inventory state/reason/flags are invalid"));
  }
  let replacement = &row[physical_length + 4..physical_length + 4 + physical_length];
  if flags & 1 != 0 {
    decode_physical_incarnation(replacement, algorithm)?;
  } else if replacement.iter().any(|byte| *byte != 0) {
    return Err(closure_error("inventory_row_replacement", "replacement is present without replacement flag"));
  }
  let tail = physical_length + 4 + physical_length;
  if u64_at(row, tail)? == 0 || (state == 1 && (u64_at(row, tail + 8)? != 0 || flags != 0)) {
    return Err(closure_error("inventory_row_time_or_sequence", "inventory observed time/sequence is invalid"));
  }
  let receipt = &row[tail + 16..tail + 16 + h];
  if (flags & 2 != 0) != !all_zero(receipt) || (state == 5) != (flags & 2 != 0) {
    return Err(closure_error("inventory_row_receipt", "inventory receipt/state flags disagree"));
  }
  Ok(())
}

fn compare_rows(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  match role {
    GcDirectoryRoleV1::Candidates => compare_physical_rows(algorithm, left, right),
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => {
      Ok(left[..algorithm.hash_length()].cmp(&right[..algorithm.hash_length()]))
    }
    GcDirectoryRoleV1::PhysicalInventory => {
      let h = algorithm.hash_length();
      let physical_length = 24 + 2 * h;
      let left_physical = decode_physical_incarnation(&left[..physical_length], algorithm)?;
      let right_physical = decode_physical_incarnation(&right[..physical_length], algorithm)?;
      Ok(left_physical.wal_offset.cmp(&right_physical.wal_offset).then_with(|| compare_physical(&left_physical, &right_physical)))
    }
  }
}

fn compare_physical_rows(algorithm: HashAlgorithm, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  let physical_length = 24 + 2 * algorithm.hash_length();
  compare_physical_bytes(algorithm, &left[..physical_length], &right[..physical_length])
}

fn compare_physical_bytes(algorithm: HashAlgorithm, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  let left = decode_physical_incarnation(left, algorithm)?;
  let right = decode_physical_incarnation(right, algorithm)?;
  Ok(compare_physical(&left, &right))
}

fn compare_physical(left: &PhysicalIncarnationV1<'_>, right: &PhysicalIncarnationV1<'_>) -> Ordering {
  left
    .logical_key
    .cmp(right.logical_key)
    .then_with(|| left.integrity_or_legacy_digest.cmp(right.integrity_or_legacy_digest))
    .then_with(|| left.wal_offset.cmp(&right.wal_offset))
    .then_with(|| left.write_sequence.cmp(&right.write_sequence))
    .then_with(|| left.entity_length.cmp(&right.entity_length))
    .then_with(|| left.entry_type.cmp(&right.entry_type))
    .then_with(|| left.entity_version.cmp(&right.entity_version))
}

fn compare_fences(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, left: &[u8], right: &[u8]) -> FormatResult<()> {
  let ordering = match role {
    GcDirectoryRoleV1::Candidates => compare_physical_bytes(algorithm, left, right)?,
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => {
      if left.len() != algorithm.hash_length() || right.len() != algorithm.hash_length() {
        return Err(closure_error("root_key_length", "root fence has wrong hash width"));
      }
      left.cmp(right)
    }
    GcDirectoryRoleV1::PhysicalInventory => {
      let physical_length = 24 + 2 * algorithm.hash_length();
      if left.len() != 8 + physical_length || right.len() != 8 + physical_length {
        return Err(closure_error("inventory_key_length", "inventory fence has wrong fixed width"));
      }
      let offset_order = u64_at(left, 0)?.cmp(&u64_at(right, 0)?);
      if offset_order == Ordering::Equal {
        compare_physical_bytes(algorithm, &left[8..], &right[8..])?
      } else {
        offset_order
      }
    }
  };
  if ordering == Ordering::Greater {
    return Err(order_error("gc_fence_order", "GC lower fence sorts after upper fence"));
  }
  Ok(())
}

fn row_key_equals_fence(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, row: &[u8], fence: &[u8]) -> FormatResult<bool> {
  let h = algorithm.hash_length();
  match role {
    GcDirectoryRoleV1::Candidates => Ok(&row[..24 + 2 * h] == fence),
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => Ok(&row[..h] == fence),
    GcDirectoryRoleV1::PhysicalInventory => {
      let physical_length = 24 + 2 * h;
      Ok(fence.len() == 8 + physical_length && fence[..8] == row[2 * h..2 * h + 8] && fence[8..] == row[..physical_length])
    }
  }
}

fn row_length(algorithm: HashAlgorithm, role: GcDirectoryRoleV1) -> usize {
  let h = algorithm.hash_length();
  match role {
    GcDirectoryRoleV1::Candidates => 52 + 2 * h,
    GcDirectoryRoleV1::RootExpiry => 40 + 3 * h,
    GcDirectoryRoleV1::PhysicalInventory => 68 + 5 * h,
    GcDirectoryRoleV1::RootCandidates => 36 + 3 * h,
  }
}

fn manifest_summary(manifest: &GcStateManifestV1<'_>) -> String {
  let state = if manifest.populated { "populated" } else { "empty" };
  match manifest.kind {
    GcArtifactKindV1::QuarantineManifest => format!("gc:manifest:quarantine:{state}:candidates={}", manifest.record_count),
    GcArtifactKindV1::RootExpiryCatalogManifest => format!("gc:manifest:root-expiry:{state}:records={}", manifest.record_count),
    GcArtifactKindV1::PhysicalInventoryManifest => {
      format!("gc:manifest:physical-inventory:{state}:records={}", manifest.record_count)
    }
    GcArtifactKindV1::RootLifecycleManifest => {
      format!("gc:manifest:root-lifecycle:{state}:candidates={}:retired={}", manifest.record_count, manifest.secondary_count)
    }
    _ => unreachable!("GcStateManifestV1 only represents lifecycle manifests"),
  }
}

fn valid_capabilities(actual: &[u8], bits: &[usize]) -> bool {
  if actual.len() != 32 {
    return false;
  }
  let mut expected = [0u8; 32];
  for bit in bits {
    expected[bit / 8] |= 1 << (bit % 8);
  }
  actual == expected
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let raw =
    bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("gc_artifact_truncated", format!("i64 at offset {offset} is truncated")))?;
  Ok(i64::from_le_bytes(raw.try_into().expect("checked GC i64 width")))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn amplification_error(code: &'static str, actual: usize, cap: usize) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, code, format!("{actual} exceeds cap {cap}"))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "gc_state_overflow", context)
}

fn trailing_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, code, context)
}

fn reserved_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, code, context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}

fn order_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
