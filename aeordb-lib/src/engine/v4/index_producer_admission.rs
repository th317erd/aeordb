use thiserror::Error;

use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1, IndexProducerSpillStoreV1,
  IndexProducerTaskKindV1, IndexProducerTaskRequestV1,
};
use super::index_task::{MutationJournalV1, MutationRecordV1};
use super::reader::FormatError;

const MUTATION_OPERATION_DOMAIN_V1: &[u8] = b"aeordb:index-producer:mutation-operation:v1\0";

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

pub fn admit_mutation_journal_tasks(
  hash_algorithm: HashAlgorithm,
  producer: &mut IndexProducerCoordinatorV1,
  journal: &MutationJournalV1<'_>,
  now_ms: u64,
  is_cancelled: &dyn Fn() -> bool,
  spill_store: &mut dyn IndexProducerSpillStoreV1,
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
    let admission = producer.admit_or_spill(
      IndexProducerTaskRequestV1 {
        operation_id,
        kind: IndexProducerTaskKindV1::MutationWindow,
        publication_sequence: record.sequence,
        namespace_root_before: record.root_before,
        namespace_root_after: record.root_after,
        semantic_state_root: journal.semantic_state_root,
        journal_head: Some(&journal.key),
        scope: None,
      },
      now_ms,
      spill_store,
    )?;
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
