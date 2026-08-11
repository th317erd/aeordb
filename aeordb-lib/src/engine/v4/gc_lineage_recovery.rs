//! Conservative, bounded recovery of missing v4 retirement lineage.
//!
//! This module consumes one externally grouped logical-key family at a time.
//! It never selects active authority and never creates reclaim permission.

use std::collections::BTreeMap;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use super::gc::{PhysicalIncarnationV1, compare_physical_incarnations_v1, decode_physical_incarnation};
use super::gc_audit::{
  CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceDurableSinkV1, CorruptGcEvidenceSinkErrorV1, CorruptGcEvidenceWriteV1,
  GcErrorClassV1, encode_corrupt_gc_evidence_v1,
};
use super::gc_retirement::{
  RetirementJournalDurableSinkV1, RetirementJournalOwnerErrorV1, RetirementJournalOwnerV1, RetirementJournalRecordWriteV1,
};
use super::gc_state::RetirementReasonV1;
use super::hash::digest_parts;
use super::reader::FormatError;
use crate::engine::HashAlgorithm;

pub const MAX_RETIREMENT_LINEAGE_RECOVERY_INCARNATIONS_V1: usize = 64;

/// Recovery evidence and synthesized retirement records must share one hard
/// publication authority; callers cannot split them across durability domains.
pub trait RetirementLineageRecoveryDurableSinkV1: RetirementJournalDurableSinkV1 + CorruptGcEvidenceDurableSinkV1 {}

impl<T> RetirementLineageRecoveryDurableSinkV1 for T where T: RetirementJournalDurableSinkV1 + CorruptGcEvidenceDurableSinkV1 {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetirementLineageRecoveryContextV1 {
  pub database_id: [u8; 16],
  pub run_id: [u8; 16],
  pub generation: u64,
  pub detected_at_ms: i64,
  pub recovery_publication_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetirementLineageRecoveryObservationV1<'a> {
  pub incarnation: &'a [u8],
  pub retirement_present: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RetirementLineageRecoveryGroupV1<'a> {
  pub selected_incarnation: &'a [u8],
  pub observations: &'a [RetirementLineageRecoveryObservationV1<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RetirementLineageRecoveryIssueV1 {
  IncarnationLimit = 1,
  MalformedIncarnation = 2,
  WrongLogicalIdentity = 3,
  NoncanonicalObservationOrder = 4,
  MissingRetirementLineage = 5,
  SelectedIncarnationMissing = 6,
  SelectedIncarnationRetired = 7,
  SelectedAuthorityMismatch = 8,
  AmbiguousHighestSequence = 9,
  OverlappingExtent = 10,
}

impl RetirementLineageRecoveryIssueV1 {
  fn name(self) -> &'static str {
    match self {
      Self::IncarnationLimit => "incarnation_limit",
      Self::MalformedIncarnation => "malformed_incarnation",
      Self::WrongLogicalIdentity => "wrong_logical_identity",
      Self::NoncanonicalObservationOrder => "noncanonical_observation_order",
      Self::MissingRetirementLineage => "missing_retirement_lineage",
      Self::SelectedIncarnationMissing => "selected_incarnation_missing",
      Self::SelectedIncarnationRetired => "selected_incarnation_retired",
      Self::SelectedAuthorityMismatch => "selected_authority_mismatch",
      Self::AmbiguousHighestSequence => "ambiguous_highest_sequence",
      Self::OverlappingExtent => "overlapping_extent",
    }
  }

  fn error_class(self) -> GcErrorClassV1 {
    match self {
      Self::IncarnationLimit | Self::NoncanonicalObservationOrder => GcErrorClassV1::IncompleteAuthorityWalk,
      Self::MalformedIncarnation => GcErrorClassV1::Framing,
      Self::WrongLogicalIdentity | Self::SelectedIncarnationRetired | Self::SelectedAuthorityMismatch => GcErrorClassV1::WrongIdentity,
      Self::MissingRetirementLineage | Self::SelectedIncarnationMissing => GcErrorClassV1::MissingEdge,
      Self::AmbiguousHighestSequence => GcErrorClassV1::AmbiguousControl,
      Self::OverlappingExtent => GcErrorClassV1::BoundsOrOverlap,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetirementLineageRecoveryDispositionV1 {
  AlreadyComplete,
  Synthesized { record_count: u32 },
  Protected { issue: RetirementLineageRecoveryIssueV1 },
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementLineageRecoveryOutcomeV1 {
  pub disposition: RetirementLineageRecoveryDispositionV1,
  pub evidence_receipt: Option<CorruptGcEvidenceDurabilityReceiptV1>,
  pub journal_hard_publication_sequence: u64,
}

impl RetirementLineageRecoveryOutcomeV1 {
  pub const fn authorizes_reclaim(&self) -> bool {
    false
  }
}

#[derive(Debug, Error)]
pub enum RetirementLineageRecoveryErrorV1 {
  #[error("retirement-lineage recovery context is invalid: {0}")]
  InvalidContext(&'static str),
  #[error("retirement-lineage recovery was canceled")]
  Canceled,
  #[error("retirement-lineage recovery has latched a terminal failure")]
  Failed,
  #[error(transparent)]
  EvidenceFormat(#[from] FormatError),
  #[error("corrupt-GC evidence durable sink failed: {0}")]
  EvidenceSink(#[from] CorruptGcEvidenceSinkErrorV1),
  #[error("corrupt-GC evidence durability receipt did not bind the exact artifact")]
  EvidenceReceipt,
  #[error("retirement-journal recovery failed after admitting {admitted_records} record(s): {source}")]
  Journal {
    #[source]
    source: RetirementJournalOwnerErrorV1,
    admitted_records: u32,
  },
  #[error("retirement-lineage recovery counters overflowed")]
  ArithmeticOverflow,
}

impl RetirementLineageRecoveryErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidContext(_) => "retirement_lineage_recovery_context",
      Self::Canceled => "retirement_lineage_recovery_cancelled",
      Self::Failed => "retirement_lineage_recovery_failed",
      Self::EvidenceFormat(error) => error.code(),
      Self::EvidenceSink(_) => "corrupt_gc_evidence_sink",
      Self::EvidenceReceipt => "corrupt_gc_evidence_receipt",
      Self::Journal { source, .. } => source.code(),
      Self::ArithmeticOverflow => "retirement_lineage_recovery_arithmetic",
    }
  }

  pub const fn admitted_records(&self) -> u32 {
    match self {
      Self::Journal { admitted_records, .. } => *admitted_records,
      _ => 0,
    }
  }
}

pub struct RetirementLineageRecoveryReconcilerV1<'a> {
  algorithm: HashAlgorithm,
  context: RetirementLineageRecoveryContextV1,
  cancellation: &'a CancellationToken,
  previous_logical_key: Vec<u8>,
  failed: bool,
}

impl<'a> RetirementLineageRecoveryReconcilerV1<'a> {
  pub fn new(
    algorithm: HashAlgorithm,
    context: RetirementLineageRecoveryContextV1,
    cancellation: &'a CancellationToken,
  ) -> Result<Self, RetirementLineageRecoveryErrorV1> {
    if context.database_id.iter().all(|byte| *byte == 0)
      || context.run_id.iter().all(|byte| *byte == 0)
      || context.generation == 0
      || context.detected_at_ms <= 0
      || context.recovery_publication_sequence == 0
    {
      return Err(RetirementLineageRecoveryErrorV1::InvalidContext(
        "database, run, generation, time, and publication sequence must be nonzero",
      ));
    }
    Ok(Self { algorithm, context, cancellation, previous_logical_key: Vec::with_capacity(algorithm.hash_length()), failed: false })
  }

  pub fn recover_group<S: RetirementLineageRecoveryDurableSinkV1>(
    &mut self,
    group: RetirementLineageRecoveryGroupV1<'_>,
    monotonic_now_ms: u64,
    owner: &mut RetirementJournalOwnerV1<'_>,
    sink: &mut S,
  ) -> Result<RetirementLineageRecoveryOutcomeV1, RetirementLineageRecoveryErrorV1> {
    if self.failed {
      return Err(RetirementLineageRecoveryErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RetirementLineageRecoveryErrorV1::Canceled);
    }
    if owner.hash_algorithm() != self.algorithm || owner.database_id() != self.context.database_id {
      return Err(RetirementLineageRecoveryErrorV1::InvalidContext("retirement owner belongs to another database or hash profile"));
    }

    let selected = match decode_physical_incarnation(group.selected_incarnation, self.algorithm) {
      Ok(selected) => selected,
      Err(_) => return self.protect(group, None, RetirementLineageRecoveryIssueV1::MalformedIncarnation, sink),
    };
    if !self.previous_logical_key.is_empty() && self.previous_logical_key.as_slice() >= selected.logical_key {
      return self.protect(group, Some(selected), RetirementLineageRecoveryIssueV1::NoncanonicalObservationOrder, sink);
    }
    if group.observations.len() > MAX_RETIREMENT_LINEAGE_RECOVERY_INCARNATIONS_V1 {
      return self.protect(group, Some(selected), RetirementLineageRecoveryIssueV1::IncarnationLimit, sink);
    }
    let issue = self.validate_group(&group, &selected)?;
    if let Some(issue) = issue {
      let outcome = self.protect(group, Some(selected), issue, sink)?;
      self.remember_logical_key(selected.logical_key);
      return Ok(outcome);
    }

    let recovery_publication_sequence = self.context.recovery_publication_sequence;
    let retired_at_ms = self.context.detected_at_ms as u64;
    let missing_records = || {
      group
        .observations
        .iter()
        .filter(|observation| observation.incarnation != group.selected_incarnation && !observation.retirement_present)
        .map(|observation| RetirementJournalRecordWriteV1 {
          reason: RetirementReasonV1::Repair,
          replacement_publication_sequence: recovery_publication_sequence,
          retired_at_ms,
          old_incarnation: observation.incarnation,
          replacement_incarnation: group.selected_incarnation,
        })
    };
    let missing_count = missing_records().count();
    if missing_count == 0 {
      self.remember_logical_key(selected.logical_key);
      return Ok(RetirementLineageRecoveryOutcomeV1 {
        disposition: RetirementLineageRecoveryDispositionV1::AlreadyComplete,
        evidence_receipt: None,
        journal_hard_publication_sequence: owner.status().last_hard_publication_sequence,
      });
    }
    owner
      .preflight_record_batch(missing_records(), monotonic_now_ms)
      .map_err(|source| RetirementLineageRecoveryErrorV1::Journal { source, admitted_records: 0 })?;
    let evidence_receipt =
      self.emit_evidence(group, Some(selected), RetirementLineageRecoveryIssueV1::MissingRetirementLineage, missing_count, sink)?;
    let mut admitted_records = 0u32;
    for record in missing_records() {
      match owner.append(record, monotonic_now_ms, sink) {
        Ok(()) => admitted_records = admitted_records.checked_add(1).ok_or(RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?,
        Err(source) => {
          if source.incoming_record_retained() {
            admitted_records = admitted_records.checked_add(1).ok_or(RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
          }
          return Err(RetirementLineageRecoveryErrorV1::Journal { source, admitted_records });
        }
      }
    }
    if let Err(source) = owner.flush(sink) {
      return Err(RetirementLineageRecoveryErrorV1::Journal { source, admitted_records });
    }
    let record_count = u32::try_from(missing_count).map_err(|_| RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
    let journal_hard_publication_sequence = owner.status().last_hard_publication_sequence;
    self.remember_logical_key(selected.logical_key);
    Ok(RetirementLineageRecoveryOutcomeV1 {
      disposition: RetirementLineageRecoveryDispositionV1::Synthesized { record_count },
      evidence_receipt: Some(evidence_receipt),
      journal_hard_publication_sequence,
    })
  }

  fn validate_group(
    &self,
    group: &RetirementLineageRecoveryGroupV1<'_>,
    selected: &PhysicalIncarnationV1<'_>,
  ) -> Result<Option<RetirementLineageRecoveryIssueV1>, RetirementLineageRecoveryErrorV1> {
    let mut previous = None;
    let mut selected_count = 0usize;
    let mut selected_retired = false;
    let mut highest_sequence = 0u64;
    let mut highest_count = 0usize;
    for observation in group.observations {
      let incarnation = match decode_physical_incarnation(observation.incarnation, self.algorithm) {
        Ok(incarnation) => incarnation,
        Err(_) => return Ok(Some(RetirementLineageRecoveryIssueV1::MalformedIncarnation)),
      };
      if incarnation.logical_key != selected.logical_key || incarnation.entry_type != selected.entry_type {
        return Ok(Some(RetirementLineageRecoveryIssueV1::WrongLogicalIdentity));
      }
      if previous.as_ref().is_some_and(|previous| compare_physical_incarnations_v1(previous, &incarnation).is_ge()) {
        return Ok(Some(RetirementLineageRecoveryIssueV1::NoncanonicalObservationOrder));
      }
      if observation.incarnation == group.selected_incarnation {
        selected_count += 1;
        selected_retired |= observation.retirement_present;
      }
      if incarnation.write_sequence > highest_sequence {
        highest_sequence = incarnation.write_sequence;
        highest_count = 1;
      } else if incarnation.write_sequence == highest_sequence {
        highest_count = highest_count.checked_add(1).ok_or(RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
      }
      previous = Some(incarnation);
    }
    if selected_count == 0 {
      return Ok(Some(RetirementLineageRecoveryIssueV1::SelectedIncarnationMissing));
    }
    if selected_count != 1 || selected_retired {
      return Ok(Some(RetirementLineageRecoveryIssueV1::SelectedIncarnationRetired));
    }
    if selected.write_sequence != highest_sequence {
      return Ok(Some(RetirementLineageRecoveryIssueV1::SelectedAuthorityMismatch));
    }
    if highest_count != 1 {
      return Ok(Some(RetirementLineageRecoveryIssueV1::AmbiguousHighestSequence));
    }
    for left_index in 0..group.observations.len() {
      let left = decode_physical_incarnation(group.observations[left_index].incarnation, self.algorithm)?;
      let left_end =
        left.wal_offset.checked_add(u64::from(left.entity_length)).ok_or(RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
      for right in &group.observations[left_index + 1..] {
        let right = decode_physical_incarnation(right.incarnation, self.algorithm)?;
        let right_end =
          right.wal_offset.checked_add(u64::from(right.entity_length)).ok_or(RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
        if left.wal_offset < right_end && right.wal_offset < left_end {
          return Ok(Some(RetirementLineageRecoveryIssueV1::OverlappingExtent));
        }
      }
    }
    Ok(None)
  }

  fn protect(
    &mut self,
    group: RetirementLineageRecoveryGroupV1<'_>,
    selected: Option<PhysicalIncarnationV1<'_>>,
    issue: RetirementLineageRecoveryIssueV1,
    evidence_sink: &mut dyn CorruptGcEvidenceDurableSinkV1,
  ) -> Result<RetirementLineageRecoveryOutcomeV1, RetirementLineageRecoveryErrorV1> {
    let evidence_receipt = self.emit_evidence(group, selected, issue, 0, evidence_sink)?;
    Ok(RetirementLineageRecoveryOutcomeV1 {
      disposition: RetirementLineageRecoveryDispositionV1::Protected { issue },
      evidence_receipt: Some(evidence_receipt),
      journal_hard_publication_sequence: 0,
    })
  }

  fn emit_evidence(
    &mut self,
    group: RetirementLineageRecoveryGroupV1<'_>,
    selected: Option<PhysicalIncarnationV1<'_>>,
    issue: RetirementLineageRecoveryIssueV1,
    synthesized_count: usize,
    sink: &mut dyn CorruptGcEvidenceDurableSinkV1,
  ) -> Result<CorruptGcEvidenceDurabilityReceiptV1, RetirementLineageRecoveryErrorV1> {
    let context =
      recovery_evidence_context(issue, group.observations.len(), synthesized_count, selected.map(|value| value.write_sequence))?;
    let mut evidence_hashes: Vec<_> = group
      .observations
      .iter()
      .take(MAX_RETIREMENT_LINEAGE_RECOVERY_INCARNATIONS_V1)
      .map(|observation| digest_parts(self.algorithm, &[b"aeordb.retirement-lineage-observation.v1\0", observation.incarnation]))
      .collect();
    evidence_hashes.push(digest_parts(self.algorithm, &[b"aeordb.retirement-lineage-selected.v1\0", group.selected_incarnation]));
    evidence_hashes.sort_unstable();
    evidence_hashes.dedup();
    evidence_hashes.truncate(64);
    let evidence_hashes: Vec<u8> = evidence_hashes.into_iter().flatten().collect();
    let issue_code = (issue as u8).to_le_bytes();
    let generation = self.context.generation.to_le_bytes();
    let evidence_digest = digest_parts(
      self.algorithm,
      &[
        b"aeordb.retirement-lineage-recovery-evidence.v1\0",
        &self.context.database_id,
        &self.context.run_id,
        &generation,
        &issue_code,
        selected.map_or(group.selected_incarnation, |value| value.logical_key),
      ],
    );
    let evidence_id: [u8; 16] = evidence_digest[..16].try_into().map_err(|_| RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
    let request = CorruptGcEvidenceWriteV1 {
      database_id: self.context.database_id,
      evidence_id,
      generation: self.context.generation,
      detected_at_ms: self.context.detected_at_ms,
      error_class: issue.error_class(),
      observed_entry_type: selected.map(|value| value.entry_type),
      observed_artifact_kind: None,
      physical_range: selected.map(|value| (value.wal_offset, value.entity_length)),
      write_sequence: selected.and_then(|value| (value.write_sequence != 0).then_some(value.write_sequence)),
      expected_hash: selected.map(|value| value.logical_key),
      observed_hash: selected.map(|value| value.integrity_or_legacy_digest),
      run_id: Some(self.context.run_id),
      control_kind: None,
      control_identity_digest: None,
      context: &context,
      evidence_hashes: &evidence_hashes,
    };
    let encoded = encode_corrupt_gc_evidence_v1(&request, self.algorithm).map_err(|error| {
      self.failed = true;
      RetirementLineageRecoveryErrorV1::EvidenceFormat(error)
    })?;
    let receipt = sink.publish_corrupt_evidence_synced(&encoded.key, &encoded.value)?;
    let stored_value_length = u32::try_from(encoded.value.len()).map_err(|_| RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?;
    if receipt.artifact_key != encoded.key || receipt.stored_value_length != stored_value_length || receipt.hard_publication_sequence == 0 {
      self.failed = true;
      return Err(RetirementLineageRecoveryErrorV1::EvidenceReceipt);
    }
    Ok(receipt)
  }

  fn remember_logical_key(&mut self, logical_key: &[u8]) {
    self.previous_logical_key.clear();
    self.previous_logical_key.extend_from_slice(logical_key);
  }
}

fn recovery_evidence_context(
  issue: RetirementLineageRecoveryIssueV1,
  observation_count: usize,
  synthesized_count: usize,
  selected_write_sequence: Option<u64>,
) -> Result<Vec<u8>, RetirementLineageRecoveryErrorV1> {
  let mut values = BTreeMap::new();
  values.insert("issue".to_string(), CanonicalConfigValueV1::String(issue.name().to_string()));
  values.insert(
    "observation_count".to_string(),
    CanonicalConfigValueV1::Signed(i64::try_from(observation_count).map_err(|_| RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?),
  );
  values.insert("schema".to_string(), CanonicalConfigValueV1::String("retirement-lineage-recovery-v1".to_string()));
  values.insert(
    "selected_write_sequence".to_string(),
    CanonicalConfigValueV1::Signed(
      i64::try_from(selected_write_sequence.unwrap_or(0)).map_err(|_| RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?,
    ),
  );
  values.insert(
    "synthesized_count".to_string(),
    CanonicalConfigValueV1::Signed(i64::try_from(synthesized_count).map_err(|_| RetirementLineageRecoveryErrorV1::ArithmeticOverflow)?),
  );
  encode_canonical_value(&CanonicalConfigValueV1::Map(values), CanonicalValueBounds::AUDIT_VALUE).map_err(Into::into)
}
