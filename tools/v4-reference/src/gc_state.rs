use std::cmp::Ordering;

use crate::core::HashProfile;
use crate::gc::{
  build_gc_value, decode_gc_value, decode_physical_incarnation, encode_physical_incarnation, immutable_key, put_u16, put_u32, put_u64,
  read_u16, read_u32, read_u64, GcFixtureCase, GcFormat, GcKind, PhysicalIncarnationId,
};

const MAX_MANIFEST_LENGTH: usize = 1024 * 1024;
const MAX_PAGE_LENGTH: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1024 * 1024;
const MAX_KEY_LENGTH: usize = 1024 * 1024;
const MAX_DELTAS: usize = 256;
const CAPABILITIES_LENGTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryRole {
  Candidates = 1,
  RootExpiry = 2,
  PhysicalInventory = 3,
  RootCandidates = 8,
}

impl DirectoryRole {
  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::Candidates),
      2 => Some(Self::RootExpiry),
      3 => Some(Self::PhysicalInventory),
      8 => Some(Self::RootCandidates),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::Candidates => "candidates",
      Self::RootExpiry => "root-expiry",
      Self::PhysicalInventory => "physical-inventory",
      Self::RootCandidates => "root-candidates",
    }
  }

  fn page_kind(self) -> GcKind {
    match self {
      Self::Candidates => GcKind::CandidatePage,
      Self::RootExpiry => GcKind::RootExpiryPage,
      Self::PhysicalInventory => GcKind::PhysicalInventoryPage,
      Self::RootCandidates => GcKind::RootCandidatePage,
    }
  }
}

#[derive(Clone)]
struct SamplePage {
  role: DirectoryRole,
  catalog_id: [u8; 16],
  generation: u64,
  page_id: u64,
  lower: Vec<u8>,
  upper: Vec<u8>,
  record_count: u32,
  logical_bytes: u64,
  bytes: Vec<u8>,
}

#[derive(Debug)]
struct DecodedPage {
  role: DirectoryRole,
  page_id: u64,
  record_count: u32,
}

#[derive(Debug)]
struct DecodedDirectory {
  role: DirectoryRole,
  #[cfg(test)]
  generation: u64,
  #[cfg(test)]
  page_id: u64,
  record_count: u64,
  #[cfg(test)]
  lower: Vec<u8>,
  #[cfg(test)]
  upper: Vec<u8>,
  #[cfg(test)]
  child_hash: Vec<u8>,
}

#[derive(Debug)]
struct DecodedManifest {
  kind: GcKind,
  populated: bool,
  record_count: u64,
  secondary_count: u64,
  root: Vec<u8>,
}

pub(crate) fn fixture_cases() -> Vec<GcFixtureCase> {
  let mut cases = Vec::with_capacity(40);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let candidate_page = build_candidate_page(profile);
    let candidate_directory = build_leaf_directory(profile, &candidate_page);
    let candidate_delta = build_candidate_delta(profile);
    let inventory_page = build_inventory_page(profile);
    let inventory_directory = build_leaf_directory(profile, &inventory_page);
    let inventory_empty = build_inventory_manifest(profile, false, &[]);
    let inventory_populated =
      build_inventory_manifest(profile, true, &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &inventory_directory));
    let lifecycle_empty = build_root_lifecycle_manifest(profile, false, &[], &[]);
    let root_retirement = build_root_retirement_commit(profile, &immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle_empty));
    let reclaim_proof = build_root_object_reclaim_proof(
      profile,
      &immutable_key(profile, GcKind::RootRetirementCommit, &root_retirement),
      &immutable_key(profile, GcKind::PhysicalInventoryManifest, &inventory_populated),
    );
    let root_page = build_root_expiry_page(
      profile,
      &immutable_key(profile, GcKind::RootRetirementCommit, &root_retirement),
      &immutable_key(profile, GcKind::RootObjectReclaimProof, &reclaim_proof),
    );
    let root_directory = build_leaf_directory(profile, &root_page);
    let root_empty = build_root_expiry_manifest(profile, false, &[]);
    let root_populated =
      build_root_expiry_manifest(profile, true, &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &root_directory));
    let root_candidate_page = build_root_candidate_page(profile);
    let root_candidate_directory = build_leaf_directory(profile, &root_candidate_page);
    let lifecycle_populated = build_root_lifecycle_manifest(
      profile,
      true,
      &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &root_candidate_directory),
      &immutable_key(profile, GcKind::RootExpiryCatalogManifest, &root_populated),
    );
    let quarantine_empty =
      build_quarantine_manifest(profile, false, &[], &immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle_empty), &[]);
    let quarantine_populated = build_quarantine_manifest(
      profile,
      true,
      &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &candidate_directory),
      &immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle_populated),
      &immutable_key(profile, GcKind::CandidateDelta, &candidate_delta),
    );
    let retirement_journal = build_retirement_journal(profile);

    for (bytes, expected, relation) in [
      (candidate_page.bytes.clone(), page_expected(&candidate_page), "quarantine-candidate-state"),
      (candidate_directory, directory_expected(profile, &candidate_page), "indexes:CandidatePage"),
      (candidate_delta, "gc:delta:candidate:records=2".to_string(), "overlays:CandidatePage"),
      (root_page.bytes.clone(), page_expected(&root_page), "root-lifecycle-evidence-not-a-pin"),
      (root_directory, directory_expected(profile, &root_page), "indexes:RootExpiryPage"),
      (root_empty, "gc:manifest:root-expiry:empty:records=0".to_string(), "root-expiry-catalog-empty"),
      (root_populated, "gc:manifest:root-expiry:populated:records=2".to_string(), "roots:RootExpiryPage-directory"),
      (root_candidate_page.bytes.clone(), page_expected(&root_candidate_page), "logical-root-pending-state"),
      (root_candidate_directory, directory_expected(profile, &root_candidate_page), "indexes:RootCandidatePage"),
      (lifecycle_empty, "gc:manifest:root-lifecycle:empty:candidates=0:retired=0".to_string(), "logical-lifecycle-authority-empty"),
      (
        lifecycle_populated,
        "gc:manifest:root-lifecycle:populated:candidates=1:retired=2".to_string(),
        "roots:RootCandidatePage-directory-and-RootExpiryCatalogManifest",
      ),
      (root_retirement, "gc:commit:root-retirement:mark=501".to_string(), "logical-retirement-linearization-evidence"),
      (reclaim_proof, "gc:proof:root-object-reclaim:incarnations=1:receipts=1".to_string(), "root-object-physical-absence-evidence"),
      (inventory_page.bytes.clone(), page_expected(&inventory_page), "physical-state-not-reclaim-authority"),
      (inventory_directory, directory_expected(profile, &inventory_page), "indexes:PhysicalInventoryPage"),
      (inventory_empty, "gc:manifest:physical-inventory:empty:records=0".to_string(), "physical-inventory-empty"),
      (inventory_populated, "gc:manifest:physical-inventory:populated:records=5".to_string(), "roots:PhysicalInventoryPage-directory"),
      (quarantine_empty, "gc:manifest:quarantine:empty:candidates=0".to_string(), "two-complete-mark-state-empty"),
      (quarantine_populated, "gc:manifest:quarantine:populated:candidates=2".to_string(), "roots:candidate-base-delta-and-root-expiry"),
      (retirement_journal, "gc:journal:retirement:records=1".to_string(), "evidence-only-until-inventory-and-two-marks"),
    ] {
      let kind = GcKind::from_id(read_u16(&bytes, 6).expect("sample kind")).expect("registered sample kind");
      let id = fixture_id(profile, kind, &expected);
      let key = immutable_key(profile, kind, &bytes);
      cases.push(GcFixtureCase {
        id,
        format: GcFormat::GcArtifactV1,
        profile,
        expected: leak(expected),
        relation: Some(relation),
        canonical_key: Some(hex::encode(key)),
        bytes,
      });
    }
  }
  cases
}

pub(crate) fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  let kind = match read_u16(bytes, 6).ok().and_then(GcKind::from_id) {
    Some(kind) => kind,
    None => return ("error:gc_state_kind".to_string(), None),
  };
  let observed = match kind {
    GcKind::CandidatePage | GcKind::RootExpiryPage | GcKind::PhysicalInventoryPage | GcKind::RootCandidatePage => {
      decode_page(profile, bytes)
        .map(|page| format!("gc:page:{}:page={}:records={}", page.role.name().trim_end_matches('s'), page.page_id, page.record_count))
    }
    GcKind::GcArtifactDirectoryNode => decode_directory(profile, bytes)
      .map(|directory| format!("gc:directory:{}:level=0:records={}", directory.role.name(), directory.record_count)),
    GcKind::CandidateDelta => decode_candidate_delta(profile, bytes).map(|count| format!("gc:delta:candidate:records={count}")),
    GcKind::RootExpiryCatalogManifest | GcKind::PhysicalInventoryManifest | GcKind::QuarantineManifest | GcKind::RootLifecycleManifest => {
      decode_manifest(profile, bytes).map(|manifest| manifest_expected(&manifest))
    }
    GcKind::RootRetirementCommit => {
      decode_root_retirement_commit(profile, bytes).map(|mark_generation| format!("gc:commit:root-retirement:mark={mark_generation}"))
    }
    GcKind::RootObjectReclaimProof => decode_root_object_reclaim_proof(profile, bytes)
      .map(|(incarnations, receipts)| format!("gc:proof:root-object-reclaim:incarnations={incarnations}:receipts={receipts}")),
    GcKind::RetirementJournalSegment => {
      decode_retirement_journal(profile, bytes).map(|count| format!("gc:journal:retirement:records={count}"))
    }
    _ => Err("gc_state_kind"),
  };
  match observed {
    Ok(expected) => (expected, Some(hex::encode(immutable_key(profile, kind, bytes)))),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub(crate) fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let kind = read_u16(bytes, 6).ok().and_then(GcKind::from_id);
  let identity_length = read_u16(bytes, 16).unwrap_or(0);
  let body_length = read_u32(bytes, 20).unwrap_or(0);
  vec![
    "envelope +0x000 len 32: AGCA common envelope".to_string(),
    format!("envelope artifact_kind: {}", kind.map_or("invalid", GcKind::name)),
    format!("identity +0x000 len {identity_length}: exact kind identity (H={})", profile.width()),
    format!("body +0x000 len {body_length}: exact bounded state body"),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

fn fixture_id(profile: HashProfile, kind: GcKind, expected: &str) -> &'static str {
  let suffix = if expected.contains(":empty:") {
    "empty"
  } else if expected.contains(":populated:") {
    "populated"
  } else {
    "valid"
  };
  let artifact_name = if kind == GcKind::GcArtifactDirectoryNode {
    let role = expected.strip_prefix("gc:directory:").and_then(|value| value.split(':').next()).expect("directory fixture role");
    format!("{role}-directory")
  } else {
    kind.name().to_string()
  };
  leak(format!("agca-{}-{artifact_name}-{suffix}", profile.label()))
}

fn page_expected(page: &SamplePage) -> String {
  format!("gc:page:{}:page={}:records={}", page.role.name().trim_end_matches('s'), page.page_id, page.record_count)
}

fn directory_expected(profile: HashProfile, page: &SamplePage) -> String {
  let decoded = decode_directory(profile, &build_leaf_directory(profile, page)).expect("sample directory");
  format!("gc:directory:{}:level=0:records={}", decoded.role.name(), decoded.record_count)
}

fn manifest_expected(manifest: &DecodedManifest) -> String {
  if matches!(manifest.kind, GcKind::RootExpiryCatalogManifest | GcKind::PhysicalInventoryManifest) {
    debug_assert_eq!(manifest.populated, manifest.root.iter().any(|byte| *byte != 0));
  }
  match manifest.kind {
    GcKind::QuarantineManifest => {
      format!("gc:manifest:quarantine:{}:candidates={}", if manifest.populated { "populated" } else { "empty" }, manifest.record_count)
    }
    GcKind::RootExpiryCatalogManifest => {
      format!("gc:manifest:root-expiry:{}:records={}", if manifest.populated { "populated" } else { "empty" }, manifest.record_count)
    }
    GcKind::PhysicalInventoryManifest => {
      format!("gc:manifest:physical-inventory:{}:records={}", if manifest.populated { "populated" } else { "empty" }, manifest.record_count)
    }
    GcKind::RootLifecycleManifest => format!(
      "gc:manifest:root-lifecycle:{}:candidates={}:retired={}",
      if manifest.populated { "populated" } else { "empty" },
      manifest.record_count,
      manifest.secondary_count
    ),
    _ => unreachable!("manifest decoder only returns supported manifests"),
  }
}

fn database_id() -> [u8; 16] {
  sequence_array(0x31)
}

fn catalog_id(role: DirectoryRole) -> [u8; 16] {
  sequence_array(0x50 + role as u8 * 0x10)
}

fn sequence_array(start: u8) -> [u8; 16] {
  let mut bytes = [0u8; 16];
  fill_sequence(&mut bytes, start);
  bytes
}

fn sample_incarnation(profile: HashProfile, ordinal: u8) -> PhysicalIncarnationId {
  let mut logical_key = vec![0u8; profile.width()];
  let mut digest = vec![0u8; profile.width()];
  fill_sequence(&mut logical_key, 0x20 + ordinal * 0x10);
  fill_sequence(&mut digest, 0x80 + ordinal * 0x10);
  PhysicalIncarnationId {
    logical_key,
    integrity_or_legacy_digest: digest,
    wal_offset: 100_000 + u64::from(ordinal) * 10_000,
    write_sequence: 1_000 + u64::from(ordinal),
    entity_length: 4_096 + u32::from(ordinal),
    entry_type: 2,
    entity_version: 1,
  }
}

fn sample_root(profile: HashProfile, ordinal: u8) -> Vec<u8> {
  let mut root = vec![0u8; profile.width()];
  fill_sequence(&mut root, 0x41 + ordinal * 0x10);
  root
}

fn candidate_row(profile: HashProfile, ordinal: u8, clear: bool) -> Vec<u8> {
  let physical = encode_physical_incarnation(profile, &sample_incarnation(profile, ordinal));
  let mut row = vec![0u8; 52 + 2 * profile.width()];
  row[..physical.len()].copy_from_slice(&physical);
  put_u16(&mut row, physical.len(), u16::from(ordinal.min(7)));
  if !clear {
    put_u64(&mut row, physical.len() + 4, 1_700_000_000_000 + u64::from(ordinal));
    put_u64(&mut row, physical.len() + 12, 40 + u64::from(ordinal));
    put_u64(&mut row, physical.len() + 20, 86_400_000);
  }
  row
}

fn decode_candidate_row(profile: HashProfile, row: &[u8], clear: bool) -> Result<PhysicalIncarnationId, &'static str> {
  let physical_length = 24 + 2 * profile.width();
  if row.len() != 52 + 2 * profile.width() {
    return Err("candidate_row_length");
  }
  let physical = decode_physical_incarnation(profile, &row[..physical_length])?;
  let class = read_u16(row, physical_length)?;
  let pending = read_u64(row, physical_length + 4)?;
  let first_generation = read_u64(row, physical_length + 12)?;
  let grace = read_u64(row, physical_length + 20)?;
  if !(1..=7).contains(&class) || read_u16(row, physical_length + 2)? != 0 {
    return Err("candidate_row_class_or_flags");
  }
  if clear {
    if pending != 0 || first_generation != 0 || grace != 0 {
      return Err("candidate_clear_state");
    }
  } else if pending == 0 || first_generation == 0 {
    return Err("candidate_state");
  }
  Ok(physical)
}

fn physical_compare(left: &PhysicalIncarnationId, right: &PhysicalIncarnationId) -> Ordering {
  left
    .logical_key
    .cmp(&right.logical_key)
    .then_with(|| left.integrity_or_legacy_digest.cmp(&right.integrity_or_legacy_digest))
    .then_with(|| left.wal_offset.cmp(&right.wal_offset))
    .then_with(|| left.write_sequence.cmp(&right.write_sequence))
    .then_with(|| left.entity_length.cmp(&right.entity_length))
    .then_with(|| left.entry_type.cmp(&right.entry_type))
    .then_with(|| left.entity_version.cmp(&right.entity_version))
}

fn candidate_key(profile: HashProfile, row: &[u8]) -> Result<Vec<u8>, &'static str> {
  let physical_length = 24 + 2 * profile.width();
  decode_candidate_row(profile, row, false)?;
  Ok(row[..physical_length].to_vec())
}

fn build_candidate_page(profile: HashProfile) -> SamplePage {
  let records = vec![candidate_row(profile, 1, false), candidate_row(profile, 2, false)];
  let keys = records.iter().map(|record| candidate_key(profile, record).expect("candidate key")).collect::<Vec<_>>();
  build_page(DirectoryRole::Candidates, 101, 11, &records, &keys)
}

fn root_expiry_row(profile: HashProfile, ordinal: u8, retirement_commit: &[u8], reclaim_proof: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let mut row = vec![0u8; 40 + 3 * h];
  row[..h].copy_from_slice(&sample_root(profile, ordinal));
  put_i64(&mut row, h, 1_700_000_010_000 + i64::from(ordinal));
  put_i64(&mut row, h + 8, 1_700_000_000_000 + i64::from(ordinal));
  put_u64(&mut row, h + 16, if ordinal == 2 { 501 } else { 500 });
  put_u16(&mut row, h + 24, u16::from(ordinal));
  row[h + 26] = ordinal;
  if ordinal == 2 {
    row[h + 27] = 1;
  }
  if ordinal == 1 {
    fill_sequence(&mut row[h + 32..h + 32 + h], 0x91);
  } else {
    row[h + 32..h + 32 + h].copy_from_slice(retirement_commit);
    row[h + 32 + h..h + 32 + 2 * h].copy_from_slice(reclaim_proof);
    put_i64(&mut row, h + 32 + 2 * h, 1_700_100_000_000);
  }
  row
}

fn decode_root_expiry_row(profile: HashProfile, row: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if row.len() != 40 + 3 * h
    || row[..h].iter().all(|byte| *byte == 0)
    || read_i64(row, h)? <= 0
    || read_i64(row, h + 8)? <= 0
    || read_i64(row, h + 8)? > read_i64(row, h)?
    || read_u64(row, h + 16)? == 0
    || read_u16(row, h + 24)? == 0
    || !matches!(row[h + 26], 1 | 2)
    || row[h + 28..h + 32].iter().any(|byte| *byte != 0)
    || row[h + 32..h + 32 + h].iter().all(|byte| *byte == 0)
  {
    return Err("root_expiry_row");
  }
  let proof = &row[h + 32 + h..h + 32 + 2 * h];
  let expires = read_i64(row, h + 32 + 2 * h)?;
  match row[h + 26] {
    1 if row[h + 27] == 0 && proof.iter().all(|byte| *byte == 0) && expires == 0 => {}
    2 if row[h + 27] == 1 && proof.iter().any(|byte| *byte != 0) && expires >= read_i64(row, h)? => {}
    _ => return Err("root_expiry_row_state"),
  }
  Ok(row[..h].to_vec())
}

fn build_root_expiry_page(profile: HashProfile, retirement_commit: &[u8], reclaim_proof: &[u8]) -> SamplePage {
  let records =
    vec![root_expiry_row(profile, 1, retirement_commit, reclaim_proof), root_expiry_row(profile, 2, retirement_commit, reclaim_proof)];
  let keys = records.iter().map(|record| record[..profile.width()].to_vec()).collect::<Vec<_>>();
  build_page(DirectoryRole::RootExpiry, 502, 21, &records, &keys)
}

fn root_candidate_row(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut row = vec![0u8; 36 + 3 * h];
  row[..h].copy_from_slice(&sample_root(profile, 3));
  row[h] = 1;
  put_u16(&mut row, h + 2, 1);
  put_i64(&mut row, h + 4, 1_700_000_060_000);
  put_u64(&mut row, h + 12, 600);
  put_u64(&mut row, h + 20, 601);
  put_u64(&mut row, h + 28, 86_400_000);
  fill_sequence(&mut row[h + 36..h + 36 + h], 0xa1);
  fill_sequence(&mut row[h + 36 + h..], 0xc1);
  row
}

fn decode_root_candidate_row(profile: HashProfile, row: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if row.len() != 36 + 3 * h
    || row[..h].iter().all(|byte| *byte == 0)
    || row[h] != 1
    || row[h + 1] != 0
    || read_u16(row, h + 2)? == 0
    || read_i64(row, h + 4)? <= 0
    || read_u64(row, h + 12)? == 0
    || read_u64(row, h + 20)? < read_u64(row, h + 12)?
    || row[h + 36..h + 36 + h].iter().all(|byte| *byte == 0)
    || row[h + 36 + h..].iter().all(|byte| *byte == 0)
  {
    return Err("root_candidate_row");
  }
  Ok(row[..h].to_vec())
}

fn build_root_candidate_page(profile: HashProfile) -> SamplePage {
  let record = root_candidate_row(profile);
  let key = record[..profile.width()].to_vec();
  build_page(DirectoryRole::RootCandidates, 601, 41, &[record], &[key])
}

fn inventory_row(profile: HashProfile, state: u8) -> Vec<u8> {
  let h = profile.width();
  let physical = sample_incarnation(profile, state);
  let encoded = encode_physical_incarnation(profile, &physical);
  let physical_length = encoded.len();
  let mut row = vec![0u8; 68 + 5 * h];
  row[..physical_length].copy_from_slice(&encoded);
  row[physical_length] = state;
  row[physical_length + 1] = if state == 1 { 0 } else { state };
  let mut flags = 0u16;
  if state == 2 {
    flags |= 1;
    let replacement = encode_physical_incarnation(profile, &sample_incarnation(profile, 6));
    row[physical_length + 4..physical_length + 4 + replacement.len()].copy_from_slice(&replacement);
  }
  if state == 5 {
    flags |= 2;
  }
  put_u16(&mut row, physical_length + 2, flags);
  let tail = physical_length + 4 + physical_length;
  put_u64(&mut row, tail, 1_700_000_020_000 + u64::from(state));
  put_u64(&mut row, tail + 8, if state == 1 { 0 } else { 2_000 + u64::from(state) });
  if state == 5 {
    fill_sequence(&mut row[tail + 16..tail + 16 + h], 0xd1);
  }
  row
}

fn decode_inventory_row(profile: HashProfile, row: &[u8]) -> Result<PhysicalIncarnationId, &'static str> {
  let h = profile.width();
  let physical_length = 24 + 2 * h;
  if row.len() != 68 + 5 * h {
    return Err("inventory_row_length");
  }
  let physical = decode_physical_incarnation(profile, &row[..physical_length])?;
  let state = row[physical_length];
  let reason = row[physical_length + 1];
  let flags = read_u16(row, physical_length + 2)?;
  if !(1..=5).contains(&state) || flags & !3 != 0 || (state == 1) != (reason == 0) {
    return Err("inventory_row_state");
  }
  let replacement = &row[physical_length + 4..physical_length + 4 + physical_length];
  if flags & 1 != 0 {
    decode_physical_incarnation(profile, replacement)?;
  } else if replacement.iter().any(|byte| *byte != 0) {
    return Err("inventory_row_replacement");
  }
  let tail = physical_length + 4 + physical_length;
  if read_u64(row, tail)? == 0 || (state == 1 && read_u64(row, tail + 8)? != 0) || (state == 1 && flags != 0) {
    return Err("inventory_row_time_or_sequence");
  }
  let receipt = &row[tail + 16..tail + 16 + h];
  if (flags & 2 != 0) != receipt.iter().any(|byte| *byte != 0) || (state == 5) != (flags & 2 != 0) {
    return Err("inventory_row_receipt");
  }
  Ok(physical)
}

fn inventory_key(profile: HashProfile, row: &[u8]) -> Result<Vec<u8>, &'static str> {
  let physical = decode_inventory_row(profile, row)?;
  let mut key = Vec::with_capacity(8 + 24 + 2 * profile.width());
  key.extend_from_slice(&physical.wal_offset.to_le_bytes());
  key.extend_from_slice(&row[..24 + 2 * profile.width()]);
  Ok(key)
}

fn build_inventory_page(profile: HashProfile) -> SamplePage {
  let records = (1..=5).map(|state| inventory_row(profile, state)).collect::<Vec<_>>();
  let keys = records.iter().map(|record| inventory_key(profile, record).expect("inventory key")).collect::<Vec<_>>();
  build_page(DirectoryRole::PhysicalInventory, 301, 31, &records, &keys)
}

fn build_page(role: DirectoryRole, generation: u64, page_id: u64, records: &[Vec<u8>], keys: &[Vec<u8>]) -> SamplePage {
  assert!(!records.is_empty() && records.len() == keys.len());
  let records_length = records.iter().map(Vec::len).sum::<usize>();
  let lower = keys.first().expect("page lower fence");
  let upper = keys.last().expect("page upper fence");
  let mut body = vec![0u8; 64 + lower.len() + upper.len() + records_length];
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, role as u16);
  put_u32(&mut body, 8, lower.len() as u32);
  put_u32(&mut body, 12, upper.len() as u32);
  put_u32(&mut body, 16, records.len() as u32);
  put_u32(&mut body, 20, records.len() as u32);
  put_u64(&mut body, 24, records_length as u64);
  put_u64(&mut body, 32, records_length as u64);
  let mut cursor = 64;
  body[cursor..cursor + lower.len()].copy_from_slice(lower);
  cursor += lower.len();
  body[cursor..cursor + upper.len()].copy_from_slice(upper);
  cursor += upper.len();
  for record in records {
    body[cursor..cursor + record.len()].copy_from_slice(record);
    cursor += record.len();
  }
  let mut identity = Vec::with_capacity(42);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&catalog_id(role));
  identity.extend_from_slice(&(role as u16).to_le_bytes());
  identity.extend_from_slice(&page_id.to_le_bytes());
  let bytes = build_gc_value(role.page_kind(), generation, &identity, &body);
  SamplePage {
    role,
    catalog_id: catalog_id(role),
    generation,
    page_id,
    lower: lower.clone(),
    upper: upper.clone(),
    record_count: records.len() as u32,
    logical_bytes: records_length as u64,
    bytes,
  }
}

fn decode_page(profile: HashProfile, bytes: &[u8]) -> Result<DecodedPage, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_PAGE_LENGTH)?;
  let role = match artifact.kind {
    GcKind::CandidatePage => DirectoryRole::Candidates,
    GcKind::RootExpiryPage => DirectoryRole::RootExpiry,
    GcKind::PhysicalInventoryPage => DirectoryRole::PhysicalInventory,
    GcKind::RootCandidatePage => DirectoryRole::RootCandidates,
    _ => return Err("gc_page_kind"),
  };
  if artifact.identity.len() != 42
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..32] != catalog_id(role)
    || read_u16(artifact.identity, 32)? != role as u16
  {
    return Err("gc_page_identity");
  }
  let page_id = read_u64(artifact.identity, 34)?;
  let body = artifact.body;
  if page_id == 0 || body.len() < 64 {
    return Err("gc_page_identity_or_length");
  }
  let lower_length = read_u32(body, 8)? as usize;
  let upper_length = read_u32(body, 12)? as usize;
  let record_count = read_u32(body, 16)?;
  let records_length = usize::try_from(read_u64(body, 24)?).map_err(|_| "gc_page_records_length")?;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != role as u16
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
    || record_count == 0
    || read_u32(body, 20)? != record_count
    || read_u64(body, 32)? != records_length as u64
    || body[40..64].iter().any(|byte| *byte != 0)
    || 64usize
      .checked_add(lower_length)
      .and_then(|length| length.checked_add(upper_length))
      .and_then(|length| length.checked_add(records_length))
      != Some(body.len())
  {
    return Err("gc_page_header");
  }
  let lower = body[64..64 + lower_length].to_vec();
  let upper = body[64 + lower_length..64 + lower_length + upper_length].to_vec();
  let mut cursor = 64 + lower_length + upper_length;
  let mut previous: Option<Vec<u8>> = None;
  for _ in 0..record_count {
    let (record_length, key) = match role {
      DirectoryRole::Candidates => {
        let length = 52 + 2 * profile.width();
        let row = body.get(cursor..cursor + length).ok_or("candidate_page_truncated")?;
        let physical = decode_candidate_row(profile, row, false)?;
        (length, encode_physical_incarnation(profile, &physical))
      }
      DirectoryRole::RootExpiry => {
        let length = 40 + 3 * profile.width();
        let row = body.get(cursor..cursor + length).ok_or("root_expiry_page_truncated")?;
        (length, decode_root_expiry_row(profile, row)?)
      }
      DirectoryRole::PhysicalInventory => {
        let length = 68 + 5 * profile.width();
        let row = body.get(cursor..cursor + length).ok_or("inventory_page_truncated")?;
        (length, inventory_key(profile, row)?)
      }
      DirectoryRole::RootCandidates => {
        let length = 36 + 3 * profile.width();
        let row = body.get(cursor..cursor + length).ok_or("root_candidate_page_truncated")?;
        (length, decode_root_candidate_row(profile, row)?)
      }
    };
    if previous.as_ref().is_some_and(|prior| compare_keys(profile, role, prior, &key).is_ok_and(|ordering| ordering != Ordering::Less)) {
      return Err("gc_page_record_order");
    }
    previous = Some(key);
    cursor += record_length;
  }
  if cursor != body.len() || previous.as_deref() != Some(upper.as_slice()) {
    return Err("gc_page_records_or_upper");
  }
  let first_key = match role {
    DirectoryRole::Candidates => body
      .get(64 + lower_length + upper_length..64 + lower_length + upper_length + 24 + 2 * profile.width())
      .ok_or("gc_page_lower")?
      .to_vec(),
    DirectoryRole::RootExpiry => {
      body.get(64 + lower_length + upper_length..64 + lower_length + upper_length + profile.width()).ok_or("gc_page_lower")?.to_vec()
    }
    DirectoryRole::PhysicalInventory => {
      let start = 64 + lower_length + upper_length;
      inventory_key(profile, body.get(start..start + 68 + 5 * profile.width()).ok_or("gc_page_lower")?)?
    }
    DirectoryRole::RootCandidates => {
      body.get(64 + lower_length + upper_length..64 + lower_length + upper_length + profile.width()).ok_or("gc_page_lower")?.to_vec()
    }
  };
  if first_key != lower || compare_keys(profile, role, &lower, &upper)?.is_gt() {
    return Err("gc_page_fences");
  }
  Ok(DecodedPage { role, page_id, record_count })
}

fn compare_keys(profile: HashProfile, role: DirectoryRole, left: &[u8], right: &[u8]) -> Result<Ordering, &'static str> {
  match role {
    DirectoryRole::Candidates => {
      let left = decode_physical_incarnation(profile, left)?;
      let right = decode_physical_incarnation(profile, right)?;
      Ok(physical_compare(&left, &right))
    }
    DirectoryRole::RootExpiry | DirectoryRole::RootCandidates => {
      if left.len() != profile.width() || right.len() != profile.width() {
        return Err("root_expiry_key_length");
      }
      Ok(left.cmp(right))
    }
    DirectoryRole::PhysicalInventory => {
      let physical_length = 24 + 2 * profile.width();
      if left.len() != 8 + physical_length || right.len() != 8 + physical_length {
        return Err("inventory_key_length");
      }
      let left_physical = decode_physical_incarnation(profile, &left[8..])?;
      let right_physical = decode_physical_incarnation(profile, &right[8..])?;
      Ok(read_u64(left, 0)?.cmp(&read_u64(right, 0)?).then_with(|| physical_compare(&left_physical, &right_physical)))
    }
  }
}

fn build_leaf_directory(profile: HashProfile, page: &SamplePage) -> Vec<u8> {
  let h = profile.width();
  let descriptor_fixed = 72 + h;
  let entries_length = descriptor_fixed + page.lower.len() + page.upper.len();
  let mut body = vec![0u8; 80 + page.lower.len() + page.upper.len() + entries_length];
  put_u16(&mut body, 2, page.role as u16);
  put_u32(&mut body, 4, 1);
  put_u32(&mut body, 16, page.lower.len() as u32);
  put_u32(&mut body, 20, page.upper.len() as u32);
  put_u64(&mut body, 24, u64::from(page.record_count));
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
  put_u64(&mut body, cursor + 8, page.page_id);
  body[cursor + 16..cursor + 16 + h].copy_from_slice(&immutable_key(profile, page.role.page_kind(), &page.bytes));
  let fields = cursor + 16 + h;
  put_u64(&mut body, fields, page.generation);
  put_u64(&mut body, fields + 8, u64::from(page.record_count));
  put_u64(&mut body, fields + 24, page.logical_bytes);
  cursor += descriptor_fixed;
  body[cursor..cursor + page.lower.len()].copy_from_slice(&page.lower);
  cursor += page.lower.len();
  body[cursor..cursor + page.upper.len()].copy_from_slice(&page.upper);
  let mut identity = Vec::with_capacity(34);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&page.catalog_id);
  identity.extend_from_slice(&(page.role as u16).to_le_bytes());
  build_gc_value(GcKind::GcArtifactDirectoryNode, page.generation + 10, &identity, &body)
}

fn decode_directory(profile: HashProfile, bytes: &[u8]) -> Result<DecodedDirectory, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_DIRECTORY_LENGTH)?;
  if artifact.kind != GcKind::GcArtifactDirectoryNode || artifact.identity.len() != 34 || artifact.identity[..16] != database_id() {
    return Err("gc_directory_identity");
  }
  let role = DirectoryRole::from_id(read_u16(artifact.identity, 32)?).ok_or("gc_directory_role")?;
  if artifact.identity[16..32] != catalog_id(role) || artifact.body.len() < 80 {
    return Err("gc_directory_catalog_or_length");
  }
  let body = artifact.body;
  let lower_length = read_u32(body, 16)? as usize;
  let upper_length = read_u32(body, 20)? as usize;
  let entries_length = read_u32(body, 72)? as usize;
  if read_u16(body, 0)? != 0
    || read_u16(body, 2)? != role as u16
    || read_u32(body, 4)? != 1
    || read_u32(body, 8)? != 0
    || read_u32(body, 12)? != 0
    || lower_length == 0
    || upper_length == 0
    || read_u32(body, 76)? != 0
    || 80usize
      .checked_add(lower_length)
      .and_then(|length| length.checked_add(upper_length))
      .and_then(|length| length.checked_add(entries_length))
      != Some(body.len())
  {
    return Err("gc_directory_header");
  }
  let lower = body[80..80 + lower_length].to_vec();
  let upper = body[80 + lower_length..80 + lower_length + upper_length].to_vec();
  let cursor = 80 + lower_length + upper_length;
  let h = profile.width();
  let fixed = 72 + h;
  if entries_length != fixed + lower_length + upper_length
    || read_u32(body, cursor)? as usize != lower_length
    || read_u32(body, cursor + 4)? as usize != upper_length
  {
    return Err("gc_directory_descriptor_length");
  }
  let page_id = read_u64(body, cursor + 8)?;
  let child_hash = body[cursor + 16..cursor + 16 + h].to_vec();
  let fields = cursor + 16 + h;
  let child_generation = read_u64(body, fields)?;
  let record_count = read_u64(body, fields + 8)?;
  let tombstones = read_u64(body, fields + 16)?;
  let logical_bytes = read_u64(body, fields + 24)?;
  let key_start = cursor + fixed;
  if page_id == 0
    || child_hash.iter().all(|byte| *byte == 0)
    || child_generation == 0
    || child_generation > artifact.generation
    || record_count == 0
    || tombstones != 0
    || logical_bytes == 0
    || body[fields + 32..fields + 56].iter().any(|byte| *byte != 0)
    || body[key_start..key_start + lower_length] != lower
    || body[key_start + lower_length..] != upper
    || compare_keys(profile, role, &lower, &upper)?.is_gt()
    || read_u64(body, 24)? != record_count
    || read_u64(body, 32)? != 0
    || read_u64(body, 40)? != 1
    || read_u64(body, 48)? != logical_bytes
    || read_u64(body, 56)? != page_id
    || read_u64(body, 64)? != page_id
  {
    return Err("gc_directory_descriptor_or_aggregate");
  }
  Ok(DecodedDirectory {
    role,
    #[cfg(test)]
    generation: artifact.generation,
    #[cfg(test)]
    page_id,
    record_count,
    #[cfg(test)]
    lower,
    #[cfg(test)]
    upper,
    #[cfg(test)]
    child_hash,
  })
}

fn build_candidate_delta(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let rows = [(1u8, candidate_row(profile, 1, false)), (2u8, candidate_row(profile, 3, true))];
  let records_length = rows.iter().map(|(_, row)| 4 + row.len()).sum::<usize>();
  let mut body = vec![0u8; 16 + h + records_length];
  put_u16(&mut body, 4, 1);
  put_u32(&mut body, 8, rows.len() as u32);
  put_u32(&mut body, 12, records_length as u32);
  let mut cursor = 16 + h;
  for (operation, row) in rows {
    body[cursor] = operation;
    body[cursor + 4..cursor + 4 + row.len()].copy_from_slice(&row);
    cursor += 4 + row.len();
  }
  let mut identity = Vec::with_capacity(28);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&41u64.to_le_bytes());
  identity.extend_from_slice(&1u32.to_le_bytes());
  build_gc_value(GcKind::CandidateDelta, 41, &identity, &body)
}

fn decode_candidate_delta(profile: HashProfile, bytes: &[u8]) -> Result<u32, &'static str> {
  let artifact = decode_gc_value(bytes, 64 * 1024 * 1024)?;
  let h = profile.width();
  if artifact.kind != GcKind::CandidateDelta
    || artifact.identity.len() != 28
    || artifact.identity[..16] != database_id()
    || read_u64(artifact.identity, 16)? != artifact.generation
    || read_u32(artifact.identity, 24)? == 0
    || artifact.body.len() < 16 + h
  {
    return Err("candidate_delta_identity");
  }
  let body = artifact.body;
  let count = read_u32(body, 8)?;
  let records_length = read_u32(body, 12)? as usize;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 0
    || count == 0
    || body[16..16 + h].iter().any(|byte| *byte != 0)
    || 16usize.checked_add(h).and_then(|length| length.checked_add(records_length)) != Some(body.len())
  {
    return Err("candidate_delta_header");
  }
  let row_length = 52 + 2 * h;
  let mut cursor = 16 + h;
  let mut previous = None;
  for _ in 0..count {
    let operation = *body.get(cursor).ok_or("candidate_delta_truncated")?;
    if !matches!(operation, 1 | 2) || body.get(cursor + 1..cursor + 4).is_none_or(|reserved| reserved.iter().any(|byte| *byte != 0)) {
      return Err("candidate_delta_operation");
    }
    let row = body.get(cursor + 4..cursor + 4 + row_length).ok_or("candidate_delta_truncated")?;
    let physical = decode_candidate_row(profile, row, operation == 2)?;
    if previous.as_ref().is_some_and(|prior| physical_compare(prior, &physical) != Ordering::Less) {
      return Err("candidate_delta_order");
    }
    previous = Some(physical);
    cursor += 4 + row_length;
  }
  if cursor != body.len() {
    return Err("candidate_delta_trailing");
  }
  Ok(count)
}

fn build_root_expiry_manifest(profile: HashProfile, populated: bool, directory_root: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let generation: u64 = if populated { 513 } else { 511 };
  let mut body = vec![0u8; 124 + h];
  write_capabilities(&mut body[4..36], &[12, 17]);
  put_u64(&mut body, 36, 90 * 24 * 60 * 60 * 1_000);
  put_u64(&mut body, 44, 64 * 1024 * 1024);
  if populated {
    body[52..52 + h].copy_from_slice(directory_root);
  }
  put_u64(&mut body, 52 + h, if populated { 22 } else { 1 });
  put_u64(&mut body, 60 + h, if populated { 2 } else { 0 });
  put_u64(&mut body, 68 + h, if populated { (2 * (40 + 3 * h)) as u64 } else { 0 });
  put_u64(&mut body, 76 + h, u64::from(populated));
  put_u64(&mut body, 84 + h, if populated { (40 + 3 * h) as u64 } else { 0 });
  put_u64(&mut body, 92 + h, u64::from(populated));
  put_u64(&mut body, 100 + h, if populated { (40 + 3 * h) as u64 } else { 0 });
  if populated {
    put_i64(&mut body, 108 + h, 1_700_000_010_001);
    put_i64(&mut body, 116 + h, 1_700_000_010_002);
  }
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&generation.to_le_bytes());
  build_gc_value(GcKind::RootExpiryCatalogManifest, generation, &identity, &body)
}

fn build_root_lifecycle_manifest(profile: HashProfile, populated: bool, candidate_root: &[u8], expiry_manifest: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let generation: u64 = if populated { 612 } else { 600 };
  let mut body = vec![0u8; 108 + 3 * h];
  write_capabilities(&mut body[4..36], &[12, 17]);
  put_u64(&mut body, 36, generation);
  put_i64(&mut body, 44, 1_700_000_070_000 + i64::from(populated));
  put_u64(&mut body, 52, 602);
  fill_sequence(&mut body[60..60 + h], 0xe1);
  if populated {
    body[60 + h..60 + 2 * h].copy_from_slice(candidate_root);
    body[60 + 2 * h..60 + 3 * h].copy_from_slice(expiry_manifest);
  }
  put_u64(&mut body, 60 + 3 * h, if populated { 42 } else { 1 });
  put_u64(&mut body, 68 + 3 * h, u64::from(populated));
  put_u64(&mut body, 76 + 3 * h, u64::from(populated));
  put_u64(&mut body, 84 + 3 * h, if populated { 2 } else { 0 });
  put_u64(&mut body, 92 + 3 * h, if populated { (36 + 3 * h) as u64 } else { 0 });
  put_u64(&mut body, 100 + 3 * h, if populated { (2 * (40 + 3 * h)) as u64 } else { 0 });
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&generation.to_le_bytes());
  build_gc_value(GcKind::RootLifecycleManifest, generation, &identity, &body)
}

fn build_inventory_manifest(profile: HashProfile, populated: bool, directory_root: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let generation: u64 = if populated { 302 } else { 301 };
  let mut body = vec![0u8; 132 + 2 * h];
  write_capabilities(&mut body[4..36], &[12, 13]);
  put_u64(&mut body, 36, generation);
  put_u64(&mut body, 44, 1_700_000_030_000);
  fill_sequence(&mut body[52..52 + h], 0x61);
  put_u64(&mut body, 52 + h, 2_000_000);
  put_u64(&mut body, 60 + h, 3_000);
  put_u64(&mut body, 68 + h, 2_999);
  if populated {
    body[76 + h..76 + 2 * h].copy_from_slice(directory_root);
  }
  put_u64(&mut body, 76 + 2 * h, if populated { 32 } else { 1 });
  for index in 0..5 {
    put_u64(&mut body, 84 + 2 * h + index * 8, u64::from(populated));
  }
  put_u64(&mut body, 124 + 2 * h, if populated { (5 * (68 + 5 * h)) as u64 } else { 0 });
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&generation.to_le_bytes());
  build_gc_value(GcKind::PhysicalInventoryManifest, generation, &identity, &body)
}

fn build_quarantine_manifest(profile: HashProfile, populated: bool, directory_root: &[u8], root_expiry: &[u8], delta: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let generation: u64 = if populated { 42 } else { 41 };
  let delta_count = usize::from(populated);
  let mut body = vec![0u8; 100 + 6 * h + delta_count * h];
  write_capabilities(&mut body[4..36], &[12, 13, 15, 17]);
  put_u64(&mut body, 36, generation);
  put_u64(&mut body, 44, 1_700_000_040_000);
  for (index, start) in [0x21, 0x31, 0x41, 0x51].into_iter().enumerate() {
    fill_sequence(&mut body[52 + index * h..52 + (index + 1) * h], start);
  }
  if populated {
    body[52 + 4 * h..52 + 5 * h].copy_from_slice(directory_root);
  }
  body[52 + 5 * h..52 + 6 * h].copy_from_slice(root_expiry);
  put_u32(&mut body, 52 + 6 * h, delta_count as u32);
  put_u64(&mut body, 60 + 6 * h, if populated { 2 } else { 0 });
  put_u64(&mut body, 68 + 6 * h, if populated { (2 * (52 + 2 * h)) as u64 } else { 0 });
  put_u64(&mut body, 76 + 6 * h, if populated { 2 } else { 0 });
  put_u64(&mut body, 84 + 6 * h, if populated { (2 * (52 + 2 * h)) as u64 } else { 0 });
  put_u64(&mut body, 92 + 6 * h, if populated { 12 } else { 1 });
  if populated {
    body[100 + 6 * h..].copy_from_slice(delta);
  }
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&generation.to_le_bytes());
  build_gc_value(GcKind::QuarantineManifest, generation, &identity, &body)
}

fn decode_manifest(profile: HashProfile, bytes: &[u8]) -> Result<DecodedManifest, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  if artifact.identity.len() != 24 || artifact.identity[..16] != database_id() || read_u64(artifact.identity, 16)? != artifact.generation {
    return Err("gc_manifest_identity");
  }
  let (populated, record_count, secondary_count, root) = match artifact.kind {
    GcKind::RootExpiryCatalogManifest => decode_root_expiry_manifest_body(profile, artifact.body)?,
    GcKind::PhysicalInventoryManifest => decode_inventory_manifest_body(profile, artifact.generation, artifact.body)?,
    GcKind::QuarantineManifest => decode_quarantine_manifest_body(profile, artifact.generation, artifact.body)?,
    GcKind::RootLifecycleManifest => decode_root_lifecycle_manifest_body(profile, artifact.generation, artifact.body)?,
    _ => return Err("gc_manifest_kind"),
  };
  Ok(DecodedManifest { kind: artifact.kind, populated, record_count, secondary_count, root })
}

fn decode_root_expiry_manifest_body(profile: HashProfile, body: &[u8]) -> Result<(bool, u64, u64, Vec<u8>), &'static str> {
  let h = profile.width();
  if body.len() != 124 + h
    || read_u32(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 17])
    || read_u64(body, 36)? == 0
    || read_u64(body, 44)? == 0
  {
    return Err("root_expiry_manifest_header");
  }
  let root = body[52..52 + h].to_vec();
  let next = read_u64(body, 52 + h)?;
  let count = read_u64(body, 60 + h)?;
  let logical_bytes = read_u64(body, 68 + h)?;
  let mandatory_count = read_u64(body, 76 + h)?;
  let mandatory_bytes = read_u64(body, 84 + h)?;
  let optional_count = read_u64(body, 92 + h)?;
  let optional_bytes = read_u64(body, 100 + h)?;
  let oldest = read_i64(body, 108 + h)?;
  let newest = read_i64(body, 116 + h)?;
  let populated = root.iter().any(|byte| *byte != 0);
  if next == 0
    || mandatory_count.checked_add(optional_count) != Some(count)
    || mandatory_bytes.checked_add(optional_bytes) != Some(logical_bytes)
    || populated != (count != 0)
    || populated != (logical_bytes != 0)
    || if populated { oldest <= 0 || newest <= 0 } else { oldest != 0 || newest != 0 }
    || oldest > newest
  {
    return Err("root_expiry_manifest_state");
  }
  Ok((populated, count, mandatory_count, root))
}

fn decode_root_lifecycle_manifest_body(
  profile: HashProfile,
  generation: u64,
  body: &[u8],
) -> Result<(bool, u64, u64, Vec<u8>), &'static str> {
  let h = profile.width();
  if body.len() != 108 + 3 * h
    || read_u32(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 17])
    || read_u64(body, 36)? != generation
    || read_i64(body, 44)? <= 0
    || read_u64(body, 52)? == 0
    || body[60..60 + h].iter().all(|byte| *byte == 0)
  {
    return Err("root_lifecycle_manifest_header");
  }
  let candidate_root = body[60 + h..60 + 2 * h].to_vec();
  let expiry_root = &body[60 + 2 * h..60 + 3 * h];
  let next_page = read_u64(body, 60 + 3 * h)?;
  let candidate_count = read_u64(body, 68 + 3 * h)?;
  let pending_count = read_u64(body, 76 + 3 * h)?;
  let retired_count = read_u64(body, 84 + 3 * h)?;
  let candidate_bytes = read_u64(body, 92 + 3 * h)?;
  let expiry_bytes = read_u64(body, 100 + 3 * h)?;
  if next_page == 0
    || candidate_count != pending_count
    || (candidate_root.iter().any(|byte| *byte != 0)) != (candidate_count != 0)
    || (expiry_root.iter().any(|byte| *byte != 0)) != (retired_count != 0)
    || (candidate_count == 0) != (candidate_bytes == 0)
    || (retired_count == 0) != (expiry_bytes == 0)
  {
    return Err("root_lifecycle_manifest_state");
  }
  Ok((candidate_count != 0 || retired_count != 0, candidate_count, retired_count, candidate_root))
}

fn decode_inventory_manifest_body(profile: HashProfile, generation: u64, body: &[u8]) -> Result<(bool, u64, u64, Vec<u8>), &'static str> {
  let h = profile.width();
  if body.len() != 132 + 2 * h
    || read_u32(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 13])
    || read_u64(body, 36)? != generation
    || read_u64(body, 44)? == 0
    || body[52..52 + h].iter().all(|byte| *byte == 0)
    || read_u64(body, 52 + h)? == 0
    || read_u64(body, 60 + h)? == 0
    || read_u64(body, 68 + h)? > read_u64(body, 60 + h)?
    || read_u64(body, 76 + 2 * h)? == 0
  {
    return Err("inventory_manifest_header");
  }
  let root = body[76 + h..76 + 2 * h].to_vec();
  let counts = (0..5).map(|index| read_u64(body, 84 + 2 * h + index * 8)).collect::<Result<Vec<_>, _>>()?;
  let count = counts.iter().try_fold(0u64, |total, value| total.checked_add(*value).ok_or("inventory_manifest_count"))?;
  let logical_bytes = read_u64(body, 124 + 2 * h)?;
  let populated = root.iter().any(|byte| *byte != 0);
  if populated != (count != 0) || populated != (logical_bytes != 0) {
    return Err("inventory_manifest_state");
  }
  Ok((populated, count, 0, root))
}

fn decode_quarantine_manifest_body(profile: HashProfile, generation: u64, body: &[u8]) -> Result<(bool, u64, u64, Vec<u8>), &'static str> {
  let h = profile.width();
  if body.len() < 100 + 6 * h
    || body.len() > MAX_MANIFEST_LENGTH
    || read_u32(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 13, 15, 17])
    || read_u64(body, 36)? != generation
    || read_u64(body, 44)? == 0
    || (0..4).any(|index| body[52 + index * h..52 + (index + 1) * h].iter().all(|byte| *byte == 0))
    || body[52 + 5 * h..52 + 6 * h].iter().all(|byte| *byte == 0)
  {
    return Err("quarantine_manifest_header");
  }
  let root = body[52 + 4 * h..52 + 5 * h].to_vec();
  let captured_lifecycle = &body[52 + 5 * h..52 + 6 * h];
  let delta_count = read_u32(body, 52 + 6 * h)? as usize;
  let count = read_u64(body, 60 + 6 * h)?;
  let bytes_count = read_u64(body, 68 + 6 * h)?;
  let eligible_count = read_u64(body, 76 + 6 * h)?;
  let eligible_bytes = read_u64(body, 84 + 6 * h)?;
  let next_page = read_u64(body, 92 + 6 * h)?;
  if delta_count > MAX_DELTAS
    || 100usize.checked_add(6 * h).and_then(|length| length.checked_add(delta_count * h)) != Some(body.len())
    || body[56 + 6 * h..60 + 6 * h].iter().any(|byte| *byte != 0)
    || body[100 + 6 * h..].chunks_exact(h).any(|hash| hash.iter().all(|byte| *byte == 0))
    || next_page == 0
    || eligible_count > count
    || eligible_bytes > bytes_count
  {
    return Err("quarantine_manifest_formula");
  }
  let populated = count != 0;
  if populated != (bytes_count != 0)
    || (eligible_count == 0) != (eligible_bytes == 0)
    || (populated && root.iter().all(|byte| *byte == 0) && delta_count == 0)
  {
    return Err("quarantine_manifest_state");
  }
  let _captured_lifecycle = captured_lifecycle;
  Ok((populated, count, eligible_count, root))
}

fn build_root_retirement_commit(profile: HashProfile, prior_lifecycle: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let namespace_root = sample_root(profile, 2);
  let retirement_id = sequence_array(0xb1);
  let mut identity = Vec::with_capacity(32 + h);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&namespace_root);
  identity.extend_from_slice(&retirement_id);
  let mut body = vec![0u8; 72 + 4 * h];
  body[..16].copy_from_slice(&database_id());
  body[16..16 + h].copy_from_slice(&namespace_root);
  body[16 + h..32 + h].copy_from_slice(&retirement_id);
  put_i64(&mut body, 32 + h, 1_700_000_080_000);
  put_i64(&mut body, 40 + h, 1_700_000_060_000);
  put_u64(&mut body, 48 + h, 10_000);
  put_u64(&mut body, 56 + h, 501);
  put_u16(&mut body, 64 + h, 2);
  body[72 + h..72 + 2 * h].copy_from_slice(prior_lifecycle);
  fill_sequence(&mut body[72 + 2 * h..72 + 3 * h], 0xe1);
  fill_sequence(&mut body[72 + 3 * h..], 0xd1);
  build_gc_value(GcKind::RootRetirementCommit, 501, &identity, &body)
}

fn decode_root_retirement_commit(profile: HashProfile, bytes: &[u8]) -> Result<u64, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::RootRetirementCommit
    || artifact.identity.len() != 32 + h
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..16 + h].iter().all(|byte| *byte == 0)
    || artifact.identity[16 + h..].iter().all(|byte| *byte == 0)
    || artifact.body.len() != 72 + 4 * h
  {
    return Err("root_retirement_shape");
  }
  let body = artifact.body;
  let committed = read_i64(body, 32 + h)?;
  let pending = read_i64(body, 40 + h)?;
  let grace = read_u64(body, 48 + h)?;
  let mark_generation = read_u64(body, 56 + h)?;
  let eligible_at = pending.checked_add(i64::try_from(grace).map_err(|_| "root_retirement_grace")?).ok_or("root_retirement_grace")?;
  if body[..16] != artifact.identity[..16]
    || body[16..16 + h] != artifact.identity[16..16 + h]
    || body[16 + h..32 + h] != artifact.identity[16 + h..]
    || committed <= 0
    || pending <= 0
    || committed < eligible_at
    || mark_generation == 0
    || mark_generation != artifact.generation
    || read_u16(body, 64 + h)? == 0
    || read_u16(body, 66 + h)? != 0
    || read_u32(body, 68 + h)? != 0
    || (1..=3).any(|index| body[72 + index * h..72 + (index + 1) * h].iter().all(|byte| *byte == 0))
  {
    return Err("root_retirement_fields");
  }
  Ok(mark_generation)
}

fn build_root_object_reclaim_proof(profile: HashProfile, retirement_commit: &[u8], inventory_manifest: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let namespace_root = sample_root(profile, 2);
  let proof_id = sequence_array(0xc1);
  let mut identity = Vec::with_capacity(32 + h);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&namespace_root);
  identity.extend_from_slice(&proof_id);
  let mut body = vec![0u8; 40 + 6 * h];
  body[..16].copy_from_slice(&database_id());
  body[16..16 + h].copy_from_slice(&namespace_root);
  body[16 + h..16 + 2 * h].copy_from_slice(retirement_commit);
  put_i64(&mut body, 16 + 2 * h, 1_700_000_090_000);
  body[24 + 2 * h..24 + 3 * h].copy_from_slice(inventory_manifest);
  fill_sequence(&mut body[24 + 3 * h..24 + 4 * h], 0xa1);
  put_u64(&mut body, 24 + 4 * h, 1);
  fill_sequence(&mut body[32 + 4 * h..32 + 5 * h], 0xb1);
  put_u64(&mut body, 32 + 5 * h, 1);
  fill_sequence(&mut body[40 + 5 * h..], 0xc1);
  build_gc_value(GcKind::RootObjectReclaimProof, 302, &identity, &body)
}

fn decode_root_object_reclaim_proof(profile: HashProfile, bytes: &[u8]) -> Result<(u64, u64), &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::RootObjectReclaimProof
    || artifact.identity.len() != 32 + h
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..16 + h].iter().all(|byte| *byte == 0)
    || artifact.identity[16 + h..].iter().all(|byte| *byte == 0)
    || artifact.body.len() != 40 + 6 * h
  {
    return Err("root_reclaim_proof_shape");
  }
  let body = artifact.body;
  let incarnation_count = read_u64(body, 24 + 4 * h)?;
  let receipt_count = read_u64(body, 32 + 5 * h)?;
  if body[..16] != artifact.identity[..16]
    || body[16..16 + h] != artifact.identity[16..16 + h]
    || body[16 + h..16 + 2 * h].iter().all(|byte| *byte == 0)
    || read_i64(body, 16 + 2 * h)? <= 0
    || body[24 + 2 * h..24 + 3 * h].iter().all(|byte| *byte == 0)
    || body[24 + 3 * h..24 + 4 * h].iter().all(|byte| *byte == 0)
    || incarnation_count == 0
    || body[32 + 4 * h..32 + 5 * h].iter().all(|byte| *byte == 0)
    || receipt_count == 0
    || body[40 + 5 * h..].iter().all(|byte| *byte == 0)
  {
    return Err("root_reclaim_proof_fields");
  }
  Ok((incarnation_count, receipt_count))
}

fn build_retirement_journal(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let record_length = 72 + 4 * h;
  let mut record = vec![0u8; record_length];
  put_u32(&mut record, 0, record_length as u32);
  put_u16(&mut record, 4, 1);
  put_u64(&mut record, 8, 5_000);
  put_u64(&mut record, 16, 1_700_000_050_000);
  let old = encode_physical_incarnation(profile, &sample_incarnation(profile, 1));
  let replacement = encode_physical_incarnation(profile, &sample_incarnation(profile, 6));
  record[24..24 + old.len()].copy_from_slice(&old);
  record[24 + old.len()..].copy_from_slice(&replacement);
  let mut body = vec![0u8; 32 + h + record.len()];
  put_u32(&mut body, 0, 1);
  put_u16(&mut body, 4, 1);
  put_u64(&mut body, 8, 5_000);
  put_u64(&mut body, 16, 5_000);
  put_u32(&mut body, 24, 1);
  put_u32(&mut body, 28, record.len() as u32);
  body[32 + h..].copy_from_slice(&record);
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&1u64.to_le_bytes());
  build_gc_value(GcKind::RetirementJournalSegment, 401, &identity, &body)
}

fn decode_retirement_journal(profile: HashProfile, bytes: &[u8]) -> Result<u32, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_PAGE_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::RetirementJournalSegment
    || artifact.identity.len() != 24
    || artifact.identity[..16] != database_id()
    || read_u64(artifact.identity, 16)? == 0
    || artifact.body.len() < 32 + h
  {
    return Err("retirement_journal_identity");
  }
  let body = artifact.body;
  let flags = read_u32(body, 0)?;
  let first = read_u64(body, 8)?;
  let last = read_u64(body, 16)?;
  let count = read_u32(body, 24)?;
  let records_length = read_u32(body, 28)? as usize;
  if flags & !1 != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 0
    || first == 0
    || first > last
    || count == 0
    || 32usize.checked_add(h).and_then(|length| length.checked_add(records_length)) != Some(body.len())
    || ((flags & 1 != 0) != body[32..32 + h].iter().all(|byte| *byte == 0))
  {
    return Err("retirement_journal_header");
  }
  let mut cursor = 32 + h;
  let mut first_observed = None;
  let mut previous: Option<(u64, PhysicalIncarnationId)> = None;
  for _ in 0..count {
    let record_length = read_u32(body, cursor)? as usize;
    if record_length != 72 + 4 * h || cursor.checked_add(record_length).is_none_or(|end| end > body.len()) {
      return Err("retirement_record_length");
    }
    let reason = read_u16(body, cursor + 4)?;
    let sequence = read_u64(body, cursor + 8)?;
    let retired_at = read_u64(body, cursor + 16)?;
    let physical_length = 24 + 2 * h;
    let old = decode_physical_incarnation(profile, &body[cursor + 24..cursor + 24 + physical_length])?;
    let replacement = decode_physical_incarnation(profile, &body[cursor + 24 + physical_length..cursor + record_length])?;
    if !(1..=5).contains(&reason)
      || read_u16(body, cursor + 6)? != 0
      || sequence == 0
      || retired_at == 0
      || old == replacement
      || previous.as_ref().is_some_and(|(prior_sequence, prior_old)| {
        *prior_sequence > sequence || (*prior_sequence == sequence && physical_compare(prior_old, &old) != Ordering::Less)
      })
    {
      return Err("retirement_record_fields");
    }
    first_observed.get_or_insert(sequence);
    previous = Some((sequence, old));
    cursor += record_length;
  }
  if cursor != body.len() || first_observed != Some(first) || previous.as_ref().map(|(sequence, _)| *sequence) != Some(last) {
    return Err("retirement_journal_order");
  }
  Ok(count)
}

fn write_capabilities(bytes: &mut [u8], bits: &[usize]) {
  assert_eq!(bytes.len(), CAPABILITIES_LENGTH);
  for bit in bits {
    bytes[bit / 8] |= 1 << (bit % 8);
  }
}

fn valid_capabilities(bytes: &[u8], bits: &[usize]) -> bool {
  if bytes.len() != CAPABILITIES_LENGTH {
    return false;
  }
  let mut expected = [0u8; CAPABILITIES_LENGTH];
  write_capabilities(&mut expected, bits);
  bytes == expected
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, &'static str> {
  Ok(i64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("gc_artifact_truncated")?.try_into().map_err(|_| "gc_artifact_truncated")?))
}

fn fill_sequence(bytes: &mut [u8], start: u8) {
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(index as u8);
  }
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repair_crc(bytes: &mut [u8]) {
    let crc_offset = bytes.len() - 4;
    put_u32(bytes, crc_offset, crc32fast::hash(&bytes[..crc_offset]));
  }

  #[test]
  fn state_fixtures_round_trip_and_keys_match() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn every_state_fixture_byte_is_crc_or_structure_protected() {
    for case in fixture_cases() {
      for index in 0..case.bytes.len() {
        let mut changed = case.bytes.clone();
        changed[index] ^= 1;
        assert!(observe(case.profile, &changed).0.starts_with("error:"), "fixture {} byte {index}", case.id);
      }
    }
  }

  #[test]
  fn directories_name_the_exact_child_page_and_manifests_name_the_exact_directory() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let retirement = vec![0xa5; profile.width()];
      let proof = vec![0x5a; profile.width()];
      for page in [
        build_candidate_page(profile),
        build_root_expiry_page(profile, &retirement, &proof),
        build_inventory_page(profile),
        build_root_candidate_page(profile),
      ] {
        let directory_bytes = build_leaf_directory(profile, &page);
        let directory = decode_directory(profile, &directory_bytes).unwrap();
        assert_eq!(directory.child_hash, immutable_key(profile, page.role.page_kind(), &page.bytes));
        assert_eq!(directory.page_id, page.page_id);
        assert_eq!(directory.lower, page.lower);
        assert_eq!(directory.upper, page.upper);
        assert_eq!(directory.generation, page.generation + 10);
      }

      let root_page = build_root_expiry_page(profile, &retirement, &proof);
      let root_directory = build_leaf_directory(profile, &root_page);
      let root_manifest =
        build_root_expiry_manifest(profile, true, &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &root_directory));
      assert_eq!(
        decode_manifest(profile, &root_manifest).unwrap().root,
        immutable_key(profile, GcKind::GcArtifactDirectoryNode, &root_directory)
      );

      let inventory_page = build_inventory_page(profile);
      let inventory_directory = build_leaf_directory(profile, &inventory_page);
      let inventory_manifest =
        build_inventory_manifest(profile, true, &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &inventory_directory));
      assert_eq!(
        decode_manifest(profile, &inventory_manifest).unwrap().root,
        immutable_key(profile, GcKind::GcArtifactDirectoryNode, &inventory_directory)
      );
    }
  }

  #[test]
  fn lifecycle_graph_uses_exact_typed_edges_without_a_content_hash_cycle() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let inventory_page = build_inventory_page(profile);
      let inventory_directory = build_leaf_directory(profile, &inventory_page);
      let inventory_manifest =
        build_inventory_manifest(profile, true, &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &inventory_directory));
      let lifecycle_prior = build_root_lifecycle_manifest(profile, false, &[], &[]);
      let lifecycle_prior_hash = immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle_prior);
      let retirement = build_root_retirement_commit(profile, &lifecycle_prior_hash);
      let retirement_hash = immutable_key(profile, GcKind::RootRetirementCommit, &retirement);
      let inventory_hash = immutable_key(profile, GcKind::PhysicalInventoryManifest, &inventory_manifest);
      let proof = build_root_object_reclaim_proof(profile, &retirement_hash, &inventory_hash);
      let proof_hash = immutable_key(profile, GcKind::RootObjectReclaimProof, &proof);
      let expiry_page = build_root_expiry_page(profile, &retirement_hash, &proof_hash);
      let expiry_directory = build_leaf_directory(profile, &expiry_page);
      let expiry_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &expiry_directory);
      let expiry_manifest = build_root_expiry_manifest(profile, true, &expiry_directory_hash);
      let expiry_manifest_hash = immutable_key(profile, GcKind::RootExpiryCatalogManifest, &expiry_manifest);
      let candidate_page = build_root_candidate_page(profile);
      let candidate_directory = build_leaf_directory(profile, &candidate_page);
      let candidate_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &candidate_directory);
      let lifecycle = build_root_lifecycle_manifest(profile, true, &candidate_directory_hash, &expiry_manifest_hash);

      let retirement_body = decode_gc_value(&retirement, MAX_MANIFEST_LENGTH).unwrap().body;
      assert_eq!(&retirement_body[72 + h..72 + 2 * h], lifecycle_prior_hash);
      assert_ne!(&retirement_body[72 + h..72 + 2 * h], immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle));

      let proof_body = decode_gc_value(&proof, MAX_MANIFEST_LENGTH).unwrap().body;
      assert_eq!(&proof_body[16 + h..16 + 2 * h], retirement_hash);
      assert_eq!(&proof_body[24 + 2 * h..24 + 3 * h], inventory_hash);

      let expiry_body = decode_gc_value(&expiry_page.bytes, MAX_PAGE_LENGTH).unwrap().body;
      let expiry_record_length = 40 + 3 * h;
      let reclaimed_record = 64 + 2 * h + expiry_record_length;
      assert_eq!(&expiry_body[reclaimed_record + h + 32..reclaimed_record + h + 32 + h], retirement_hash);
      assert_eq!(&expiry_body[reclaimed_record + h + 32 + h..reclaimed_record + h + 32 + 2 * h], proof_hash);

      let expiry_manifest_body = decode_gc_value(&expiry_manifest, MAX_MANIFEST_LENGTH).unwrap().body;
      assert_eq!(&expiry_manifest_body[52..52 + h], expiry_directory_hash);

      let lifecycle_body = decode_gc_value(&lifecycle, MAX_MANIFEST_LENGTH).unwrap().body;
      assert_eq!(&lifecycle_body[60 + h..60 + 2 * h], candidate_directory_hash);
      assert_eq!(&lifecycle_body[60 + 2 * h..60 + 3 * h], expiry_manifest_hash);

      let physical_candidates = build_candidate_page(profile);
      let physical_directory = build_leaf_directory(profile, &physical_candidates);
      let delta = build_candidate_delta(profile);
      let quarantine = build_quarantine_manifest(
        profile,
        true,
        &immutable_key(profile, GcKind::GcArtifactDirectoryNode, &physical_directory),
        &immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle),
        &immutable_key(profile, GcKind::CandidateDelta, &delta),
      );
      let quarantine_body = decode_gc_value(&quarantine, MAX_MANIFEST_LENGTH).unwrap().body;
      assert_eq!(&quarantine_body[52 + 5 * h..52 + 6 * h], immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle));
      assert_eq!(&quarantine_body[100 + 6 * h..], immutable_key(profile, GcKind::CandidateDelta, &delta));
    }
  }

  #[test]
  fn lifecycle_candidate_and_expiry_catalogs_are_independently_optional() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let candidate_root = vec![0x41; h];
      let expiry_root = vec![0x82; h];
      let both = build_root_lifecycle_manifest(profile, true, &candidate_root, &expiry_root);

      let mut expiry_only = both.clone();
      let body = 32 + read_u16(&expiry_only, 16).unwrap() as usize;
      expiry_only[body + 60 + h..body + 60 + 2 * h].fill(0);
      put_u64(&mut expiry_only, body + 68 + 3 * h, 0);
      put_u64(&mut expiry_only, body + 76 + 3 * h, 0);
      put_u64(&mut expiry_only, body + 92 + 3 * h, 0);
      repair_crc(&mut expiry_only);
      let decoded = decode_manifest(profile, &expiry_only).unwrap();
      assert_eq!((decoded.record_count, decoded.secondary_count), (0, 2));

      let mut candidates_only = both;
      let body = 32 + read_u16(&candidates_only, 16).unwrap() as usize;
      candidates_only[body + 60 + 2 * h..body + 60 + 3 * h].fill(0);
      put_u64(&mut candidates_only, body + 84 + 3 * h, 0);
      put_u64(&mut candidates_only, body + 100 + 3 * h, 0);
      repair_crc(&mut candidates_only);
      let decoded = decode_manifest(profile, &candidates_only).unwrap();
      assert_eq!((decoded.record_count, decoded.secondary_count), (1, 0));
    }
  }

  #[test]
  fn candidate_delta_overlay_preserves_two_base_candidates() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let delta = build_candidate_delta(profile);
      let body = decode_gc_value(&delta, 64 * 1024 * 1024).unwrap().body;
      let row_length = 52 + 2 * profile.width();
      let first = 16 + profile.width();
      let second = first + 4 + row_length;
      assert_eq!(body[first], 1);
      assert_eq!(body[second], 2);
      let set = decode_candidate_row(profile, &body[first + 4..first + 4 + row_length], false).unwrap();
      let clear = decode_candidate_row(profile, &body[second + 4..second + 4 + row_length], true).unwrap();
      assert_eq!(set, sample_incarnation(profile, 1));
      assert_eq!(clear, sample_incarnation(profile, 3));
      assert_ne!(clear, sample_incarnation(profile, 2));
    }
  }

  #[test]
  fn exact_manifest_capabilities_reject_missing_or_extra_bits_after_crc_repair() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      for mut manifest in [
        build_root_expiry_manifest(profile, false, &[]),
        build_inventory_manifest(profile, false, &[]),
        build_root_lifecycle_manifest(profile, false, &[], &[]),
        build_quarantine_manifest(
          profile,
          false,
          &[],
          &immutable_key(profile, GcKind::RootLifecycleManifest, &build_root_lifecycle_manifest(profile, false, &[], &[])),
          &[],
        ),
      ] {
        let body = 32 + read_u16(&manifest, 16).unwrap() as usize;
        manifest[body + 4] ^= 1;
        repair_crc(&mut manifest);
        assert!(decode_manifest(profile, &manifest).is_err());
      }
    }
  }

  #[test]
  fn candidate_eligibility_requires_later_complete_mark_and_frozen_grace() {
    let profile = HashProfile::Blake3_256;
    let row = candidate_row(profile, 1, false);
    let physical_length = 24 + 2 * profile.width();
    let pending = read_u64(&row, physical_length + 4).unwrap();
    let first_generation = read_u64(&row, physical_length + 12).unwrap();
    let grace = read_u64(&row, physical_length + 20).unwrap();
    assert!(!candidate_is_eligible(first_generation, first_generation, pending.saturating_add(grace), pending, grace));
    assert!(!candidate_is_eligible(
      first_generation + 1,
      first_generation,
      pending.saturating_add(grace).saturating_sub(1),
      pending,
      grace
    ));
    assert!(candidate_is_eligible(first_generation + 1, first_generation, pending.saturating_add(grace), pending, grace));
    assert!(!candidate_is_eligible(first_generation + 1, first_generation, u64::MAX - 1, u64::MAX - 2, 10));
    assert!(!candidate_is_eligible(first_generation + 1, first_generation, u64::MAX, u64::MAX - 2, 10));
  }

  #[test]
  fn orphan_inventory_rows_do_not_require_a_retirement_publication_sequence() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let mut row = inventory_row(profile, 3);
      let physical_length = 24 + 2 * profile.width();
      put_u64(&mut row, physical_length + 4 + physical_length + 8, 0);
      assert!(decode_inventory_row(profile, &row).is_ok());
    }
  }

  #[test]
  fn repaired_crc_rejects_manifest_page_delta_and_journal_semantic_corruption() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let mut page = build_candidate_page(profile).bytes;
      let identity_length = read_u16(&page, 16).unwrap() as usize;
      let body = 32 + identity_length;
      page[body + 40] = 1;
      repair_crc(&mut page);
      assert!(decode_page(profile, &page).is_err());

      let mut delta = build_candidate_delta(profile);
      let body = 32 + read_u16(&delta, 16).unwrap() as usize;
      delta[body + 16 + profile.width()] = 3;
      repair_crc(&mut delta);
      assert!(decode_candidate_delta(profile, &delta).is_err());

      let lifecycle = build_root_lifecycle_manifest(profile, false, &[], &[]);
      let mut manifest =
        build_quarantine_manifest(profile, false, &[], &immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle), &[]);
      let body = 32 + read_u16(&manifest, 16).unwrap() as usize;
      put_u64(&mut manifest, body + 60 + 6 * profile.width(), 1);
      repair_crc(&mut manifest);
      assert!(decode_manifest(profile, &manifest).is_err());

      let lifecycle = build_root_lifecycle_manifest(profile, false, &[], &[]);
      let mut missing_lifecycle =
        build_quarantine_manifest(profile, false, &[], &immutable_key(profile, GcKind::RootLifecycleManifest, &lifecycle), &[]);
      let body = 32 + read_u16(&missing_lifecycle, 16).unwrap() as usize;
      missing_lifecycle[body + 52 + 5 * profile.width()..body + 52 + 6 * profile.width()].fill(0);
      repair_crc(&mut missing_lifecycle);
      assert!(decode_manifest(profile, &missing_lifecycle).is_err());

      let mut journal = build_retirement_journal(profile);
      let body = 32 + read_u16(&journal, 16).unwrap() as usize;
      put_u16(&mut journal, body + 32 + profile.width() + 4, 9);
      repair_crc(&mut journal);
      assert!(decode_retirement_journal(profile, &journal).is_err());

      let mut lifecycle = build_root_lifecycle_manifest(profile, true, &vec![0x41; profile.width()], &vec![0x82; profile.width()]);
      let body = 32 + read_u16(&lifecycle, 16).unwrap() as usize;
      put_u64(&mut lifecycle, body + 76 + 3 * profile.width(), 2);
      repair_crc(&mut lifecycle);
      assert!(decode_manifest(profile, &lifecycle).is_err());

      let mut root_candidate = build_root_candidate_page(profile).bytes;
      let body = 32 + read_u16(&root_candidate, 16).unwrap() as usize;
      let row = body + 64 + 2 * profile.width();
      root_candidate[row + profile.width()] = 2;
      repair_crc(&mut root_candidate);
      assert!(decode_page(profile, &root_candidate).is_err());

      let mut expiry_page = build_root_expiry_page(profile, &vec![0xa5; profile.width()], &vec![0x5a; profile.width()]).bytes;
      let body = 32 + read_u16(&expiry_page, 16).unwrap() as usize;
      let row = body + 64 + 2 * profile.width();
      expiry_page[row + profile.width() + 27] = 1;
      repair_crc(&mut expiry_page);
      assert!(decode_page(profile, &expiry_page).is_err());

      let mut retirement = build_root_retirement_commit(profile, &vec![0x41; profile.width()]);
      let body = 32 + read_u16(&retirement, 16).unwrap() as usize;
      put_u16(&mut retirement, body + 66 + profile.width(), 1);
      repair_crc(&mut retirement);
      assert!(decode_root_retirement_commit(profile, &retirement).is_err());

      let mut proof = build_root_object_reclaim_proof(profile, &vec![0x41; profile.width()], &vec![0x82; profile.width()]);
      let body = 32 + read_u16(&proof, 16).unwrap() as usize;
      put_u64(&mut proof, body + 24 + 4 * profile.width(), 0);
      repair_crc(&mut proof);
      assert!(decode_root_object_reclaim_proof(profile, &proof).is_err());
    }
  }

  fn candidate_is_eligible(active_generation: u64, first_generation: u64, now: u64, pending: u64, grace: u64) -> bool {
    active_generation > first_generation && pending.checked_add(grace).is_some_and(|eligible_at| now >= eligible_at)
  }
}
