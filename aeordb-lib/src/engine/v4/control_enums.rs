//! Shared frozen enums used by low-volume v4 control families.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum StableReasonV1 {
  NoneOrSuccess = 0,
  Requested = 1,
  SourceChanged = 2,
  IncompleteCoverage = 3,
  DependencyUnavailable = 4,
  UnsupportedDefinitionOrFormat = 5,
  CorruptDerivedArtifact = 6,
  CorruptAuthority = 7,
  ResourceAdmission = 8,
  Canceled = 9,
  Shutdown = 10,
  RetryableIo = 11,
  PermanentIo = 12,
  StaleFence = 13,
  InvalidConfiguration = 14,
  RootUnavailable = 15,
  RebuildRequired = 16,
  RepairRequired = 17,
  MigrationReset = 18,
  CaptureGap = 19,
  PolicyDisabled = 20,
  IntegrityMismatch = 21,
  UncertainCompletion = 22,
  CollisionAlarm = 23,
  UnknownProtectedFamily = 24,
}

impl StableReasonV1 {
  pub fn from_u16(value: u16) -> Option<Self> {
    match value {
      0 => Some(Self::NoneOrSuccess),
      1 => Some(Self::Requested),
      2 => Some(Self::SourceChanged),
      3 => Some(Self::IncompleteCoverage),
      4 => Some(Self::DependencyUnavailable),
      5 => Some(Self::UnsupportedDefinitionOrFormat),
      6 => Some(Self::CorruptDerivedArtifact),
      7 => Some(Self::CorruptAuthority),
      8 => Some(Self::ResourceAdmission),
      9 => Some(Self::Canceled),
      10 => Some(Self::Shutdown),
      11 => Some(Self::RetryableIo),
      12 => Some(Self::PermanentIo),
      13 => Some(Self::StaleFence),
      14 => Some(Self::InvalidConfiguration),
      15 => Some(Self::RootUnavailable),
      16 => Some(Self::RebuildRequired),
      17 => Some(Self::RepairRequired),
      18 => Some(Self::MigrationReset),
      19 => Some(Self::CaptureGap),
      20 => Some(Self::PolicyDisabled),
      21 => Some(Self::IntegrityMismatch),
      22 => Some(Self::UncertainCompletion),
      23 => Some(Self::CollisionAlarm),
      24 => Some(Self::UnknownProtectedFamily),
      _ => None,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum RetryClassV1 {
  None = 0,
  Immediate = 1,
  BoundedBackoff = 2,
  AfterDependency = 3,
  AfterRepair = 4,
  Never = 5,
}

impl RetryClassV1 {
  pub fn from_u16(value: u16) -> Option<Self> {
    match value {
      0 => Some(Self::None),
      1 => Some(Self::Immediate),
      2 => Some(Self::BoundedBackoff),
      3 => Some(Self::AfterDependency),
      4 => Some(Self::AfterRepair),
      5 => Some(Self::Never),
      _ => None,
    }
  }
}
