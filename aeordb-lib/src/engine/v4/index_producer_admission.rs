use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::path_utils::normalize_path;

use super::hash::digest_parts;
use super::index_producer_coordinator::{
  INDEX_PRODUCER_SCOPE_BYTES_MAX, IndexProducerAdmissionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1,
  IndexProducerDurableTaskStoreV1, IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskRequestV1,
};
use super::index_task::{MutationJournalV1, MutationRecordV1};
use super::reader::FormatError;

const MUTATION_OPERATION_DOMAIN_V1: &[u8] = b"aeordb:index-producer:mutation-operation:v1\0";
const MAINTENANCE_OPERATION_DOMAIN_V1: &[u8] = b"aeordb:index-producer:maintenance-operation:v1\0";
const IMPLICIT_MAINTENANCE_SOURCE_DOMAIN_V1: &[u8] = b"aeordb:index-producer:implicit-maintenance-source:v1\0";

#[derive(Debug, Clone, Copy)]
pub struct IndexProducerMaintenanceIntentV1<'a> {
  pub source_operation_id: [u8; 16],
  pub class: IndexProducerMaintenanceClassV1,
  pub publication_sequence: u64,
  pub namespace_root: &'a [u8],
  pub semantic_state_root: &'a [u8],
  pub scope: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexProducerMaintenanceTargetV1<'a> {
  pub class: IndexProducerMaintenanceClassV1,
  pub scope: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProducerMaintenanceClassV1 {
  DeleteCleanup,
  ConfigurationRetirement,
  Reindex,
  Repair,
  ExplicitLegacyMutation,
  LegacyMigration,
  DefinitionBuild,
  Compaction,
}

impl IndexProducerMaintenanceClassV1 {
  pub const fn id(self) -> u16 {
    match self {
      Self::DeleteCleanup => 1,
      Self::ConfigurationRetirement => 2,
      Self::Reindex => 3,
      Self::Repair => 4,
      Self::ExplicitLegacyMutation => 5,
      Self::LegacyMigration => 6,
      Self::DefinitionBuild => 7,
      Self::Compaction => 8,
    }
  }

  pub const fn task_kind(self) -> IndexProducerTaskKindV1 {
    match self {
      Self::DeleteCleanup | Self::Reindex => IndexProducerTaskKindV1::Rebuild,
      Self::ConfigurationRetirement => IndexProducerTaskKindV1::Retire,
      Self::Repair => IndexProducerTaskKindV1::Repair,
      Self::ExplicitLegacyMutation => IndexProducerTaskKindV1::ExplicitMutation,
      Self::LegacyMigration => IndexProducerTaskKindV1::LegacyMigration,
      Self::DefinitionBuild => IndexProducerTaskKindV1::Build,
      Self::Compaction => IndexProducerTaskKindV1::Compact,
    }
  }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerMaintenanceAdmissionErrorV1 {
  #[error("invalid index producer maintenance intent: {0}")]
  Invalid(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexProducerJournalAdmissionSummaryV1 {
  pub queued: u32,
  pub duplicates: u32,
  pub spilled: u32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerJournalAdmissionErrorV1 {
  #[error("mutation journal hash profile is inconsistent with {algorithm:?}: {role} has {actual} bytes, expected {expected}")]
  HashProfileMismatch { algorithm: HashAlgorithm, role: &'static str, expected: usize, actual: usize },
  #[error("mutation identity has {actual} bytes, expected {expected} for {algorithm:?}")]
  InvalidMutationIdentity { algorithm: HashAlgorithm, expected: usize, actual: usize },
  #[error("derived mutation operation identity is all zeroes")]
  ZeroOperationIdentity,
  #[error("mutation journal record decoding failed: {0}")]
  Format(#[from] FormatError),
  #[error("mutation journal root continuity failed between records {previous_record} and {next_record}")]
  DiscontinuousRoots { previous_record: u32, next_record: u32 },
  #[error("mutation journal admission accounting overflowed")]
  AccountingOverflow,
  #[error("mutation journal task admission was cancelled")]
  Cancelled,
  #[error("index producer task admission failed: {0}")]
  Coordinator(#[from] IndexProducerCoordinatorErrorV1),
}

pub fn derive_mutation_operation_id(
  hash_algorithm: HashAlgorithm,
  mutation_id: &[u8],
  batch_ordinal: u32,
) -> Result<[u8; 16], IndexProducerJournalAdmissionErrorV1> {
  let expected = hash_algorithm.hash_length();
  if mutation_id.len() != expected {
    return Err(IndexProducerJournalAdmissionErrorV1::InvalidMutationIdentity {
      algorithm: hash_algorithm,
      expected,
      actual: mutation_id.len(),
    });
  }
  let ordinal = batch_ordinal.to_le_bytes();
  let digest = digest_parts(hash_algorithm, &[MUTATION_OPERATION_DOMAIN_V1, mutation_id, &ordinal]);
  let operation_prefix = digest.get(..16).ok_or(IndexProducerJournalAdmissionErrorV1::InvalidMutationIdentity {
    algorithm: hash_algorithm,
    expected: 16,
    actual: digest.len(),
  })?;
  let mut operation_id = [0u8; 16];
  operation_id.copy_from_slice(operation_prefix);
  if operation_id == [0; 16] {
    return Err(IndexProducerJournalAdmissionErrorV1::ZeroOperationIdentity);
  }
  Ok(operation_id)
}

pub fn build_maintenance_task<'a>(
  hash_algorithm: HashAlgorithm,
  intent: IndexProducerMaintenanceIntentV1<'a>,
) -> Result<IndexProducerTaskRequestV1<'a>, IndexProducerMaintenanceAdmissionErrorV1> {
  if intent.source_operation_id == [0; 16] {
    return Err(maintenance_invalid("source operation identity is all zeroes"));
  }
  if intent.publication_sequence == 0 {
    return Err(maintenance_invalid("publication sequence must be nonzero"));
  }
  let hash_width = hash_algorithm.hash_length();
  for (role, value) in [("namespace root", intent.namespace_root), ("semantic-state root", intent.semantic_state_root)] {
    if value.len() != hash_width || value.iter().all(|byte| *byte == 0) {
      return Err(maintenance_invalid(format!("{role} must be a nonzero complete database hash")));
    }
  }
  validate_maintenance_scope(intent.scope)?;
  let class = intent.class.id().to_le_bytes();
  let kind = intent.class.task_kind();
  let kind_id = kind.id().to_le_bytes();
  let publication_sequence = intent.publication_sequence.to_le_bytes();
  let scope_length = u64::try_from(intent.scope.len())
    .map_err(|error| maintenance_invalid(format!("scope length does not fit u64: {error}")))?
    .to_le_bytes();
  let digest = digest_parts(
    hash_algorithm,
    &[
      MAINTENANCE_OPERATION_DOMAIN_V1,
      &intent.source_operation_id,
      &class,
      &kind_id,
      &publication_sequence,
      intent.namespace_root,
      intent.semantic_state_root,
      &scope_length,
      intent.scope.as_bytes(),
    ],
  );
  let operation_prefix =
    digest.get(..16).ok_or_else(|| maintenance_invalid(format!("derived operation digest has only {} bytes", digest.len())))?;
  let mut operation_id = [0u8; 16];
  operation_id.copy_from_slice(operation_prefix);
  if operation_id == [0; 16] {
    return Err(maintenance_invalid("derived operation identity is all zeroes"));
  }
  Ok(IndexProducerTaskRequestV1 {
    operation_id,
    kind,
    publication_sequence: intent.publication_sequence,
    namespace_root_before: intent.namespace_root,
    namespace_root_after: intent.namespace_root,
    semantic_state_root: intent.semantic_state_root,
    journal_head: None,
    scope: Some(intent.scope),
  })
}

pub fn derive_implicit_maintenance_source_operation_id(
  hash_algorithm: HashAlgorithm,
  class: IndexProducerMaintenanceClassV1,
  scope: &str,
) -> Result<[u8; 16], IndexProducerMaintenanceAdmissionErrorV1> {
  validate_maintenance_scope(scope)?;
  let class = class.id().to_le_bytes();
  let scope_length =
    u64::try_from(scope.len()).map_err(|error| maintenance_invalid(format!("scope length does not fit u64: {error}")))?.to_le_bytes();
  let digest = digest_parts(hash_algorithm, &[IMPLICIT_MAINTENANCE_SOURCE_DOMAIN_V1, &class, &scope_length, scope.as_bytes()]);
  let operation_prefix =
    digest.get(..16).ok_or_else(|| maintenance_invalid(format!("implicit source operation digest has only {} bytes", digest.len())))?;
  let mut operation_id = [0u8; 16];
  operation_id.copy_from_slice(operation_prefix);
  if operation_id == [0; 16] {
    return Err(maintenance_invalid("derived implicit source operation identity is all zeroes"));
  }
  Ok(operation_id)
}

fn validate_maintenance_scope(scope: &str) -> Result<(), IndexProducerMaintenanceAdmissionErrorV1> {
  if scope.is_empty() || !scope.starts_with('/') || scope.len() > INDEX_PRODUCER_SCOPE_BYTES_MAX || normalize_path(scope) != scope {
    return Err(maintenance_invalid("scope must be a nonempty canonical absolute path within the fixed bound"));
  }
  Ok(())
}

fn maintenance_invalid(message: impl Into<String>) -> IndexProducerMaintenanceAdmissionErrorV1 {
  IndexProducerMaintenanceAdmissionErrorV1::Invalid(message.into())
}

pub fn admit_mutation_journal_tasks(
  hash_algorithm: HashAlgorithm,
  producer: &mut IndexProducerCoordinatorV1,
  journal: &MutationJournalV1<'_>,
  now_ms: u64,
  is_cancelled: &dyn Fn() -> bool,
  spill_store: &mut dyn IndexProducerSpillStoreV1,
) -> Result<IndexProducerJournalAdmissionSummaryV1, IndexProducerJournalAdmissionErrorV1> {
  visit_mutation_journal_tasks(hash_algorithm, journal, is_cancelled, |request| producer.admit_or_spill(request, now_ms, spill_store))
}

pub fn admit_durable_mutation_journal_tasks<Store>(
  hash_algorithm: HashAlgorithm,
  producer: &mut IndexProducerCoordinatorV1,
  journal: &MutationJournalV1<'_>,
  now_ms: u64,
  is_cancelled: &dyn Fn() -> bool,
  store: &mut Store,
) -> Result<IndexProducerJournalAdmissionSummaryV1, IndexProducerJournalAdmissionErrorV1>
where
  Store: IndexProducerDurableTaskStoreV1 + IndexProducerSpillStoreV1,
{
  visit_mutation_journal_tasks(hash_algorithm, journal, is_cancelled, |request| producer.admit_durable_or_spill(request, now_ms, store))
}

fn visit_mutation_journal_tasks(
  hash_algorithm: HashAlgorithm,
  journal: &MutationJournalV1<'_>,
  is_cancelled: &dyn Fn() -> bool,
  mut admit: impl FnMut(IndexProducerTaskRequestV1<'_>) -> Result<IndexProducerAdmissionV1, IndexProducerCoordinatorErrorV1>,
) -> Result<IndexProducerJournalAdmissionSummaryV1, IndexProducerJournalAdmissionErrorV1> {
  if is_cancelled() {
    return Err(IndexProducerJournalAdmissionErrorV1::Cancelled);
  }
  validate_journal(hash_algorithm, journal, is_cancelled)?;
  let mut summary = IndexProducerJournalAdmissionSummaryV1::default();
  for record in journal.records.iter() {
    if is_cancelled() {
      return Err(IndexProducerJournalAdmissionErrorV1::Cancelled);
    }
    let record = record?;
    let operation_id = derive_mutation_operation_id(hash_algorithm, record.mutation_id, record.batch_ordinal)?;
    let admission = admit(IndexProducerTaskRequestV1 {
      operation_id,
      kind: IndexProducerTaskKindV1::MutationWindow,
      publication_sequence: record.sequence,
      namespace_root_before: record.root_before,
      namespace_root_after: record.root_after,
      semantic_state_root: journal.semantic_state_root,
      journal_head: Some(&journal.key),
      scope: None,
    })?;
    match admission {
      IndexProducerAdmissionV1::Queued => increment(&mut summary.queued)?,
      IndexProducerAdmissionV1::Duplicate => increment(&mut summary.duplicates)?,
      IndexProducerAdmissionV1::Spilled { .. } => increment(&mut summary.spilled)?,
    }
  }
  Ok(summary)
}

fn validate_journal(
  hash_algorithm: HashAlgorithm,
  journal: &MutationJournalV1<'_>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<(), IndexProducerJournalAdmissionErrorV1> {
  let expected = hash_algorithm.hash_length();
  for (role, value) in [
    ("journal key", journal.key.as_slice()),
    ("journal source root before", journal.source_root_before),
    ("journal source root after", journal.source_root_after),
    ("journal semantic-state root", journal.semantic_state_root),
  ] {
    if value.len() != expected {
      return Err(IndexProducerJournalAdmissionErrorV1::HashProfileMismatch {
        algorithm: hash_algorithm,
        role,
        expected,
        actual: value.len(),
      });
    }
  }

  let mut previous_batch: Option<(u32, &[u8])> = None;
  let mut record_index = 0u32;
  for record in journal.records.iter() {
    if is_cancelled() {
      return Err(IndexProducerJournalAdmissionErrorV1::Cancelled);
    }
    let record = record?;
    validate_record_hashes(hash_algorithm, &record)?;
    let index = record_index;
    record_index = record_index.checked_add(1).ok_or(IndexProducerJournalAdmissionErrorV1::AccountingOverflow)?;
    if record.batch_ordinal != 0 {
      continue;
    }
    if let Some((previous_index, previous_root_after)) = previous_batch {
      if record.root_before != previous_root_after {
        return Err(IndexProducerJournalAdmissionErrorV1::DiscontinuousRoots { previous_record: previous_index, next_record: index });
      }
    } else if record.root_before != journal.source_root_before {
      return Err(IndexProducerJournalAdmissionErrorV1::DiscontinuousRoots { previous_record: 0, next_record: index });
    }
    previous_batch = Some((index, record.root_after));
  }
  if let Some((previous_record, root_after)) = previous_batch {
    if root_after != journal.source_root_after {
      let next_record = previous_record.checked_add(1).ok_or(IndexProducerJournalAdmissionErrorV1::AccountingOverflow)?;
      return Err(IndexProducerJournalAdmissionErrorV1::DiscontinuousRoots { previous_record, next_record });
    }
  }
  Ok(())
}

fn validate_record_hashes(
  hash_algorithm: HashAlgorithm,
  record: &MutationRecordV1<'_>,
) -> Result<(), IndexProducerJournalAdmissionErrorV1> {
  let expected = hash_algorithm.hash_length();
  for (role, value) in
    [("mutation identity", record.mutation_id), ("mutation root before", record.root_before), ("mutation root after", record.root_after)]
  {
    if value.len() != expected {
      return Err(IndexProducerJournalAdmissionErrorV1::HashProfileMismatch {
        algorithm: hash_algorithm,
        role,
        expected,
        actual: value.len(),
      });
    }
  }
  Ok(())
}

fn increment(value: &mut u32) -> Result<(), IndexProducerJournalAdmissionErrorV1> {
  *value = value.checked_add(1).ok_or(IndexProducerJournalAdmissionErrorV1::AccountingOverflow)?;
  Ok(())
}
