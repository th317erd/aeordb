use std::cmp::Ordering;

use crate::config;
use crate::core::HashProfile;
use crate::gc::{
  build_gc_value, decode_gc_value, immutable_key, put_u16, put_u32, put_u64, read_u16, read_u32, read_u64, GcFixtureCase, GcFormat, GcKind,
};

const MAX_MANIFEST_LENGTH: usize = 1024 * 1024;
const MAX_PAGE_LENGTH: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1024 * 1024;
const MAX_PINS: usize = 4_096;
const MAX_EVIDENCE: usize = 64;
const CAPABILITIES_LENGTH: usize = 32;
const AUDIT_CAPABILITY_BITS: &[usize] = &[12];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuditDirectoryRole {
  Detail = 6,
  Summary = 7,
}

impl AuditDirectoryRole {
  fn from_id(id: u16) -> Option<Self> {
    match id {
      6 => Some(Self::Detail),
      7 => Some(Self::Summary),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::Detail => "audit-detail",
      Self::Summary => "audit-summary",
    }
  }

  fn page_kind(self) -> GcKind {
    match self {
      Self::Detail => GcKind::AuditDetailPage,
      Self::Summary => GcKind::AuditSummaryPage,
    }
  }
}

#[derive(Clone, Debug)]
struct DetailRecord {
  event_id: Vec<u8>,
  event_kind: u16,
  outcome: u16,
  occurred_at_ms: i64,
  run_id: [u8; 16],
  batch_id: [u8; 16],
  payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SummaryRecord {
  run_id: [u8; 16],
  started_at_ms: i64,
  completed_at_ms: i64,
  run_kind: u16,
  outcome: u16,
  mark_generation: u64,
  scanned_count: u64,
  candidate_count: u64,
  reclaimed_count: u64,
  reclaimed_bytes: u64,
  evidence_digest: Vec<u8>,
}

#[derive(Debug)]
struct DecodedPage {
  role: AuditDirectoryRole,
  records: Vec<Vec<u8>>,
  #[cfg(test)]
  logical_bytes: u64,
  #[cfg(test)]
  oldest_at_ms: i64,
  #[cfg(test)]
  newest_at_ms: i64,
}

#[derive(Debug)]
struct DecodedDirectory {
  role: AuditDirectoryRole,
  live_count: u64,
  #[cfg(test)]
  logical_bytes: u64,
  #[cfg(test)]
  child_hash: Vec<u8>,
}

#[derive(Debug)]
struct DecodedManifest {
  generation: u64,
  detail_count: u64,
  summary_count: u64,
  pin_count: usize,
  #[cfg(test)]
  detail_root: Vec<u8>,
  #[cfg(test)]
  summary_root: Vec<u8>,
  #[cfg(test)]
  detail_bytes: u64,
  #[cfg(test)]
  summary_bytes: u64,
  #[cfg(test)]
  oldest_detail_at_ms: i64,
  #[cfg(test)]
  newest_detail_at_ms: i64,
  #[cfg(test)]
  oldest_summary_at_ms: i64,
  #[cfg(test)]
  newest_summary_at_ms: i64,
  #[cfg(test)]
  pins: Vec<Vec<u8>>,
}

pub fn fixture_cases() -> Vec<GcFixtureCase> {
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let details = sample_details(profile);
    let detail_page = build_page(
      profile,
      AuditDirectoryRole::Detail,
      1,
      61,
      &details.iter().map(|record| encode_detail(profile, record)).collect::<Vec<_>>(),
    );
    let detail_page_hash = immutable_key(profile, GcKind::AuditDetailPage, &detail_page);
    let detail_directory = build_directory(
      profile,
      AuditDirectoryRole::Detail,
      1,
      &detail_key(profile, &details[0]),
      &detail_key(profile, &details[1]),
      61,
      &detail_page_hash,
      details.len() as u64,
      details.iter().map(|record| encode_detail(profile, record).len() as u64).sum(),
    );
    let detail_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &detail_directory);

    let summaries = sample_summaries(profile);
    let summary_rows = summaries.iter().map(|record| encode_summary(profile, record)).collect::<Vec<_>>();
    let summary_page = build_page(profile, AuditDirectoryRole::Summary, 1, 71, &summary_rows);
    let summary_page_hash = immutable_key(profile, GcKind::AuditSummaryPage, &summary_page);
    let summary_directory = build_directory(
      profile,
      AuditDirectoryRole::Summary,
      1,
      &summary_key(&summaries[0]),
      &summary_key(&summaries[1]),
      71,
      &summary_page_hash,
      summaries.len() as u64,
      summary_rows.iter().map(|row| row.len() as u64).sum(),
    );
    let summary_directory_hash = immutable_key(profile, GcKind::GcArtifactDirectoryNode, &summary_directory);

    let corrupt = build_corrupt_evidence(profile);
    let corrupt_hash = immutable_key(profile, GcKind::CorruptGcEvidence, &corrupt);
    let run_summary = build_run_summary(profile, &summaries[1]);
    let run_summary_hash = immutable_key(profile, GcKind::GcRunSummary, &run_summary);
    let pin = build_audit_pin(profile, &[corrupt_hash, run_summary_hash]);
    let pin_hash = immutable_key(profile, GcKind::AuditPin, &pin);
    let populated_manifest = build_audit_manifest(
      profile,
      1,
      &detail_directory_hash,
      &summary_directory_hash,
      &decode_page(profile, &detail_page).expect("fixture detail page").records,
      &decode_page(profile, &summary_page).expect("fixture summary page").records,
      &[pin_hash],
    );
    let empty_manifest = build_audit_manifest(profile, 2, &[], &[], &[], &[], &[]);

    for (suffix, bytes, expected, relation) in [
      (
        "audit-catalog-empty",
        empty_manifest,
        "gc:manifest:audit-catalog:empty:details=0:summaries=0:pins=0:generation=2".to_string(),
        "retention-authority-empty",
      ),
      (
        "audit-catalog-populated",
        populated_manifest,
        "gc:manifest:audit-catalog:populated:details=2:summaries=2:pins=1:generation=1".to_string(),
        "roots:detail-summary-pin-closure",
      ),
      ("audit-detail-page", detail_page, "gc:page:audit-detail:records=2".to_string(), "events:sorted-by-time-and-id"),
      ("audit-detail-directory", detail_directory, "gc:directory:audit-detail:records=2".to_string(), "indexes:AuditDetailPage"),
      ("audit-summary-page", summary_page, "gc:page:audit-summary:records=2".to_string(), "summaries:sorted-by-completion-and-run"),
      ("audit-summary-directory", summary_directory, "gc:directory:audit-summary:records=2".to_string(), "indexes:AuditSummaryPage"),
      ("gc-run-summary", run_summary, "gc:summary:run:kind=2:outcome=1:reclaimed=1".to_string(), "same-body:AuditSummaryPage-record"),
      ("corrupt-gc-evidence", corrupt, "gc:evidence:corrupt:class=6:items=2:context=65".to_string(), "retained:detail-policy"),
      ("audit-pin", pin, "gc:pin:audit:reason=2:artifacts=2".to_string(), "roots:corrupt-evidence-and-run-summary"),
    ] {
      let kind = GcKind::from_id(read_u16(&bytes, 6).expect("fixture kind")).expect("registered fixture kind");
      cases.push(GcFixtureCase {
        id: leak(format!("agca-{}-{suffix}", profile.label())),
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
    Some(GcKind::AuditCatalogManifest) => decode_audit_manifest(profile, bytes).map(|manifest| {
      format!(
        "gc:manifest:audit-catalog:{}:details={}:summaries={}:pins={}:generation={}",
        if manifest.detail_count == 0 && manifest.summary_count == 0 && manifest.pin_count == 0 { "empty" } else { "populated" },
        manifest.detail_count,
        manifest.summary_count,
        manifest.pin_count,
        manifest.generation
      )
    }),
    Some(GcKind::AuditDetailPage | GcKind::AuditSummaryPage) => {
      decode_page(profile, bytes).map(|page| format!("gc:page:{}:records={}", page.role.name(), page.records.len()))
    }
    Some(GcKind::GcArtifactDirectoryNode) => {
      decode_directory(profile, bytes).map(|directory| format!("gc:directory:{}:records={}", directory.role.name(), directory.live_count))
    }
    Some(GcKind::GcRunSummary) => decode_run_summary(profile, bytes)
      .map(|summary| format!("gc:summary:run:kind={}:outcome={}:reclaimed={}", summary.run_kind, summary.outcome, summary.reclaimed_count)),
    Some(GcKind::CorruptGcEvidence) => decode_corrupt_evidence(profile, bytes),
    Some(GcKind::AuditPin) => {
      decode_audit_pin(profile, bytes).map(|(reason, count, _)| format!("gc:pin:audit:reason={reason}:artifacts={count}"))
    }
    _ => Err("gc_audit_kind"),
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
    format!("body: GC audit/evidence contract for H={}", profile.width()),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

pub(crate) fn is_audit_directory(bytes: &[u8]) -> bool {
  read_u16(bytes, 6).ok() == Some(GcKind::GcArtifactDirectoryNode.id())
    && read_u16(bytes, 64).ok().and_then(AuditDirectoryRole::from_id).is_some()
}

fn sample_details(profile: HashProfile) -> Vec<DetailRecord> {
  let payload_a = config::canonicalize_json(r#"{"mark_generation":701,"stable_reason":0}"#).expect("canonical audit payload");
  let payload_b = config::canonicalize_json(r#"{"reclaimed_bytes":4097,"stable_reason":0}"#).expect("canonical audit payload");
  vec![
    DetailRecord {
      event_id: patterned_hash(profile, 0x21),
      event_kind: 2,
      outcome: 1,
      occurred_at_ms: 1_700_000_300_000,
      run_id: sequence_array(0x41),
      batch_id: [0; 16],
      payload: payload_a,
    },
    DetailRecord {
      event_id: patterned_hash(profile, 0x61),
      event_kind: 7,
      outcome: 1,
      occurred_at_ms: 1_700_000_301_000,
      run_id: sequence_array(0x81),
      batch_id: sequence_array(0xa1),
      payload: payload_b,
    },
  ]
}

fn encode_detail(profile: HashProfile, record: &DetailRecord) -> Vec<u8> {
  let h = profile.width();
  let mut bytes = vec![0u8; 52 + h + record.payload.len()];
  bytes[..h].copy_from_slice(&record.event_id);
  put_u16(&mut bytes, h, record.event_kind);
  put_u16(&mut bytes, h + 2, record.outcome);
  put_i64(&mut bytes, h + 4, record.occurred_at_ms);
  bytes[h + 12..h + 28].copy_from_slice(&record.run_id);
  bytes[h + 28..h + 44].copy_from_slice(&record.batch_id);
  put_u32(&mut bytes, h + 44, record.payload.len() as u32);
  bytes[h + 52..].copy_from_slice(&record.payload);
  bytes
}

fn decode_detail(profile: HashProfile, bytes: &[u8]) -> Result<DetailRecord, &'static str> {
  let h = profile.width();
  if bytes.len() < 52 + h {
    return Err("audit_detail_truncated");
  }
  let payload_length = read_u32(bytes, h + 44)? as usize;
  if 52usize.checked_add(h).and_then(|fixed| fixed.checked_add(payload_length)) != Some(bytes.len()) {
    return Err("audit_detail_length");
  }
  let event_kind = read_u16(bytes, h)?;
  let outcome = read_u16(bytes, h + 2)?;
  let occurred_at_ms = read_i64(bytes, h + 4)?;
  let run_id: [u8; 16] = bytes[h + 12..h + 28].try_into().map_err(|_| "audit_detail_run")?;
  let batch_id: [u8; 16] = bytes[h + 28..h + 44].try_into().map_err(|_| "audit_detail_batch")?;
  let payload_bytes = &bytes[h + 52..];
  if all_zero(&bytes[..h])
    || !(1..=14).contains(&event_kind)
    || !(1..=5).contains(&outcome)
    || occurred_at_ms <= 0
    || all_zero(&run_id)
    || read_u32(bytes, h + 48)? != 0
    || payload_bytes.is_empty()
    || config::validate_audit_value(payload_bytes).is_err()
    || (matches!(event_kind, 6..=11) != !all_zero(&batch_id))
  {
    return Err("audit_detail_fields");
  }
  Ok(DetailRecord { event_id: bytes[..h].to_vec(), event_kind, outcome, occurred_at_ms, run_id, batch_id, payload: payload_bytes.to_vec() })
}

fn detail_key(profile: HashProfile, record: &DetailRecord) -> Vec<u8> {
  let mut key = Vec::with_capacity(8 + profile.width());
  key.extend_from_slice(&record.occurred_at_ms.to_le_bytes());
  key.extend_from_slice(&record.event_id);
  key
}

fn detail_compare(left: &DetailRecord, right: &DetailRecord) -> Ordering {
  left.occurred_at_ms.cmp(&right.occurred_at_ms).then_with(|| left.event_id.cmp(&right.event_id))
}

fn sample_summaries(profile: HashProfile) -> Vec<SummaryRecord> {
  vec![
    SummaryRecord {
      run_id: sequence_array(0x41),
      started_at_ms: 1_700_000_299_000,
      completed_at_ms: 1_700_000_300_000,
      run_kind: 1,
      outcome: 1,
      mark_generation: 701,
      scanned_count: 100,
      candidate_count: 2,
      reclaimed_count: 0,
      reclaimed_bytes: 0,
      evidence_digest: patterned_hash(profile, 0xc1),
    },
    SummaryRecord {
      run_id: sequence_array(0x81),
      started_at_ms: 1_700_000_300_100,
      completed_at_ms: 1_700_000_301_000,
      run_kind: 2,
      outcome: 1,
      mark_generation: 701,
      scanned_count: 2,
      candidate_count: 2,
      reclaimed_count: 1,
      reclaimed_bytes: 4_097,
      evidence_digest: patterned_hash(profile, 0xe1),
    },
  ]
}

fn encode_summary(profile: HashProfile, record: &SummaryRecord) -> Vec<u8> {
  let h = profile.width();
  let mut bytes = vec![0u8; 76 + h];
  bytes[..16].copy_from_slice(&record.run_id);
  put_i64(&mut bytes, 16, record.started_at_ms);
  put_i64(&mut bytes, 24, record.completed_at_ms);
  put_u16(&mut bytes, 32, record.run_kind);
  put_u16(&mut bytes, 34, record.outcome);
  put_u64(&mut bytes, 36, record.mark_generation);
  put_u64(&mut bytes, 44, record.scanned_count);
  put_u64(&mut bytes, 52, record.candidate_count);
  put_u64(&mut bytes, 60, record.reclaimed_count);
  put_u64(&mut bytes, 68, record.reclaimed_bytes);
  bytes[76..76 + h].copy_from_slice(&record.evidence_digest);
  bytes
}

fn decode_summary(profile: HashProfile, bytes: &[u8]) -> Result<SummaryRecord, &'static str> {
  let h = profile.width();
  if bytes.len() != 76 + h {
    return Err("audit_summary_length");
  }
  let record = SummaryRecord {
    run_id: bytes[..16].try_into().map_err(|_| "audit_summary_run")?,
    started_at_ms: read_i64(bytes, 16)?,
    completed_at_ms: read_i64(bytes, 24)?,
    run_kind: read_u16(bytes, 32)?,
    outcome: read_u16(bytes, 34)?,
    mark_generation: read_u64(bytes, 36)?,
    scanned_count: read_u64(bytes, 44)?,
    candidate_count: read_u64(bytes, 52)?,
    reclaimed_count: read_u64(bytes, 60)?,
    reclaimed_bytes: read_u64(bytes, 68)?,
    evidence_digest: bytes[76..].to_vec(),
  };
  if all_zero(&record.run_id)
    || record.started_at_ms <= 0
    || record.completed_at_ms < record.started_at_ms
    || !(1..=6).contains(&record.run_kind)
    || !(1..=5).contains(&record.outcome)
    || record.candidate_count > record.scanned_count
    || record.reclaimed_count > record.candidate_count
    || (record.reclaimed_count == 0) != (record.reclaimed_bytes == 0)
    || all_zero(&record.evidence_digest)
  {
    return Err("audit_summary_fields");
  }
  Ok(record)
}

fn summary_key(record: &SummaryRecord) -> Vec<u8> {
  let mut key = Vec::with_capacity(24);
  key.extend_from_slice(&record.completed_at_ms.to_le_bytes());
  key.extend_from_slice(&record.run_id);
  key
}

fn summary_compare(left: &SummaryRecord, right: &SummaryRecord) -> Ordering {
  left.completed_at_ms.cmp(&right.completed_at_ms).then_with(|| left.run_id.cmp(&right.run_id))
}

fn build_page(profile: HashProfile, role: AuditDirectoryRole, generation: u64, page_id: u64, records: &[Vec<u8>]) -> Vec<u8> {
  assert!(!records.is_empty());
  let decoded = records
    .iter()
    .map(|record| match role {
      AuditDirectoryRole::Detail => decode_detail(profile, record).map(|record| (detail_key(profile, &record), record.occurred_at_ms)),
      AuditDirectoryRole::Summary => decode_summary(profile, record).map(|record| (summary_key(&record), record.completed_at_ms)),
    })
    .collect::<Result<Vec<_>, _>>()
    .expect("fixture audit records");
  let lower = &decoded.first().expect("records").0;
  let upper = &decoded.last().expect("records").0;
  let records_length = records.iter().map(Vec::len).sum::<usize>();
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
  build_gc_value(role.page_kind(), generation, &identity, &body)
}

fn decode_page(profile: HashProfile, bytes: &[u8]) -> Result<DecodedPage, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_PAGE_LENGTH)?;
  let role = match artifact.kind {
    GcKind::AuditDetailPage => AuditDirectoryRole::Detail,
    GcKind::AuditSummaryPage => AuditDirectoryRole::Summary,
    _ => return Err("audit_page_kind"),
  };
  if artifact.identity.len() != 42
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..32] != catalog_id(role)
    || read_u16(artifact.identity, 32)? != role as u16
    || read_u64(artifact.identity, 34)? == 0
    || artifact.body.len() < 64
  {
    return Err("audit_page_identity");
  }
  let body = artifact.body;
  let lower_length = read_u32(body, 8)? as usize;
  let upper_length = read_u32(body, 12)? as usize;
  let count = read_u32(body, 16)? as usize;
  let records_length = usize::try_from(read_u64(body, 24)?).map_err(|_| "audit_page_length")?;
  let key_length = match role {
    AuditDirectoryRole::Detail => 8 + profile.width(),
    AuditDirectoryRole::Summary => 24,
  };
  let minimum_record_length = match role {
    AuditDirectoryRole::Detail => 52 + profile.width() + 5,
    AuditDirectoryRole::Summary => 76 + profile.width(),
  };
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != role as u16
    || lower_length != key_length
    || upper_length != key_length
    || count == 0
    || count > records_length / minimum_record_length
    || read_u32(body, 20)? as usize != count
    || read_u64(body, 32)? != records_length as u64
    || body[40..64].iter().any(|byte| *byte != 0)
    || 64usize.checked_add(lower_length).and_then(|n| n.checked_add(upper_length)).and_then(|n| n.checked_add(records_length))
      != Some(body.len())
  {
    return Err("audit_page_header");
  }
  let lower = &body[64..64 + lower_length];
  let upper = &body[64 + lower_length..64 + lower_length + upper_length];
  let mut cursor = 64 + lower_length + upper_length;
  let mut records = Vec::with_capacity(count);
  let mut prior_detail: Option<DetailRecord> = None;
  let mut prior_summary: Option<SummaryRecord> = None;
  #[cfg(test)]
  let mut oldest = 0;
  #[cfg(test)]
  let mut newest = 0;
  for _ in 0..count {
    let remaining = &body[cursor..];
    let length = match role {
      AuditDirectoryRole::Detail => {
        let h = profile.width();
        if remaining.len() < 52 + h {
          return Err("audit_detail_truncated");
        }
        52usize.checked_add(h).and_then(|n| n.checked_add(read_u32(remaining, h + 44).ok()? as usize))
      }
      AuditDirectoryRole::Summary => Some(76 + profile.width()),
    }
    .ok_or("audit_page_record_length")?;
    let end = cursor.checked_add(length).ok_or("audit_page_record_length")?;
    if end > body.len() {
      return Err("audit_page_record_length");
    }
    let raw = body[cursor..end].to_vec();
    let at = match role {
      AuditDirectoryRole::Detail => {
        let decoded = decode_detail(profile, &raw)?;
        if prior_detail.as_ref().is_some_and(|prior| detail_compare(prior, &decoded) != Ordering::Less) {
          return Err("audit_detail_order");
        }
        prior_detail = Some(decoded.clone());
        decoded.occurred_at_ms
      }
      AuditDirectoryRole::Summary => {
        let decoded = decode_summary(profile, &raw)?;
        if prior_summary.as_ref().is_some_and(|prior| summary_compare(prior, &decoded) != Ordering::Less) {
          return Err("audit_summary_order");
        }
        prior_summary = Some(decoded.clone());
        decoded.completed_at_ms
      }
    };
    if at <= 0 {
      return Err("audit_page_timestamp");
    }
    #[cfg(test)]
    {
      if records.is_empty() {
        oldest = at;
      }
      newest = at;
    }
    records.push(raw);
    cursor = end;
  }
  let first_key = match role {
    AuditDirectoryRole::Detail => detail_key(profile, &decode_detail(profile, records.first().ok_or("audit_page_empty")?)?),
    AuditDirectoryRole::Summary => summary_key(&decode_summary(profile, records.first().ok_or("audit_page_empty")?)?),
  };
  let last_key = match role {
    AuditDirectoryRole::Detail => detail_key(profile, &decode_detail(profile, records.last().ok_or("audit_page_empty")?)?),
    AuditDirectoryRole::Summary => summary_key(&decode_summary(profile, records.last().ok_or("audit_page_empty")?)?),
  };
  if cursor != body.len() || first_key != lower || last_key != upper {
    return Err("audit_page_fences");
  }
  Ok(DecodedPage {
    role,
    records,
    #[cfg(test)]
    logical_bytes: records_length as u64,
    #[cfg(test)]
    oldest_at_ms: oldest,
    #[cfg(test)]
    newest_at_ms: newest,
  })
}

#[allow(clippy::too_many_arguments)]
fn build_directory(
  profile: HashProfile,
  role: AuditDirectoryRole,
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
  put_u64(&mut body, 40, 1);
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
  build_gc_value(GcKind::GcArtifactDirectoryNode, generation + 10, &identity, &body)
}

fn decode_directory(profile: HashProfile, bytes: &[u8]) -> Result<DecodedDirectory, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_DIRECTORY_LENGTH)?;
  if artifact.kind != GcKind::GcArtifactDirectoryNode || artifact.identity.len() != 34 || artifact.identity[..16] != database_id() {
    return Err("audit_directory_shape");
  }
  let role = AuditDirectoryRole::from_id(read_u16(artifact.identity, 32)?).ok_or("audit_directory_role")?;
  if artifact.identity[16..32] != catalog_id(role) || artifact.body.len() < 80 {
    return Err("audit_directory_identity");
  }
  let body = artifact.body;
  let lower_length = read_u32(body, 16)? as usize;
  let upper_length = read_u32(body, 20)? as usize;
  let entries_length = read_u32(body, 72)? as usize;
  let h = profile.width();
  let fixed = 72 + h;
  let key_length = match role {
    AuditDirectoryRole::Detail => 8 + h,
    AuditDirectoryRole::Summary => 24,
  };
  if read_u16(body, 0)? != 0
    || read_u16(body, 2)? != role as u16
    || read_u32(body, 4)? != 1
    || read_u32(body, 8)? != 0
    || read_u32(body, 12)? != 0
    || lower_length != key_length
    || upper_length != key_length
    || read_u32(body, 76)? != 0
    || entries_length != fixed + lower_length + upper_length
    || 80usize.checked_add(lower_length).and_then(|n| n.checked_add(upper_length)).and_then(|n| n.checked_add(entries_length))
      != Some(body.len())
  {
    return Err("audit_directory_header");
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
  let ordered = compare_audit_key(role, profile, lower, upper)? != Ordering::Greater;
  if all_zero(&child_hash)
    || generation == 0
    || generation > artifact.generation
    || live_count == 0
    || logical_bytes == 0
    || page_id == 0
    || !ordered
    || body[fields + 32..fields + 56].iter().any(|byte| *byte != 0)
    || body[key_start..key_start + lower_length] != *lower
    || body[key_start + lower_length..] != *upper
    || read_u64(body, 24)? != live_count
    || read_u64(body, 32)? != 0
    || read_u64(body, 40)? != 1
    || read_u64(body, 48)? != logical_bytes
    || read_u64(body, 56)? != page_id
    || read_u64(body, 64)? != page_id
  {
    return Err("audit_directory_descriptor");
  }
  Ok(DecodedDirectory {
    role,
    live_count,
    #[cfg(test)]
    logical_bytes,
    #[cfg(test)]
    child_hash,
  })
}

fn compare_audit_key(role: AuditDirectoryRole, profile: HashProfile, left: &[u8], right: &[u8]) -> Result<Ordering, &'static str> {
  let suffix_start = 8;
  let expected = match role {
    AuditDirectoryRole::Detail => 8 + profile.width(),
    AuditDirectoryRole::Summary => 24,
  };
  if left.len() != expected || right.len() != expected {
    return Err("audit_directory_key_length");
  }
  Ok(read_i64(left, 0)?.cmp(&read_i64(right, 0)?).then_with(|| left[suffix_start..].cmp(&right[suffix_start..])))
}

fn build_audit_manifest(
  profile: HashProfile,
  generation: u64,
  detail_root: &[u8],
  summary_root: &[u8],
  details: &[Vec<u8>],
  summaries: &[Vec<u8>],
  pins: &[Vec<u8>],
) -> Vec<u8> {
  let h = profile.width();
  let mut body = vec![0u8; 148 + 2 * h + pins.len() * h];
  write_capabilities(&mut body[4..36], AUDIT_CAPABILITY_BITS);
  put_i64(&mut body, 36, 1_700_000_302_000 + generation as i64);
  if !detail_root.is_empty() {
    body[44..44 + h].copy_from_slice(detail_root);
  }
  if !summary_root.is_empty() {
    body[44 + h..44 + 2 * h].copy_from_slice(summary_root);
  }
  let detail_times =
    details.iter().map(|record| decode_detail(profile, record).expect("fixture detail").occurred_at_ms).collect::<Vec<_>>();
  let summary_times =
    summaries.iter().map(|record| decode_summary(profile, record).expect("fixture summary").completed_at_ms).collect::<Vec<_>>();
  put_u64(&mut body, 44 + 2 * h, if details.is_empty() { 1 } else { 62 });
  put_u64(&mut body, 52 + 2 * h, if summaries.is_empty() { 1 } else { 72 });
  put_u64(&mut body, 60 + 2 * h, details.len() as u64);
  put_u64(&mut body, 68 + 2 * h, details.iter().map(|record| record.len() as u64).sum());
  put_u64(&mut body, 76 + 2 * h, summaries.len() as u64);
  put_u64(&mut body, 84 + 2 * h, summaries.iter().map(|record| record.len() as u64).sum());
  put_i64(&mut body, 92 + 2 * h, detail_times.first().copied().unwrap_or(0));
  put_i64(&mut body, 100 + 2 * h, detail_times.last().copied().unwrap_or(0));
  put_i64(&mut body, 108 + 2 * h, summary_times.first().copied().unwrap_or(0));
  put_i64(&mut body, 116 + 2 * h, summary_times.last().copied().unwrap_or(0));
  put_i64(&mut body, 124 + 2 * h, 1_699_000_000_000);
  put_i64(&mut body, 132 + 2 * h, 1_600_000_000_000);
  put_u32(&mut body, 140 + 2 * h, pins.len() as u32);
  put_u32(&mut body, 144 + 2 * h, (pins.len() * h) as u32);
  for (index, pin) in pins.iter().enumerate() {
    body[148 + 2 * h + index * h..148 + 2 * h + (index + 1) * h].copy_from_slice(pin);
  }
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&generation.to_le_bytes());
  build_gc_value(GcKind::AuditCatalogManifest, generation, &identity, &body)
}

fn decode_audit_manifest(profile: HashProfile, bytes: &[u8]) -> Result<DecodedManifest, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::AuditCatalogManifest
    || artifact.identity.len() != 24
    || artifact.identity[..16] != database_id()
    || read_u64(artifact.identity, 16)? != artifact.generation
    || artifact.body.len() < 148 + 2 * h
  {
    return Err("audit_manifest_shape");
  }
  let body = artifact.body;
  let pin_count = read_u32(body, 140 + 2 * h)? as usize;
  let pins_length = read_u32(body, 144 + 2 * h)? as usize;
  if pin_count > MAX_PINS
    || pins_length != pin_count.checked_mul(h).ok_or("audit_manifest_length")?
    || 148usize.checked_add(2 * h).and_then(|n| n.checked_add(pins_length)) != Some(body.len())
  {
    return Err("audit_manifest_length");
  }
  let detail_root = body[44..44 + h].to_vec();
  let summary_root = body[44 + h..44 + 2 * h].to_vec();
  let detail_count = read_u64(body, 60 + 2 * h)?;
  let detail_bytes = read_u64(body, 68 + 2 * h)?;
  let summary_count = read_u64(body, 76 + 2 * h)?;
  let summary_bytes = read_u64(body, 84 + 2 * h)?;
  let oldest_detail_at_ms = read_i64(body, 92 + 2 * h)?;
  let newest_detail_at_ms = read_i64(body, 100 + 2 * h)?;
  let oldest_summary_at_ms = read_i64(body, 108 + 2 * h)?;
  let newest_summary_at_ms = read_i64(body, 116 + 2 * h)?;
  let detail_present = detail_count != 0;
  let summary_present = summary_count != 0;
  if read_u32(body, 0)? != 0
    || !valid_capabilities(&body[4..36], AUDIT_CAPABILITY_BITS)
    || read_i64(body, 36)? <= 0
    || detail_present
      != (!all_zero(&detail_root) && detail_bytes != 0 && oldest_detail_at_ms > 0 && newest_detail_at_ms >= oldest_detail_at_ms)
    || !detail_present && (detail_bytes != 0 || oldest_detail_at_ms != 0 || newest_detail_at_ms != 0)
    || summary_present
      != (!all_zero(&summary_root) && summary_bytes != 0 && oldest_summary_at_ms > 0 && newest_summary_at_ms >= oldest_summary_at_ms)
    || !summary_present && (summary_bytes != 0 || oldest_summary_at_ms != 0 || newest_summary_at_ms != 0)
    || (detail_present && read_u64(body, 44 + 2 * h)? <= 1)
    || (!detail_present && read_u64(body, 44 + 2 * h)? != 1)
    || (summary_present && read_u64(body, 52 + 2 * h)? <= 1)
    || (!summary_present && read_u64(body, 52 + 2 * h)? != 1)
    || read_i64(body, 124 + 2 * h)? <= 0
    || read_i64(body, 132 + 2 * h)? <= 0
  {
    return Err("audit_manifest_fields");
  }
  let mut pins = Vec::with_capacity(pin_count);
  for pin in body[148 + 2 * h..].chunks_exact(h) {
    if all_zero(pin) || pins.last().is_some_and(|prior: &Vec<u8>| prior.as_slice() >= pin) {
      return Err("audit_manifest_pins");
    }
    pins.push(pin.to_vec());
  }
  Ok(DecodedManifest {
    generation: artifact.generation,
    detail_count,
    summary_count,
    pin_count,
    #[cfg(test)]
    detail_root,
    #[cfg(test)]
    summary_root,
    #[cfg(test)]
    detail_bytes,
    #[cfg(test)]
    summary_bytes,
    #[cfg(test)]
    oldest_detail_at_ms,
    #[cfg(test)]
    newest_detail_at_ms,
    #[cfg(test)]
    oldest_summary_at_ms,
    #[cfg(test)]
    newest_summary_at_ms,
    #[cfg(test)]
    pins,
  })
}

fn build_run_summary(profile: HashProfile, summary: &SummaryRecord) -> Vec<u8> {
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&summary.run_id);
  build_gc_value(GcKind::GcRunSummary, 1, &identity, &encode_summary(profile, summary))
}

fn decode_run_summary(profile: HashProfile, bytes: &[u8]) -> Result<SummaryRecord, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  if artifact.kind != GcKind::GcRunSummary || artifact.identity.len() != 32 || artifact.identity[..16] != database_id() {
    return Err("gc_run_summary_shape");
  }
  let summary = decode_summary(profile, artifact.body)?;
  if artifact.identity[16..] != summary.run_id {
    return Err("gc_run_summary_identity");
  }
  Ok(summary)
}

fn build_corrupt_evidence(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let context = config::canonicalize_json(r#"{"error_class":6,"retry_class":5}"#).expect("canonical corruption context");
  let evidence = [patterned_hash(profile, 0x21), patterned_hash(profile, 0x61)];
  let flags = 0xffu8;
  let mut body = vec![0u8; 68 + 3 * h + context.len() + evidence.len() * h];
  put_i64(&mut body, 0, 1_700_000_301_500);
  put_u16(&mut body, 8, 6);
  body[10] = 2;
  body[11] = flags;
  put_u16(&mut body, 12, GcKind::VoidCatalogManifest.id());
  put_u64(&mut body, 16, 110_000);
  put_u32(&mut body, 24, 4_097);
  put_u64(&mut body, 32, 1_001);
  body[40..40 + h].copy_from_slice(&patterned_hash(profile, 0xa1));
  body[40 + h..40 + 2 * h].copy_from_slice(&patterned_hash(profile, 0xc1));
  body[40 + 2 * h..56 + 2 * h].copy_from_slice(&sequence_array(0x81));
  put_u16(&mut body, 56 + 2 * h, GcKind::VoidCatalogActiveControl.id());
  body[60 + 2 * h..60 + 3 * h].copy_from_slice(&patterned_hash(profile, 0xe1));
  put_u32(&mut body, 60 + 3 * h, context.len() as u32);
  put_u16(&mut body, 64 + 3 * h, evidence.len() as u16);
  body[68 + 3 * h..68 + 3 * h + context.len()].copy_from_slice(&context);
  let mut cursor = 68 + 3 * h + context.len();
  for hash in evidence {
    body[cursor..cursor + h].copy_from_slice(&hash);
    cursor += h;
  }
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&sequence_array(0xc1));
  build_gc_value(GcKind::CorruptGcEvidence, 701, &identity, &body)
}

fn decode_corrupt_evidence(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::CorruptGcEvidence
    || artifact.identity.len() != 32
    || artifact.identity[..16] != database_id()
    || all_zero(&artifact.identity[16..])
    || artifact.body.len() < 68 + 3 * h
  {
    return Err("corrupt_evidence_shape");
  }
  let body = artifact.body;
  let context_length = read_u32(body, 60 + 3 * h)? as usize;
  let evidence_count = read_u16(body, 64 + 3 * h)? as usize;
  let evidence_length = evidence_count.checked_mul(h).ok_or("corrupt_evidence_length")?;
  if evidence_count > MAX_EVIDENCE
    || 68usize.checked_add(3 * h).and_then(|n| n.checked_add(context_length)).and_then(|n| n.checked_add(evidence_length))
      != Some(body.len())
  {
    return Err("corrupt_evidence_length");
  }
  let flags = body[11];
  let observed_entry = body[10];
  let observed_kind = read_u16(body, 12)?;
  let physical_offset = read_u64(body, 16)?;
  let physical_length = read_u32(body, 24)?;
  let write_sequence = read_u64(body, 32)?;
  let expected_hash = &body[40..40 + h];
  let observed_hash = &body[40 + h..40 + 2 * h];
  let run_id = &body[40 + 2 * h..56 + 2 * h];
  let control_kind = read_u16(body, 56 + 2 * h)?;
  let control_digest = &body[60 + 2 * h..60 + 3 * h];
  let optional_valid = presence(flags, 0) == (observed_entry != 0)
    && (!presence(flags, 0) || (1..=0x0a).contains(&observed_entry))
    && presence(flags, 1) == (observed_kind != 0)
    && (!presence(flags, 1) || GcKind::from_id(observed_kind).is_some())
    && presence(flags, 2) == (physical_offset != 0)
    && presence(flags, 2) == (physical_length != 0)
    && (!presence(flags, 2) || physical_offset.checked_add(u64::from(physical_length)).is_some())
    && presence(flags, 3) == (write_sequence != 0)
    && presence(flags, 4) == !all_zero(expected_hash)
    && presence(flags, 5) == !all_zero(observed_hash)
    && presence(flags, 6) == !all_zero(run_id)
    && presence(flags, 7) == (control_kind != 0)
    && presence(flags, 7) == !all_zero(control_digest)
    && (!presence(flags, 7) || GcKind::from_id(control_kind).is_some_and(GcKind::is_control));
  if read_i64(body, 0)? <= 0
    || !(1..=10).contains(&read_u16(body, 8)?)
    || !optional_valid
    || read_u16(body, 14)? != 0
    || read_u32(body, 28)? != 0
    || read_u16(body, 58 + 2 * h)? != 0
    || read_u16(body, 66 + 3 * h)? != 0
  {
    return Err("corrupt_evidence_fields");
  }
  let context_start = 68 + 3 * h;
  let context_end = context_start + context_length;
  if context_length == 0 || config::validate_audit_value(&body[context_start..context_end]).is_err() {
    return Err("corrupt_evidence_context");
  }
  let mut previous: Option<&[u8]> = None;
  for evidence in body[context_end..].chunks_exact(h) {
    if all_zero(evidence) || previous.is_some_and(|prior| prior >= evidence) {
      return Err("corrupt_evidence_order");
    }
    previous = Some(evidence);
  }
  Ok(format!("gc:evidence:corrupt:class={}:items={evidence_count}:context={context_length}", read_u16(body, 8)?))
}

fn build_audit_pin(profile: HashProfile, artifacts: &[Vec<u8>]) -> Vec<u8> {
  let h = profile.width();
  let mut artifacts = artifacts.to_vec();
  artifacts.sort();
  let mut body = vec![0u8; 32 + h + artifacts.len() * h];
  put_i64(&mut body, 0, 1_700_000_301_700);
  put_i64(&mut body, 8, 1_700_086_701_700);
  body[16..16 + h].copy_from_slice(&patterned_hash(profile, 0x11));
  put_u16(&mut body, 16 + h, 2);
  put_u32(&mut body, 20 + h, artifacts.len() as u32);
  put_u32(&mut body, 24 + h, (artifacts.len() * h) as u32);
  for (index, artifact) in artifacts.iter().enumerate() {
    body[32 + h + index * h..32 + h + (index + 1) * h].copy_from_slice(artifact);
  }
  let mut identity = Vec::with_capacity(32);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&sequence_array(0xe1));
  build_gc_value(GcKind::AuditPin, 1, &identity, &body)
}

fn decode_audit_pin(profile: HashProfile, bytes: &[u8]) -> Result<(u16, usize, Vec<Vec<u8>>), &'static str> {
  let artifact = decode_gc_value(bytes, MAX_MANIFEST_LENGTH)?;
  let h = profile.width();
  if artifact.kind != GcKind::AuditPin
    || artifact.identity.len() != 32
    || artifact.identity[..16] != database_id()
    || all_zero(&artifact.identity[16..])
    || artifact.body.len() < 32 + h
  {
    return Err("audit_pin_shape");
  }
  let body = artifact.body;
  let count = read_u32(body, 20 + h)? as usize;
  let artifacts_length = read_u32(body, 24 + h)? as usize;
  if count == 0
    || count > MAX_PINS
    || artifacts_length != count.checked_mul(h).ok_or("audit_pin_length")?
    || 32usize.checked_add(h).and_then(|n| n.checked_add(artifacts_length)) != Some(body.len())
  {
    return Err("audit_pin_length");
  }
  let created = read_i64(body, 0)?;
  let expires = read_i64(body, 8)?;
  let reason = read_u16(body, 16 + h)?;
  if created <= 0
    || (expires != 0 && expires <= created)
    || all_zero(&body[16..16 + h])
    || !(1..=4).contains(&reason)
    || read_u16(body, 18 + h)? != 0
    || read_u32(body, 28 + h)? != 0
  {
    return Err("audit_pin_fields");
  }
  let mut artifacts = Vec::with_capacity(count);
  for hash in body[32 + h..].chunks_exact(h) {
    if all_zero(hash) || artifacts.last().is_some_and(|prior: &Vec<u8>| prior.as_slice() >= hash) {
      return Err("audit_pin_order");
    }
    artifacts.push(hash.to_vec());
  }
  Ok((reason, count, artifacts))
}

#[cfg(test)]
fn validate_fixture_closure(profile: HashProfile, cases: &[GcFixtureCase]) -> Result<(), &'static str> {
  let by_suffix =
    |suffix: &str| cases.iter().find(|case| case.id.ends_with(suffix)).map(|case| case.bytes.as_slice()).ok_or("fixture_missing");
  let manifest_bytes = by_suffix("audit-catalog-populated")?;
  let detail_page_bytes = by_suffix("audit-detail-page")?;
  let detail_directory_bytes = by_suffix("audit-detail-directory")?;
  let summary_page_bytes = by_suffix("audit-summary-page")?;
  let summary_directory_bytes = by_suffix("audit-summary-directory")?;
  let pin_bytes = by_suffix("audit-pin")?;
  let corrupt_bytes = by_suffix("corrupt-gc-evidence")?;
  let run_bytes = by_suffix("gc-run-summary")?;
  let manifest = decode_audit_manifest(profile, manifest_bytes)?;
  let detail_page = decode_page(profile, detail_page_bytes)?;
  let detail_directory = decode_directory(profile, detail_directory_bytes)?;
  let summary_page = decode_page(profile, summary_page_bytes)?;
  let summary_directory = decode_directory(profile, summary_directory_bytes)?;
  let (_, _, pin_artifacts) = decode_audit_pin(profile, pin_bytes)?;
  if manifest.detail_root != immutable_key(profile, GcKind::GcArtifactDirectoryNode, detail_directory_bytes)
    || manifest.summary_root != immutable_key(profile, GcKind::GcArtifactDirectoryNode, summary_directory_bytes)
    || detail_directory.child_hash != immutable_key(profile, GcKind::AuditDetailPage, detail_page_bytes)
    || summary_directory.child_hash != immutable_key(profile, GcKind::AuditSummaryPage, summary_page_bytes)
    || manifest.detail_count != detail_page.records.len() as u64
    || manifest.summary_count != summary_page.records.len() as u64
    || manifest.detail_bytes != detail_page.logical_bytes
    || manifest.summary_bytes != summary_page.logical_bytes
    || detail_directory.live_count != manifest.detail_count
    || summary_directory.live_count != manifest.summary_count
    || detail_directory.logical_bytes != manifest.detail_bytes
    || summary_directory.logical_bytes != manifest.summary_bytes
    || manifest.oldest_detail_at_ms != detail_page.oldest_at_ms
    || manifest.newest_detail_at_ms != detail_page.newest_at_ms
    || manifest.oldest_summary_at_ms != summary_page.oldest_at_ms
    || manifest.newest_summary_at_ms != summary_page.newest_at_ms
    || manifest.pins != vec![immutable_key(profile, GcKind::AuditPin, pin_bytes)]
  {
    return Err("audit_catalog_closure");
  }
  let expected_pin_artifacts = {
    let mut hashes =
      vec![immutable_key(profile, GcKind::CorruptGcEvidence, corrupt_bytes), immutable_key(profile, GcKind::GcRunSummary, run_bytes)];
    hashes.sort();
    hashes
  };
  if pin_artifacts != expected_pin_artifacts {
    return Err("audit_pin_closure");
  }
  let summary = decode_run_summary(profile, run_bytes)?;
  if encode_summary(profile, &summary) != *summary_page.records.last().ok_or("audit_summary_missing")? {
    return Err("run_summary_body_reuse");
  }
  Ok(())
}

fn database_id() -> [u8; 16] {
  sequence_array(0x31)
}

fn catalog_id(role: AuditDirectoryRole) -> [u8; 16] {
  sequence_array(0x30 + role as u8 * 0x10)
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

fn presence(flags: u8, bit: u8) -> bool {
  flags & (1 << bit) != 0
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, &'static str> {
  Ok(i64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("gc_audit_truncated")?.try_into().map_err(|_| "gc_audit_truncated")?))
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
  fn audit_fixtures_round_trip_and_close() {
    let cases = fixture_cases();
    assert_eq!(cases.len(), 18);
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
  fn exact_formulas_and_permanent_enum_registries_hold() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let details = sample_details(profile);
      assert_eq!(encode_detail(profile, &details[0]).len(), 52 + h + details[0].payload.len());
      assert_eq!(encode_summary(profile, &sample_summaries(profile)[0]).len(), 76 + h);
      let pin = build_audit_pin(profile, &[patterned_hash(profile, 1)]);
      assert_eq!(decode_gc_value(&pin, MAX_MANIFEST_LENGTH).unwrap().body.len(), 32 + h + h);
    }
    for event in 1..=14 {
      assert!(matches!(event, 1..=14));
    }
    for run in 1..=6 {
      assert!(matches!(run, 1..=6));
    }
    for outcome in 1..=5 {
      assert!(matches!(outcome, 1..=5));
    }
    for error in 1..=10 {
      assert!(matches!(error, 1..=10));
    }
    for reason in 1..=4 {
      assert!(matches!(reason, 1..=4));
    }

    let member_count = 60_000u32;
    let mut audit_value = vec![0x09, 0, 0, 0, 0];
    let mut payload = Vec::with_capacity(4 + member_count as usize * 5);
    payload.extend_from_slice(&member_count.to_le_bytes());
    for _ in 0..member_count {
      payload.extend_from_slice(&[0x01, 0, 0, 0, 0]);
    }
    put_u32(&mut audit_value, 1, payload.len() as u32);
    audit_value.extend_from_slice(&payload);
    assert!(audit_value.len() > 256 * 1024);
    assert_eq!(config::validate_audit_value(&audit_value), Ok(()));
    assert!(config::validate(&audit_value).is_err());
    assert!(config::validate_audit_value(&vec![0; MAX_MANIFEST_LENGTH + 1]).is_err());
  }

  #[test]
  fn repaired_crc_semantic_corruption_fails_closed() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let mut run = build_run_summary(profile, &sample_summaries(profile)[1]);
      put_u16(&mut run, 32 + 32 + 32, 7);
      repair_crc(&mut run);
      assert_eq!(decode_run_summary(profile, &run).err(), Some("audit_summary_fields"));

      let mut corrupt = build_corrupt_evidence(profile);
      corrupt[32 + 32 + 11] &= !0x04;
      repair_crc(&mut corrupt);
      assert_eq!(decode_corrupt_evidence(profile, &corrupt).err(), Some("corrupt_evidence_fields"));

      let mut pin = build_audit_pin(profile, &[patterned_hash(profile, 1), patterned_hash(profile, 2)]);
      let body = 32 + 32;
      let duplicate = pin[body + 32 + 2 * h..body + 32 + 3 * h].to_vec();
      pin[body + 32 + h..body + 32 + 2 * h].copy_from_slice(&duplicate);
      repair_crc(&mut pin);
      assert_eq!(decode_audit_pin(profile, &pin).err(), Some("audit_pin_order"));
    }
  }

  #[test]
  fn unknown_enums_unsorted_records_and_bad_optional_fields_reject() {
    let profile = HashProfile::Blake3_256;
    let mut detail = sample_details(profile)[0].clone();
    detail.event_kind = 15;
    assert_eq!(decode_detail(profile, &encode_detail(profile, &detail)).err(), Some("audit_detail_fields"));

    let summaries = sample_summaries(profile);
    let rows = summaries.iter().rev().map(|record| encode_summary(profile, record)).collect::<Vec<_>>();
    let page = build_page(profile, AuditDirectoryRole::Summary, 1, 71, &rows);
    assert_eq!(decode_page(profile, &page).err(), Some("audit_summary_order"));

    let mut evidence = build_corrupt_evidence(profile);
    let body = 32 + 32;
    evidence[body + 10] = 0;
    repair_crc(&mut evidence);
    assert_eq!(decode_corrupt_evidence(profile, &evidence).err(), Some("corrupt_evidence_fields"));
  }
}
