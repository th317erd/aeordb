//! Bounded conversion of optional migration notices into immutable AINX.
//!
//! Persistence and AMPR selection remain owned by the migration capture
//! workspace and migration state owner. This module never acknowledges source
//! writes and cannot make their success depend on capture.

use super::coverage_journal::{
  CoverageJournalEncodeOptionsV1, CoverageJournalErrorV1, CoverageJournalWindowOptionsV1, CoverageJournalWindowOutcomeV1,
  CoverageRebuildReasonV1, encode_owned_soft_mutation_journal_segment, order_soft_mutation_window,
};
use super::coverage_runtime::{CoverageAuthorityV1, CoverageBoundaryV1, SoftMutationDrainV1};
use super::index_task::JournalOwnerKindV1;
use crate::engine::HashAlgorithm;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureDrainPlanV1 {
  hash_algorithm: HashAlgorithm,
  migration_id: [u8; 16],
  capture_generation: u64,
  segment_ordinal: u64,
  runtime_boot_id: [u8; 16],
  covered_publication_sequence: u64,
  covered_source_root: Vec<u8>,
  previous_segment: Vec<u8>,
  maximum_notices: usize,
  maximum_retained_bytes: usize,
}

impl MigrationCaptureDrainPlanV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    hash_algorithm: HashAlgorithm,
    migration_id: [u8; 16],
    capture_generation: u64,
    segment_ordinal: u64,
    runtime_boot_id: [u8; 16],
    covered_publication_sequence: u64,
    covered_source_root: Vec<u8>,
    previous_segment: Vec<u8>,
    maximum_notices: usize,
    maximum_retained_bytes: usize,
  ) -> Result<Self, MigrationCaptureRuntimeErrorV1> {
    let hash_width = hash_algorithm.hash_length();
    if migration_id == [0; 16]
      || runtime_boot_id == [0; 16]
      || capture_generation == 0
      || segment_ordinal == 0
      || maximum_notices == 0
      || maximum_retained_bytes == 0
      || covered_source_root.len() != hash_width
      || covered_source_root.iter().all(|byte| *byte == 0)
      || previous_segment.len() != hash_width
    {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidPlan);
    }
    Ok(Self {
      hash_algorithm,
      migration_id,
      capture_generation,
      segment_ordinal,
      runtime_boot_id,
      covered_publication_sequence,
      covered_source_root,
      previous_segment,
      maximum_notices,
      maximum_retained_bytes,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationCaptureInexactReasonV1 {
  InvalidNotice,
  ConflictingOperation,
  AuthorityDiscontinuity,
  PublicationGap,
  WindowLimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedMigrationCaptureSegmentV1 {
  bytes: Vec<u8>,
  identity: Vec<u8>,
  captured_through_publication_sequence: u64,
  source_root_after: Vec<u8>,
}

impl PreparedMigrationCaptureSegmentV1 {
  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  pub fn identity(&self) -> &[u8] {
    &self.identity
  }

  pub const fn captured_through_publication_sequence(&self) -> u64 {
    self.captured_through_publication_sequence
  }

  pub fn source_root_after(&self) -> &[u8] {
    &self.source_root_after
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationCaptureDrainOutcomeV1 {
  Empty,
  Exact(PreparedMigrationCaptureSegmentV1),
  FullReconciliationRequired(MigrationCaptureInexactReasonV1),
}

pub fn prepare_migration_capture_drain(
  drain: SoftMutationDrainV1,
  plan: &MigrationCaptureDrainPlanV1,
) -> Result<MigrationCaptureDrainOutcomeV1, MigrationCaptureRuntimeErrorV1> {
  if drain.notices.len() > plan.maximum_notices || drain.retained_bytes > plan.maximum_retained_bytes {
    return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::WindowLimitExceeded));
  }
  let retained_bytes = drain
    .notices
    .iter()
    .try_fold(0usize, |total, notice| total.checked_add(notice.retained_bytes()))
    .ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  if retained_bytes != drain.retained_bytes {
    return Err(MigrationCaptureRuntimeErrorV1::DrainAccounting);
  }
  if drain.notices.is_empty() {
    return Ok(MigrationCaptureDrainOutcomeV1::Empty);
  }

  let selected = drain
    .notices
    .iter()
    .max_by(|left, right| {
      left.publication_sequence.cmp(&right.publication_sequence).then_with(|| left.operation_id.cmp(&right.operation_id))
    })
    .ok_or(MigrationCaptureRuntimeErrorV1::DrainAccounting)?;
  if selected.namespace_root.len() != plan.hash_algorithm.hash_length() || selected.namespace_root.iter().all(|byte| *byte == 0) {
    return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::InvalidNotice));
  }
  let covered_authority =
    CoverageAuthorityV1::new(plan.hash_algorithm, plan.covered_source_root.clone(), Vec::new()).map_err(CoverageJournalErrorV1::Runtime)?;
  let selected_authority =
    CoverageAuthorityV1::new(plan.hash_algorithm, selected.namespace_root.clone(), Vec::new()).map_err(CoverageJournalErrorV1::Runtime)?;
  let covered = CoverageBoundaryV1 { authority: covered_authority, publication_sequence: plan.covered_publication_sequence };
  let selected = CoverageBoundaryV1 { authority: selected_authority, publication_sequence: selected.publication_sequence };
  let window = match order_soft_mutation_window(
    plan.hash_algorithm,
    drain.notices,
    &covered,
    &selected,
    CoverageJournalWindowOptionsV1::new(plan.maximum_notices, plan.maximum_retained_bytes)?,
  ) {
    CoverageJournalWindowOutcomeV1::Exact(window) => window,
    CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason } | CoverageJournalWindowOutcomeV1::RebuildRequired(reason) => {
      return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(map_inexact_reason(reason)));
    }
  };

  let mut expected_sequence = plan.covered_publication_sequence.checked_add(1).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  for notice in window.notices() {
    if notice.publication_sequence != expected_sequence {
      return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::PublicationGap));
    }
    expected_sequence = expected_sequence.checked_add(1).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  }

  let encoded = encode_owned_soft_mutation_journal_segment(
    plan.hash_algorithm,
    &window,
    plan.migration_id,
    JournalOwnerKindV1::Task,
    window.root_after(),
    CoverageJournalEncodeOptionsV1 {
      generation: plan.capture_generation,
      segment_ordinal: plan.segment_ordinal,
      previous_segment: plan.previous_segment.clone(),
      runtime_boot_id: plan.runtime_boot_id,
    },
  )?;
  Ok(MigrationCaptureDrainOutcomeV1::Exact(PreparedMigrationCaptureSegmentV1 {
    bytes: encoded.value,
    identity: encoded.key,
    captured_through_publication_sequence: selected.publication_sequence,
    source_root_after: selected.authority.source_namespace_root,
  }))
}

fn map_inexact_reason(reason: CoverageRebuildReasonV1) -> MigrationCaptureInexactReasonV1 {
  match reason {
    CoverageRebuildReasonV1::InvalidNotice => MigrationCaptureInexactReasonV1::InvalidNotice,
    CoverageRebuildReasonV1::ConflictingMutation => MigrationCaptureInexactReasonV1::ConflictingOperation,
    CoverageRebuildReasonV1::WindowLimitExceeded | CoverageRebuildReasonV1::JournalLimitExceeded => {
      MigrationCaptureInexactReasonV1::WindowLimitExceeded
    }
    _ => MigrationCaptureInexactReasonV1::AuthorityDiscontinuity,
  }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationCaptureRuntimeErrorV1 {
  #[error("migration capture drain plan is invalid")]
  InvalidPlan,
  #[error("migration capture drain accounting overflowed")]
  AccountingOverflow,
  #[error("migration capture drain retained-byte accounting is inconsistent")]
  DrainAccounting,
  #[error("migration capture journal preparation failed: {0}")]
  Journal(#[from] CoverageJournalErrorV1),
}

impl MigrationCaptureRuntimeErrorV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::InvalidPlan => "migration_capture_runtime_plan",
      Self::AccountingOverflow => "migration_capture_runtime_overflow",
      Self::DrainAccounting => "migration_capture_runtime_drain_accounting",
      Self::Journal(_) => "migration_capture_runtime_journal",
    }
  }
}
