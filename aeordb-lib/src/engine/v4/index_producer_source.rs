use std::collections::BTreeSet;
use std::mem::size_of;

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{MemoryOwner, MemoryReservation};

use super::hash::digest_parts;
use super::index_producer_admission::{IndexProducerJournalAdmissionErrorV1, derive_mutation_operation_id};
use super::field_definition::decode_field_index_definition;
use super::index_producer_collector::{
  IndexCollectorDocumentRevisionTransitionV1, IndexCollectorDocumentV1, IndexCollectorFieldDefinitionV1, IndexCollectorScopeDefinitionV1,
  IndexCollectorScopeWorkV1, IndexCollectorValueStoreDefinitionV1,
};
use super::index_producer_coordinator::{
  IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1, IndexProducerLeaseV1, IndexProducerTaskKindV1,
};
use super::index_task::{MutationJournalV1, MutationRecordV1};
use super::reader::FormatError;
use super::scope::decode_scope_definition;
use super::value_store::decode_value_store_definition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexFileRevisionReadErrorClassV1 {
  Cancelled,
  Retryable,
  Corrupt,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("index file revision read failed ({code}): {context}")]
pub struct IndexFileRevisionReadErrorV1 {
  class: IndexFileRevisionReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexFileRevisionReadErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexFileRevisionReadErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn retryable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexFileRevisionReadErrorClassV1::Retryable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexFileRevisionReadErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexFileRevisionReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedIndexFileRevisionV1 {
  pub revision_hash: Vec<u8>,
  pub file_record: FileRecord,
}

pub struct IndexFileRevisionReadV1 {
  revision: LoadedIndexFileRevisionV1,
  reservation: MemoryReservation,
}

impl std::fmt::Debug for IndexFileRevisionReadV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("IndexFileRevisionReadV1")
      .field("revision", &self.revision)
      .field("reservation_owner", &self.reservation.owner())
      .field("reserved_bytes", &self.reservation.bytes())
      .finish()
  }
}

impl IndexFileRevisionReadV1 {
  pub fn new(revision: LoadedIndexFileRevisionV1, reservation: MemoryReservation) -> Result<Self, IndexFileRevisionReadErrorV1> {
    if reservation.owner() != MemoryOwner::Task {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "revision_memory_owner",
        format!("file revision is owned by {:?}, expected Task", reservation.owner()),
      ));
    }
    let required_bytes = file_revision_retained_bytes(&revision)?;
    if reservation.bytes() < required_bytes {
      return Err(IndexFileRevisionReadErrorV1::corrupt(
        "revision_memory_reservation",
        format!("file revision retains at least {required_bytes} bytes but reserves {}", reservation.bytes()),
      ));
    }
    Ok(Self { revision, reservation })
  }

  pub const fn revision(&self) -> &LoadedIndexFileRevisionV1 {
    &self.revision
  }

  pub const fn reserved_bytes(&self) -> u64 {
    self.reservation.bytes()
  }

  pub fn into_parts(self) -> (LoadedIndexFileRevisionV1, MemoryReservation) {
    (self.revision, self.reservation)
  }
}

fn file_revision_retained_bytes(revision: &LoadedIndexFileRevisionV1) -> Result<u64, IndexFileRevisionReadErrorV1> {
  let record = &revision.file_record;
  let mut retained = size_of::<LoadedIndexFileRevisionV1>();
  retained = add_revision_retained_bytes(retained, revision.revision_hash.capacity(), "revision hash")?;
  retained = add_revision_retained_bytes(retained, record.path.capacity(), "file path")?;
  retained = add_revision_retained_bytes(retained, record.content_type.as_ref().map_or(0, String::capacity), "content type")?;
  retained = add_revision_retained_bytes(retained, record.metadata.capacity(), "metadata")?;
  retained = add_revision_retained_bytes(retained, record.content_hash.capacity(), "content hash")?;
  retained = add_revision_retained_bytes(
    retained,
    record
      .chunk_hashes
      .capacity()
      .checked_mul(size_of::<Vec<u8>>())
      .ok_or_else(|| IndexFileRevisionReadErrorV1::corrupt("revision_memory_overflow", "chunk-vector accounting overflowed"))?,
    "chunk vector",
  )?;
  for chunk_hash in &record.chunk_hashes {
    retained = add_revision_retained_bytes(retained, chunk_hash.capacity(), "chunk hash")?;
  }
  u64::try_from(retained).map_err(|error| {
    IndexFileRevisionReadErrorV1::corrupt("revision_memory_overflow", format!("retained revision bytes exceed u64: {error}"))
  })
}

fn add_revision_retained_bytes(retained: usize, additional: usize, resource: &'static str) -> Result<usize, IndexFileRevisionReadErrorV1> {
  retained.checked_add(additional).ok_or_else(|| {
    IndexFileRevisionReadErrorV1::corrupt("revision_memory_overflow", format!("retained {resource} byte accounting overflowed"))
  })
}

pub trait IndexFileRevisionSourceV1: Send + Sync {
  fn load_file_revision(&self, namespace_root: &[u8], path: &str) -> Result<Option<IndexFileRevisionReadV1>, IndexFileRevisionReadErrorV1>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedIndexDocumentV1 {
  pub namespace_root: Vec<u8>,
  pub revision_hash: Vec<u8>,
  pub file_record: FileRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedIndexDocumentTransitionV1 {
  pub before: Option<ResolvedIndexDocumentV1>,
  pub after: Option<ResolvedIndexDocumentV1>,
}

impl ResolvedIndexDocumentTransitionV1 {
  pub fn as_collector_transition(&self) -> IndexCollectorDocumentRevisionTransitionV1<'_> {
    IndexCollectorDocumentRevisionTransitionV1 {
      before: self.before.as_ref().map(|document| IndexCollectorDocumentV1 {
        namespace_root: &document.namespace_root,
        record_revision_hash: &document.revision_hash,
        file_record: &document.file_record,
      }),
      after: self.after.as_ref().map(|document| IndexCollectorDocumentV1 {
        namespace_root: &document.namespace_root,
        record_revision_hash: &document.revision_hash,
        file_record: &document.file_record,
      }),
    }
  }
}

pub struct ResolvedIndexDocumentTransitionReadV1 {
  transition: ResolvedIndexDocumentTransitionV1,
  _before_reservation: Option<MemoryReservation>,
  _after_reservation: Option<MemoryReservation>,
}

impl std::fmt::Debug for ResolvedIndexDocumentTransitionReadV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.debug_struct("ResolvedIndexDocumentTransitionReadV1").field("transition", &self.transition).finish_non_exhaustive()
  }
}

impl ResolvedIndexDocumentTransitionReadV1 {
  pub const fn transition(&self) -> &ResolvedIndexDocumentTransitionV1 {
    &self.transition
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSemanticScopeLimitsV1 {
  max_scopes: u32,
  max_value_stores: u32,
  max_field_indexes: u32,
  max_definition_bytes: u64,
}

impl IndexSemanticScopeLimitsV1 {
  pub fn new(
    max_scopes: u32,
    max_value_stores: u32,
    max_field_indexes: u32,
    max_definition_bytes: u64,
  ) -> Result<Self, IndexProducerSourceErrorV1> {
    if max_scopes == 0 || max_value_stores == 0 || max_field_indexes == 0 || max_definition_bytes == 0 {
      return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(
        "all semantic scope resolution limits must be nonzero".to_string(),
      ));
    }
    Ok(Self { max_scopes, max_value_stores, max_field_indexes, max_definition_bytes })
  }

  pub const fn max_scopes(self) -> u32 {
    self.max_scopes
  }

  pub const fn max_value_stores(self) -> u32 {
    self.max_value_stores
  }

  pub const fn max_field_indexes(self) -> u32 {
    self.max_field_indexes
  }

  pub const fn max_definition_bytes(self) -> u64 {
    self.max_definition_bytes
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSemanticScopeReadErrorClassV1 {
  Cancelled,
  Retryable,
  Corrupt,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("index semantic scope read failed ({code}): {context}")]
pub struct IndexSemanticScopeReadErrorV1 {
  class: IndexSemanticScopeReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexSemanticScopeReadErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexSemanticScopeReadErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn retryable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexSemanticScopeReadErrorClassV1::Retryable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexSemanticScopeReadErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexSemanticScopeReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedIndexFieldDefinitionV1 {
  pub index_id: Vec<u8>,
  pub encoded_definition: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedIndexValueStoreDefinitionV1 {
  pub value_store_id: Vec<u8>,
  pub encoded_definition: Vec<u8>,
  pub field_indexes: Vec<OwnedIndexFieldDefinitionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedIndexScopeDefinitionV1 {
  pub scope_id: Vec<u8>,
  pub encoded_definition: Vec<u8>,
  pub value_stores: Vec<OwnedIndexValueStoreDefinitionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIndexScopeWorkV1 {
  pub semantic_state_root: Vec<u8>,
  pub document_ordinal: u64,
  pub scope: OwnedIndexScopeDefinitionV1,
}

impl ResolvedIndexScopeWorkV1 {
  pub fn as_collector_scope_work(&self) -> IndexCollectorScopeWorkV1<'_> {
    IndexCollectorScopeWorkV1 {
      document_ordinal: self.document_ordinal,
      scope_bundle: IndexCollectorScopeDefinitionV1 {
        expected_scope_id: &self.scope.scope_id,
        encoded_definition: &self.scope.encoded_definition,
        value_stores: self
          .scope
          .value_stores
          .iter()
          .map(|value_store| IndexCollectorValueStoreDefinitionV1 {
            expected_value_store_id: &value_store.value_store_id,
            encoded_definition: &value_store.encoded_definition,
            field_indexes: value_store
              .field_indexes
              .iter()
              .map(|field| IndexCollectorFieldDefinitionV1 {
                expected_index_id: &field.index_id,
                encoded_definition: &field.encoded_definition,
              })
              .collect(),
          })
          .collect(),
      },
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexSemanticScopeResolutionV1 {
  Complete { semantic_state_root: Vec<u8>, scope_work: Vec<ResolvedIndexScopeWorkV1> },
  ContentOnly { semantic_state_root: Vec<u8> },
}

impl IndexSemanticScopeResolutionV1 {
  fn semantic_state_root(&self) -> &[u8] {
    match self {
      Self::Complete { semantic_state_root, .. } | Self::ContentOnly { semantic_state_root } => semantic_state_root,
    }
  }
}

pub struct IndexSemanticScopeReadV1 {
  resolution: IndexSemanticScopeResolutionV1,
  reservation: MemoryReservation,
}

impl std::fmt::Debug for IndexSemanticScopeReadV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("IndexSemanticScopeReadV1")
      .field("resolution", &self.resolution)
      .field("reservation_owner", &self.reservation.owner())
      .field("reserved_bytes", &self.reservation.bytes())
      .finish()
  }
}

impl IndexSemanticScopeReadV1 {
  pub fn new(resolution: IndexSemanticScopeResolutionV1, reservation: MemoryReservation) -> Result<Self, IndexSemanticScopeReadErrorV1> {
    if reservation.owner() != MemoryOwner::Task {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_memory_owner",
        format!("semantic scope resolution is owned by {:?}, expected Task", reservation.owner()),
      ));
    }
    let required_bytes = semantic_resolution_retained_bytes(&resolution)?;
    if reservation.bytes() < required_bytes {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_memory_reservation",
        format!("semantic scope resolution retains at least {required_bytes} bytes but reserves {}", reservation.bytes()),
      ));
    }
    Ok(Self { resolution, reservation })
  }

  pub fn resolution(&self) -> &IndexSemanticScopeResolutionV1 {
    &self.resolution
  }

  pub const fn reserved_bytes(&self) -> u64 {
    self.reservation.bytes()
  }

  pub fn into_parts(self) -> (IndexSemanticScopeResolutionV1, MemoryReservation) {
    (self.resolution, self.reservation)
  }
}

pub trait IndexSemanticScopeSourceV1: Send + Sync {
  fn resolve_scopes(&self, request: IndexSemanticScopeReadRequestV1<'_>)
    -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1>;
}

#[derive(Clone, Copy)]
pub struct IndexSemanticScopeReadRequestV1<'request> {
  pub operation_id: [u8; 16],
  pub source_publication_sequence: u64,
  pub semantic_state_root: &'request [u8],
  pub transition: &'request ResolvedIndexDocumentTransitionV1,
  pub limits: IndexSemanticScopeLimitsV1,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerSourceErrorV1 {
  #[error("index producer source resolution was cancelled")]
  Cancelled,
  #[error("index producer leased task does not match source evidence: {0}")]
  TaskMismatch(String),
  #[error("index producer journal record decoding failed: {0}")]
  Format(#[from] FormatError),
  #[error("index producer operation identity derivation failed: {0}")]
  OperationIdentity(#[from] IndexProducerJournalAdmissionErrorV1),
  #[error("index producer coordinator rejected the lease: {0}")]
  Coordinator(#[from] IndexProducerCoordinatorErrorV1),
  #[error("index producer journal has no source record for operation {operation_id:?}")]
  MissingMutationRecord { operation_id: [u8; 16] },
  #[error("index producer journal has multiple source records for operation {operation_id:?}")]
  AmbiguousMutationRecord { operation_id: [u8; 16] },
  #[error("index producer source revision read failed: {0}")]
  RevisionRead(#[from] IndexFileRevisionReadErrorV1),
  #[error("index producer source revision is missing at root {root} path {path}")]
  MissingRevision { root: String, path: String },
  #[error("index producer source revision mismatch at {path}: expected {expected}, received {actual}")]
  RevisionMismatch { path: String, expected: String, actual: String },
  #[error("index producer source FileRecord path mismatch: expected {expected}, received {actual}")]
  PathMismatch { expected: String, actual: String },
  #[error("index producer source FileKey mismatch at {path}")]
  FileKeyMismatch { path: String },
  #[error("index producer semantic source read failed: {0}")]
  SemanticRead(#[from] IndexSemanticScopeReadErrorV1),
  #[error("index producer semantic root mismatch: expected {expected}, received {actual}")]
  SemanticRootMismatch { expected: String, actual: String },
  #[error("index producer semantic source repeated owner {owner}")]
  DuplicateSemanticOwner { owner: String },
  #[error("index producer semantic scope {scope} has invalid document ordinal {ordinal}")]
  InvalidDocumentOrdinal { scope: String, ordinal: u64 },
  #[error("index producer semantic {resource} exceeds limit {limit}: {actual}")]
  SemanticLimitExceeded { resource: &'static str, limit: u64, actual: u64 },
  #[error("index producer semantic source is invalid: {0}")]
  InvalidSemanticResolution(String),
}

pub fn resolve_semantic_scope_work(
  hash_algorithm: HashAlgorithm,
  operation_id: [u8; 16],
  source_publication_sequence: u64,
  semantic_state_root: &[u8],
  transition: &ResolvedIndexDocumentTransitionV1,
  source: &dyn IndexSemanticScopeSourceV1,
  limits: IndexSemanticScopeLimitsV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<IndexSemanticScopeReadV1, IndexProducerSourceErrorV1> {
  if is_cancelled() {
    return Err(IndexProducerSourceErrorV1::Cancelled);
  }
  if transition.before.is_none() && transition.after.is_none() {
    return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(
      "document transition has neither a before nor after revision".to_string(),
    ));
  }
  if semantic_state_root.len() != hash_algorithm.hash_length() || semantic_state_root.iter().all(|byte| *byte == 0) {
    return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(
      "semantic-state root is zero or does not match the database hash width".to_string(),
    ));
  }
  if source_publication_sequence == 0 {
    return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution("source publication sequence is zero".to_string()));
  }
  let resolution = match source.resolve_scopes(IndexSemanticScopeReadRequestV1 {
    operation_id,
    source_publication_sequence,
    semantic_state_root,
    transition,
    limits,
    is_cancelled,
  }) {
    Ok(resolution) => resolution,
    Err(error) if error.class() == IndexSemanticScopeReadErrorClassV1::Cancelled => {
      return Err(IndexProducerSourceErrorV1::Cancelled);
    }
    Err(error) => return Err(IndexProducerSourceErrorV1::SemanticRead(error)),
  };
  if is_cancelled() {
    return Err(IndexProducerSourceErrorV1::Cancelled);
  }
  validate_semantic_root(semantic_state_root, resolution.resolution().semantic_state_root())?;
  let IndexSemanticScopeResolutionV1::Complete { scope_work, .. } = resolution.resolution() else {
    return Ok(resolution);
  };
  validate_scope_work(hash_algorithm, semantic_state_root, scope_work, limits, is_cancelled)?;
  Ok(resolution)
}

fn semantic_resolution_retained_bytes(resolution: &IndexSemanticScopeResolutionV1) -> Result<u64, IndexSemanticScopeReadErrorV1> {
  let mut retained = usize_to_semantic_bytes(resolution.semantic_state_root().len(), "semantic-state root")?;
  let IndexSemanticScopeResolutionV1::Complete { scope_work, .. } = resolution else {
    return Ok(retained);
  };
  for work in scope_work {
    retained = add_semantic_retained_bytes(retained, work.semantic_state_root.len(), "scope semantic-state root")?;
    retained = add_semantic_retained_bytes(retained, work.scope.scope_id.len(), "ScopeId")?;
    retained = add_semantic_retained_bytes(retained, work.scope.encoded_definition.len(), "ScopeDefinition")?;
    for value_store in &work.scope.value_stores {
      retained = add_semantic_retained_bytes(retained, value_store.value_store_id.len(), "ValueStoreId")?;
      retained = add_semantic_retained_bytes(retained, value_store.encoded_definition.len(), "ValueStoreDefinition")?;
      for field in &value_store.field_indexes {
        retained = add_semantic_retained_bytes(retained, field.index_id.len(), "IndexId")?;
        retained = add_semantic_retained_bytes(retained, field.encoded_definition.len(), "FieldIndexDefinition")?;
      }
    }
  }
  Ok(retained)
}

fn add_semantic_retained_bytes(retained: u64, bytes: usize, resource: &'static str) -> Result<u64, IndexSemanticScopeReadErrorV1> {
  retained.checked_add(usize_to_semantic_bytes(bytes, resource)?).ok_or_else(|| {
    IndexSemanticScopeReadErrorV1::corrupt("semantic_memory_overflow", format!("retained {resource} byte accounting overflowed"))
  })
}

fn usize_to_semantic_bytes(bytes: usize, resource: &'static str) -> Result<u64, IndexSemanticScopeReadErrorV1> {
  u64::try_from(bytes).map_err(|source| {
    IndexSemanticScopeReadErrorV1::corrupt(
      "semantic_memory_overflow",
      format!("retained {resource} byte length does not fit u64: {source}"),
    )
  })
}

fn validate_semantic_root(expected: &[u8], actual: &[u8]) -> Result<(), IndexProducerSourceErrorV1> {
  if expected != actual {
    return Err(IndexProducerSourceErrorV1::SemanticRootMismatch { expected: hex::encode(expected), actual: hex::encode(actual) });
  }
  Ok(())
}

fn validate_scope_work(
  hash_algorithm: HashAlgorithm,
  semantic_state_root: &[u8],
  scope_work: &[ResolvedIndexScopeWorkV1],
  limits: IndexSemanticScopeLimitsV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<(), IndexProducerSourceErrorV1> {
  enforce_semantic_limit("scopes", limits.max_scopes as u64, scope_work.len() as u64)?;
  let mut value_store_count = 0u64;
  let mut field_index_count = 0u64;
  let mut definition_bytes = 0u64;
  let mut owners = BTreeSet::new();

  for work in scope_work {
    if is_cancelled() {
      return Err(IndexProducerSourceErrorV1::Cancelled);
    }
    validate_semantic_root(semantic_state_root, &work.semantic_state_root)?;
    if work.document_ordinal == 0 {
      return Err(IndexProducerSourceErrorV1::InvalidDocumentOrdinal {
        scope: hex::encode(&work.scope.scope_id),
        ordinal: work.document_ordinal,
      });
    }

    definition_bytes = add_definition_bytes(definition_bytes, work.scope.encoded_definition.len())?;
    enforce_semantic_limit("definition bytes", limits.max_definition_bytes, definition_bytes)?;
    let scope = decode_scope_definition(&work.scope.encoded_definition, hash_algorithm)?;
    if scope.scope_id != work.scope.scope_id {
      return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(format!(
        "ScopeDefinition identity mismatch for {}",
        hex::encode(&work.scope.scope_id)
      )));
    }
    insert_semantic_owner(&mut owners, &work.scope.scope_id)?;

    for value_store in &work.scope.value_stores {
      value_store_count = value_store_count
        .checked_add(1)
        .ok_or_else(|| IndexProducerSourceErrorV1::InvalidSemanticResolution("ValueStore count overflow".to_string()))?;
      enforce_semantic_limit("value stores", limits.max_value_stores as u64, value_store_count)?;
      definition_bytes = add_definition_bytes(definition_bytes, value_store.encoded_definition.len())?;
      enforce_semantic_limit("definition bytes", limits.max_definition_bytes, definition_bytes)?;
      let definition = decode_value_store_definition(&value_store.encoded_definition, hash_algorithm)?;
      if definition.value_store_id != value_store.value_store_id || definition.scope_id != work.scope.scope_id {
        return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(format!(
          "ValueStoreDefinition closure mismatch for {}",
          hex::encode(&value_store.value_store_id)
        )));
      }
      insert_semantic_owner(&mut owners, &value_store.value_store_id)?;

      for field in &value_store.field_indexes {
        field_index_count = field_index_count
          .checked_add(1)
          .ok_or_else(|| IndexProducerSourceErrorV1::InvalidSemanticResolution("FieldIndex count overflow".to_string()))?;
        enforce_semantic_limit("field indexes", limits.max_field_indexes as u64, field_index_count)?;
        definition_bytes = add_definition_bytes(definition_bytes, field.encoded_definition.len())?;
        enforce_semantic_limit("definition bytes", limits.max_definition_bytes, definition_bytes)?;
        let definition = decode_field_index_definition(&field.encoded_definition, hash_algorithm)?;
        if definition.index_id != field.index_id || definition.value_store_id != value_store.value_store_id {
          return Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(format!(
            "FieldIndexDefinition closure mismatch for {}",
            hex::encode(&field.index_id)
          )));
        }
        insert_semantic_owner(&mut owners, &field.index_id)?;
      }
    }
  }
  Ok(())
}

fn add_definition_bytes(total: u64, bytes: usize) -> Result<u64, IndexProducerSourceErrorV1> {
  let bytes = u64::try_from(bytes).map_err(|source| {
    IndexProducerSourceErrorV1::InvalidSemanticResolution(format!("definition byte length does not fit u64: {source}"))
  })?;
  total
    .checked_add(bytes)
    .ok_or_else(|| IndexProducerSourceErrorV1::InvalidSemanticResolution("definition byte count overflow".to_string()))
}

fn enforce_semantic_limit(resource: &'static str, limit: u64, actual: u64) -> Result<(), IndexProducerSourceErrorV1> {
  if actual > limit {
    return Err(IndexProducerSourceErrorV1::SemanticLimitExceeded { resource, limit, actual });
  }
  Ok(())
}

fn insert_semantic_owner(owners: &mut BTreeSet<Vec<u8>>, owner: &[u8]) -> Result<(), IndexProducerSourceErrorV1> {
  if !owners.insert(owner.to_vec()) {
    return Err(IndexProducerSourceErrorV1::DuplicateSemanticOwner { owner: hex::encode(owner) });
  }
  Ok(())
}

pub fn resolve_leased_mutation_record<'a>(
  hash_algorithm: HashAlgorithm,
  producer: &IndexProducerCoordinatorV1,
  lease: &IndexProducerLeaseV1,
  journal: &'a MutationJournalV1<'a>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<MutationRecordV1<'a>, IndexProducerSourceErrorV1> {
  if is_cancelled() {
    return Err(IndexProducerSourceErrorV1::Cancelled);
  }
  let task = producer.leased_task(lease)?;
  if task.kind() != IndexProducerTaskKindV1::MutationWindow {
    return Err(IndexProducerSourceErrorV1::TaskMismatch("leased task is not mutation-window work".to_string()));
  }
  if task.journal_head() != Some(journal.key.as_slice()) {
    return Err(IndexProducerSourceErrorV1::TaskMismatch("journal head does not match the retained task".to_string()));
  }
  if task.semantic_state_root() != journal.semantic_state_root {
    return Err(IndexProducerSourceErrorV1::TaskMismatch("journal semantic-state root does not match the retained task".to_string()));
  }

  let mut matched = None;
  for record in journal.records.iter() {
    if is_cancelled() {
      return Err(IndexProducerSourceErrorV1::Cancelled);
    }
    let record = record?;
    let operation_id = derive_mutation_operation_id(hash_algorithm, record.mutation_id, record.batch_ordinal)?;
    if operation_id != task.operation_id() {
      continue;
    }
    if matched.is_some() {
      return Err(IndexProducerSourceErrorV1::AmbiguousMutationRecord { operation_id });
    }
    if record.sequence != task.publication_sequence()
      || record.root_before != task.namespace_root_before()
      || record.root_after != task.namespace_root_after()
    {
      return Err(IndexProducerSourceErrorV1::TaskMismatch(
        "journal record publication sequence or namespace roots do not match the retained task".to_string(),
      ));
    }
    matched = Some(record);
  }
  matched.ok_or(IndexProducerSourceErrorV1::MissingMutationRecord { operation_id: task.operation_id() })
}

pub fn resolve_mutation_document_transition(
  hash_algorithm: HashAlgorithm,
  record: &MutationRecordV1<'_>,
  source: &dyn IndexFileRevisionSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<ResolvedIndexDocumentTransitionReadV1, IndexProducerSourceErrorV1> {
  if is_cancelled() {
    return Err(IndexProducerSourceErrorV1::Cancelled);
  }
  let before = resolve_side(
    hash_algorithm,
    MutationSideRefV1 {
      namespace_root: record.root_before,
      path: record.before_path,
      file_key: record.before_file_key,
      revision_hash: record.before_revision,
    },
    source,
    is_cancelled,
  )?;
  let after = resolve_side(
    hash_algorithm,
    MutationSideRefV1 {
      namespace_root: record.root_after,
      path: record.after_path,
      file_key: record.after_file_key,
      revision_hash: record.after_revision,
    },
    source,
    is_cancelled,
  )?;
  let (before, before_reservation) = match before {
    Some((document, reservation)) => (Some(document), Some(reservation)),
    None => (None, None),
  };
  let (after, after_reservation) = match after {
    Some((document, reservation)) => (Some(document), Some(reservation)),
    None => (None, None),
  };
  Ok(ResolvedIndexDocumentTransitionReadV1 {
    transition: ResolvedIndexDocumentTransitionV1 { before, after },
    _before_reservation: before_reservation,
    _after_reservation: after_reservation,
  })
}

#[derive(Clone, Copy)]
struct MutationSideRefV1<'a> {
  namespace_root: &'a [u8],
  path: Option<&'a str>,
  file_key: Option<&'a [u8]>,
  revision_hash: Option<&'a [u8]>,
}

fn resolve_side(
  hash_algorithm: HashAlgorithm,
  side: MutationSideRefV1<'_>,
  source: &dyn IndexFileRevisionSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Option<(ResolvedIndexDocumentV1, MemoryReservation)>, IndexProducerSourceErrorV1> {
  let Some(path) = side.path else {
    return Ok(None);
  };
  if is_cancelled() {
    return Err(IndexProducerSourceErrorV1::Cancelled);
  }
  let expected_key =
    side.file_key.ok_or_else(|| IndexProducerSourceErrorV1::TaskMismatch("present mutation side has no FileKey".to_string()))?;
  let expected_revision =
    side.revision_hash.ok_or_else(|| IndexProducerSourceErrorV1::TaskMismatch("present mutation side has no revision".to_string()))?;
  let read = match source.load_file_revision(side.namespace_root, path) {
    Ok(Some(read)) => read,
    Ok(None) => {
      return Err(IndexProducerSourceErrorV1::MissingRevision { root: hex::encode(side.namespace_root), path: path.to_string() });
    }
    Err(error) if error.class() == IndexFileRevisionReadErrorClassV1::Cancelled => {
      return Err(IndexProducerSourceErrorV1::Cancelled);
    }
    Err(error) => return Err(IndexProducerSourceErrorV1::RevisionRead(error)),
  };
  let (loaded, mut reservation) = read.into_parts();
  if loaded.revision_hash != expected_revision {
    return Err(IndexProducerSourceErrorV1::RevisionMismatch {
      path: path.to_string(),
      expected: hex::encode(expected_revision),
      actual: hex::encode(&loaded.revision_hash),
    });
  }
  if loaded.file_record.path != path {
    return Err(IndexProducerSourceErrorV1::PathMismatch { expected: path.to_string(), actual: loaded.file_record.path });
  }
  let actual_key = digest_parts(hash_algorithm, &[b"file:", loaded.file_record.path.as_bytes()]);
  if actual_key != expected_key {
    return Err(IndexProducerSourceErrorV1::FileKeyMismatch { path: path.to_string() });
  }
  let namespace_root_bytes = u64::try_from(side.namespace_root.len()).map_err(|error| {
    IndexProducerSourceErrorV1::RevisionRead(IndexFileRevisionReadErrorV1::corrupt(
      "revision_transition_memory",
      format!("namespace-root retained byte length does not fit u64: {error}"),
    ))
  })?;
  reservation.grow(namespace_root_bytes).map_err(|error| {
    IndexProducerSourceErrorV1::RevisionRead(IndexFileRevisionReadErrorV1::retryable(
      "revision_transition_memory",
      format!("namespace-root retention admission failed: {error}"),
    ))
  })?;
  Ok(Some((
    ResolvedIndexDocumentV1 {
      namespace_root: side.namespace_root.to_vec(),
      revision_hash: loaded.revision_hash,
      file_record: loaded.file_record,
    },
    reservation,
  )))
}
