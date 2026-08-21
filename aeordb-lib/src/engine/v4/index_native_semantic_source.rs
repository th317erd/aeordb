//! Native v4 semantic-object and scope-ordinal source adapters.
//!
//! The source engine and v4 shadow are separate files. Semantic reads must
//! therefore use the selected shadow authority, while each scope-local ordinal
//! claim must route through the exact selected recovery operation for that
//! scope. The retained descriptor catalog preserves that routing information
//! independently of the evictable per-scope adapter cache.

use std::mem::size_of;
use std::sync::Arc;

use thiserror::Error;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::{EngineError, HashAlgorithm};

use super::first_authority::{FirstAuthorityPublicationErrorV1, V4FirstAuthorityPublisher};
use super::header_publication::DatabaseHeaderPublicationErrorV4;
use super::index_recovery_store::{IndexScopeOrdinalStoreRegistryErrorV1, IndexScopeOrdinalStoreRegistryV1, NativeIndexOperationDescriptorV1};
use super::index_scope_ordinal_authority::{DurableIndexScopeOrdinalAuthorityV1, IndexScopeOrdinalStateOptionsV1};
use super::index_semantic_source::{
  IndexScopeOrdinalAuthorityV1, IndexScopeOrdinalClaimErrorV1, IndexScopeOrdinalClaimRequestV1, IndexSemanticObjectReadSourceV1,
};
use super::index_producer_source::IndexSemanticScopeReadErrorV1;

#[derive(Debug, Error)]
pub enum NativeIndexSemanticSourceErrorV1 {
  #[error("native index semantic source construction was cancelled")]
  Cancelled,
  #[error("native index semantic source rejected {code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("native index semantic source memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
}

impl NativeIndexSemanticSourceErrorV1 {
  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

pub struct FirstAuthorityIndexSemanticObjectReadSourceV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
}

impl FirstAuthorityIndexSemanticObjectReadSourceV1 {
  pub const fn new(publisher: Arc<V4FirstAuthorityPublisher>) -> Self {
    Self { publisher }
  }
}

impl IndexSemanticObjectReadSourceV1 for FirstAuthorityIndexSemanticObjectReadSourceV1 {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, IndexSemanticScopeReadErrorV1> {
    self.publisher.load_semantic_object(kind_id, object_id).map_err(map_semantic_authority_error)
  }
}

pub struct NativeIndexOperationDescriptorCatalogV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  descriptors: Vec<NativeIndexOperationDescriptorV1>,
  retained_bytes: u64,
  _reservation: Option<MemoryReservation>,
}

impl NativeIndexOperationDescriptorCatalogV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    hash_algorithm: HashAlgorithm,
    database_id: [u8; 16],
    descriptors: &[NativeIndexOperationDescriptorV1],
    maximum_descriptors: usize,
    maximum_retained_bytes: u64,
    memory: Arc<MemoryCoordinator>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<Self, NativeIndexSemanticSourceErrorV1> {
    if is_cancelled() {
      return Err(NativeIndexSemanticSourceErrorV1::Cancelled);
    }
    if database_id.iter().all(|byte| *byte == 0) || maximum_descriptors == 0 || maximum_retained_bytes == 0 {
      return Err(NativeIndexSemanticSourceErrorV1::invalid(
        "native_scope_descriptor_options",
        "database identity and descriptor count/byte bounds must be nonzero",
      ));
    }
    if descriptors.len() > maximum_descriptors || descriptors.len() > u32::MAX as usize {
      return Err(NativeIndexSemanticSourceErrorV1::invalid(
        "native_scope_descriptor_count",
        "selected scope descriptor count exceeds the configured bound",
      ));
    }
    let mut retained_bytes = u64::try_from(size_of::<Self>()).map_err(|error| {
      NativeIndexSemanticSourceErrorV1::invalid("native_scope_descriptor_size", format!("catalog fixed size exceeds u64: {error}"))
    })?;
    for descriptor in descriptors {
      if is_cancelled() {
        return Err(NativeIndexSemanticSourceErrorV1::Cancelled);
      }
      if descriptor.hash_algorithm() != hash_algorithm || descriptor.database_id() != database_id {
        return Err(NativeIndexSemanticSourceErrorV1::invalid(
          "native_scope_descriptor_authority",
          "selected scope descriptor belongs to another shadow authority",
        ));
      }
      retained_bytes = retained_bytes
        .checked_add(
          descriptor
            .retained_identity_bytes()
            .map_err(|error| NativeIndexSemanticSourceErrorV1::invalid("native_scope_descriptor_size", error.to_string()))?,
        )
        .ok_or_else(|| {
          NativeIndexSemanticSourceErrorV1::invalid("native_scope_descriptor_size", "selected scope descriptor bytes overflowed")
        })?;
      if retained_bytes > maximum_retained_bytes {
        return Err(NativeIndexSemanticSourceErrorV1::invalid(
          "native_scope_descriptor_bytes",
          "selected scope descriptor bytes exceed the configured bound",
        ));
      }
    }
    let reservation =
      if retained_bytes == 0 { None } else { Some(memory.reserve(MemoryOwner::IndexCleanCache, retained_bytes, AdmissionClass::Cache)?) };
    let mut owned = Vec::new();
    owned.try_reserve_exact(descriptors.len()).map_err(|error| {
      NativeIndexSemanticSourceErrorV1::invalid(
        "native_scope_descriptor_allocation",
        format!("descriptor catalog allocation failed: {error}"),
      )
    })?;
    for descriptor in descriptors {
      if is_cancelled() {
        return Err(NativeIndexSemanticSourceErrorV1::Cancelled);
      }
      owned.push(
        descriptor
          .try_clone_retained()
          .map_err(|error| NativeIndexSemanticSourceErrorV1::invalid("native_scope_descriptor_allocation", error.to_string()))?,
      );
    }
    owned
      .sort_unstable_by(|left, right| left.index_id().cmp(right.index_id()).then_with(|| left.operation_id().cmp(&right.operation_id())));
    if owned.windows(2).any(|adjacent| adjacent[0].index_id() == adjacent[1].index_id()) {
      return Err(NativeIndexSemanticSourceErrorV1::invalid(
        "native_scope_descriptor_duplicate",
        "selected recovery contains more than one operation descriptor for one index scope",
      ));
    }
    Ok(Self { hash_algorithm, database_id, descriptors: owned, retained_bytes, _reservation: reservation })
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub fn len(&self) -> usize {
    self.descriptors.len()
  }

  pub fn is_empty(&self) -> bool {
    self.descriptors.is_empty()
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }

  pub fn descriptors(&self) -> &[NativeIndexOperationDescriptorV1] {
    &self.descriptors
  }

  pub fn descriptor(&self, scope_id: &[u8]) -> Option<&NativeIndexOperationDescriptorV1> {
    let index = self.descriptors.partition_point(|descriptor| descriptor.index_id() < scope_id);
    match self.descriptors.get(index) {
      Some(descriptor) if descriptor.index_id() == scope_id => Some(descriptor),
      Some(_) | None => None,
    }
  }
}

pub struct NativeIndexScopeOrdinalAuthorityV1 {
  hash_algorithm: HashAlgorithm,
  catalog: Arc<NativeIndexOperationDescriptorCatalogV1>,
  registry: Arc<IndexScopeOrdinalStoreRegistryV1>,
  options: IndexScopeOrdinalStateOptionsV1,
}

impl NativeIndexScopeOrdinalAuthorityV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    catalog: Arc<NativeIndexOperationDescriptorCatalogV1>,
    registry: Arc<IndexScopeOrdinalStoreRegistryV1>,
    options: IndexScopeOrdinalStateOptionsV1,
  ) -> Result<Self, NativeIndexSemanticSourceErrorV1> {
    if catalog.hash_algorithm() != hash_algorithm
      || registry.hash_algorithm() != hash_algorithm
      || catalog.database_id() != registry.database_id()
    {
      return Err(NativeIndexSemanticSourceErrorV1::invalid(
        "native_scope_ordinal_authority",
        "scope descriptor catalog and adapter registry must share one database authority",
      ));
    }
    Ok(Self { hash_algorithm, catalog, registry, options })
  }

  pub fn registry(&self) -> &Arc<IndexScopeOrdinalStoreRegistryV1> {
    &self.registry
  }
}

impl IndexScopeOrdinalAuthorityV1 for NativeIndexScopeOrdinalAuthorityV1 {
  fn claim_scope_ordinal(&self, request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    if (request.is_cancelled)() {
      return Err(IndexScopeOrdinalClaimErrorV1::cancelled(
        "native_scope_cancelled",
        "scope ordinal routing was cancelled before descriptor selection",
      ));
    }
    let descriptor = self.catalog.descriptor(request.scope_id).ok_or_else(|| {
      IndexScopeOrdinalClaimErrorV1::corrupt(
        "native_scope_descriptor_missing",
        format!("selected runtime has no operation descriptor for scope {}", hex::encode(request.scope_id)),
      )
    })?;
    let descriptor = descriptor
      .try_clone_retained()
      .map_err(|error| IndexScopeOrdinalClaimErrorV1::retryable("native_scope_descriptor_allocation", error.to_string()))?;
    let store = self.registry.acquire(descriptor).map_err(map_registry_error)?;
    DurableIndexScopeOrdinalAuthorityV1::new(self.hash_algorithm, store.as_ref(), self.options).claim_scope_ordinal(request)
  }
}

fn map_registry_error(error: IndexScopeOrdinalStoreRegistryErrorV1) -> IndexScopeOrdinalClaimErrorV1 {
  match error {
    IndexScopeOrdinalStoreRegistryErrorV1::Canceled => {
      IndexScopeOrdinalClaimErrorV1::cancelled("native_scope_registry_cancelled", "scope ordinal adapter registry was cancelled")
    }
    error @ (IndexScopeOrdinalStoreRegistryErrorV1::AllCandidatesPinned | IndexScopeOrdinalStoreRegistryErrorV1::Memory(_)) => {
      IndexScopeOrdinalClaimErrorV1::retryable("native_scope_registry_pressure", error.to_string())
    }
    IndexScopeOrdinalStoreRegistryErrorV1::Store(error) => {
      IndexScopeOrdinalClaimErrorV1::retryable("native_scope_registry_store", error.to_string())
    }
    error => IndexScopeOrdinalClaimErrorV1::corrupt("native_scope_registry_invalid", error.to_string()),
  }
}

fn map_semantic_authority_error(error: FirstAuthorityPublicationErrorV1) -> IndexSemanticScopeReadErrorV1 {
  match error {
    FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled(context)) => {
      IndexSemanticScopeReadErrorV1::cancelled("native_semantic_cancelled", context)
    }
    FirstAuthorityPublicationErrorV1::Engine(
      error @ (EngineError::IoError(_) | EngineError::ResourceExhausted(_) | EngineError::ShuttingDown),
    ) => IndexSemanticScopeReadErrorV1::retryable("native_semantic_authority_unavailable", error.to_string()),
    FirstAuthorityPublicationErrorV1::Header(
      error @ (DatabaseHeaderPublicationErrorV4::Native(_) | DatabaseHeaderPublicationErrorV4::Durability(_)),
    ) => IndexSemanticScopeReadErrorV1::retryable("native_semantic_authority_unavailable", error.to_string()),
    FirstAuthorityPublicationErrorV1::Invalid { code, message }
      if code == "first_authority_readback_io" || code.ends_with("_allocation") =>
    {
      IndexSemanticScopeReadErrorV1::retryable(code, message)
    }
    error => IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.to_string()),
  }
}
