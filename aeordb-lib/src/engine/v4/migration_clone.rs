//! Constant-state planning for one preflight-admitted v3-to-v4 base clone.
//!
//! This module classifies a caller-owned source stream. It neither retains the
//! stream nor owns source, destination, namespace, or service I/O.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use tokio_util::sync::CancellationToken;

use super::migration_preflight::{AuthorityInventoryCountsV1, MigrationPreflightPermitV1};
use super::reader::FormatError;
use super::system_family::{MigrationPolicyV1, SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilySubjectV1};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationBaseCloneItemV1<'a> {
  pub subject: SystemFamilySubjectV1<'a>,
  pub logical_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationCloneDecisionV1 {
  CopyOrdinary,
  TraverseStructuralContainer,
  CopyKnown { family_id: u16 },
  InitializeDestination { family_id: u16 },
  RebuildDestination { family_id: u16 },
  ConvertWithOwner { family_id: u16 },
  OmitDeclared { family_id: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationBaseCloneSourceClosureV1<'a> {
  pub database_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub source_header_sequence: u64,
  pub source_capture_head: &'a [u8],
  pub source_authority_digest: [u8; 32],
  pub source_authority_counts: AuthorityInventoryCountsV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationBaseClonePlanSummaryV1 {
  pub processed_items: u64,
  pub copy_items: u64,
  pub copy_logical_bytes: u64,
  pub structural_containers: u64,
  pub destination_local_items: u64,
  pub rebuild_items: u64,
  pub owner_conversion_items: u64,
  pub omitted_items: u64,
  pub source_authority_digest: [u8; 32],
  pub source_authority_counts: AuthorityInventoryCountsV1,
}

#[derive(Debug)]
pub enum MigrationBaseCloneErrorV1 {
  Canceled,
  LimitInvalid,
  ItemLimit,
  ArithmeticOverflow,
  SourceBasisMismatch,
  RegistryMismatch,
  PolicyRefused { family_id: u16 },
  SystemFamily(FormatError),
  Failed,
}

impl MigrationBaseCloneErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "migration_clone_canceled",
      Self::LimitInvalid => "migration_clone_limit_invalid",
      Self::ItemLimit => "migration_clone_item_limit",
      Self::ArithmeticOverflow => "migration_clone_arithmetic_overflow",
      Self::SourceBasisMismatch => "migration_clone_source_basis_mismatch",
      Self::RegistryMismatch => "migration_clone_registry_mismatch",
      Self::PolicyRefused { .. } => "migration_clone_policy_refused",
      Self::SystemFamily(source) => source.code(),
      Self::Failed => "migration_clone_failed",
    }
  }
}

impl Display for MigrationBaseCloneErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::PolicyRefused { family_id } => {
        write!(formatter, "{}: migration policy for family 0x{family_id:04x} refuses the clone", self.code())
      }
      Self::SystemFamily(source) => Display::fmt(source, formatter),
      _ => formatter.write_str(self.code()),
    }
  }
}

impl Error for MigrationBaseCloneErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::SystemFamily(source) => Some(source),
      _ => None,
    }
  }
}

impl From<FormatError> for MigrationBaseCloneErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::SystemFamily(source)
  }
}

#[derive(Debug)]
pub struct MigrationBaseClonePlannerV1<'a> {
  permit: &'a MigrationPreflightPermitV1,
  cancellation: &'a CancellationToken,
  resolver: SystemFamilyPolicyResolverV1,
  maximum_items: u64,
  processed_items: u64,
  copy_items: u64,
  copy_logical_bytes: u64,
  structural_containers: u64,
  destination_local_items: u64,
  rebuild_items: u64,
  owner_conversion_items: u64,
  omitted_items: u64,
  failed: bool,
}

impl<'a> MigrationBaseClonePlannerV1<'a> {
  pub fn new(
    permit: &'a MigrationPreflightPermitV1,
    cancellation: &'a CancellationToken,
    maximum_items: u64,
  ) -> Result<Self, MigrationBaseCloneErrorV1> {
    if maximum_items == 0 {
      return Err(MigrationBaseCloneErrorV1::LimitInvalid);
    }
    if cancellation.is_cancelled() {
      return Err(MigrationBaseCloneErrorV1::Canceled);
    }
    let resolver = SystemFamilyPolicyResolverV1::embedded(permit.hash_algorithm())?;
    if resolver.registry().operational_fingerprint != permit.system_family_registry_fingerprint() {
      return Err(MigrationBaseCloneErrorV1::RegistryMismatch);
    }
    Ok(Self {
      permit,
      cancellation,
      resolver,
      maximum_items,
      processed_items: 0,
      copy_items: 0,
      copy_logical_bytes: 0,
      structural_containers: 0,
      destination_local_items: 0,
      rebuild_items: 0,
      owner_conversion_items: 0,
      omitted_items: 0,
      failed: false,
    })
  }

  pub fn classify(&mut self, item: MigrationBaseCloneItemV1<'_>) -> Result<MigrationCloneDecisionV1, MigrationBaseCloneErrorV1> {
    if self.failed {
      return Err(MigrationBaseCloneErrorV1::Failed);
    }
    match self.classify_inner(item) {
      Ok(decision) => Ok(decision),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(
    self,
    closure: MigrationBaseCloneSourceClosureV1<'_>,
  ) -> Result<MigrationBaseClonePlanSummaryV1, MigrationBaseCloneErrorV1> {
    if self.failed {
      return Err(MigrationBaseCloneErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(MigrationBaseCloneErrorV1::Canceled);
    }
    if closure.database_id != self.permit.database_id()
      || closure.source_physical_instance_id != self.permit.source_physical_instance_id()
      || closure.source_header_sequence != self.permit.source_header_sequence()
      || closure.source_capture_head != self.permit.source_capture_head()
      || closure.source_authority_digest != self.permit.source_authority_digest()
      || closure.source_authority_counts != self.permit.source_authority_counts()
    {
      return Err(MigrationBaseCloneErrorV1::SourceBasisMismatch);
    }
    Ok(MigrationBaseClonePlanSummaryV1 {
      processed_items: self.processed_items,
      copy_items: self.copy_items,
      copy_logical_bytes: self.copy_logical_bytes,
      structural_containers: self.structural_containers,
      destination_local_items: self.destination_local_items,
      rebuild_items: self.rebuild_items,
      owner_conversion_items: self.owner_conversion_items,
      omitted_items: self.omitted_items,
      source_authority_digest: closure.source_authority_digest,
      source_authority_counts: closure.source_authority_counts,
    })
  }

  fn classify_inner(&mut self, item: MigrationBaseCloneItemV1<'_>) -> Result<MigrationCloneDecisionV1, MigrationBaseCloneErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(MigrationBaseCloneErrorV1::Canceled);
    }
    let processed_items = self.processed_items.checked_add(1).ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;
    if processed_items > self.maximum_items {
      return Err(MigrationBaseCloneErrorV1::ItemLimit);
    }
    let policy = self.resolver.policy(item.subject, "migration clone planning")?;
    let decision = match policy {
      SystemFamilyPolicyDecisionV1::Ordinary => MigrationCloneDecisionV1::CopyOrdinary,
      SystemFamilyPolicyDecisionV1::StructuralContainer => MigrationCloneDecisionV1::TraverseStructuralContainer,
      SystemFamilyPolicyDecisionV1::Known { family_id, policy } => match policy.migration_policy {
        MigrationPolicyV1::RequiredCopy => MigrationCloneDecisionV1::CopyKnown { family_id },
        MigrationPolicyV1::DestinationLocal => MigrationCloneDecisionV1::InitializeDestination { family_id },
        MigrationPolicyV1::RebuildDestination => MigrationCloneDecisionV1::RebuildDestination { family_id },
        MigrationPolicyV1::OwnerConverter => MigrationCloneDecisionV1::ConvertWithOwner { family_id },
        MigrationPolicyV1::OmitDeclared => MigrationCloneDecisionV1::OmitDeclared { family_id },
        MigrationPolicyV1::FailUnknown => return Err(MigrationBaseCloneErrorV1::PolicyRefused { family_id }),
      },
    };

    let is_copy = matches!(decision, MigrationCloneDecisionV1::CopyOrdinary | MigrationCloneDecisionV1::CopyKnown { .. });
    let copy_items = self.copy_items.checked_add(u64::from(is_copy)).ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;
    let copy_logical_bytes = if is_copy {
      self.copy_logical_bytes.checked_add(item.logical_bytes).ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?
    } else {
      self.copy_logical_bytes
    };
    let structural_containers = self
      .structural_containers
      .checked_add(u64::from(matches!(decision, MigrationCloneDecisionV1::TraverseStructuralContainer)))
      .ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;
    let destination_local_items = self
      .destination_local_items
      .checked_add(u64::from(matches!(decision, MigrationCloneDecisionV1::InitializeDestination { .. })))
      .ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;
    let rebuild_items = self
      .rebuild_items
      .checked_add(u64::from(matches!(decision, MigrationCloneDecisionV1::RebuildDestination { .. })))
      .ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;
    let owner_conversion_items = self
      .owner_conversion_items
      .checked_add(u64::from(matches!(decision, MigrationCloneDecisionV1::ConvertWithOwner { .. })))
      .ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;
    let omitted_items = self
      .omitted_items
      .checked_add(u64::from(matches!(decision, MigrationCloneDecisionV1::OmitDeclared { .. })))
      .ok_or(MigrationBaseCloneErrorV1::ArithmeticOverflow)?;

    self.processed_items = processed_items;
    self.copy_items = copy_items;
    self.copy_logical_bytes = copy_logical_bytes;
    self.structural_containers = structural_containers;
    self.destination_local_items = destination_local_items;
    self.rebuild_items = rebuild_items;
    self.owner_conversion_items = owner_conversion_items;
    self.omitted_items = omitted_items;
    Ok(decision)
  }
}
