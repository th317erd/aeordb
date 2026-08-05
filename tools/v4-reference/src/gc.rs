use crate::core::HashProfile;

const AGCA_HEADER_LENGTH: usize = 32;
const MAX_GC_ARTIFACT_LENGTH: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum GcFormat {
  GcArtifactV1,
}

impl GcFormat {
  pub fn id(self) -> &'static str {
    "gc-artifact-v1"
  }

  pub fn family(self) -> &'static str {
    "GcArtifactV1"
  }
}

#[derive(Clone)]
pub struct GcFixtureCase {
  pub id: &'static str,
  pub format: GcFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GcKind {
  QuarantineActiveControl = 0x0001,
  MarkRunActiveControl = 0x0002,
  PhysicalInventoryActiveControl = 0x0003,
  AuditCatalogActiveControl = 0x0004,
  VoidCatalogActiveControl = 0x0005,
  QuarantineManifest = 0x0010,
  RootExpiryCatalogManifest = 0x0011,
  PhysicalInventoryManifest = 0x0012,
  MarkRunCheckpoint = 0x0013,
  AuditCatalogManifest = 0x0014,
  GcRunSummary = 0x0015,
  VoidCatalogManifest = 0x0016,
  GcArtifactDirectoryNode = 0x001f,
  CandidatePage = 0x0020,
  CandidateDelta = 0x0021,
  RootExpiryPage = 0x0022,
  RetirementJournalSegment = 0x0023,
  PhysicalInventoryPage = 0x0024,
  MarkMutationJournalSegment = 0x0025,
  VoidExtentPage = 0x0026,
  VoidClaim = 0x0027,
  SweepProposal = 0x0030,
  SweepCommitReceipt = 0x0031,
  RecoveredSweepReceipt = 0x0032,
  CorruptGcEvidence = 0x0033,
  AuditDetailPage = 0x0034,
  AuditSummaryPage = 0x0035,
  AuditPin = 0x0036,
}

impl GcKind {
  pub(crate) const ALL: [Self; 28] = [
    Self::QuarantineActiveControl,
    Self::MarkRunActiveControl,
    Self::PhysicalInventoryActiveControl,
    Self::AuditCatalogActiveControl,
    Self::VoidCatalogActiveControl,
    Self::QuarantineManifest,
    Self::RootExpiryCatalogManifest,
    Self::PhysicalInventoryManifest,
    Self::MarkRunCheckpoint,
    Self::AuditCatalogManifest,
    Self::GcRunSummary,
    Self::VoidCatalogManifest,
    Self::GcArtifactDirectoryNode,
    Self::CandidatePage,
    Self::CandidateDelta,
    Self::RootExpiryPage,
    Self::RetirementJournalSegment,
    Self::PhysicalInventoryPage,
    Self::MarkMutationJournalSegment,
    Self::VoidExtentPage,
    Self::VoidClaim,
    Self::SweepProposal,
    Self::SweepCommitReceipt,
    Self::RecoveredSweepReceipt,
    Self::CorruptGcEvidence,
    Self::AuditDetailPage,
    Self::AuditSummaryPage,
    Self::AuditPin,
  ];

  pub(crate) fn from_id(id: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| kind.id() == id)
  }

  pub(crate) fn id(self) -> u16 {
    self as u16
  }

  fn name(self) -> &'static str {
    match self {
      Self::QuarantineActiveControl => "quarantine",
      Self::MarkRunActiveControl => "mark-run",
      Self::PhysicalInventoryActiveControl => "physical-inventory",
      Self::AuditCatalogActiveControl => "audit-catalog",
      Self::VoidCatalogActiveControl => "void-catalog",
      Self::QuarantineManifest => "quarantine-manifest",
      Self::RootExpiryCatalogManifest => "root-expiry-catalog-manifest",
      Self::PhysicalInventoryManifest => "physical-inventory-manifest",
      Self::MarkRunCheckpoint => "mark-run-checkpoint",
      Self::AuditCatalogManifest => "audit-catalog-manifest",
      Self::GcRunSummary => "gc-run-summary",
      Self::VoidCatalogManifest => "void-catalog-manifest",
      Self::GcArtifactDirectoryNode => "gc-artifact-directory-node",
      Self::CandidatePage => "candidate-page",
      Self::CandidateDelta => "candidate-delta",
      Self::RootExpiryPage => "root-expiry-page",
      Self::RetirementJournalSegment => "retirement-journal-segment",
      Self::PhysicalInventoryPage => "physical-inventory-page",
      Self::MarkMutationJournalSegment => "mark-mutation-journal-segment",
      Self::VoidExtentPage => "void-extent-page",
      Self::VoidClaim => "void-claim",
      Self::SweepProposal => "sweep-proposal",
      Self::SweepCommitReceipt => "sweep-commit-receipt",
      Self::RecoveredSweepReceipt => "recovered-sweep-receipt",
      Self::CorruptGcEvidence => "corrupt-gc-evidence",
      Self::AuditDetailPage => "audit-detail-page",
      Self::AuditSummaryPage => "audit-summary-page",
      Self::AuditPin => "audit-pin",
    }
  }

  fn is_control(self) -> bool {
    matches!(
      self,
      Self::QuarantineActiveControl
        | Self::MarkRunActiveControl
        | Self::PhysicalInventoryActiveControl
        | Self::AuditCatalogActiveControl
        | Self::VoidCatalogActiveControl
    )
  }

  fn control_target(self) -> Option<Self> {
    match self {
      Self::QuarantineActiveControl => Some(Self::QuarantineManifest),
      Self::MarkRunActiveControl => Some(Self::MarkRunCheckpoint),
      Self::PhysicalInventoryActiveControl => Some(Self::PhysicalInventoryManifest),
      Self::AuditCatalogActiveControl => Some(Self::AuditCatalogManifest),
      Self::VoidCatalogActiveControl => Some(Self::VoidCatalogManifest),
      _ => None,
    }
  }
}

#[derive(Debug)]
pub(crate) struct DecodedGcArtifact<'a> {
  pub kind: GcKind,
  pub generation: u64,
  pub identity: &'a [u8],
  pub body: &'a [u8],
}

#[derive(Debug)]
struct DecodedControl {
  kind: GcKind,
  slot: u8,
  sequence: u64,
  generation: u64,
  #[cfg(test)]
  database_id: [u8; 16],
  #[cfg(test)]
  target: Vec<u8>,
  key: Vec<u8>,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhysicalIncarnationId {
  pub logical_key: Vec<u8>,
  pub integrity_or_legacy_digest: Vec<u8>,
  pub wal_offset: u64,
  pub write_sequence: u64,
  pub entity_length: u32,
  pub entry_type: u8,
  pub entity_version: u8,
}

pub fn fixture_cases() -> Vec<GcFixtureCase> {
  let mut cases = Vec::with_capacity(20);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for kind in [
      GcKind::QuarantineActiveControl,
      GcKind::MarkRunActiveControl,
      GcKind::PhysicalInventoryActiveControl,
      GcKind::AuditCatalogActiveControl,
      GcKind::VoidCatalogActiveControl,
    ] {
      for slot in [0u8, 1u8] {
        let sequence = if slot == 0 { 1 } else { u64::MAX };
        let generation = 10_000 + u64::from(kind.id());
        let bytes = build_control(profile, kind, slot, sequence, generation);
        let decoded = decode_control(profile, &bytes).expect("GC control fixture must decode");
        cases.push(GcFixtureCase {
          id: leak(format!("agca-{}-{}-control-{}", profile.label(), kind.name(), if slot == 0 { 'a' } else { 'b' })),
          format: GcFormat::GcArtifactV1,
          profile,
          expected: leak(format!(
            "gc:control:{}:slot-{}:sequence={sequence}:generation={generation}",
            kind.name(),
            if slot == 0 { 'a' } else { 'b' }
          )),
          relation: Some(leak(format!("targets:{}", kind.control_target().expect("control target").name()))),
          canonical_key: Some(hex::encode(decoded.key)),
          bytes,
        });
      }
    }
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_control(profile, bytes) {
    Ok(control) => (
      format!(
        "gc:control:{}:slot-{}:sequence={}:generation={}",
        control.kind.name(),
        if control.slot == 0 { 'a' } else { 'b' },
        control.sequence,
        control.generation
      ),
      Some(hex::encode(control.key)),
    ),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let h = profile.width();
  let kind = read_u16(bytes, 6).ok().and_then(GcKind::from_id);
  vec![
    "envelope +0x000 len 32: AGCA common envelope".to_string(),
    format!("envelope artifact_kind: {}", kind.map_or("invalid", GcKind::name)),
    "identity +0x000 len 16: database_id".to_string(),
    "identity +0x010 len 1: A/B slot".to_string(),
    "body +0x000 len 8: control_sequence".to_string(),
    format!("body +0x008 len {h}: target_manifest_hash"),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

fn sample_database_id() -> [u8; 16] {
  let mut id = [0u8; 16];
  fill_sequence(&mut id, 0x31);
  id
}

fn build_control(profile: HashProfile, kind: GcKind, slot: u8, sequence: u64, generation: u64) -> Vec<u8> {
  assert!(kind.is_control());
  assert!(slot <= 1 && sequence != 0 && generation != 0);
  let mut identity = Vec::with_capacity(17);
  identity.extend_from_slice(&sample_database_id());
  identity.push(slot);
  let mut body = vec![0u8; 8 + profile.width()];
  put_u64(&mut body, 0, sequence);
  fill_sequence(&mut body[8..], 0x80u8.wrapping_add(kind.id() as u8));
  build_gc_value(kind, generation, &identity, &body)
}

pub(crate) fn build_gc_value(kind: GcKind, generation: u64, identity: &[u8], body: &[u8]) -> Vec<u8> {
  assert!(generation != 0 && !identity.is_empty() && identity.len() <= u16::MAX as usize);
  let total_length = AGCA_HEADER_LENGTH + identity.len() + body.len() + 4;
  assert!(total_length <= MAX_GC_ARTIFACT_LENGTH && body.len() <= u32::MAX as usize);
  let mut value = vec![0u8; total_length];
  value[0..4].copy_from_slice(b"AGCA");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, kind.id());
  put_u16(&mut value, 8, AGCA_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, total_length as u32);
  put_u16(&mut value, 16, identity.len() as u16);
  put_u32(&mut value, 20, body.len() as u32);
  put_u64(&mut value, 24, generation);
  value[32..32 + identity.len()].copy_from_slice(identity);
  value[32 + identity.len()..32 + identity.len() + body.len()].copy_from_slice(body);
  write_trailing_crc(&mut value);
  value
}

pub(crate) fn decode_gc_value(value: &[u8], maximum_length: usize) -> Result<DecodedGcArtifact<'_>, &'static str> {
  if value.len() < AGCA_HEADER_LENGTH + 1 + 4 || value.len() > maximum_length || value.len() > MAX_GC_ARTIFACT_LENGTH {
    return Err("gc_artifact_length");
  }
  if &value[..4] != b"AGCA" || read_u16(value, 4)? != 1 || read_u16(value, 8)? != AGCA_HEADER_LENGTH as u16 {
    return Err("gc_artifact_envelope");
  }
  let kind = GcKind::from_id(read_u16(value, 6)?).ok_or("gc_artifact_kind")?;
  let identity_length = read_u16(value, 16)? as usize;
  let body_length = read_u32(value, 20)? as usize;
  let generation = read_u64(value, 24)?;
  if read_u16(value, 10)? != 0
    || read_u32(value, 12)? as usize != value.len()
    || identity_length == 0
    || read_u16(value, 18)? != 0
    || generation == 0
    || AGCA_HEADER_LENGTH
      .checked_add(identity_length)
      .and_then(|length| length.checked_add(body_length))
      .and_then(|length| length.checked_add(4))
      != Some(value.len())
  {
    return Err("gc_artifact_metadata");
  }
  verify_trailing_crc(value)?;
  let identity_end = AGCA_HEADER_LENGTH + identity_length;
  Ok(DecodedGcArtifact {
    kind,
    generation,
    identity: &value[AGCA_HEADER_LENGTH..identity_end],
    body: &value[identity_end..value.len() - 4],
  })
}

fn decode_control(profile: HashProfile, value: &[u8]) -> Result<DecodedControl, &'static str> {
  let decoded = decode_gc_value(value, 61 + profile.width())?;
  if !decoded.kind.is_control() || decoded.identity.len() != 17 || decoded.body.len() != 8 + profile.width() {
    return Err("gc_control_shape");
  }
  let slot = decoded.identity[16];
  let sequence = read_u64(decoded.body, 0)?;
  let target = &decoded.body[8..];
  if decoded.identity[..16].iter().all(|byte| *byte == 0) || slot > 1 || sequence == 0 || target.iter().all(|byte| *byte == 0) {
    return Err("gc_control_identity_or_body");
  }
  Ok(DecodedControl {
    kind: decoded.kind,
    slot,
    sequence,
    generation: decoded.generation,
    #[cfg(test)]
    database_id: decoded.identity[..16].try_into().map_err(|_| "gc_control_identity_or_body")?,
    #[cfg(test)]
    target: target.to_vec(),
    key: control_key(profile, decoded.kind, decoded.identity),
  })
}

fn control_key(profile: HashProfile, kind: GcKind, identity: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(38 + identity.len());
  preimage.extend_from_slice(b"aeordb.gc-artifact.control.v1\0");
  preimage.extend_from_slice(&kind.id().to_le_bytes());
  preimage.extend_from_slice(identity);
  profile.digest(&preimage)
}

#[cfg(test)]
pub(crate) fn encode_physical_incarnation(profile: HashProfile, incarnation: &PhysicalIncarnationId) -> Vec<u8> {
  let h = profile.width();
  assert_eq!(incarnation.logical_key.len(), h);
  assert_eq!(incarnation.integrity_or_legacy_digest.len(), h);
  let mut bytes = vec![0u8; 24 + 2 * h];
  bytes[..h].copy_from_slice(&incarnation.logical_key);
  bytes[h..2 * h].copy_from_slice(&incarnation.integrity_or_legacy_digest);
  put_u64(&mut bytes, 2 * h, incarnation.wal_offset);
  put_u64(&mut bytes, 2 * h + 8, incarnation.write_sequence);
  put_u32(&mut bytes, 2 * h + 16, incarnation.entity_length);
  bytes[2 * h + 20] = incarnation.entry_type;
  bytes[2 * h + 21] = incarnation.entity_version;
  bytes
}

#[cfg(test)]
pub(crate) fn decode_physical_incarnation(profile: HashProfile, bytes: &[u8]) -> Result<PhysicalIncarnationId, &'static str> {
  let h = profile.width();
  if bytes.len() != 24 + 2 * h {
    return Err("physical_incarnation_length");
  }
  let wal_offset = read_u64(bytes, 2 * h)?;
  let write_sequence = read_u64(bytes, 2 * h + 8)?;
  let entity_length = read_u32(bytes, 2 * h + 16)?;
  let entry_type = bytes[2 * h + 20];
  let entity_version = bytes[2 * h + 21];
  if bytes[..h].iter().all(|byte| *byte == 0)
    || bytes[h..2 * h].iter().all(|byte| *byte == 0)
    || wal_offset == 0
    || entity_length == 0
    || !(1..=0x0a).contains(&entry_type)
    || (entity_version == 0) != (write_sequence == 0)
    || bytes[2 * h + 22..].iter().any(|byte| *byte != 0)
    || wal_offset.checked_add(u64::from(entity_length)).is_none()
  {
    return Err("physical_incarnation_fields");
  }
  Ok(PhysicalIncarnationId {
    logical_key: bytes[..h].to_vec(),
    integrity_or_legacy_digest: bytes[h..2 * h].to_vec(),
    wal_offset,
    write_sequence,
    entity_length,
    entry_type,
    entity_version,
  })
}

#[cfg(test)]
fn legacy_physical_digest(profile: HashProfile, complete_v0_entity: &[u8]) -> Vec<u8> {
  let mut preimage = Vec::with_capacity(48 + complete_v0_entity.len());
  preimage.extend_from_slice(b"aeordb.legacy-physical-incarnation.v1\0");
  preimage.extend_from_slice(complete_v0_entity);
  profile.digest(&preimage)
}

fn fill_sequence(bytes: &mut [u8], start: u8) {
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(index as u8);
  }
}

fn write_trailing_crc(bytes: &mut [u8]) {
  let crc_offset = bytes.len() - 4;
  put_u32(bytes, crc_offset, crc32fast::hash(&bytes[..crc_offset]));
}

fn verify_trailing_crc(bytes: &[u8]) -> Result<(), &'static str> {
  let crc_offset = bytes.len().checked_sub(4).ok_or("gc_artifact_crc")?;
  if read_u32(bytes, crc_offset)? != crc32fast::hash(&bytes[..crc_offset]) {
    return Err("gc_artifact_crc");
  }
  Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  Ok(u16::from_le_bytes(bytes.get(offset..offset + 2).ok_or("gc_artifact_truncated")?.try_into().map_err(|_| "gc_artifact_truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  Ok(u32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("gc_artifact_truncated")?.try_into().map_err(|_| "gc_artifact_truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  Ok(u64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("gc_artifact_truncated")?.try_into().map_err(|_| "gc_artifact_truncated")?))
}

pub(crate) fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repair_crc(bytes: &mut [u8]) {
    write_trailing_crc(bytes);
  }

  fn sample_incarnation(profile: HashProfile, version: u8) -> PhysicalIncarnationId {
    let mut logical_key = vec![0u8; profile.width()];
    let mut digest = vec![0u8; profile.width()];
    fill_sequence(&mut logical_key, 0x21);
    fill_sequence(&mut digest, 0x61);
    PhysicalIncarnationId {
      logical_key,
      integrity_or_legacy_digest: digest,
      wal_offset: 2_048,
      write_sequence: if version == 0 { 0 } else { 77 },
      entity_length: 4_096,
      entry_type: 2,
      entity_version: version,
    }
  }

  #[test]
  fn permanent_kind_registry_is_complete_unique_and_closed() {
    assert_eq!(GcKind::ALL.len(), 28);
    let mut ids = GcKind::ALL.map(GcKind::id).to_vec();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), GcKind::ALL.len());
    for kind in GcKind::ALL {
      assert_eq!(GcKind::from_id(kind.id()), Some(kind));
      assert!(!kind.name().is_empty());
    }
    for unknown in [0, 0x0006, 0x0017, 0x001e, 0x0028, 0x002f, 0x0037, u16::MAX] {
      assert_eq!(GcKind::from_id(unknown), None);
    }
  }

  #[test]
  fn control_fixtures_round_trip_with_stable_keys_and_targets() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
      let decoded = decode_control(case.profile, &case.bytes).unwrap();
      assert!(decoded.kind.control_target().is_some());
    }
  }

  #[test]
  fn every_control_fixture_byte_is_crc_or_structure_protected() {
    for case in fixture_cases() {
      for index in 0..case.bytes.len() {
        let mut changed = case.bytes.clone();
        changed[index] ^= 1;
        assert!(observe(case.profile, &changed).0.starts_with("error:"), "fixture {} byte {index}", case.id);
      }
    }
  }

  #[test]
  fn repaired_crc_envelope_identity_and_body_corruption_fails_closed() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let baseline = build_control(profile, GcKind::QuarantineActiveControl, 0, 1, 9);
      for offset in [0, 4, 6, 8, 10, 12, 16, 18, 20, 48] {
        let mut changed = baseline.clone();
        changed[offset] ^= 0x80;
        repair_crc(&mut changed);
        assert!(decode_control(profile, &changed).is_err(), "offset {offset} accepted");
      }
      let mut zero_sequence = baseline.clone();
      let body_offset = 49;
      zero_sequence[body_offset..body_offset + 8].fill(0);
      repair_crc(&mut zero_sequence);
      assert_eq!(decode_control(profile, &zero_sequence).err(), Some("gc_control_identity_or_body"));
      let mut zero_target = baseline.clone();
      zero_target[body_offset + 8..body_offset + 8 + profile.width()].fill(0);
      repair_crc(&mut zero_target);
      assert_eq!(decode_control(profile, &zero_target).err(), Some("gc_control_identity_or_body"));
    }
  }

  #[test]
  fn control_pair_selection_is_deterministic_and_ambiguous_equal_state_fails() {
    let profile = HashProfile::Blake3_256;
    let low = decode_control(profile, &build_control(profile, GcKind::VoidCatalogActiveControl, 0, 1, 7)).unwrap();
    let high = decode_control(profile, &build_control(profile, GcKind::VoidCatalogActiveControl, 1, 2, 8)).unwrap();
    assert_eq!(select_control_pair(&low, true, &high, true), Ok(Some(1)));
    assert_eq!(select_control_pair(&low, true, &high, false), Ok(Some(0)));
    assert_eq!(select_control_pair(&low, false, &high, false), Ok(None));

    let equal_a = decode_control(profile, &build_control(profile, GcKind::VoidCatalogActiveControl, 0, 3, 9)).unwrap();
    let mut equal_b = decode_control(profile, &build_control(profile, GcKind::VoidCatalogActiveControl, 1, 3, 9)).unwrap();
    equal_b.target = equal_a.target.clone();
    assert_eq!(select_control_pair(&equal_a, true, &equal_b, true), Ok(Some(0)));
    equal_b.target[0] ^= 1;
    assert_eq!(select_control_pair(&equal_a, true, &equal_b, false), Err("ambiguous_equal_sequence"));
  }

  #[test]
  fn physical_incarnation_v0_and_v1_round_trip_and_invalid_ranges_fail_closed() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      for version in [0, 1] {
        let incarnation = sample_incarnation(profile, version);
        let encoded = encode_physical_incarnation(profile, &incarnation);
        assert_eq!(encoded.len(), 24 + 2 * profile.width());
        assert_eq!(decode_physical_incarnation(profile, &encoded), Ok(incarnation));
      }
      let legacy = legacy_physical_digest(profile, b"exact v0 entity bytes");
      assert_eq!(legacy.len(), profile.width());
      assert_ne!(legacy, legacy_physical_digest(profile, b"exact v0 entity byteS"));

      let baseline = encode_physical_incarnation(profile, &sample_incarnation(profile, 1));
      let h = profile.width();
      for range in [0..h, h..2 * h, 2 * h..2 * h + 8, 2 * h + 8..2 * h + 16, 2 * h + 16..2 * h + 20] {
        let mut changed = baseline.clone();
        changed[range].fill(0);
        assert!(decode_physical_incarnation(profile, &changed).is_err());
      }
      let mut overflow = baseline.clone();
      put_u64(&mut overflow, 2 * h, u64::MAX - 1);
      put_u32(&mut overflow, 2 * h + 16, 4);
      assert_eq!(decode_physical_incarnation(profile, &overflow).err(), Some("physical_incarnation_fields"));
      let mut bad_reserved = baseline.clone();
      bad_reserved[2 * h + 22] = 1;
      assert_eq!(decode_physical_incarnation(profile, &bad_reserved).err(), Some("physical_incarnation_fields"));
    }
  }

  fn select_control_pair(
    left: &DecodedControl,
    left_closure_valid: bool,
    right: &DecodedControl,
    right_closure_valid: bool,
  ) -> Result<Option<u8>, &'static str> {
    if left.kind != right.kind || left.database_id != right.database_id || left.slot == right.slot {
      return Err("control_kind_mismatch");
    }
    if left.sequence == right.sequence && (left.target != right.target || left.generation != right.generation) {
      return Err("ambiguous_equal_sequence");
    }
    match (left_closure_valid, right_closure_valid) {
      (false, false) => Ok(None),
      (true, false) => Ok(Some(left.slot)),
      (false, true) => Ok(Some(right.slot)),
      (true, true) if left.sequence >= right.sequence => Ok(Some(left.slot)),
      (true, true) => Ok(Some(right.slot)),
    }
  }
}
