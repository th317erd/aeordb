use std::cmp::Ordering;

use super::config_value::{CanonicalValueBounds, validate_canonical_value};
use super::gc::decode_physical_incarnation;
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const HEADER_LENGTH: usize = 32;
const CRC_LENGTH: usize = 4;
const CONTROL_ROOT: &str = "/.aeordb-system/controls/v1";
pub const SYSTEM_CONTROL_IDENTITY_LENGTH_CAP: usize = 4_096;
const ONE_MIB: usize = 1_048_576;
const FOUR_KIB: usize = 4_096;
const MAX_SEGMENT_LENGTH: usize = 64 * ONE_MIB;
const JOURNAL_LENGTH: usize = 2_048;
const JOURNAL_SLOT_LENGTH: usize = 1_024;
const JOURNAL_SLOT_CRC_OFFSET: usize = 1_020;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum SystemControlKindV1 {
  IndexRegistry = 0x0001,
  IndexOperation = 0x0002,
  IndexDegraded = 0x0003,
  LifecycleLastKnownGood = 0x0010,
  LifecycleDiagnostics = 0x0011,
  RuntimeLastKnownGood = 0x0012,
  RuntimeDiagnostics = 0x0013,
  RepairTicket = 0x0020,
  PathWriteLatch = 0x0021,
  MigrationLease = 0x0030,
  MigrationProgress = 0x0031,
  LegacyRootMapControl = 0x0032,
  LegacyRootMapPage = 0x0033,
  TaskPin = 0x0040,
  SemanticMutationSegment = 0x0041,
  RootPublicationPrepare = 0x0042,
  RootAdmissionCommit = 0x0043,
  DurabilityLatch = 0x0050,
  EmergencySpillCatalog = 0x0051,
  SideBySideCutover = 0x0052,
}

impl SystemControlKindV1 {
  pub const ALL: [Self; 20] = [
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

  pub fn from_u16(value: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| *kind as u16 == value)
  }

  pub fn from_magic(magic: &[u8]) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| magic == kind.magic())
  }

  pub fn magic(self) -> &'static [u8; 4] {
    match self {
      Self::IndexRegistry => b"AIRG",
      Self::IndexOperation => b"AIOP",
      Self::IndexDegraded => b"AIDG",
      Self::LifecycleLastKnownGood => b"ALLG",
      Self::LifecycleDiagnostics => b"ALDG",
      Self::RuntimeLastKnownGood => b"ARLG",
      Self::RuntimeDiagnostics => b"ARDG",
      Self::RepairTicket => b"ARTK",
      Self::PathWriteLatch => b"APWL",
      Self::MigrationLease => b"AMLE",
      Self::MigrationProgress => b"AMPR",
      Self::LegacyRootMapControl => b"ALRM",
      Self::LegacyRootMapPage => b"ALRP",
      Self::TaskPin => b"ATPN",
      Self::SemanticMutationSegment => b"ASMJ",
      Self::RootPublicationPrepare => b"ARTX",
      Self::RootAdmissionCommit => b"ARAC",
      Self::DurabilityLatch => b"ADLT",
      Self::EmergencySpillCatalog => b"ASPC",
      Self::SideBySideCutover => b"ACUT",
    }
  }

  pub fn slug(self) -> &'static str {
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

  pub fn is_immutable(self) -> bool {
    matches!(self, Self::LegacyRootMapPage | Self::SemanticMutationSegment | Self::RootPublicationPrepare | Self::RootAdmissionCommit)
  }

  pub fn body_cap(self) -> usize {
    match self {
      Self::SemanticMutationSegment => MAX_SEGMENT_LENGTH,
      Self::MigrationProgress => FOUR_KIB,
      _ => ONE_MIB,
    }
  }

  pub fn encoded_cap(self) -> usize {
    HEADER_LENGTH + self.body_cap() + CRC_LENGTH
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemControlSlotV1 {
  A,
  B,
  Immutable,
}

impl SystemControlSlotV1 {
  pub fn file_name(self) -> &'static str {
    match self {
      Self::A => "a.ctrl",
      Self::B => "b.ctrl",
      Self::Immutable => "i.ctrl",
    }
  }
}

#[derive(Clone, Debug)]
pub struct SystemControlV1<'a> {
  pub kind: SystemControlKindV1,
  pub sequence: u64,
  pub database_id: &'a [u8],
  pub identity: Vec<u8>,
  pub body: &'a [u8],
}

impl SystemControlV1<'_> {
  pub fn summary(&self) -> String {
    format!("control:{}:sequence={}:body={}", self.kind.slug(), self.sequence, self.body.len())
  }

  pub fn canonical_path(&self) -> String {
    let slot = if self.kind.is_immutable() { SystemControlSlotV1::Immutable } else { SystemControlSlotV1::A };
    self.canonical_path_for_slot(slot).expect("default slot matches control mutability")
  }

  pub fn canonical_path_for_slot(&self, slot: SystemControlSlotV1) -> FormatResult<String> {
    system_control_path(self.kind, &self.identity, slot)
  }
}

#[derive(Clone, Debug)]
pub struct SystemControlSelectionV1<'a> {
  pub selected_slot: SystemControlSlotV1,
  pub control: SystemControlV1<'a>,
  pub redundancy_degraded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityLatchBodyV1 {
  pub database_id: [u8; 16],
  pub latch_generation: u64,
  pub first_failure_at_ms: i64,
  pub latest_failure_at_ms: i64,
  pub severity: u16,
  pub state: u16,
  pub failed_operation: u16,
  pub os_error_class: u16,
  pub os_error_code: i32,
  pub flags: u32,
  pub last_selected_header_sequence: u64,
  pub last_durable_write_sequence: u64,
  pub last_durable_publication_sequence: u64,
  pub emergency_spill_catalog_payload_hash: Vec<u8>,
  pub evidence_digest: Vec<u8>,
  pub diagnostic: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmergencySpillCatalogRowV1 {
  pub source_location_class: u16,
  pub replay_state: u16,
  pub path_encoding: u16,
  pub flags: u16,
  pub created_at_ms: i64,
  pub creation_sequence: u64,
  pub file_length: u64,
  pub complete_file_digest: Vec<u8>,
  pub native_path: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmergencySpillCatalogBodyV1 {
  pub database_id: [u8; 16],
  pub catalog_generation: u64,
  pub discovered_at_ms: i64,
  pub state: u16,
  pub flags: u16,
  pub repair_receipt_hash: Vec<u8>,
  pub rows: Vec<EmergencySpillCatalogRowV1>,
}

#[derive(Clone, Debug)]
pub struct CutoverJournalSelectionV1<'a> {
  pub selected_slot: SystemControlSlotV1,
  pub sequence: u64,
  pub body: &'a [u8],
  pub redundancy_degraded: bool,
}

impl CutoverJournalSelectionV1<'_> {
  pub fn summary(&self) -> String {
    format!("cutover:external-journal:selected={}", self.sequence)
  }
}

pub fn decode_system_control(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SystemControlV1<'_>> {
  if bytes.len() < HEADER_LENGTH + CRC_LENGTH {
    return Err(trailing_error("system_control_length", "control is shorter than its framing"));
  }
  if bytes.len() > MAX_SEGMENT_LENGTH + HEADER_LENGTH + CRC_LENGTH {
    return Err(amplification_error("system_control_length", bytes.len(), MAX_SEGMENT_LENGTH + HEADER_LENGTH + CRC_LENGTH));
  }
  let kind = SystemControlKindV1::from_magic(bytes.get(..4).unwrap_or_default())
    .ok_or_else(|| magic_error("system_control_magic", "unknown control magic"))?;
  if u16_at(bytes, 4)? != 1 || usize::from(u16_at(bytes, 6)?) != HEADER_LENGTH {
    return Err(magic_error("system_control_version_or_header", "unsupported control version or header length"));
  }
  let total = usize::try_from(u32_at(bytes, 8)?).map_err(|_| overflow_error("control total length"))?;
  let flags = u32_at(bytes, 12)?;
  let sequence = u64_at(bytes, 16)?;
  let body_length = usize::try_from(u32_at(bytes, 24)?).map_err(|_| overflow_error("control body length"))?;
  if body_length > kind.body_cap() {
    return Err(amplification_error("system_control_body_cap", body_length, kind.body_cap()));
  }
  let expected_total = checked_add(checked_add(HEADER_LENGTH, body_length, "control body")?, CRC_LENGTH, "control CRC")?;
  if total != bytes.len() || total != expected_total {
    return Err(trailing_error("system_control_body_length", "control lengths do not close"));
  }
  if flags != 0 || u32_at(bytes, 28)? != 0 {
    return Err(reserved_error("system_control_header_fields", "control flags and reserve must be zero"));
  }
  if sequence == 0 {
    return Err(identity_error("system_control_sequence", "control sequence must be nonzero"));
  }
  verify_crc(bytes, "system_control_crc")?;
  let body = &bytes[HEADER_LENGTH..HEADER_LENGTH + body_length];
  let identity = validate_body(kind, body, algorithm)?;
  if identity.len() > SYSTEM_CONTROL_IDENTITY_LENGTH_CAP {
    return Err(amplification_error("system_control_identity_length", identity.len(), SYSTEM_CONTROL_IDENTITY_LENGTH_CAP));
  }
  if kind.is_immutable() && sequence != 1 {
    return Err(identity_error("system_control_immutable_sequence", "immutable controls require sequence one"));
  }
  Ok(SystemControlV1 { kind, sequence, database_id: &body[..16], identity, body })
}

pub fn decode_durability_latch_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<DurabilityLatchBodyV1> {
  validate_durability_latch(body, algorithm)?;
  let hash_width = algorithm.hash_length();
  let fixed = 88 + 2 * hash_width;
  Ok(DurabilityLatchBodyV1 {
    database_id: body[..16].try_into().expect("validated durability latch database ID width"),
    latch_generation: u64_at(body, 16)?,
    first_failure_at_ms: i64_at(body, 24)?,
    latest_failure_at_ms: i64_at(body, 32)?,
    severity: u16_at(body, 40)?,
    state: u16_at(body, 42)?,
    failed_operation: u16_at(body, 44)?,
    os_error_class: u16_at(body, 46)?,
    os_error_code: i32_at(body, 48)?,
    flags: u32_at(body, 52)?,
    last_selected_header_sequence: u64_at(body, 56)?,
    last_durable_write_sequence: u64_at(body, 64)?,
    last_durable_publication_sequence: u64_at(body, 72)?,
    emergency_spill_catalog_payload_hash: body[80..80 + hash_width].to_vec(),
    evidence_digest: body[80 + hash_width..80 + 2 * hash_width].to_vec(),
    diagnostic: body[fixed..].to_vec(),
  })
}

pub fn decode_emergency_spill_catalog_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<EmergencySpillCatalogBodyV1> {
  validate_spill_catalog(body, algorithm)?;
  let hash_width = algorithm.hash_length();
  let fixed = 44 + hash_width;
  let row_count = usize::try_from(u32_at(body, 36)?).map_err(|_| overflow_error("spill row count"))?;
  let mut rows = Vec::with_capacity(row_count);
  let mut cursor = fixed;
  for _ in 0..row_count {
    let path_length = usize::try_from(u32_at(body, cursor + 64)?).map_err(|_| overflow_error("spill path length"))?;
    let path_start = checked_add(cursor, 72, "spill row fixed body")?;
    let path_end = checked_add(path_start, path_length, "spill row path")?;
    rows.push(EmergencySpillCatalogRowV1 {
      source_location_class: u16_at(body, cursor)?,
      replay_state: u16_at(body, cursor + 2)?,
      path_encoding: u16_at(body, cursor + 4)?,
      flags: u16_at(body, cursor + 6)?,
      created_at_ms: i64_at(body, cursor + 8)?,
      creation_sequence: u64_at(body, cursor + 16)?,
      file_length: u64_at(body, cursor + 24)?,
      complete_file_digest: body[cursor + 32..cursor + 64].to_vec(),
      native_path: body[path_start..path_end].to_vec(),
    });
    cursor = path_end;
  }
  Ok(EmergencySpillCatalogBodyV1 {
    database_id: body[..16].try_into().expect("validated spill catalog database ID width"),
    catalog_generation: u64_at(body, 16)?,
    discovered_at_ms: i64_at(body, 24)?,
    state: u16_at(body, 32)?,
    flags: u16_at(body, 34)?,
    repair_receipt_hash: body[44..fixed].to_vec(),
    rows,
  })
}

pub fn encode_durability_latch_control(sequence: u64, latch: &DurabilityLatchBodyV1, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  require_hash_width(&latch.emergency_spill_catalog_payload_hash, hash_width, "durability spill catalog payload hash")?;
  require_hash_width(&latch.evidence_digest, hash_width, "durability evidence digest")?;
  let diagnostic_length = u32::try_from(latch.diagnostic.len()).map_err(|_| overflow_error("durability diagnostic length"))?;
  let capacity = checked_add(88 + 2 * hash_width, latch.diagnostic.len(), "durability latch body")?;
  let mut body = Vec::with_capacity(capacity);
  body.extend_from_slice(&latch.database_id);
  body.extend_from_slice(&latch.latch_generation.to_le_bytes());
  body.extend_from_slice(&latch.first_failure_at_ms.to_le_bytes());
  body.extend_from_slice(&latch.latest_failure_at_ms.to_le_bytes());
  body.extend_from_slice(&latch.severity.to_le_bytes());
  body.extend_from_slice(&latch.state.to_le_bytes());
  body.extend_from_slice(&latch.failed_operation.to_le_bytes());
  body.extend_from_slice(&latch.os_error_class.to_le_bytes());
  body.extend_from_slice(&latch.os_error_code.to_le_bytes());
  body.extend_from_slice(&latch.flags.to_le_bytes());
  body.extend_from_slice(&latch.last_selected_header_sequence.to_le_bytes());
  body.extend_from_slice(&latch.last_durable_write_sequence.to_le_bytes());
  body.extend_from_slice(&latch.last_durable_publication_sequence.to_le_bytes());
  body.extend_from_slice(&latch.emergency_spill_catalog_payload_hash);
  body.extend_from_slice(&latch.evidence_digest);
  body.extend_from_slice(&diagnostic_length.to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(&latch.diagnostic);
  encode_system_control(SystemControlKindV1::DurabilityLatch, sequence, &body, algorithm)
}

pub fn encode_emergency_spill_catalog_control(
  sequence: u64,
  catalog: &EmergencySpillCatalogBodyV1,
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  require_hash_width(&catalog.repair_receipt_hash, hash_width, "spill repair receipt hash")?;
  let mut row_bytes = Vec::new();
  for row in &catalog.rows {
    require_hash_width(&row.complete_file_digest, 32, "spill complete-file BLAKE3 digest")?;
    let path_length = u32::try_from(row.native_path.len()).map_err(|_| overflow_error("spill path length"))?;
    row_bytes.extend_from_slice(&row.source_location_class.to_le_bytes());
    row_bytes.extend_from_slice(&row.replay_state.to_le_bytes());
    row_bytes.extend_from_slice(&row.path_encoding.to_le_bytes());
    row_bytes.extend_from_slice(&row.flags.to_le_bytes());
    row_bytes.extend_from_slice(&row.created_at_ms.to_le_bytes());
    row_bytes.extend_from_slice(&row.creation_sequence.to_le_bytes());
    row_bytes.extend_from_slice(&row.file_length.to_le_bytes());
    row_bytes.extend_from_slice(&row.complete_file_digest);
    row_bytes.extend_from_slice(&path_length.to_le_bytes());
    row_bytes.extend_from_slice(&0u32.to_le_bytes());
    row_bytes.extend_from_slice(&row.native_path);
  }
  let row_count = u32::try_from(catalog.rows.len()).map_err(|_| overflow_error("spill row count"))?;
  let rows_length = u32::try_from(row_bytes.len()).map_err(|_| overflow_error("spill rows length"))?;
  let capacity = checked_add(44 + hash_width, row_bytes.len(), "spill catalog body")?;
  let mut body = Vec::with_capacity(capacity);
  body.extend_from_slice(&catalog.database_id);
  body.extend_from_slice(&catalog.catalog_generation.to_le_bytes());
  body.extend_from_slice(&catalog.discovered_at_ms.to_le_bytes());
  body.extend_from_slice(&catalog.state.to_le_bytes());
  body.extend_from_slice(&catalog.flags.to_le_bytes());
  body.extend_from_slice(&row_count.to_le_bytes());
  body.extend_from_slice(&rows_length.to_le_bytes());
  body.extend_from_slice(&catalog.repair_receipt_hash);
  body.extend_from_slice(&row_bytes);
  encode_system_control(SystemControlKindV1::EmergencySpillCatalog, sequence, &body, algorithm)
}

fn encode_system_control(kind: SystemControlKindV1, sequence: u64, body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  if sequence == 0 {
    return Err(identity_error("system_control_sequence", "control sequence must be nonzero"));
  }
  if body.len() > kind.body_cap() {
    return Err(amplification_error("system_control_body_cap", body.len(), kind.body_cap()));
  }
  let total = checked_add(checked_add(HEADER_LENGTH, body.len(), "control body")?, CRC_LENGTH, "control CRC")?;
  let total_u32 = u32::try_from(total).map_err(|_| overflow_error("control total length"))?;
  let body_length = u32::try_from(body.len()).map_err(|_| overflow_error("control body length"))?;
  let mut bytes = vec![0u8; total];
  bytes[..4].copy_from_slice(kind.magic());
  bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
  bytes[6..8].copy_from_slice(&(HEADER_LENGTH as u16).to_le_bytes());
  bytes[8..12].copy_from_slice(&total_u32.to_le_bytes());
  bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
  bytes[24..28].copy_from_slice(&body_length.to_le_bytes());
  bytes[HEADER_LENGTH..HEADER_LENGTH + body.len()].copy_from_slice(body);
  let crc_offset = bytes.len() - CRC_LENGTH;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  let decoded = decode_system_control(&bytes, algorithm)?;
  if decoded.kind != kind || decoded.sequence != sequence {
    return Err(identity_error("system_control_encode_roundtrip", "encoded control did not round-trip its kind and sequence"));
  }
  Ok(bytes)
}

fn require_hash_width(bytes: &[u8], expected: usize, context: &'static str) -> FormatResult<()> {
  if bytes.len() != expected {
    return Err(overflow_error(format!("{context} has width {}, expected {expected}", bytes.len())));
  }
  Ok(())
}

pub fn select_system_control_pair<'a>(algorithm: HashAlgorithm, a: &'a [u8], b: &'a [u8]) -> FormatResult<SystemControlSelectionV1<'a>> {
  let a_control = decode_system_control(a, algorithm);
  let b_control = decode_system_control(b, algorithm);
  match (a_control, b_control) {
    (Ok(a), Ok(b)) => select_valid_control_pair(a, b),
    (Ok(control), Err(_)) => {
      ensure_mutable(&control)?;
      Ok(SystemControlSelectionV1 { selected_slot: SystemControlSlotV1::A, control, redundancy_degraded: true })
    }
    (Err(_), Ok(control)) => {
      ensure_mutable(&control)?;
      Ok(SystemControlSelectionV1 { selected_slot: SystemControlSlotV1::B, control, redundancy_degraded: true })
    }
    (Err(a_error), Err(b_error)) => Err(FormatError::new(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "system_control_no_valid_slot",
      format!("A slot: {a_error}; B slot: {b_error}"),
    )),
  }
}

fn select_valid_control_pair<'a>(a: SystemControlV1<'a>, b: SystemControlV1<'a>) -> FormatResult<SystemControlSelectionV1<'a>> {
  ensure_mutable(&a)?;
  ensure_mutable(&b)?;
  if a.kind != b.kind || a.database_id != b.database_id || a.identity != b.identity {
    return Err(identity_error("system_control_pair_identity", "A/B controls do not repeat one identity"));
  }
  match a.sequence.cmp(&b.sequence) {
    Ordering::Greater => Ok(SystemControlSelectionV1 { selected_slot: SystemControlSlotV1::A, control: a, redundancy_degraded: false }),
    Ordering::Less => Ok(SystemControlSelectionV1 { selected_slot: SystemControlSlotV1::B, control: b, redundancy_degraded: false }),
    Ordering::Equal if a.body == b.body => {
      Ok(SystemControlSelectionV1 { selected_slot: SystemControlSlotV1::A, control: a, redundancy_degraded: false })
    }
    Ordering::Equal => Err(ambiguous_error("system_control_equal_sequence", "equal control sequences contain different bodies")),
  }
}

fn ensure_mutable(control: &SystemControlV1<'_>) -> FormatResult<()> {
  if control.kind.is_immutable() {
    return Err(identity_error("system_control_pair_immutable", "immutable controls cannot use A/B selection"));
  }
  Ok(())
}

pub fn select_cutover_journal(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<CutoverJournalSelectionV1<'_>> {
  if bytes.len() != JOURNAL_LENGTH {
    return Err(trailing_error("cutover_journal_length", "external cutover journal must be exactly 2048 bytes"));
  }
  let a = decode_cutover_slot(&bytes[..JOURNAL_SLOT_LENGTH], algorithm);
  let b = decode_cutover_slot(&bytes[JOURNAL_SLOT_LENGTH..], algorithm);
  match (a, b) {
    (Ok(a), Ok(b)) => match a.sequence.cmp(&b.sequence) {
      Ordering::Greater => Ok(CutoverJournalSelectionV1 {
        selected_slot: SystemControlSlotV1::A,
        sequence: a.sequence,
        body: a.body,
        redundancy_degraded: false,
      }),
      Ordering::Less => Ok(CutoverJournalSelectionV1 {
        selected_slot: SystemControlSlotV1::B,
        sequence: b.sequence,
        body: b.body,
        redundancy_degraded: false,
      }),
      Ordering::Equal if a.body == b.body => Ok(CutoverJournalSelectionV1 {
        selected_slot: SystemControlSlotV1::A,
        sequence: a.sequence,
        body: a.body,
        redundancy_degraded: false,
      }),
      Ordering::Equal => Err(ambiguous_error("cutover_journal_equal_sequence", "equal journal sequences contain different bodies")),
    },
    (Ok(slot), Err(_)) => Ok(CutoverJournalSelectionV1 {
      selected_slot: SystemControlSlotV1::A,
      sequence: slot.sequence,
      body: slot.body,
      redundancy_degraded: true,
    }),
    (Err(_), Ok(slot)) => Ok(CutoverJournalSelectionV1 {
      selected_slot: SystemControlSlotV1::B,
      sequence: slot.sequence,
      body: slot.body,
      redundancy_degraded: true,
    }),
    (Err(a_error), Err(b_error)) => Err(FormatError::new(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "cutover_journal_no_valid_slot",
      format!("A slot: {a_error}; B slot: {b_error}"),
    )),
  }
}

#[derive(Clone, Copy)]
struct CutoverSlotV1<'a> {
  sequence: u64,
  body: &'a [u8],
}

fn decode_cutover_slot(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<CutoverSlotV1<'_>> {
  if bytes.len() != JOURNAL_SLOT_LENGTH {
    return Err(trailing_error("cutover_journal_slot_length", "cutover journal slot has wrong length"));
  }
  if bytes.get(..4) != Some(b"ACUT") || u16_at(bytes, 4)? != 1 || usize::from(u16_at(bytes, 6)?) != JOURNAL_SLOT_LENGTH {
    return Err(magic_error("cutover_journal_slot_header", "cutover journal slot framing is invalid"));
  }
  let sequence = u64_at(bytes, 8)?;
  if sequence == 0 {
    return Err(identity_error("cutover_journal_slot_sequence", "cutover journal slot sequence must be nonzero"));
  }
  if u32_at(bytes, 20)? != 0 || bytes[24..32].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("cutover_journal_slot_reserved", "cutover journal slot reserve must be zero"));
  }
  if u32_at(bytes, JOURNAL_SLOT_CRC_OFFSET)? != crc32fast::hash(&bytes[..JOURNAL_SLOT_CRC_OFFSET]) {
    return Err(integrity_error("cutover_journal_slot_crc", "cutover journal slot CRC does not match"));
  }
  let body_length = usize::try_from(u32_at(bytes, 16)?).map_err(|_| overflow_error("cutover journal body length"))?;
  let body_end = checked_add(32, body_length, "cutover journal body")?;
  if body_end > JOURNAL_SLOT_CRC_OFFSET {
    return Err(amplification_error("cutover_journal_body_cap", body_end, JOURNAL_SLOT_CRC_OFFSET));
  }
  if bytes[body_end..JOURNAL_SLOT_CRC_OFFSET].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("cutover_journal_slot_padding", "cutover journal slot padding must be zero"));
  }
  let body = &bytes[32..body_end];
  validate_cutover(body, algorithm)?;
  Ok(CutoverSlotV1 { sequence, body })
}

pub fn system_control_path(kind: SystemControlKindV1, identity: &[u8], slot: SystemControlSlotV1) -> FormatResult<String> {
  if identity.len() > SYSTEM_CONTROL_IDENTITY_LENGTH_CAP {
    return Err(amplification_error("system_control_identity_length", identity.len(), SYSTEM_CONTROL_IDENTITY_LENGTH_CAP));
  }
  if kind.is_immutable() != (slot == SystemControlSlotV1::Immutable) {
    return Err(identity_error("system_control_slot_kind", "slot does not match control mutability"));
  }
  Ok(control_path(kind, identity, slot))
}

fn control_path(kind: SystemControlKindV1, identity: &[u8], slot: SystemControlSlotV1) -> String {
  let kind_bytes = (kind as u16).to_le_bytes();
  let identity_length = u16::try_from(identity.len()).expect("validated control identity fits u16").to_le_bytes();
  let digest = digest_parts(HashAlgorithm::Blake3_256, &[b"aeordb.system-control-identity.v1\0", &kind_bytes, &identity_length, identity]);
  format!("{CONTROL_ROOT}/{:04x}/{}/{}", kind as u16, hex::encode(digest), slot.file_name())
}

fn validate_body(kind: SystemControlKindV1, body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  if body.len() < 16 {
    return Err(trailing_error("system_control_database_id", "control body omits the database ID"));
  }
  if all_zero(&body[..16]) {
    return Err(identity_error("system_control_database_id", "database ID must be nonzero"));
  }
  match kind {
    SystemControlKindV1::IndexRegistry => validate_index_registry(body, algorithm),
    SystemControlKindV1::IndexOperation => validate_index_operation(body, algorithm),
    SystemControlKindV1::IndexDegraded => validate_index_degraded(body, algorithm),
    SystemControlKindV1::LifecycleLastKnownGood => validate_lkg(body, algorithm, 1),
    SystemControlKindV1::LifecycleDiagnostics => validate_diagnostics(body, algorithm, 1),
    SystemControlKindV1::RuntimeLastKnownGood => validate_lkg(body, algorithm, 2),
    SystemControlKindV1::RuntimeDiagnostics => validate_diagnostics(body, algorithm, 2),
    SystemControlKindV1::RepairTicket => validate_repair_ticket(body, algorithm),
    SystemControlKindV1::PathWriteLatch => validate_path_latch(body, algorithm),
    SystemControlKindV1::MigrationLease => validate_migration_lease(body),
    SystemControlKindV1::MigrationProgress => validate_migration_progress(body, algorithm),
    SystemControlKindV1::LegacyRootMapControl => validate_root_map_control(body, algorithm),
    SystemControlKindV1::LegacyRootMapPage => validate_root_map_page(body, algorithm),
    SystemControlKindV1::TaskPin => validate_task_pin(body, algorithm),
    SystemControlKindV1::SemanticMutationSegment => validate_mutation_segment(body, algorithm),
    SystemControlKindV1::RootPublicationPrepare => validate_root_prepare(body, algorithm),
    SystemControlKindV1::RootAdmissionCommit => validate_root_commit(body, algorithm),
    SystemControlKindV1::DurabilityLatch => validate_durability_latch(body, algorithm),
    SystemControlKindV1::EmergencySpillCatalog => validate_spill_catalog(body, algorithm),
    SystemControlKindV1::SideBySideCutover => validate_cutover(body, algorithm),
  }
}

fn validate_index_registry(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = checked_add(32, checked_mul(3, hash_width, "index registry hashes")?, "index registry fixed body")?;
  if body.len() < fixed {
    return Err(trailing_error("index_registry_length", "index registry is shorter than its fixed body"));
  }
  let generation = u64_at(body, 16)?;
  let count = usize::try_from(u32_at(body, 24 + 2 * hash_width)?).map_err(|_| overflow_error("index registry count"))?;
  let entries_length = usize::try_from(u32_at(body, 28 + 2 * hash_width)?).map_err(|_| overflow_error("index registry entries length"))?;
  let row_length = checked_add(32, checked_mul(4, hash_width, "index registry row hashes")?, "index registry row")?;
  if count > 65_535 {
    return Err(amplification_error("index_registry_count", count, 65_535));
  }
  if entries_length != checked_mul(count, row_length, "index registry entries")? {
    return Err(trailing_error("index_registry_entries_length", "entry count and byte length disagree"));
  }
  if body.len() != checked_add(fixed, entries_length, "index registry body")? {
    return Err(trailing_error("index_registry_length", "index registry entries do not close body"));
  }
  if generation == 0 || zero_hash_at(body, 24, hash_width)? || zero_hash_at(body, 24 + hash_width, hash_width)? {
    return Err(identity_error("index_registry_fields", "registry generation or roots are invalid"));
  }
  let mut previous: Option<&[u8]> = None;
  for row in body[fixed..].chunks_exact(row_length) {
    let index_id = &row[..hash_width];
    if all_zero(index_id) || previous.is_some_and(|value| value >= index_id) {
      return Err(order_error("index_registry_order", "index IDs are zero, duplicate, or out of order"));
    }
    previous = Some(index_id);
    if !(1..=4).contains(&row[hash_width]) || !(1..=7).contains(&row[hash_width + 1]) {
      return Err(kind_error("index_registry_entry_kind", "index registry kind or state is unknown"));
    }
    if u16_at(row, hash_width + 2)? & !0x0003 != 0 || u16_at(row, 30 + 4 * hash_width)? != 0 {
      return Err(reserved_error("index_registry_entry_reserved", "index registry flags or reserve contain unknown bits"));
    }
    if u16_at(row, 28 + 4 * hash_width)? > 0x0018 {
      return Err(kind_error("index_registry_reason", "index registry reason is outside the frozen enum"));
    }
    if row[hash_width + 1] == 3 && all_zero(&row[hash_width + 4..hash_width + 4 + hash_width]) {
      return Err(closure_error("index_registry_active_manifest", "active registry row lacks a manifest"));
    }
  }
  Ok(Vec::new())
}

fn validate_index_operation(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let expected = checked_add(88, checked_mul(7, hash_width, "index operation hashes")?, "index operation body")?;
  if body.len() != expected {
    return Err(trailing_error("index_operation_length", "index operation has wrong fixed length"));
  }
  let identity = body[16..32 + hash_width].to_vec();
  if all_zero(&identity[..hash_width]) || all_zero(&identity[hash_width..]) {
    return Err(identity_error("index_operation_identity", "index or operation ID is zero"));
  }
  if !(1..=4).contains(&u16_at(body, 32 + hash_width)?) || !(1..=7).contains(&u16_at(body, 34 + hash_width)?) {
    return Err(kind_error("index_operation_kind", "operation or state is outside the frozen enum"));
  }
  let started_at = i64_at(body, 36 + hash_width)?;
  let updated_at = i64_at(body, 44 + hash_width)?;
  if started_at < 0 || updated_at < started_at {
    return Err(closure_error("index_operation_times", "index operation times are invalid"));
  }
  if zero_hash_at(body, 52 + hash_width, hash_width)? || zero_hash_at(body, 52 + 2 * hash_width, hash_width)? {
    return Err(identity_error("index_operation_roots", "requested manifest or definition hash is zero"));
  }
  if u64_at(body, 52 + 6 * hash_width)? > u64_at(body, 60 + 6 * hash_width)?
    || u64_at(body, 68 + 6 * hash_width)? > u64_at(body, 76 + 6 * hash_width)?
  {
    return Err(closure_error("index_operation_counters", "operation watermarks or counters are inverted"));
  }
  if u16_at(body, 84 + 6 * hash_width)? > 0x0018 || u16_at(body, 86 + 6 * hash_width)? > 5 {
    return Err(kind_error("index_operation_reason", "operation reason or retry class is outside the frozen enum"));
  }
  Ok(identity)
}

fn validate_index_degraded(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  if body.len() != 44 + 4 * hash_width {
    return Err(trailing_error("index_degraded_length", "index degraded control has wrong fixed length"));
  }
  let identity = body[16..16 + hash_width].to_vec();
  if all_zero(&identity) || u64_at(body, 16 + hash_width)? == 0 {
    return Err(identity_error("index_degraded_identity", "index ID or generation is zero"));
  }
  if i64_at(body, 24 + hash_width)? < 0 {
    return Err(closure_error("index_degraded_time", "degraded time is negative"));
  }
  if u16_at(body, 32 + hash_width)? > 0x0018 || !(1..=3).contains(&u16_at(body, 34 + hash_width)?) {
    return Err(kind_error("index_degraded_reason", "degraded reason or fallback is outside the frozen enum"));
  }
  Ok(identity)
}

fn validate_lkg(body: &[u8], algorithm: HashAlgorithm, expected_kind: u16) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 68 + 2 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("config_lkg_length", "last-known-good control is shorter than its fixed body"));
  }
  let payload_length = usize::try_from(u32_at(body, 28 + 2 * hash_width)?).map_err(|_| overflow_error("LKG payload length"))?;
  if body.len() != checked_add(fixed, payload_length, "LKG body")? {
    return Err(trailing_error("config_lkg_length", "last-known-good payload does not close body"));
  }
  if u16_at(body, 16)? != expected_kind || u16_at(body, 18)? == 0 {
    return Err(kind_error("config_lkg_kind", "configuration kind or schema is invalid"));
  }
  if i64_at(body, 20)? < 0
    || zero_hash_at(body, 28, hash_width)?
    || zero_hash_at(body, 28 + hash_width, hash_width)?
    || all_zero(&body[36 + 2 * hash_width..68 + 2 * hash_width])
  {
    return Err(closure_error("config_lkg_fields", "last-known-good time, hashes, or policy fingerprint is invalid"));
  }
  if u32_at(body, 32 + 2 * hash_width)? != 0 {
    return Err(reserved_error("config_lkg_reserved", "last-known-good reserve must be zero"));
  }
  validate_canonical_value(&body[fixed..], CanonicalValueBounds::AUDIT_VALUE)?;
  Ok(Vec::new())
}

fn validate_diagnostics(body: &[u8], algorithm: HashAlgorithm, expected_kind: u16) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 68 + 2 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("config_diagnostics_length", "diagnostics control is shorter than its fixed body"));
  }
  let detail_length = usize::try_from(u32_at(body, 64 + 2 * hash_width)?).map_err(|_| overflow_error("diagnostics detail length"))?;
  if body.len() != checked_add(fixed, detail_length, "diagnostics body")? {
    return Err(trailing_error("config_diagnostics_length", "diagnostics detail does not close body"));
  }
  if u16_at(body, 16)? != expected_kind || !(1..=5).contains(&u16_at(body, 18)?) {
    return Err(kind_error("config_diagnostics_kind", "configuration kind or diagnostics state is invalid"));
  }
  if i64_at(body, 20)? < 0 || all_zero(&body[28 + 2 * hash_width..60 + 2 * hash_width]) {
    return Err(closure_error("config_diagnostics_fields", "diagnostics time or policy fingerprint is invalid"));
  }
  validate_canonical_value(&body[fixed..], CanonicalValueBounds::AUDIT_VALUE)?;
  Ok(Vec::new())
}

fn validate_repair_ticket(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let physical_length = 24 + 2 * hash_width;
  let fixed = 104 + 4 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("repair_ticket_length", "repair ticket is shorter than its fixed body"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) {
    return Err(identity_error("repair_ticket_identity", "repair ticket ID is zero"));
  }
  let created_at = i64_at(body, 32)?;
  let updated_at = i64_at(body, 40)?;
  if created_at < 0 || updated_at < created_at {
    return Err(closure_error("repair_ticket_times", "repair ticket times are invalid"));
  }
  if !(1..=4).contains(&u16_at(body, 48)?) || !(1..=13).contains(&u16_at(body, 50)?) || u16_at(body, 52)? == 0 {
    return Err(kind_error("repair_ticket_kind", "repair state, operation class, or family is invalid"));
  }
  let flags = u16_at(body, 54)?;
  if flags & !0x0007 != 0 {
    return Err(reserved_error("repair_ticket_flags", "repair ticket contains unknown flag bits"));
  }
  let root = &body[56..56 + hash_width];
  let path = &body[56 + hash_width..56 + 2 * hash_width];
  let incarnation_present = body[56 + 2 * hash_width];
  if incarnation_present > 1 {
    return Err(boolean_error("repair_ticket_incarnation_presence", "physical-incarnation presence is noncanonical"));
  }
  if presence_u16(flags, 0) == all_zero(root)
    || presence_u16(flags, 1) == all_zero(path)
    || presence_u16(flags, 2) != (incarnation_present == 1)
  {
    return Err(boolean_error("repair_ticket_presence", "repair ticket flags disagree with optional fields"));
  }
  if body[57 + 2 * hash_width..64 + 2 * hash_width].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("repair_ticket_padding", "repair ticket presence padding must be zero"));
  }
  let incarnation = &body[64 + 2 * hash_width..64 + 2 * hash_width + physical_length];
  if incarnation_present == 1 {
    decode_physical_incarnation(incarnation, algorithm)?;
  } else if !all_zero(incarnation) {
    return Err(boolean_error("repair_ticket_incarnation_absent", "absent physical incarnation must be zero"));
  }
  let vector_offset = 64 + 2 * hash_width + physical_length;
  let count = usize::from(u16_at(body, vector_offset)?);
  if count > 64 {
    return Err(amplification_error("repair_ticket_evidence_count", count, 64));
  }
  if u16_at(body, vector_offset + 2)? != 0 || u32_at(body, vector_offset + 12)? != 0 {
    return Err(reserved_error("repair_ticket_reserved", "repair ticket vector reserve must be zero"));
  }
  let evidence_length = usize::try_from(u32_at(body, vector_offset + 4)?).map_err(|_| overflow_error("repair evidence length"))?;
  let diagnostic_length = usize::try_from(u32_at(body, vector_offset + 8)?).map_err(|_| overflow_error("repair diagnostic length"))?;
  if evidence_length != checked_mul(count, hash_width, "repair evidence hashes")? {
    return Err(trailing_error("repair_ticket_evidence_length", "repair evidence count and length disagree"));
  }
  let evidence_end = checked_add(fixed, evidence_length, "repair evidence")?;
  if checked_add(evidence_end, diagnostic_length, "repair diagnostic")? != body.len() {
    return Err(trailing_error("repair_ticket_length", "repair ticket variable fields do not close body"));
  }
  validate_sorted_values(&body[fixed..evidence_end], hash_width, "repair_ticket_evidence_order")?;
  validate_canonical_value(&body[evidence_end..], CanonicalValueBounds::AUDIT_VALUE)?;
  Ok(identity)
}

fn validate_path_latch(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 32 + hash_width;
  if body.len() < fixed {
    return Err(trailing_error("path_latch_length", "path latch is shorter than its fixed body"));
  }
  let identity = body[16..16 + hash_width].to_vec();
  if all_zero(&identity) {
    return Err(identity_error("path_latch_identity", "path digest is zero"));
  }
  if i64_at(body, 16 + hash_width)? < 0 {
    return Err(closure_error("path_latch_time", "path latch time is negative"));
  }
  let count = usize::from(u16_at(body, 24 + hash_width)?);
  if !(1..=64).contains(&count) {
    return Err(amplification_error("path_latch_ticket_count", count, 64));
  }
  if !(1..=2).contains(&u16_at(body, 26 + hash_width)?) {
    return Err(kind_error("path_latch_state", "path latch state is outside the frozen enum"));
  }
  if u32_at(body, 28 + hash_width)? != 0 {
    return Err(reserved_error("path_latch_reserved", "path latch reserve must be zero"));
  }
  if body.len() != checked_add(fixed, checked_mul(count, 16, "path latch tickets")?, "path latch body")? {
    return Err(trailing_error("path_latch_length", "path latch ticket vector does not close body"));
  }
  validate_sorted_values(&body[fixed..], 16, "path_latch_ticket_order")?;
  Ok(identity)
}

fn validate_migration_lease(body: &[u8]) -> FormatResult<Vec<u8>> {
  if body.len() != 132 {
    return Err(trailing_error("migration_lease_length", "migration lease has wrong fixed length"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) || body[32..80].chunks_exact(16).any(all_zero) {
    return Err(identity_error("migration_lease_identity", "migration lease IDs are zero"));
  }
  let acquired_at = i64_at(body, 88)?;
  let renewed_at = i64_at(body, 96)?;
  let expires_at = i64_at(body, 104)?;
  if u64_at(body, 80)? == 0 || u64_at(body, 112)? == 0 {
    return Err(identity_error("migration_lease_fencing", "migration fencing or generation is zero"));
  }
  if acquired_at < 0 || renewed_at < acquired_at || expires_at <= renewed_at {
    return Err(closure_error("migration_lease_times", "migration lease times are invalid"));
  }
  if !(1..=4).contains(&u16_at(body, 120)?) || u16_at(body, 122)? != 3 || u16_at(body, 124)? != 4 {
    return Err(kind_error("migration_lease_formats", "migration state or source/target format is invalid"));
  }
  if u16_at(body, 126)? != 0 || u32_at(body, 128)? != 0 {
    return Err(reserved_error("migration_lease_reserved", "migration lease reserve must be zero"));
  }
  Ok(identity)
}

fn validate_migration_progress(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let expected = 156 + 6 * hash_width;
  if expected > FOUR_KIB {
    return Err(amplification_error("migration_progress_cap", expected, FOUR_KIB));
  }
  if body.len() != expected {
    return Err(trailing_error("migration_progress_length", "migration progress has wrong fixed length"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) || body[32..64].chunks_exact(16).any(all_zero) {
    return Err(identity_error("migration_progress_identity", "migration progress IDs are zero"));
  }
  if u64_at(body, 64)? == 0 {
    return Err(identity_error("migration_progress_fencing", "migration fencing token is zero"));
  }
  if u16_at(body, 72)? != 3 || u16_at(body, 74)? != 4 || !(1..=8).contains(&u16_at(body, 76)?) || !(1..=6).contains(&u16_at(body, 78)?) {
    return Err(kind_error("migration_progress_state", "migration formats, phase, or state are invalid"));
  }
  if u32_at(body, 80)? & !0x0007 != 0 {
    return Err(reserved_error("migration_progress_flags", "migration progress contains unknown flag bits"));
  }
  if i64_at(body, 148)? < 0 {
    return Err(closure_error("migration_progress_time", "migration progress update time is negative"));
  }
  if zero_hash_at(body, 156 + 3 * hash_width, hash_width)? || zero_hash_at(body, 156 + 4 * hash_width, hash_width)? {
    return Err(identity_error("migration_progress_policy", "migration configuration or SystemFamily hash is zero"));
  }
  Ok(identity)
}

fn validate_root_map_control(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  if body.len() != 104 + 3 * hash_width {
    return Err(trailing_error("legacy_root_map_control_length", "legacy root-map control has wrong fixed length"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) || body[32..80].chunks_exact(16).any(all_zero) {
    return Err(identity_error("legacy_root_map_control_identity", "legacy root-map IDs are zero"));
  }
  if u16_at(body, 80)? != 3 || u16_at(body, 82)? != 4 {
    return Err(kind_error("legacy_root_map_control_formats", "legacy root-map formats are invalid"));
  }
  if u32_at(body, 84)? != 0 {
    return Err(reserved_error("legacy_root_map_control_reserved", "legacy root-map reserve must be zero"));
  }
  if u64_at(body, 88)? == 0 {
    return Err(identity_error("legacy_root_map_control_generation", "legacy root-map generation is zero"));
  }
  let page_count = u32_at(body, 96)?;
  let record_count = u32_at(body, 100)?;
  let populated = page_count != 0;
  if populated != (record_count != 0)
    || populated != !all_zero(&body[104..104 + hash_width])
    || populated != !all_zero(&body[104 + hash_width..104 + 2 * hash_width])
    || (record_count > 0 && all_zero(&body[104 + 2 * hash_width..104 + 3 * hash_width]))
  {
    return Err(closure_error("legacy_root_map_control_fields", "legacy root-map counts, roots, or digest disagree"));
  }
  Ok(identity)
}

fn validate_root_map_page(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 96 + 2 * hash_width;
  let row_length = 12 + 2 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("legacy_root_map_page_length", "legacy root-map page is shorter than its fixed body"));
  }
  if body[16..80].chunks_exact(16).any(all_zero) {
    return Err(identity_error("legacy_root_map_page_identity", "legacy root-map page IDs are zero"));
  }
  let ordinal = u64_at(body, 80)?;
  let count = usize::try_from(u32_at(body, 88 + 2 * hash_width)?).map_err(|_| overflow_error("root-map row count"))?;
  let rows_length = usize::try_from(u32_at(body, 92 + 2 * hash_width)?).map_err(|_| overflow_error("root-map rows length"))?;
  let maximum_count = body.len().saturating_sub(fixed) / row_length;
  if count > maximum_count {
    return Err(amplification_error("legacy_root_map_page_count", count, maximum_count));
  }
  if rows_length != checked_mul(count, row_length, "root-map rows")? || body.len() != checked_add(fixed, rows_length, "root-map body")? {
    return Err(trailing_error("legacy_root_map_page_length", "root-map row count and lengths do not close body"));
  }
  let mut previous: Option<&[u8]> = None;
  for row in body[fixed..].chunks_exact(row_length) {
    let legacy = &row[..hash_width];
    if all_zero(legacy) || all_zero(&row[hash_width..2 * hash_width]) || previous.is_some_and(|value| value >= legacy) {
      return Err(order_error("legacy_root_map_page_order", "legacy root-map rows are zero, duplicate, or out of order"));
    }
    previous = Some(legacy);
    if !(1..=2).contains(&u16_at(row, 2 * hash_width)?) || u16_at(row, 2 * hash_width + 2)? > 0x0018 {
      return Err(kind_error("legacy_root_map_page_row_kind", "root-map availability or reason is invalid"));
    }
    if u64_at(row, 2 * hash_width + 4)? == 0 {
      return Err(identity_error("legacy_root_map_page_row_sequence", "root-map source write sequence is zero"));
    }
  }
  let mut identity = body[16..32].to_vec();
  identity.extend_from_slice(&ordinal.to_le_bytes());
  Ok(identity)
}

fn validate_task_pin(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  if body.len() < 76 {
    return Err(trailing_error("task_pin_length", "task pin is shorter than its fixed body"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) {
    return Err(identity_error("task_pin_identity", "task ID is zero"));
  }
  if !(1..=11).contains(&u16_at(body, 32)?) || !(1..=3).contains(&u16_at(body, 34)?) {
    return Err(kind_error("task_pin_kind", "task kind or state is outside the frozen enum"));
  }
  let created_at = i64_at(body, 36)?;
  let renewed_at = i64_at(body, 44)?;
  let expires_at = i64_at(body, 52)?;
  if created_at < 0 || renewed_at < created_at || expires_at < 0 || (expires_at != 0 && expires_at <= renewed_at) {
    return Err(closure_error("task_pin_times", "task pin times are invalid"));
  }
  if u64_at(body, 60)? == 0 {
    return Err(identity_error("task_pin_fencing", "task pin fencing token is zero"));
  }
  let root_count = usize::try_from(u32_at(body, 68)?).map_err(|_| overflow_error("task pin root count"))?;
  let artifact_count = usize::try_from(u32_at(body, 72)?).map_err(|_| overflow_error("task pin artifact count"))?;
  if root_count > 4_096 || artifact_count > 4_096 {
    return Err(amplification_error("task_pin_count", root_count.max(artifact_count), 4_096));
  }
  let row_count = checked_add(root_count, artifact_count, "task pin count")?;
  if body.len() != checked_add(76, checked_mul(row_count, hash_width, "task pin hashes")?, "task pin body")? {
    return Err(trailing_error("task_pin_length", "task pin hash vectors do not close body"));
  }
  let roots_end = 76 + root_count * hash_width;
  validate_sorted_values(&body[76..roots_end], hash_width, "task_pin_root_order")?;
  validate_sorted_values(&body[roots_end..], hash_width, "task_pin_artifact_order")?;
  Ok(identity)
}

fn validate_mutation_segment(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 48 + hash_width;
  let row_length = 32 + 7 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("semantic_mutation_segment_length", "mutation segment is shorter than its fixed body"));
  }
  let ordinal = u64_at(body, 16)?;
  let first = u64_at(body, 24)?;
  let last = u64_at(body, 32)?;
  let count = usize::try_from(u32_at(body, 40)?).map_err(|_| overflow_error("mutation record count"))?;
  let records_length = usize::try_from(u32_at(body, 44)?).map_err(|_| overflow_error("mutation records length"))?;
  let maximum_count = body.len().saturating_sub(fixed) / row_length;
  if count > maximum_count {
    return Err(amplification_error("semantic_mutation_segment_count", count, maximum_count));
  }
  if records_length != checked_mul(count, row_length, "mutation records")?
    || body.len() != checked_add(fixed, records_length, "mutation segment body")?
  {
    return Err(trailing_error("semantic_mutation_segment_length", "mutation record count and lengths do not close body"));
  }
  if first == 0 || last < first {
    return Err(closure_error("semantic_mutation_segment_range", "mutation sequence range is invalid"));
  }
  let mut previous: Option<(u64, &[u8])> = None;
  for row in body[fixed..].chunks_exact(row_length) {
    let sequence = u64_at(row, 0)?;
    let mutation_id = &row[8..8 + hash_width];
    if sequence < first
      || sequence > last
      || all_zero(mutation_id)
      || previous.is_some_and(|prior| prior.0 > sequence || (prior.0 == sequence && prior.1 >= mutation_id))
    {
      return Err(order_error("semantic_mutation_segment_order", "mutation rows are outside range, duplicate, or out of order"));
    }
    if u16_at(row, 8 + hash_width)? == 0 || !(1..=10).contains(&u16_at(row, 10 + hash_width)?) {
      return Err(kind_error("semantic_mutation_segment_kind", "mutation family or operation is invalid"));
    }
    if u32_at(row, 12 + hash_width)? != 0 {
      return Err(reserved_error("semantic_mutation_segment_flags", "mutation flags contain unknown bits"));
    }
    if all_zero(&row[row_length - 16..]) {
      return Err(identity_error("semantic_mutation_segment_operation_id", "mutation operation ID is zero"));
    }
    previous = Some((sequence, mutation_id));
  }
  if count > 0 {
    let first_row = u64_at(body, fixed)?;
    let last_row = u64_at(body, fixed + (count - 1) * row_length)?;
    if first_row != first || last_row != last {
      return Err(closure_error("semantic_mutation_segment_coverage", "first or last mutation row does not close sequence range"));
    }
  }
  Ok(ordinal.to_le_bytes().to_vec())
}

fn validate_root_prepare(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 64 + 5 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("root_prepare_length", "root publication prepare is shorter than its fixed body"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) {
    return Err(identity_error("root_prepare_identity", "root publication operation ID is zero"));
  }
  let authority_length = usize::from(u16_at(body, 44 + 3 * hash_width)?);
  if authority_length == 0 || authority_length > SYSTEM_CONTROL_IDENTITY_LENGTH_CAP {
    return Err(amplification_error("root_prepare_authority_length", authority_length, SYSTEM_CONTROL_IDENTITY_LENGTH_CAP));
  }
  if body.len() != checked_add(fixed, authority_length, "root prepare authority")? {
    return Err(trailing_error("root_prepare_length", "authority identity does not close prepare body"));
  }
  if i64_at(body, 32)? < 0
    || zero_hash_at(body, 40, hash_width)?
    || zero_hash_at(body, 40 + hash_width, hash_width)?
    || zero_hash_at(body, 40 + 2 * hash_width, hash_width)?
    || zero_hash_at(body, 48 + 4 * hash_width, hash_width)?
  {
    return Err(identity_error("root_prepare_hashes", "prepare time or required root/mutation hash is invalid"));
  }
  if !(1..=5).contains(&u16_at(body, 40 + 3 * hash_width)?) || u16_at(body, 42 + 3 * hash_width)? != 1 {
    return Err(kind_error("root_prepare_kind", "root kind or schema is invalid"));
  }
  if u16_at(body, 46 + 3 * hash_width)? != 0 {
    return Err(reserved_error("root_prepare_reserved", "root prepare reserve must be zero"));
  }
  if u64_at(body, 48 + 5 * hash_width)? == 0 || u64_at(body, 56 + 5 * hash_width)? == 0 {
    return Err(identity_error("root_prepare_sequences", "publication or header sequence is zero"));
  }
  Ok(identity)
}

fn validate_root_commit(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  if body.len() != 64 + 4 * hash_width {
    return Err(trailing_error("root_commit_length", "root admission commit has wrong fixed length"));
  }
  let identity = body[16..16 + hash_width].to_vec();
  if all_zero(&identity)
    || all_zero(&body[16 + hash_width..32 + hash_width])
    || zero_hash_at(body, 48 + hash_width, hash_width)?
    || zero_hash_at(body, 48 + 2 * hash_width, hash_width)?
    || zero_hash_at(body, 64 + 3 * hash_width, hash_width)?
  {
    return Err(identity_error("root_commit_identity", "commit root, operation ID, or authority hashes are zero"));
  }
  if i64_at(body, 32 + hash_width)? < 0 {
    return Err(closure_error("root_commit_time", "root admission time is negative"));
  }
  if !(1..=5).contains(&u16_at(body, 40 + hash_width)?) || u16_at(body, 42 + hash_width)? != 1 {
    return Err(kind_error("root_commit_kind", "root kind or schema is invalid"));
  }
  if u32_at(body, 44 + hash_width)? & !0x0001 != 0 {
    return Err(reserved_error("root_commit_flags", "root commit contains unknown flag bits"));
  }
  if u64_at(body, 48 + 3 * hash_width)? == 0 || u64_at(body, 56 + 3 * hash_width)? == 0 {
    return Err(identity_error("root_commit_sequences", "publication or header sequence is zero"));
  }
  Ok(identity)
}

fn validate_durability_latch(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 88 + 2 * hash_width;
  if body.len() < fixed {
    return Err(trailing_error("durability_latch_length", "durability latch is shorter than its fixed body"));
  }
  let diagnostic_length = usize::try_from(u32_at(body, 80 + 2 * hash_width)?).map_err(|_| overflow_error("latch diagnostic length"))?;
  if body.len() != checked_add(fixed, diagnostic_length, "durability latch body")? {
    return Err(trailing_error("durability_latch_length", "durability diagnostic does not close body"));
  }
  let flags = u32_at(body, 52)?;
  let spill = &body[80..80 + hash_width];
  if u64_at(body, 16)? == 0 || u64_at(body, 56)? == 0 || u64_at(body, 64)? == 0 || u64_at(body, 72)? == 0 {
    return Err(identity_error("durability_latch_sequences", "latch generation or write sequences are zero"));
  }
  let first_failed_at = i64_at(body, 24)?;
  let latest_failed_at = i64_at(body, 32)?;
  if first_failed_at < 0 || latest_failed_at < first_failed_at {
    return Err(closure_error("durability_latch_times", "durability failure times are invalid"));
  }
  if u16_at(body, 40)? != 1
    || !(1..=3).contains(&u16_at(body, 42)?)
    || !(1..=15).contains(&u16_at(body, 44)?)
    || !(1..=13).contains(&u16_at(body, 46)?)
  {
    return Err(kind_error("durability_latch_kind", "latch state, retry, failure, or operation class is invalid"));
  }
  if i32_at(body, 48)? == 0 {
    return Err(closure_error("durability_latch_os_code", "durability latch OS code is zero"));
  }
  if flags & !0x0001 != 0 {
    return Err(reserved_error("durability_latch_flags", "durability latch contains unknown flag bits"));
  }
  if presence_u32_bit(flags, 0) == all_zero(spill) {
    return Err(boolean_error("durability_latch_spill_presence", "spill presence flag disagrees with hash"));
  }
  if all_zero(&body[80 + hash_width..80 + 2 * hash_width]) {
    return Err(identity_error("durability_latch_evidence", "durability evidence hash is zero"));
  }
  if u32_at(body, 84 + 2 * hash_width)? != 0 {
    return Err(reserved_error("durability_latch_reserved", "durability latch reserve must be zero"));
  }
  validate_canonical_value(&body[fixed..], CanonicalValueBounds::AUDIT_VALUE)?;
  Ok(Vec::new())
}

fn validate_spill_catalog(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  let fixed = 44 + hash_width;
  if body.len() < fixed {
    return Err(trailing_error("spill_catalog_length", "spill catalog is shorter than its fixed body"));
  }
  if u64_at(body, 16)? == 0 {
    return Err(identity_error("spill_catalog_generation", "spill catalog generation is zero"));
  }
  if i64_at(body, 24)? < 0 {
    return Err(closure_error("spill_catalog_time", "spill catalog update time is negative"));
  }
  let state = u16_at(body, 32)?;
  if !(1..=4).contains(&state) {
    return Err(kind_error("spill_catalog_state", "spill catalog state is outside the frozen enum"));
  }
  if u16_at(body, 34)? != 0 {
    return Err(reserved_error("spill_catalog_reserved", "spill catalog reserve must be zero"));
  }
  let count = usize::try_from(u32_at(body, 36)?).map_err(|_| overflow_error("spill row count"))?;
  let rows_length = usize::try_from(u32_at(body, 40)?).map_err(|_| overflow_error("spill rows length"))?;
  if body.len() != checked_add(fixed, rows_length, "spill catalog body")? {
    return Err(trailing_error("spill_catalog_length", "spill rows length does not close body"));
  }
  let maximum_count = rows_length / 73;
  if count > maximum_count {
    return Err(amplification_error("spill_catalog_count", count, maximum_count));
  }
  let receipt = &body[44..44 + hash_width];
  if (state == 3) != !all_zero(receipt) {
    return Err(boolean_error("spill_catalog_receipt_presence", "spill receipt presence disagrees with state"));
  }
  let mut cursor = fixed;
  let mut previous: Option<(i64, u64, &[u8], &[u8])> = None;
  for _ in 0..count {
    let row_fixed_end = checked_add(cursor, 72, "spill row fixed body")?;
    if row_fixed_end > body.len() {
      return Err(trailing_error("spill_catalog_row_truncated", "spill row exceeds catalog body"));
    }
    let row = &body[cursor..];
    if !(1..=3).contains(&u16_at(row, 0)?) || !(1..=5).contains(&u16_at(row, 2)?) || !(1..=2).contains(&u16_at(row, 4)?) {
      return Err(kind_error("spill_catalog_row_kind", "spill row kind, state, or path encoding is invalid"));
    }
    if u16_at(row, 6)? != 0 || u32_at(row, 68)? != 0 {
      return Err(reserved_error("spill_catalog_row_reserved", "spill row reserve must be zero"));
    }
    let created_at = i64_at(row, 8)?;
    let sequence = u64_at(row, 16)?;
    let digest = &row[32..64];
    let path_length = usize::try_from(u32_at(row, 64)?).map_err(|_| overflow_error("spill path length"))?;
    let row_end = checked_add(row_fixed_end, path_length, "spill row path")?;
    if row_end > body.len() {
      return Err(trailing_error("spill_catalog_row_length", "spill row path exceeds catalog body"));
    }
    if created_at < 0 || sequence == 0 || u64_at(row, 24)? == 0 || all_zero(digest) || path_length == 0 {
      return Err(identity_error("spill_catalog_row_fields", "spill row time, sequence, length, digest, or path is invalid"));
    }
    let path = &body[row_fixed_end..row_end];
    match u16_at(row, 4)? {
      1 => {
        if path.is_empty() || path.contains(&0) {
          return Err(path_error("spill_catalog_unix_path", "Unix spill path is empty or contains NUL"));
        }
      }
      2 => {
        if !path.len().is_multiple_of(2) {
          return Err(path_error("spill_catalog_windows_path", "Windows spill path has odd UTF-16 byte length"));
        }
        if path.chunks_exact(2).any(|word| word == [0, 0]) {
          return Err(path_error("spill_catalog_windows_path", "Windows spill path contains NUL"));
        }
      }
      _ => {}
    }
    let key = (created_at, sequence, digest, path);
    if previous.is_some_and(|prior| prior >= key) {
      return Err(order_error("spill_catalog_order", "spill rows are duplicate or out of order"));
    }
    previous = Some(key);
    cursor = row_end;
  }
  if cursor != body.len() || cursor - fixed != rows_length {
    return Err(trailing_error("spill_catalog_row_count", "spill row count does not consume rows body"));
  }
  Ok(Vec::new())
}

fn validate_cutover(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  if body.len() != 140 + 5 * hash_width {
    return Err(trailing_error("cutover_length", "side-by-side cutover control has wrong fixed length"));
  }
  let identity = body[16..32].to_vec();
  if all_zero(&identity) || body[32..80].chunks_exact(16).any(all_zero) {
    return Err(identity_error("cutover_identity", "cutover IDs are zero"));
  }
  if u64_at(body, 80)? == 0 || u64_at(body, 100)? == 0 || u64_at(body, 108)? == 0 || u64_at(body, 116)? == 0 || u64_at(body, 124)? == 0 {
    return Err(identity_error("cutover_sequences", "cutover fencing, journal, header, or file sequence is zero"));
  }
  if !(1..=8).contains(&u16_at(body, 88)?) || u16_at(body, 92)? != 3 || u16_at(body, 94)? != 4 {
    return Err(kind_error("cutover_state", "cutover phase or source/target format is invalid"));
  }
  if u16_at(body, 90)? != 0 || u32_at(body, 96)? != 0 {
    return Err(reserved_error("cutover_reserved", "cutover reserve must be zero"));
  }
  if i64_at(body, 132)? < 0 {
    return Err(closure_error("cutover_time", "cutover update time is negative"));
  }
  if body[140..140 + 4 * hash_width].chunks_exact(hash_width).any(all_zero) {
    return Err(identity_error("cutover_hashes", "cutover requires four nonzero state hashes"));
  }
  Ok(identity)
}

fn validate_sorted_values(bytes: &[u8], width: usize, code: &'static str) -> FormatResult<()> {
  if width == 0 || !bytes.len().is_multiple_of(width) {
    return Err(trailing_error(code, "sorted value vector has wrong element width"));
  }
  let mut previous = None;
  for value in bytes.chunks_exact(width) {
    if all_zero(value) || previous.is_some_and(|prior| prior >= value) {
      return Err(order_error(code, "values are zero, duplicate, or out of order"));
    }
    previous = Some(value);
  }
  Ok(())
}

fn verify_crc(bytes: &[u8], code: &'static str) -> FormatResult<()> {
  let crc_offset = bytes.len().checked_sub(CRC_LENGTH).ok_or_else(|| trailing_error(code, "CRC offset underflows"))?;
  if u32_at(bytes, crc_offset)? != crc32fast::hash(&bytes[..crc_offset]) {
    return Err(integrity_error(code, "CRC-32/ISO-HDLC does not match"));
  }
  Ok(())
}

fn zero_hash_at(bytes: &[u8], offset: usize, width: usize) -> FormatResult<bool> {
  let end = checked_add(offset, width, "hash field")?;
  let hash = bytes.get(offset..end).ok_or_else(|| trailing_error("system_control_truncated", format!("hash at offset {offset}")))?;
  Ok(all_zero(hash))
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes.get(offset..offset + 2).ok_or_else(|| trailing_error("system_control_truncated", format!("u16 at offset {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked control u16 width")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes.get(offset..offset + 4).ok_or_else(|| trailing_error("system_control_truncated", format!("u32 at offset {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked control u32 width")))
}

fn i32_at(bytes: &[u8], offset: usize) -> FormatResult<i32> {
  let raw = bytes.get(offset..offset + 4).ok_or_else(|| trailing_error("system_control_truncated", format!("i32 at offset {offset}")))?;
  Ok(i32::from_le_bytes(raw.try_into().expect("checked control i32 width")))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let raw = bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("system_control_truncated", format!("u64 at offset {offset}")))?;
  Ok(u64::from_le_bytes(raw.try_into().expect("checked control u64 width")))
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let raw = bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("system_control_truncated", format!("i64 at offset {offset}")))?;
  Ok(i64::from_le_bytes(raw.try_into().expect("checked control i64 width")))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn presence_u16(flags: u16, bit: u8) -> bool {
  flags & (1 << bit) != 0
}

fn presence_u32_bit(flags: u32, bit: u8) -> bool {
  flags & (1 << bit) != 0
}

fn checked_add(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_add(right).ok_or_else(|| overflow_error(context))
}

fn checked_mul(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_mul(right).ok_or_else(|| overflow_error(context))
}

fn amplification_error(code: &'static str, actual: usize, cap: usize) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, code, format!("{actual} exceeds cap {cap}"))
}

fn overflow_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "system_control_overflow", context)
}

fn trailing_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, code, context)
}

fn magic_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::UnknownMagicOrVersion, code, context)
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

fn boolean_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalBooleanOrOptionalPresence, code, context)
}

fn order_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, code, context)
}

fn integrity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::ChecksumOrIntegrityMismatch, code, context)
}

fn ambiguous_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AmbiguousEqualSequenceSelector, code, context)
}

fn path_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
