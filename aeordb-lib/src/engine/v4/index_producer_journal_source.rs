//! Bounded mutation-journal source contract for producer task execution.

use std::mem::size_of;

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{MemoryOwner, MemoryReservation};

use super::index_task::{MutationJournalV1, decode_mutation_journal};

pub struct IndexProducerJournalReadRequestV1<'request> {
  pub hash_algorithm: HashAlgorithm,
  pub journal_head: &'request [u8],
  pub is_cancelled: &'request dyn Fn() -> bool,
}

pub struct IndexProducerJournalReadV1 {
  encoded: Vec<u8>,
  _reservation: MemoryReservation,
}

impl std::fmt::Debug for IndexProducerJournalReadV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("IndexProducerJournalReadV1")
      .field("encoded_bytes", &self.encoded.len())
      .field("reservation_owner", &self._reservation.owner())
      .field("reserved_bytes", &self._reservation.bytes())
      .finish()
  }
}

impl IndexProducerJournalReadV1 {
  pub fn new(
    request: &IndexProducerJournalReadRequestV1<'_>,
    encoded: Vec<u8>,
    reservation: MemoryReservation,
  ) -> Result<Self, IndexProducerJournalReadErrorV1> {
    validate_request(request)?;
    if reservation.owner() != MemoryOwner::Task {
      return Err(IndexProducerJournalReadErrorV1::corrupt(
        "journal_memory_owner",
        format!("mutation journal is owned by {:?}, expected Task", reservation.owner()),
      ));
    }
    let required_bytes = size_of::<Self>()
      .checked_add(encoded.capacity())
      .ok_or_else(|| IndexProducerJournalReadErrorV1::corrupt("journal_memory_overflow", "journal retained-byte accounting overflowed"))?;
    let required_bytes = u64::try_from(required_bytes)
      .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("journal_memory_overflow", error.to_string()))?;
    if reservation.bytes() < required_bytes {
      return Err(IndexProducerJournalReadErrorV1::corrupt(
        "journal_memory_reservation",
        format!("mutation journal retains at least {required_bytes} bytes but reserves {}", reservation.bytes()),
      ));
    }
    Ok(Self { encoded, _reservation: reservation })
  }

  pub fn encoded(&self) -> &[u8] {
    &self.encoded
  }

  pub const fn reserved_bytes(&self) -> u64 {
    self._reservation.bytes()
  }

  pub fn decode_journal(
    &self,
    hash_algorithm: HashAlgorithm,
    expected_head: &[u8],
  ) -> Result<MutationJournalV1<'_>, IndexProducerJournalReadErrorV1> {
    let hash_width = hash_algorithm.hash_length();
    if expected_head.len() != hash_width || expected_head.iter().all(|byte| *byte == 0) {
      return Err(IndexProducerJournalReadErrorV1::corrupt(
        "journal_request_identity",
        "retained mutation-journal head is absent or has the wrong hash width",
      ));
    }
    let journal = decode_mutation_journal(&self.encoded, hash_algorithm)
      .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("journal_format", error.to_string()))?;
    if journal.key != expected_head {
      return Err(IndexProducerJournalReadErrorV1::corrupt(
        "journal_identity",
        format!("decoded mutation journal {} does not match retained head {}", hex::encode(journal.key), hex::encode(expected_head)),
      ));
    }
    Ok(journal)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProducerJournalReadErrorClassV1 {
  Cancelled,
  Retryable,
  Corrupt,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("index producer mutation-journal read failed ({code}): {context}")]
pub struct IndexProducerJournalReadErrorV1 {
  class: IndexProducerJournalReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexProducerJournalReadErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexProducerJournalReadErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn retryable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexProducerJournalReadErrorClassV1::Retryable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexProducerJournalReadErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexProducerJournalReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

pub trait IndexProducerJournalSourceV1: Send + Sync {
  /// Load the exact immutable journal named by a retained producer task.
  ///
  /// Implementations must reserve Task memory before retaining encoded bytes
  /// and keep that reservation inside the returned read until it is dropped.
  fn load_journal(
    &self,
    request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1>;
}

fn validate_request(request: &IndexProducerJournalReadRequestV1<'_>) -> Result<(), IndexProducerJournalReadErrorV1> {
  if (request.is_cancelled)() {
    return Err(IndexProducerJournalReadErrorV1::cancelled("journal_cancelled", "mutation-journal read was cancelled"));
  }
  let hash_width = request.hash_algorithm.hash_length();
  if request.journal_head.len() != hash_width || request.journal_head.iter().all(|byte| *byte == 0) {
    return Err(IndexProducerJournalReadErrorV1::corrupt(
      "journal_request_identity",
      "retained mutation-journal head is absent or has the wrong hash width",
    ));
  }
  Ok(())
}
