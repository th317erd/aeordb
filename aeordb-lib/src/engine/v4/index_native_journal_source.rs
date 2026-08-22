//! Bounded first-authority mutation-journal reads for the native v4 runtime.

use std::mem::size_of;
use std::sync::Arc;

use crate::engine::errors::EngineError;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner};
use crate::engine::HashAlgorithm;

use super::first_authority::{FirstAuthorityPublicationErrorV1, V4FirstAuthorityPublisher};
use super::entity::checked_whole_entity_encoded_length;
use super::header_publication::DatabaseHeaderPublicationErrorV4;
use super::index_artifact::ImmutableIndexArtifactKindV1;
use super::index_producer_journal_source::{
  IndexProducerJournalReadErrorV1, IndexProducerJournalReadRequestV1, IndexProducerJournalReadV1, IndexProducerJournalSourceV1,
};

pub struct FirstAuthorityIndexProducerJournalSourceV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
  memory: Arc<MemoryCoordinator>,
  hash_algorithm: HashAlgorithm,
}

impl FirstAuthorityIndexProducerJournalSourceV1 {
  pub const fn new(publisher: Arc<V4FirstAuthorityPublisher>, memory: Arc<MemoryCoordinator>, hash_algorithm: HashAlgorithm) -> Self {
    Self { publisher, memory, hash_algorithm }
  }
}

impl IndexProducerJournalSourceV1 for FirstAuthorityIndexProducerJournalSourceV1 {
  fn load_journal(
    &self,
    request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1> {
    validate_request(self.hash_algorithm, &request)?;
    let expected_length = self.publisher.index_artifact_length(request.journal_head).map_err(map_authority_error)?.ok_or_else(|| {
      IndexProducerJournalReadErrorV1::corrupt(
        "native_journal_missing",
        format!("retained mutation journal {} is absent from first authority", hex::encode(request.journal_head)),
      )
    })?;
    check_cancelled(request.is_cancelled, "native_journal_cancelled_after_probe")?;

    let maximum_value_bytes = ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length();
    let maximum_value_bytes = u64::try_from(maximum_value_bytes)
      .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("native_journal_limit", error.to_string()))?;
    if expected_length == 0 || expected_length > maximum_value_bytes {
      return Err(IndexProducerJournalReadErrorV1::corrupt(
        "native_journal_length",
        format!("retained mutation journal length {expected_length} is outside the frozen limit {maximum_value_bytes}"),
      ));
    }
    // First-authority loading verifies the complete WholeEntity before moving
    // its value to the front of the same allocation. Account for that retained
    // envelope capacity, not only the decoded artifact-value ceiling.
    let maximum_allocation_bytes = checked_whole_entity_encoded_length(
      self.hash_algorithm,
      self.hash_algorithm.hash_length(),
      ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length(),
    )
    .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_limit", error.to_string()))?;
    let maximum_retained_bytes = u64::try_from(size_of::<IndexProducerJournalReadV1>())
      .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_overflow", error.to_string()))?
      .checked_add(
        u64::try_from(maximum_allocation_bytes)
          .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_overflow", error.to_string()))?,
      )
      .ok_or_else(|| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_overflow", "journal reservation overflowed"))?;
    let mut reservation =
      self.memory.reserve(MemoryOwner::Task, maximum_retained_bytes, AdmissionClass::Maintenance).map_err(map_memory_admission_error)?;
    check_cancelled(request.is_cancelled, "native_journal_cancelled_after_admission")?;

    let encoded =
      self.publisher.load_index_artifact(request.journal_head, expected_length).map_err(map_authority_error)?.ok_or_else(|| {
        IndexProducerJournalReadErrorV1::corrupt(
          "native_journal_changed",
          format!("retained mutation journal {} changed after its length probe", hex::encode(request.journal_head)),
        )
      })?;
    check_cancelled(request.is_cancelled, "native_journal_cancelled_after_read")?;
    let exact_retained_bytes = u64::try_from(size_of::<IndexProducerJournalReadV1>())
      .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_overflow", error.to_string()))?
      .checked_add(
        u64::try_from(encoded.capacity())
          .map_err(|error| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_overflow", error.to_string()))?,
      )
      .ok_or_else(|| IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_overflow", "journal retained bytes overflowed"))?;
    let release_bytes = reservation.bytes().checked_sub(exact_retained_bytes).ok_or_else(|| {
      IndexProducerJournalReadErrorV1::corrupt(
        "native_journal_memory_accounting",
        "loaded journal retains more bytes than the frozen pre-read reservation",
      )
    })?;
    reservation.shrink(release_bytes).map_err(map_memory_invariant_error)?;
    IndexProducerJournalReadV1::new(&request, encoded, reservation)
  }
}

fn validate_request(
  expected_algorithm: HashAlgorithm,
  request: &IndexProducerJournalReadRequestV1<'_>,
) -> Result<(), IndexProducerJournalReadErrorV1> {
  check_cancelled(request.is_cancelled, "native_journal_cancelled")?;
  if request.hash_algorithm != expected_algorithm
    || request.journal_head.len() != expected_algorithm.hash_length()
    || request.journal_head.iter().all(|byte| *byte == 0)
  {
    return Err(IndexProducerJournalReadErrorV1::corrupt(
      "native_journal_authority",
      "journal request does not match the installed first-authority hash profile",
    ));
  }
  Ok(())
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool, code: &'static str) -> Result<(), IndexProducerJournalReadErrorV1> {
  if is_cancelled() {
    Err(IndexProducerJournalReadErrorV1::cancelled(code, "first-authority mutation-journal read was cancelled"))
  } else {
    Ok(())
  }
}

fn map_memory_admission_error(error: MemoryCoordinatorError) -> IndexProducerJournalReadErrorV1 {
  IndexProducerJournalReadErrorV1::retryable("native_journal_memory_pressure", error.to_string())
}

fn map_memory_invariant_error(error: MemoryCoordinatorError) -> IndexProducerJournalReadErrorV1 {
  IndexProducerJournalReadErrorV1::corrupt("native_journal_memory_accounting", error.to_string())
}

fn map_authority_error(error: FirstAuthorityPublicationErrorV1) -> IndexProducerJournalReadErrorV1 {
  match error {
    FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled(context)) => {
      IndexProducerJournalReadErrorV1::cancelled("native_journal_cancelled", context)
    }
    FirstAuthorityPublicationErrorV1::Engine(
      error @ (EngineError::IoError(_) | EngineError::ResourceExhausted(_) | EngineError::ShuttingDown),
    ) => IndexProducerJournalReadErrorV1::retryable("native_journal_authority_unavailable", error.to_string()),
    FirstAuthorityPublicationErrorV1::Header(
      error @ (DatabaseHeaderPublicationErrorV4::Native(_) | DatabaseHeaderPublicationErrorV4::Durability(_)),
    ) => IndexProducerJournalReadErrorV1::retryable("native_journal_authority_unavailable", error.to_string()),
    FirstAuthorityPublicationErrorV1::Invalid { code, message }
      if code == "first_authority_readback_io" || code.ends_with("_allocation") =>
    {
      IndexProducerJournalReadErrorV1::retryable(code, message)
    }
    error => IndexProducerJournalReadErrorV1::corrupt(error.code(), error.to_string()),
  }
}
