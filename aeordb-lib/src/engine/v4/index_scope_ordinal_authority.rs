//! Durable selected-state coordination for scope-local document ordinals.
//!
//! This module owns the storage-neutral compare-and-swap protocol. A concrete
//! store must not report `Committed` until the claim and high-water are both
//! durably selected. P6-2d binds that contract to checkpoint persistence.

use thiserror::Error;

use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::index_semantic_source::{
  IndexScopeOrdinalAuthorityV1, IndexScopeOrdinalClaimErrorV1, IndexScopeOrdinalClaimObservationV1, IndexScopeOrdinalClaimPlanV1,
  IndexScopeOrdinalClaimRequestV1, plan_scope_ordinal_claim,
};
use super::scope::validate_canonical_absolute_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScopeOrdinalStateStoreErrorClassV1 {
  Retryable,
  Corrupt,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("index scope ordinal state store failed ({code}): {context}")]
pub struct IndexScopeOrdinalStateStoreErrorV1 {
  class: IndexScopeOrdinalStateStoreErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexScopeOrdinalStateStoreErrorV1 {
  pub fn retryable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexScopeOrdinalStateStoreErrorClassV1::Retryable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexScopeOrdinalStateStoreErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexScopeOrdinalStateStoreErrorClassV1 {
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
pub struct IndexScopeOrdinalDurableClaimV1 {
  pub request_fingerprint: Vec<u8>,
  pub document_ordinal: u64,
  pub source_publication_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexScopeOrdinalSelectedObservationV1 {
  pub checkpoint_sequence: u64,
  pub checkpoint_key: Vec<u8>,
  pub generation: u64,
  pub scope_id: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
  pub next_document_ordinal: u64,
  pub pending_claim_count: u32,
  pub prior_operation_claim: Option<IndexScopeOrdinalDurableClaimV1>,
  pub before_live_ordinal: Option<u64>,
  pub after_live_ordinal: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexScopeOrdinalStoreObservationRequestV1<'request> {
  pub scope_id: &'request [u8],
  pub semantic_state_root: &'request [u8],
  pub operation_id: [u8; 16],
  pub before_file_key: Option<&'request [u8]>,
  pub after_file_key: Option<&'request [u8]>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexScopeOrdinalPublishRequestV1<'request> {
  pub expected_checkpoint_sequence: u64,
  pub expected_checkpoint_key: &'request [u8],
  pub generation: u64,
  pub scope_id: &'request [u8],
  pub semantic_state_root: &'request [u8],
  pub operation_id: [u8; 16],
  pub request_fingerprint: &'request [u8],
  pub document_ordinal: u64,
  pub next_document_ordinal: u64,
  pub source_publication_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexScopeOrdinalPublishOutcomeV1 {
  Committed,
  SelectionChanged,
}

/// Selector-last persistence boundary for one scope's shadow generation.
///
/// `observe_selected` must resolve live reverse mappings from the exact
/// selected generation it returns. `publish_selected_synced` must preserve all
/// other pending claims and may return `Committed` only after the supplied
/// claim and high-water are durably selected together.
pub trait IndexScopeOrdinalStateStoreV1: Send + Sync {
  fn observe_selected(
    &self,
    request: IndexScopeOrdinalStoreObservationRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalSelectedObservationV1, IndexScopeOrdinalStateStoreErrorV1>;

  fn publish_selected_synced(
    &self,
    request: IndexScopeOrdinalPublishRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalStateStoreErrorV1>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexScopeOrdinalStateOptionsV1 {
  maximum_selection_attempts: u16,
  maximum_pending_claims: u32,
}

impl IndexScopeOrdinalStateOptionsV1 {
  pub fn new(maximum_selection_attempts: u16, maximum_pending_claims: u32) -> Result<Self, IndexScopeOrdinalClaimErrorV1> {
    if maximum_selection_attempts == 0 || maximum_pending_claims == 0 {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_options",
        "maximum selector attempts and pending claims must be nonzero",
      ));
    }
    Ok(Self { maximum_selection_attempts, maximum_pending_claims })
  }

  pub const fn maximum_selection_attempts(self) -> u16 {
    self.maximum_selection_attempts
  }

  pub const fn maximum_pending_claims(self) -> u32 {
    self.maximum_pending_claims
  }
}

pub struct DurableIndexScopeOrdinalAuthorityV1<'store, Store> {
  hash_algorithm: HashAlgorithm,
  store: &'store Store,
  options: IndexScopeOrdinalStateOptionsV1,
}

impl<'store, Store> DurableIndexScopeOrdinalAuthorityV1<'store, Store>
where
  Store: IndexScopeOrdinalStateStoreV1,
{
  pub const fn new(hash_algorithm: HashAlgorithm, store: &'store Store, options: IndexScopeOrdinalStateOptionsV1) -> Self {
    Self { hash_algorithm, store, options }
  }
}

impl<Store> IndexScopeOrdinalAuthorityV1 for DurableIndexScopeOrdinalAuthorityV1<'_, Store>
where
  Store: IndexScopeOrdinalStateStoreV1,
{
  fn claim_scope_ordinal(&self, request: IndexScopeOrdinalClaimRequestV1<'_>) -> Result<u64, IndexScopeOrdinalClaimErrorV1> {
    let transition = validate_claim_request(self.hash_algorithm, request)?;
    let request_fingerprint = fingerprint_claim_request(self.hash_algorithm, request)?;

    for _ in 0..self.options.maximum_selection_attempts() {
      if (request.is_cancelled)() {
        return Err(IndexScopeOrdinalClaimErrorV1::cancelled(
          "scope_ordinal_cancelled",
          "scope ordinal claim was cancelled before selected-state observation",
        ));
      }
      let selected = self
        .store
        .observe_selected(IndexScopeOrdinalStoreObservationRequestV1 {
          scope_id: request.scope_id,
          semantic_state_root: request.semantic_state_root,
          operation_id: request.operation_id,
          before_file_key: transition.before_file_key.as_deref(),
          after_file_key: transition.after_file_key.as_deref(),
        })
        .map_err(map_store_error)?;
      validate_selected(self.hash_algorithm, request, &selected, &request_fingerprint)?;

      let prior_operation_claim = selected.prior_operation_claim.as_ref().map(|claim| claim.document_ordinal);
      let plan = plan_scope_ordinal_claim(
        request,
        IndexScopeOrdinalClaimObservationV1 {
          prior_operation_claim,
          before_live_ordinal: selected.before_live_ordinal,
          after_live_ordinal: selected.after_live_ordinal,
          next_document_ordinal: selected.next_document_ordinal,
        },
      )?;
      if selected.prior_operation_claim.is_some() {
        return Ok(plan.document_ordinal());
      }
      if selected.pending_claim_count >= self.options.maximum_pending_claims() {
        return Err(IndexScopeOrdinalClaimErrorV1::retryable(
          "scope_ordinal_claim_pressure",
          "selected scope ordinal state reached its pending-claim limit and must be checkpointed",
        ));
      }
      let (document_ordinal, next_document_ordinal) = match plan {
        IndexScopeOrdinalClaimPlanV1::Reuse { document_ordinal } => (document_ordinal, selected.next_document_ordinal),
        IndexScopeOrdinalClaimPlanV1::Allocate { document_ordinal, next_document_ordinal } => (document_ordinal, next_document_ordinal),
      };
      if (request.is_cancelled)() {
        return Err(IndexScopeOrdinalClaimErrorV1::cancelled(
          "scope_ordinal_cancelled",
          "scope ordinal claim was cancelled before durable selection",
        ));
      }
      match self
        .store
        .publish_selected_synced(IndexScopeOrdinalPublishRequestV1 {
          expected_checkpoint_sequence: selected.checkpoint_sequence,
          expected_checkpoint_key: &selected.checkpoint_key,
          generation: selected.generation,
          scope_id: request.scope_id,
          semantic_state_root: request.semantic_state_root,
          operation_id: request.operation_id,
          request_fingerprint: &request_fingerprint,
          document_ordinal,
          next_document_ordinal,
          source_publication_sequence: request.source_publication_sequence,
        })
        .map_err(map_store_error)?
      {
        IndexScopeOrdinalPublishOutcomeV1::Committed => return Ok(document_ordinal),
        IndexScopeOrdinalPublishOutcomeV1::SelectionChanged => {}
      }
    }

    Err(IndexScopeOrdinalClaimErrorV1::retryable(
      "scope_ordinal_selection_busy",
      "selected scope ordinal state changed throughout the bounded retry window",
    ))
  }
}

impl IndexScopeOrdinalClaimPlanV1 {
  const fn document_ordinal(self) -> u64 {
    match self {
      Self::Reuse { document_ordinal } | Self::Allocate { document_ordinal, .. } => document_ordinal,
    }
  }
}

struct ValidatedTransitionV1 {
  before_file_key: Option<Vec<u8>>,
  after_file_key: Option<Vec<u8>>,
}

fn validate_claim_request(
  hash_algorithm: HashAlgorithm,
  request: IndexScopeOrdinalClaimRequestV1<'_>,
) -> Result<ValidatedTransitionV1, IndexScopeOrdinalClaimErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  if request.operation_id.iter().all(|byte| *byte == 0)
    || request.source_publication_sequence == 0
    || !valid_hash(request.semantic_state_root, hash_width)
    || !valid_hash(request.scope_id, hash_width)
  {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
      "scope_ordinal_request_identity",
      "operation, source publication sequence, semantic-state, or scope identity is zero or has the wrong width",
    ));
  }
  if request.before_in_scope && request.transition.before.is_none() || request.after_in_scope && request.transition.after.is_none() {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
      "scope_ordinal_request_transition",
      "scope membership names a missing document revision",
    ));
  }
  let before_file_key = validate_document(hash_algorithm, request.transition.before.as_ref())?;
  let after_file_key = validate_document(hash_algorithm, request.transition.after.as_ref())?;
  Ok(ValidatedTransitionV1 { before_file_key, after_file_key })
}

fn validate_document(
  hash_algorithm: HashAlgorithm,
  document: Option<&super::index_producer_source::ResolvedIndexDocumentV1>,
) -> Result<Option<Vec<u8>>, IndexScopeOrdinalClaimErrorV1> {
  let Some(document) = document else {
    return Ok(None);
  };
  let hash_width = hash_algorithm.hash_length();
  if !valid_hash(&document.namespace_root, hash_width) || !valid_hash(&document.revision_hash, hash_width) {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
      "scope_ordinal_request_transition",
      "document namespace root or revision is zero or has the wrong width",
    ));
  }
  validate_canonical_absolute_path(&document.file_record.path).map_err(|error| {
    IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_request_transition", format!("document path is not canonical: {error}"))
  })?;
  Ok(Some(digest_parts(hash_algorithm, &[b"file:", document.file_record.path.as_bytes()])))
}

fn fingerprint_claim_request(
  hash_algorithm: HashAlgorithm,
  request: IndexScopeOrdinalClaimRequestV1<'_>,
) -> Result<Vec<u8>, IndexScopeOrdinalClaimErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  let mut bytes = Vec::new();
  let path_bytes = request
    .transition
    .before
    .as_ref()
    .map_or(0usize, |document| document.file_record.path.len())
    .checked_add(request.transition.after.as_ref().map_or(0usize, |document| document.file_record.path.len()))
    .ok_or_else(|| IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_request_fingerprint", "transition path bytes overflow"))?;
  let capacity = 13usize
    .checked_add(2 * hash_width)
    .and_then(|value| value.checked_add(2 * (1 + 2 * hash_width + 4)))
    .and_then(|value| value.checked_add(path_bytes))
    .ok_or_else(|| IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_request_fingerprint", "transition fingerprint size overflow"))?;
  bytes.try_reserve_exact(capacity).map_err(|error| {
    IndexScopeOrdinalClaimErrorV1::retryable("scope_ordinal_fingerprint_allocation", format!("fingerprint allocation failed: {error}"))
  })?;
  bytes.extend_from_slice(b"SOC1");
  bytes.extend_from_slice(&request.source_publication_sequence.to_le_bytes());
  bytes.extend_from_slice(request.semantic_state_root);
  bytes.extend_from_slice(request.scope_id);
  bytes.push(u8::from(request.before_in_scope) | (u8::from(request.after_in_scope) << 1));
  append_fingerprint_side(&mut bytes, request.transition.before.as_ref())?;
  append_fingerprint_side(&mut bytes, request.transition.after.as_ref())?;
  Ok(digest_parts(hash_algorithm, &[b"aeordb.scope-ordinal-claim.v1\0", &bytes]))
}

fn append_fingerprint_side(
  output: &mut Vec<u8>,
  document: Option<&super::index_producer_source::ResolvedIndexDocumentV1>,
) -> Result<(), IndexScopeOrdinalClaimErrorV1> {
  let Some(document) = document else {
    output.push(0);
    return Ok(());
  };
  output.push(1);
  output.extend_from_slice(&document.namespace_root);
  output.extend_from_slice(&document.revision_hash);
  let path_length = u32::try_from(document.file_record.path.len()).map_err(|error| {
    IndexScopeOrdinalClaimErrorV1::corrupt("scope_ordinal_request_fingerprint", format!("path length does not fit u32: {error}"))
  })?;
  output.extend_from_slice(&path_length.to_le_bytes());
  output.extend_from_slice(document.file_record.path.as_bytes());
  Ok(())
}

fn validate_selected(
  hash_algorithm: HashAlgorithm,
  request: IndexScopeOrdinalClaimRequestV1<'_>,
  selected: &IndexScopeOrdinalSelectedObservationV1,
  request_fingerprint: &[u8],
) -> Result<(), IndexScopeOrdinalClaimErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  if selected.checkpoint_sequence == 0
    || selected.generation == 0
    || !valid_hash(&selected.checkpoint_key, hash_width)
    || !valid_hash(&selected.scope_id, hash_width)
    || !valid_hash(&selected.semantic_state_root, hash_width)
    || selected.next_document_ordinal == 0
  {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
      "scope_ordinal_selected_state",
      "selected scope ordinal checkpoint identity, generation, or high-water is malformed",
    ));
  }
  if selected.scope_id != request.scope_id || selected.semantic_state_root != request.semantic_state_root {
    return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
      "scope_ordinal_selected_identity",
      "selected scope ordinal state disagrees with the requested scope or semantic root",
    ));
  }
  if let Some(claim) = &selected.prior_operation_claim {
    if !valid_hash(&claim.request_fingerprint, hash_width)
      || claim.document_ordinal == 0
      || claim.document_ordinal >= selected.next_document_ordinal
      || claim.source_publication_sequence == 0
    {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_selected_claim",
        "selected operation claim has a malformed fingerprint, ordinal, or source publication sequence",
      ));
    }
    if claim.source_publication_sequence != request.source_publication_sequence {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_operation_sequence_conflict",
        "operation identity was already claimed at a different source publication sequence",
      ));
    }
    if claim.request_fingerprint != request_fingerprint {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_operation_conflict",
        "operation identity was already claimed by a different scope transition",
      ));
    }
    if selected.before_live_ordinal.is_some_and(|ordinal| ordinal != claim.document_ordinal)
      || selected.after_live_ordinal.is_some_and(|ordinal| ordinal != claim.document_ordinal)
    {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_claim_mapping_conflict",
        "durable operation claim disagrees with a selected live reverse mapping",
      ));
    }
  }
  for (label, ordinal) in [("before", selected.before_live_ordinal), ("after", selected.after_live_ordinal)] {
    if ordinal.is_some_and(|ordinal| ordinal == 0 || ordinal >= selected.next_document_ordinal) {
      return Err(IndexScopeOrdinalClaimErrorV1::corrupt(
        "scope_ordinal_selected_mapping",
        format!("selected {label} reverse mapping is zero or reaches the next-document high-water"),
      ));
    }
  }
  Ok(())
}

fn map_store_error(error: IndexScopeOrdinalStateStoreErrorV1) -> IndexScopeOrdinalClaimErrorV1 {
  match error.class() {
    IndexScopeOrdinalStateStoreErrorClassV1::Retryable => IndexScopeOrdinalClaimErrorV1::retryable(error.code(), error.context()),
    IndexScopeOrdinalStateStoreErrorClassV1::Corrupt => IndexScopeOrdinalClaimErrorV1::corrupt(error.code(), error.context()),
  }
}

fn valid_hash(bytes: &[u8], hash_width: usize) -> bool {
  bytes.len() == hash_width && bytes.iter().any(|byte| *byte != 0)
}
