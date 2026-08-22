//! Bounded semantic-catalog resolution for one leased index producer task.

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::errors::EngineError;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner};
use crate::engine::storage_engine::StorageEngine;

use super::index_producer_source::{
  IndexSemanticScopeReadErrorV1, IndexSemanticScopeReadRequestV1, IndexSemanticScopeReadV1, IndexSemanticScopeResolutionV1,
  IndexSemanticScopeSourceV1, OwnedIndexFieldDefinitionV1, OwnedIndexScopeDefinitionV1, OwnedIndexValueStoreDefinitionV1,
  ResolvedIndexDocumentTransitionV1, ResolvedIndexScopeWorkV1,
};
use super::field_definition::decode_field_index_definition;
use super::namespace::{
  SemanticAvailabilityV1, SemanticCatalogNodeV1, SemanticCatalogRecordV1, SemanticObjectKind, decode_semantic_catalog_node,
  decode_semantic_definition_record, decode_semantic_object,
};
use super::scope::{decode_scope_definition, scope_matches_path, validate_canonical_absolute_path};
use super::semantic_store::V4SemanticObjectStore;
use super::value_store::decode_value_store_definition;

const SEMANTIC_TRAVERSAL_WORKSPACE_BYTES: u64 = 4 * 1_024 * 1_024;

pub trait IndexSemanticObjectReadSourceV1: Send + Sync {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, IndexSemanticScopeReadErrorV1>;
}

pub struct StoredIndexSemanticObjectReadSourceV1<'engine> {
  engine: &'engine StorageEngine,
}

impl<'engine> StoredIndexSemanticObjectReadSourceV1<'engine> {
  pub const fn new(engine: &'engine StorageEngine) -> Self {
    Self { engine }
  }
}

impl IndexSemanticObjectReadSourceV1 for StoredIndexSemanticObjectReadSourceV1<'_> {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, IndexSemanticScopeReadErrorV1> {
    V4SemanticObjectStore::new(self.engine)
      .load(kind_id, object_id)
      .map(|loaded| loaded.map(|object| object.bytes))
      .map_err(map_semantic_store_error)
  }
}

fn map_semantic_store_error(error: EngineError) -> IndexSemanticScopeReadErrorV1 {
  match error {
    EngineError::Cancelled(context) => IndexSemanticScopeReadErrorV1::cancelled("semantic_store_cancelled", context),
    error @ (EngineError::IoError(_) | EngineError::ResourceExhausted(_) | EngineError::ShuttingDown) => {
      IndexSemanticScopeReadErrorV1::retryable("semantic_store_retryable", error.to_string())
    }
    error => IndexSemanticScopeReadErrorV1::corrupt("semantic_store_corrupt", error.to_string()),
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScopeOrdinalClaimErrorClassV1 {
  Cancelled,
  Retryable,
  Corrupt,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("index scope ordinal claim failed ({code}): {context}")]
pub struct IndexScopeOrdinalClaimErrorV1 {
  class: IndexScopeOrdinalClaimErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexScopeOrdinalClaimErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexScopeOrdinalClaimErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn retryable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexScopeOrdinalClaimErrorClassV1::Retryable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexScopeOrdinalClaimErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexScopeOrdinalClaimErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

#[derive(Clone, Copy)]
pub struct IndexScopeOrdinalClaimRequestV1<'request> {
  pub operation_id: [u8; 16],
  pub source_publication_sequence: u64,
  pub semantic_state_root: &'request [u8],
  pub scope_id: &'request [u8],
  pub transition: &'request ResolvedIndexDocumentTransitionV1,
  pub before_in_scope: bool,
  pub after_in_scope: bool,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

pub trait IndexScopeOrdinalAuthorityV1: Send + Sync {
  fn claim_scope_ordinal(&self, request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexScopeOrdinalClaimObservationV1 {
  pub prior_operation_claim: Option<u64>,
  pub before_live_ordinal: Option<u64>,
  pub after_live_ordinal: Option<u64>,
  pub next_document_ordinal: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScopeOrdinalClaimPlanV1 {
  Reuse { document_ordinal: u64 },
  Allocate { document_ordinal: u64, next_document_ordinal: u64 },
}

pub fn plan_scope_ordinal_claim(
  request: IndexScopeOrdinalClaimRequestV1<'_>,
  observation: IndexScopeOrdinalClaimObservationV1,
) -> Result<IndexScopeOrdinalClaimPlanV1, IndexScopeOrdinalClaimErrorV1> {
  if (request.is_cancelled)() {
    return Err(IndexScopeOrdinalClaimErrorV1::cancelled("scope_ordinal_cancelled", "scope ordinal planning was cancelled"));
  }
  if !request.before_in_scope && !request.after_in_scope {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
      "scope_ordinal_membership",
      "scope ordinal claim has neither an in-scope before nor after revision",
    ));
  }
  validate_optional_ordinal(observation.prior_operation_claim, "prior operation claim")?;
  validate_optional_ordinal(observation.before_live_ordinal, "before live reverse mapping")?;
  validate_optional_ordinal(observation.after_live_ordinal, "after live reverse mapping")?;
  if observation.next_document_ordinal == 0 {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_high_water", "scope next-document ordinal is zero"));
  }
  if let Some(document_ordinal) = observation.prior_operation_claim {
    return Ok(IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal });
  }

  if request.before_in_scope {
    let before = observation.before_live_ordinal.ok_or_else(|| {
      IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_before_missing",
        "in-scope before revision has no live reverse mapping or durable operation claim",
      )
    })?;
    if request.after_in_scope && observation.after_live_ordinal.is_some_and(|after| after != before) {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_conflict",
        "before and after live reverse mappings disagree during a same-scope transition",
      ));
    }
    return Ok(IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal: before });
  }

  if let Some(document_ordinal) = observation.after_live_ordinal {
    return Ok(IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal });
  }
  let next_document_ordinal = observation.next_document_ordinal.checked_add(1).ok_or_else(|| {
    IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_exhausted", "scope document ordinal high-water cannot advance without reuse")
  })?;
  Ok(IndexScopeOrdinalClaimPlanV1::Allocate { document_ordinal: observation.next_document_ordinal, next_document_ordinal })
}

fn validate_optional_ordinal(ordinal: Option<u64>, label: &'static str) -> Result<(), IndexScopeOrdinalClaimErrorV1> {
  if ordinal == Some(0) {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_zero", format!("{label} is zero")));
  }
  Ok(())
}

pub struct CatalogIndexSemanticScopeSourceV1<'source> {
  hash_algorithm: HashAlgorithm,
  memory: MemoryCoordinator,
  objects: &'source dyn IndexSemanticObjectReadSourceV1,
  ordinals: &'source dyn IndexScopeOrdinalAuthorityV1,
}

impl<'source> CatalogIndexSemanticScopeSourceV1<'source> {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    objects: &'source dyn IndexSemanticObjectReadSourceV1,
    ordinals: &'source dyn IndexScopeOrdinalAuthorityV1,
  ) -> Self {
    Self { hash_algorithm, memory, objects, ordinals }
  }
}

#[derive(Clone, Copy)]
pub struct IndexCompactionSemanticInventoryRequestV1<'request> {
  pub semantic_state_root: &'request [u8],
  pub maintenance_scope: &'request str,
  pub limits: super::index_producer_source::IndexSemanticScopeLimitsV1,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCompactionSemanticValueStoreV1 {
  value_store_id: Vec<u8>,
  field_index_ids: Vec<Vec<u8>>,
}

impl IndexCompactionSemanticValueStoreV1 {
  pub fn value_store_id(&self) -> &[u8] {
    &self.value_store_id
  }

  pub fn field_index_ids(&self) -> &[Vec<u8>] {
    &self.field_index_ids
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCompactionSemanticScopeV1 {
  scope_id: Vec<u8>,
  value_stores: Vec<IndexCompactionSemanticValueStoreV1>,
}

impl IndexCompactionSemanticScopeV1 {
  pub fn scope_id(&self) -> &[u8] {
    &self.scope_id
  }

  pub fn value_stores(&self) -> &[IndexCompactionSemanticValueStoreV1] {
    &self.value_stores
  }
}

pub struct IndexCompactionSemanticInventoryV1 {
  semantic_state_root: Vec<u8>,
  scopes: Vec<IndexCompactionSemanticScopeV1>,
  _reservation: crate::engine::memory_coordinator::MemoryReservation,
}

impl std::fmt::Debug for IndexCompactionSemanticInventoryV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("IndexCompactionSemanticInventoryV1")
      .field("semantic_state_root", &hex::encode(&self.semantic_state_root))
      .field("scopes", &self.scopes)
      .finish_non_exhaustive()
  }
}

impl IndexCompactionSemanticInventoryV1 {
  pub fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub fn scopes(&self) -> &[IndexCompactionSemanticScopeV1] {
    &self.scopes
  }
}

impl IndexSemanticScopeSourceV1 for CatalogIndexSemanticScopeSourceV1<'_> {
  fn resolve_scopes(
    &self,
    request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    if (request.is_cancelled)() {
      return Err(IndexSemanticScopeReadErrorV1::cancelled("semantic_cancelled", "semantic resolution was cancelled before admission"));
    }
    let reservation_bytes = semantic_reservation_bytes(self.hash_algorithm, request)?;
    let reservation = self
      .memory
      .reserve(MemoryOwner::Task, reservation_bytes, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("semantic_memory_pressure", error.to_string()))?;
    let state_bytes = self
      .objects
      .load_semantic_object(0x0001, request.semantic_state_root)?
      .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_state_missing", "semantic-state object is absent"))?;
    let object = decode_semantic_object(&state_bytes, self.hash_algorithm)
      .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
    if object.object_id != request.semantic_state_root || !matches!(object.kind, SemanticObjectKind::State { .. }) {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_state_identity",
        "semantic-state bytes do not match the requested state identity",
      ));
    }
    let state = object.semantic_state.ok_or_else(|| {
      IndexSemanticScopeReadErrorV1::corrupt("semantic_state_fields", "semantic-state object has no decoded state fields")
    })?;
    if (request.is_cancelled)() {
      return Err(IndexSemanticScopeReadErrorV1::cancelled("semantic_cancelled", "semantic resolution was cancelled after state read"));
    }
    match state.availability {
      SemanticAvailabilityV1::ContentOnly { .. } => IndexSemanticScopeReadV1::new(
        IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: request.semantic_state_root.to_vec() },
        reservation,
      ),
      SemanticAvailabilityV1::Complete {
        catalog_root,
        catalog_record_count,
        catalog_node_count,
        definition_count,
        dependency_count,
        ..
      } => self.resolve_complete(
        request,
        reservation,
        &catalog_root,
        CatalogExpectedCountsV1 {
          records: catalog_record_count,
          nodes: catalog_node_count,
          definitions: definition_count,
          dependencies: dependency_count,
        },
      ),
    }
  }
}

#[derive(Debug, Clone)]
struct ApplicableScopeV1 {
  scope: OwnedIndexScopeDefinitionV1,
  before_in_scope: bool,
  after_in_scope: bool,
}

#[derive(Debug, Clone, Copy)]
struct CatalogExpectedCountsV1 {
  records: u64,
  nodes: u64,
  definitions: u64,
  dependencies: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CatalogWalkStatsV1 {
  records: u64,
  nodes: u64,
  class_counts: [u64; 8],
}

#[derive(Debug, Clone)]
struct OwnedCatalogChildV1 {
  edge: u8,
  record_count: u64,
  object_id: Vec<u8>,
}

enum CatalogWalkFrameV1 {
  Visit { object_id: Vec<u8>, expected_prefix: Vec<u8>, expected_records: u64 },
  Children { prefix: Vec<u8>, children: Vec<OwnedCatalogChildV1>, next_child: usize },
}

impl CatalogIndexSemanticScopeSourceV1<'_> {
  fn resolve_complete(
    &self,
    request: IndexSemanticScopeReadRequestV1<'_>,
    reservation: crate::engine::memory_coordinator::MemoryReservation,
    catalog_root: &[u8],
    expected: CatalogExpectedCountsV1,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    if expected.definitions == 0 || expected.definitions > expected.records || expected.dependencies > expected.definitions {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_state_counts",
        "semantic-state definition/dependency counts do not fit the catalog record count",
      ));
    }

    let mut definition_bytes = 0u64;
    let mut scopes = Vec::new();
    let scope_stats = self.walk_catalog(catalog_root, request.is_cancelled, |record| {
      if record.record_kind != 3 {
        return Ok(());
      }
      self.with_definition(record, request.is_cancelled, |definition| {
        let scope = decode_scope_definition(definition, self.hash_algorithm)
          .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
        validate_definition_identity(record, &scope.scope_id)?;
        let before_in_scope = match request.transition.before.as_ref() {
          Some(document) => scope_matches_path(&scope, &document.file_record.path)
            .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?,
          None => false,
        };
        let after_in_scope = match request.transition.after.as_ref() {
          Some(document) => scope_matches_path(&scope, &document.file_record.path)
            .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?,
          None => false,
        };
        if !before_in_scope && !after_in_scope {
          return Ok(());
        }
        enforce_count_limit("scopes", scopes.len(), request.limits.max_scopes())?;
        definition_bytes = retained_definition_bytes(definition_bytes, definition.len(), request.limits.max_definition_bytes())?;
        scopes.push(ApplicableScopeV1 {
          scope: OwnedIndexScopeDefinitionV1 {
            scope_id: scope.scope_id,
            encoded_definition: definition.to_vec(),
            value_stores: Vec::new(),
          },
          before_in_scope,
          after_in_scope,
        });
        Ok(())
      })
    })?;
    validate_walk_counts(scope_stats, expected)?;

    let mut value_store_count = 0usize;
    let value_stats = self.walk_catalog(catalog_root, request.is_cancelled, |record| {
      if record.record_kind != 4 {
        return Ok(());
      }
      self.with_definition(record, request.is_cancelled, |definition| {
        let value_store = decode_value_store_definition(definition, self.hash_algorithm)
          .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
        validate_definition_identity(record, &value_store.value_store_id)?;
        let Some(scope) = scopes.iter_mut().find(|scope| scope.scope.scope_id == value_store.scope_id) else {
          return Ok(());
        };
        enforce_count_limit("value stores", value_store_count, request.limits.max_value_stores())?;
        definition_bytes = retained_definition_bytes(definition_bytes, definition.len(), request.limits.max_definition_bytes())?;
        scope.scope.value_stores.push(OwnedIndexValueStoreDefinitionV1 {
          value_store_id: value_store.value_store_id,
          encoded_definition: definition.to_vec(),
          field_indexes: Vec::new(),
        });
        value_store_count += 1;
        Ok(())
      })
    })?;
    validate_walk_counts(value_stats, expected)?;

    let mut field_index_count = 0usize;
    let field_stats = self.walk_catalog(catalog_root, request.is_cancelled, |record| {
      if record.record_kind != 5 {
        return Ok(());
      }
      self.with_definition(record, request.is_cancelled, |definition| {
        let field = decode_field_index_definition(definition, self.hash_algorithm)
          .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
        validate_definition_identity(record, &field.index_id)?;
        let value_store = scopes
          .iter_mut()
          .flat_map(|scope| scope.scope.value_stores.iter_mut())
          .find(|value_store| value_store.value_store_id == field.value_store_id);
        let Some(value_store) = value_store else {
          return Ok(());
        };
        enforce_count_limit("field indexes", field_index_count, request.limits.max_field_indexes())?;
        definition_bytes = retained_definition_bytes(definition_bytes, definition.len(), request.limits.max_definition_bytes())?;
        value_store.field_indexes.push(OwnedIndexFieldDefinitionV1 { index_id: field.index_id, encoded_definition: definition.to_vec() });
        field_index_count += 1;
        Ok(())
      })
    })?;
    validate_walk_counts(field_stats, expected)?;

    scopes.sort_by(|left, right| left.scope.scope_id.cmp(&right.scope.scope_id));
    for scope in &mut scopes {
      scope.scope.value_stores.sort_by(|left, right| left.value_store_id.cmp(&right.value_store_id));
      for value_store in &mut scope.scope.value_stores {
        value_store.field_indexes.sort_by(|left, right| left.index_id.cmp(&right.index_id));
      }
    }
    let mut scope_work = Vec::new();
    scope_work.try_reserve_exact(scopes.len()).map_err(|error| {
      IndexSemanticScopeReadErrorV1::retryable("semantic_scope_allocation", format!("scope work allocation failed: {error}"))
    })?;
    for scope in scopes {
      if (request.is_cancelled)() {
        return Err(IndexSemanticScopeReadErrorV1::cancelled(
          "semantic_cancelled",
          "semantic resolution was cancelled before ordinal claim",
        ));
      }
      let ordinal = self
        .ordinals
        .claim_scope_ordinal(IndexScopeOrdinalClaimRequestV1 {
          operation_id: request.operation_id,
          source_publication_sequence: request.source_publication_sequence,
          semantic_state_root: request.semantic_state_root,
          scope_id: &scope.scope.scope_id,
          transition: request.transition,
          before_in_scope: scope.before_in_scope,
          after_in_scope: scope.after_in_scope,
          is_cancelled: request.is_cancelled,
        })
        .map_err(map_ordinal_error)?;
      if ordinal == 0 {
        return Err(IndexSemanticScopeReadErrorV1::corrupt(
          "scope_ordinal_zero",
          format!("scope {} ordinal authority returned zero", hex::encode(&scope.scope.scope_id)),
        ));
      }
      scope_work.push(ResolvedIndexScopeWorkV1 {
        semantic_state_root: request.semantic_state_root.to_vec(),
        document_ordinal: ordinal,
        scope: scope.scope,
      });
    }
    IndexSemanticScopeReadV1::new(
      IndexSemanticScopeResolutionV1::Complete { semantic_state_root: request.semantic_state_root.to_vec(), scope_work },
      reservation,
    )
  }

  pub fn resolve_compaction_inventory(
    &self,
    request: IndexCompactionSemanticInventoryRequestV1<'_>,
  ) -> Result<IndexCompactionSemanticInventoryV1, IndexSemanticScopeReadErrorV1> {
    if (request.is_cancelled)() {
      return Err(IndexSemanticScopeReadErrorV1::cancelled(
        "semantic_cancelled",
        "compaction semantic inventory was cancelled before admission",
      ));
    }
    validate_canonical_absolute_path(request.maintenance_scope)
      .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
    let reservation_bytes = semantic_limits_reservation_bytes(self.hash_algorithm, request.limits)?;
    let reservation = self
      .memory
      .reserve(MemoryOwner::Task, reservation_bytes, AdmissionClass::Maintenance)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("semantic_memory_pressure", error.to_string()))?;
    let state_bytes = self
      .objects
      .load_semantic_object(0x0001, request.semantic_state_root)?
      .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_state_missing", "semantic-state object is absent"))?;
    let object = decode_semantic_object(&state_bytes, self.hash_algorithm)
      .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
    if object.object_id != request.semantic_state_root || !matches!(object.kind, SemanticObjectKind::State { .. }) {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_state_identity",
        "semantic-state bytes do not match the requested compaction inventory identity",
      ));
    }
    let state = object.semantic_state.ok_or_else(|| {
      IndexSemanticScopeReadErrorV1::corrupt("semantic_state_fields", "semantic-state object has no decoded state fields")
    })?;
    let semantic_state_root = request.semantic_state_root.to_vec();
    let SemanticAvailabilityV1::Complete {
      catalog_root, catalog_record_count, catalog_node_count, definition_count, dependency_count, ..
    } = state.availability
    else {
      return Ok(IndexCompactionSemanticInventoryV1 { semantic_state_root, scopes: Vec::new(), _reservation: reservation });
    };
    let expected = CatalogExpectedCountsV1 {
      records: catalog_record_count,
      nodes: catalog_node_count,
      definitions: definition_count,
      dependencies: dependency_count,
    };
    if expected.definitions == 0 || expected.definitions > expected.records || expected.dependencies > expected.definitions {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_state_counts",
        "semantic-state definition/dependency counts do not fit the catalog record count",
      ));
    }

    let mut definition_bytes = 0u64;
    let mut scopes = Vec::new();
    let scope_stats = self.walk_catalog(&catalog_root, request.is_cancelled, |record| {
      if record.record_kind != 3 {
        return Ok(());
      }
      self.with_definition(record, request.is_cancelled, |definition| {
        let scope = decode_scope_definition(definition, self.hash_algorithm)
          .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
        validate_definition_identity(record, &scope.scope_id)?;
        if !canonical_paths_overlap(scope.owner_path, request.maintenance_scope) {
          return Ok(());
        }
        enforce_count_limit("scopes", scopes.len(), request.limits.max_scopes())?;
        definition_bytes = retained_definition_bytes(definition_bytes, definition.len(), request.limits.max_definition_bytes())?;
        scopes.push(IndexCompactionSemanticScopeV1 { scope_id: scope.scope_id, value_stores: Vec::new() });
        Ok(())
      })
    })?;
    validate_walk_counts(scope_stats, expected)?;

    let mut value_store_count = 0usize;
    let value_stats = self.walk_catalog(&catalog_root, request.is_cancelled, |record| {
      if record.record_kind != 4 {
        return Ok(());
      }
      self.with_definition(record, request.is_cancelled, |definition| {
        let value_store = decode_value_store_definition(definition, self.hash_algorithm)
          .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
        validate_definition_identity(record, &value_store.value_store_id)?;
        let Some(scope) = scopes.iter_mut().find(|scope| scope.scope_id == value_store.scope_id) else {
          return Ok(());
        };
        enforce_count_limit("value stores", value_store_count, request.limits.max_value_stores())?;
        definition_bytes = retained_definition_bytes(definition_bytes, definition.len(), request.limits.max_definition_bytes())?;
        scope
          .value_stores
          .push(IndexCompactionSemanticValueStoreV1 { value_store_id: value_store.value_store_id, field_index_ids: Vec::new() });
        value_store_count += 1;
        Ok(())
      })
    })?;
    validate_walk_counts(value_stats, expected)?;

    let mut field_index_count = 0usize;
    let field_stats = self.walk_catalog(&catalog_root, request.is_cancelled, |record| {
      if record.record_kind != 5 {
        return Ok(());
      }
      self.with_definition(record, request.is_cancelled, |definition| {
        let field = decode_field_index_definition(definition, self.hash_algorithm)
          .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
        validate_definition_identity(record, &field.index_id)?;
        let value_store = scopes
          .iter_mut()
          .flat_map(|scope| scope.value_stores.iter_mut())
          .find(|value_store| value_store.value_store_id == field.value_store_id);
        let Some(value_store) = value_store else {
          return Ok(());
        };
        enforce_count_limit("field indexes", field_index_count, request.limits.max_field_indexes())?;
        definition_bytes = retained_definition_bytes(definition_bytes, definition.len(), request.limits.max_definition_bytes())?;
        value_store.field_index_ids.push(field.index_id);
        field_index_count += 1;
        Ok(())
      })
    })?;
    validate_walk_counts(field_stats, expected)?;
    if (request.is_cancelled)() {
      return Err(IndexSemanticScopeReadErrorV1::cancelled(
        "semantic_cancelled",
        "compaction semantic inventory was cancelled before ordering",
      ));
    }
    scopes.sort_unstable_by(|left, right| left.scope_id.cmp(&right.scope_id));
    for scope in &mut scopes {
      scope.value_stores.sort_unstable_by(|left, right| left.value_store_id.cmp(&right.value_store_id));
      for value_store in &mut scope.value_stores {
        value_store.field_index_ids.sort_unstable();
      }
    }
    Ok(IndexCompactionSemanticInventoryV1 { semantic_state_root, scopes, _reservation: reservation })
  }

  fn walk_catalog(
    &self,
    catalog_root: &[u8],
    is_cancelled: &dyn Fn() -> bool,
    mut visit_record: impl FnMut(SemanticCatalogRecordV1<'_>) -> Result<(), IndexSemanticScopeReadErrorV1>,
  ) -> Result<CatalogWalkStatsV1, IndexSemanticScopeReadErrorV1> {
    let hash_width = self.hash_algorithm.hash_length();
    let mut stack = vec![CatalogWalkFrameV1::Visit { object_id: catalog_root.to_vec(), expected_prefix: Vec::new(), expected_records: 0 }];
    let mut stats = CatalogWalkStatsV1::default();
    while let Some(frame) = stack.pop() {
      if is_cancelled() {
        return Err(IndexSemanticScopeReadErrorV1::cancelled("semantic_cancelled", "semantic catalog traversal was cancelled"));
      }
      if stack.len() > hash_width.saturating_mul(2) {
        return Err(IndexSemanticScopeReadErrorV1::corrupt(
          "semantic_catalog_depth",
          "semantic catalog traversal exceeded the database hash width",
        ));
      }
      match frame {
        CatalogWalkFrameV1::Visit { object_id, expected_prefix, expected_records } => {
          let bytes = self.load_catalog_node(&object_id)?;
          let node = decode_semantic_catalog_node(&bytes, self.hash_algorithm)
            .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
          if node.object_id() != object_id {
            return Err(IndexSemanticScopeReadErrorV1::corrupt(
              "semantic_catalog_identity",
              "semantic catalog node bytes do not match the requested object identity",
            ));
          }
          stats.nodes = stats
            .nodes
            .checked_add(1)
            .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog node count overflow"))?;
          match node {
            SemanticCatalogNodeV1::Leaf(leaf) => {
              if !leaf.lookup_digest().starts_with(&expected_prefix)
                || (expected_records != 0 && u64::from(leaf.record_count()) != expected_records)
              {
                return Err(IndexSemanticScopeReadErrorV1::corrupt(
                  "semantic_catalog_leaf_closure",
                  "semantic catalog leaf disagrees with its parent prefix or record count",
                ));
              }
              for record in leaf.records() {
                let record = record.map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
                stats.records = stats.records.checked_add(1).ok_or_else(|| {
                  IndexSemanticScopeReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog record count overflow")
                })?;
                stats.class_counts[usize::from(record.record_kind)] =
                  stats.class_counts[usize::from(record.record_kind)].checked_add(1).ok_or_else(|| {
                    IndexSemanticScopeReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog class count overflow")
                  })?;
                visit_record(record)?;
              }
            }
            SemanticCatalogNodeV1::Internal(internal) => {
              if usize::from(internal.depth()) != expected_prefix.len()
                || (expected_records != 0 && internal.subtree_record_count() != expected_records)
              {
                return Err(IndexSemanticScopeReadErrorV1::corrupt(
                  "semantic_catalog_internal_closure",
                  "semantic catalog internal node disagrees with its parent depth or record count",
                ));
              }
              let mut prefix = expected_prefix;
              prefix.extend_from_slice(internal.prefix());
              let mut children = Vec::new();
              children.try_reserve_exact(usize::from(internal.child_count())).map_err(|error| {
                IndexSemanticScopeReadErrorV1::retryable("semantic_catalog_allocation", format!("catalog child allocation failed: {error}"))
              })?;
              for child in internal.children() {
                let child = child.map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
                children.push(OwnedCatalogChildV1 {
                  edge: child.edge,
                  record_count: child.record_count,
                  object_id: child.object_id.to_vec(),
                });
              }
              stack.push(CatalogWalkFrameV1::Children { prefix, children, next_child: 0 });
            }
          }
        }
        CatalogWalkFrameV1::Children { prefix, children, next_child } => {
          let Some(child) = children.get(next_child).cloned() else {
            continue;
          };
          let mut child_prefix = prefix.clone();
          child_prefix.push(child.edge);
          if child_prefix.len() > hash_width {
            return Err(IndexSemanticScopeReadErrorV1::corrupt(
              "semantic_catalog_depth",
              "semantic catalog child prefix exceeds the database hash width",
            ));
          }
          stack.push(CatalogWalkFrameV1::Children { prefix, children, next_child: next_child + 1 });
          stack.push(CatalogWalkFrameV1::Visit {
            object_id: child.object_id,
            expected_prefix: child_prefix,
            expected_records: child.record_count,
          });
        }
      }
    }
    Ok(stats)
  }

  fn load_catalog_node(&self, object_id: &[u8]) -> Result<Vec<u8>, IndexSemanticScopeReadErrorV1> {
    let leaf = self.objects.load_semantic_object(0x0002, object_id)?;
    let internal = self.objects.load_semantic_object(0x0003, object_id)?;
    match (leaf, internal) {
      (Some(bytes), None) | (None, Some(bytes)) => Ok(bytes),
      (None, None) => Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_catalog_missing",
        format!("semantic catalog node {} is absent", hex::encode(object_id)),
      )),
      (Some(_), Some(_)) => Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_catalog_ambiguous",
        format!("semantic catalog node {} exists under both registered kinds", hex::encode(object_id)),
      )),
    }
  }

  fn with_definition<T>(
    &self,
    record: SemanticCatalogRecordV1<'_>,
    is_cancelled: &dyn Fn() -> bool,
    inspect: impl FnOnce(&[u8]) -> Result<T, IndexSemanticScopeReadErrorV1>,
  ) -> Result<T, IndexSemanticScopeReadErrorV1> {
    if is_cancelled() {
      return Err(IndexSemanticScopeReadErrorV1::cancelled("semantic_cancelled", "semantic definition read was cancelled"));
    }
    let bytes = self.objects.load_semantic_object(0x0004, record.definition_object_id)?.ok_or_else(|| {
      IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_definition_missing",
        format!("semantic definition {} is absent", hex::encode(record.definition_object_id)),
      )
    })?;
    let definition = decode_semantic_definition_record(&bytes, self.hash_algorithm)
      .map_err(|error| IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()))?;
    if definition.object_id != record.definition_object_id
      || definition.class != record.record_kind
      || definition.semantic_id != record.semantic_id
    {
      return Err(IndexSemanticScopeReadErrorV1::corrupt(
        "semantic_definition_closure",
        "semantic definition identity, class, or semantic ID disagrees with its catalog binding",
      ));
    }
    inspect(definition.definition)
  }
}

fn validate_definition_identity(record: SemanticCatalogRecordV1<'_>, actual: &[u8]) -> Result<(), IndexSemanticScopeReadErrorV1> {
  if actual != record.semantic_id || actual != record.owner_key {
    return Err(IndexSemanticScopeReadErrorV1::corrupt(
      "semantic_definition_identity",
      "decoded semantic definition identity disagrees with its semantic ID or owner key",
    ));
  }
  Ok(())
}

fn validate_walk_counts(stats: CatalogWalkStatsV1, expected: CatalogExpectedCountsV1) -> Result<(), IndexSemanticScopeReadErrorV1> {
  let dependencies = stats.class_counts[6]
    .checked_add(stats.class_counts[7])
    .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog dependency count overflow"))?;
  if stats.records != expected.records || stats.nodes != expected.nodes || dependencies != expected.dependencies {
    return Err(IndexSemanticScopeReadErrorV1::corrupt(
      "semantic_catalog_counts",
      format!(
        "catalog walk observed {} records, {} nodes, and {} dependencies; expected {}, {}, and {}",
        stats.records, stats.nodes, dependencies, expected.records, expected.nodes, expected.dependencies
      ),
    ));
  }
  let required_definition_count = stats.class_counts[3]
    .checked_add(stats.class_counts[4])
    .and_then(|value| value.checked_add(stats.class_counts[5]))
    .and_then(|value| value.checked_add(expected.dependencies))
    .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_catalog_count_overflow", "required definition count overflow"))?;
  if expected.definitions < required_definition_count {
    return Err(IndexSemanticScopeReadErrorV1::corrupt(
      "semantic_catalog_counts",
      "semantic-state definition count is smaller than its uniquely owned scope/value/field/dependency definitions",
    ));
  }
  Ok(())
}

fn enforce_count_limit(resource: &'static str, current: usize, limit: u32) -> Result<(), IndexSemanticScopeReadErrorV1> {
  if current >= limit as usize {
    return Err(IndexSemanticScopeReadErrorV1::corrupt(
      "semantic_limit_exceeded",
      format!("applicable {resource} exceed configured limit {limit}"),
    ));
  }
  Ok(())
}

fn retained_definition_bytes(total: u64, bytes: usize, limit: u64) -> Result<u64, IndexSemanticScopeReadErrorV1> {
  let bytes =
    u64::try_from(bytes).map_err(|error| IndexSemanticScopeReadErrorV1::corrupt("semantic_memory_overflow", error.to_string()))?;
  let total = total
    .checked_add(bytes)
    .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_memory_overflow", "definition byte count overflow"))?;
  if total > limit {
    return Err(IndexSemanticScopeReadErrorV1::corrupt(
      "semantic_limit_exceeded",
      format!("applicable definition bytes {total} exceed configured limit {limit}"),
    ));
  }
  Ok(total)
}

fn map_ordinal_error(error: IndexScopeOrdinalClaimErrorV1) -> IndexSemanticScopeReadErrorV1 {
  match error.class() {
    IndexScopeOrdinalClaimErrorClassV1::Cancelled => IndexSemanticScopeReadErrorV1::cancelled(error.code(), error.context()),
    IndexScopeOrdinalClaimErrorClassV1::Retryable => IndexSemanticScopeReadErrorV1::retryable(error.code(), error.context()),
    IndexScopeOrdinalClaimErrorClassV1::Corrupt => IndexSemanticScopeReadErrorV1::corrupt(error.code(), error.context()),
  }
}

fn semantic_reservation_bytes(
  hash_algorithm: HashAlgorithm,
  request: IndexSemanticScopeReadRequestV1<'_>,
) -> Result<u64, IndexSemanticScopeReadErrorV1> {
  semantic_limits_reservation_bytes(hash_algorithm, request.limits)
}

fn semantic_limits_reservation_bytes(
  hash_algorithm: HashAlgorithm,
  limits: super::index_producer_source::IndexSemanticScopeLimitsV1,
) -> Result<u64, IndexSemanticScopeReadErrorV1> {
  let hash_width = hash_algorithm.hash_length() as u64;
  let identity_count = u64::from(limits.max_scopes())
    .checked_mul(2)
    .and_then(|value| value.checked_add(u64::from(limits.max_value_stores()) * 2))
    .and_then(|value| value.checked_add(u64::from(limits.max_field_indexes()) * 2))
    .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_memory_overflow", "semantic identity count overflow"))?;
  SEMANTIC_TRAVERSAL_WORKSPACE_BYTES
    .checked_add(limits.max_definition_bytes())
    .and_then(|value| value.checked_add(identity_count.checked_mul(hash_width + 128)?))
    .and_then(|value| value.checked_add(hash_width * 4))
    .ok_or_else(|| IndexSemanticScopeReadErrorV1::corrupt("semantic_memory_overflow", "semantic reservation byte overflow"))
}

fn canonical_paths_overlap(left: &str, right: &str) -> bool {
  canonical_path_contains(left, right) || canonical_path_contains(right, left)
}

fn canonical_path_contains(parent: &str, child: &str) -> bool {
  parent == "/" || parent == child || child.strip_prefix(parent).is_some_and(|suffix| suffix.starts_with('/'))
}
