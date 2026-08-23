//! Migration-qualified lifecycle owner for the selected index-coverage cache.
//!
//! Persistent selection remains owned by first authority. This owner retains
//! only one bounded immutable metadata snapshot plus refresh diagnostics and
//! routes refreshes through the runtime's existing shared cadence.

use std::mem::size_of;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::admission::CapabilitySetV1;
use super::first_authority::V4FirstAuthorityPublisher;
use super::index_coverage_registry::{
  FirstAuthorityIndexCoverageRegistrySourceV1, IndexCoverageNvtStatusV1, IndexCoverageRegistryErrorV1, IndexCoverageRegistryOptionsV1,
  IndexCoverageRegistryOwnerRequestV1, IndexCoverageRegistrySelectionV1, IndexCoverageRegistrySnapshotV1,
  IndexCoverageRegistrySourceErrorV1, IndexCoverageRegistryV1,
};
use super::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryErrorV1, IndexScopeOrdinalStoreRegistrySnapshotV1, IndexScopeOrdinalStoreRegistryV1,
};

const MAXIMUM_FAILURE_CONTEXT_BYTES: usize = 16 * 1_024;
const OWNER_REQUEST_SET_FIXED_ALLOWANCE: u64 = 256;
const OWNER_REQUEST_FIXED_ALLOWANCE: u64 = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoverageRuntimeFailureV1 {
  pub code: &'static str,
  pub context: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoverageRuntimeSnapshotV1 {
  pub refresh_attempts: u64,
  pub successful_refreshes: u64,
  pub failed_refreshes: u64,
  pub refresh_pending: bool,
  pub registry_entries: usize,
  pub registry_retained_bytes: u64,
  pub owner_requests_retained_bytes: u64,
  pub total_retained_bytes: u64,
  pub selected_generations: usize,
  pub unavailable_generations: usize,
  pub usable_nvt_generations: usize,
  pub last_failure: Option<IndexCoverageRuntimeFailureV1>,
  pub scope_ordinal_cache: IndexScopeOrdinalStoreRegistrySnapshotV1,
}

#[derive(Default)]
struct IndexCoverageRuntimeStateV1 {
  refresh_attempts: u64,
  successful_refreshes: u64,
  failed_refreshes: u64,
  refresh_pending: bool,
  last_failure: Option<IndexCoverageRuntimeFailureV1>,
}

pub struct NativeIndexCoverageRuntimeV1 {
  registry: Arc<IndexCoverageRegistryV1>,
  requests: Vec<IndexCoverageRegistryOwnerRequestV1>,
  owner_requests_retained_bytes: u64,
  _owner_requests_reservation: MemoryReservation,
  publisher: Arc<V4FirstAuthorityPublisher>,
  scope_ordinal_cache: Arc<IndexScopeOrdinalStoreRegistryV1>,
  cancellation: CancellationToken,
  state: Mutex<IndexCoverageRuntimeStateV1>,
}

impl NativeIndexCoverageRuntimeV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    hash_algorithm: HashAlgorithm,
    database_id: [u8; 16],
    supported_reader_capabilities: CapabilitySetV1,
    options: IndexCoverageRegistryOptionsV1,
    requests: &[IndexCoverageRegistryOwnerRequestV1],
    publisher: Arc<V4FirstAuthorityPublisher>,
    scope_ordinal_cache: Arc<IndexScopeOrdinalStoreRegistryV1>,
    memory: Arc<MemoryCoordinator>,
    cancellation: CancellationToken,
  ) -> Result<Self, IndexCoverageRuntimeErrorV1> {
    let registry =
      Arc::new(IndexCoverageRegistryV1::new(hash_algorithm, database_id, supported_reader_capabilities, options, Arc::clone(&memory))?);
    registry.validate_owner_requests(requests)?;
    let owner_requests_retained_bytes = owner_requests_retained_bound(hash_algorithm, requests.len())?;
    let owner_requests_reservation = memory.reserve(MemoryOwner::IndexCleanCache, owner_requests_retained_bytes, AdmissionClass::Cache)?;
    let mut retained_requests = Vec::new();
    retained_requests.try_reserve_exact(requests.len()).map_err(|error| IndexCoverageRuntimeErrorV1::Allocation(error.to_string()))?;
    for request in requests {
      retained_requests.push(request.try_clone_retained()?);
    }
    Ok(Self {
      registry,
      requests: retained_requests,
      owner_requests_retained_bytes,
      _owner_requests_reservation: owner_requests_reservation,
      publisher,
      scope_ordinal_cache,
      cancellation,
      state: Mutex::new(IndexCoverageRuntimeStateV1 { refresh_pending: true, ..IndexCoverageRuntimeStateV1::default() }),
    })
  }

  pub fn refresh(&self) -> Result<Arc<IndexCoverageRegistrySnapshotV1>, IndexCoverageRuntimeErrorV1> {
    self.begin_refresh_attempt()?;
    let result = (|| {
      let mut source = FirstAuthorityIndexCoverageRegistrySourceV1::new(Arc::clone(&self.publisher))?;
      self.registry.refresh(&mut source, &self.requests, &self.cancellation).map_err(IndexCoverageRuntimeErrorV1::from)
    })();
    match result {
      Ok(snapshot) => {
        self.finish_refresh_success()?;
        Ok(snapshot)
      }
      Err(error) => {
        self.finish_refresh_failure(&error)?;
        Err(error)
      }
    }
  }

  pub fn refresh_if_pending(&self) -> Result<Option<Arc<IndexCoverageRegistrySnapshotV1>>, IndexCoverageRuntimeErrorV1> {
    if !self.lock_state()?.refresh_pending {
      return Ok(None);
    }
    self.refresh().map(Some)
  }

  pub fn mark_refresh_pending(&self) -> Result<(), IndexCoverageRuntimeErrorV1> {
    self.lock_state()?.refresh_pending = true;
    Ok(())
  }

  pub fn registry_snapshot(&self) -> Result<Arc<IndexCoverageRegistrySnapshotV1>, IndexCoverageRuntimeErrorV1> {
    self.registry.snapshot().map_err(IndexCoverageRuntimeErrorV1::from)
  }

  pub fn snapshot(&self) -> Result<IndexCoverageRuntimeSnapshotV1, IndexCoverageRuntimeErrorV1> {
    let registry = self.registry.snapshot()?;
    let scope_ordinal_cache = self.scope_ordinal_cache.snapshot()?;
    let state = self.lock_state()?;
    let selected_generations =
      registry.entries().iter().filter(|entry| matches!(entry.selection(), IndexCoverageRegistrySelectionV1::Selected(_))).count();
    let unavailable_generations = registry.entries().len().saturating_sub(selected_generations);
    let usable_nvt_generations =
      registry.entries().iter().filter(|entry| matches!(entry.nvt_status(), IndexCoverageNvtStatusV1::Usable(_))).count();
    let total_retained_bytes =
      registry.retained_bytes().checked_add(self.owner_requests_retained_bytes).ok_or(IndexCoverageRuntimeErrorV1::ArithmeticOverflow)?;
    Ok(IndexCoverageRuntimeSnapshotV1 {
      refresh_attempts: state.refresh_attempts,
      successful_refreshes: state.successful_refreshes,
      failed_refreshes: state.failed_refreshes,
      refresh_pending: state.refresh_pending,
      registry_entries: registry.len(),
      registry_retained_bytes: registry.retained_bytes(),
      owner_requests_retained_bytes: self.owner_requests_retained_bytes,
      total_retained_bytes,
      selected_generations,
      unavailable_generations,
      usable_nvt_generations,
      last_failure: state.last_failure.clone(),
      scope_ordinal_cache,
    })
  }

  pub fn evict_all_unpinned(&self) -> Result<u64, IndexCoverageRuntimeErrorV1> {
    self.scope_ordinal_cache.evict_all_unpinned().map_err(IndexCoverageRuntimeErrorV1::from)
  }

  fn begin_refresh_attempt(&self) -> Result<(), IndexCoverageRuntimeErrorV1> {
    let mut state = self.lock_state()?;
    state.refresh_attempts = state.refresh_attempts.checked_add(1).ok_or(IndexCoverageRuntimeErrorV1::ArithmeticOverflow)?;
    state.refresh_pending = true;
    Ok(())
  }

  fn finish_refresh_success(&self) -> Result<(), IndexCoverageRuntimeErrorV1> {
    let mut state = self.lock_state()?;
    state.successful_refreshes = state.successful_refreshes.checked_add(1).ok_or(IndexCoverageRuntimeErrorV1::ArithmeticOverflow)?;
    state.refresh_pending = false;
    state.last_failure = None;
    Ok(())
  }

  fn finish_refresh_failure(&self, error: &IndexCoverageRuntimeErrorV1) -> Result<(), IndexCoverageRuntimeErrorV1> {
    let failure = IndexCoverageRuntimeFailureV1 { code: error.code(), context: bounded_context(error.to_string()) };
    let mut state = self.lock_state()?;
    state.failed_refreshes = state.failed_refreshes.checked_add(1).ok_or(IndexCoverageRuntimeErrorV1::ArithmeticOverflow)?;
    state.refresh_pending = true;
    state.last_failure = Some(failure);
    Ok(())
  }

  fn lock_state(&self) -> Result<MutexGuard<'_, IndexCoverageRuntimeStateV1>, IndexCoverageRuntimeErrorV1> {
    self.state.lock().map_err(|error| IndexCoverageRuntimeErrorV1::Poisoned(error.to_string()))
  }
}

fn owner_requests_retained_bound(hash_algorithm: HashAlgorithm, request_count: usize) -> Result<u64, IndexCoverageRuntimeErrorV1> {
  // Every Rust target supported by AeorDB has a pointer width no wider than
  // this accounting type; the checked operations below retain overflow proof.
  let count = request_count as u64;
  let request_struct = size_of::<IndexCoverageRegistryOwnerRequestV1>() as u64;
  let owner_id = hash_algorithm.hash_length() as u64;
  request_struct
    .checked_add(OWNER_REQUEST_FIXED_ALLOWANCE)
    .and_then(|bytes| bytes.checked_add(owner_id))
    .and_then(|bytes| bytes.checked_mul(count))
    .and_then(|bytes| bytes.checked_add(OWNER_REQUEST_SET_FIXED_ALLOWANCE))
    .ok_or(IndexCoverageRuntimeErrorV1::ArithmeticOverflow)
}

#[derive(Debug, Error)]
pub enum IndexCoverageRuntimeErrorV1 {
  #[error("index coverage runtime registry failed: {0}")]
  Registry(#[from] IndexCoverageRegistryErrorV1),
  #[error("index coverage runtime source failed: {0}")]
  Source(#[from] IndexCoverageRegistrySourceErrorV1),
  #[error("index coverage runtime scope cache failed: {0}")]
  ScopeCache(#[from] IndexScopeOrdinalStoreRegistryErrorV1),
  #[error("index coverage runtime memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("index coverage runtime allocation failed: {0}")]
  Allocation(String),
  #[error("index coverage runtime accounting overflowed")]
  ArithmeticOverflow,
  #[error("index coverage runtime lock is poisoned: {0}")]
  Poisoned(String),
}

impl IndexCoverageRuntimeErrorV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::Registry(IndexCoverageRegistryErrorV1::Cancelled) => "index_coverage_refresh_cancelled",
      Self::Registry(IndexCoverageRegistryErrorV1::Invalid { .. }) => "index_coverage_refresh_invalid",
      Self::Registry(IndexCoverageRegistryErrorV1::Corrupt { .. }) => "index_coverage_refresh_corrupt",
      Self::Registry(IndexCoverageRegistryErrorV1::Source(_)) | Self::Source(_) => "index_coverage_refresh_source",
      Self::Registry(IndexCoverageRegistryErrorV1::SelectionChanged) => "index_coverage_refresh_selection_changed",
      Self::Registry(IndexCoverageRegistryErrorV1::RefreshBusy) => "index_coverage_refresh_busy",
      Self::Registry(IndexCoverageRegistryErrorV1::Memory(_)) | Self::Memory(_) => "index_coverage_refresh_memory",
      Self::Registry(IndexCoverageRegistryErrorV1::Allocation(_)) | Self::Allocation(_) => "index_coverage_refresh_allocation",
      Self::Registry(IndexCoverageRegistryErrorV1::Poisoned { .. }) | Self::Poisoned(_) => "index_coverage_refresh_poisoned",
      Self::ScopeCache(_) => "index_coverage_scope_cache",
      Self::ArithmeticOverflow => "index_coverage_refresh_accounting",
    }
  }

  pub const fn is_installation_contract_failure(&self) -> bool {
    matches!(
      self,
      Self::Registry(IndexCoverageRegistryErrorV1::Invalid { .. })
        | Self::Registry(IndexCoverageRegistryErrorV1::Poisoned { .. })
        | Self::ScopeCache(IndexScopeOrdinalStoreRegistryErrorV1::Invalid(_))
        | Self::ScopeCache(IndexScopeOrdinalStoreRegistryErrorV1::DescriptorConflict)
        | Self::ScopeCache(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)
        | Self::ScopeCache(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticConversion(_))
        | Self::ScopeCache(IndexScopeOrdinalStoreRegistryErrorV1::Poisoned(_))
        | Self::Allocation(_)
        | Self::ArithmeticOverflow
        | Self::Poisoned(_)
    )
  }
}

fn bounded_context(mut context: String) -> String {
  if context.len() <= MAXIMUM_FAILURE_CONTEXT_BYTES {
    return context;
  }
  let mut boundary = MAXIMUM_FAILURE_CONTEXT_BYTES;
  while !context.is_char_boundary(boundary) {
    boundary -= 1;
  }
  context.truncate(boundary);
  context
}
