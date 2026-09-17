//! Native staging adapter; the physical publisher remains the only writer.
use tokio_util::sync::CancellationToken;

use crate::engine::EngineError;

use super::first_authority::{
  FirstAuthorityPublicationErrorV1, ImmutableEntityBatchPublicationErrorV1, ImmutableSemanticObjectBatchPublicationRequestV1,
  NativeStagingProtectionV1, V4FirstAuthorityPublisher,
};
use super::header_publication::DatabaseHeaderPublicationErrorV4;
use super::namespace::EncodedSemanticObjectV1;
use super::semantic_catalog::{SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1};
use super::semantic_catalog_compiler::SemanticCatalogStagingStoreV1;

type Result<T> = std::result::Result<T, SemanticCatalogReadErrorV1>;

/// Borrow the existing physical owner for bounded, dependency-first staging.
/// This adapter never changes HEAD, acquires a namespace guard, or creates a
/// second KV/file owner. It borrows active in-process staging protection for
/// its entire usable lifetime. The caller must retain that guard after this
/// adapter drops until checkpoint selection or safe discard. Neither this
/// adapter nor a compiler result creates a durable task pin. Compiler scratch
/// and physical KV accounting stay with their respective owners.
pub struct NativeSemanticCatalogStagingStoreV1<'a> {
  publisher: &'a V4FirstAuthorityPublisher,
  _protection: &'a NativeStagingProtectionV1<'a>,
  database_id: [u8; 16],
  publication_timestamp_ms: u64,
  cancellation: &'a CancellationToken,
}

impl<'a> NativeSemanticCatalogStagingStoreV1<'a> {
  pub fn new(
    protection: &'a NativeStagingProtectionV1<'a>,
    database_id: [u8; 16],
    publication_timestamp_ms: u64,
    cancellation: &'a CancellationToken,
  ) -> Result<Self> {
    check_cancelled(cancellation)?;
    if publication_timestamp_ms == 0 || publication_timestamp_ms > i64::MAX as u64 {
      return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_publication_time", "invalid staging timestamp"));
    }
    let publisher = protection.publisher();
    let observation = publisher.observe().map_err(authority_error)?;
    if observation.selected.header.database_id != database_id {
      return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_database", "staging belongs to another logical database"));
    }
    check_cancelled(cancellation)?;
    Ok(Self { publisher, _protection: protection, database_id, publication_timestamp_ms, cancellation })
  }
}

impl SemanticCatalogObjectSourceV1 for NativeSemanticCatalogStagingStoreV1<'_> {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>> {
    check_cancelled(self.cancellation)?;
    // Capture the current physical frontier to include earlier staging batches.
    // Kind-specific caps and canonical file/chunk/semantic identity are checked
    // by the physical owner before a body allocation reaches this adapter.
    let captured = self.publisher.observe().map_err(authority_error)?.selected;
    self.publisher.load_semantic_object_at_captured_header(&captured, kind_id, object_id, self.cancellation).map_err(authority_error)
  }
}

impl SemanticCatalogStagingStoreV1 for NativeSemanticCatalogStagingStoreV1<'_> {
  fn publish_semantic_objects(&mut self, objects: &[EncodedSemanticObjectV1]) -> Result<()> {
    check_cancelled(self.cancellation)?;
    self
      .publisher
      .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
        database_id: &self.database_id,
        objects,
        publication_timestamp_ms: self.publication_timestamp_ms,
      })
      .map_err(publication_error)?;
    // Cancellation after a durable batch leaves unselected objects; it must
    // not return a completed catalog or undo successful physical publication.
    check_cancelled(self.cancellation)
  }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<()> {
  if cancellation.is_cancelled() {
    return Err(SemanticCatalogReadErrorV1::cancelled("semantic_catalog_cancelled", "native staging was cancelled"));
  }
  Ok(())
}

fn authority_error(error: FirstAuthorityPublicationErrorV1) -> SemanticCatalogReadErrorV1 {
  let code = error.code();
  if let FirstAuthorityPublicationErrorV1::Engine(source) = &error {
    return match source {
      EngineError::ResourceExhausted(_) => SemanticCatalogReadErrorV1::resource(code, error.to_string()),
      EngineError::Cancelled(_) => SemanticCatalogReadErrorV1::cancelled(code, error.to_string()),
      EngineError::IoError(_)
      | EngineError::DurabilityFailure(_)
      | EngineError::PostMutationDurabilityFailure(_)
      | EngineError::ShuttingDown => SemanticCatalogReadErrorV1::unavailable(code, error.to_string()),
      _ => SemanticCatalogReadErrorV1::corrupt(code, error.to_string()),
    };
  }
  if matches!(&error, FirstAuthorityPublicationErrorV1::Format(source) if source.is_allocation_failure())
    || matches!(&error, FirstAuthorityPublicationErrorV1::Header(DatabaseHeaderPublicationErrorV4::Format(source)) if source.is_allocation_failure())
    || matches!(
      code,
      "first_authority_readback_allocation" | "first_authority_system_file_allocation" | "immutable_semantic_object_allocation"
    )
  {
    return SemanticCatalogReadErrorV1::resource(code, error.to_string());
  }
  if code == "captured_authority_cancelled" {
    return SemanticCatalogReadErrorV1::cancelled(code, error.to_string());
  }
  if matches!(&error, FirstAuthorityPublicationErrorV1::StateLockPoisoned | FirstAuthorityPublicationErrorV1::Committed { .. })
    || matches!(code, "native_io_failure" | "durability_failure" | "publication_lock_poisoned" | "first_authority_readback_io")
  {
    return SemanticCatalogReadErrorV1::unavailable(code, error.to_string());
  }
  SemanticCatalogReadErrorV1::corrupt(code, error.to_string())
}

fn publication_error(error: ImmutableEntityBatchPublicationErrorV1) -> SemanticCatalogReadErrorV1 {
  match error {
    ImmutableEntityBatchPublicationErrorV1::Authority(error) => authority_error(error),
    ImmutableEntityBatchPublicationErrorV1::Committed { code, message, .. } => SemanticCatalogReadErrorV1::unavailable(code, message),
    ImmutableEntityBatchPublicationErrorV1::Invalid { code, message } => {
      if matches!(
        code,
        "immutable_semantic_object_allocation"
          | "immutable_entity_batch_allocation"
          | "immutable_entity_receipt_allocation"
          | "immutable_entity_allocation"
          | "immutable_entity_expectation_allocation"
      ) {
        SemanticCatalogReadErrorV1::resource(code, message)
      } else {
        SemanticCatalogReadErrorV1::corrupt(code, message)
      }
    }
  }
}

#[cfg(test)]
#[path = "../../../spec/engine/semantic_catalog_native_error_spec.rs"]
mod error_tests;
