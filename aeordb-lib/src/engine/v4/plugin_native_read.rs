//! Inactive root-aware plugin file reads over captured v4 authority.
//!
//! This module does not register a host import or invoke a plugin. It only
//! adapts an exact row from an already-authorized selected namespace reader to
//! the shared selected-file body primitive. Public plugin and SDK activation
//! remains a coordinated P7-5/P8 concern.

use std::error::Error;
use std::fmt;

use tokio_util::sync::CancellationToken;

use super::read_view_native::{
  NativeSelectedFileBodyLimitsV1, NativeSelectedFileBodyV1, NativeSelectedNamespaceFileRowV1, NativeSelectedNamespaceReadErrorClassV1,
  NativeSelectedNamespaceReadErrorV1, NativeSelectedNamespaceReaderV1,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRootAwarePluginReadErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  HistoricalViewUnavailable,
  CorruptSource,
  Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeRootAwarePluginReadErrorV1 {
  RequestCancelled,
  SelectedBody(NativeSelectedNamespaceReadErrorV1),
}

impl NativeRootAwarePluginReadErrorV1 {
  pub const fn class(&self) -> NativeRootAwarePluginReadErrorClassV1 {
    match self {
      Self::RequestCancelled => NativeRootAwarePluginReadErrorClassV1::Cancelled,
      Self::SelectedBody(error) => match error.class() {
        NativeSelectedNamespaceReadErrorClassV1::InvalidRequest => NativeRootAwarePluginReadErrorClassV1::InvalidRequest,
        NativeSelectedNamespaceReadErrorClassV1::ResourceLimit => NativeRootAwarePluginReadErrorClassV1::ResourceLimit,
        NativeSelectedNamespaceReadErrorClassV1::Unavailable => NativeRootAwarePluginReadErrorClassV1::HistoricalViewUnavailable,
        NativeSelectedNamespaceReadErrorClassV1::Corrupt => NativeRootAwarePluginReadErrorClassV1::CorruptSource,
        NativeSelectedNamespaceReadErrorClassV1::Cancelled => NativeRootAwarePluginReadErrorClassV1::Cancelled,
      },
    }
  }

  pub const fn code(&self) -> &'static str {
    match self {
      Self::RequestCancelled => "native_plugin_read_cancelled",
      Self::SelectedBody(error) => error.code(),
    }
  }

  pub fn context(&self) -> &str {
    match self {
      Self::RequestCancelled => "root-aware plugin read was cancelled",
      Self::SelectedBody(error) => error.context(),
    }
  }
}

impl fmt::Display for NativeRootAwarePluginReadErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code(), self.context())
  }
}

impl Error for NativeRootAwarePluginReadErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::RequestCancelled => None,
      Self::SelectedBody(error) => Some(error),
    }
  }
}

impl From<NativeSelectedNamespaceReadErrorV1> for NativeRootAwarePluginReadErrorV1 {
  fn from(error: NativeSelectedNamespaceReadErrorV1) -> Self {
    Self::SelectedBody(error)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeRootAwarePluginReadLimitsV1 {
  selected_body: NativeSelectedFileBodyLimitsV1,
}

impl NativeRootAwarePluginReadLimitsV1 {
  pub fn new(maximum_body_bytes: u64, maximum_chunks: usize) -> Result<Self, NativeRootAwarePluginReadErrorV1> {
    NativeSelectedFileBodyLimitsV1::new(maximum_body_bytes, maximum_chunks).map(|selected_body| Self { selected_body }).map_err(Into::into)
  }
}

pub struct NativeRootAwarePluginReadAdapterV1<'reader, 'view> {
  reader: &'reader NativeSelectedNamespaceReaderV1<'view>,
}

impl<'reader, 'view> NativeRootAwarePluginReadAdapterV1<'reader, 'view> {
  pub const fn new(reader: &'reader NativeSelectedNamespaceReaderV1<'view>) -> Self {
    Self { reader }
  }

  pub fn read_file_v1<'row>(
    &self,
    row: &'row NativeSelectedNamespaceFileRowV1,
    cancellation: &CancellationToken,
    limits: NativeRootAwarePluginReadLimitsV1,
  ) -> Result<NativeRootAwarePluginFileReadV1<'view, 'row>, NativeRootAwarePluginReadErrorV1> {
    require_not_cancelled(cancellation)?;
    let body = self.reader.read_file_body(row, limits.selected_body)?;
    require_not_cancelled(cancellation)?;
    Ok(NativeRootAwarePluginFileReadV1 { selected_namespace_root: self.reader.selected_namespace_root_for_adapter_v1(), row, body })
  }
}

pub struct NativeRootAwarePluginFileReadV1<'view, 'row> {
  selected_namespace_root: &'view [u8],
  row: &'row NativeSelectedNamespaceFileRowV1,
  body: NativeSelectedFileBodyV1,
}

impl NativeRootAwarePluginFileReadV1<'_, '_> {
  pub const fn selected_namespace_root(&self) -> &[u8] {
    self.selected_namespace_root
  }

  pub fn file_key(&self) -> &[u8] {
    self.row.file_key()
  }

  pub fn record_revision(&self) -> &[u8] {
    self.row.record_revision()
  }

  pub fn path(&self) -> &str {
    self.row.path()
  }

  pub fn content_type(&self) -> Option<&str> {
    self.row.file_record().content_type.as_deref()
  }

  pub const fn source_size(&self) -> u64 {
    self.row.file_record().total_size
  }

  pub const fn created_at(&self) -> i64 {
    self.row.file_record().created_at
  }

  pub const fn updated_at(&self) -> i64 {
    self.row.file_record().updated_at
  }

  pub fn content_hash(&self) -> &[u8] {
    &self.row.file_record().content_hash
  }

  pub fn bytes(&self) -> &[u8] {
    self.body.as_bytes()
  }
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), NativeRootAwarePluginReadErrorV1> {
  if cancellation.is_cancelled() {
    return Err(NativeRootAwarePluginReadErrorV1::RequestCancelled);
  }
  Ok(())
}
