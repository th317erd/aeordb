use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::file_record::FileRecord;

use super::hash::digest_parts;
use super::index_producer_admission::{IndexProducerJournalAdmissionErrorV1, derive_mutation_operation_id};
use super::index_producer_collector::{IndexCollectorDocumentRevisionTransitionV1, IndexCollectorDocumentV1};
use super::index_producer_coordinator::{
  IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1, IndexProducerLeaseV1, IndexProducerTaskKindV1,
};
use super::index_task::{MutationJournalV1, MutationRecordV1};
use super::reader::FormatError;

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

pub trait IndexFileRevisionSourceV1: Send + Sync {
  fn load_file_revision(
    &self,
    namespace_root: &[u8],
    path: &str,
  ) -> Result<Option<LoadedIndexFileRevisionV1>, IndexFileRevisionReadErrorV1>;
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
) -> Result<ResolvedIndexDocumentTransitionV1, IndexProducerSourceErrorV1> {
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
  Ok(ResolvedIndexDocumentTransitionV1 { before, after })
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
) -> Result<Option<ResolvedIndexDocumentV1>, IndexProducerSourceErrorV1> {
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
  let loaded = match source.load_file_revision(side.namespace_root, path) {
    Ok(Some(loaded)) => loaded,
    Ok(None) => {
      return Err(IndexProducerSourceErrorV1::MissingRevision { root: hex::encode(side.namespace_root), path: path.to_string() });
    }
    Err(error) if error.class() == IndexFileRevisionReadErrorClassV1::Cancelled => {
      return Err(IndexProducerSourceErrorV1::Cancelled);
    }
    Err(error) => return Err(IndexProducerSourceErrorV1::RevisionRead(error)),
  };
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
  Ok(Some(ResolvedIndexDocumentV1 {
    namespace_root: side.namespace_root.to_vec(),
    revision_hash: loaded.revision_hash,
    file_record: loaded.file_record,
  }))
}
