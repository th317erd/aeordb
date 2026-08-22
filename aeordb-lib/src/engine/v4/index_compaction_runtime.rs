//! Runtime contract for one bounded v4 artifact-compaction attempt.
//!
//! Candidate discovery and first-authority publication are supplied by the
//! native adapter. The runtime owner retains the durable producer task until
//! this contract proves either complete work or a safely published increment.

use thiserror::Error;

#[derive(Clone, Copy)]
pub struct IndexArtifactCompactionExecutionRequestV1<'request> {
  pub operation_id: [u8; 16],
  pub publication_sequence: u64,
  pub namespace_root: &'request [u8],
  pub semantic_state_root: &'request [u8],
  pub scope: &'request str,
  pub now_ms: u64,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexArtifactCompactionExecutionOutcomeV1 {
  Complete { published_owners: u32, publication_bytes: u64 },
  Progress { published_owners: u32, publication_bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexRuntimeCompactionErrorClassV1 {
  RetryableBeforeSelection,
  CancelledBeforeSelection,
  CommitUnknown,
  Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("index runtime compaction failed ({code}, {class:?}): {context}")]
pub struct IndexRuntimeCompactionErrorV1 {
  class: IndexRuntimeCompactionErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexRuntimeCompactionErrorV1 {
  pub fn new(class: IndexRuntimeCompactionErrorClassV1, code: &'static str, context: impl Into<String>) -> Self {
    Self { class, code, context: context.into() }
  }

  pub const fn class(&self) -> IndexRuntimeCompactionErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

pub trait IndexRuntimeCompactionExecutorV1 {
  /// Execute at most one bounded compaction candidate.
  ///
  /// `RetryableBeforeSelection` and `CancelledBeforeSelection` guarantee no
  /// pointer selection. Any unresolved result after selection must be
  /// classified as `CommitUnknown`.
  fn execute(
    &self,
    request: IndexArtifactCompactionExecutionRequestV1<'_>,
  ) -> Result<IndexArtifactCompactionExecutionOutcomeV1, IndexRuntimeCompactionErrorV1>;
}
