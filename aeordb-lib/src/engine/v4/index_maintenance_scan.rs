//! Bounded, root-pinned document scan contract for native index maintenance.
//!
//! The resume path is an in-process optimization over an immutable namespace
//! root. It is deliberately absent from the frozen producer-task payload: after
//! restart, replay begins at the task scope and relies on deterministic
//! mutation identity rather than trusting an unselected cursor.

use std::mem::size_of;

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;

use super::hash::digest_parts;
use super::index_producer_coordinator::IndexProducerTaskKindV1;

const MAINTENANCE_DOCUMENT_OPERATION_DOMAIN_V1: &[u8] = b"aeordb:index-maintenance-document-operation:v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexProducerServiceModeV1 {
  JournalTransition,
  AuthoritativeUpsertScan,
  AuthoritativeRetirementScan,
  ArtifactCompaction,
}

pub const fn index_producer_service_mode_v1(kind: IndexProducerTaskKindV1) -> IndexProducerServiceModeV1 {
  match kind {
    IndexProducerTaskKindV1::MutationWindow | IndexProducerTaskKindV1::Reconcile => IndexProducerServiceModeV1::JournalTransition,
    IndexProducerTaskKindV1::Build
    | IndexProducerTaskKindV1::Rebuild
    | IndexProducerTaskKindV1::Repair
    | IndexProducerTaskKindV1::ExplicitMutation
    | IndexProducerTaskKindV1::LegacyMigration => IndexProducerServiceModeV1::AuthoritativeUpsertScan,
    IndexProducerTaskKindV1::Retire => IndexProducerServiceModeV1::AuthoritativeRetirementScan,
    IndexProducerTaskKindV1::Compact => IndexProducerServiceModeV1::ArtifactCompaction,
  }
}

pub fn derive_index_maintenance_document_operation_id_v1(
  hash_algorithm: HashAlgorithm,
  parent_operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  namespace_root: &[u8],
  revision_hash: &[u8],
  path: &str,
) -> Result<[u8; 16], IndexMaintenanceScanErrorV1> {
  let mode = index_producer_service_mode_v1(kind);
  if !matches!(mode, IndexProducerServiceModeV1::AuthoritativeUpsertScan | IndexProducerServiceModeV1::AuthoritativeRetirementScan) {
    return Err(IndexMaintenanceScanErrorV1::InvalidRequest("only document-scan tasks have per-document operation identities".to_string()));
  }
  let hash_width = hash_algorithm.hash_length();
  if parent_operation_id == [0; 16]
    || namespace_root.len() != hash_width
    || namespace_root.iter().all(|byte| *byte == 0)
    || revision_hash.len() != hash_width
    || revision_hash.iter().all(|byte| *byte == 0)
  {
    return Err(IndexMaintenanceScanErrorV1::InvalidRequest(
      "maintenance document identity contains an absent or wrong-width owner, root, or revision".to_string(),
    ));
  }
  validate_request_path(path, "/", u32::MAX, "document")?;
  let kind_id = kind.id().to_le_bytes();
  let path_length = u64::try_from(path.len())
    .map_err(|error| IndexMaintenanceScanErrorV1::InvalidRequest(format!("document path length exceeds u64: {error}")))?
    .to_le_bytes();
  let digest = digest_parts(
    hash_algorithm,
    &[
      MAINTENANCE_DOCUMENT_OPERATION_DOMAIN_V1,
      &parent_operation_id,
      &kind_id,
      namespace_root,
      revision_hash,
      &path_length,
      path.as_bytes(),
    ],
  );
  let prefix = digest
    .get(..16)
    .ok_or_else(|| IndexMaintenanceScanErrorV1::InvalidRequest("document operation digest is shorter than 16 bytes".to_string()))?;
  let mut operation_id = [0u8; 16];
  operation_id.copy_from_slice(prefix);
  if operation_id == [0; 16] {
    return Err(IndexMaintenanceScanErrorV1::InvalidRequest("derived document operation identity is all zeroes".to_string()));
  }
  Ok(operation_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexMaintenanceScanLimitsV1 {
  maximum_documents: u32,
  maximum_retained_bytes: u64,
  maximum_path_bytes: u32,
}

impl IndexMaintenanceScanLimitsV1 {
  pub fn new(maximum_documents: u32, maximum_retained_bytes: u64, maximum_path_bytes: u32) -> Result<Self, IndexMaintenanceScanErrorV1> {
    if maximum_documents == 0 || maximum_retained_bytes == 0 || maximum_path_bytes == 0 {
      return Err(IndexMaintenanceScanErrorV1::InvalidOptions("all authoritative scan limits must be nonzero".to_string()));
    }
    Ok(Self { maximum_documents, maximum_retained_bytes, maximum_path_bytes })
  }

  pub const fn maximum_documents(self) -> u32 {
    self.maximum_documents
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }

  pub const fn maximum_path_bytes(self) -> u32 {
    self.maximum_path_bytes
  }
}

pub struct IndexMaintenanceScanRequestV1<'request> {
  pub namespace_root: &'request [u8],
  pub scope: &'request str,
  pub resume_after: Option<&'request str>,
  pub limits: IndexMaintenanceScanLimitsV1,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMaintenanceScanDocumentV1 {
  pub revision_hash: Vec<u8>,
  pub file_record: FileRecord,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMaintenanceScanPageV1 {
  pub documents: Vec<IndexMaintenanceScanDocumentV1>,
  pub next_resume_after: Option<String>,
  pub complete: bool,
  pub retained_bytes: u64,
}

pub struct IndexMaintenanceScanReadV1 {
  page: IndexMaintenanceScanPageV1,
  _reservation: MemoryReservation,
}

impl IndexMaintenanceScanReadV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    request: &IndexMaintenanceScanRequestV1<'_>,
    page: IndexMaintenanceScanPageV1,
    reservation: MemoryReservation,
  ) -> Result<Self, IndexMaintenanceScanErrorV1> {
    validate_index_maintenance_scan_page_v1(hash_algorithm, request, &page)?;
    if reservation.owner() != MemoryOwner::Task || reservation.bytes() < page.retained_bytes {
      return Err(IndexMaintenanceScanErrorV1::InvalidPage(
        "authoritative scan page is not covered by a task-memory reservation".to_string(),
      ));
    }
    Ok(Self { page, _reservation: reservation })
  }

  pub const fn page(&self) -> &IndexMaintenanceScanPageV1 {
    &self.page
  }

  pub fn into_parts(self) -> (IndexMaintenanceScanPageV1, MemoryReservation) {
    (self.page, self._reservation)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMaintenanceScanReadErrorClassV1 {
  Cancelled,
  Retryable,
  Corrupt,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("authoritative index maintenance scan failed ({code}): {context}")]
pub struct IndexMaintenanceScanReadErrorV1 {
  class: IndexMaintenanceScanReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexMaintenanceScanReadErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexMaintenanceScanReadErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn retryable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexMaintenanceScanReadErrorClassV1::Retryable, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: IndexMaintenanceScanReadErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexMaintenanceScanReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

pub trait IndexMaintenanceScanSourceV1: Send + Sync {
  /// Read one strictly ordered page from the exact immutable namespace root.
  ///
  /// Implementations must reserve before retaining page data, must not return
  /// paths at or before `resume_after`, and must retain that reservation in the
  /// returned value until the caller consumes or drops the page.
  fn scan(&self, request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexMaintenanceScanErrorV1 {
  #[error("invalid authoritative index scan options: {0}")]
  InvalidOptions(String),
  #[error("invalid authoritative index scan request: {0}")]
  InvalidRequest(String),
  #[error("invalid authoritative index scan page: {0}")]
  InvalidPage(String),
  #[error("authoritative index scan was cancelled")]
  Cancelled,
}

pub fn validate_index_maintenance_scan_page_v1(
  hash_algorithm: HashAlgorithm,
  request: &IndexMaintenanceScanRequestV1<'_>,
  page: &IndexMaintenanceScanPageV1,
) -> Result<(), IndexMaintenanceScanErrorV1> {
  validate_index_maintenance_scan_request_v1(hash_algorithm, request)?;
  if (request.is_cancelled)() {
    return Err(IndexMaintenanceScanErrorV1::Cancelled);
  }
  if page.documents.len() > request.limits.maximum_documents as usize {
    return Err(IndexMaintenanceScanErrorV1::InvalidPage("document count exceeds the bounded page limit".to_string()));
  }
  if page.retained_bytes > request.limits.maximum_retained_bytes {
    return Err(IndexMaintenanceScanErrorV1::InvalidPage("retained bytes exceed the bounded page limit".to_string()));
  }
  let minimum_retained = index_maintenance_scan_page_retained_bytes_v1(page)?;
  if page.retained_bytes < minimum_retained {
    return Err(IndexMaintenanceScanErrorV1::InvalidPage("declared retained bytes do not cover the returned documents".to_string()));
  }

  let hash_width = hash_algorithm.hash_length();
  let mut previous = request.resume_after;
  for document in &page.documents {
    let path = document.file_record.path.as_str();
    if document.revision_hash.len() != hash_width || document.revision_hash.iter().all(|byte| *byte == 0) {
      return Err(IndexMaintenanceScanErrorV1::InvalidPage("document revision hash is absent or has the wrong width".to_string()));
    }
    validate_page_path(path, request.scope, request.limits.maximum_path_bytes, "document")?;
    if previous.is_some_and(|prior| path <= prior) {
      return Err(IndexMaintenanceScanErrorV1::InvalidPage(
        "document paths are not strictly ordered after the resume boundary".to_string(),
      ));
    }
    previous = Some(path);
  }

  match (page.complete, page.next_resume_after.as_deref(), page.documents.last()) {
    (true, None, _) => Ok(()),
    (false, Some(next), Some(last)) if next == last.file_record.path => {
      validate_page_path(next, request.scope, request.limits.maximum_path_bytes, "resume")
    }
    (false, _, None) => Err(IndexMaintenanceScanErrorV1::InvalidPage("an incomplete scan page must make document progress".to_string())),
    (false, _, Some(_)) => {
      Err(IndexMaintenanceScanErrorV1::InvalidPage("an incomplete scan page must resume after its final document".to_string()))
    }
    (true, Some(_), _) => {
      Err(IndexMaintenanceScanErrorV1::InvalidPage("a complete scan page cannot retain a continuation path".to_string()))
    }
  }
}

pub(super) fn validate_index_maintenance_scan_request_v1(
  hash_algorithm: HashAlgorithm,
  request: &IndexMaintenanceScanRequestV1<'_>,
) -> Result<(), IndexMaintenanceScanErrorV1> {
  if request.namespace_root.len() != hash_algorithm.hash_length() || request.namespace_root.iter().all(|byte| *byte == 0) {
    return Err(IndexMaintenanceScanErrorV1::InvalidRequest("namespace root is absent or has the wrong hash width".to_string()));
  }
  validate_request_path(request.scope, "/", request.limits.maximum_path_bytes, "scope")?;
  if let Some(resume_after) = request.resume_after {
    validate_request_path(resume_after, request.scope, request.limits.maximum_path_bytes, "resume")?;
  }
  Ok(())
}

fn validate_request_path(path: &str, scope: &str, maximum_bytes: u32, role: &'static str) -> Result<(), IndexMaintenanceScanErrorV1> {
  if !scan_path_is_valid(path, scope, maximum_bytes) {
    return Err(IndexMaintenanceScanErrorV1::InvalidRequest(format!(
      "{role} path is not canonical, bounded, and inside the requested scope"
    )));
  }
  Ok(())
}

fn validate_page_path(path: &str, scope: &str, maximum_bytes: u32, role: &'static str) -> Result<(), IndexMaintenanceScanErrorV1> {
  if !scan_path_is_valid(path, scope, maximum_bytes) {
    return Err(IndexMaintenanceScanErrorV1::InvalidPage(format!("{role} path is not canonical, bounded, and inside the requested scope")));
  }
  Ok(())
}

fn scan_path_is_valid(path: &str, scope: &str, maximum_bytes: u32) -> bool {
  if path.is_empty()
    || !path.starts_with('/')
    || path.len() > maximum_bytes as usize
    || normalize_path(path) != path
    || !path_is_within_scope(scope, path)
  {
    return false;
  }
  true
}

fn path_is_within_scope(scope: &str, path: &str) -> bool {
  scope == "/" || path == scope || path.strip_prefix(scope).is_some_and(|suffix| suffix.starts_with('/'))
}

pub(super) fn index_maintenance_scan_page_retained_bytes_v1(page: &IndexMaintenanceScanPageV1) -> Result<u64, IndexMaintenanceScanErrorV1> {
  let mut retained = size_of::<IndexMaintenanceScanPageV1>();
  retained = retained
    .checked_add(
      page
        .documents
        .capacity()
        .checked_mul(size_of::<IndexMaintenanceScanDocumentV1>())
        .ok_or_else(|| IndexMaintenanceScanErrorV1::InvalidPage("document-vector retained-byte accounting overflowed".to_string()))?,
    )
    .ok_or_else(|| IndexMaintenanceScanErrorV1::InvalidPage("page retained-byte accounting overflowed".to_string()))?;
  retained = retained
    .checked_add(page.next_resume_after.as_ref().map_or(0, String::capacity))
    .ok_or_else(|| IndexMaintenanceScanErrorV1::InvalidPage("resume retained-byte accounting overflowed".to_string()))?;

  for document in &page.documents {
    let record = &document.file_record;
    retained = add_retained_capacity(retained, document.revision_hash.capacity(), "revision hash")?;
    retained = add_retained_capacity(retained, record.path.capacity(), "document path")?;
    retained = add_retained_capacity(retained, record.content_type.as_ref().map_or(0, String::capacity), "content type")?;
    retained = add_retained_capacity(retained, record.metadata.capacity(), "metadata")?;
    retained = add_retained_capacity(retained, record.content_hash.capacity(), "content hash")?;
    retained = add_retained_capacity(
      retained,
      record
        .chunk_hashes
        .capacity()
        .checked_mul(size_of::<Vec<u8>>())
        .ok_or_else(|| IndexMaintenanceScanErrorV1::InvalidPage("chunk-vector retained-byte accounting overflowed".to_string()))?,
      "chunk vector",
    )?;
    for chunk_hash in &record.chunk_hashes {
      retained = add_retained_capacity(retained, chunk_hash.capacity(), "chunk hash")?;
    }
  }

  u64::try_from(retained).map_err(|error| IndexMaintenanceScanErrorV1::InvalidPage(format!("page retained bytes exceed u64: {error}")))
}

fn add_retained_capacity(retained: usize, additional: usize, resource: &'static str) -> Result<usize, IndexMaintenanceScanErrorV1> {
  retained
    .checked_add(additional)
    .ok_or_else(|| IndexMaintenanceScanErrorV1::InvalidPage(format!("{resource} retained-byte accounting overflowed")))
}
