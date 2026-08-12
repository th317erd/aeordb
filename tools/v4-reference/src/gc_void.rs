use std::cmp::Ordering;

use crate::core::HashProfile;
use crate::gc::{
  build_gc_value, decode_gc_value, decode_physical_incarnation, encode_physical_incarnation, immutable_key, put_u16, put_u32, put_u64,
  read_u16, read_u32, read_u64, GcFixtureCase, GcFormat, GcKind, PhysicalIncarnationId,
};

const MAX_MANIFEST_LENGTH: usize = 1024 * 1024;
const MAX_PAGE_LENGTH: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1024 * 1024;
const MAX_SWEEP_LENGTH: usize = 16 * 1024 * 1024;
const MAX_CANDIDATES: usize = 4_096;
const CAPABILITIES_LENGTH: usize = 32;
const VOID_CAPABILITY_BITS: &[usize] = &[12, 13, 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoidDirectoryRole {
  FreeExtents = 4,
  Claims = 5,
}

impl VoidDirectoryRole {
  fn from_id(id: u16) -> Option<Self> {
    match id {
      4 => Some(Self::FreeExtents),
      5 => Some(Self::Claims),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::FreeExtents => "void-free-extents",
      Self::Claims => "void-claims",
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VoidExtent {
  offset: u64,
  length: u32,
  proposal_hash: Vec<u8>,
  quarantine_hash: Vec<u8>,
  incarnation_digest: Vec<u8>,
  reclaim_commit_sequence: u64,
  void_generation: u64,
}

#[derive(Debug)]
struct DecodedVoidManifest {
  generation: u64,
  #[cfg(test)]
  published_at_ms: i64,
  free_count: u64,
  #[cfg(test)]
  free_bytes: u64,
  claim_count: u64,
  #[cfg(test)]
  claimed_bytes: u64,
  #[cfg(test)]
  free_root: Vec<u8>,
  #[cfg(test)]
  claim_root: Vec<u8>,
}

#[derive(Debug)]
struct DecodedDirectory {
  role: VoidDirectoryRole,
  live_count: u64,
  #[cfg(test)]
  child_hash: Vec<u8>,
}

pub fn fixture_cases() -> Vec<GcFixtureCase> {
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let proposal = build_sweep_proposal(profile);
    let proposal_hash = immutable_key(profile, GcKind::SweepProposal, &proposal);
    let quarantine_hash = patterned_hash(profile, 0x31);
    let all_extents = sample_extents(profile, &proposal_hash, &quarantine_hash);
    let remaining_extents = vec![all_extents[1].clone()];

    let source_page = build_void_extent_page(profile, 1, &all_extents);
    let source_page_hash = immutable_key(profile, GcKind::VoidExtentPage, &source_page);
    let source_directory = build_directory(
      profile,
      VoidDirectoryRole::FreeExtents,
      1,
      &all_extents[0].offset.to_le_bytes(),
      &all_extents[1].offset.to_le_bytes(),
      41,
      &source_page_hash,
      2,
      all_extents.iter().map(|extent| u64::from(extent.length)).sum(),
    );
    let source_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &source_directory);

    let source_manifest = build_void_manifest(
      profile,
      1,
      &source_directory_hash,
      &[],
      2,
      all_extents.iter().map(|extent| u64::from(extent.length)).sum(),
      0,
      0,
      0,
    );
    let source_manifest_hash = immutable_key(profile, GcKind::VoidCatalogManifest, &source_manifest);
    let claim = build_void_claim(profile, &source_manifest_hash, &all_extents[0]);
    let claim_hash = immutable_key(profile, GcKind::VoidClaim, &claim);
    let claim_directory =
      build_directory(profile, VoidDirectoryRole::Claims, 2, &claim_id(), &claim_id(), 0, &claim_hash, 1, claim.len() as u64);
    let claim_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &claim_directory);

    let remaining_page = build_void_extent_page(profile, 2, &remaining_extents);
    let remaining_page_hash = immutable_key(profile, GcKind::VoidExtentPage, &remaining_page);
    let remaining_directory = build_directory(
      profile,
      VoidDirectoryRole::FreeExtents,
      2,
      &remaining_extents[0].offset.to_le_bytes(),
      &remaining_extents[0].offset.to_le_bytes(),
      42,
      &remaining_page_hash,
      1,
      u64::from(remaining_extents[0].length),
    );
    let remaining_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &remaining_directory);

    let outstanding_manifest = build_void_manifest(
      profile,
      2,
      &remaining_directory_hash,
      &claim_directory_hash,
      1,
      u64::from(remaining_extents[0].length),
      1,
      u64::from(all_extents[0].length),
      1,
    );
    let outstanding_manifest_hash = immutable_key(profile, GcKind::VoidCatalogManifest, &outstanding_manifest);
    let settled_manifest =
      build_void_manifest(profile, 3, &remaining_directory_hash, &[], 1, u64::from(remaining_extents[0].length), 0, 0, 2);
    let settled_manifest_hash = immutable_key(profile, GcKind::VoidCatalogManifest, &settled_manifest);
    let empty_manifest = build_void_manifest(profile, 4, &[], &[], 0, 0, 0, 0, 3);

    let commit_receipt = build_sweep_receipt(profile, false, &proposal_hash, &outstanding_manifest_hash);
    let recovered_receipt = build_sweep_receipt(profile, true, &proposal_hash, &outstanding_manifest_hash);
    let settlement = build_settlement_receipt(profile, false, 1, &outstanding_manifest_hash, &settled_manifest_hash);

    for (id_suffix, bytes, expected, relation) in [
      ("sweep-proposal", proposal, "gc:proposal:sweep:candidates=2:mark=501".to_string(), "roots:two-quarantine-candidates"),
      ("void-extent-page-source", source_page, "gc:page:void-free-extents:records=2".to_string(), "receipt-backed-source-extents"),
      (
        "void-free-directory-source",
        source_directory,
        "gc:directory:void-free-extents:records=2".to_string(),
        "indexes:VoidExtentPage-source",
      ),
      ("void-extent-page-remaining", remaining_page, "gc:page:void-free-extents:records=1".to_string(), "claim-removed-before-overwrite"),
      (
        "void-free-directory-remaining",
        remaining_directory,
        "gc:directory:void-free-extents:records=1".to_string(),
        "indexes:VoidExtentPage-remaining",
      ),
      (
        "void-catalog-empty",
        empty_manifest,
        "gc:manifest:void-catalog:empty:free=0:claims=0:generation=4".to_string(),
        "allocator-authority-empty",
      ),
      (
        "void-catalog-source",
        source_manifest,
        "gc:manifest:void-catalog:populated:free=2:claims=0:generation=1".to_string(),
        "source-before-claim",
      ),
      ("void-claim", claim, "gc:claim:void:extents=1:bytes=4097".to_string(), "immutable-reservation-evidence"),
      ("void-claims-directory", claim_directory, "gc:directory:void-claims:records=1".to_string(), "indexes:immutable-VoidClaim"),
      (
        "void-catalog-outstanding",
        outstanding_manifest,
        "gc:manifest:void-catalog:populated:free=1:claims=1:generation=2".to_string(),
        "claim-presence-means-outstanding",
      ),
      (
        "void-catalog-settled",
        settled_manifest,
        "gc:manifest:void-catalog:populated:free=1:claims=0:generation=3".to_string(),
        "claim-omission-means-settled",
      ),
      (
        "sweep-commit-receipt",
        commit_receipt,
        "gc:receipt:sweep-commit:outcomes=2:reclaimed=1:skipped=1:failed=0".to_string(),
        "post-void-publication-audit",
      ),
      (
        "sweep-recovered-receipt",
        recovered_receipt,
        "gc:receipt:sweep-recovered:outcomes=2:reclaimed=1:skipped=1:failed=0".to_string(),
        "startup-idempotent-recovery",
      ),
      (
        "void-claim-settlement",
        settlement,
        "gc:receipt:void-claim-settlement:settled:used=1:unused=0".to_string(),
        "non-authoritative-idempotent-evidence",
      ),
    ] {
      let kind = GcKind::from_id(read_u16(&bytes, 6).expect("fixture kind")).expect("registered fixture kind");
      cases.push(GcFixtureCase {
        id: leak(format!("agca-{}-{id_suffix}", profile.label())),
        format: GcFormat::GcArtifactV1,
        profile,
        expected: leak(expected),
        relation: Some(relation),
        canonical_key: Some(hex::encode(immutable_key(profile, kind, &bytes))),
        bytes,
      });
    }
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  let kind = read_u16(bytes, 6).ok().and_then(GcKind::from_id);
  let result = match kind {
    Some(GcKind::SweepProposal) => decode_sweep_proposal(profile, bytes),
    Some(GcKind::SweepCommitReceipt) => decode_sweep_receipt(profile, bytes, false),
    Some(GcKind::RecoveredSweepReceipt) => decode_sweep_receipt(profile, bytes, true),
    Some(GcKind::VoidCatalogManifest) => decode_void_manifest(profile, bytes).map(|manifest| {
      format!(
        "gc:manifest:void-catalog:{}:free={}:claims={}:generation={}",
        if manifest.free_count == 0 && manifest.claim_count == 0 { "empty" } else { "populated" },
        manifest.free_count,
        manifest.claim_count,
        manifest.generation
      )
    }),
    Some(GcKind::VoidExtentPage) => {
      decode_void_extent_page(profile, bytes).map(|extents| format!("gc:page:void-free-extents:records={}", extents.len()))
    }
    Some(GcKind::VoidClaim) => {
      decode_void_claim(profile, bytes).map(|(count, total, _, _)| format!("gc:claim:void:extents={count}:bytes={total}"))
    }
    Some(GcKind::GcArtifactDirectoryNode) => {
      decode_directory(profile, bytes).map(|directory| format!("gc:directory:{}:records={}", directory.role.name(), directory.live_count))
    }
    Some(GcKind::VoidClaimSettlementReceipt) => decode_settlement_receipt(profile, bytes),
    _ => Err("sweep_void_kind"),
  };
  match result {
    Ok(observed) => {
      let key = kind.map(|kind| hex::encode(immutable_key(profile, kind, bytes)));
      (observed, key)
    }
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let kind = read_u16(bytes, 6).ok().and_then(GcKind::from_id).map_or("invalid", GcKind::name);
  vec![
    "envelope +0x000 len 32: AGCA common envelope".to_string(),
    format!("envelope artifact_kind: {kind}"),
    format!("body: corrected sweep/Void contract for H={}", profile.width()),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

pub(crate) fn is_void_directory(bytes: &[u8]) -> bool {
  read_u16(bytes, 6).ok() == Some(GcKind::GcArtifactDirectoryNode.id())
    && read_u16(bytes, 64).ok().and_then(VoidDirectoryRole::from_id).is_some()
}

fn build_sweep_proposal(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let candidates = [sample_incarnation(profile, 1), sample_incarnation(profile, 2)];
  let records = candidates.iter().flat_map(|candidate| encode_physical_incarnation(profile, candidate)).collect::<Vec<_>>();
  let mut digest_input = Vec::with_capacity(32 + records.len());
  digest_input.extend_from_slice(b"aeordb.sweep-proposal.v1\0");
  digest_input.extend_from_slice(&records);
  let digest = profile.digest(&digest_input);
  let mut body = vec![0u8; 32 + 2 * h + records.len()];
  put_u16(&mut body, 4, 1);
  put_i64(&mut body, 8, 1_700_000_200_000);
  fill_sequence(&mut body[16..16 + h], 0x31);
  put_u64(&mut body, 16 + h, 501);
  put_u32(&mut body, 24 + h, candidates.len() as u32);
  put_u32(&mut body, 28 + h, records.len() as u32);
  body[32 + h..32 + 2 * h].copy_from_slice(&digest);
  body[32 + 2 * h..].copy_from_slice(&records);
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&batch_id());
  build_gc_value(GcKind::SweepProposal, 501, &identity, &body)
}

fn decode_sweep_proposal(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_SWEEP_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::SweepProposal
    || artifact.identity.len() != 32
    || artifact.identity[..16] != database_id()
    || all_zero(&artifact.identity[16..])
    || artifact.body.len() < 32 + 2 * h
  {
    return Err("sweep_proposal_shape");
  }
  let body = artifact.body;
  let count = read_u32(body, 24 + h)? as usize;
  let records_length = read_u32(body, 28 + h)? as usize;
  let record_length = 24 + 2 * h;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 0
    || read_i64(body, 8)? <= 0
    || all_zero(&body[16..16 + h])
    || read_u64(body, 16 + h)? != artifact.generation
    || count == 0
    || count > MAX_CANDIDATES
    || records_length != count.checked_mul(record_length).ok_or("sweep_proposal_length")?
    || 32usize.checked_add(2 * h).and_then(|n| n.checked_add(records_length)) != Some(body.len())
  {
    return Err("sweep_proposal_fields");
  }
  let records = &body[32 + 2 * h..];
  let mut digest_input = Vec::with_capacity(32 + records.len());
  digest_input.extend_from_slice(b"aeordb.sweep-proposal.v1\0");
  digest_input.extend_from_slice(records);
  if body[32 + h..32 + 2 * h] != profile.digest(&digest_input) {
    return Err("sweep_proposal_digest");
  }
  let mut previous: Option<PhysicalIncarnationId> = None;
  for record in records.chunks_exact(record_length) {
    let candidate = decode_physical_incarnation(profile, record)?;
    if previous.as_ref().is_some_and(|prior| physical_compare(prior, &candidate) != Ordering::Less) {
      return Err("sweep_proposal_order");
    }
    previous = Some(candidate);
  }
  Ok(format!("gc:proposal:sweep:candidates={count}:mark={}", artifact.generation))
}

fn build_sweep_receipt(profile: HashProfile, recovered: bool, proposal_hash: &[u8], void_manifest_hash: &[u8]) -> Vec<u8> {
  let h = profile.width();
  let candidates = [sample_incarnation(profile, 1), sample_incarnation(profile, 2)];
  let mut records = Vec::new();
  for (index, candidate) in candidates.iter().enumerate() {
    let mut record = vec![0u8; 48 + 2 * h];
    let physical = encode_physical_incarnation(profile, candidate);
    record[..physical.len()].copy_from_slice(&physical);
    let cursor = physical.len();
    put_u16(&mut record, cursor, if index == 0 { 1 } else { 3 });
    put_u16(&mut record, cursor + 2, if index == 0 { 0 } else { 2 });
    if index == 0 {
      put_u64(&mut record, cursor + 8, candidate.wal_offset);
      put_u32(&mut record, cursor + 16, candidate.entity_length);
    }
    records.extend_from_slice(&record);
  }
  let mut body = vec![0u8; 64 + 2 * h + records.len()];
  put_u32(&mut body, 0, u32::from(recovered));
  put_u16(&mut body, 4, 1);
  put_i64(&mut body, 8, 1_700_000_200_500);
  body[16..16 + h].copy_from_slice(proposal_hash);
  body[16 + h..16 + 2 * h].copy_from_slice(void_manifest_hash);
  put_u64(&mut body, 16 + 2 * h, 501);
  put_u32(&mut body, 24 + 2 * h, 2);
  put_u32(&mut body, 28 + 2 * h, records.len() as u32);
  put_u64(&mut body, 32 + 2 * h, 1);
  put_u64(&mut body, 40 + 2 * h, u64::from(candidates[0].entity_length));
  put_u64(&mut body, 48 + 2 * h, 1);
  body[64 + 2 * h..].copy_from_slice(&records);
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&batch_id());
  build_gc_value(if recovered { GcKind::RecoveredSweepReceipt } else { GcKind::SweepCommitReceipt }, 501, &identity, &body)
}

fn decode_sweep_receipt(profile: HashProfile, bytes: &[u8], recovered: bool) -> Result<String, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_SWEEP_LENGTH)?;
  let expected_kind = if recovered { GcKind::RecoveredSweepReceipt } else { GcKind::SweepCommitReceipt };
  let h = profile.width();
  if artifact.kind != expected_kind
    || artifact.identity.len() != 32
    || artifact.identity[..16] != database_id()
    || artifact.body.len() < 64 + 2 * h
  {
    return Err("sweep_receipt_shape");
  }
  let body = artifact.body;
  let count = read_u32(body, 24 + 2 * h)? as usize;
  let records_length = read_u32(body, 28 + 2 * h)? as usize;
  let record_length = 48 + 2 * h;
  if read_u32(body, 0)? != u32::from(recovered)
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 0
    || read_i64(body, 8)? <= 0
    || body[16..16 + 2 * h].chunks_exact(h).any(all_zero)
    || read_u64(body, 16 + 2 * h)? != artifact.generation
    || count == 0
    || count > MAX_CANDIDATES
    || records_length != count.checked_mul(record_length).ok_or("sweep_receipt_length")?
    || 64usize.checked_add(2 * h).and_then(|n| n.checked_add(records_length)) != Some(body.len())
  {
    return Err("sweep_receipt_fields");
  }
  let mut reclaimed = 0u64;
  let mut reclaimed_bytes = 0u64;
  let mut skipped = 0u64;
  let mut failed = 0u64;
  let mut previous: Option<PhysicalIncarnationId> = None;
  for record in body[64 + 2 * h..].chunks_exact(record_length) {
    let physical_length = 24 + 2 * h;
    let physical = decode_physical_incarnation(profile, &record[..physical_length])?;
    let outcome = read_u16(record, physical_length)?;
    let reason = read_u16(record, physical_length + 2)?;
    let offset = read_u64(record, physical_length + 8)?;
    let length = read_u32(record, physical_length + 16)?;
    if !(1..=7).contains(&outcome)
      || read_u32(record, physical_length + 4)? != 0
      || read_u32(record, physical_length + 20)? != 0
      || (outcome == 1) != (offset != 0 && length != 0)
      || (outcome == 1 && (offset != physical.wal_offset || length != physical.entity_length || reason != 0))
      || (outcome != 1 && (offset != 0 || length != 0 || reason == 0))
      || previous.as_ref().is_some_and(|prior| physical_compare(prior, &physical) != Ordering::Less)
    {
      return Err("sweep_outcome_fields");
    }
    match outcome {
      1 => {
        reclaimed += 1;
        reclaimed_bytes += u64::from(length);
      }
      2..=5 => skipped += 1,
      6..=7 => failed += 1,
      _ => unreachable!(),
    }
    previous = Some(physical);
  }
  if read_u64(body, 32 + 2 * h)? != reclaimed
    || read_u64(body, 40 + 2 * h)? != reclaimed_bytes
    || read_u64(body, 48 + 2 * h)? != skipped
    || read_u64(body, 56 + 2 * h)? != failed
  {
    return Err("sweep_receipt_totals");
  }
  Ok(format!(
    "gc:receipt:sweep-{}:outcomes={count}:reclaimed={reclaimed}:skipped={skipped}:failed={failed}",
    if recovered { "recovered" } else { "commit" }
  ))
}

fn sample_extents(profile: HashProfile, proposal_hash: &[u8], quarantine_hash: &[u8]) -> Vec<VoidExtent> {
  [sample_incarnation(profile, 1), sample_incarnation(profile, 2)]
    .into_iter()
    .enumerate()
    .map(|(index, incarnation)| VoidExtent {
      offset: incarnation.wal_offset,
      length: incarnation.entity_length,
      proposal_hash: proposal_hash.to_vec(),
      quarantine_hash: quarantine_hash.to_vec(),
      incarnation_digest: profile.digest(&encode_physical_incarnation(profile, &incarnation)),
      reclaim_commit_sequence: 900 + index as u64,
      void_generation: 1,
    })
    .collect()
}

fn encode_void_extent(profile: HashProfile, extent: &VoidExtent) -> Vec<u8> {
  let h = profile.width();
  let mut row = vec![0u8; 32 + 3 * h];
  put_u64(&mut row, 0, extent.offset);
  put_u32(&mut row, 8, extent.length);
  row[16..16 + h].copy_from_slice(&extent.proposal_hash);
  row[16 + h..16 + 2 * h].copy_from_slice(&extent.quarantine_hash);
  row[16 + 2 * h..16 + 3 * h].copy_from_slice(&extent.incarnation_digest);
  put_u64(&mut row, 16 + 3 * h, extent.reclaim_commit_sequence);
  put_u64(&mut row, 24 + 3 * h, extent.void_generation);
  row
}

fn decode_void_extent(profile: HashProfile, row: &[u8]) -> Result<VoidExtent, &'static str> {
  let h = profile.width();
  if row.len() != 32 + 3 * h {
    return Err("void_extent_length");
  }
  let offset = read_u64(row, 0)?;
  let length = read_u32(row, 8)?;
  if offset < 2_048
    || length == 0
    || offset.checked_add(u64::from(length)).is_none()
    || read_u32(row, 12)? != 0
    || row[16..16 + 3 * h].chunks(h).any(all_zero)
    || read_u64(row, 16 + 3 * h)? == 0
    || read_u64(row, 24 + 3 * h)? == 0
  {
    return Err("void_extent_fields");
  }
  Ok(VoidExtent {
    offset,
    length,
    proposal_hash: row[16..16 + h].to_vec(),
    quarantine_hash: row[16 + h..16 + 2 * h].to_vec(),
    incarnation_digest: row[16 + 2 * h..16 + 3 * h].to_vec(),
    reclaim_commit_sequence: read_u64(row, 16 + 3 * h)?,
    void_generation: read_u64(row, 24 + 3 * h)?,
  })
}

fn build_void_extent_page(profile: HashProfile, generation: u64, extents: &[VoidExtent]) -> Vec<u8> {
  let records = extents.iter().map(|extent| encode_void_extent(profile, extent)).collect::<Vec<_>>();
  let records_length = records.iter().map(Vec::len).sum::<usize>();
  let lower = extents.first().expect("nonempty extent page").offset.to_le_bytes();
  let upper = extents.last().expect("nonempty extent page").offset.to_le_bytes();
  let mut body = vec![0u8; 64 + lower.len() + upper.len() + records_length];
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, VoidDirectoryRole::FreeExtents as u16);
  put_u32(&mut body, 8, lower.len() as u32);
  put_u32(&mut body, 12, upper.len() as u32);
  put_u32(&mut body, 16, extents.len() as u32);
  put_u32(&mut body, 20, extents.len() as u32);
  put_u64(&mut body, 24, records_length as u64);
  put_u64(&mut body, 32, extents.iter().map(|extent| u64::from(extent.length)).sum());
  let mut cursor = 64;
  body[cursor..cursor + 8].copy_from_slice(&lower);
  cursor += 8;
  body[cursor..cursor + 8].copy_from_slice(&upper);
  cursor += 8;
  for record in records {
    body[cursor..cursor + record.len()].copy_from_slice(&record);
    cursor += record.len();
  }
  let mut identity = Vec::with_capacity(42);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&catalog_id(VoidDirectoryRole::FreeExtents));
  identity.extend_from_slice(&(VoidDirectoryRole::FreeExtents as u16).to_le_bytes());
  identity.extend_from_slice(&(40 + generation).to_le_bytes());
  build_gc_value(GcKind::VoidExtentPage, generation, &identity, &body)
}

fn decode_void_extent_page(profile: HashProfile, bytes: &[u8]) -> Result<Vec<VoidExtent>, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_PAGE_LENGTH)?;
  let h = profile.width();
  let row_length = 32 + 3 * h;
  if artifact.kind != GcKind::VoidExtentPage
    || artifact.identity.len() != 42
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..32] != catalog_id(VoidDirectoryRole::FreeExtents)
    || read_u16(artifact.identity, 32)? != VoidDirectoryRole::FreeExtents as u16
    || read_u64(artifact.identity, 34)? == 0
    || artifact.body.len() < 80
  {
    return Err("void_page_shape");
  }
  let body = artifact.body;
  let count = read_u32(body, 16)? as usize;
  let records_length = usize::try_from(read_u64(body, 24)?).map_err(|_| "void_page_length")?;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != VoidDirectoryRole::FreeExtents as u16
    || read_u32(body, 8)? != 8
    || read_u32(body, 12)? != 8
    || count == 0
    || read_u32(body, 20)? as usize != count
    || records_length != count.checked_mul(row_length).ok_or("void_page_length")?
    || body[40..64].iter().any(|byte| *byte != 0)
    || 80usize.checked_add(records_length) != Some(body.len())
  {
    return Err("void_page_header");
  }
  let lower = read_u64(body, 64)?;
  let upper = read_u64(body, 72)?;
  let mut extents = Vec::with_capacity(count);
  let mut cursor = 80;
  for _ in 0..count {
    let extent = decode_void_extent(profile, &body[cursor..cursor + row_length])?;
    if extents
      .last()
      .is_some_and(|previous: &VoidExtent| previous.offset >= extent.offset || previous.offset + u64::from(previous.length) > extent.offset)
    {
      return Err("void_page_order_or_overlap");
    }
    extents.push(extent);
    cursor += row_length;
  }
  if extents.first().map(|extent| extent.offset) != Some(lower)
    || extents.last().map(|extent| extent.offset) != Some(upper)
    || extents.iter().map(|extent| u64::from(extent.length)).sum::<u64>() != read_u64(body, 32)?
  {
    return Err("void_page_fences_or_totals");
  }
  Ok(extents)
}

#[allow(clippy::too_many_arguments)]
fn build_directory(
  profile: HashProfile,
  role: VoidDirectoryRole,
  generation: u64,
  lower: &[u8],
  upper: &[u8],
  page_id: u64,
  child_hash: &[u8],
  live_count: u64,
  logical_bytes: u64,
) -> Vec<u8> {
  let h = profile.width();
  let fixed = 72 + h;
  let entries_length = fixed + lower.len() + upper.len();
  let mut body = vec![0u8; 80 + lower.len() + upper.len() + entries_length];
  put_u16(&mut body, 2, role as u16);
  put_u32(&mut body, 4, 1);
  put_u32(&mut body, 16, lower.len() as u32);
  put_u32(&mut body, 20, upper.len() as u32);
  put_u64(&mut body, 24, live_count);
  put_u64(&mut body, 40, u64::from(page_id != 0));
  put_u64(&mut body, 48, logical_bytes);
  put_u64(&mut body, 56, page_id);
  put_u64(&mut body, 64, page_id);
  put_u32(&mut body, 72, entries_length as u32);
  let mut cursor = 80;
  body[cursor..cursor + lower.len()].copy_from_slice(lower);
  cursor += lower.len();
  body[cursor..cursor + upper.len()].copy_from_slice(upper);
  cursor += upper.len();
  put_u32(&mut body, cursor, lower.len() as u32);
  put_u32(&mut body, cursor + 4, upper.len() as u32);
  put_u64(&mut body, cursor + 8, page_id);
  body[cursor + 16..cursor + 16 + h].copy_from_slice(child_hash);
  let fields = cursor + 16 + h;
  put_u64(&mut body, fields, generation);
  put_u64(&mut body, fields + 8, live_count);
  put_u64(&mut body, fields + 24, logical_bytes);
  cursor += fixed;
  body[cursor..cursor + lower.len()].copy_from_slice(lower);
  cursor += lower.len();
  body[cursor..cursor + upper.len()].copy_from_slice(upper);
  let mut identity = Vec::with_capacity(34);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&catalog_id(role));
  identity.extend_from_slice(&(role as u16).to_le_bytes());
  build_gc_value(GcKind::GcArtifactDirectoryNode, generation, &identity, &body)
}

fn decode_directory(profile: HashProfile, bytes: &[u8]) -> Result<DecodedDirectory, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_DIRECTORY_LENGTH)?;
  if artifact.kind != GcKind::GcArtifactDirectoryNode || artifact.identity.len() != 34 || artifact.identity[..16] != database_id() {
    return Err("void_directory_shape");
  }
  let role = VoidDirectoryRole::from_id(read_u16(artifact.identity, 32)?).ok_or("void_directory_role")?;
  if artifact.identity[16..32] != catalog_id(role) || artifact.body.len() < 80 {
    return Err("void_directory_identity");
  }
  let body = artifact.body;
  let lower_length = read_u32(body, 16)? as usize;
  let upper_length = read_u32(body, 20)? as usize;
  let entries_length = read_u32(body, 72)? as usize;
  let h = profile.width();
  let fixed = 72 + h;
  if read_u16(body, 0)? != 0
    || read_u16(body, 2)? != role as u16
    || read_u32(body, 4)? != 1
    || read_u32(body, 8)? != 0
    || read_u32(body, 12)? != 0
    || lower_length == 0
    || upper_length == 0
    || read_u32(body, 76)? != 0
    || entries_length != fixed + lower_length + upper_length
    || 80usize.checked_add(lower_length).and_then(|n| n.checked_add(upper_length)).and_then(|n| n.checked_add(entries_length))
      != Some(body.len())
  {
    return Err("void_directory_header");
  }
  let lower = &body[80..80 + lower_length];
  let upper = &body[80 + lower_length..80 + lower_length + upper_length];
  let cursor = 80 + lower_length + upper_length;
  let page_id = read_u64(body, cursor + 8)?;
  let child_hash = body[cursor + 16..cursor + 16 + h].to_vec();
  let fields = cursor + 16 + h;
  let generation = read_u64(body, fields)?;
  let live_count = read_u64(body, fields + 8)?;
  let logical_bytes = read_u64(body, fields + 24)?;
  let key_start = cursor + fixed;
  let ordered = match role {
    VoidDirectoryRole::FreeExtents if lower.len() == 8 && upper.len() == 8 => read_u64(lower, 0)? <= read_u64(upper, 0)?,
    VoidDirectoryRole::Claims if lower.len() == 16 && upper.len() == 16 => lower <= upper,
    _ => false,
  };
  let expected_page_count = u64::from(role == VoidDirectoryRole::FreeExtents);
  if all_zero(&child_hash)
    || generation == 0
    || generation > artifact.generation
    || live_count == 0
    || logical_bytes == 0
    || !ordered
    || (role == VoidDirectoryRole::FreeExtents) != (page_id != 0)
    || body[fields + 32..fields + 56].iter().any(|byte| *byte != 0)
    || body[key_start..key_start + lower_length] != *lower
    || body[key_start + lower_length..] != *upper
    || read_u64(body, 24)? != live_count
    || read_u64(body, 32)? != 0
    || read_u64(body, 40)? != expected_page_count
    || read_u64(body, 48)? != logical_bytes
    || read_u64(body, 56)? != page_id
    || read_u64(body, 64)? != page_id
  {
    return Err("void_directory_descriptor");
  }
  Ok(DecodedDirectory {
    role,
    live_count,
    #[cfg(test)]
    child_hash,
  })
}

#[allow(clippy::too_many_arguments)]
fn build_void_manifest(
  profile: HashProfile,
  generation: u64,
  free_root: &[u8],
  claim_root: &[u8],
  free_count: u64,
  free_bytes: u64,
  claim_count: u64,
  claimed_bytes: u64,
  previous_control_sequence: u64,
) -> Vec<u8> {
  let h = profile.width();
  assert!(free_root.is_empty() || free_root.len() == h);
  assert!(claim_root.is_empty() || claim_root.len() == h);
  let mut body = vec![0u8; 92 + 2 * h];
  write_capabilities(&mut body[4..36], VOID_CAPABILITY_BITS);
  put_i64(&mut body, 36, 1_700_000_201_000 + generation as i64);
  if !free_root.is_empty() {
    body[44..44 + h].copy_from_slice(free_root);
  }
  if !claim_root.is_empty() {
    body[44 + h..44 + 2 * h].copy_from_slice(claim_root);
  }
  put_u64(&mut body, 44 + 2 * h, if free_count + claim_count == 0 { 1 } else { 100 + generation });
  put_u64(&mut body, 52 + 2 * h, free_count);
  put_u64(&mut body, 60 + 2 * h, free_bytes);
  put_u64(&mut body, 68 + 2 * h, claim_count);
  put_u64(&mut body, 76 + 2 * h, claimed_bytes);
  put_u64(&mut body, 84 + 2 * h, previous_control_sequence);
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&generation.to_le_bytes());
  build_gc_value(GcKind::VoidCatalogManifest, generation, &identity, &body)
}

fn decode_void_manifest(profile: HashProfile, bytes: &[u8]) -> Result<DecodedVoidManifest, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::VoidCatalogManifest
    || artifact.identity.len() != 24
    || artifact.identity[..16] != database_id()
    || read_u64(artifact.identity, 16)? != artifact.generation
    || artifact.body.len() != 92 + 2 * h
  {
    return Err("void_manifest_shape");
  }
  let body = artifact.body;
  let free_root = body[44..44 + h].to_vec();
  let claim_root = body[44 + h..44 + 2 * h].to_vec();
  let free_count = read_u64(body, 52 + 2 * h)?;
  let free_bytes = read_u64(body, 60 + 2 * h)?;
  let claim_count = read_u64(body, 68 + 2 * h)?;
  let claimed_bytes = read_u64(body, 76 + 2 * h)?;
  if read_u32(body, 0)? != 0
    || !valid_capabilities(&body[4..36], VOID_CAPABILITY_BITS)
    || read_i64(body, 36)? <= 0
    || (free_count == 0) != (free_bytes == 0 && all_zero(&free_root))
    || (claim_count == 0) != (claimed_bytes == 0 && all_zero(&claim_root))
    || (free_count + claim_count > 0 && read_u64(body, 44 + 2 * h)? == 0)
    || (artifact.generation == 1 && read_u64(body, 84 + 2 * h)? != 0)
    || (artifact.generation > 1 && read_u64(body, 84 + 2 * h)? == 0)
  {
    return Err("void_manifest_fields");
  }
  Ok(DecodedVoidManifest {
    generation: artifact.generation,
    #[cfg(test)]
    published_at_ms: read_i64(body, 36)?,
    free_count,
    #[cfg(test)]
    free_bytes,
    claim_count,
    #[cfg(test)]
    claimed_bytes,
    #[cfg(test)]
    free_root,
    #[cfg(test)]
    claim_root,
  })
}

fn build_void_claim(profile: HashProfile, source_manifest: &[u8], extent: &VoidExtent) -> Vec<u8> {
  let h = profile.width();
  let record_length = 16 + h;
  let mut body = vec![0u8; 56 + h + record_length];
  put_u16(&mut body, 4, 1);
  // The immutable claim cannot postdate the replacement catalog that roots it.
  put_i64(&mut body, 8, 1_700_000_201_001);
  body[16..32].copy_from_slice(&boot_id());
  body[32..48].copy_from_slice(&batch_id());
  body[48..48 + h].copy_from_slice(source_manifest);
  put_u32(&mut body, 48 + h, 1);
  put_u32(&mut body, 52 + h, record_length as u32);
  put_u64(&mut body, 56 + h, extent.offset);
  put_u32(&mut body, 64 + h, extent.length);
  body[72 + h..72 + 2 * h].copy_from_slice(&extent.proposal_hash);
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&claim_id());
  build_gc_value(GcKind::VoidClaim, 2, &identity, &body)
}

fn decode_void_claim(profile: HashProfile, bytes: &[u8]) -> Result<(usize, u64, Vec<u8>, i64), &'static str> {
  let artifact = decode_gc_value(bytes, MAX_SWEEP_LENGTH)?;
  let h = profile.width();
  let record_length = 16 + h;
  if artifact.kind != GcKind::VoidClaim
    || artifact.identity.len() != 32
    || artifact.identity[..16] != database_id()
    || all_zero(&artifact.identity[16..])
    || artifact.body.len() < 56 + h
  {
    return Err("void_claim_shape");
  }
  let body = artifact.body;
  let count = read_u32(body, 48 + h)? as usize;
  let records_length = read_u32(body, 52 + h)? as usize;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 0
    || read_i64(body, 8)? <= 0
    || all_zero(&body[16..48])
    || all_zero(&body[48..48 + h])
    || count == 0
    || count > MAX_CANDIDATES
    || records_length != count.checked_mul(record_length).ok_or("void_claim_length")?
    || 56usize.checked_add(h).and_then(|n| n.checked_add(records_length)) != Some(body.len())
  {
    return Err("void_claim_fields");
  }
  let mut total = 0u64;
  let mut previous_end = None;
  for record in body[56 + h..].chunks_exact(record_length) {
    let offset = read_u64(record, 0)?;
    let length = read_u32(record, 8)?;
    if offset < 2_048
      || length == 0
      || read_u32(record, 12)? != 0
      || all_zero(&record[16..])
      || offset.checked_add(u64::from(length)).is_none()
      || previous_end.is_some_and(|end| end > offset)
    {
      return Err("void_claim_extent");
    }
    previous_end = Some(offset + u64::from(length));
    total = total.checked_add(u64::from(length)).ok_or("void_claim_total")?;
  }
  Ok((count, total, body[48..48 + h].to_vec(), read_i64(body, 8)?))
}

fn build_settlement_receipt(
  profile: HashProfile,
  recovered: bool,
  outcome: u16,
  source_manifest: &[u8],
  result_manifest: &[u8],
) -> Vec<u8> {
  let h = profile.width();
  let mut body = vec![0u8; 40 + 3 * h];
  put_u32(&mut body, 0, u32::from(recovered));
  put_u16(&mut body, 4, outcome);
  put_i64(&mut body, 8, 1_700_000_202_000);
  body[16..16 + h].copy_from_slice(source_manifest);
  body[16 + h..16 + 2 * h].copy_from_slice(result_manifest);
  put_u32(&mut body, 16 + 2 * h, 1);
  put_u64(&mut body, 24 + 2 * h, 4_097);
  fill_sequence(&mut body[40 + 2 * h..], 0xe1);
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&claim_id());
  build_gc_value(GcKind::VoidClaimSettlementReceipt, 3, &identity, &body)
}

fn decode_settlement_receipt(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::VoidClaimSettlementReceipt
    || artifact.identity.len() != 32
    || artifact.identity[..16] != database_id()
    || all_zero(&artifact.identity[16..])
    || artifact.body.len() != 40 + 3 * h
  {
    return Err("void_settlement_shape");
  }
  let body = artifact.body;
  let flags = read_u32(body, 0)?;
  let outcome = read_u16(body, 4)?;
  let used = read_u32(body, 16 + 2 * h)?;
  let unused = read_u32(body, 20 + 2 * h)?;
  let used_bytes = read_u64(body, 24 + 2 * h)?;
  let returned_bytes = read_u64(body, 32 + 2 * h)?;
  if flags & !1 != 0
    || !(1..=3).contains(&outcome)
    || (flags & 1 != 0) != (outcome == 2)
    || read_u16(body, 6)? != 0
    || read_i64(body, 8)? <= 0
    || body[16..16 + 2 * h].chunks_exact(h).any(all_zero)
    || body[16..16 + h] == body[16 + h..16 + 2 * h]
    || all_zero(&body[40 + 2 * h..])
    || (outcome == 1 && (used == 0 || used_bytes == 0))
    || (outcome == 3 && (used != 0 || unused != 0 || used_bytes != 0 || returned_bytes != 0))
    || (unused == 0) != (returned_bytes == 0)
  {
    return Err("void_settlement_fields");
  }
  let label = match outcome {
    1 => "settled",
    2 => "recovered",
    3 => "abandoned",
    _ => unreachable!(),
  };
  Ok(format!("gc:receipt:void-claim-settlement:{label}:used={used}:unused={unused}"))
}

#[cfg(test)]
fn validate_fixture_closure(profile: HashProfile, cases: &[GcFixtureCase]) -> Result<(), &'static str> {
  let by_suffix =
    |suffix: &str| cases.iter().find(|case| case.id.ends_with(suffix)).map(|case| case.bytes.as_slice()).ok_or("fixture_missing");
  let source_page = by_suffix("void-extent-page-source")?;
  let source_directory = by_suffix("void-free-directory-source")?;
  let source_manifest = by_suffix("void-catalog-source")?;
  let claim = by_suffix("void-claim")?;
  let claim_directory = by_suffix("void-claims-directory")?;
  let outstanding_manifest = by_suffix("void-catalog-outstanding")?;
  let remaining_page = by_suffix("void-extent-page-remaining")?;
  let remaining_directory = by_suffix("void-free-directory-remaining")?;
  let settled_manifest = by_suffix("void-catalog-settled")?;
  let proposal = by_suffix("sweep-proposal")?;
  let commit_receipt = by_suffix("sweep-commit-receipt")?;
  let recovered_receipt = by_suffix("sweep-recovered-receipt")?;
  let settlement = by_suffix("void-claim-settlement")?;

  let source_extents = decode_void_extent_page(profile, source_page)?;
  let source_bytes = source_extents.iter().map(|extent| u64::from(extent.length)).sum::<u64>();
  let source_dir = decode_directory(profile, source_directory)?;
  if source_dir.child_hash != immutable_key(profile, GcKind::VoidExtentPage, source_page)
    || source_dir.live_count != source_extents.len() as u64
  {
    return Err("source_directory_closure");
  }
  let source = decode_void_manifest(profile, source_manifest)?;
  if source.free_root != immutable_key(profile, GcKind::GcArtifactDirectoryNode, source_directory)
    || source.free_count != source_extents.len() as u64
    || source.free_bytes != source_bytes
    || source.claim_count != 0
    || source.claimed_bytes != 0
  {
    return Err("source_manifest_closure");
  }
  let (claim_count, claim_bytes, claim_source, claim_created_at_ms) = decode_void_claim(profile, claim)?;
  if claim_count != 1
    || claim_bytes != u64::from(source_extents[0].length)
    || claim_source != immutable_key(profile, GcKind::VoidCatalogManifest, source_manifest)
    || claim_created_at_ms < source.published_at_ms
  {
    return Err("claim_source_closure");
  }
  let claims_dir = decode_directory(profile, claim_directory)?;
  if claims_dir.child_hash != immutable_key(profile, GcKind::VoidClaim, claim) || claims_dir.live_count != claim_count as u64 {
    return Err("claim_directory_closure");
  }
  let remaining_extents = decode_void_extent_page(profile, remaining_page)?;
  let remaining_bytes = remaining_extents.iter().map(|extent| u64::from(extent.length)).sum::<u64>();
  let outstanding = decode_void_manifest(profile, outstanding_manifest)?;
  if outstanding.claim_root != immutable_key(profile, GcKind::GcArtifactDirectoryNode, claim_directory)
    || outstanding.free_root != immutable_key(profile, GcKind::GcArtifactDirectoryNode, remaining_directory)
    || decode_directory(profile, remaining_directory)?.child_hash != immutable_key(profile, GcKind::VoidExtentPage, remaining_page)
    || outstanding.free_count != remaining_extents.len() as u64
    || outstanding.free_bytes != remaining_bytes
    || outstanding.claim_count != claim_count as u64
    || outstanding.claimed_bytes != claim_bytes
    || outstanding.published_at_ms < claim_created_at_ms
  {
    return Err("outstanding_manifest_closure");
  }
  let settled = decode_void_manifest(profile, settled_manifest)?;
  if settled.free_root != outstanding.free_root
    || settled.free_count != outstanding.free_count
    || settled.free_bytes != outstanding.free_bytes
    || settled.claim_count != 0
    || settled.claimed_bytes != 0
  {
    return Err("settled_manifest_closure");
  }

  let proposal_artifact = decode_gc_value(proposal, MAX_SWEEP_LENGTH)?;
  let proposal_records = &proposal_artifact.body[32 + 2 * profile.width()..];
  for (receipt, recovered) in [(commit_receipt, false), (recovered_receipt, true)] {
    decode_sweep_receipt(profile, receipt, recovered)?;
    let receipt_artifact = decode_gc_value(receipt, MAX_SWEEP_LENGTH)?;
    if receipt_artifact.body[16..16 + profile.width()] != immutable_key(profile, GcKind::SweepProposal, proposal)
      || receipt_artifact.body[16 + profile.width()..16 + 2 * profile.width()]
        != immutable_key(profile, GcKind::VoidCatalogManifest, outstanding_manifest)
    {
      return Err("sweep_receipt_root_closure");
    }
    let outcome_records = &receipt_artifact.body[64 + 2 * profile.width()..];
    let proposal_record_length = 24 + 2 * profile.width();
    let outcome_record_length = 48 + 2 * profile.width();
    if proposal_records.len() / proposal_record_length != outcome_records.len() / outcome_record_length
      || !proposal_records
        .chunks_exact(proposal_record_length)
        .zip(outcome_records.chunks_exact(outcome_record_length))
        .all(|(candidate, outcome)| candidate == &outcome[..proposal_record_length])
    {
      return Err("sweep_receipt_candidate_closure");
    }
  }

  decode_settlement_receipt(profile, settlement)?;
  let settlement_artifact = decode_gc_value(settlement, MAX_MANIFEST_LENGTH)?;
  if settlement_artifact.body[16..16 + profile.width()] != immutable_key(profile, GcKind::VoidCatalogManifest, outstanding_manifest)
    || settlement_artifact.body[16 + profile.width()..16 + 2 * profile.width()]
      != immutable_key(profile, GcKind::VoidCatalogManifest, settled_manifest)
  {
    return Err("settlement_manifest_closure");
  }
  Ok(())
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

fn database_id() -> [u8; 16] {
  sequence_array(0x31)
}

fn batch_id() -> [u8; 16] {
  sequence_array(0x51)
}

fn claim_id() -> [u8; 16] {
  sequence_array(0x71)
}

fn boot_id() -> [u8; 16] {
  sequence_array(0x91)
}

fn catalog_id(role: VoidDirectoryRole) -> [u8; 16] {
  sequence_array(0x50 + role as u8 * 0x10)
}

fn sequence_array(start: u8) -> [u8; 16] {
  let mut bytes = [0u8; 16];
  fill_sequence(&mut bytes, start);
  bytes
}

fn patterned_hash(profile: HashProfile, start: u8) -> Vec<u8> {
  let mut bytes = vec![0u8; profile.width()];
  fill_sequence(&mut bytes, start);
  bytes
}

fn fill_sequence(bytes: &mut [u8], start: u8) {
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(index as u8);
  }
}

fn write_capabilities(bytes: &mut [u8], bits: &[usize]) {
  assert_eq!(bytes.len(), CAPABILITIES_LENGTH);
  for bit in bits {
    bytes[bit / 8] |= 1 << (bit % 8);
  }
}

fn valid_capabilities(bytes: &[u8], bits: &[usize]) -> bool {
  let mut expected = [0u8; CAPABILITIES_LENGTH];
  write_capabilities(&mut expected, bits);
  bytes == expected
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, &'static str> {
  Ok(i64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("sweep_void_truncated")?.try_into().map_err(|_| "sweep_void_truncated")?))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
fn repair_crc(bytes: &mut [u8]) {
  let offset = bytes.len() - 4;
  put_u32(bytes, offset, crc32fast::hash(&bytes[..offset]));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sweep_void_fixtures_round_trip_and_close() {
    let cases = fixture_cases();
    assert_eq!(cases.len(), 28);
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let profile_cases = cases.iter().filter(|case| case.profile == profile).cloned().collect::<Vec<_>>();
      assert_eq!(validate_fixture_closure(profile, &profile_cases), Ok(()));
      for case in profile_cases {
        let (observed, key) = observe(profile, &case.bytes);
        assert_eq!(observed, case.expected, "fixture {}", case.id);
        assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
      }
    }
  }

  #[test]
  fn every_fixture_byte_is_crc_or_structure_protected() {
    for case in fixture_cases() {
      for index in 0..case.bytes.len() {
        let mut changed = case.bytes.clone();
        changed[index] ^= 1;
        assert!(observe(case.profile, &changed).0.starts_with("error:"), "fixture {} byte {index}", case.id);
      }
    }
  }

  #[test]
  fn corrected_formulas_hold_for_both_hash_widths() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let proposal = build_sweep_proposal(profile);
      let proposal = decode_gc_value(&proposal, MAX_SWEEP_LENGTH).unwrap();
      assert_eq!(proposal.body.len(), 32 + 2 * h + 2 * (24 + 2 * h));
      let extent = encode_void_extent(profile, &sample_extents(profile, &patterned_hash(profile, 1), &patterned_hash(profile, 2))[0]);
      assert_eq!(extent.len(), 32 + 3 * h);
      let source = patterned_hash(profile, 3);
      let result = patterned_hash(profile, 4);
      let receipt = build_settlement_receipt(profile, false, 1, &source, &result);
      assert_eq!(decode_gc_value(&receipt, MAX_MANIFEST_LENGTH).unwrap().body.len(), 40 + 3 * h);
    }
  }

  #[test]
  fn repaired_crc_semantic_corruption_fails_closed() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let mut proposal = build_sweep_proposal(profile);
      let proposal_body = 32 + 32;
      put_u32(&mut proposal, proposal_body + 24 + h, 0);
      repair_crc(&mut proposal);
      assert!(decode_sweep_proposal(profile, &proposal).is_err());

      let hash_a = patterned_hash(profile, 1);
      let hash_b = patterned_hash(profile, 2);
      let mut settlement = build_settlement_receipt(profile, false, 1, &hash_a, &hash_b);
      let settlement_body = 32 + 32;
      put_u32(&mut settlement, settlement_body, 1);
      repair_crc(&mut settlement);
      assert_eq!(decode_settlement_receipt(profile, &settlement).err(), Some("void_settlement_fields"));

      let mut claim =
        build_void_claim(profile, &hash_a, &sample_extents(profile, &patterned_hash(profile, 3), &patterned_hash(profile, 4))[0]);
      let claim_body = 32 + 32;
      put_u16(&mut claim, claim_body + 6, 2);
      repair_crc(&mut claim);
      assert_eq!(decode_void_claim(profile, &claim).err(), Some("void_claim_fields"));
    }
  }

  #[test]
  fn overlap_claim_presence_and_settlement_outcomes_are_strict() {
    let profile = HashProfile::Blake3_256;
    let proposal_hash = patterned_hash(profile, 1);
    let quarantine_hash = patterned_hash(profile, 2);
    let mut extents = sample_extents(profile, &proposal_hash, &quarantine_hash);
    extents[1].offset = extents[0].offset + 1;
    let page = build_void_extent_page(profile, 1, &extents);
    assert_eq!(decode_void_extent_page(profile, &page).err(), Some("void_page_order_or_overlap"));

    let source = patterned_hash(profile, 3);
    let result = patterned_hash(profile, 4);
    let recovered = build_settlement_receipt(profile, true, 2, &source, &result);
    assert_eq!(decode_settlement_receipt(profile, &recovered).unwrap(), "gc:receipt:void-claim-settlement:recovered:used=1:unused=0");
    let abandoned = build_settlement_receipt(profile, false, 3, &source, &result);
    assert_eq!(decode_settlement_receipt(profile, &abandoned).err(), Some("void_settlement_fields"));
  }
}
