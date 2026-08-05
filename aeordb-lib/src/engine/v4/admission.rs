use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::sync::OnceLock;

use super::contract_generated::CAPABILITY_BITS;
use super::database_header::{DatabaseHeaderV4, SelectedDatabaseHeaderV4};
use super::system_family::{SystemFamilyRegistryV1, decode_system_family_registry};
use crate::engine::HashAlgorithm;

const CAPABILITY_WIDTH: usize = 32;
const KNOWN_CAPABILITY_COUNT: u16 = 24;
const SYSTEM_FAMILY_REGISTRY_VERSION: u16 = 1;
const SYSTEM_FAMILY_REGISTRY_V1_BYTES: &[u8] = include_bytes!("../../../spec/fixtures/system-family-registry-v1.bin");
static BLAKE3_256_REGISTRY: OnceLock<Result<SystemFamilyRegistryV1<'static>, super::reader::FormatError>> = OnceLock::new();
static SHA256_REGISTRY: OnceLock<Result<SystemFamilyRegistryV1<'static>, super::reader::FormatError>> = OnceLock::new();
static SHA512_REGISTRY: OnceLock<Result<SystemFamilyRegistryV1<'static>, super::reader::FormatError>> = OnceLock::new();
static SHA3_256_REGISTRY: OnceLock<Result<SystemFamilyRegistryV1<'static>, super::reader::FormatError>> = OnceLock::new();
static SHA3_512_REGISTRY: OnceLock<Result<SystemFamilyRegistryV1<'static>, super::reader::FormatError>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilitySetV1([u8; CAPABILITY_WIDTH]);

impl CapabilitySetV1 {
  pub const fn empty() -> Self {
    Self([0; CAPABILITY_WIDTH])
  }

  pub const fn v4_baseline() -> Self {
    let mut bytes = [0u8; CAPABILITY_WIDTH];
    bytes[0] = 0x7f;
    bytes[2] = 0x6c;
    Self(bytes)
  }

  pub fn from_bits(bits: impl IntoIterator<Item = u16>) -> Result<Self, V4AdmissionError> {
    let mut set = Self::empty();
    for bit in bits {
      if bit >= KNOWN_CAPABILITY_COUNT {
        return Err(V4AdmissionError::new("unknown_capability_bit", format!("capability bit {bit} is not assigned")));
      }
      set.0[usize::from(bit / 8)] |= 1 << (bit % 8);
    }
    Ok(set)
  }

  pub fn from_bytes(bytes: [u8; CAPABILITY_WIDTH]) -> Result<Self, V4AdmissionError> {
    if bytes[usize::from(KNOWN_CAPABILITY_COUNT / 8)..].iter().any(|byte| *byte != 0) {
      return Err(V4AdmissionError::new("unknown_capability_bit", format!("capability bit {KNOWN_CAPABILITY_COUNT} or greater is set")));
    }
    Ok(Self(bytes))
  }

  pub const fn into_bytes(self) -> [u8; CAPABILITY_WIDTH] {
    self.0
  }

  pub fn contains(self, bit: u16) -> bool {
    bit < KNOWN_CAPABILITY_COUNT && self.0[usize::from(bit / 8)] & (1 << (bit % 8)) != 0
  }

  pub fn union(self, other: Self) -> Self {
    let mut bytes = [0u8; CAPABILITY_WIDTH];
    for (index, byte) in bytes.iter_mut().enumerate() {
      *byte = self.0[index] | other.0[index];
    }
    Self(bytes)
  }

  pub fn difference(self, supported: Self) -> Self {
    let mut bytes = [0u8; CAPABILITY_WIDTH];
    for (index, byte) in bytes.iter_mut().enumerate() {
      *byte = self.0[index] & !supported.0[index];
    }
    Self(bytes)
  }

  pub fn is_empty(self) -> bool {
    self.0.iter().all(|byte| *byte == 0)
  }

  pub fn bits(self) -> Vec<u16> {
    (0..KNOWN_CAPABILITY_COUNT).filter(|bit| self.contains(*bit)).collect()
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinaryCapabilityProfileV1 {
  pub supported_reader_capabilities: CapabilitySetV1,
  pub supported_writer_capabilities: CapabilitySetV1,
}

impl BinaryCapabilityProfileV1 {
  pub const fn new(supported_reader_capabilities: CapabilitySetV1, supported_writer_capabilities: CapabilitySetV1) -> Self {
    Self { supported_reader_capabilities, supported_writer_capabilities }
  }

  /// P1b can decode every frozen v4 family, but no v4 writer is active yet.
  pub const fn current() -> Self {
    let mut reader = [0u8; CAPABILITY_WIDTH];
    reader[0] = 0xff;
    reader[1] = 0xff;
    reader[2] = 0xff;
    Self::new(CapabilitySetV1(reader), CapabilitySetV1::empty())
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionModeV1 {
  SemanticReadOnly,
  DiagnosticRaw,
  Writable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalIdentityEvidenceV1 {
  ExistingInstance { physical_instance_id: [u8; 16], previous_writer_fence_epoch: u64 },
  AdoptedCopy { source_physical_instance_id: [u8; 16], source_writer_fence_epoch: u64 },
}

#[derive(Clone, Debug)]
pub struct SemanticReadOnlyAdmissionV1 {
  pub selected_slot: usize,
  pub redundancy_degraded: bool,
  pub registry: &'static SystemFamilyRegistryV1<'static>,
}

#[derive(Clone, Debug)]
pub struct DiagnosticRawAdmissionV1 {
  pub issues: Vec<V4AdmissionError>,
}

impl DiagnosticRawAdmissionV1 {
  pub const fn mutation_allowed(&self) -> bool {
    false
  }
}

#[derive(Clone, Debug)]
pub struct WritableAdmissionV1 {
  physical_instance_id: [u8; 16],
  writer_fence_epoch: u64,
  registry: &'static SystemFamilyRegistryV1<'static>,
}

impl WritableAdmissionV1 {
  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub const fn writer_fence_epoch(&self) -> u64 {
    self.writer_fence_epoch
  }

  pub const fn registry(&self) -> &SystemFamilyRegistryV1<'static> {
    self.registry
  }
}

#[derive(Clone, Debug)]
pub enum V4AdmissionResult {
  SemanticReadOnly(SemanticReadOnlyAdmissionV1),
  DiagnosticRaw(DiagnosticRawAdmissionV1),
  Writable(WritableAdmissionV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4AdmissionError {
  code: &'static str,
  message: String,
  capability_bits: Vec<u16>,
}

impl V4AdmissionError {
  fn new(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into(), capability_bits: Vec::new() }
  }

  fn capabilities(code: &'static str, role: &'static str, capabilities: CapabilitySetV1) -> Self {
    let capability_bits = capabilities.bits();
    Self { code, message: format!("missing {role} capabilities {capability_bits:?}"), capability_bits }
  }

  pub fn missing_reader(capabilities: CapabilitySetV1) -> Self {
    Self::capabilities("missing_reader_capabilities", "reader", capabilities)
  }

  fn missing_writer(capabilities: CapabilitySetV1) -> Self {
    Self::capabilities("missing_writer_capabilities", "writer", capabilities)
  }

  fn missing_baseline(code: &'static str, role: &'static str, capabilities: CapabilitySetV1) -> Self {
    Self::capabilities(code, role, capabilities)
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn capability_bits(&self) -> &[u16] {
    &self.capability_bits
  }

  pub fn capability_names(&self) -> Vec<&'static str> {
    self.capability_bits.iter().filter_map(|bit| CAPABILITY_BITS.iter().find(|value| value.id == *bit).map(|value| value.name)).collect()
  }
}

impl Display for V4AdmissionError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for V4AdmissionError {}

/// Evaluate v4 header compatibility without mutating storage. The supplied
/// profile is evidence for this pure check, not an engine write capability;
/// writer activation still requires the P3a writer-owned publication gate.
pub fn admit_v4_header(
  selected: &SelectedDatabaseHeaderV4,
  mode: AdmissionModeV1,
  profile: BinaryCapabilityProfileV1,
  identity: Option<PhysicalIdentityEvidenceV1>,
) -> Result<V4AdmissionResult, V4AdmissionError> {
  let required_readers = CapabilitySetV1::from_bytes(selected.header.required_reader_capabilities)?;
  let required_writers = CapabilitySetV1::from_bytes(selected.header.required_writer_capabilities)?;
  let mut issues = Vec::new();

  collect_baseline_issues(required_readers, required_writers, &mut issues);

  let missing_readers = required_readers.difference(profile.supported_reader_capabilities);
  if !missing_readers.is_empty() {
    issues.push(V4AdmissionError::missing_reader(missing_readers));
  }

  let registry = load_selected_registry(&selected.header).inspect_err(|error| issues.push((*error).clone()));

  if mode == AdmissionModeV1::DiagnosticRaw {
    return Ok(V4AdmissionResult::DiagnosticRaw(DiagnosticRawAdmissionV1 { issues }));
  }
  if let Some(error) = issues.into_iter().next() {
    return Err(error);
  }
  let registry = registry.expect("successful semantic registry admission");

  if mode == AdmissionModeV1::SemanticReadOnly {
    return Ok(V4AdmissionResult::SemanticReadOnly(SemanticReadOnlyAdmissionV1 {
      selected_slot: selected.selected_slot,
      redundancy_degraded: selected.redundancy_degraded,
      registry,
    }));
  }

  let writer_floor = required_readers.union(required_writers);
  let missing_writers = writer_floor.difference(profile.supported_writer_capabilities);
  if !missing_writers.is_empty() {
    return Err(V4AdmissionError::missing_writer(missing_writers));
  }
  validate_physical_identity(&selected.header, identity)?;
  Ok(V4AdmissionResult::Writable(WritableAdmissionV1 {
    physical_instance_id: selected.header.physical_instance_id,
    writer_fence_epoch: selected.header.writer_fence_epoch,
    registry,
  }))
}

fn collect_baseline_issues(readers: CapabilitySetV1, writers: CapabilitySetV1, issues: &mut Vec<V4AdmissionError>) {
  let baseline = CapabilitySetV1::v4_baseline();
  let missing_readers = baseline.difference(readers);
  if !missing_readers.is_empty() {
    issues.push(V4AdmissionError::missing_baseline("missing_baseline_reader_capabilities", "baseline reader", missing_readers));
  }
  let missing_writers = baseline.difference(writers);
  if !missing_writers.is_empty() {
    issues.push(V4AdmissionError::missing_baseline("missing_baseline_writer_capabilities", "baseline writer", missing_writers));
  }
}

fn load_selected_registry(header: &DatabaseHeaderV4) -> Result<&'static SystemFamilyRegistryV1<'static>, V4AdmissionError> {
  if header.system_family_registry_version != SYSTEM_FAMILY_REGISTRY_VERSION {
    return Err(V4AdmissionError::new(
      "unsupported_system_family_registry",
      format!("registry version {} is not embedded", header.system_family_registry_version),
    ));
  }
  let registry = embedded_registry(header.hash_algorithm)?;
  if registry.operational_fingerprint != header.system_family_registry_fingerprint {
    return Err(V4AdmissionError::new(
      "system_family_registry_fingerprint_mismatch",
      "selected header does not name the embedded registry bytes",
    ));
  }
  Ok(registry)
}

fn embedded_registry(algorithm: HashAlgorithm) -> Result<&'static SystemFamilyRegistryV1<'static>, V4AdmissionError> {
  let cell = match algorithm {
    HashAlgorithm::Blake3_256 => &BLAKE3_256_REGISTRY,
    HashAlgorithm::Sha256 => &SHA256_REGISTRY,
    HashAlgorithm::Sha512 => &SHA512_REGISTRY,
    HashAlgorithm::Sha3_256 => &SHA3_256_REGISTRY,
    HashAlgorithm::Sha3_512 => &SHA3_512_REGISTRY,
  };
  cell
    .get_or_init(|| decode_system_family_registry(SYSTEM_FAMILY_REGISTRY_V1_BYTES, algorithm))
    .as_ref()
    .map_err(|error| V4AdmissionError::new("embedded_system_family_registry_invalid", error.to_string()))
}

fn validate_physical_identity(header: &DatabaseHeaderV4, evidence: Option<PhysicalIdentityEvidenceV1>) -> Result<(), V4AdmissionError> {
  match evidence {
    None => Err(V4AdmissionError::new(
      "physical_identity_evidence_required",
      "writable admission requires durable local identity and fence evidence",
    )),
    Some(PhysicalIdentityEvidenceV1::ExistingInstance { physical_instance_id, previous_writer_fence_epoch }) => {
      if header.physical_instance_id != physical_instance_id {
        return Err(V4AdmissionError::new(
          "physical_instance_identity_mismatch",
          "selected header does not belong to the expected physical instance",
        ));
      }
      require_advanced_fence(header.writer_fence_epoch, previous_writer_fence_epoch)
    }
    Some(PhysicalIdentityEvidenceV1::AdoptedCopy { source_physical_instance_id, source_writer_fence_epoch }) => {
      if header.physical_instance_id == source_physical_instance_id {
        return Err(V4AdmissionError::new(
          "clone_physical_identity_not_adopted",
          "copied database still carries the source physical instance identity",
        ));
      }
      require_advanced_fence(header.writer_fence_epoch, source_writer_fence_epoch)
    }
  }
}

fn require_advanced_fence(current: u64, previous: u64) -> Result<(), V4AdmissionError> {
  if current <= previous {
    return Err(V4AdmissionError::new(
      "writer_fence_not_advanced",
      format!("selected writer fence {current} is not greater than prior fence {previous}"),
    ));
  }
  Ok(())
}

/// Capability-bearing fields extracted from an already authenticated peer
/// transcript. This is not the peer wire format and performs no authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCapabilityViewV1 {
  pub database_id: [u8; 16],
  pub database_header_version: u16,
  pub hash_algorithm: HashAlgorithm,
  pub selected_header_sequence: u64,
  pub writer_fence_epoch: u64,
  pub physical_instance_id: [u8; 16],
  pub required_reader_capabilities: CapabilitySetV1,
  pub required_writer_capabilities: CapabilitySetV1,
  pub supported_reader_capabilities: CapabilitySetV1,
  pub supported_writer_capabilities: CapabilitySetV1,
  pub system_family_registry_version: u16,
  pub system_family_registry_fingerprint: Vec<u8>,
}

impl PeerCapabilityViewV1 {
  pub fn from_selected(selected: &SelectedDatabaseHeaderV4, profile: BinaryCapabilityProfileV1) -> Self {
    let header = &selected.header;
    Self {
      database_id: header.database_id,
      database_header_version: 4,
      hash_algorithm: header.hash_algorithm,
      selected_header_sequence: header.slot_sequence,
      writer_fence_epoch: header.writer_fence_epoch,
      physical_instance_id: header.physical_instance_id,
      required_reader_capabilities: CapabilitySetV1::from_bytes(header.required_reader_capabilities)
        .expect("decoded header already validated capability width"),
      required_writer_capabilities: CapabilitySetV1::from_bytes(header.required_writer_capabilities)
        .expect("decoded header already validated capability width"),
      supported_reader_capabilities: profile.supported_reader_capabilities,
      supported_writer_capabilities: profile.supported_writer_capabilities,
      system_family_registry_version: header.system_family_registry_version,
      system_family_registry_fingerprint: header.system_family_registry_fingerprint.clone(),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerCapabilityAdmissionV1 {
  Compatible,
}

/// Validate the v4 capability/registry subset of an authenticated peer
/// transcript. Route policy, semantic closure, codecs, and transcript
/// authentication remain mandatory outer gates.
pub fn admit_peer_capabilities_v4(
  source: &PeerCapabilityViewV1,
  destination: &PeerCapabilityViewV1,
) -> Result<PeerCapabilityAdmissionV1, V4AdmissionError> {
  if source.database_header_version != 4 || destination.database_header_version != 4 {
    return Err(V4AdmissionError::new("peer_header_version_mismatch", "both peers must use DatabaseHeader v4"));
  }
  validate_peer_identity(source)?;
  validate_peer_identity(destination)?;
  validate_peer_baseline(source)?;
  validate_peer_baseline(destination)?;
  if source.database_id != destination.database_id {
    return Err(V4AdmissionError::new("peer_database_identity_mismatch", "peers name different logical databases"));
  }
  if source.physical_instance_id == destination.physical_instance_id {
    return Err(V4AdmissionError::new("peer_physical_identity_collision", "independent peers cannot share one physical instance identity"));
  }
  if source.hash_algorithm != destination.hash_algorithm {
    return Err(V4AdmissionError::new("peer_hash_algorithm_mismatch", "peer hash profiles differ"));
  }
  validate_peer_registry(source)?;
  validate_peer_registry(destination)?;
  if source.system_family_registry_version != destination.system_family_registry_version
    || source.system_family_registry_fingerprint != destination.system_family_registry_fingerprint
  {
    return Err(V4AdmissionError::new("peer_system_family_registry_mismatch", "peers do not select the exact same SystemFamily registry"));
  }
  if !source.required_reader_capabilities.difference(source.supported_reader_capabilities).is_empty() {
    return Err(V4AdmissionError::new("peer_source_reader_capability_mismatch", "source cannot decode its advertised reader floor"));
  }
  if !destination.required_reader_capabilities.difference(destination.supported_reader_capabilities).is_empty() {
    return Err(V4AdmissionError::new(
      "peer_destination_reader_capability_mismatch",
      "destination cannot decode its advertised reader floor",
    ));
  }
  let transfer_floor = source.required_reader_capabilities.union(source.required_writer_capabilities);
  if !source.required_reader_capabilities.difference(destination.required_reader_capabilities).is_empty() {
    return Err(V4AdmissionError::new(
      "peer_destination_stored_reader_floor_mismatch",
      "destination stored reader floor does not permit source-required state",
    ));
  }
  if !transfer_floor.difference(destination.required_writer_capabilities).is_empty() {
    return Err(V4AdmissionError::new(
      "peer_destination_stored_writer_floor_mismatch",
      "destination stored writer floor does not permit source-required state",
    ));
  }
  let destination_writer_floor =
    destination.required_reader_capabilities.union(destination.required_writer_capabilities).union(transfer_floor);
  if !destination_writer_floor.difference(destination.supported_writer_capabilities).is_empty() {
    return Err(V4AdmissionError::new(
      "peer_destination_writer_capability_mismatch",
      "destination binary cannot write its local or source-required state",
    ));
  }
  Ok(PeerCapabilityAdmissionV1::Compatible)
}

fn validate_peer_baseline(peer: &PeerCapabilityViewV1) -> Result<(), V4AdmissionError> {
  let baseline = CapabilitySetV1::v4_baseline();
  let missing_readers = baseline.difference(peer.required_reader_capabilities);
  let missing_writers = baseline.difference(peer.required_writer_capabilities);
  if !missing_readers.is_empty() || !missing_writers.is_empty() {
    return Err(V4AdmissionError::new(
      "peer_capability_floor_invalid",
      format!("peer omits baseline reader bits {:?} or writer bits {:?}", missing_readers.bits(), missing_writers.bits()),
    ));
  }
  Ok(())
}

fn validate_peer_identity(peer: &PeerCapabilityViewV1) -> Result<(), V4AdmissionError> {
  if peer.database_id.iter().all(|byte| *byte == 0)
    || peer.physical_instance_id.iter().all(|byte| *byte == 0)
    || peer.selected_header_sequence == 0
    || peer.writer_fence_epoch == 0
  {
    return Err(V4AdmissionError::new(
      "peer_identity_or_sequence_invalid",
      "peer hello has a zero database/physical identity, header sequence, or writer fence",
    ));
  }
  Ok(())
}

fn validate_peer_registry(peer: &PeerCapabilityViewV1) -> Result<(), V4AdmissionError> {
  if peer.system_family_registry_version != SYSTEM_FAMILY_REGISTRY_VERSION {
    return Err(V4AdmissionError::new(
      "peer_system_family_registry_mismatch",
      format!("peer names unsupported registry version {}", peer.system_family_registry_version),
    ));
  }
  let registry = embedded_registry(peer.hash_algorithm)?;
  if registry.operational_fingerprint != peer.system_family_registry_fingerprint {
    return Err(V4AdmissionError::new("peer_system_family_registry_mismatch", "peer does not name the exact embedded registry bytes"));
  }
  Ok(())
}
