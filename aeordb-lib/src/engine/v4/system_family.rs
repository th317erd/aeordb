use std::cmp::Ordering;
use std::sync::OnceLock;

use super::contract_generated::SYSTEM_FAMILIES;
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAGIC: &[u8; 4] = b"ASFR";
const VERSION: u16 = 1;
const HEADER_LENGTH: usize = 32;
const DESCRIPTOR_FIXED_LENGTH: usize = 32;
const CRC_LENGTH: usize = 4;
const MAX_REGISTRY_LENGTH: usize = 1_048_576;
const FROZEN_DESCRIPTOR_COUNT: usize = 63;
const UNKNOWN_PROTECTED_FAMILY_ID: u16 = 0xfffe;
const SYSTEM_FAMILY_REGISTRY_V1_BYTES: &[u8] = include_bytes!("../../../spec/fixtures/system-family-registry-v1.bin");
static BLAKE3_256_REGISTRY: OnceLock<FormatResult<SystemFamilyRegistryV1<'static>>> = OnceLock::new();
static SHA256_REGISTRY: OnceLock<FormatResult<SystemFamilyRegistryV1<'static>>> = OnceLock::new();
static SHA512_REGISTRY: OnceLock<FormatResult<SystemFamilyRegistryV1<'static>>> = OnceLock::new();
static SHA3_256_REGISTRY: OnceLock<FormatResult<SystemFamilyRegistryV1<'static>>> = OnceLock::new();
static SHA3_512_REGISTRY: OnceLock<FormatResult<SystemFamilyRegistryV1<'static>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum StorageDomainV1 {
  Path = 1,
  EntryType = 2,
  KvKeyPrefix = 3,
  ControlRegion = 4,
  ExternalWorkspace = 5,
}

impl StorageDomainV1 {
  fn from_u8(value: u8) -> Option<Self> {
    match value {
      1 => Some(Self::Path),
      2 => Some(Self::EntryType),
      3 => Some(Self::KvKeyPrefix),
      4 => Some(Self::ControlRegion),
      5 => Some(Self::ExternalWorkspace),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SystemFamilyMatchKindV1 {
  AbsolutePathExact = 1,
  AbsolutePathPrefix = 2,
  DescendantReservedFile = 3,
  DescendantReservedSubtree = 4,
  ReservedPathSegment = 5,
  EntryTypeExact = 6,
  KvKeyPrefix = 7,
  ControlTagExact = 8,
  WorkspaceKindExact = 9,
}

impl SystemFamilyMatchKindV1 {
  fn from_u8(value: u8) -> Option<Self> {
    match value {
      1 => Some(Self::AbsolutePathExact),
      2 => Some(Self::AbsolutePathPrefix),
      3 => Some(Self::DescendantReservedFile),
      4 => Some(Self::DescendantReservedSubtree),
      5 => Some(Self::ReservedPathSegment),
      6 => Some(Self::EntryTypeExact),
      7 => Some(Self::KvKeyPrefix),
      8 => Some(Self::ControlTagExact),
      9 => Some(Self::WorkspaceKindExact),
      _ => None,
    }
  }
}

macro_rules! policy_enum {
  ($name:ident { $($variant:ident = $value:literal),+ $(,)? }) => {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    #[repr(u8)]
    pub enum $name {
      $($variant = $value),+
    }

    impl $name {
      pub const fn as_u8(self) -> u8 {
        self as u8
      }

      fn from_u8(value: u8) -> Option<Self> {
        match value {
          $($value => Some(Self::$variant),)+
          _ => None,
        }
      }
    }
  };
}

policy_enum!(SemanticRoleV1 {
  None = 0,
  CanonicalProjection = 1,
  ExecutableDependency = 2,
  AuthoritativeSemanticObject = 3,
  DerivedDisposable = 4,
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GcPolicyV1(u8);

impl GcPolicyV1 {
  pub const TRACE_EDGES: Self = Self(0x01);
  pub const PIN_WHILE_AUTHORITATIVE: Self = Self(0x02);
  pub const QUARANTINE: Self = Self(0x04);
  pub const DERIVED_REBUILDABLE: Self = Self(0x08);
  pub const EVIDENCE_RETENTION: Self = Self(0x10);
  pub const CONSERVATIVE_RETAIN: Self = Self(0x20);
  const KNOWN_BITS: u8 = Self::TRACE_EDGES.0
    | Self::PIN_WHILE_AUTHORITATIVE.0
    | Self::QUARANTINE.0
    | Self::DERIVED_REBUILDABLE.0
    | Self::EVIDENCE_RETENTION.0
    | Self::CONSERVATIVE_RETAIN.0;

  pub const fn bits(self) -> u8 {
    self.0
  }

  pub const fn contains(self, policy: Self) -> bool {
    self.0 & policy.0 == policy.0
  }

  fn from_bits(bits: u8) -> Option<Self> {
    (bits != 0 && bits & !Self::KNOWN_BITS == 0).then_some(Self(bits))
  }
}

policy_enum!(TransferPolicyV1 {
  RequiredInclude = 1,
  OptionalValidated = 2,
  OmitDeclared = 3,
  NodeLocal = 4,
  RedactOmit = 5,
  NamedSubsetOnly = 6,
  FailUnknown = 7,
});

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemFamilyTransferOperationV1 {
  PhysicalCopy,
  LogicalBackup,
  DataExport,
  PeerReplication,
  ClusterJoin,
  ClientSync,
  Import,
}

impl SystemFamilyTransferOperationV1 {
  pub const fn name(self) -> &'static str {
    match self {
      Self::PhysicalCopy => "physical copy",
      Self::LogicalBackup => "logical backup",
      Self::DataExport => "data export",
      Self::PeerReplication => "peer replication",
      Self::ClusterJoin => "cluster join",
      Self::ClientSync => "client sync",
      Self::Import => "import",
    }
  }
}

policy_enum!(VerifyPolicyV1 {
  StrictIfPresent = 1,
  StrictRequired = 2,
  Rebuildable = 3,
  ConservativeUnknown = 4,
});

policy_enum!(RepairPolicyV1 {
  DiagnoseOnly = 1,
  OwnerSpecific = 2,
  RebuildDerived = 3,
  RecoveryReplay = 4,
  ManualRequired = 5,
});

policy_enum!(MigrationPolicyV1 {
  RequiredCopy = 1,
  DestinationLocal = 2,
  RebuildDestination = 3,
  OwnerConverter = 4,
  OmitDeclared = 5,
  FailUnknown = 6,
});

policy_enum!(SpillPolicyV1 {
  Ineligible = 1,
  HotTailSource = 2,
  RecoveryArtifact = 3,
  ResumableWorkspace = 4,
});

policy_enum!(SensitivityV1 {
  Internal = 0,
  Protected = 1,
  Credential = 2,
  Secret = 3,
  PublicMetadata = 4,
});

policy_enum!(EventPolicyV1 {
  None = 0,
  AuthorizedNamespace = 1,
  SystemAdministrative = 2,
  OperationalRedacted = 3,
  SensitiveSuppressed = 4,
});

policy_enum!(AbsencePolicyV1 {
  AllowedDefault = 1,
  AllowedEmpty = 2,
  DegradedVisible = 3,
  RebuildRequired = 4,
  FatalIfAuthoritative = 5,
  DisableDestructiveGc = 6,
  LegacyDiagnostic = 7,
});

policy_enum!(UnknownChildPolicyV1 {
  NoChildren = 0,
  Reject = 1,
  ClassifyByRegistry = 2,
  RetainAndFailComplete = 3,
});

policy_enum!(IndexPolicyV1 {
  NotApplicable = 0,
  IncludeUnderOrdinaryScope = 1,
  ExcludeFromAllIndexes = 2,
  CanonicalProjectionOnly = 3,
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemFamilyPolicyV1 {
  pub semantic_role: SemanticRoleV1,
  pub gc_policy: GcPolicyV1,
  pub physical_copy_policy: TransferPolicyV1,
  pub logical_backup_policy: TransferPolicyV1,
  pub data_export_policy: TransferPolicyV1,
  pub peer_replication_policy: TransferPolicyV1,
  pub cluster_join_policy: TransferPolicyV1,
  pub client_sync_policy: TransferPolicyV1,
  pub import_policy: TransferPolicyV1,
  pub verify_policy: VerifyPolicyV1,
  pub repair_policy: RepairPolicyV1,
  pub migration_policy: MigrationPolicyV1,
  pub spill_policy: SpillPolicyV1,
  pub sensitivity: SensitivityV1,
  pub event_policy: EventPolicyV1,
  pub absence_policy: AbsencePolicyV1,
  pub unknown_child_policy: UnknownChildPolicyV1,
  pub index_policy: IndexPolicyV1,
}

impl SystemFamilyPolicyV1 {
  pub const fn transfer_policy(self, operation: SystemFamilyTransferOperationV1) -> TransferPolicyV1 {
    match operation {
      SystemFamilyTransferOperationV1::PhysicalCopy => self.physical_copy_policy,
      SystemFamilyTransferOperationV1::LogicalBackup => self.logical_backup_policy,
      SystemFamilyTransferOperationV1::DataExport => self.data_export_policy,
      SystemFamilyTransferOperationV1::PeerReplication => self.peer_replication_policy,
      SystemFamilyTransferOperationV1::ClusterJoin => self.cluster_join_policy,
      SystemFamilyTransferOperationV1::ClientSync => self.client_sync_policy,
      SystemFamilyTransferOperationV1::Import => self.import_policy,
    }
  }

  fn from_bytes(bytes: &[u8]) -> Option<Self> {
    Some(Self {
      semantic_role: SemanticRoleV1::from_u8(*bytes.first()?)?,
      gc_policy: GcPolicyV1::from_bits(*bytes.get(1)?)?,
      physical_copy_policy: TransferPolicyV1::from_u8(*bytes.get(2)?)?,
      logical_backup_policy: TransferPolicyV1::from_u8(*bytes.get(3)?)?,
      data_export_policy: TransferPolicyV1::from_u8(*bytes.get(4)?)?,
      peer_replication_policy: TransferPolicyV1::from_u8(*bytes.get(5)?)?,
      cluster_join_policy: TransferPolicyV1::from_u8(*bytes.get(6)?)?,
      client_sync_policy: TransferPolicyV1::from_u8(*bytes.get(7)?)?,
      import_policy: TransferPolicyV1::from_u8(*bytes.get(8)?)?,
      verify_policy: VerifyPolicyV1::from_u8(*bytes.get(9)?)?,
      repair_policy: RepairPolicyV1::from_u8(*bytes.get(10)?)?,
      migration_policy: MigrationPolicyV1::from_u8(*bytes.get(11)?)?,
      spill_policy: SpillPolicyV1::from_u8(*bytes.get(12)?)?,
      sensitivity: SensitivityV1::from_u8(*bytes.get(13)?)?,
      event_policy: EventPolicyV1::from_u8(*bytes.get(14)?)?,
      absence_policy: AbsencePolicyV1::from_u8(*bytes.get(15)?)?,
      unknown_child_policy: UnknownChildPolicyV1::from_u8(*bytes.get(16)?)?,
      index_policy: IndexPolicyV1::from_u8(*bytes.get(17)?)?,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemFamilyDescriptorV1<'a> {
  pub family_id: u16,
  pub domain: StorageDomainV1,
  pub match_kind: SystemFamilyMatchKindV1,
  pub policy: SystemFamilyPolicyV1,
  pub matcher: &'a [u8],
}

#[derive(Clone, Debug)]
pub struct SystemFamilyRegistryV1<'a> {
  pub bytes: &'a [u8],
  pub descriptor_count: u32,
  pub family_count: u16,
  pub operational_fingerprint: Vec<u8>,
  pub semantic_projection_fingerprint: Vec<u8>,
  descriptors: &'a [u8],
}

impl<'a> SystemFamilyRegistryV1<'a> {
  pub fn summary(&self) -> String {
    format!("system-family:registry:descriptors={}:families={}", self.descriptor_count, self.family_count)
  }

  pub fn iter(&self) -> SystemFamilyDescriptorIterV1<'a> {
    SystemFamilyDescriptorIterV1 { bytes: self.descriptors, offset: 0, remaining: self.descriptor_count }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemFamilySubjectV1<'a> {
  Path(&'a str),
  EntryType(u16),
  KvKey(&'a [u8]),
  ControlTag(u16),
  ExternalWorkspaceKind(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownSystemFamilyV1 {
  pub family_id: u16,
  pub policy: SystemFamilyPolicyV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemFamilyClassificationV1 {
  Ordinary,
  StructuralContainer,
  Known(KnownSystemFamilyV1),
  UnknownProtected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemFamilyPolicyDecisionV1<T> {
  Ordinary,
  StructuralContainer,
  Known { family_id: u16, policy: T },
}

#[derive(Clone, Copy, Debug)]
pub struct SystemFamilyPolicyResolverV1 {
  registry: &'static SystemFamilyRegistryV1<'static>,
}

impl SystemFamilyPolicyResolverV1 {
  pub fn embedded(algorithm: HashAlgorithm) -> FormatResult<Self> {
    Ok(Self { registry: embedded_system_family_registry(algorithm)? })
  }

  pub const fn registry(self) -> &'static SystemFamilyRegistryV1<'static> {
    self.registry
  }

  pub fn classify(self, subject: SystemFamilySubjectV1<'_>) -> FormatResult<SystemFamilyClassificationV1> {
    classify_system_family(self.registry, subject)
  }

  pub fn policy(
    self,
    subject: SystemFamilySubjectV1<'_>,
    operation: &'static str,
  ) -> FormatResult<SystemFamilyPolicyDecisionV1<SystemFamilyPolicyV1>> {
    let classification = self.classify(subject)?;
    match require_complete_system_family(classification, operation)? {
      Some(family) => Ok(SystemFamilyPolicyDecisionV1::Known { family_id: family.family_id, policy: family.policy }),
      None => match classification {
        SystemFamilyClassificationV1::Ordinary => Ok(SystemFamilyPolicyDecisionV1::Ordinary),
        SystemFamilyClassificationV1::StructuralContainer => Ok(SystemFamilyPolicyDecisionV1::StructuralContainer),
        SystemFamilyClassificationV1::Known(_) | SystemFamilyClassificationV1::UnknownProtected => {
          unreachable!("complete classification changed within immutable registry")
        }
      },
    }
  }

  pub fn transfer_policy(
    self,
    subject: SystemFamilySubjectV1<'_>,
    operation: SystemFamilyTransferOperationV1,
  ) -> FormatResult<SystemFamilyPolicyDecisionV1<TransferPolicyV1>> {
    Ok(match self.policy(subject, operation.name())? {
      SystemFamilyPolicyDecisionV1::Ordinary => SystemFamilyPolicyDecisionV1::Ordinary,
      SystemFamilyPolicyDecisionV1::StructuralContainer => SystemFamilyPolicyDecisionV1::StructuralContainer,
      SystemFamilyPolicyDecisionV1::Known { family_id, policy } => {
        SystemFamilyPolicyDecisionV1::Known { family_id, policy: policy.transfer_policy(operation) }
      }
    })
  }

  pub fn index_policy(self, subject: SystemFamilySubjectV1<'_>) -> FormatResult<SystemFamilyPolicyDecisionV1<IndexPolicyV1>> {
    Ok(match self.policy(subject, "indexing")? {
      SystemFamilyPolicyDecisionV1::Ordinary => SystemFamilyPolicyDecisionV1::Ordinary,
      SystemFamilyPolicyDecisionV1::StructuralContainer => SystemFamilyPolicyDecisionV1::StructuralContainer,
      SystemFamilyPolicyDecisionV1::Known { family_id, policy } => {
        SystemFamilyPolicyDecisionV1::Known { family_id, policy: policy.index_policy }
      }
    })
  }
}

impl SystemFamilyClassificationV1 {
  pub const fn family_id(self) -> Option<u16> {
    match self {
      Self::Known(family) => Some(family.family_id),
      Self::Ordinary | Self::StructuralContainer | Self::UnknownProtected => None,
    }
  }
}

/// Classify one storage subject through the exact selected registry.
///
/// Unrecognized `.aeordb-*` paths, `aeordb.*` KV domains, control tags, and
/// external-workspace kinds remain protected when no descriptor wins. Callers
/// must not reinterpret that result as ordinary user data.
pub fn classify_system_family(
  registry: &SystemFamilyRegistryV1<'_>,
  subject: SystemFamilySubjectV1<'_>,
) -> FormatResult<SystemFamilyClassificationV1> {
  match subject {
    SystemFamilySubjectV1::Path(path) => classify_path(registry, path),
    SystemFamilySubjectV1::EntryType(value) => classify_scalar(registry, StorageDomainV1::EntryType, value),
    SystemFamilySubjectV1::KvKey(key) => classify_kv_key(registry, key),
    SystemFamilySubjectV1::ControlTag(value) => classify_scalar(registry, StorageDomainV1::ControlRegion, value).map(protect_unmatched),
    SystemFamilySubjectV1::ExternalWorkspaceKind(value) => {
      classify_scalar(registry, StorageDomainV1::ExternalWorkspace, value).map(protect_unmatched)
    }
  }
}

pub fn require_complete_system_family(
  classification: SystemFamilyClassificationV1,
  operation: &'static str,
) -> FormatResult<Option<KnownSystemFamilyV1>> {
  match classification {
    SystemFamilyClassificationV1::Ordinary | SystemFamilyClassificationV1::StructuralContainer => Ok(None),
    SystemFamilyClassificationV1::Known(family) => Ok(Some(family)),
    SystemFamilyClassificationV1::UnknownProtected => {
      Err(closure_error("unknown_protected_system_family", format!("{operation} cannot safely process unrecognized protected state")))
    }
  }
}

fn classify_path(registry: &SystemFamilyRegistryV1<'_>, path: &str) -> FormatResult<SystemFamilyClassificationV1> {
  validate_absolute_path(path)?;
  if path.len() > 1 && path.ends_with('/') {
    return Err(path_error("system_family_matcher_exact_shape", "classified path has a trailing slash"));
  }

  let mut winner: Option<(u8, usize, KnownSystemFamilyV1)> = None;
  for descriptor in registry.iter() {
    let descriptor = descriptor?;
    if descriptor.domain != StorageDomainV1::Path {
      continue;
    }
    let Some((priority, specificity)) = path_match_score(path, descriptor.match_kind, descriptor.matcher)? else {
      continue;
    };
    let candidate = KnownSystemFamilyV1 { family_id: descriptor.family_id, policy: descriptor.policy };
    match winner {
      None => winner = Some((priority, specificity, candidate)),
      Some((winner_priority, winner_specificity, winner_family)) => {
        if (priority, specificity) > (winner_priority, winner_specificity) {
          winner = Some((priority, specificity, candidate));
        } else if (priority, specificity) == (winner_priority, winner_specificity) && winner_family.family_id != candidate.family_id {
          return Err(closure_error(
            "system_family_cross_family_overlap",
            format!("path {path} resolves equally to families 0x{:04x} and 0x{:04x}", winner_family.family_id, candidate.family_id),
          ));
        }
      }
    }
  }

  if let Some((_, _, family)) = winner {
    Ok(SystemFamilyClassificationV1::Known(family))
  } else if is_absolute_family_structural_container(registry, path)? {
    Ok(SystemFamilyClassificationV1::StructuralContainer)
  } else if path.split('/').skip(1).any(|segment| segment.starts_with(".aeordb-")) {
    Ok(SystemFamilyClassificationV1::UnknownProtected)
  } else {
    Ok(SystemFamilyClassificationV1::Ordinary)
  }
}

fn is_absolute_family_structural_container(registry: &SystemFamilyRegistryV1<'_>, path: &str) -> FormatResult<bool> {
  if path == "/" {
    return Ok(false);
  }
  for descriptor in registry.iter() {
    let descriptor = descriptor?;
    if descriptor.domain != StorageDomainV1::Path
      || !matches!(descriptor.match_kind, SystemFamilyMatchKindV1::AbsolutePathExact | SystemFamilyMatchKindV1::AbsolutePathPrefix)
    {
      continue;
    }
    let family_path = std::str::from_utf8(descriptor.matcher)
      .map_err(|_| path_error("system_family_matcher_path_utf8", "absolute matcher path is not UTF-8"))?;
    let family_path = family_path.strip_suffix('/').unwrap_or(family_path);
    let is_prefix_root = descriptor.match_kind == SystemFamilyMatchKindV1::AbsolutePathPrefix && family_path == path;
    let is_strict_ancestor = family_path.starts_with(path) && family_path.as_bytes().get(path.len()) == Some(&b'/');
    if is_prefix_root || is_strict_ancestor {
      return Ok(true);
    }
  }
  Ok(false)
}

fn path_match_score(path: &str, kind: SystemFamilyMatchKindV1, matcher: &[u8]) -> FormatResult<Option<(u8, usize)>> {
  match kind {
    SystemFamilyMatchKindV1::AbsolutePathExact => Ok((path.as_bytes() == matcher).then_some((5, matcher.len()))),
    SystemFamilyMatchKindV1::AbsolutePathPrefix => Ok(path.as_bytes().starts_with(matcher).then_some((2, matcher.len()))),
    SystemFamilyMatchKindV1::DescendantReservedFile => {
      let (segment, suffix) = decode_descendant_file_matcher(matcher)?;
      let matched_index = deepest_segment_match(path, segment, |remaining| remaining == suffix, true)?;
      matched_index.map(|index| path_specificity(index, matcher.len())).transpose().map(|value| value.map(|specificity| (4, specificity)))
    }
    SystemFamilyMatchKindV1::DescendantReservedSubtree => {
      let segment = decode_segment_matcher(matcher)?;
      let matched_index = deepest_segment_match(path, segment, |_| true, true)?;
      matched_index.map(|index| path_specificity(index, matcher.len())).transpose().map(|value| value.map(|specificity| (3, specificity)))
    }
    SystemFamilyMatchKindV1::ReservedPathSegment => {
      let segment = decode_segment_matcher(matcher)?;
      let matched_index = deepest_segment_match(path, segment, |_| true, false)?;
      matched_index.map(|index| path_specificity(index, matcher.len())).transpose().map(|value| value.map(|specificity| (1, specificity)))
    }
    SystemFamilyMatchKindV1::EntryTypeExact
    | SystemFamilyMatchKindV1::KvKeyPrefix
    | SystemFamilyMatchKindV1::ControlTagExact
    | SystemFamilyMatchKindV1::WorkspaceKindExact => Ok(None),
  }
}

fn deepest_segment_match(
  path: &str,
  wanted: &str,
  accepts_remaining: impl Fn(&str) -> bool,
  requires_ordinary_ancestor: bool,
) -> FormatResult<Option<usize>> {
  let relative = path.strip_prefix('/').expect("validated absolute path");
  let mut start = 0usize;
  let mut matched = None;
  for (index, segment) in relative.split('/').enumerate() {
    let end = checked_add(start, segment.len(), "classified path segment")?;
    let remaining = if end < relative.len() { &relative[end + 1..] } else { "" };
    if (!requires_ordinary_ancestor || index > 0) && segment == wanted && accepts_remaining(remaining) {
      matched = Some(index);
    }
    start = checked_add(end, 1, "classified path separator")?;
  }
  Ok(matched)
}

fn path_specificity(index: usize, matcher_length: usize) -> FormatResult<usize> {
  index
    .checked_mul(usize::from(u16::MAX))
    .and_then(|value| value.checked_add(matcher_length))
    .ok_or_else(|| overflow_error("system family path specificity"))
}

fn decode_descendant_file_matcher(bytes: &[u8]) -> FormatResult<(&str, &str)> {
  let segment_length = usize::from(u16_at(bytes, 0)?);
  let suffix_offset = checked_add(2, segment_length, "descendant segment")?;
  let suffix_length = usize::from(u16_at(bytes, suffix_offset)?);
  let suffix_start = checked_add(suffix_offset, 2, "descendant suffix length")?;
  let suffix_end = checked_add(suffix_start, suffix_length, "descendant suffix")?;
  let segment = std::str::from_utf8(
    bytes.get(2..suffix_offset).ok_or_else(|| trailing_error("system_family_descendant_file_length", "segment is truncated"))?,
  )
  .map_err(|_| path_error("system_family_segment_utf8", "matcher segment is not UTF-8"))?;
  let suffix = std::str::from_utf8(
    bytes.get(suffix_start..suffix_end).ok_or_else(|| trailing_error("system_family_descendant_file_length", "suffix is truncated"))?,
  )
  .map_err(|_| path_error("system_family_relative_path_utf8", "relative matcher path is not UTF-8"))?;
  Ok((segment, suffix))
}

fn decode_segment_matcher(bytes: &[u8]) -> FormatResult<&str> {
  let length = usize::from(u16_at(bytes, 0)?);
  let end = checked_add(2, length, "segment matcher")?;
  std::str::from_utf8(bytes.get(2..end).ok_or_else(|| trailing_error("system_family_matcher_segment_length", "segment is truncated"))?)
    .map_err(|_| path_error("system_family_segment_utf8", "matcher segment is not UTF-8"))
}

fn classify_scalar(
  registry: &SystemFamilyRegistryV1<'_>,
  domain: StorageDomainV1,
  value: u16,
) -> FormatResult<SystemFamilyClassificationV1> {
  let matcher = value.to_le_bytes();
  let mut winner = None;
  for descriptor in registry.iter() {
    let descriptor = descriptor?;
    if descriptor.domain != domain || descriptor.matcher != matcher {
      continue;
    }
    let candidate = KnownSystemFamilyV1 { family_id: descriptor.family_id, policy: descriptor.policy };
    if winner.is_some_and(|family: KnownSystemFamilyV1| family.family_id != candidate.family_id) {
      return Err(closure_error("system_family_cross_family_overlap", format!("scalar {value} resolves to multiple families")));
    }
    winner = Some(candidate);
  }
  Ok(winner.map(SystemFamilyClassificationV1::Known).unwrap_or(SystemFamilyClassificationV1::Ordinary))
}

fn classify_kv_key(registry: &SystemFamilyRegistryV1<'_>, key: &[u8]) -> FormatResult<SystemFamilyClassificationV1> {
  let mut winner: Option<(usize, KnownSystemFamilyV1)> = None;
  for descriptor in registry.iter() {
    let descriptor = descriptor?;
    if descriptor.domain != StorageDomainV1::KvKeyPrefix || !key.starts_with(descriptor.matcher) {
      continue;
    }
    let candidate = KnownSystemFamilyV1 { family_id: descriptor.family_id, policy: descriptor.policy };
    match winner {
      None => winner = Some((descriptor.matcher.len(), candidate)),
      Some((length, _)) if descriptor.matcher.len() > length => winner = Some((descriptor.matcher.len(), candidate)),
      Some((length, family)) if descriptor.matcher.len() == length && family.family_id != candidate.family_id => {
        return Err(closure_error("system_family_cross_family_overlap", "KV key resolves equally to multiple families"));
      }
      Some(_) => {}
    }
  }
  Ok(winner.map(|(_, family)| SystemFamilyClassificationV1::Known(family)).unwrap_or_else(|| {
    if key.starts_with(b"aeordb.") {
      SystemFamilyClassificationV1::UnknownProtected
    } else {
      SystemFamilyClassificationV1::Ordinary
    }
  }))
}

fn protect_unmatched(classification: SystemFamilyClassificationV1) -> SystemFamilyClassificationV1 {
  match classification {
    SystemFamilyClassificationV1::Ordinary => SystemFamilyClassificationV1::UnknownProtected,
    other => other,
  }
}

pub fn embedded_system_family_registry(algorithm: HashAlgorithm) -> FormatResult<&'static SystemFamilyRegistryV1<'static>> {
  let cell = match algorithm {
    HashAlgorithm::Blake3_256 => &BLAKE3_256_REGISTRY,
    HashAlgorithm::Sha256 => &SHA256_REGISTRY,
    HashAlgorithm::Sha512 => &SHA512_REGISTRY,
    HashAlgorithm::Sha3_256 => &SHA3_256_REGISTRY,
    HashAlgorithm::Sha3_512 => &SHA3_512_REGISTRY,
  };
  match cell.get_or_init(|| decode_system_family_registry(SYSTEM_FAMILY_REGISTRY_V1_BYTES, algorithm)) {
    Ok(registry) => Ok(registry),
    Err(error) => Err(error.clone()),
  }
}

pub struct SystemFamilyDescriptorIterV1<'a> {
  bytes: &'a [u8],
  offset: usize,
  remaining: u32,
}

impl<'a> Iterator for SystemFamilyDescriptorIterV1<'a> {
  type Item = FormatResult<SystemFamilyDescriptorV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    let result = decode_descriptor(self.bytes, self.offset).map(|(descriptor, next)| {
      self.offset = next;
      self.remaining -= 1;
      descriptor
    });
    if result.is_err() {
      self.remaining = 0;
    }
    Some(result)
  }
}

pub fn decode_system_family_registry(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SystemFamilyRegistryV1<'_>> {
  if bytes.len() < HEADER_LENGTH + CRC_LENGTH {
    return Err(trailing_error("system_family_registry_length", "registry is shorter than its framing"));
  }
  if bytes.len() > MAX_REGISTRY_LENGTH {
    return Err(amplification_error("system_family_registry_length", bytes.len(), MAX_REGISTRY_LENGTH));
  }
  if bytes.get(..4) != Some(MAGIC) || u16_at(bytes, 4)? != VERSION || usize::from(u16_at(bytes, 6)?) != HEADER_LENGTH {
    return Err(magic_error("system_family_registry_header", "registry magic, version, or header length is invalid"));
  }
  let total_length = usize::try_from(u32_at(bytes, 8)?).map_err(|_| overflow_error("registry total length"))?;
  if total_length != bytes.len() {
    return Err(trailing_error("system_family_registry_total_length", "registry total length does not match input"));
  }
  let descriptor_count = usize::try_from(u32_at(bytes, 12)?).map_err(|_| overflow_error("registry descriptor count"))?;
  let descriptors_length = usize::try_from(u32_at(bytes, 16)?).map_err(|_| overflow_error("registry descriptors length"))?;
  let expected_descriptors_length = bytes.len() - HEADER_LENGTH - CRC_LENGTH;
  if descriptors_length != expected_descriptors_length {
    return Err(trailing_error("system_family_registry_descriptors_length", "descriptor bytes do not close registry"));
  }
  let maximum_count = descriptors_length / DESCRIPTOR_FIXED_LENGTH;
  if descriptor_count > maximum_count {
    return Err(amplification_error("system_family_registry_descriptor_count", descriptor_count, maximum_count));
  }
  if descriptor_count != FROZEN_DESCRIPTOR_COUNT {
    return Err(closure_error(
      "system_family_registry_descriptor_count",
      format!("expected {FROZEN_DESCRIPTOR_COUNT} descriptors, found {descriptor_count}"),
    ));
  }
  if u32_at(bytes, 20)? != 0 || bytes[24..32].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("system_family_registry_reserved", "registry flags and reserve must be zero"));
  }
  let crc_offset = bytes.len() - CRC_LENGTH;
  if u32_at(bytes, crc_offset)? != crc32fast::hash(&bytes[..crc_offset]) {
    return Err(integrity_error("system_family_registry_crc", "registry CRC does not match"));
  }

  let descriptors = &bytes[HEADER_LENGTH..crc_offset];
  let mut offset = 0usize;
  let mut previous = None;
  let mut active_family = None;
  let mut active_policy = None;
  let mut expected_families = SYSTEM_FAMILIES.iter();
  let mut family_count = 0usize;
  let mut semantic_projection = Vec::with_capacity(descriptors.len().min(MAX_REGISTRY_LENGTH));
  for _ in 0..descriptor_count {
    let (descriptor, next) = decode_descriptor(descriptors, offset)?;
    if previous.as_ref().is_some_and(|prior| descriptor_key_cmp(prior, &descriptor) != Ordering::Less) {
      return Err(order_error("system_family_descriptor_order", "descriptors are duplicate or out of canonical order"));
    }
    if active_family != Some(descriptor.family_id) {
      let expected = expected_families
        .next()
        .ok_or_else(|| closure_error("system_family_registry_family_count", "registry contains an extra family"))?;
      if expected.id != descriptor.family_id {
        return Err(closure_error(
          "system_family_registry_family_coverage",
          format!("expected family 0x{:04x}, found 0x{:04x}", expected.id, descriptor.family_id),
        ));
      }
      active_family = Some(descriptor.family_id);
      active_policy = Some(descriptor.policy);
      family_count += 1;
    } else if active_policy != Some(descriptor.policy) {
      return Err(closure_error("system_family_registry_policy_drift", "one family has multiple policies"));
    }
    if descriptor.policy.semantic_role != SemanticRoleV1::None {
      append_semantic_projection(&mut semantic_projection, &descriptor)?;
    }
    previous = Some(descriptor);
    offset = next;
  }
  if offset != descriptors.len() {
    return Err(trailing_error("system_family_registry_trailing_bytes", "descriptor count leaves trailing bytes"));
  }
  if expected_families.next().is_some() || family_count != SYSTEM_FAMILIES.len() {
    return Err(closure_error("system_family_registry_family_count", "registry omits one or more frozen families"));
  }

  Ok(SystemFamilyRegistryV1 {
    bytes,
    descriptor_count: descriptor_count as u32,
    family_count: family_count as u16,
    operational_fingerprint: digest_parts(algorithm, &[b"aeordb.system-family-registry.v1\0", bytes]),
    semantic_projection_fingerprint: digest_parts(algorithm, &[b"aeordb.system-family-semantic-projection.v1\0", &semantic_projection]),
    descriptors,
  })
}

fn decode_descriptor(bytes: &[u8], offset: usize) -> FormatResult<(SystemFamilyDescriptorV1<'_>, usize)> {
  let fixed_end = checked_add(offset, DESCRIPTOR_FIXED_LENGTH, "system family descriptor fixed body")?;
  if fixed_end > bytes.len() {
    return Err(trailing_error("system_family_descriptor_truncated", "descriptor fixed body exceeds registry"));
  }
  let family_id = u16_at(bytes, offset)?;
  if family_id == 0 || family_id == UNKNOWN_PROTECTED_FAMILY_ID {
    return Err(identity_error("system_family_descriptor_identity", "family ID is zero or runtime-only unknown-protected"));
  }
  let domain = StorageDomainV1::from_u8(bytes[offset + 2])
    .ok_or_else(|| kind_error("system_family_storage_domain", "storage domain is outside the frozen enum"))?;
  let match_kind = SystemFamilyMatchKindV1::from_u8(bytes[offset + 3])
    .ok_or_else(|| kind_error("system_family_match_kind", "match kind is outside the frozen enum"))?;
  let policy = SystemFamilyPolicyV1::from_bytes(&bytes[offset + 4..offset + 22])
    .ok_or_else(|| kind_error("system_family_descriptor_policy", "descriptor policy contains an unknown enum or bit"))?;
  if bytes[offset + 22..offset + 24].iter().any(|byte| *byte != 0) || u32_at(bytes, offset + 24)? != 0 || u16_at(bytes, offset + 30)? != 0 {
    return Err(reserved_error("system_family_descriptor_reserved", "descriptor flags and reserves must be zero"));
  }
  let matcher_length = usize::from(u16_at(bytes, offset + 28)?);
  let matcher_end = checked_add(fixed_end, matcher_length, "system family matcher")?;
  if matcher_end > bytes.len() {
    return Err(trailing_error("system_family_matcher_truncated", "matcher exceeds descriptor bytes"));
  }
  let matcher = &bytes[fixed_end..matcher_end];
  validate_matcher(domain, match_kind, matcher)?;
  Ok((SystemFamilyDescriptorV1 { family_id, domain, match_kind, policy, matcher }, matcher_end))
}

fn validate_matcher(domain: StorageDomainV1, kind: SystemFamilyMatchKindV1, bytes: &[u8]) -> FormatResult<()> {
  let compatible = matches!(
    (domain, kind),
    (
      StorageDomainV1::Path,
      SystemFamilyMatchKindV1::AbsolutePathExact
        | SystemFamilyMatchKindV1::AbsolutePathPrefix
        | SystemFamilyMatchKindV1::DescendantReservedFile
        | SystemFamilyMatchKindV1::DescendantReservedSubtree
        | SystemFamilyMatchKindV1::ReservedPathSegment
    ) | (StorageDomainV1::EntryType, SystemFamilyMatchKindV1::EntryTypeExact)
      | (StorageDomainV1::KvKeyPrefix, SystemFamilyMatchKindV1::KvKeyPrefix)
      | (StorageDomainV1::ControlRegion, SystemFamilyMatchKindV1::ControlTagExact)
      | (StorageDomainV1::ExternalWorkspace, SystemFamilyMatchKindV1::WorkspaceKindExact)
  );
  if !compatible {
    return Err(kind_error("system_family_matcher_domain", "matcher kind is incompatible with storage domain"));
  }
  match kind {
    SystemFamilyMatchKindV1::AbsolutePathExact | SystemFamilyMatchKindV1::AbsolutePathPrefix => {
      let path =
        std::str::from_utf8(bytes).map_err(|_| path_error("system_family_matcher_path_utf8", "absolute matcher path is not UTF-8"))?;
      validate_absolute_path(path)?;
      if kind == SystemFamilyMatchKindV1::AbsolutePathExact && path.len() > 1 && path.ends_with('/') {
        return Err(path_error("system_family_matcher_exact_shape", "exact path has a trailing slash"));
      }
      if kind == SystemFamilyMatchKindV1::AbsolutePathPrefix && (path == "/" || !path.ends_with('/')) {
        return Err(path_error("system_family_matcher_prefix_shape", "prefix path must end in a slash and cannot be root"));
      }
    }
    SystemFamilyMatchKindV1::DescendantReservedFile => validate_descendant_file(bytes)?,
    SystemFamilyMatchKindV1::DescendantReservedSubtree | SystemFamilyMatchKindV1::ReservedPathSegment => {
      if bytes.len() < 3 {
        return Err(trailing_error("system_family_matcher_segment_length", "segment matcher is too short"));
      }
      let segment_length = usize::from(u16_at(bytes, 0)?);
      if checked_add(segment_length, 2, "segment matcher")? != bytes.len() {
        return Err(trailing_error("system_family_matcher_segment_length", "segment length does not close matcher"));
      }
      validate_segment(&bytes[2..])?;
    }
    SystemFamilyMatchKindV1::EntryTypeExact | SystemFamilyMatchKindV1::ControlTagExact | SystemFamilyMatchKindV1::WorkspaceKindExact => {
      if bytes.len() != 2 {
        return Err(trailing_error("system_family_matcher_scalar", "scalar matcher must be two bytes"));
      }
      if u16_at(bytes, 0)? == 0 {
        return Err(identity_error("system_family_matcher_scalar", "scalar matcher must be nonzero"));
      }
    }
    SystemFamilyMatchKindV1::KvKeyPrefix if bytes.is_empty() => {
      return Err(identity_error("system_family_matcher_kv_prefix", "KV prefix matcher must be nonempty"));
    }
    SystemFamilyMatchKindV1::KvKeyPrefix => {}
  }
  Ok(())
}

fn validate_descendant_file(bytes: &[u8]) -> FormatResult<()> {
  if bytes.len() < 4 {
    return Err(trailing_error("system_family_descendant_file_length", "descendant-file matcher is too short"));
  }
  let segment_length = usize::from(u16_at(bytes, 0)?);
  let suffix_length_offset = checked_add(2, segment_length, "descendant segment")?;
  let suffix_field_end = checked_add(suffix_length_offset, 2, "descendant suffix length")?;
  if suffix_field_end > bytes.len() {
    return Err(trailing_error("system_family_descendant_file_length", "descendant segment exceeds matcher"));
  }
  let suffix_length = usize::from(u16_at(bytes, suffix_length_offset)?);
  if checked_add(suffix_field_end, suffix_length, "descendant suffix")? != bytes.len() {
    return Err(trailing_error("system_family_descendant_file_length", "descendant suffix does not close matcher"));
  }
  validate_segment(&bytes[2..suffix_length_offset])?;
  if suffix_length > 0 {
    validate_relative_path(&bytes[suffix_field_end..])?;
  }
  Ok(())
}

fn validate_absolute_path(path: &str) -> FormatResult<()> {
  if !path.starts_with('/') || path.as_bytes().contains(&0) || path.contains("//") {
    return Err(path_error("system_family_absolute_path", "absolute path has invalid root, NUL, or separator"));
  }
  let core = path.strip_suffix('/').unwrap_or(path);
  if core.split('/').skip(1).any(|segment| segment.is_empty() || matches!(segment, "." | "..")) {
    return Err(path_error("system_family_absolute_path_segment", "absolute path has an invalid segment"));
  }
  Ok(())
}

fn validate_relative_path(bytes: &[u8]) -> FormatResult<()> {
  let path =
    std::str::from_utf8(bytes).map_err(|_| path_error("system_family_relative_path_utf8", "relative matcher path is not UTF-8"))?;
  if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains("//") || path.as_bytes().contains(&0) {
    return Err(path_error("system_family_relative_path", "relative matcher path has invalid shape"));
  }
  if path.split('/').any(|segment| segment.is_empty() || matches!(segment, "." | "..")) {
    return Err(path_error("system_family_relative_path_segment", "relative matcher path has an invalid segment"));
  }
  Ok(())
}

fn validate_segment(bytes: &[u8]) -> FormatResult<()> {
  let segment = std::str::from_utf8(bytes).map_err(|_| path_error("system_family_segment_utf8", "matcher segment is not UTF-8"))?;
  if segment.is_empty() || segment.contains('/') || segment.as_bytes().contains(&0) || matches!(segment, "." | "..") {
    return Err(path_error("system_family_segment", "matcher segment has invalid shape"));
  }
  Ok(())
}

fn append_semantic_projection(output: &mut Vec<u8>, descriptor: &SystemFamilyDescriptorV1<'_>) -> FormatResult<()> {
  let matcher_length = u16::try_from(descriptor.matcher.len())
    .map_err(|_| amplification_error("system_family_matcher_length", descriptor.matcher.len(), usize::from(u16::MAX)))?;
  let additional = checked_add(8, descriptor.matcher.len(), "semantic projection descriptor")?;
  let new_length = checked_add(output.len(), additional, "semantic projection")?;
  if new_length > MAX_REGISTRY_LENGTH {
    return Err(amplification_error("system_family_semantic_projection", new_length, MAX_REGISTRY_LENGTH));
  }
  output.extend_from_slice(&descriptor.family_id.to_le_bytes());
  output.push(descriptor.domain as u8);
  output.push(descriptor.match_kind as u8);
  output.extend_from_slice(&matcher_length.to_le_bytes());
  output.extend_from_slice(descriptor.matcher);
  output.push(descriptor.policy.semantic_role.as_u8());
  output.push(descriptor.policy.index_policy.as_u8());
  Ok(())
}

fn descriptor_key_cmp(left: &SystemFamilyDescriptorV1<'_>, right: &SystemFamilyDescriptorV1<'_>) -> Ordering {
  (left.family_id, left.domain as u8, left.match_kind as u8, left.matcher).cmp(&(
    right.family_id,
    right.domain as u8,
    right.match_kind as u8,
    right.matcher,
  ))
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes.get(offset..offset + 2).ok_or_else(|| trailing_error("system_family_truncated", format!("u16 at offset {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked system-family u16 width")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes.get(offset..offset + 4).ok_or_else(|| trailing_error("system_family_truncated", format!("u32 at offset {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked system-family u32 width")))
}

fn checked_add(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_add(right).ok_or_else(|| overflow_error(context))
}

fn amplification_error(code: &'static str, actual: usize, cap: usize) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, code, format!("{actual} exceeds cap {cap}"))
}

fn overflow_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "system_family_overflow", context)
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

fn integrity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::ChecksumOrIntegrityMismatch, code, context)
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

fn path_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
