use crate::config;
use crate::core::HashProfile;
use crate::gc::{decode_physical_incarnation, encode_physical_incarnation, PhysicalIncarnationId};

const HEADER_LENGTH: usize = 32;
const CRC_LENGTH: usize = 4;
const IDENTITY_LENGTH_CAP: usize = 4_096;
const ONE_MIB: usize = 1_048_576;
const FOUR_KIB: usize = 4_096;
const MAX_SEGMENT_LENGTH: usize = 64 * ONE_MIB;
const CONTROL_ROOT: &str = "/.aeordb-system/controls/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemControlFormat {
  SystemControlV1,
  CutoverJournalV1,
}

impl SystemControlFormat {
  pub fn id(self) -> &'static str {
    match self {
      Self::SystemControlV1 => "system-control-v1",
      Self::CutoverJournalV1 => "cutover-journal-v1",
    }
  }

  pub fn family(self) -> &'static str {
    match self {
      Self::SystemControlV1 => "SystemControlV1",
      Self::CutoverJournalV1 => "SideBySideCutoverJournalV1",
    }
  }
}

#[derive(Clone)]
pub struct SystemControlFixtureCase {
  pub id: &'static str,
  pub format: SystemControlFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ControlKind {
  IndexRegistry,
  IndexOperation,
  IndexDegraded,
  LifecycleLastKnownGood,
  LifecycleDiagnostics,
  RuntimeLastKnownGood,
  RuntimeDiagnostics,
  RepairTicket,
  PathWriteLatch,
  MigrationLease,
  MigrationProgress,
  LegacyRootMapControl,
  LegacyRootMapPage,
  TaskPin,
  SemanticMutationSegment,
  RootPublicationPrepare,
  RootAdmissionCommit,
  DurabilityLatch,
  EmergencySpillCatalog,
  SideBySideCutover,
}

impl ControlKind {
  const ALL: [Self; 20] = [
    Self::IndexRegistry,
    Self::IndexOperation,
    Self::IndexDegraded,
    Self::LifecycleLastKnownGood,
    Self::LifecycleDiagnostics,
    Self::RuntimeLastKnownGood,
    Self::RuntimeDiagnostics,
    Self::RepairTicket,
    Self::PathWriteLatch,
    Self::MigrationLease,
    Self::MigrationProgress,
    Self::LegacyRootMapControl,
    Self::LegacyRootMapPage,
    Self::TaskPin,
    Self::SemanticMutationSegment,
    Self::RootPublicationPrepare,
    Self::RootAdmissionCommit,
    Self::DurabilityLatch,
    Self::EmergencySpillCatalog,
    Self::SideBySideCutover,
  ];

  fn id(self) -> u16 {
    match self {
      Self::IndexRegistry => 0x0001,
      Self::IndexOperation => 0x0002,
      Self::IndexDegraded => 0x0003,
      Self::LifecycleLastKnownGood => 0x0010,
      Self::LifecycleDiagnostics => 0x0011,
      Self::RuntimeLastKnownGood => 0x0012,
      Self::RuntimeDiagnostics => 0x0013,
      Self::RepairTicket => 0x0020,
      Self::PathWriteLatch => 0x0021,
      Self::MigrationLease => 0x0030,
      Self::MigrationProgress => 0x0031,
      Self::LegacyRootMapControl => 0x0032,
      Self::LegacyRootMapPage => 0x0033,
      Self::TaskPin => 0x0040,
      Self::SemanticMutationSegment => 0x0041,
      Self::RootPublicationPrepare => 0x0042,
      Self::RootAdmissionCommit => 0x0043,
      Self::DurabilityLatch => 0x0050,
      Self::EmergencySpillCatalog => 0x0051,
      Self::SideBySideCutover => 0x0052,
    }
  }

  #[cfg(test)]
  fn from_id(id: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| kind.id() == id)
  }

  fn magic(self) -> [u8; 4] {
    match self {
      Self::IndexRegistry => *b"AIRG",
      Self::IndexOperation => *b"AIOP",
      Self::IndexDegraded => *b"AIDG",
      Self::LifecycleLastKnownGood => *b"ALLG",
      Self::LifecycleDiagnostics => *b"ALDG",
      Self::RuntimeLastKnownGood => *b"ARLG",
      Self::RuntimeDiagnostics => *b"ARDG",
      Self::RepairTicket => *b"ARTK",
      Self::PathWriteLatch => *b"APWL",
      Self::MigrationLease => *b"AMLE",
      Self::MigrationProgress => *b"AMPR",
      Self::LegacyRootMapControl => *b"ALRM",
      Self::LegacyRootMapPage => *b"ALRP",
      Self::TaskPin => *b"ATPN",
      Self::SemanticMutationSegment => *b"ASMJ",
      Self::RootPublicationPrepare => *b"ARTX",
      Self::RootAdmissionCommit => *b"ARAC",
      Self::DurabilityLatch => *b"ADLT",
      Self::EmergencySpillCatalog => *b"ASPC",
      Self::SideBySideCutover => *b"ACUT",
    }
  }

  fn slug(self) -> &'static str {
    match self {
      Self::IndexRegistry => "index-registry",
      Self::IndexOperation => "index-operation",
      Self::IndexDegraded => "index-degraded",
      Self::LifecycleLastKnownGood => "lifecycle-lkg",
      Self::LifecycleDiagnostics => "lifecycle-diagnostics",
      Self::RuntimeLastKnownGood => "runtime-lkg",
      Self::RuntimeDiagnostics => "runtime-diagnostics",
      Self::RepairTicket => "repair-ticket",
      Self::PathWriteLatch => "path-write-latch",
      Self::MigrationLease => "migration-lease",
      Self::MigrationProgress => "migration-progress",
      Self::LegacyRootMapControl => "legacy-root-map",
      Self::LegacyRootMapPage => "legacy-root-map-page",
      Self::TaskPin => "task-pin",
      Self::SemanticMutationSegment => "semantic-mutation-segment",
      Self::RootPublicationPrepare => "root-publication-prepare",
      Self::RootAdmissionCommit => "root-admission-commit",
      Self::DurabilityLatch => "durability-latch",
      Self::EmergencySpillCatalog => "emergency-spill-catalog",
      Self::SideBySideCutover => "side-by-side-cutover",
    }
  }

  fn immutable(self) -> bool {
    matches!(self, Self::LegacyRootMapPage | Self::SemanticMutationSegment | Self::RootPublicationPrepare | Self::RootAdmissionCommit)
  }

  fn body_cap(self) -> usize {
    match self {
      Self::LifecycleLastKnownGood
      | Self::LifecycleDiagnostics
      | Self::RuntimeLastKnownGood
      | Self::RuntimeDiagnostics
      | Self::RepairTicket
      | Self::LegacyRootMapPage
      | Self::DurabilityLatch
      | Self::EmergencySpillCatalog => ONE_MIB,
      Self::SemanticMutationSegment => MAX_SEGMENT_LENGTH,
      Self::MigrationProgress => FOUR_KIB,
      _ => ONE_MIB,
    }
  }
}

#[derive(Debug)]
struct DecodedControl<'a> {
  kind: ControlKind,
  sequence: u64,
  identity: Vec<u8>,
  body: &'a [u8],
}

pub fn fixture_cases() -> Vec<SystemControlFixtureCase> {
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for kind in ControlKind::ALL {
      let body = build_body(profile, kind);
      let sequence = if kind.immutable() { 1 } else { 7 };
      let bytes = build_control(kind, sequence, &body);
      let decoded = decode_control(profile, &bytes).expect("system-control fixture must decode");
      let slot = if kind.immutable() { 2 } else { 0 };
      cases.push(SystemControlFixtureCase {
        id: leak(format!("control-{}-{}-valid", profile.label(), kind.slug())),
        format: SystemControlFormat::SystemControlV1,
        profile,
        expected: leak(format!("control:{}:sequence={sequence}:body={}", kind.slug(), decoded.body.len())),
        relation: Some(if kind.immutable() { "slot:immutable-i" } else { "slot:mutable-a" }),
        canonical_key: Some(control_path(kind, &decoded.identity, slot)),
        bytes,
      });
    }

    let body = build_body(profile, ControlKind::SideBySideCutover);
    let journal = build_cutover_journal(&body, 11, 12);
    cases.push(SystemControlFixtureCase {
      id: leak(format!("cutover-{}-external-journal-valid", profile.label())),
      format: SystemControlFormat::CutoverJournalV1,
      profile,
      expected: "cutover:external-journal:selected=12",
      relation: Some("mirrors:side-by-side-cutover-control-body"),
      canonical_key: None,
      bytes: journal,
    });
  }
  cases
}

pub fn observe(profile: HashProfile, format: SystemControlFormat, bytes: &[u8]) -> (String, Option<String>) {
  match format {
    SystemControlFormat::SystemControlV1 => match decode_control(profile, bytes) {
      Ok(decoded) => {
        let slot = if decoded.kind.immutable() { 2 } else { 0 };
        (
          format!("control:{}:sequence={}:body={}", decoded.kind.slug(), decoded.sequence, decoded.body.len()),
          Some(control_path(decoded.kind, &decoded.identity, slot)),
        )
      }
      Err(error) => (format!("error:{error}"), None),
    },
    SystemControlFormat::CutoverJournalV1 => match select_cutover_journal(profile, bytes) {
      Ok(sequence) => (format!("cutover:external-journal:selected={sequence}"), None),
      Err(error) => (format!("error:{error}"), None),
    },
  }
}

pub fn annotation_lines(format: SystemControlFormat, bytes: &[u8]) -> Vec<String> {
  match format {
    SystemControlFormat::SystemControlV1 => vec![
      "control +0x000 len 4: kind-specific magic".to_string(),
      "control +0x004 len 2: schema_version=1".to_string(),
      "control +0x006 len 2: header_length=32".to_string(),
      "control +0x008 len 4: total_length".to_string(),
      "control +0x00c len 4: kind-specific flags".to_string(),
      "control +0x010 len 8: control_sequence".to_string(),
      "control +0x018 len 4: body_length".to_string(),
      "control +0x01c len 4: reserved zero".to_string(),
      format!("control +0x020 len {}: canonical kind body", bytes.len().saturating_sub(36)),
      "control final len 4: CRC-32/ISO-HDLC".to_string(),
    ],
    SystemControlFormat::CutoverJournalV1 => vec![
      "journal +0x000 len 1024: A slot".to_string(),
      "journal +0x400 len 1024: B slot".to_string(),
      "each slot +0x000 len 4: ACUT".to_string(),
      "each slot +0x008 len 8: slot_sequence".to_string(),
      "each slot +0x020: identical SideBySideCutoverControl body".to_string(),
      "each slot +0x3fc len 4: CRC-32/ISO-HDLC".to_string(),
    ],
  }
}

fn build_control(kind: ControlKind, sequence: u64, body: &[u8]) -> Vec<u8> {
  assert!(sequence > 0);
  assert!(body.len() <= kind.body_cap());
  let total = HEADER_LENGTH.checked_add(body.len()).and_then(|value| value.checked_add(CRC_LENGTH)).expect("fixture length");
  let mut bytes = vec![0u8; total];
  bytes[..4].copy_from_slice(&kind.magic());
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, HEADER_LENGTH as u16);
  put_u32(&mut bytes, 8, total as u32);
  put_u64(&mut bytes, 16, sequence);
  put_u32(&mut bytes, 24, body.len() as u32);
  bytes[HEADER_LENGTH..HEADER_LENGTH + body.len()].copy_from_slice(body);
  write_crc(&mut bytes);
  bytes
}

fn decode_control(profile: HashProfile, bytes: &[u8]) -> Result<DecodedControl<'_>, &'static str> {
  if bytes.len() < HEADER_LENGTH + CRC_LENGTH || bytes.len() > MAX_SEGMENT_LENGTH + HEADER_LENGTH + CRC_LENGTH {
    return Err("system_control_length");
  }
  let kind = ControlKind::ALL.into_iter().find(|kind| bytes.get(..4) == Some(kind.magic().as_slice())).ok_or("system_control_magic")?;
  if read_u16(bytes, 4)? != 1 || read_u16(bytes, 6)? as usize != HEADER_LENGTH {
    return Err("system_control_version_or_header");
  }
  let total = read_u32(bytes, 8)? as usize;
  let flags = read_u32(bytes, 12)?;
  let sequence = read_u64(bytes, 16)?;
  let body_length = read_u32(bytes, 24)? as usize;
  if total != bytes.len()
    || total != HEADER_LENGTH.checked_add(body_length).and_then(|value| value.checked_add(CRC_LENGTH)).ok_or("system_control_overflow")?
    || body_length > kind.body_cap()
  {
    return Err("system_control_body_length");
  }
  if flags != 0 || sequence == 0 || read_u32(bytes, 28)? != 0 {
    return Err("system_control_header_fields");
  }
  verify_crc(bytes)?;
  let body = &bytes[HEADER_LENGTH..HEADER_LENGTH + body_length];
  let identity = validate_body(profile, kind, body)?;
  if identity.len() > IDENTITY_LENGTH_CAP {
    return Err("system_control_identity_length");
  }
  if kind.immutable() && sequence != 1 {
    return Err("system_control_immutable_sequence");
  }
  Ok(DecodedControl { kind, sequence, identity, body })
}

fn control_path(kind: ControlKind, identity: &[u8], slot: u8) -> String {
  assert!(identity.len() <= IDENTITY_LENGTH_CAP);
  assert!(matches!(slot, 0..=2));
  let mut preimage = Vec::with_capacity(42 + identity.len());
  preimage.extend_from_slice(b"aeordb.system-control-identity.v1\0");
  preimage.extend_from_slice(&kind.id().to_le_bytes());
  preimage.extend_from_slice(&(identity.len() as u16).to_le_bytes());
  preimage.extend_from_slice(identity);
  let digest = blake3::hash(&preimage).to_hex();
  let name = match slot {
    0 => "a.ctrl",
    1 => "b.ctrl",
    _ => "i.ctrl",
  };
  format!("{CONTROL_ROOT}/{:04x}/{digest}/{name}", kind.id())
}

fn build_body(profile: HashProfile, kind: ControlKind) -> Vec<u8> {
  match kind {
    ControlKind::IndexRegistry => build_index_registry(profile),
    ControlKind::IndexOperation => build_index_operation(profile),
    ControlKind::IndexDegraded => build_index_degraded(profile),
    ControlKind::LifecycleLastKnownGood => build_lkg(profile, 1),
    ControlKind::LifecycleDiagnostics => build_diagnostics(profile, 1),
    ControlKind::RuntimeLastKnownGood => build_lkg(profile, 2),
    ControlKind::RuntimeDiagnostics => build_diagnostics(profile, 2),
    ControlKind::RepairTicket => build_repair_ticket(profile),
    ControlKind::PathWriteLatch => build_path_write_latch(profile),
    ControlKind::MigrationLease => build_migration_lease(),
    ControlKind::MigrationProgress => build_migration_progress(profile),
    ControlKind::LegacyRootMapControl => build_legacy_root_map_control(profile),
    ControlKind::LegacyRootMapPage => build_legacy_root_map_page(profile),
    ControlKind::TaskPin => build_task_pin(profile),
    ControlKind::SemanticMutationSegment => build_mutation_segment(profile),
    ControlKind::RootPublicationPrepare => build_root_prepare(profile),
    ControlKind::RootAdmissionCommit => build_root_commit(profile),
    ControlKind::DurabilityLatch => build_durability_latch(profile),
    ControlKind::EmergencySpillCatalog => build_spill_catalog(profile),
    ControlKind::SideBySideCutover => build_cutover(profile),
  }
}

fn build_index_registry(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(64 + 7 * h);
  push_id(&mut body, 0x10);
  push_u64(&mut body, 3);
  push_hash(&mut body, profile, 0x20);
  push_hash(&mut body, profile, 0x30);
  push_u32(&mut body, 1);
  push_u32(&mut body, (32 + 4 * h) as u32);
  push_hash(&mut body, profile, 0x40);
  push_hash(&mut body, profile, 0x50);
  body.push(1);
  body.push(3);
  push_u16(&mut body, 1);
  push_hash(&mut body, profile, 0x60);
  push_zero_hash(&mut body, profile);
  push_hash(&mut body, profile, 0x70);
  push_u64(&mut body, 77);
  push_id(&mut body, 0x80);
  push_u16(&mut body, 0);
  push_u16(&mut body, 0);
  assert_eq!(body.len(), 64 + 7 * h);
  body
}

fn build_index_operation(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(88 + 7 * h);
  push_id(&mut body, 0x10);
  push_hash(&mut body, profile, 0x20);
  push_id(&mut body, 0x30);
  push_u16(&mut body, 2);
  push_u16(&mut body, 3);
  push_i64(&mut body, 1_700_000_000_000);
  push_i64(&mut body, 1_700_000_001_000);
  push_hash(&mut body, profile, 0x40);
  push_hash(&mut body, profile, 0x50);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  push_hash(&mut body, profile, 0x80);
  push_u64(&mut body, 70);
  push_u64(&mut body, 70);
  push_u64(&mut body, 500);
  push_u64(&mut body, 1_000);
  push_u16(&mut body, 3);
  push_u16(&mut body, 3);
  push_zero_hash(&mut body, profile);
  assert_eq!(body.len(), 88 + 7 * h);
  body
}

fn build_index_degraded(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(44 + 4 * h);
  push_id(&mut body, 0x10);
  push_hash(&mut body, profile, 0x20);
  push_u64(&mut body, 3);
  push_i64(&mut body, 1_700_000_002_000);
  push_u16(&mut body, 6);
  push_u16(&mut body, 1);
  push_hash(&mut body, profile, 0x30);
  push_hash(&mut body, profile, 0x40);
  push_hash(&mut body, profile, 0x50);
  push_u64(&mut body, 5_000);
  assert_eq!(body.len(), 44 + 4 * h);
  body
}

fn build_lkg(profile: HashProfile, config_kind: u16) -> Vec<u8> {
  let h = profile.width();
  let config = config::canonicalize_json(if config_kind == 1 {
    r#"{"garbage_collection":{"enabled":false}}"#
  } else {
    r#"{"memory":{"maximum_bytes":8589934592}}"#
  })
  .expect("canonical LKG fixture");
  let mut body = Vec::with_capacity(68 + 2 * h + config.len());
  push_id(&mut body, 0x10);
  push_u16(&mut body, config_kind);
  push_u16(&mut body, 1);
  push_i64(&mut body, 1_700_000_003_000);
  push_hash(&mut body, profile, 0x20);
  push_hash(&mut body, profile, 0x30);
  push_u32(&mut body, config.len() as u32);
  push_u32(&mut body, 0);
  body.extend_from_slice(&sample_bytes(32, 0x40));
  body.extend_from_slice(&config);
  assert_eq!(body.len(), 68 + 2 * h + config.len());
  body
}

fn build_diagnostics(profile: HashProfile, config_kind: u16) -> Vec<u8> {
  let h = profile.width();
  let detail =
    config::canonicalize_json(r#"{"disabled_capabilities":[],"errors":[],"sources":["stored"]}"#).expect("canonical diagnostic fixture");
  let mut body = Vec::with_capacity(68 + 2 * h + detail.len());
  push_id(&mut body, 0x10);
  push_u16(&mut body, config_kind);
  push_u16(&mut body, 1);
  push_i64(&mut body, 1_700_000_004_000);
  push_hash(&mut body, profile, 0x20);
  push_hash(&mut body, profile, 0x30);
  body.extend_from_slice(&sample_bytes(32, 0x40));
  push_u16(&mut body, 1);
  push_u16(&mut body, 0);
  push_u32(&mut body, detail.len() as u32);
  body.extend_from_slice(&detail);
  assert_eq!(body.len(), 68 + 2 * h + detail.len());
  body
}

fn build_repair_ticket(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let diagnostic =
    config::canonicalize_json(r#"{"class":"locator_identity_mismatch","path":"/docs/a.txt"}"#).expect("repair diagnostic fixture");
  let incarnation = PhysicalIncarnationId {
    logical_key: sample_hash(profile, 0x50),
    integrity_or_legacy_digest: sample_hash(profile, 0x60),
    wal_offset: 2_048,
    write_sequence: 71,
    entity_length: 4_096,
    entry_type: 2,
    entity_version: 1,
  };
  let evidence = [sample_hash(profile, 0x70), sample_hash(profile, 0x80)];
  let mut body = Vec::with_capacity(104 + 6 * h + diagnostic.len());
  push_id(&mut body, 0x10);
  push_id(&mut body, 0x20);
  push_i64(&mut body, 1_700_000_005_000);
  push_i64(&mut body, 1_700_000_006_000);
  push_u16(&mut body, 1);
  push_u16(&mut body, 4);
  push_u16(&mut body, 0x0055);
  push_u16(&mut body, 0x0007);
  push_hash(&mut body, profile, 0x30);
  push_hash(&mut body, profile, 0x40);
  body.push(1);
  body.extend_from_slice(&[0; 7]);
  body.extend_from_slice(&encode_physical_incarnation(profile, &incarnation));
  push_u16(&mut body, evidence.len() as u16);
  push_u16(&mut body, 0);
  push_u32(&mut body, (evidence.len() * h) as u32);
  push_u32(&mut body, diagnostic.len() as u32);
  push_u32(&mut body, 0);
  for hash in evidence {
    body.extend_from_slice(&hash);
  }
  body.extend_from_slice(&diagnostic);
  assert_eq!(body.len(), 104 + 6 * h + diagnostic.len());
  body
}

fn build_path_write_latch(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(32 + h + 32);
  push_id(&mut body, 0x10);
  push_hash(&mut body, profile, 0x20);
  push_i64(&mut body, 1_700_000_007_000);
  push_u16(&mut body, 2);
  push_u16(&mut body, 1);
  push_u32(&mut body, 0);
  push_id(&mut body, 0x30);
  push_id(&mut body, 0x50);
  assert_eq!(body.len(), 32 + h + 32);
  body
}

fn build_migration_lease() -> Vec<u8> {
  let mut body = Vec::with_capacity(132);
  for start in [0x10, 0x20, 0x30, 0x40, 0x50] {
    push_id(&mut body, start);
  }
  push_u64(&mut body, 9);
  push_i64(&mut body, 1_700_000_008_000);
  push_i64(&mut body, 1_700_000_009_000);
  push_i64(&mut body, 1_700_000_069_000);
  push_u64(&mut body, 12);
  push_u16(&mut body, 1);
  push_u16(&mut body, 3);
  push_u16(&mut body, 4);
  push_u16(&mut body, 0);
  push_u32(&mut body, 0);
  assert_eq!(body.len(), 132);
  body
}

fn build_migration_progress(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(156 + 6 * h);
  for start in [0x10, 0x20, 0x30, 0x40] {
    push_id(&mut body, start);
  }
  push_u64(&mut body, 9);
  push_u16(&mut body, 3);
  push_u16(&mut body, 4);
  push_u16(&mut body, 3);
  push_u16(&mut body, 2);
  push_u32(&mut body, 0x0001);
  for value in [12, 8, 90, 88, 88, 500, 5_000, 1_048_576] {
    push_u64(&mut body, value);
  }
  push_i64(&mut body, 1_700_000_010_000);
  push_hash(&mut body, profile, 0x50);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  push_hash(&mut body, profile, 0x80);
  push_hash(&mut body, profile, 0x90);
  push_zero_hash(&mut body, profile);
  assert_eq!(body.len(), 156 + 6 * h);
  body
}

fn build_legacy_root_map_control(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(104 + 3 * h);
  for start in [0x10, 0x20, 0x30, 0x40, 0x50] {
    push_id(&mut body, start);
  }
  push_u16(&mut body, 3);
  push_u16(&mut body, 4);
  push_u32(&mut body, 0);
  push_u64(&mut body, 2);
  push_u32(&mut body, 1);
  push_u32(&mut body, 1);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  assert_eq!(body.len(), 104 + 3 * h);
  body
}

fn build_legacy_root_map_page(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let row_length = 12 + 2 * h;
  let mut body = Vec::with_capacity(96 + 2 * h + row_length);
  for start in [0x10, 0x20, 0x30, 0x40, 0x50] {
    push_id(&mut body, start);
  }
  push_u64(&mut body, 0);
  push_zero_hash(&mut body, profile);
  push_zero_hash(&mut body, profile);
  push_u32(&mut body, 1);
  push_u32(&mut body, row_length as u32);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  push_u16(&mut body, 1);
  push_u16(&mut body, 0);
  push_u64(&mut body, 88);
  assert_eq!(body.len(), 96 + 2 * h + row_length);
  body
}

fn build_task_pin(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(76 + 4 * h);
  push_id(&mut body, 0x10);
  push_id(&mut body, 0x20);
  push_u16(&mut body, 8);
  push_u16(&mut body, 1);
  push_i64(&mut body, 1_700_000_011_000);
  push_i64(&mut body, 1_700_000_012_000);
  push_i64(&mut body, 1_700_000_072_000);
  push_u64(&mut body, 9);
  push_u32(&mut body, 2);
  push_u32(&mut body, 2);
  for start in [0x30, 0x40, 0x50, 0x60] {
    push_hash(&mut body, profile, start);
  }
  assert_eq!(body.len(), 76 + 4 * h);
  body
}

fn build_mutation_segment(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let record_length = 32 + 7 * h;
  let mut body = Vec::with_capacity(48 + h + record_length);
  push_id(&mut body, 0x10);
  push_u64(&mut body, 3);
  push_u64(&mut body, 100);
  push_u64(&mut body, 100);
  push_u32(&mut body, 1);
  push_u32(&mut body, record_length as u32);
  push_hash(&mut body, profile, 0x20);
  push_u64(&mut body, 100);
  push_hash(&mut body, profile, 0x30);
  push_u16(&mut body, 0x0001);
  push_u16(&mut body, 2);
  push_u32(&mut body, 0);
  push_hash(&mut body, profile, 0x40);
  push_hash(&mut body, profile, 0x50);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  push_hash(&mut body, profile, 0x80);
  push_hash(&mut body, profile, 0x90);
  push_id(&mut body, 0xa0);
  assert_eq!(body.len(), 48 + h + record_length);
  body
}

fn build_root_prepare(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let authority_identity = b"HEAD";
  let mut body = Vec::with_capacity(64 + 5 * h + authority_identity.len());
  push_id(&mut body, 0x10);
  push_id(&mut body, 0x20);
  push_i64(&mut body, 1_700_000_013_000);
  push_hash(&mut body, profile, 0x30);
  push_hash(&mut body, profile, 0x40);
  push_hash(&mut body, profile, 0x50);
  push_u16(&mut body, 1);
  push_u16(&mut body, 1);
  push_u16(&mut body, authority_identity.len() as u16);
  push_u16(&mut body, 0);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  push_u64(&mut body, 14);
  push_u64(&mut body, 100);
  body.extend_from_slice(authority_identity);
  assert_eq!(body.len(), 64 + 5 * h + authority_identity.len());
  body
}

fn build_root_commit(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(64 + 4 * h);
  push_id(&mut body, 0x10);
  push_hash(&mut body, profile, 0x20);
  push_id(&mut body, 0x30);
  push_i64(&mut body, 1_700_000_014_000);
  push_u16(&mut body, 1);
  push_u16(&mut body, 1);
  push_u32(&mut body, 0);
  push_hash(&mut body, profile, 0x40);
  push_hash(&mut body, profile, 0x50);
  push_u64(&mut body, 14);
  push_u64(&mut body, 100);
  push_hash(&mut body, profile, 0x60);
  assert_eq!(body.len(), 64 + 4 * h);
  body
}

fn build_durability_latch(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let diagnostic =
    config::canonicalize_json(r#"{"operation":"authority_barrier","redacted_error":"no space"}"#).expect("durability diagnostic fixture");
  let mut body = Vec::with_capacity(88 + 2 * h + diagnostic.len());
  push_id(&mut body, 0x10);
  push_u64(&mut body, 2);
  push_i64(&mut body, 1_700_000_015_000);
  push_i64(&mut body, 1_700_000_016_000);
  push_u16(&mut body, 1);
  push_u16(&mut body, 1);
  push_u16(&mut body, 4);
  push_u16(&mut body, 2);
  push_i32(&mut body, 28);
  push_u32(&mut body, 1);
  push_u64(&mut body, 14);
  push_u64(&mut body, 99);
  push_u64(&mut body, 100);
  push_hash(&mut body, profile, 0x20);
  push_hash(&mut body, profile, 0x30);
  push_u32(&mut body, diagnostic.len() as u32);
  push_u32(&mut body, 0);
  body.extend_from_slice(&diagnostic);
  assert_eq!(body.len(), 88 + 2 * h + diagnostic.len());
  body
}

fn build_spill_catalog(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let path = b"/var/lib/aeordb/spill/hot-tail-0001.bin";
  let mut row = Vec::with_capacity(72 + path.len());
  push_u16(&mut row, 1);
  push_u16(&mut row, 1);
  push_u16(&mut row, 1);
  push_u16(&mut row, 0);
  push_i64(&mut row, 1_700_000_017_000);
  push_u64(&mut row, 101);
  push_u64(&mut row, 4_096);
  row.extend_from_slice(&sample_bytes(32, 0x20));
  push_u32(&mut row, path.len() as u32);
  push_u32(&mut row, 0);
  row.extend_from_slice(path);
  let mut body = Vec::with_capacity(44 + h + row.len());
  push_id(&mut body, 0x10);
  push_u64(&mut body, 1);
  push_i64(&mut body, 1_700_000_018_000);
  push_u16(&mut body, 1);
  push_u16(&mut body, 0);
  push_u32(&mut body, 1);
  push_u32(&mut body, row.len() as u32);
  push_zero_hash(&mut body, profile);
  body.extend_from_slice(&row);
  assert_eq!(body.len(), 44 + h + row.len());
  body
}

fn build_cutover(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut body = Vec::with_capacity(140 + 5 * h);
  for start in [0x10, 0x20, 0x30, 0x40, 0x50] {
    push_id(&mut body, start);
  }
  push_u64(&mut body, 9);
  push_u16(&mut body, 3);
  push_u16(&mut body, 0);
  push_u16(&mut body, 3);
  push_u16(&mut body, 4);
  push_u32(&mut body, 0);
  push_u64(&mut body, 12);
  push_u64(&mut body, 8);
  push_u64(&mut body, 2_000_000);
  push_u64(&mut body, 2_100_000);
  push_i64(&mut body, 1_700_000_019_000);
  push_hash(&mut body, profile, 0x60);
  push_hash(&mut body, profile, 0x70);
  push_hash(&mut body, profile, 0x80);
  push_hash(&mut body, profile, 0x90);
  push_zero_hash(&mut body, profile);
  assert_eq!(body.len(), 140 + 5 * h);
  body
}

fn validate_body(profile: HashProfile, kind: ControlKind, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  if body.len() < 16 || all_zero(&body[..16]) {
    return Err("system_control_database_id");
  }
  match kind {
    ControlKind::IndexRegistry => validate_index_registry(profile, body),
    ControlKind::IndexOperation => validate_index_operation(profile, body),
    ControlKind::IndexDegraded => validate_index_degraded(profile, body),
    ControlKind::LifecycleLastKnownGood => validate_lkg(profile, body, 1),
    ControlKind::LifecycleDiagnostics => validate_diagnostics(profile, body, 1),
    ControlKind::RuntimeLastKnownGood => validate_lkg(profile, body, 2),
    ControlKind::RuntimeDiagnostics => validate_diagnostics(profile, body, 2),
    ControlKind::RepairTicket => validate_repair_ticket(profile, body),
    ControlKind::PathWriteLatch => validate_path_latch(profile, body),
    ControlKind::MigrationLease => validate_migration_lease(body),
    ControlKind::MigrationProgress => validate_migration_progress(profile, body),
    ControlKind::LegacyRootMapControl => validate_root_map_control(profile, body),
    ControlKind::LegacyRootMapPage => validate_root_map_page(profile, body),
    ControlKind::TaskPin => validate_task_pin(profile, body),
    ControlKind::SemanticMutationSegment => validate_mutation_segment(profile, body),
    ControlKind::RootPublicationPrepare => validate_root_prepare(profile, body),
    ControlKind::RootAdmissionCommit => validate_root_commit(profile, body),
    ControlKind::DurabilityLatch => validate_durability_latch(profile, body),
    ControlKind::EmergencySpillCatalog => validate_spill_catalog(profile, body),
    ControlKind::SideBySideCutover => validate_cutover(profile, body),
  }
}

fn validate_index_registry(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 32 + 3 * h;
  if body.len() < fixed {
    return Err("index_registry_length");
  }
  let generation = read_u64(body, 16)?;
  let count = read_u32(body, 24 + 2 * h)? as usize;
  let entries_length = read_u32(body, 28 + 2 * h)? as usize;
  let row_length = 32 + 4 * h;
  if generation == 0
    || any_zero_hash(body, 24, h)
    || any_zero_hash(body, 24 + h, h)
    || count > 65_535
    || entries_length != count.checked_mul(row_length).ok_or("index_registry_overflow")?
    || body.len() != fixed.checked_add(entries_length).ok_or("index_registry_overflow")?
  {
    return Err("index_registry_fields");
  }
  let mut previous: Option<&[u8]> = None;
  for row in body[fixed..].chunks_exact(row_length) {
    let index_id = &row[..h];
    if all_zero(index_id) || previous.is_some_and(|value| value >= index_id) {
      return Err("index_registry_order");
    }
    previous = Some(index_id);
    if !(1..=4).contains(&row[h])
      || !(1..=7).contains(&row[h + 1])
      || read_u16(row, h + 2)? & !0x0003 != 0
      || read_u16(row, 28 + 4 * h)? > 0x0018
      || read_u16(row, 30 + 4 * h)? != 0
    {
      return Err("index_registry_entry");
    }
    if row[h + 1] == 3 && all_zero(&row[h + 4..h + 4 + h]) {
      return Err("index_registry_active_manifest");
    }
  }
  Ok(Vec::new())
}

fn validate_index_operation(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() != 88 + 7 * h {
    return Err("index_operation_length");
  }
  let identity = body[16..32 + h].to_vec();
  if all_zero(&identity[..h])
    || all_zero(&identity[h..])
    || !(1..=4).contains(&read_u16(body, 32 + h)?)
    || !(1..=7).contains(&read_u16(body, 34 + h)?)
    || read_i64(body, 36 + h)? < 0
    || read_i64(body, 44 + h)? < read_i64(body, 36 + h)?
    || any_zero_hash(body, 52 + h, h)
    || any_zero_hash(body, 52 + 2 * h, h)
    || read_u64(body, 52 + 6 * h)? > read_u64(body, 60 + 6 * h)?
    || read_u64(body, 68 + 6 * h)? > read_u64(body, 76 + 6 * h)?
    || read_u16(body, 84 + 6 * h)? > 0x0018
    || read_u16(body, 86 + 6 * h)? > 5
  {
    return Err("index_operation_fields");
  }
  Ok(identity)
}

fn validate_index_degraded(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() != 44 + 4 * h {
    return Err("index_degraded_length");
  }
  let identity = body[16..16 + h].to_vec();
  if all_zero(&identity)
    || read_u64(body, 16 + h)? == 0
    || read_i64(body, 24 + h)? < 0
    || read_u16(body, 32 + h)? > 0x0018
    || !(1..=3).contains(&read_u16(body, 34 + h)?)
  {
    return Err("index_degraded_fields");
  }
  Ok(identity)
}

fn validate_lkg(profile: HashProfile, body: &[u8], expected_kind: u16) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 68 + 2 * h;
  if body.len() < fixed {
    return Err("config_lkg_length");
  }
  let payload_length = read_u32(body, 28 + 2 * h)? as usize;
  if read_u16(body, 16)? != expected_kind
    || read_u16(body, 18)? == 0
    || read_i64(body, 20)? < 0
    || any_zero_hash(body, 28, h)
    || any_zero_hash(body, 28 + h, h)
    || read_u32(body, 32 + 2 * h)? != 0
    || all_zero(&body[36 + 2 * h..68 + 2 * h])
    || body.len() != fixed.checked_add(payload_length).ok_or("config_lkg_overflow")?
  {
    return Err("config_lkg_fields");
  }
  config::validate_audit_value(&body[fixed..]).map_err(|_| "config_lkg_payload")?;
  Ok(Vec::new())
}

fn validate_diagnostics(profile: HashProfile, body: &[u8], expected_kind: u16) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 68 + 2 * h;
  if body.len() < fixed {
    return Err("config_diagnostics_length");
  }
  let detail_length = read_u32(body, 64 + 2 * h)? as usize;
  if read_u16(body, 16)? != expected_kind
    || !(1..=5).contains(&read_u16(body, 18)?)
    || read_i64(body, 20)? < 0
    || all_zero(&body[28 + 2 * h..60 + 2 * h])
    || body.len() != fixed.checked_add(detail_length).ok_or("config_diagnostics_overflow")?
  {
    return Err("config_diagnostics_fields");
  }
  config::validate_audit_value(&body[fixed..]).map_err(|_| "config_diagnostics_payload")?;
  Ok(Vec::new())
}

fn validate_repair_ticket(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let physical_length = 24 + 2 * h;
  let fixed = 104 + 4 * h;
  if body.len() < fixed {
    return Err("repair_ticket_length");
  }
  let identity = body[16..32].to_vec();
  let flags = read_u16(body, 54)?;
  let root = &body[56..56 + h];
  let path = &body[56 + h..56 + 2 * h];
  let incarnation_present = body[56 + 2 * h];
  let incarnation = &body[64 + 2 * h..64 + 2 * h + physical_length];
  let vector_offset = 64 + 2 * h + physical_length;
  let count = read_u16(body, vector_offset)? as usize;
  let evidence_length = read_u32(body, vector_offset + 4)? as usize;
  let diagnostic_length = read_u32(body, vector_offset + 8)? as usize;
  if all_zero(&identity)
    || read_i64(body, 32)? < 0
    || read_i64(body, 40)? < read_i64(body, 32)?
    || !(1..=4).contains(&read_u16(body, 48)?)
    || !(1..=13).contains(&read_u16(body, 50)?)
    || read_u16(body, 52)? == 0
    || flags & !0x0007 != 0
    || presence(flags, 0) == all_zero(root)
    || presence(flags, 1) == all_zero(path)
    || presence(flags, 2) != (incarnation_present == 1)
    || incarnation_present > 1
    || body[57 + 2 * h..64 + 2 * h].iter().any(|byte| *byte != 0)
    || (incarnation_present == 1 && decode_physical_incarnation(profile, incarnation).is_err())
    || (incarnation_present == 0 && !all_zero(incarnation))
    || count > 64
    || read_u16(body, vector_offset + 2)? != 0
    || evidence_length != count.checked_mul(h).ok_or("repair_ticket_overflow")?
    || read_u32(body, vector_offset + 12)? != 0
    || body.len()
      != fixed.checked_add(evidence_length).and_then(|value| value.checked_add(diagnostic_length)).ok_or("repair_ticket_overflow")?
  {
    return Err("repair_ticket_fields");
  }
  validate_sorted_hashes(&body[fixed..fixed + evidence_length], h, false)?;
  config::validate_audit_value(&body[fixed + evidence_length..]).map_err(|_| "repair_ticket_diagnostic")?;
  Ok(identity)
}

fn validate_path_latch(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 32 + h;
  if body.len() < fixed {
    return Err("path_latch_length");
  }
  let identity = body[16..16 + h].to_vec();
  let count = read_u16(body, 24 + h)? as usize;
  if all_zero(&identity)
    || read_i64(body, 16 + h)? < 0
    || !(1..=64).contains(&count)
    || !(1..=2).contains(&read_u16(body, 26 + h)?)
    || read_u32(body, 28 + h)? != 0
    || body.len() != fixed.checked_add(count.checked_mul(16).ok_or("path_latch_overflow")?).ok_or("path_latch_overflow")?
  {
    return Err("path_latch_fields");
  }
  validate_sorted_hashes(&body[fixed..], 16, false)?;
  Ok(identity)
}

fn validate_migration_lease(body: &[u8]) -> Result<Vec<u8>, &'static str> {
  if body.len() != 132 {
    return Err("migration_lease_length");
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity)
    || body[32..80].chunks_exact(16).any(all_zero)
    || read_u64(body, 80)? == 0
    || read_i64(body, 88)? < 0
    || read_i64(body, 96)? < read_i64(body, 88)?
    || read_i64(body, 104)? <= read_i64(body, 96)?
    || read_u64(body, 112)? == 0
    || !(1..=4).contains(&read_u16(body, 120)?)
    || read_u16(body, 122)? != 3
    || read_u16(body, 124)? != 4
    || read_u16(body, 126)? != 0
    || read_u32(body, 128)? != 0
  {
    return Err("migration_lease_fields");
  }
  Ok(identity)
}

fn validate_migration_progress(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() != 156 + 6 * h || body.len() > FOUR_KIB {
    return Err("migration_progress_length");
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity)
    || body[32..64].chunks_exact(16).any(all_zero)
    || read_u64(body, 64)? == 0
    || read_u16(body, 72)? != 3
    || read_u16(body, 74)? != 4
    || !(1..=8).contains(&read_u16(body, 76)?)
    || !(1..=6).contains(&read_u16(body, 78)?)
    || read_u32(body, 80)? & !0x0007 != 0
    || read_i64(body, 148)? < 0
    || any_zero_hash(body, 156 + 3 * h, h)
    || any_zero_hash(body, 156 + 4 * h, h)
  {
    return Err("migration_progress_fields");
  }
  Ok(identity)
}

fn validate_root_map_control(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() != 104 + 3 * h {
    return Err("legacy_root_map_control_length");
  }
  let identity = body[16..32].to_vec();
  let page_count = read_u32(body, 96)?;
  let record_count = read_u32(body, 100)?;
  if all_zero(&identity)
    || body[32..80].chunks_exact(16).any(all_zero)
    || read_u16(body, 80)? != 3
    || read_u16(body, 82)? != 4
    || read_u32(body, 84)? != 0
    || read_u64(body, 88)? == 0
    || (page_count == 0) != (record_count == 0)
    || presence_u32(page_count) != !all_zero(&body[104..104 + h])
    || presence_u32(page_count) != !all_zero(&body[104 + h..104 + 2 * h])
    || (record_count > 0 && all_zero(&body[104 + 2 * h..104 + 3 * h]))
  {
    return Err("legacy_root_map_control_fields");
  }
  Ok(identity)
}

fn validate_root_map_page(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 96 + 2 * h;
  let row_length = 12 + 2 * h;
  if body.len() < fixed {
    return Err("legacy_root_map_page_length");
  }
  let ordinal = read_u64(body, 80)?;
  let count = read_u32(body, 88 + 2 * h)? as usize;
  let rows_length = read_u32(body, 92 + 2 * h)? as usize;
  if body[16..80].chunks_exact(16).any(all_zero)
    || rows_length != count.checked_mul(row_length).ok_or("legacy_root_map_page_overflow")?
    || body.len() != fixed.checked_add(rows_length).ok_or("legacy_root_map_page_overflow")?
  {
    return Err("legacy_root_map_page_fields");
  }
  let mut previous: Option<&[u8]> = None;
  for row in body[fixed..].chunks_exact(row_length) {
    let legacy = &row[..h];
    if all_zero(legacy) || all_zero(&row[h..2 * h]) || previous.is_some_and(|value| value >= legacy) {
      return Err("legacy_root_map_page_order");
    }
    previous = Some(legacy);
    if !(1..=2).contains(&read_u16(row, 2 * h)?) || read_u16(row, 2 * h + 2)? > 0x0018 || read_u64(row, 2 * h + 4)? == 0 {
      return Err("legacy_root_map_page_row");
    }
  }
  let mut identity = body[16..32].to_vec();
  identity.extend_from_slice(&ordinal.to_le_bytes());
  Ok(identity)
}

fn validate_task_pin(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() < 76 {
    return Err("task_pin_length");
  }
  let identity = body[16..32].to_vec();
  let task_kind = read_u16(body, 32)?;
  let state = read_u16(body, 34)?;
  let created = read_i64(body, 36)?;
  let renewed = read_i64(body, 44)?;
  let expires = read_i64(body, 52)?;
  let root_count = read_u32(body, 68)? as usize;
  let artifact_count = read_u32(body, 72)? as usize;
  let row_count = root_count.checked_add(artifact_count).ok_or("task_pin_overflow")?;
  if all_zero(&identity)
    || !(1..=11).contains(&task_kind)
    || !(1..=3).contains(&state)
    || created < 0
    || renewed < created
    || expires < 0
    || (expires != 0 && expires <= renewed)
    || read_u64(body, 60)? == 0
    || root_count > 4_096
    || artifact_count > 4_096
    || body.len() != 76usize.checked_add(row_count.checked_mul(h).ok_or("task_pin_overflow")?).ok_or("task_pin_overflow")?
  {
    return Err("task_pin_fields");
  }
  let roots_end = 76 + root_count * h;
  validate_sorted_hashes(&body[76..roots_end], h, false)?;
  validate_sorted_hashes(&body[roots_end..], h, false)?;
  Ok(identity)
}

fn validate_mutation_segment(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 48 + h;
  let row_length = 32 + 7 * h;
  if body.len() < fixed {
    return Err("semantic_mutation_segment_length");
  }
  let ordinal = read_u64(body, 16)?;
  let first = read_u64(body, 24)?;
  let last = read_u64(body, 32)?;
  let count = read_u32(body, 40)? as usize;
  let records_length = read_u32(body, 44)? as usize;
  if first == 0
    || last < first
    || records_length != count.checked_mul(row_length).ok_or("semantic_mutation_segment_overflow")?
    || body.len() != fixed.checked_add(records_length).ok_or("semantic_mutation_segment_overflow")?
  {
    return Err("semantic_mutation_segment_fields");
  }
  let mut previous: Option<(u64, &[u8])> = None;
  for row in body[fixed..].chunks_exact(row_length) {
    let sequence = read_u64(row, 0)?;
    let mutation_id = &row[8..8 + h];
    if sequence < first
      || sequence > last
      || all_zero(mutation_id)
      || previous.is_some_and(|prior| prior.0 > sequence || (prior.0 == sequence && prior.1 >= mutation_id))
      || read_u16(row, 8 + h)? == 0
      || !(1..=10).contains(&read_u16(row, 10 + h)?)
      || read_u32(row, 12 + h)? != 0
      || all_zero(&row[row_length - 16..])
    {
      return Err("semantic_mutation_segment_record");
    }
    previous = Some((sequence, mutation_id));
  }
  if count > 0 {
    let first_row = read_u64(body, fixed)?;
    let last_row = read_u64(body, fixed + (count - 1) * row_length)?;
    if first_row != first || last_row != last {
      return Err("semantic_mutation_segment_coverage");
    }
  }
  Ok(ordinal.to_le_bytes().to_vec())
}

fn validate_root_prepare(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 64 + 5 * h;
  if body.len() < fixed {
    return Err("root_prepare_length");
  }
  let identity = body[16..32].to_vec();
  let authority_length = read_u16(body, 44 + 3 * h)? as usize;
  if all_zero(&identity)
    || read_i64(body, 32)? < 0
    || any_zero_hash(body, 40, h)
    || any_zero_hash(body, 40 + h, h)
    || any_zero_hash(body, 40 + 2 * h, h)
    || !(1..=5).contains(&read_u16(body, 40 + 3 * h)?)
    || read_u16(body, 42 + 3 * h)? != 1
    || authority_length == 0
    || authority_length > IDENTITY_LENGTH_CAP
    || read_u16(body, 46 + 3 * h)? != 0
    || all_zero(&body[48 + 4 * h..48 + 5 * h])
    || read_u64(body, 48 + 5 * h)? == 0
    || read_u64(body, 56 + 5 * h)? == 0
    || body.len() != fixed.checked_add(authority_length).ok_or("root_prepare_overflow")?
  {
    return Err("root_prepare_fields");
  }
  Ok(identity)
}

fn validate_root_commit(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() != 64 + 4 * h {
    return Err("root_commit_length");
  }
  let identity = body[16..16 + h].to_vec();
  if all_zero(&identity)
    || all_zero(&body[16 + h..32 + h])
    || read_i64(body, 32 + h)? < 0
    || !(1..=5).contains(&read_u16(body, 40 + h)?)
    || read_u16(body, 42 + h)? != 1
    || read_u32(body, 44 + h)? & !0x0001 != 0
    || any_zero_hash(body, 48 + h, h)
    || any_zero_hash(body, 48 + 2 * h, h)
    || read_u64(body, 48 + 3 * h)? == 0
    || read_u64(body, 56 + 3 * h)? == 0
    || any_zero_hash(body, 64 + 3 * h, h)
  {
    return Err("root_commit_fields");
  }
  Ok(identity)
}

fn validate_durability_latch(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 88 + 2 * h;
  if body.len() < fixed {
    return Err("durability_latch_length");
  }
  let diagnostic_length = read_u32(body, 80 + 2 * h)? as usize;
  let flags = read_u32(body, 52)?;
  let spill = &body[80..80 + h];
  if read_u64(body, 16)? == 0
    || read_i64(body, 24)? < 0
    || read_i64(body, 32)? < read_i64(body, 24)?
    || read_u16(body, 40)? != 1
    || !(1..=3).contains(&read_u16(body, 42)?)
    || !(1..=15).contains(&read_u16(body, 44)?)
    || !(1..=13).contains(&read_u16(body, 46)?)
    || read_i32(body, 48)? == 0
    || flags & !0x0001 != 0
    || presence(flags as u16, 0) == all_zero(spill)
    || read_u64(body, 56)? == 0
    || read_u64(body, 64)? == 0
    || read_u64(body, 72)? == 0
    || all_zero(&body[80 + h..80 + 2 * h])
    || read_u32(body, 84 + 2 * h)? != 0
    || body.len() != fixed.checked_add(diagnostic_length).ok_or("durability_latch_overflow")?
  {
    return Err("durability_latch_fields");
  }
  config::validate_audit_value(&body[fixed..]).map_err(|_| "durability_latch_diagnostic")?;
  Ok(Vec::new())
}

fn validate_spill_catalog(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let fixed = 44 + h;
  if body.len() < fixed {
    return Err("spill_catalog_length");
  }
  let state = read_u16(body, 32)?;
  let count = read_u32(body, 36)? as usize;
  let rows_length = read_u32(body, 40)? as usize;
  let receipt = &body[44..44 + h];
  if read_u64(body, 16)? == 0
    || read_i64(body, 24)? < 0
    || !(1..=4).contains(&state)
    || read_u16(body, 34)? != 0
    || (state == 3) != !all_zero(receipt)
    || body.len() != fixed.checked_add(rows_length).ok_or("spill_catalog_overflow")?
  {
    return Err("spill_catalog_fields");
  }
  let mut cursor = fixed;
  let mut previous: Option<(i64, u64, &[u8], &[u8])> = None;
  for _ in 0..count {
    let row_fixed_end = cursor.checked_add(72).ok_or("spill_catalog_overflow")?;
    if row_fixed_end > body.len() {
      return Err("spill_catalog_row_truncated");
    }
    let row = &body[cursor..];
    let created = read_i64(row, 8)?;
    let sequence = read_u64(row, 16)?;
    let digest = &row[32..64];
    let path_length = read_u32(row, 64)? as usize;
    let row_end = row_fixed_end.checked_add(path_length).ok_or("spill_catalog_overflow")?;
    if row_end > body.len()
      || !(1..=3).contains(&read_u16(row, 0)?)
      || !(1..=5).contains(&read_u16(row, 2)?)
      || !(1..=2).contains(&read_u16(row, 4)?)
      || read_u16(row, 6)? != 0
      || created < 0
      || sequence == 0
      || read_u64(row, 24)? == 0
      || all_zero(digest)
      || path_length == 0
      || read_u32(row, 68)? != 0
    {
      return Err("spill_catalog_row");
    }
    let path = &body[row_fixed_end..row_end];
    if read_u16(row, 4)? == 1 {
      std::str::from_utf8(path).map_err(|_| "spill_catalog_unix_path")?;
    } else if !path.len().is_multiple_of(2) {
      return Err("spill_catalog_windows_path");
    }
    let key = (created, sequence, digest, path);
    if previous.is_some_and(|prior| prior >= key) {
      return Err("spill_catalog_order");
    }
    previous = Some(key);
    cursor = row_end;
  }
  if cursor != body.len() {
    return Err("spill_catalog_row_count");
  }
  Ok(Vec::new())
}

fn validate_cutover(profile: HashProfile, body: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  if body.len() != 140 + 5 * h {
    return Err("cutover_length");
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity)
    || body[32..80].chunks_exact(16).any(all_zero)
    || read_u64(body, 80)? == 0
    || !(1..=8).contains(&read_u16(body, 88)?)
    || read_u16(body, 90)? != 0
    || read_u16(body, 92)? != 3
    || read_u16(body, 94)? != 4
    || read_u32(body, 96)? != 0
    || read_u64(body, 100)? == 0
    || read_u64(body, 108)? == 0
    || read_u64(body, 116)? == 0
    || read_u64(body, 124)? == 0
    || read_i64(body, 132)? < 0
    || body[140..140 + 4 * h].chunks_exact(h).any(all_zero)
  {
    return Err("cutover_fields");
  }
  Ok(identity)
}

fn build_cutover_journal(body: &[u8], sequence_a: u64, sequence_b: u64) -> Vec<u8> {
  let mut journal = vec![0u8; 2_048];
  for (slot, sequence) in [(0usize, sequence_a), (1usize, sequence_b)] {
    let start = slot * 1_024;
    let slot_bytes = &mut journal[start..start + 1_024];
    slot_bytes[..4].copy_from_slice(b"ACUT");
    put_u16(slot_bytes, 4, 1);
    put_u16(slot_bytes, 6, 1_024);
    put_u64(slot_bytes, 8, sequence);
    put_u32(slot_bytes, 16, body.len() as u32);
    slot_bytes[32..32 + body.len()].copy_from_slice(body);
    let crc = crc32fast::hash(&slot_bytes[..1_020]);
    put_u32(slot_bytes, 1_020, crc);
  }
  journal
}

fn select_cutover_journal(profile: HashProfile, bytes: &[u8]) -> Result<u64, &'static str> {
  if bytes.len() != 2_048 {
    return Err("cutover_journal_length");
  }
  let a = decode_cutover_slot(profile, &bytes[..1_024]);
  let b = decode_cutover_slot(profile, &bytes[1_024..]);
  match (a, b) {
    (Ok((sequence_a, _)), Ok((sequence_b, _))) if sequence_a > sequence_b => Ok(sequence_a),
    (Ok((sequence_a, _)), Ok((sequence_b, _))) if sequence_b > sequence_a => Ok(sequence_b),
    (Ok((sequence_a, body_a)), Ok((sequence_b, body_b))) if sequence_a == sequence_b && body_a == body_b => Ok(sequence_a),
    (Ok((sequence_a, _)), Ok((sequence_b, _))) if sequence_a == sequence_b => Err("ambiguous_equal_sequence"),
    (Ok((sequence, _)), Err(_)) | (Err(_), Ok((sequence, _))) => Ok(sequence),
    (Err(_), Err(_)) => Err("cutover_journal_no_valid_slot"),
    _ => Err("cutover_journal_selection"),
  }
}

fn decode_cutover_slot(profile: HashProfile, bytes: &[u8]) -> Result<(u64, &[u8]), &'static str> {
  if bytes.len() != 1_024
    || bytes.get(..4) != Some(b"ACUT")
    || read_u16(bytes, 4)? != 1
    || read_u16(bytes, 6)? != 1_024
    || read_u64(bytes, 8)? == 0
    || read_u32(bytes, 20)? != 0
    || bytes[24..32].iter().any(|byte| *byte != 0)
    || read_u32(bytes, 1_020)? != crc32fast::hash(&bytes[..1_020])
  {
    return Err("cutover_journal_slot");
  }
  let body_length = read_u32(bytes, 16)? as usize;
  let body_end = 32usize.checked_add(body_length).ok_or("cutover_journal_overflow")?;
  if body_end > 1_020 || bytes[body_end..1_020].iter().any(|byte| *byte != 0) {
    return Err("cutover_journal_padding");
  }
  let body = &bytes[32..body_end];
  validate_cutover(profile, body)?;
  Ok((read_u64(bytes, 8)?, body))
}

fn sample_hash(profile: HashProfile, start: u8) -> Vec<u8> {
  sample_bytes(profile.width(), start)
}

fn sample_bytes(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn push_id(bytes: &mut Vec<u8>, start: u8) {
  bytes.extend_from_slice(&sample_bytes(16, start));
}

fn push_hash(bytes: &mut Vec<u8>, profile: HashProfile, start: u8) {
  bytes.extend_from_slice(&sample_hash(profile, start));
}

fn push_zero_hash(bytes: &mut Vec<u8>, profile: HashProfile) {
  bytes.resize(bytes.len() + profile.width(), 0);
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(bytes: &mut Vec<u8>, value: i32) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i64(bytes: &mut Vec<u8>, value: i64) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn any_zero_hash(bytes: &[u8], offset: usize, width: usize) -> bool {
  bytes.get(offset..offset + width).is_none_or(all_zero)
}

fn presence(flags: u16, bit: u8) -> bool {
  flags & (1 << bit) != 0
}

fn presence_u32(value: u32) -> bool {
  value != 0
}

fn validate_sorted_hashes(bytes: &[u8], width: usize, allow_zero: bool) -> Result<(), &'static str> {
  if width == 0 || !bytes.len().is_multiple_of(width) {
    return Err("sorted_hash_length");
  }
  let mut previous: Option<&[u8]> = None;
  for value in bytes.chunks_exact(width) {
    if (!allow_zero && all_zero(value)) || previous.is_some_and(|prior| prior >= value) {
      return Err("sorted_hash_order");
    }
    previous = Some(value);
  }
  Ok(())
}

fn write_crc(bytes: &mut [u8]) {
  let offset = bytes.len() - CRC_LENGTH;
  put_u32(bytes, offset, crc32fast::hash(&bytes[..offset]));
}

fn verify_crc(bytes: &[u8]) -> Result<(), &'static str> {
  let offset = bytes.len().checked_sub(CRC_LENGTH).ok_or("system_control_crc")?;
  if read_u32(bytes, offset)? != crc32fast::hash(&bytes[..offset]) {
    return Err("system_control_crc");
  }
  Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  Ok(u16::from_le_bytes(bytes.get(offset..offset + 2).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  Ok(u32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, &'static str> {
  Ok(i32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  Ok(u64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, &'static str> {
  Ok(i64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
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

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn permanent_registry_is_complete_unique_and_matches_magic() {
    assert_eq!(ControlKind::ALL.len(), 20);
    let mut ids = ControlKind::ALL.map(ControlKind::id).to_vec();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), ControlKind::ALL.len());
    let mut magics = ControlKind::ALL.map(ControlKind::magic).to_vec();
    magics.sort_unstable();
    magics.dedup();
    assert_eq!(magics.len(), ControlKind::ALL.len());
    for kind in ControlKind::ALL {
      assert_eq!(ControlKind::from_id(kind.id()), Some(kind));
    }
    for unknown in [0, 0x0004, 0x000f, 0x0014, 0x0022, 0x0034, 0x0044, 0x0053, u16::MAX] {
      assert_eq!(ControlKind::from_id(unknown), None);
    }
  }

  #[test]
  fn every_fixture_round_trips_with_identity_bound_path() {
    for case in fixture_cases() {
      let observed = observe(case.profile, case.format, &case.bytes);
      assert_eq!(observed.0, case.expected, "fixture {}", case.id);
      assert_eq!(observed.1, case.canonical_key, "fixture {} key", case.id);
      if let Some(path) = observed.1 {
        assert!(path.starts_with("/.aeordb-system/controls/v1/"));
        assert!(path.ends_with(if case.relation == Some("slot:immutable-i") { "/i.ctrl" } else { "/a.ctrl" }));
      }
    }
  }

  #[test]
  fn common_header_crc_lengths_slots_and_immutable_sequences_fail_closed() {
    let profile = HashProfile::Blake3_256;
    let baseline = build_control(ControlKind::IndexDegraded, 7, &build_body(profile, ControlKind::IndexDegraded));
    for offset in [0, 4, 6, 8, 12, 16, 24, 28, baseline.len() - 1] {
      let mut changed = baseline.clone();
      changed[offset] ^= 1;
      assert!(decode_control(profile, &changed).is_err(), "offset {offset}");
    }
    let mut repaired_reserved = baseline.clone();
    repaired_reserved[28] = 1;
    write_crc(&mut repaired_reserved);
    assert_eq!(decode_control(profile, &repaired_reserved).err(), Some("system_control_header_fields"));

    let immutable_body = build_body(profile, ControlKind::RootAdmissionCommit);
    let immutable = build_control(ControlKind::RootAdmissionCommit, 2, &immutable_body);
    assert_eq!(decode_control(profile, &immutable).err(), Some("system_control_immutable_sequence"));
  }

  #[test]
  fn mutable_pair_selection_accepts_identical_equal_and_rejects_disagreement() {
    let profile = HashProfile::Blake3_256;
    let body = build_body(profile, ControlKind::IndexRegistry);
    let a_bytes = build_control(ControlKind::IndexRegistry, 7, &body);
    let b_bytes = build_control(ControlKind::IndexRegistry, 8, &body);
    let a = decode_control(profile, &a_bytes).unwrap();
    let b = decode_control(profile, &b_bytes).unwrap();
    assert_eq!(select_control_pair(&a, &b), Ok(1));
    let equal_bytes = build_control(ControlKind::IndexRegistry, 7, &body);
    let equal = decode_control(profile, &equal_bytes).unwrap();
    assert_eq!(select_control_pair(&a, &equal), Ok(0));
    let other_body = build_index_registry(HashProfile::Blake3_256);
    let mut changed = other_body;
    changed[16] = 4;
    let disagreement_bytes = build_control(ControlKind::IndexRegistry, 7, &changed);
    let disagreement = decode_control(profile, &disagreement_bytes).unwrap();
    assert_eq!(select_control_pair(&a, &disagreement), Err("ambiguous_equal_sequence"));
  }

  #[test]
  fn repaired_crc_semantic_mutations_reject_every_kind() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      for kind in ControlKind::ALL {
        let mut bytes = build_control(kind, if kind.immutable() { 1 } else { 7 }, &build_body(profile, kind));
        bytes[32..48].fill(0);
        write_crc(&mut bytes);
        assert!(decode_control(profile, &bytes).is_err(), "kind {}", kind.slug());
      }
    }
  }

  #[test]
  fn cutover_journal_selection_torn_slots_and_ambiguity_are_deterministic() {
    let profile = HashProfile::Blake3_256;
    let body = build_cutover(profile);
    let journal = build_cutover_journal(&body, 11, 12);
    assert_eq!(select_cutover_journal(profile, &journal), Ok(12));

    let mut torn = journal.clone();
    torn[1_100] ^= 1;
    assert_eq!(select_cutover_journal(profile, &torn), Ok(11));

    let mut ambiguous = build_cutover_journal(&body, 12, 12);
    let state_offset = 1_024 + 32 + 88;
    put_u16(&mut ambiguous, state_offset, 4);
    let crc = crc32fast::hash(&ambiguous[1_024..2_044]);
    put_u32(&mut ambiguous, 2_044, crc);
    assert_eq!(select_cutover_journal(profile, &ambiguous), Err("ambiguous_equal_sequence"));
  }

  #[test]
  fn identity_digest_is_always_blake3_and_slot_names_do_not_alias() {
    let profile = HashProfile::Sha512;
    let bytes = build_control(ControlKind::IndexDegraded, 7, &build_body(profile, ControlKind::IndexDegraded));
    let decoded = decode_control(profile, &bytes).unwrap();
    let a = control_path(decoded.kind, &decoded.identity, 0);
    let b = control_path(decoded.kind, &decoded.identity, 1);
    assert_ne!(a, b);
    let digest = a.split('/').nth_back(1).unwrap();
    assert_eq!(digest.len(), 64);
  }

  fn select_control_pair(a: &DecodedControl<'_>, b: &DecodedControl<'_>) -> Result<usize, &'static str> {
    if a.kind != b.kind || a.identity != b.identity || a.kind.immutable() {
      return Err("control_pair_identity");
    }
    if a.sequence > b.sequence {
      Ok(0)
    } else if b.sequence > a.sequence {
      Ok(1)
    } else if a.body == b.body {
      Ok(0)
    } else {
      Err("ambiguous_equal_sequence")
    }
  }
}
