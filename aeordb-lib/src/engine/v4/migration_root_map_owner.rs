//! Bounded external staging, publication, and selected reads for legacy roots.
//!
//! The workspace is bulk evidence only. Immutable pages become usable only
//! after one mutable `LegacyRootMapControl` selects their complete chain.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use super::first_authority::{
  FirstAuthorityPublicationErrorV1, ImmutableSystemControlBatchPublicationRequestV1, ImmutableSystemControlPublicationErrorV1,
  ImmutableSystemControlWriteV1, MigrationMapRootAuthorityBatchPublicationRequestV1, MigrationMapRootAuthorityWriteV1,
  LoadedMutableSystemControlV1, MutableSystemControlAuthorityExpectationV1, MutableSystemControlExpectationV1,
  MutableSystemControlPublicationErrorV1, MutableSystemControlPublicationRequestV1, V4FirstAuthorityPublisher,
};
use super::gc_retirement::RetirementJournalOwnerV1;
use super::hash::digest_parts;
use super::header_publication::DatabaseHeaderObservationV4;
use super::migration_base_clone_execution::{MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedResultSinkV1, MigrationBaseCloneSeedV1};
use super::migration_capture_replay::{MigrationCaptureReplayAuthorityTemplateV1, MigrationCaptureReplayRootSinkV1};
use super::migration_final_authority_reconciliation::{
  MigrationFinalAuthoritySeedV1, MigrationFinalPriorRootMappingLookupV1, MigrationFinalRootMappingClosureV1,
  MigrationFinalRootMappingSinkV1, MigrationFinalRootMappingV1,
};
use super::migration_root_map::{
  LegacyRootMapChainVerifierV1, LegacyRootMapControlBodyV1, LegacyRootMapPageBodyV1, LegacyRootMapRowV1, LegacyRootSemanticAvailabilityV1,
  PAGE_BODY_MAX_BYTES, decode_legacy_root_map_control, decode_legacy_root_map_page, decode_row, encode_legacy_root_map_control,
  encode_legacy_root_map_page, encode_row, legacy_root_map_page_identity_hash, row_width, validate_row,
};
use super::namespace::{
  EncodedNamespaceRootV1, NamespaceRootWriteV1, SemanticAvailabilityV1, decode_namespace_root, decode_semantic_object,
  encode_namespace_root,
};
use super::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_directory_synced, ensure_capacity, secure_platform_private_regular_file,
  validate_existing_directory, validate_private_directory, validate_private_directory_readonly, validate_private_regular_file,
  validate_regular_database_path,
};
use super::reader::FormatError;
use super::system_control::SystemControlKindV1;
use crate::engine::emergency_spill::{
  create_new_regular_file_read_write_no_follow, create_new_regular_file_no_follow, open_regular_file_no_follow,
  open_regular_file_read_write_no_follow,
};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{NativeDurabilityError, durable_install_new_native, sync_directory_native, sync_file_all_native};
use crate::engine::HashAlgorithm;

const WORKSPACE_SCHEMA: u16 = 1;
const WORKSPACE_METADATA_BYTES: usize = 116;
const STAGE_FRAME_HEADER_BYTES: usize = 16;
const STAGE_FRAME_CRC_BYTES: usize = 4;
const RUN_HEADER_BYTES: usize = 64;
const RUN_CRC_BYTES: usize = 4;
const SEAL_FIXED_BYTES: usize = 148;
const IO_CHUNK_BYTES: usize = 64 * 1024;
const STAGE_DIGEST_DOMAIN: &[u8] = b"aeordb.migration-root-map.stage.v1\0";
const STATE_MEMORY_BYTES: u64 = 4 * 1024;
const MAXIMUM_WORKSPACE_ENTRIES: u64 = 1_000_000;
const SEALED_WORKSPACE_ENTRY_COUNT: u64 = 5;
const MAXIMUM_MERGE_FAN_IN: usize = 64;
const MAXIMUM_IMMUTABLE_CONTROL_BATCH: usize = 255;
const MAXIMUM_MIGRATION_ROOT_AUTHORITY_BATCH: usize = 51;
const ROW_MEMORY_OVERHEAD: usize = 128;
const MERGE_READER_MEMORY_OVERHEAD: usize = 256;
const SELECTED_READER_RETAINED_MEMORY_BYTES: u64 = 512 * 1024;
// The shared system-file loader briefly retains the encoded chunk, decoded
// body, FileRecord entity, and decode bookkeeping at the same time.
const SELECTED_READER_PAGE_MEMORY_BYTES: u64 = 2 * PAGE_BODY_MAX_BYTES as u64 + 128 * 1024;
const SELECTED_READER_OPEN_MEMORY_BYTES: u64 = SELECTED_READER_RETAINED_MEMORY_BYTES + SELECTED_READER_PAGE_MEMORY_BYTES;
const MAXIMUM_PRIOR_LOOKUP_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const PRIOR_LOOKUP_ROW_MEMORY_OVERHEAD: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
struct StagedLegacyRootMappingV1 {
  row: LegacyRootMapRowV1,
  namespace_root: EncodedNamespaceRootV1,
}

#[derive(Debug)]
pub enum LegacyRootMapOwnerErrorV1 {
  Invalid { code: &'static str, message: String },
  SelectionCommitted { receipt: Box<LegacyRootMapPublicationReceiptV1>, source: Box<LegacyRootMapOwnerErrorV1> },
  Canceled,
  Workspace(String),
  Capacity(String),
  Allocation(String),
  Io { operation: &'static str, source: std::io::Error },
  Durability(Box<NativeDurabilityError>),
  Memory(Box<MemoryCoordinatorError>),
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  ImmutablePublication(ImmutableSystemControlPublicationErrorV1),
  MutablePublication(MutableSystemControlPublicationErrorV1),
}

impl LegacyRootMapOwnerErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::SelectionCommitted { .. } => "migration_root_map_selection_committed",
      Self::Canceled => "migration_root_map_cancelled",
      Self::Workspace(_) => "migration_root_map_workspace",
      Self::Capacity(_) => "migration_root_map_capacity",
      Self::Allocation(_) => "migration_root_map_allocation",
      Self::Io { .. } => "migration_root_map_io",
      Self::Durability(_) => "migration_root_map_durability",
      Self::Memory(_) => "migration_root_map_memory",
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::ImmutablePublication(source) => source.code(),
      Self::MutablePublication(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  pub fn committed_receipt(&self) -> Option<&LegacyRootMapPublicationReceiptV1> {
    match self {
      Self::SelectionCommitted { receipt, .. } => Some(receipt),
      Self::Invalid { .. }
      | Self::Canceled
      | Self::Workspace(_)
      | Self::Capacity(_)
      | Self::Allocation(_)
      | Self::Io { .. }
      | Self::Durability(_)
      | Self::Memory(_)
      | Self::Format(_)
      | Self::Authority(_)
      | Self::ImmutablePublication(_)
      | Self::MutablePublication(_) => None,
    }
  }
}

impl Display for LegacyRootMapOwnerErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::SelectionCommitted { receipt, source } => write!(
        formatter,
        "migration_root_map_selection_committed: root-map control {} committed, but selected readback failed: {source}",
        receipt.control_sequence
      ),
      Self::Canceled => formatter.write_str("migration_root_map_cancelled: root-map work was canceled"),
      Self::Workspace(message) => write!(formatter, "migration_root_map_workspace: {message}"),
      Self::Capacity(message) => write!(formatter, "migration_root_map_capacity: {message}"),
      Self::Allocation(message) => write!(formatter, "migration_root_map_allocation: {message}"),
      Self::Io { operation, source } => write!(formatter, "migration_root_map_io: {operation}: {source}"),
      Self::Durability(source) => write!(formatter, "migration_root_map_durability: {source}"),
      Self::Memory(source) => write!(formatter, "migration_root_map_memory: {source}"),
      Self::Format(source) => Display::fmt(source, formatter),
      Self::Authority(source) => Display::fmt(source, formatter),
      Self::ImmutablePublication(source) => Display::fmt(source, formatter),
      Self::MutablePublication(source) => Display::fmt(source, formatter),
    }
  }
}

impl Error for LegacyRootMapOwnerErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::SelectionCommitted { source, .. } => Some(source.as_ref()),
      Self::Io { source, .. } => Some(source),
      Self::Durability(source) => Some(source),
      Self::Memory(source) => Some(source),
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::ImmutablePublication(source) => Some(source),
      Self::MutablePublication(source) => Some(source),
      Self::Invalid { .. } | Self::Canceled | Self::Workspace(_) | Self::Capacity(_) | Self::Allocation(_) => None,
    }
  }
}

impl From<PrivateWorkspaceErrorV1> for LegacyRootMapOwnerErrorV1 {
  fn from(source: PrivateWorkspaceErrorV1) -> Self {
    match source {
      PrivateWorkspaceErrorV1::Path(message) => Self::Workspace(message),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::State(message) => Self::Workspace(message),
      PrivateWorkspaceErrorV1::Capacity(message) => Self::Capacity(message),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::Allocation(message) => Self::Allocation(message),
      PrivateWorkspaceErrorV1::Io { operation, source } => Self::Io { operation, source },
      PrivateWorkspaceErrorV1::Durability(source) => Self::Durability(source),
    }
  }
}

impl From<FormatError> for LegacyRootMapOwnerErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for LegacyRootMapOwnerErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<ImmutableSystemControlPublicationErrorV1> for LegacyRootMapOwnerErrorV1 {
  fn from(source: ImmutableSystemControlPublicationErrorV1) -> Self {
    Self::ImmutablePublication(source)
  }
}

impl From<MutableSystemControlPublicationErrorV1> for LegacyRootMapOwnerErrorV1 {
  fn from(source: MutableSystemControlPublicationErrorV1) -> Self {
    Self::MutablePublication(source)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyRootMapWorkspaceIdentityV1 {
  database_id: [u8; 16],
  migration_id: [u8; 16],
  logical_database_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  map_generation: u64,
  algorithm: HashAlgorithm,
}

impl LegacyRootMapWorkspaceIdentityV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    database_id: [u8; 16],
    migration_id: [u8; 16],
    logical_database_id: [u8; 16],
    source_physical_instance_id: [u8; 16],
    destination_physical_instance_id: [u8; 16],
    map_generation: u64,
    algorithm: HashAlgorithm,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    if [&database_id, &migration_id, &logical_database_id, &source_physical_instance_id, &destination_physical_instance_id]
      .into_iter()
      .any(|value| all_zero(value))
      || source_physical_instance_id == destination_physical_instance_id
      || map_generation == 0
      || !matches!(algorithm, HashAlgorithm::Blake3_256 | HashAlgorithm::Sha512)
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_identity",
        "root-map IDs, physical separation, generation, or hash profile are invalid",
      ));
    }
    Ok(Self {
      database_id,
      migration_id,
      logical_database_id,
      source_physical_instance_id,
      destination_physical_instance_id,
      map_generation,
      algorithm,
    })
  }

  pub const fn hash_algorithm(self) -> HashAlgorithm {
    self.algorithm
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRootMapWorkspaceOptionsV1 {
  scratch_root: Option<PathBuf>,
  maximum_stored_bytes: u64,
  maximum_staged_rows: u64,
  minimum_free_bytes: u64,
  maximum_sort_memory_bytes: u64,
  maximum_open_runs: usize,
  maximum_page_rows: usize,
  maximum_publication_batch_bytes: usize,
}

impl LegacyRootMapWorkspaceOptionsV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    scratch_root: Option<PathBuf>,
    maximum_stored_bytes: u64,
    maximum_staged_rows: u64,
    minimum_free_bytes: u64,
    maximum_sort_memory_bytes: u64,
    maximum_open_runs: usize,
    maximum_page_rows: usize,
    maximum_publication_batch_bytes: usize,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    validate_options(
      scratch_root.as_deref(),
      maximum_stored_bytes,
      maximum_staged_rows,
      maximum_sort_memory_bytes,
      maximum_open_runs,
      maximum_page_rows,
      maximum_publication_batch_bytes,
    )?;
    Ok(Self {
      scratch_root,
      maximum_stored_bytes,
      maximum_staged_rows,
      minimum_free_bytes,
      maximum_sort_memory_bytes,
      maximum_open_runs,
      maximum_page_rows,
      maximum_publication_batch_bytes,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyRootMapWorkspaceReopenOptionsV1 {
  maximum_stored_bytes: u64,
  maximum_staged_rows: u64,
  minimum_free_bytes: u64,
  maximum_sort_memory_bytes: u64,
  maximum_open_runs: usize,
  maximum_page_rows: usize,
  maximum_publication_batch_bytes: usize,
}

impl LegacyRootMapWorkspaceReopenOptionsV1 {
  pub fn new(
    maximum_stored_bytes: u64,
    maximum_staged_rows: u64,
    minimum_free_bytes: u64,
    maximum_sort_memory_bytes: u64,
    maximum_open_runs: usize,
    maximum_page_rows: usize,
    maximum_publication_batch_bytes: usize,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    validate_options(
      None,
      maximum_stored_bytes,
      maximum_staged_rows,
      maximum_sort_memory_bytes,
      maximum_open_runs,
      maximum_page_rows,
      maximum_publication_batch_bytes,
    )?;
    Ok(Self {
      maximum_stored_bytes,
      maximum_staged_rows,
      minimum_free_bytes,
      maximum_sort_memory_bytes,
      maximum_open_runs,
      maximum_page_rows,
      maximum_publication_batch_bytes,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LegacyRootMapSealV1 {
  mapping_count: u64,
  omitted_mapping_count: u64,
  authority_digest: [u8; 32],
  mapping_digest: [u8; 32],
  staged_row_count: u64,
  staged_byte_count: u64,
  stage_digest: [u8; 32],
  destination_namespace_root: Vec<u8>,
}

pub struct LegacyRootMapStagingWorkspaceV1 {
  identity: LegacyRootMapWorkspaceIdentityV1,
  options: LegacyRootMapWorkspaceOptionsV1,
  publication_timestamp_ms: u64,
  workspace_path: PathBuf,
  runs_path: PathBuf,
  pages_path: PathBuf,
  stage_file: File,
  staged_rows: u64,
  stored_bytes: u64,
  seal: Option<LegacyRootMapSealV1>,
  cancellation: CancellationToken,
  _state_memory: MemoryReservation,
}

struct StagedPriorDestinationV1 {
  legacy_root: Vec<u8>,
  namespace_root: Vec<u8>,
  semantic_availability: LegacyRootSemanticAvailabilityV1,
  destination_tree: Vec<u8>,
}

/// Bounded immutable snapshot of base-clone and replay mappings staged before
/// final authority reconciliation.
///
/// The snapshot validates the complete durable stage prefix once, detects
/// conflicting duplicate roots, and then serves allocation-small binary
/// searches without borrowing the mutable staging workspace needed by the
/// final mapping sink.
pub struct LegacyRootMapStagedPriorLookupV1 {
  rows: Vec<StagedPriorDestinationV1>,
  cancellation: CancellationToken,
  remaining_lookups: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for LegacyRootMapStagedPriorLookupV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("LegacyRootMapStagedPriorLookupV1")
      .field("row_count", &self.rows.len())
      .field("remaining_lookups", &self.remaining_lookups)
      .finish_non_exhaustive()
  }
}

impl LegacyRootMapStagedPriorLookupV1 {
  pub fn snapshot(
    workspace: &mut LegacyRootMapStagingWorkspaceV1,
    memory: &MemoryCoordinator,
    maximum_memory_bytes: u64,
    maximum_lookups: u64,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    check_cancelled(&workspace.cancellation)?;
    if maximum_memory_bytes == 0
      || maximum_memory_bytes > MAXIMUM_PRIOR_LOOKUP_MEMORY_BYTES
      || maximum_lookups == 0
      || maximum_lookups > MAXIMUM_WORKSPACE_ENTRIES
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_prior_lookup_bounds",
        "staged prior-root lookup memory and work bounds are invalid",
      ));
    }
    let required_memory = prior_lookup_memory_charge(workspace.staged_rows, workspace.identity.algorithm)?;
    if required_memory > maximum_memory_bytes {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
        "staged prior-root lookup requires {required_memory} bytes, exceeding its {maximum_memory_bytes}-byte bound"
      )));
    }
    let retained_memory = memory
      .reserve(MemoryOwner::Migration, required_memory, AdmissionClass::Maintenance)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;
    sync_file_all_native(&workspace.stage_file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    let expected_bytes = expected_stage_byte_count(workspace.staged_rows, workspace.identity.algorithm)?;
    validate_stage_snapshot(
      &mut workspace.stage_file,
      workspace.identity.algorithm,
      workspace.staged_rows,
      expected_bytes,
      None,
      false,
      &workspace.cancellation,
    )?;

    let mut stage =
      workspace.stage_file.try_clone().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "prior-root stage clone", source })?;
    stage.seek(SeekFrom::Start(0)).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "prior-root stage rewind", source })?;
    let row_capacity = usize::try_from(workspace.staged_rows)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("prior-root row count exceeds usize: {error}")))?;
    let mut rows = Vec::new();
    rows
      .try_reserve_exact(row_capacity)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("prior-root lookup allocation failed: {error}")))?;
    for _ in 0..workspace.staged_rows {
      check_cancelled(&workspace.cancellation)?;
      let mapping = read_next_stage_mapping(&mut stage, workspace.identity.algorithm)?.ok_or_else(|| {
        LegacyRootMapOwnerErrorV1::invalid("migration_root_map_prior_lookup_stage", "validated root-map stage ended before its row count")
      })?;
      let namespace = decode_namespace_root(&mapping.namespace_root.value, workspace.identity.algorithm)?;
      rows.push(StagedPriorDestinationV1 {
        legacy_root: mapping.row.legacy_root_hash,
        namespace_root: mapping.row.namespace_root_v1_hash,
        semantic_availability: mapping.row.semantic_availability,
        destination_tree: namespace.namespace_tree_root,
      });
    }
    if read_next_stage_mapping(&mut stage, workspace.identity.algorithm)?.is_some() {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_prior_lookup_stage",
        "validated root-map stage contains rows beyond its selected prefix",
      ));
    }
    rows.sort_by(|left, right| left.legacy_root.cmp(&right.legacy_root));
    for pair in rows.windows(2) {
      if pair[0].legacy_root == pair[1].legacy_root
        && (pair[0].namespace_root != pair[1].namespace_root || pair[0].semantic_availability != pair[1].semantic_availability)
      {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_conflicting_mapping",
          "one legacy root maps to different destination or semantic state",
        ));
      }
    }
    Ok(Self { rows, cancellation: workspace.cancellation.clone(), remaining_lookups: maximum_lookups, _memory: retained_memory })
  }
}

impl MigrationFinalPriorRootMappingLookupV1 for LegacyRootMapStagedPriorLookupV1 {
  fn lookup_destination_entity(&mut self, seed: &MigrationFinalAuthoritySeedV1) -> EngineResult<Option<Vec<u8>>> {
    check_cancelled(&self.cancellation).map_err(owner_engine_error)?;
    if self.remaining_lookups == 0 {
      return Err(EngineError::ResourceExhausted("staged prior-root lookup exhausted its configured work bound".to_string()));
    }
    self.remaining_lookups -= 1;
    Ok(
      self
        .rows
        .binary_search_by(|row| row.legacy_root.as_slice().cmp(&seed.seed.hash))
        .ok()
        .map(|index| self.rows[index].destination_tree.clone()),
    )
  }
}

impl fmt::Debug for LegacyRootMapStagingWorkspaceV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("LegacyRootMapStagingWorkspaceV1")
      .field("workspace_path", &self.workspace_path)
      .field("staged_rows", &self.staged_rows)
      .field("stored_bytes", &self.stored_bytes)
      .field("sealed", &self.seal.is_some())
      .finish()
  }
}

impl LegacyRootMapStagingWorkspaceV1 {
  pub fn create(
    database_path: &Path,
    identity: LegacyRootMapWorkspaceIdentityV1,
    publication_timestamp_ms: u64,
    options: LegacyRootMapWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    check_cancelled(&cancellation)?;
    validate_publication_timestamp(publication_timestamp_ms)?;
    validate_sort_memory(&options, identity.algorithm)?;
    validate_regular_database_path(database_path, "legacy root-map source")?;
    let database_parent =
      database_path.parent().ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_path", "database path has no parent"))?;
    let base = match options.scratch_root.as_deref() {
      Some(scratch_root) => scratch_root,
      None => database_parent,
    };
    validate_existing_directory(base, "legacy root-map workspace base")?;
    ensure_capacity(base, 0, options.minimum_free_bytes)?;
    let metadata = encode_workspace_metadata(identity, publication_timestamp_ms);
    if u64::try_from(metadata.len())
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("workspace metadata length exceeds u64: {error}")))?
      > options.maximum_stored_bytes
    {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
        "root-map workspace metadata exceeds the {}-byte workspace cap",
        options.maximum_stored_bytes
      )));
    }
    let workspace_path = create_workspace_path(database_path, base, identity, options.scratch_root.is_some())?;
    let runs_path = workspace_path.join("runs");
    create_private_directory_synced(&runs_path, &workspace_path)?;
    let pages_path = workspace_path.join("pages");
    create_private_directory_synced(&pages_path, &workspace_path)?;
    let metadata_path = workspace_path.join("workspace.armw");
    write_private_immutable(&metadata_path, &metadata, &cancellation)?;
    let stage_path = workspace_path.join("rows.stage");
    let stage_file = create_new_regular_file_read_write_no_follow(&stage_path)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
    secure_platform_private_regular_file(&stage_path)?;
    validate_private_regular_file(&stage_path, &stage_file, "legacy root-map stage")?;
    sync_file_all_native(&stage_file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    sync_directory_native(&workspace_path).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    let state_memory = memory
      .reserve(MemoryOwner::Migration, STATE_MEMORY_BYTES, AdmissionClass::Maintenance)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;
    let stored_bytes = u64::try_from(metadata.len())
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("workspace metadata length exceeds u64: {error}")))?;
    Ok(Self {
      identity,
      options,
      publication_timestamp_ms,
      workspace_path,
      runs_path,
      pages_path,
      stage_file,
      staged_rows: 0,
      stored_bytes,
      seal: None,
      cancellation,
      _state_memory: state_memory,
    })
  }

  pub fn reopen(
    workspace_path: &Path,
    expected_identity: LegacyRootMapWorkspaceIdentityV1,
    options: LegacyRootMapWorkspaceReopenOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    check_cancelled(&cancellation)?;
    validate_private_directory_readonly(workspace_path, "legacy root-map workspace")?;
    let metadata_path = workspace_path.join("workspace.armw");
    let metadata = read_private_file(&metadata_path, WORKSPACE_METADATA_BYTES, &cancellation)?;
    let (identity, publication_timestamp_ms) = decode_workspace_metadata(&metadata)?;
    if identity != expected_identity {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_identity",
        "workspace metadata differs from the expected root-map identity",
      ));
    }
    let options = LegacyRootMapWorkspaceOptionsV1 {
      scratch_root: None,
      maximum_stored_bytes: options.maximum_stored_bytes,
      maximum_staged_rows: options.maximum_staged_rows,
      minimum_free_bytes: options.minimum_free_bytes,
      maximum_sort_memory_bytes: options.maximum_sort_memory_bytes,
      maximum_open_runs: options.maximum_open_runs,
      maximum_page_rows: options.maximum_page_rows,
      maximum_publication_batch_bytes: options.maximum_publication_batch_bytes,
    };
    validate_sort_memory(&options, identity.algorithm)?;
    ensure_capacity(workspace_path, 0, options.minimum_free_bytes)?;
    let runs_path = workspace_path.join("runs");
    let pages_path = workspace_path.join("pages");
    validate_private_directory_readonly(&runs_path, "legacy root-map run directory")?;
    validate_private_directory_readonly(&pages_path, "legacy root-map page directory")?;
    let mut cleanup_entries = 0u64;
    remove_stale_pending_files(workspace_path, "legacy root-map workspace", &cancellation, &mut cleanup_entries)?;
    remove_stale_pending_files(&runs_path, "legacy root-map run directory", &cancellation, &mut cleanup_entries)?;
    remove_stale_pending_files(&pages_path, "legacy root-map page directory", &cancellation, &mut cleanup_entries)?;
    let stage_path = workspace_path.join("rows.stage");
    let mut stage_file =
      open_regular_file_read_write_no_follow(&stage_path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
    validate_private_regular_file(&stage_path, &stage_file, "legacy root-map stage")?;
    let seal_path = workspace_path.join("closure.armc");
    let seal = if seal_path.exists() {
      let maximum = seal_encoded_length(identity.algorithm)?;
      Some(decode_seal(&read_private_file(&seal_path, maximum, &cancellation)?, identity.algorithm)?)
    } else {
      None
    };
    let staged_rows = match seal.as_ref() {
      Some(seal) => validate_sealed_stage(&mut stage_file, identity.algorithm, seal, true, &cancellation)?,
      None => validate_and_repair_stage(&mut stage_file, identity.algorithm, &cancellation)?,
    };
    if staged_rows > options.maximum_staged_rows {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
        "root-map workspace has {staged_rows} staged rows, exceeding cap {}",
        options.maximum_staged_rows
      )));
    }
    let stored_bytes = inventory_workspace(workspace_path, options.maximum_stored_bytes, &cancellation)?;
    let state_memory = memory
      .reserve(MemoryOwner::Migration, STATE_MEMORY_BYTES, AdmissionClass::Maintenance)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;
    Ok(Self {
      identity,
      options,
      publication_timestamp_ms,
      workspace_path: workspace_path.to_path_buf(),
      runs_path,
      pages_path,
      stage_file,
      staged_rows,
      stored_bytes,
      seal,
      cancellation,
      _state_memory: state_memory,
    })
  }

  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  /// Revalidate a previously sealed staging closure against a newly produced
  /// final-authority closure after process restart.
  ///
  /// The live source freeze proof is intentionally process-local.  Resume
  /// must therefore reproduce the final closure, while the already sealed
  /// workspace remains immutable and must match it exactly.
  pub(crate) fn validate_sealed_final_closure(
    &self,
    closure: &MigrationFinalRootMappingClosureV1,
  ) -> Result<(), LegacyRootMapOwnerErrorV1> {
    let seal = self
      .seal
      .as_ref()
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_unsealed", "root-map workspace has no final closure"))?;
    if closure.database_id != self.identity.database_id
      || closure.migration_id != self.identity.migration_id
      || closure.source_physical_instance_id != self.identity.source_physical_instance_id
      || closure.destination_physical_instance_id != self.identity.destination_physical_instance_id
      || seal.mapping_count != closure.mapping_count
      || seal.omitted_mapping_count != closure.omitted_mapping_count
      || seal.authority_digest != closure.authority_digest
      || seal.mapping_digest != closure.mapping_digest
      || seal.destination_namespace_root != closure.destination_namespace_root
      || seal.staged_row_count != self.staged_rows
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_closure_changed",
        "reproduced final-authority closure differs from the sealed root-map workspace",
      ));
    }
    Ok(())
  }

  pub fn stage_mapping(
    &mut self,
    row: &LegacyRootMapRowV1,
    namespace_root: &EncodedNamespaceRootV1,
  ) -> Result<(), LegacyRootMapOwnerErrorV1> {
    check_cancelled(&self.cancellation)?;
    if self.seal.is_some() {
      return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_sealed", "cannot append a row after final mapping closure"));
    }
    if self.staged_rows >= self.options.maximum_staged_rows {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
        "root-map staging exceeds the {}-row work cap",
        self.options.maximum_staged_rows
      )));
    }
    validate_row(row, self.identity.algorithm.hash_length())?;
    validate_staged_namespace_root(row, namespace_root, self.identity.algorithm)?;
    let frame = encode_stage_frame(row, namespace_root, self.identity.algorithm)?;
    self.reserve_stored_bytes(frame.len())?;
    self.stage_file.seek(SeekFrom::End(0)).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage seek", source })?;
    self.stage_file.write_all(&frame).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage append", source })?;
    sync_file_all_native(&self.stage_file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    self.staged_rows = self
      .staged_rows
      .checked_add(1)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("staged root-map row count overflowed".to_string()))?;
    Ok(())
  }

  pub fn seal(
    &mut self,
    mapping_count: u64,
    omitted_mapping_count: u64,
    authority_digest: [u8; 32],
    mapping_digest: [u8; 32],
    destination_namespace_root: &[u8],
  ) -> Result<(), LegacyRootMapOwnerErrorV1> {
    check_cancelled(&self.cancellation)?;
    if mapping_count < omitted_mapping_count
      || all_zero(&authority_digest)
      || all_zero(&mapping_digest)
      || destination_namespace_root.len() != self.identity.algorithm.hash_length()
      || all_zero(destination_namespace_root)
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_closure",
        "root-map closure counts, digests, or selected destination root are invalid",
      ));
    }
    sync_file_all_native(&self.stage_file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    let staged_byte_count = expected_stage_byte_count(self.staged_rows, self.identity.algorithm)?;
    let stage_digest = validate_stage_snapshot(
      &mut self.stage_file,
      self.identity.algorithm,
      self.staged_rows,
      staged_byte_count,
      None,
      false,
      &self.cancellation,
    )?;
    let seal = LegacyRootMapSealV1 {
      mapping_count,
      omitted_mapping_count,
      authority_digest,
      mapping_digest,
      staged_row_count: self.staged_rows,
      staged_byte_count,
      stage_digest,
      destination_namespace_root: destination_namespace_root.to_vec(),
    };
    let encoded = encode_seal(&seal, self.identity.algorithm)?;
    let path = self.workspace_path.join("closure.armc");
    if path.exists() {
      let current = read_private_file(&path, encoded.len(), &self.cancellation)?;
      if current != encoded {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_closure_collision",
          "an existing final root-map closure differs from this closure",
        ));
      }
    } else {
      self.reserve_stored_bytes(encoded.len())?;
      write_private_immutable(&path, &encoded, &self.cancellation)?;
    }
    self.seal = Some(seal);
    Ok(())
  }

  fn reserve_stored_bytes(&mut self, additional: usize) -> Result<(), LegacyRootMapOwnerErrorV1> {
    let additional =
      u64::try_from(additional).map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("workspace addition exceeds u64: {error}")))?;
    let projected = self
      .stored_bytes
      .checked_add(additional)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("workspace byte count overflowed".to_string()))?;
    if projected > self.options.maximum_stored_bytes {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
        "root-map workspace would use {projected} bytes, exceeding {}",
        self.options.maximum_stored_bytes
      )));
    }
    ensure_capacity(&self.workspace_path, additional, self.options.minimum_free_bytes)?;
    self.stored_bytes = projected;
    Ok(())
  }
}

/// One staging adapter shared by every migration root producer.
///
/// The adapter freezes semantic availability once and independently derives
/// every NamespaceRoot from the producer's destination tree. This keeps base
/// clone, replay, and final reconciliation on one mapping path.
#[derive(Debug)]
pub struct LegacyRootMapProducerSinkV1<'a> {
  workspace: &'a mut LegacyRootMapStagingWorkspaceV1,
  required_capabilities: [u8; 32],
  semantic_state_root: Vec<u8>,
  semantic_availability: LegacyRootSemanticAvailabilityV1,
  base_source_write_sequence: u64,
}

impl<'a> LegacyRootMapProducerSinkV1<'a> {
  pub fn new(
    workspace: &'a mut LegacyRootMapStagingWorkspaceV1,
    authority: &MigrationCaptureReplayAuthorityTemplateV1,
    base_source_write_sequence: u64,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    if base_source_write_sequence == 0 {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_source_sequence",
        "base-clone root mappings require a nonzero source write sequence",
      ));
    }
    let semantic = decode_semantic_object(&authority.semantic_state.value, workspace.identity.algorithm)?;
    if semantic.object_id != authority.semantic_state.object_id {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_semantic_identity",
        "frozen semantic-state bytes do not match their declared object identity",
      ));
    }
    let state = semantic.semantic_state.ok_or_else(|| {
      LegacyRootMapOwnerErrorV1::invalid("migration_root_map_semantic_kind", "root-map authority must identify a semantic-state object")
    })?;
    let semantic_availability = match state.availability {
      SemanticAvailabilityV1::Complete { .. } => LegacyRootSemanticAvailabilityV1::Complete,
      SemanticAvailabilityV1::ContentOnly { reason } => LegacyRootSemanticAvailabilityV1::ContentOnly { reason },
    };
    if authority.semantic_state.object_id.len() != workspace.identity.algorithm.hash_length()
      || all_zero(&authority.semantic_state.object_id)
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_semantic_identity",
        "semantic-state root is not one nonzero database-width hash",
      ));
    }
    Ok(Self {
      workspace,
      required_capabilities: authority.required_capabilities,
      semantic_state_root: authority.semantic_state.object_id.clone(),
      semantic_availability,
      base_source_write_sequence,
    })
  }

  fn stage_tree_mapping(
    &mut self,
    source_root: &[u8],
    destination_tree_root: &[u8],
    supplied_namespace_root: Option<&[u8]>,
    source_write_sequence: u64,
  ) -> Result<(), LegacyRootMapOwnerErrorV1> {
    if source_write_sequence == 0 {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_source_sequence",
        "root mappings require a nonzero source write sequence",
      ));
    }
    let namespace = encode_namespace_root(
      &NamespaceRootWriteV1 {
        required_capabilities: self.required_capabilities,
        namespace_tree_root: destination_tree_root.to_vec(),
        semantic_state_root: self.semantic_state_root.clone(),
      },
      self.workspace.identity.algorithm,
    )?;
    if supplied_namespace_root.is_some_and(|supplied| supplied != namespace.root_hash) {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_namespace_mismatch",
        "producer-supplied NamespaceRoot differs from the canonical destination tree and semantic authority",
      ));
    }
    self.workspace.stage_mapping(
      &LegacyRootMapRowV1 {
        legacy_root_hash: source_root.to_vec(),
        namespace_root_v1_hash: namespace.root_hash.clone(),
        semantic_availability: self.semantic_availability,
        captured_source_write_sequence: source_write_sequence,
      },
      &namespace,
    )
  }

  fn as_engine_result(result: Result<(), LegacyRootMapOwnerErrorV1>) -> EngineResult<()> {
    result.map_err(owner_engine_error)
  }
}

impl MigrationBaseCloneSeedResultSinkV1 for LegacyRootMapProducerSinkV1<'_> {
  fn record_seed_result(&mut self, seed: &MigrationBaseCloneSeedV1, destination_hash: Option<&[u8]>) -> EngineResult<()> {
    if seed.kind == MigrationBaseCloneSeedKindV1::DetachedProtectedPath {
      return Ok(());
    }
    let result = destination_hash
      .ok_or_else(|| {
        LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_base_destination",
          "base-clone retained root has no destination tree mapping",
        )
      })
      .and_then(|destination| self.stage_tree_mapping(&seed.hash, destination, None, self.base_source_write_sequence));
    Self::as_engine_result(result)
  }
}

impl MigrationCaptureReplayRootSinkV1 for LegacyRootMapProducerSinkV1<'_> {
  fn record_root_mapping(
    &mut self,
    source_publication_sequence: u64,
    source_root: &[u8],
    destination_namespace_root: &[u8],
    destination_tree_root: &[u8],
  ) -> EngineResult<()> {
    let result = self.stage_tree_mapping(source_root, destination_tree_root, Some(destination_namespace_root), source_publication_sequence);
    Self::as_engine_result(result)
  }
}

impl MigrationFinalRootMappingSinkV1 for LegacyRootMapProducerSinkV1<'_> {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> EngineResult<()> {
    if mapping.kind == MigrationBaseCloneSeedKindV1::DetachedProtectedPath {
      if mapping.destination_namespace_root.is_some() || mapping.destination_tree_root.is_some() {
        return Err(EngineError::InvalidInput("detached protected mappings cannot identify destination namespace roots".to_string()));
      }
      return Ok(());
    }
    let result = mapping
      .destination_tree_root
      .as_deref()
      .zip(mapping.destination_namespace_root.as_deref())
      .ok_or_else(|| {
        LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_final_destination",
          "final retained root has no destination tree and NamespaceRoot mapping",
        )
      })
      .and_then(|(tree, namespace)| self.stage_tree_mapping(&mapping.source_root, tree, Some(namespace), mapping.source_write_sequence));
    Self::as_engine_result(result)
  }

  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> EngineResult<()> {
    let result = (|| {
      if closure.database_id != self.workspace.identity.database_id
        || closure.migration_id != self.workspace.identity.migration_id
        || closure.source_physical_instance_id != self.workspace.identity.source_physical_instance_id
        || closure.destination_physical_instance_id != self.workspace.identity.destination_physical_instance_id
      {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_closure_identity",
          "final mapping closure belongs to another database, migration, or physical incarnation",
        ));
      }
      let namespace = encode_namespace_root(
        &NamespaceRootWriteV1 {
          required_capabilities: self.required_capabilities,
          namespace_tree_root: closure.destination_tree_root.clone(),
          semantic_state_root: self.semantic_state_root.clone(),
        },
        self.workspace.identity.algorithm,
      )?;
      if namespace.root_hash != closure.destination_namespace_root {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_closure_namespace",
          "final mapping closure NamespaceRoot differs from its destination tree and semantic authority",
        ));
      }
      self.workspace.seal(
        closure.mapping_count,
        closure.omitted_mapping_count,
        closure.authority_digest,
        closure.mapping_digest,
        &closure.destination_namespace_root,
      )
    })();
    Self::as_engine_result(result)
  }
}

fn owner_engine_error(error: LegacyRootMapOwnerErrorV1) -> EngineError {
  match error {
    committed @ LegacyRootMapOwnerErrorV1::SelectionCommitted { .. } => EngineError::PostMutationDurabilityFailure(committed.to_string()),
    LegacyRootMapOwnerErrorV1::Canceled => EngineError::Cancelled("legacy root-map staging".to_string()),
    LegacyRootMapOwnerErrorV1::Capacity(message) | LegacyRootMapOwnerErrorV1::Allocation(message) => {
      EngineError::ResourceExhausted(message)
    }
    LegacyRootMapOwnerErrorV1::Memory(source) => EngineError::ResourceExhausted(source.to_string()),
    LegacyRootMapOwnerErrorV1::Io { source, .. } => EngineError::IoError(source),
    LegacyRootMapOwnerErrorV1::Durability(source) => EngineError::PostMutationDurabilityFailure(source.to_string()),
    other => EngineError::InvalidInput(other.to_string()),
  }
}

fn validate_options(
  scratch_root: Option<&Path>,
  maximum_stored_bytes: u64,
  maximum_staged_rows: u64,
  maximum_sort_memory_bytes: u64,
  maximum_open_runs: usize,
  maximum_page_rows: usize,
  maximum_publication_batch_bytes: usize,
) -> Result<(), LegacyRootMapOwnerErrorV1> {
  if scratch_root.is_some_and(|path| !path.is_absolute()) {
    return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_path", "configured root-map scratch root must be absolute"));
  }
  if maximum_stored_bytes == 0
    || maximum_staged_rows == 0
    || maximum_sort_memory_bytes == 0
    || !(2..=MAXIMUM_MERGE_FAN_IN).contains(&maximum_open_runs)
    || maximum_page_rows == 0
    || maximum_publication_batch_bytes == 0
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_limits",
      "root-map disk, row-work, memory, merge fan-in, page, and publication limits must be bounded and nonzero",
    ));
  }
  Ok(())
}

fn validate_sort_memory(options: &LegacyRootMapWorkspaceOptionsV1, algorithm: HashAlgorithm) -> Result<(), LegacyRootMapOwnerErrorV1> {
  let row_charge = row_memory_charge(algorithm)?;
  let per_reader = row_charge
    .checked_add(MERGE_READER_MEMORY_OVERHEAD)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge-reader memory charge overflowed".to_string()))?;
  let required_merge_bytes = per_reader
    .checked_mul(options.maximum_open_runs)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge memory charge overflowed".to_string()))?;
  let configured = usize::try_from(options.maximum_sort_memory_bytes)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("sort memory limit exceeds usize: {error}")))?;
  if configured < row_charge || configured < required_merge_bytes {
    return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
      "root-map sort memory {configured} cannot cover one run row and {required_merge_bytes} bytes for the configured merge fan-in"
    )));
  }
  Ok(())
}

fn row_memory_charge(algorithm: HashAlgorithm) -> Result<usize, LegacyRootMapOwnerErrorV1> {
  row_width(algorithm.hash_length())?
    .checked_add(size_of::<LegacyRootMapRowV1>())
    .and_then(|value| value.checked_add(ROW_MEMORY_OVERHEAD))
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map row memory charge overflowed".to_string()))
}

fn prior_lookup_memory_charge(rows: u64, algorithm: HashAlgorithm) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  let retained_row = size_of::<StagedPriorDestinationV1>()
    .checked_add(
      algorithm
        .hash_length()
        .checked_mul(3)
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("prior-root hash memory charge overflowed".to_string()))?,
    )
    .and_then(|value| value.checked_add(PRIOR_LOOKUP_ROW_MEMORY_OVERHEAD))
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("prior-root row memory charge overflowed".to_string()))?;
  let retained_row = u64::try_from(retained_row)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("prior-root row memory charge exceeds u64: {error}")))?;
  let retained_rows = rows
    .checked_mul(retained_row)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("prior-root retained memory charge overflowed".to_string()))?;
  let frame = u64::try_from(stage_frame_length(algorithm)?)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("prior-root frame memory charge exceeds u64: {error}")))?;
  STATE_MEMORY_BYTES
    .checked_add(retained_rows)
    .and_then(|value| value.checked_add(frame))
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("prior-root total memory charge overflowed".to_string()))
}

fn create_workspace_path(
  database_path: &Path,
  base: &Path,
  identity: LegacyRootMapWorkspaceIdentityV1,
  overridden: bool,
) -> Result<PathBuf, LegacyRootMapOwnerErrorV1> {
  let database_id = hex::encode(identity.database_id);
  let migration_id = hex::encode(identity.migration_id);
  let generation = format!("{:016x}", identity.map_generation);
  if overridden {
    let database_directory = base.join(database_id);
    if database_directory.exists() {
      validate_private_directory(&database_directory, "legacy root-map database workspace")?;
    } else {
      create_private_directory_synced(&database_directory, base)?;
    }
    let migration_directory = database_directory.join(migration_id);
    if migration_directory.exists() {
      validate_private_directory(&migration_directory, "legacy root-map migration workspace")?;
    } else {
      create_private_directory_synced(&migration_directory, &database_directory)?;
    }
    let workspace = migration_directory.join(format!("root-map-{generation}"));
    create_private_directory_synced(&workspace, &migration_directory)?;
    return Ok(workspace);
  }
  let filename = database_path
    .file_name()
    .and_then(|value| value.to_str())
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_path", "database filename is not canonical UTF-8"))?;
  let workspace = base.join(format!(".{filename}-root-map-{database_id}-{migration_id}-{generation}"));
  create_private_directory_synced(&workspace, base)?;
  Ok(workspace)
}

fn encode_workspace_metadata(identity: LegacyRootMapWorkspaceIdentityV1, publication_timestamp_ms: u64) -> [u8; WORKSPACE_METADATA_BYTES] {
  let mut encoded = [0u8; WORKSPACE_METADATA_BYTES];
  encoded[..4].copy_from_slice(b"ARMW");
  put_u16(&mut encoded, 4, WORKSPACE_SCHEMA);
  put_u16(&mut encoded, 6, identity.algorithm.to_u16());
  put_u32(&mut encoded, 8, WORKSPACE_METADATA_BYTES as u32);
  encoded[16..32].copy_from_slice(&identity.database_id);
  encoded[32..48].copy_from_slice(&identity.migration_id);
  encoded[48..64].copy_from_slice(&identity.logical_database_id);
  encoded[64..80].copy_from_slice(&identity.source_physical_instance_id);
  encoded[80..96].copy_from_slice(&identity.destination_physical_instance_id);
  put_u64(&mut encoded, 96, identity.map_generation);
  put_u64(&mut encoded, 104, publication_timestamp_ms);
  let crc = crc32fast::hash(&encoded[..112]);
  put_u32(&mut encoded, 112, crc);
  encoded
}

fn decode_workspace_metadata(bytes: &[u8]) -> Result<(LegacyRootMapWorkspaceIdentityV1, u64), LegacyRootMapOwnerErrorV1> {
  if bytes.len() != WORKSPACE_METADATA_BYTES
    || &bytes[..4] != b"ARMW"
    || u16_at(bytes, 4)? != WORKSPACE_SCHEMA
    || u32_at(bytes, 8)? as usize != WORKSPACE_METADATA_BYTES
    || bytes[12..16].iter().any(|byte| *byte != 0)
    || crc32fast::hash(&bytes[..112]) != u32_at(bytes, 112)?
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_workspace_metadata",
      "root-map workspace metadata magic, version, reserve, length, or checksum is invalid",
    ));
  }
  let algorithm = HashAlgorithm::from_u16(u16_at(bytes, 6)?)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_workspace_metadata", "unknown workspace hash algorithm"))?;
  let identity = LegacyRootMapWorkspaceIdentityV1::new(
    array_16(bytes, 16)?,
    array_16(bytes, 32)?,
    array_16(bytes, 48)?,
    array_16(bytes, 64)?,
    array_16(bytes, 80)?,
    u64_at(bytes, 96)?,
    algorithm,
  )?;
  let timestamp = u64_at(bytes, 104)?;
  validate_publication_timestamp(timestamp)?;
  Ok((identity, timestamp))
}

fn namespace_root_value_width(algorithm: HashAlgorithm) -> Result<usize, LegacyRootMapOwnerErrorV1> {
  108usize
    .checked_add(
      algorithm
        .hash_length()
        .checked_mul(2)
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("NamespaceRoot hash width overflowed".to_string()))?,
    )
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("NamespaceRoot value width overflowed".to_string()))
}

fn stage_frame_length(algorithm: HashAlgorithm) -> Result<usize, LegacyRootMapOwnerErrorV1> {
  let row_bytes = row_width(algorithm.hash_length())?;
  let namespace_root_bytes = namespace_root_value_width(algorithm)?;
  STAGE_FRAME_HEADER_BYTES
    .checked_add(row_bytes)
    .and_then(|value| value.checked_add(namespace_root_bytes))
    .and_then(|value| value.checked_add(STAGE_FRAME_CRC_BYTES))
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map stage frame length overflowed".to_string()))
}

fn validate_staged_namespace_root(
  row: &LegacyRootMapRowV1,
  namespace_root: &EncodedNamespaceRootV1,
  algorithm: HashAlgorithm,
) -> Result<(), LegacyRootMapOwnerErrorV1> {
  if namespace_root.value.len() != namespace_root_value_width(algorithm)? {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_namespace_length",
      "staged NamespaceRoot does not have the canonical fixed v1 width",
    ));
  }
  let decoded = decode_namespace_root(&namespace_root.value, algorithm)?;
  if decoded.root_hash != namespace_root.root_hash || row.namespace_root_v1_hash != namespace_root.root_hash {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_namespace_identity",
      "staged NamespaceRoot bytes, declared identity, and mapping target differ",
    ));
  }
  Ok(())
}

fn encode_stage_frame(
  row: &LegacyRootMapRowV1,
  namespace_root: &EncodedNamespaceRootV1,
  algorithm: HashAlgorithm,
) -> Result<Vec<u8>, LegacyRootMapOwnerErrorV1> {
  validate_staged_namespace_root(row, namespace_root, algorithm)?;
  let total = stage_frame_length(algorithm)?;
  let mut encoded = Vec::new();
  encoded
    .try_reserve_exact(total)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("stage frame allocation failed: {error}")))?;
  encoded.extend_from_slice(b"AROW");
  encoded.extend_from_slice(&WORKSPACE_SCHEMA.to_le_bytes());
  encoded.extend_from_slice(&(STAGE_FRAME_HEADER_BYTES as u16).to_le_bytes());
  encoded.extend_from_slice(
    &u32::try_from(total)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("stage frame length exceeds u32: {error}")))?
      .to_le_bytes(),
  );
  encoded.extend_from_slice(&0u32.to_le_bytes());
  encode_row(&mut encoded, row);
  encoded.extend_from_slice(&namespace_root.value);
  let crc = crc32fast::hash(&encoded);
  encoded.extend_from_slice(&crc.to_le_bytes());
  Ok(encoded)
}

fn decode_stage_frame(bytes: &[u8], algorithm: HashAlgorithm) -> Result<StagedLegacyRootMappingV1, LegacyRootMapOwnerErrorV1> {
  let width = row_width(algorithm.hash_length())?;
  let expected = stage_frame_length(algorithm)?;
  if bytes.len() != expected
    || &bytes[..4] != b"AROW"
    || u16_at(bytes, 4)? != WORKSPACE_SCHEMA
    || u16_at(bytes, 6)? as usize != STAGE_FRAME_HEADER_BYTES
    || u32_at(bytes, 8)? as usize != expected
    || bytes[12..16].iter().any(|byte| *byte != 0)
    || crc32fast::hash(&bytes[..expected - 4]) != u32_at(bytes, expected - 4)?
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_stage_frame",
      "complete root-map stage frame has invalid framing or checksum",
    ));
  }
  let row_end = STAGE_FRAME_HEADER_BYTES + width;
  let row = decode_row(&bytes[STAGE_FRAME_HEADER_BYTES..row_end], algorithm.hash_length())?;
  let value = bytes[row_end..expected - STAGE_FRAME_CRC_BYTES].to_vec();
  let decoded = decode_namespace_root(&value, algorithm)?;
  let namespace_root = EncodedNamespaceRootV1 { root_hash: decoded.root_hash, value };
  validate_staged_namespace_root(&row, &namespace_root, algorithm)?;
  Ok(StagedLegacyRootMappingV1 { row, namespace_root })
}

fn validate_and_repair_stage(
  file: &mut File,
  algorithm: HashAlgorithm,
  cancellation: &CancellationToken,
) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  let length = file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage metadata", source })?.len();
  let frame_length = u64::try_from(stage_frame_length(algorithm)?)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("stage frame length exceeds u64: {error}")))?;
  let complete_length = length - (length % frame_length);
  let mut offset = 0u64;
  let mut rows = 0u64;
  let frame_length_usize = usize::try_from(frame_length)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("stage frame length exceeds usize: {error}")))?;
  let mut frame = vec![0; frame_length_usize];
  file.seek(SeekFrom::Start(0)).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage rewind", source })?;
  while offset < complete_length {
    check_cancelled(cancellation)?;
    file.read_exact(&mut frame).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage validation", source })?;
    decode_stage_frame(&frame, algorithm)?;
    offset = offset
      .checked_add(frame_length)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("stage validation offset overflowed".to_string()))?;
    rows = rows.checked_add(1).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("stage validation row count overflowed".to_string()))?;
  }
  if complete_length != length {
    file
      .set_len(complete_length)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map torn suffix truncation", source })?;
    sync_file_all_native(file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
  }
  Ok(rows)
}

fn expected_stage_byte_count(rows: u64, algorithm: HashAlgorithm) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  let frame_length = u64::try_from(stage_frame_length(algorithm)?)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("stage frame length exceeds u64: {error}")))?;
  rows.checked_mul(frame_length).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("sealed stage byte count overflowed".to_string()))
}

fn validate_sealed_stage(
  file: &mut File,
  algorithm: HashAlgorithm,
  seal: &LegacyRootMapSealV1,
  repair_torn_suffix: bool,
  cancellation: &CancellationToken,
) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  validate_stage_snapshot(
    file,
    algorithm,
    seal.staged_row_count,
    seal.staged_byte_count,
    Some(seal.stage_digest),
    repair_torn_suffix,
    cancellation,
  )?;
  Ok(seal.staged_row_count)
}

fn validate_stage_snapshot(
  file: &mut File,
  algorithm: HashAlgorithm,
  expected_rows: u64,
  expected_bytes: u64,
  expected_digest: Option<[u8; 32]>,
  repair_torn_suffix: bool,
  cancellation: &CancellationToken,
) -> Result<[u8; 32], LegacyRootMapOwnerErrorV1> {
  let frame_length = u64::try_from(stage_frame_length(algorithm)?)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("stage frame length exceeds u64: {error}")))?;
  if expected_stage_byte_count(expected_rows, algorithm)? != expected_bytes {
    return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_stage_seal", "sealed root-map stage row and byte counts disagree"));
  }
  let initial_length =
    file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "sealed root-map stage metadata", source })?.len();
  if initial_length < expected_bytes {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_stage_seal",
      "sealed root-map stage is shorter than its committed byte prefix",
    ));
  }
  let suffix_length = initial_length - expected_bytes;
  if suffix_length != 0 && (!repair_torn_suffix || suffix_length >= frame_length) {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_stage_seal",
      "sealed root-map stage contains bytes beyond the only repairable torn suffix",
    ));
  }

  let frame_length_usize = usize::try_from(frame_length)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("stage frame length exceeds usize: {error}")))?;
  let mut frame = vec![0; frame_length_usize];
  let mut hasher = blake3::Hasher::new();
  hasher.update(STAGE_DIGEST_DOMAIN);
  file.seek(SeekFrom::Start(0)).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "sealed root-map stage rewind", source })?;
  for _ in 0..expected_rows {
    check_cancelled(cancellation)?;
    file
      .read_exact(&mut frame)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "sealed root-map stage validation", source })?;
    decode_stage_frame(&frame, algorithm)?;
    hasher.update(&frame);
  }
  let digest = *hasher.finalize().as_bytes();
  if expected_digest.is_some_and(|expected| expected != digest) {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_stage_seal",
      "sealed root-map stage digest differs from its committed byte prefix",
    ));
  }
  let validated_length = file
    .metadata()
    .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "sealed root-map stage post-validation metadata", source })?
    .len();
  if validated_length != initial_length {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_stage_seal",
      "sealed root-map stage changed while it was being validated",
    ));
  }
  if suffix_length != 0 {
    file
      .set_len(expected_bytes)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "sealed root-map torn suffix truncation", source })?;
    sync_file_all_native(file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
  }
  Ok(digest)
}

fn seal_encoded_length(algorithm: HashAlgorithm) -> Result<usize, LegacyRootMapOwnerErrorV1> {
  SEAL_FIXED_BYTES
    .checked_add(algorithm.hash_length())
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map closure length overflowed".to_string()))
}

fn encode_seal(seal: &LegacyRootMapSealV1, algorithm: HashAlgorithm) -> Result<Vec<u8>, LegacyRootMapOwnerErrorV1> {
  let length = seal_encoded_length(algorithm)?;
  let mut encoded = vec![0; length];
  encoded[..4].copy_from_slice(b"ARMC");
  put_u16(&mut encoded, 4, WORKSPACE_SCHEMA);
  put_u16(&mut encoded, 6, algorithm.to_u16());
  put_u32(
    &mut encoded,
    8,
    u32::try_from(length).map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("closure length exceeds u32: {error}")))?,
  );
  put_u64(&mut encoded, 16, seal.mapping_count);
  put_u64(&mut encoded, 24, seal.omitted_mapping_count);
  encoded[32..64].copy_from_slice(&seal.authority_digest);
  encoded[64..96].copy_from_slice(&seal.mapping_digest);
  put_u64(&mut encoded, 96, seal.staged_row_count);
  put_u64(&mut encoded, 104, seal.staged_byte_count);
  encoded[112..144].copy_from_slice(&seal.stage_digest);
  encoded[144..144 + algorithm.hash_length()].copy_from_slice(&seal.destination_namespace_root);
  let crc_offset = length - 4;
  let crc = crc32fast::hash(&encoded[..crc_offset]);
  put_u32(&mut encoded, crc_offset, crc);
  Ok(encoded)
}

fn decode_seal(bytes: &[u8], algorithm: HashAlgorithm) -> Result<LegacyRootMapSealV1, LegacyRootMapOwnerErrorV1> {
  let expected = seal_encoded_length(algorithm)?;
  let crc_offset = expected - 4;
  if bytes.len() != expected
    || &bytes[..4] != b"ARMC"
    || u16_at(bytes, 4)? != WORKSPACE_SCHEMA
    || u16_at(bytes, 6)? != algorithm.to_u16()
    || u32_at(bytes, 8)? as usize != expected
    || bytes[12..16].iter().any(|byte| *byte != 0)
    || crc32fast::hash(&bytes[..crc_offset]) != u32_at(bytes, crc_offset)?
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_closure",
      "root-map closure framing, identity, reserve, length, or checksum is invalid",
    ));
  }
  let seal = LegacyRootMapSealV1 {
    mapping_count: u64_at(bytes, 16)?,
    omitted_mapping_count: u64_at(bytes, 24)?,
    authority_digest: array_32(bytes, 32)?,
    mapping_digest: array_32(bytes, 64)?,
    staged_row_count: u64_at(bytes, 96)?,
    staged_byte_count: u64_at(bytes, 104)?,
    stage_digest: array_32(bytes, 112)?,
    destination_namespace_root: bytes[144..crc_offset].to_vec(),
  };
  if seal.mapping_count < seal.omitted_mapping_count
    || all_zero(&seal.authority_digest)
    || all_zero(&seal.mapping_digest)
    || all_zero(&seal.stage_digest)
    || seal.staged_byte_count != expected_stage_byte_count(seal.staged_row_count, algorithm)?
    || seal.destination_namespace_root.len() != algorithm.hash_length()
    || all_zero(&seal.destination_namespace_root)
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_closure",
      "root-map closure counts, digests, or selected destination root are invalid",
    ));
  }
  Ok(seal)
}

#[derive(Clone, Debug)]
struct SortedRootMapV1 {
  path: PathBuf,
  row_count: u64,
  merge_passes: u32,
  maximum_run_rows: u64,
  maximum_open_runs: usize,
}

impl LegacyRootMapStagingWorkspaceV1 {
  fn prepare_sorted(&mut self, memory: &MemoryCoordinator) -> Result<SortedRootMapV1, LegacyRootMapOwnerErrorV1> {
    check_cancelled(&self.cancellation)?;
    let seal = self.seal.clone().ok_or_else(|| {
      LegacyRootMapOwnerErrorV1::invalid("migration_root_map_unsealed", "root-map publication requires the final mapping closure")
    })?;
    sync_file_all_native(&self.stage_file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    self.staged_rows = validate_sealed_stage(&mut self.stage_file, self.identity.algorithm, &seal, true, &self.cancellation)?;
    self.stored_bytes = inventory_workspace(&self.workspace_path, self.options.maximum_stored_bytes, &self.cancellation)?;
    clear_derived_directory(&self.runs_path, "root-map run", &self.cancellation)?;
    clear_derived_directory(&self.pages_path, "root-map page", &self.cancellation)?;
    self.stored_bytes = inventory_workspace(&self.workspace_path, self.options.maximum_stored_bytes, &self.cancellation)?;

    let row_charge = row_memory_charge(self.identity.algorithm)?;
    let sort_bytes = usize::try_from(self.options.maximum_sort_memory_bytes)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("sort memory limit exceeds usize: {error}")))?;
    let maximum_rows = sort_bytes / row_charge;
    if maximum_rows == 0 {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
        "sort memory limit {} cannot hold one charged {row_charge}-byte root-map row",
        self.options.maximum_sort_memory_bytes
      )));
    }
    let maximum_rows_u64 = u64::try_from(maximum_rows)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("root-map run row limit exceeds u64: {error}")))?;
    let peak_derived_entries = initial_run_peak_derived_entries(self.staged_rows, maximum_rows_u64)?;
    ensure_workspace_entry_capacity(peak_derived_entries)?;
    let _sort_memory = memory
      .reserve(MemoryOwner::Migration, self.options.maximum_sort_memory_bytes, AdmissionClass::Maintenance)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;

    let mut stage =
      self.stage_file.try_clone().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage clone", source })?;
    stage.seek(SeekFrom::Start(0)).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage sort rewind", source })?;
    let mut initial_run_count = 0u64;
    let mut maximum_run_rows = 0u64;
    loop {
      check_cancelled(&self.cancellation)?;
      let mut rows = Vec::new();
      rows
        .try_reserve_exact(maximum_rows)
        .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("root-map sort run allocation failed: {error}")))?;
      while rows.len() < maximum_rows {
        let Some(row) = read_next_stage_mapping(&mut stage, self.identity.algorithm)?.map(|staged| staged.row) else {
          break;
        };
        rows.push(row);
      }
      if rows.is_empty() {
        if initial_run_count == 0 {
          let path = run_path(&self.runs_path, 0, 0);
          let outcome = self.write_rows_run(&path, &mut rows)?;
          maximum_run_rows = maximum_run_rows.max(outcome.row_count);
          initial_run_count = 1;
        }
        break;
      }
      canonicalize_rows(&mut rows)?;
      let path = run_path(&self.runs_path, 0, initial_run_count);
      let outcome = self.write_rows_run(&path, &mut rows)?;
      maximum_run_rows = maximum_run_rows.max(outcome.row_count);
      initial_run_count = initial_run_count
        .checked_add(1)
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map initial run count overflowed".to_string()))?;
    }

    let mut pass = 0u32;
    let mut run_count = initial_run_count;
    let mut maximum_open_runs = 1usize;
    while run_count > 1 {
      let next_pass =
        pass.checked_add(1).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge pass count overflowed".to_string()))?;
      let fan_in = u64::try_from(self.options.maximum_open_runs)
        .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("merge fan-in exceeds u64: {error}")))?;
      let next_count = run_count
        .checked_add(fan_in - 1)
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge group count overflowed".to_string()))?
        / fan_in;
      for output_ordinal in 0..next_count {
        check_cancelled(&self.cancellation)?;
        let first = output_ordinal
          .checked_mul(fan_in)
          .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge input ordinal overflowed".to_string()))?;
        let end = first
          .checked_add(fan_in)
          .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge input range overflowed".to_string()))?
          .min(run_count);
        let group_count = usize::try_from(end - first)
          .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("merge group count exceeds usize: {error}")))?;
        maximum_open_runs = maximum_open_runs.max(group_count);
        let output = run_path(&self.runs_path, next_pass, output_ordinal);
        self.merge_run_group(pass, first, end, &output)?;
      }
      pass = next_pass;
      run_count = next_count;
    }
    let path = run_path(&self.runs_path, pass, 0);
    let row_count = verify_run(&path, self.identity, &self.cancellation)?;
    Ok(SortedRootMapV1 { path, row_count, merge_passes: pass, maximum_run_rows, maximum_open_runs })
  }

  fn write_rows_run(&mut self, path: &Path, rows: &mut [LegacyRootMapRowV1]) -> Result<RunWriteOutcomeV1, LegacyRootMapOwnerErrorV1> {
    let row_count = u64::try_from(rows.len())
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("root-map run row count exceeds u64: {error}")))?;
    let upper = run_encoded_length(self.identity.algorithm, row_count)?;
    self.reserve_stored_bytes(
      usize::try_from(upper).map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("root-map run length exceeds usize: {error}")))?,
    )?;
    let mut writer = RunWriterV1::create(path, self.identity, &self.cancellation)?;
    for row in rows.iter() {
      writer.push(row)?;
    }
    let outcome = writer.finish()?;
    self.release_overreservation(upper, outcome.stored_bytes)?;
    Ok(outcome)
  }

  fn merge_run_group(&mut self, pass: u32, first: u64, end: u64, output: &Path) -> Result<(), LegacyRootMapOwnerErrorV1> {
    let mut readers = Vec::new();
    let count = usize::try_from(end - first)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("merge reader count exceeds usize: {error}")))?;
    readers
      .try_reserve_exact(count)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("merge reader allocation failed: {error}")))?;
    let mut input_bytes = 0u64;
    for ordinal in first..end {
      let path = run_path(&self.runs_path, pass, ordinal);
      let reader = RunReaderV1::open(path, self.identity, &self.cancellation)?;
      input_bytes = input_bytes
        .checked_add(reader.stored_bytes)
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("merge input bytes overflowed".to_string()))?;
      readers.push(reader);
    }
    let output_upper = input_bytes;
    self.reserve_stored_bytes(
      usize::try_from(output_upper)
        .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("merge output bound exceeds usize: {error}")))?,
    )?;
    let mut writer = RunWriterV1::create(output, self.identity, &self.cancellation)?;
    let mut heap = BinaryHeap::new();
    for (reader_index, reader) in readers.iter_mut().enumerate() {
      if let Some(row) = reader.next_row()? {
        heap.push(MergeHeapRowV1 { row, reader_index });
      }
    }
    let mut pending: Option<LegacyRootMapRowV1> = None;
    while let Some(item) = heap.pop() {
      check_cancelled(&self.cancellation)?;
      if let Some(next) = readers[item.reader_index].next_row()? {
        heap.push(MergeHeapRowV1 { row: next, reader_index: item.reader_index });
      }
      match pending.as_mut() {
        Some(current) if current.legacy_root_hash == item.row.legacy_root_hash => merge_duplicate(current, &item.row)?,
        Some(_) => {
          let current = pending
            .replace(item.row)
            .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_merge_state", "pending merge row disappeared"))?;
          writer.push(&current)?;
        }
        None => pending = Some(item.row),
      }
    }
    if let Some(row) = pending {
      writer.push(&row)?;
    }
    let outcome = writer.finish()?;
    self.release_overreservation(output_upper, outcome.stored_bytes)?;
    drop(readers);
    for ordinal in first..end {
      let path = run_path(&self.runs_path, pass, ordinal);
      let length =
        fs::metadata(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map consumed run metadata", source })?.len();
      fs::remove_file(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map consumed run removal", source })?;
      self.stored_bytes = self
        .stored_bytes
        .checked_sub(length)
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map stored-byte accounting underflowed".to_string()))?;
    }
    sync_directory_native(&self.runs_path).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    Ok(())
  }

  fn release_overreservation(&mut self, reserved: u64, actual: u64) -> Result<(), LegacyRootMapOwnerErrorV1> {
    if actual > reserved {
      return Err(LegacyRootMapOwnerErrorV1::Capacity(format!("root-map writer stored {actual} bytes beyond its {reserved}-byte bound")));
    }
    self.stored_bytes = self
      .stored_bytes
      .checked_sub(reserved - actual)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map overreservation accounting underflowed".to_string()))?;
    Ok(())
  }
}

#[derive(Clone, Copy, Debug)]
struct RunWriteOutcomeV1 {
  row_count: u64,
  stored_bytes: u64,
}

struct RunWriterV1<'a> {
  target: PathBuf,
  pending: PathBuf,
  file: File,
  crc: crc32fast::Hasher,
  row_count: u64,
  identity: LegacyRootMapWorkspaceIdentityV1,
  cancellation: &'a CancellationToken,
}

impl<'a> RunWriterV1<'a> {
  fn create(
    target: &Path,
    identity: LegacyRootMapWorkspaceIdentityV1,
    cancellation: &'a CancellationToken,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    check_cancelled(cancellation)?;
    let parent = target.parent().ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_path", "run path has no parent"))?;
    validate_private_directory(parent, "legacy root-map run parent")?;
    let pending = parent.join(format!(".root-map-{}.pending", uuid::Uuid::new_v4().simple()));
    let mut file =
      create_new_regular_file_no_follow(&pending).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
    secure_platform_private_regular_file(&pending)?;
    validate_private_regular_file(&pending, &file, "legacy root-map pending run")?;
    let header = encode_run_header(identity);
    let mut crc = crc32fast::Hasher::new();
    crc.update(&header);
    file.write_all(&header).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run header", source })?;
    Ok(Self { target: target.to_path_buf(), pending, file, crc, row_count: 0, identity, cancellation })
  }

  fn push(&mut self, row: &LegacyRootMapRowV1) -> Result<(), LegacyRootMapOwnerErrorV1> {
    check_cancelled(self.cancellation)?;
    validate_row(row, self.identity.algorithm.hash_length())?;
    let mut encoded = Vec::new();
    encoded
      .try_reserve_exact(row_width(self.identity.algorithm.hash_length())?)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("run row allocation failed: {error}")))?;
    encode_row(&mut encoded, row);
    self.file.write_all(&encoded).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run row", source })?;
    self.crc.update(&encoded);
    self.row_count =
      self.row_count.checked_add(1).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map run row count overflowed".to_string()))?;
    Ok(())
  }

  fn finish(mut self) -> Result<RunWriteOutcomeV1, LegacyRootMapOwnerErrorV1> {
    check_cancelled(self.cancellation)?;
    let checksum = std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize();
    self
      .file
      .write_all(&checksum.to_le_bytes())
      .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run checksum", source })?;
    sync_file_all_native(&self.file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    let stored_bytes =
      self.file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run metadata", source })?.len();
    drop(self.file);
    durable_install_new_native(&self.pending, &self.target).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
    let observed = verify_run(&self.target, self.identity, self.cancellation)?;
    if observed != self.row_count {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_run_count",
        "root-map run readback count differs from the writer count",
      ));
    }
    Ok(RunWriteOutcomeV1 { row_count: self.row_count, stored_bytes })
  }
}

struct RunReaderV1 {
  file: File,
  remaining: u64,
  row_bytes: usize,
  algorithm: HashAlgorithm,
  stored_bytes: u64,
}

impl RunReaderV1 {
  fn open(
    path: PathBuf,
    identity: LegacyRootMapWorkspaceIdentityV1,
    cancellation: &CancellationToken,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    let row_count = verify_run(&path, identity, cancellation)?;
    let mut file = open_regular_file_no_follow(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
    validate_private_regular_file(&path, &file, "legacy root-map run")?;
    let stored_bytes =
      file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run metadata", source })?.len();
    file
      .seek(SeekFrom::Start(RUN_HEADER_BYTES as u64))
      .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run data seek", source })?;
    Ok(Self {
      file,
      remaining: row_count,
      row_bytes: row_width(identity.algorithm.hash_length())?,
      algorithm: identity.algorithm,
      stored_bytes,
    })
  }

  fn next_row(&mut self) -> Result<Option<LegacyRootMapRowV1>, LegacyRootMapOwnerErrorV1> {
    if self.remaining == 0 {
      return Ok(None);
    }
    let mut encoded = vec![0; self.row_bytes];
    self.file.read_exact(&mut encoded).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run row read", source })?;
    self.remaining -= 1;
    Ok(Some(decode_row(&encoded, self.algorithm.hash_length())?))
  }
}

#[derive(Debug, Eq, PartialEq)]
struct MergeHeapRowV1 {
  row: LegacyRootMapRowV1,
  reader_index: usize,
}

impl Ord for MergeHeapRowV1 {
  fn cmp(&self, other: &Self) -> Ordering {
    other.row.legacy_root_hash.cmp(&self.row.legacy_root_hash).then_with(|| other.reader_index.cmp(&self.reader_index))
  }
}

impl PartialOrd for MergeHeapRowV1 {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

fn canonicalize_rows(rows: &mut Vec<LegacyRootMapRowV1>) -> Result<(), LegacyRootMapOwnerErrorV1> {
  rows.sort_unstable_by(|left, right| left.legacy_root_hash.cmp(&right.legacy_root_hash));
  for adjacent in rows.windows(2) {
    if adjacent[0].legacy_root_hash == adjacent[1].legacy_root_hash {
      validate_duplicate(&adjacent[0], &adjacent[1])?;
    }
  }
  rows.dedup_by(|current, previous| {
    if current.legacy_root_hash == previous.legacy_root_hash {
      previous.captured_source_write_sequence = previous.captured_source_write_sequence.max(current.captured_source_write_sequence);
      true
    } else {
      false
    }
  });
  Ok(())
}

fn merge_duplicate(current: &mut LegacyRootMapRowV1, incoming: &LegacyRootMapRowV1) -> Result<(), LegacyRootMapOwnerErrorV1> {
  validate_duplicate(current, incoming)?;
  current.captured_source_write_sequence = current.captured_source_write_sequence.max(incoming.captured_source_write_sequence);
  Ok(())
}

fn validate_duplicate(left: &LegacyRootMapRowV1, right: &LegacyRootMapRowV1) -> Result<(), LegacyRootMapOwnerErrorV1> {
  if left.namespace_root_v1_hash != right.namespace_root_v1_hash || left.semantic_availability != right.semantic_availability {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_conflicting_mapping",
      "one legacy root maps to different destination or semantic state",
    ));
  }
  Ok(())
}

fn read_next_stage_mapping(
  file: &mut File,
  algorithm: HashAlgorithm,
) -> Result<Option<StagedLegacyRootMappingV1>, LegacyRootMapOwnerErrorV1> {
  let length = stage_frame_length(algorithm)?;
  let mut frame = vec![0; length];
  match file.read_exact(&mut frame) {
    Ok(()) => decode_stage_frame(&frame, algorithm).map(Some),
    Err(source) if source.kind() == ErrorKind::UnexpectedEof => {
      let current =
        file.stream_position().map_err(|error| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage position", source: error })?;
      let file_length =
        file.metadata().map_err(|error| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage metadata", source: error })?.len();
      if current == file_length {
        Ok(None)
      } else {
        Err(LegacyRootMapOwnerErrorV1::Io { operation: "root-map complete stage frame", source })
      }
    }
    Err(source) => Err(LegacyRootMapOwnerErrorV1::Io { operation: "root-map stage row", source }),
  }
}

fn encode_run_header(identity: LegacyRootMapWorkspaceIdentityV1) -> [u8; RUN_HEADER_BYTES] {
  let mut header = [0u8; RUN_HEADER_BYTES];
  header[..4].copy_from_slice(b"ARUN");
  put_u16(&mut header, 4, WORKSPACE_SCHEMA);
  put_u16(&mut header, 6, identity.algorithm.to_u16());
  put_u16(&mut header, 8, RUN_HEADER_BYTES as u16);
  header[16..32].copy_from_slice(&identity.database_id);
  header[32..48].copy_from_slice(&identity.migration_id);
  put_u64(&mut header, 48, identity.map_generation);
  header
}

fn verify_run(
  path: &Path,
  identity: LegacyRootMapWorkspaceIdentityV1,
  cancellation: &CancellationToken,
) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  check_cancelled(cancellation)?;
  let mut file = open_regular_file_no_follow(path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
  validate_private_regular_file(path, &file, "legacy root-map run")?;
  let length = file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run metadata", source })?.len();
  let minimum = u64::try_from(RUN_HEADER_BYTES + RUN_CRC_BYTES)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("run minimum length exceeds u64: {error}")))?;
  if length < minimum {
    return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_run", "root-map run is truncated"));
  }
  let mut header = [0u8; RUN_HEADER_BYTES];
  file.read_exact(&mut header).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run header read", source })?;
  if header != encode_run_header(identity) {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_run_identity",
      "root-map run header differs from the workspace identity",
    ));
  }
  let body_length = length - minimum;
  let row_bytes = u64::try_from(row_width(identity.algorithm.hash_length())?)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("run row width exceeds u64: {error}")))?;
  if body_length % row_bytes != 0 {
    return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_run_length", "root-map run body is not an exact row multiple"));
  }
  file.seek(SeekFrom::Start(0)).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run rewind", source })?;
  let mut crc = crc32fast::Hasher::new();
  let mut remaining = length - RUN_CRC_BYTES as u64;
  let mut buffer = [0u8; IO_CHUNK_BYTES];
  while remaining != 0 {
    check_cancelled(cancellation)?;
    let width = usize::try_from(remaining.min(buffer.len() as u64))
      .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("run checksum width exceeds usize: {error}")))?;
    file
      .read_exact(&mut buffer[..width])
      .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run checksum read", source })?;
    crc.update(&buffer[..width]);
    remaining -= width as u64;
  }
  let mut stored_crc = [0u8; 4];
  file.read_exact(&mut stored_crc).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map run checksum", source })?;
  if crc.finalize() != u32::from_le_bytes(stored_crc) {
    return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_run_checksum", "root-map run checksum does not match its bytes"));
  }
  Ok(body_length / row_bytes)
}

fn run_encoded_length(algorithm: HashAlgorithm, rows: u64) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  let row_bytes = u64::try_from(row_width(algorithm.hash_length())?)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("run row width exceeds u64: {error}")))?;
  rows
    .checked_mul(row_bytes)
    .and_then(|value| value.checked_add((RUN_HEADER_BYTES + RUN_CRC_BYTES) as u64))
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map run length overflowed".to_string()))
}

fn run_path(directory: &Path, pass: u32, ordinal: u64) -> PathBuf {
  directory.join(format!("run-{pass:08x}-{ordinal:016x}.arun"))
}

#[derive(Debug)]
struct PreparedRootMapV1 {
  control_body: LegacyRootMapControlBodyV1,
  control_bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct LegacyRootMapPublicationReceiptV1 {
  pub page_count: u32,
  pub record_count: u32,
  pub control_sequence: u64,
  pub control_payload_hash: Vec<u8>,
  pub merge_passes: u32,
  pub maximum_run_rows: u64,
  pub maximum_open_runs: usize,
  pub maximum_publication_batch_bytes: usize,
  pub idempotent: bool,
}

pub struct LegacyRootMapPublicationRequestV1<'a> {
  pub workspace: LegacyRootMapStagingWorkspaceV1,
  pub retirement_owner: &'a mut RetirementJournalOwnerV1,
  pub cancellation: &'a CancellationToken,
  pub monotonic_now_ms: u64,
}

pub struct LegacyRootMapOwnerV1<'a> {
  destination: &'a V4FirstAuthorityPublisher,
}

impl<'a> LegacyRootMapOwnerV1<'a> {
  pub const fn new(destination: &'a V4FirstAuthorityPublisher) -> Self {
    Self { destination }
  }

  pub fn publish(
    &self,
    mut request: LegacyRootMapPublicationRequestV1<'_>,
    memory: &MemoryCoordinator,
  ) -> Result<LegacyRootMapPublicationReceiptV1, LegacyRootMapOwnerErrorV1> {
    check_cancelled(request.cancellation)?;
    if request.monotonic_now_ms == 0 {
      return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_time", "root-map monotonic publication time is zero"));
    }
    let seal = request.workspace.seal.clone().ok_or_else(|| {
      LegacyRootMapOwnerErrorV1::invalid("migration_root_map_unsealed", "root-map publication requires final mapping closure")
    })?;
    let initial = self.destination.observe()?;
    validate_destination_observation(&initial, request.workspace.identity, &seal.destination_namespace_root)?;
    let current = self.destination.load_mutable_system_control(
      SystemControlKindV1::LegacyRootMapControl,
      &request.workspace.identity.database_id,
      &request.workspace.identity.migration_id,
    )?;
    let control_sequence = current.as_ref().map_or(1, |selected| selected.control_sequence);
    let sorted = request.workspace.prepare_sorted(memory)?;
    let _publication_memory = memory
      .reserve(
        MemoryOwner::Migration,
        u64::try_from(request.workspace.options.maximum_publication_batch_bytes)
          .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("publication memory limit exceeds u64: {error}")))?,
        AdmissionClass::Maintenance,
      )
      .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;
    let prepared = prepare_pages(&mut request.workspace, &sorted, control_sequence)?;
    if let Some(selected) = current.as_ref() {
      if selected.bytes != prepared.control_bytes {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_selected_collision",
          "the selected finite root map differs from the staged mapping closure",
        ));
      }
    }
    if current.is_some() {
      let receipt = publication_receipt(&request.workspace, &sorted, &prepared, control_sequence, true);
      if let Err(source) = publish_staged_root_authorities(self.destination, &request.workspace, request.cancellation, memory) {
        return Err(LegacyRootMapOwnerErrorV1::SelectionCommitted { receipt: Box::new(receipt), source: Box::new(source) });
      }
      let reader = match VerifiedLegacyRootMapReaderV1::open(
        self.destination,
        request.workspace.identity.database_id,
        request.workspace.identity.migration_id,
        request.cancellation,
        memory,
      ) {
        Ok(reader) => reader,
        Err(source) => {
          return Err(LegacyRootMapOwnerErrorV1::SelectionCommitted { receipt: Box::new(receipt), source: Box::new(source) });
        }
      };
      if reader.control.sequence != control_sequence || reader.control.body != prepared.control_body {
        return Err(LegacyRootMapOwnerErrorV1::SelectionCommitted {
          receipt: Box::new(receipt),
          source: Box::new(LegacyRootMapOwnerErrorV1::invalid(
            "migration_root_map_selected_readback",
            "selected root-map retry readback differs from the prepared control",
          )),
        });
      }
      return Ok(receipt);
    }

    publish_staged_root_authorities(self.destination, &request.workspace, request.cancellation, memory)?;
    publish_pages(self.destination, &request.workspace, &prepared.control_body, request.cancellation)?;
    let before_control = self.destination.observe()?;
    validate_destination_observation(&before_control, request.workspace.identity, &seal.destination_namespace_root)?;
    check_cancelled(request.cancellation)?;
    let readback_memory = VerifiedLegacyRootMapReaderV1::reserve_open_memory(memory)?;
    check_cancelled(request.cancellation)?;
    let publication = match self.destination.publish_mutable_system_control_with_authority_expectation(
      MutableSystemControlPublicationRequestV1 {
        database_id: &request.workspace.identity.database_id,
        kind: SystemControlKindV1::LegacyRootMapControl,
        identity: &request.workspace.identity.migration_id,
        expected: None,
        guards: &[],
        encoded_control: &prepared.control_bytes,
        publication_timestamp_ms: request.workspace.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      MutableSystemControlAuthorityExpectationV1 {
        selected_header_sequence: before_control.selected.header.slot_sequence,
        head_hash: &seal.destination_namespace_root,
      },
      request.retirement_owner,
    ) {
      Ok(publication) => publication,
      Err(source) => {
        let Some((control_sequence, idempotent)) = source.committed_receipt().map(|receipt| (receipt.control_sequence, receipt.idempotent))
        else {
          return Err(LegacyRootMapOwnerErrorV1::MutablePublication(source));
        };
        let receipt = publication_receipt(&request.workspace, &sorted, &prepared, control_sequence, idempotent);
        return Err(LegacyRootMapOwnerErrorV1::SelectionCommitted {
          receipt: Box::new(receipt),
          source: Box::new(LegacyRootMapOwnerErrorV1::MutablePublication(source)),
        });
      }
    };
    let committed_readback = CancellationToken::new();
    let receipt = publication_receipt(&request.workspace, &sorted, &prepared, publication.control_sequence, publication.idempotent);
    let reader = match VerifiedLegacyRootMapReaderV1::open_with_reservation(
      self.destination,
      request.workspace.identity.database_id,
      request.workspace.identity.migration_id,
      &committed_readback,
      memory,
      readback_memory,
    ) {
      Ok(reader) => reader,
      Err(source) => return Err(LegacyRootMapOwnerErrorV1::SelectionCommitted { receipt: Box::new(receipt), source: Box::new(source) }),
    };
    if reader.control.sequence != publication.control_sequence || reader.control.body != prepared.control_body {
      return Err(LegacyRootMapOwnerErrorV1::SelectionCommitted {
        receipt: Box::new(receipt),
        source: Box::new(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_selected_readback",
          "selected root-map readback differs from the published control",
        )),
      });
    }
    Ok(receipt)
  }
}

fn publish_staged_root_authorities(
  destination: &V4FirstAuthorityPublisher,
  workspace: &LegacyRootMapStagingWorkspaceV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<(), LegacyRootMapOwnerErrorV1> {
  let mut stage = workspace
    .stage_file
    .try_clone()
    .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map authority stage clone", source })?;
  stage
    .seek(SeekFrom::Start(0))
    .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map authority stage rewind", source })?;
  loop {
    check_cancelled(cancellation)?;
    let mut roots = Vec::new();
    roots
      .try_reserve_exact(MAXIMUM_MIGRATION_ROOT_AUTHORITY_BATCH)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("migration-map authority batch allocation failed: {error}")))?;
    let mut seen = std::collections::HashSet::new();
    seen
      .try_reserve(MAXIMUM_MIGRATION_ROOT_AUTHORITY_BATCH)
      .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("migration-map authority identity allocation failed: {error}")))?;
    while roots.len() < MAXIMUM_MIGRATION_ROOT_AUTHORITY_BATCH {
      let Some(mapping) = read_next_stage_mapping(&mut stage, workspace.identity.algorithm)? else {
        break;
      };
      if seen.insert(mapping.namespace_root.root_hash.clone()) {
        roots.push(mapping);
      }
    }
    if roots.is_empty() {
      break;
    }
    let writes: Vec<_> = roots
      .iter()
      .map(|mapping| MigrationMapRootAuthorityWriteV1 {
        source_legacy_root: &mapping.row.legacy_root_hash,
        captured_source_write_sequence: mapping.row.captured_source_write_sequence,
        namespace_root: &mapping.namespace_root,
      })
      .collect();
    destination.publish_migration_map_root_authorities(MigrationMapRootAuthorityBatchPublicationRequestV1 {
      database_id: &workspace.identity.database_id,
      migration_id: &workspace.identity.migration_id,
      map_generation: workspace.identity.map_generation,
      roots: &writes,
      expected_destination_physical_instance_id: &workspace.identity.destination_physical_instance_id,
      expected_head_hash: &control_namespace_head(workspace)?,
      publication_timestamp_ms: workspace.publication_timestamp_ms,
      maximum_encoded_batch_bytes: workspace.options.maximum_publication_batch_bytes,
      cancellation,
      memory,
    })?;
  }
  Ok(())
}

fn publication_receipt(
  workspace: &LegacyRootMapStagingWorkspaceV1,
  sorted: &SortedRootMapV1,
  prepared: &PreparedRootMapV1,
  control_sequence: u64,
  idempotent: bool,
) -> LegacyRootMapPublicationReceiptV1 {
  LegacyRootMapPublicationReceiptV1 {
    page_count: prepared.control_body.page_count,
    record_count: prepared.control_body.record_count,
    control_sequence,
    control_payload_hash: digest_parts(workspace.identity.algorithm, &[&prepared.control_bytes]),
    merge_passes: sorted.merge_passes,
    maximum_run_rows: sorted.maximum_run_rows,
    maximum_open_runs: sorted.maximum_open_runs,
    maximum_publication_batch_bytes: workspace.options.maximum_publication_batch_bytes,
    idempotent,
  }
}

fn prepare_pages(
  workspace: &mut LegacyRootMapStagingWorkspaceV1,
  sorted: &SortedRootMapV1,
  control_sequence: u64,
) -> Result<PreparedRootMapV1, LegacyRootMapOwnerErrorV1> {
  let algorithm = workspace.identity.algorithm;
  let hash_width = algorithm.hash_length();
  let physical_rows = (PAGE_BODY_MAX_BYTES
    .checked_sub(96 + 2 * hash_width)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map page fixed body exceeds cap".to_string()))?)
    / row_width(hash_width)?;
  let page_rows = workspace.options.maximum_page_rows.min(physical_rows);
  if page_rows == 0 {
    return Err(LegacyRootMapOwnerErrorV1::Capacity("root-map page limit cannot hold one row".to_string()));
  }
  let page_rows_u64 = u64::try_from(page_rows)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("root-map page row limit exceeds u64: {error}")))?;
  let page_count_u64 = if sorted.row_count == 0 {
    0
  } else {
    sorted
      .row_count
      .checked_add(page_rows_u64 - 1)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map page count overflowed".to_string()))?
      / page_rows_u64
  };
  ensure_workspace_entry_capacity(page_peak_derived_entries(page_count_u64)?)?;
  let page_count = u32::try_from(page_count_u64)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("root-map page count exceeds u32: {error}")))?;
  let record_count = u32::try_from(sorted.row_count)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("root-map record count exceeds u32: {error}")))?;
  let zero = vec![0; hash_width];
  let first_page_hash = if page_count == 0 {
    zero.clone()
  } else {
    legacy_root_map_page_identity_hash(algorithm, workspace.identity.database_id, workspace.identity.migration_id, 0)?
  };
  let last_page_hash = if page_count == 0 {
    zero.clone()
  } else {
    legacy_root_map_page_identity_hash(
      algorithm,
      workspace.identity.database_id,
      workspace.identity.migration_id,
      u64::from(page_count - 1),
    )?
  };
  let mut body = LegacyRootMapControlBodyV1 {
    database_id: workspace.identity.database_id,
    migration_id: workspace.identity.migration_id,
    logical_database_id: workspace.identity.logical_database_id,
    source_physical_instance_id: workspace.identity.source_physical_instance_id,
    destination_physical_instance_id: workspace.identity.destination_physical_instance_id,
    map_generation: workspace.identity.map_generation,
    page_count,
    record_count,
    first_page_hash,
    last_page_hash,
    complete_map_digest: if page_count == 0 { zero.clone() } else { vec![1; hash_width] },
  };
  if page_count != 0 {
    let mut digest = LegacyRootMapChainVerifierV1::digest_builder(&body, algorithm)?;
    let mut reader = RunReaderV1::open(sorted.path.clone(), workspace.identity, &workspace.cancellation)?;
    for ordinal in 0..page_count {
      check_cancelled(&workspace.cancellation)?;
      let mut rows = Vec::new();
      rows
        .try_reserve_exact(page_rows)
        .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("root-map page row allocation failed: {error}")))?;
      while rows.len() < page_rows {
        let Some(row) = reader.next_row()? else {
          break;
        };
        rows.push(row);
      }
      if rows.is_empty() {
        return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_page_count", "sorted run ended before the declared page count"));
      }
      let previous_page_hash = if ordinal == 0 {
        zero.clone()
      } else {
        legacy_root_map_page_identity_hash(
          algorithm,
          workspace.identity.database_id,
          workspace.identity.migration_id,
          u64::from(ordinal - 1),
        )?
      };
      let next_page_hash = if ordinal + 1 == page_count {
        zero.clone()
      } else {
        legacy_root_map_page_identity_hash(
          algorithm,
          workspace.identity.database_id,
          workspace.identity.migration_id,
          u64::from(ordinal + 1),
        )?
      };
      let page = encode_legacy_root_map_page(
        &LegacyRootMapPageBodyV1 {
          database_id: workspace.identity.database_id,
          migration_id: workspace.identity.migration_id,
          logical_database_id: workspace.identity.logical_database_id,
          source_physical_instance_id: workspace.identity.source_physical_instance_id,
          destination_physical_instance_id: workspace.identity.destination_physical_instance_id,
          page_ordinal: u64::from(ordinal),
          previous_page_hash,
          next_page_hash,
          rows,
        },
        algorithm,
      )?;
      if page.len() > workspace.options.maximum_publication_batch_bytes {
        return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
          "one {}-byte root-map page exceeds the {}-byte publication batch bound",
          page.len(),
          workspace.options.maximum_publication_batch_bytes
        )));
      }
      workspace.reserve_stored_bytes(page.len())?;
      let path = page_path(&workspace.pages_path, ordinal);
      write_private_immutable(&path, &page, &workspace.cancellation)?;
      digest.push_page(&page)?;
    }
    if reader.next_row()?.is_some() {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_page_count",
        "sorted run contains rows beyond the declared page count",
      ));
    }
    body.complete_map_digest = digest.finish()?;
  }
  let control_bytes = encode_legacy_root_map_control(control_sequence, &body, algorithm)?;
  Ok(PreparedRootMapV1 { control_body: body, control_bytes })
}

fn publish_pages(
  destination: &V4FirstAuthorityPublisher,
  workspace: &LegacyRootMapStagingWorkspaceV1,
  control: &LegacyRootMapControlBodyV1,
  cancellation: &CancellationToken,
) -> Result<(), LegacyRootMapOwnerErrorV1> {
  let mut next = 0u32;
  while next < control.page_count {
    check_cancelled(cancellation)?;
    let mut pages: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut encoded_bytes = 0usize;
    while next < control.page_count && pages.len() < MAXIMUM_IMMUTABLE_CONTROL_BATCH {
      let bytes = read_private_file(&page_path(&workspace.pages_path, next), PAGE_BODY_MAX_BYTES + 512, cancellation)?;
      let projected = encoded_bytes
        .checked_add(bytes.len())
        .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map publication batch bytes overflowed".to_string()))?;
      if !pages.is_empty() && projected > workspace.options.maximum_publication_batch_bytes {
        break;
      }
      if projected > workspace.options.maximum_publication_batch_bytes {
        return Err(LegacyRootMapOwnerErrorV1::Capacity("one root-map page exceeds the publication batch bound".to_string()));
      }
      pages.push((page_identity(workspace.identity.migration_id, u64::from(next)), bytes));
      encoded_bytes = projected;
      next += 1;
    }
    let writes: Vec<_> = pages
      .iter()
      .map(|(identity, bytes)| ImmutableSystemControlWriteV1 {
        kind: SystemControlKindV1::LegacyRootMapPage,
        identity,
        encoded_control: bytes,
      })
      .collect();
    destination.publish_immutable_system_controls(ImmutableSystemControlBatchPublicationRequestV1 {
      database_id: &workspace.identity.database_id,
      controls: &writes,
      publication_timestamp_ms: workspace.publication_timestamp_ms,
    })?;
    for (identity, expected) in &pages {
      let loaded = destination
        .load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &workspace.identity.database_id, identity)?
        .ok_or_else(|| {
          LegacyRootMapOwnerErrorV1::invalid("migration_root_map_page_readback", "published immutable root-map page is missing")
        })?;
      if loaded.bytes != *expected {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_page_readback",
          "published immutable root-map page differs from prepared bytes",
        ));
      }
    }
    let observation = destination.observe()?;
    validate_destination_observation(&observation, workspace.identity, &control_namespace_head(workspace)?)?;
  }
  Ok(())
}

fn control_namespace_head(workspace: &LegacyRootMapStagingWorkspaceV1) -> Result<Vec<u8>, LegacyRootMapOwnerErrorV1> {
  workspace
    .seal
    .as_ref()
    .map(|seal| seal.destination_namespace_root.clone())
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_unsealed", "root-map workspace has no closure"))
}

fn validate_destination_observation(
  observation: &DatabaseHeaderObservationV4,
  identity: LegacyRootMapWorkspaceIdentityV1,
  expected_head: &[u8],
) -> Result<(), LegacyRootMapOwnerErrorV1> {
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded
    || header.database_id != identity.database_id
    || header.physical_instance_id != identity.destination_physical_instance_id
    || header.hash_algorithm != identity.algorithm
    || header.head_hash != expected_head
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_destination_authority",
      "destination identity, hash profile, redundancy, or selected HEAD changed",
    ));
  }
  Ok(())
}

pub struct VerifiedLegacyRootMapReaderV1<'a> {
  destination: &'a V4FirstAuthorityPublisher,
  memory: MemoryCoordinator,
  _retained_memory: MemoryReservation,
  algorithm: HashAlgorithm,
  control: super::migration_root_map::DecodedLegacyRootMapControlV1,
  loaded_control: LoadedMutableSystemControlV1,
  control_payload_hash: Vec<u8>,
  destination_header_sequence: u64,
  destination_head: Vec<u8>,
  destination_physical_instance_id: [u8; 16],
}

impl fmt::Debug for VerifiedLegacyRootMapReaderV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("VerifiedLegacyRootMapReaderV1")
      .field("algorithm", &self.algorithm)
      .field("control_sequence", &self.control.sequence)
      .field("map_generation", &self.control.body.map_generation)
      .field("page_count", &self.control.body.page_count)
      .field("record_count", &self.control.body.record_count)
      .field("destination_header_sequence", &self.destination_header_sequence)
      .finish_non_exhaustive()
  }
}

impl<'a> VerifiedLegacyRootMapReaderV1<'a> {
  pub fn open(
    destination: &'a V4FirstAuthorityPublisher,
    database_id: [u8; 16],
    migration_id: [u8; 16],
    cancellation: &CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    check_cancelled(cancellation)?;
    let retained_memory = Self::reserve_open_memory(memory)?;
    Self::open_with_reservation(destination, database_id, migration_id, cancellation, memory, retained_memory)
  }

  fn reserve_open_memory(memory: &MemoryCoordinator) -> Result<MemoryReservation, LegacyRootMapOwnerErrorV1> {
    memory
      .reserve(MemoryOwner::Migration, SELECTED_READER_OPEN_MEMORY_BYTES, AdmissionClass::Maintenance)
      .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))
  }

  fn open_with_reservation(
    destination: &'a V4FirstAuthorityPublisher,
    database_id: [u8; 16],
    migration_id: [u8; 16],
    cancellation: &CancellationToken,
    memory: &MemoryCoordinator,
    mut retained_memory: MemoryReservation,
  ) -> Result<Self, LegacyRootMapOwnerErrorV1> {
    check_cancelled(cancellation)?;
    if retained_memory.owner() != MemoryOwner::Migration || retained_memory.bytes() != SELECTED_READER_OPEN_MEMORY_BYTES {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_reader_reservation",
        "selected root-map reader requires its exact migration-memory reservation",
      ));
    }
    let observation = destination.observe()?;
    let algorithm = observation.selected.header.hash_algorithm;
    if observation.selected.redundancy_degraded || observation.selected.header.database_id != database_id {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_selected_authority",
        "selected root-map read requires matching non-degraded destination authority",
      ));
    }
    let loaded = destination
      .load_mutable_system_control(SystemControlKindV1::LegacyRootMapControl, &database_id, &migration_id)?
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_not_selected", "no legacy root map is selected"))?;
    let control = decode_legacy_root_map_control(&loaded.bytes, algorithm)?;
    if control.body.database_id != database_id
      || control.body.migration_id != migration_id
      || control.body.destination_physical_instance_id != observation.selected.header.physical_instance_id
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_selected_identity",
        "selected root-map control belongs to another database, migration, or destination incarnation",
      ));
    }
    let mut verifier = LegacyRootMapChainVerifierV1::new(&control, algorithm)?;
    for ordinal in 0..control.body.page_count {
      check_cancelled(cancellation)?;
      let identity = page_identity(migration_id, u64::from(ordinal));
      let page =
        destination.load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &database_id, &identity)?.ok_or_else(|| {
          LegacyRootMapOwnerErrorV1::invalid("migration_root_map_selected_page_missing", "selected root-map page is missing")
        })?;
      verifier.push_page(&page.bytes)?;
    }
    let verified_count = verifier.finish()?;
    if verified_count != control.body.record_count {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_selected_count",
        "selected root-map verifier returned a different record count",
      ));
    }
    retained_memory.shrink(SELECTED_READER_PAGE_MEMORY_BYTES).map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;
    let reader = Self {
      destination,
      memory: memory.clone(),
      _retained_memory: retained_memory,
      algorithm,
      control,
      control_payload_hash: digest_parts(algorithm, &[&loaded.bytes]),
      loaded_control: loaded,
      destination_header_sequence: observation.selected.header.slot_sequence,
      destination_head: observation.selected.header.head_hash.clone(),
      destination_physical_instance_id: observation.selected.header.physical_instance_id,
    };
    reader.validate_selected_unchanged()?;
    Ok(reader)
  }

  pub const fn record_count(&self) -> u32 {
    self.control.body.record_count
  }

  pub const fn destination_header_sequence(&self) -> u64 {
    self.destination_header_sequence
  }

  pub(crate) const fn control_body(&self) -> &LegacyRootMapControlBodyV1 {
    &self.control.body
  }

  pub(crate) fn control_payload_hash(&self) -> &[u8] {
    &self.control_payload_hash
  }

  pub(crate) fn destination_head(&self) -> &[u8] {
    &self.destination_head
  }

  pub(crate) fn control_expectation(&self) -> MutableSystemControlExpectationV1 {
    MutableSystemControlExpectationV1 {
      selected_slot: self.loaded_control.selected_slot,
      control_sequence: self.loaded_control.control_sequence,
      control_digest: self.loaded_control.control_digest.clone(),
    }
  }

  pub(crate) fn validate_selected_unchanged(&self) -> Result<(), LegacyRootMapOwnerErrorV1> {
    let before = self.destination.observe()?;
    self.validate_destination_observation(&before)?;
    let selected = self
      .destination
      .load_mutable_system_control(
        SystemControlKindV1::LegacyRootMapControl,
        &self.control.body.database_id,
        &self.control.body.migration_id,
      )?
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_selected_changed", "selected root-map control disappeared"))?;
    if selected != self.loaded_control {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_selected_changed",
        "selected root-map control changed after reader verification",
      ));
    }
    let after = self.destination.observe()?;
    self.validate_destination_observation(&after)
  }

  fn validate_destination_observation(&self, observation: &DatabaseHeaderObservationV4) -> Result<(), LegacyRootMapOwnerErrorV1> {
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.database_id != self.control.body.database_id
      || header.physical_instance_id != self.destination_physical_instance_id
      || header.hash_algorithm != self.algorithm
      || header.slot_sequence != self.destination_header_sequence
      || header.head_hash != self.destination_head
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_selected_changed",
        "destination authority changed after root-map reader verification",
      ));
    }
    Ok(())
  }

  pub fn lookup(
    &self,
    legacy_root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<Option<LegacyRootMapRowV1>, LegacyRootMapOwnerErrorV1> {
    if legacy_root_hash.len() != self.algorithm.hash_length() || all_zero(legacy_root_hash) {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_lookup_hash",
        "legacy root lookup requires one nonzero database-width hash",
      ));
    }
    for ordinal in 0..self.control.body.page_count {
      check_cancelled(cancellation)?;
      let _page_memory = self
        .memory
        .reserve(MemoryOwner::Migration, SELECTED_READER_PAGE_MEMORY_BYTES, AdmissionClass::Maintenance)
        .map_err(|source| LegacyRootMapOwnerErrorV1::Memory(Box::new(source)))?;
      let identity = page_identity(self.control.body.migration_id, u64::from(ordinal));
      let loaded = self
        .destination
        .load_immutable_system_control(SystemControlKindV1::LegacyRootMapPage, &self.control.body.database_id, &identity)?
        .ok_or_else(|| {
          LegacyRootMapOwnerErrorV1::invalid("migration_root_map_selected_page_missing", "verified root-map page is now missing")
        })?;
      let page = decode_legacy_root_map_page(&loaded.bytes, self.algorithm)?;
      if page.sequence != self.control.sequence {
        return Err(LegacyRootMapOwnerErrorV1::invalid(
          "migration_root_map_selected_page_identity",
          "selected root-map lookup page belongs to another control sequence",
        ));
      }
      self.validate_lookup_page(&page.body, ordinal)?;
      match page.body.rows.binary_search_by(|row| row.legacy_root_hash.as_slice().cmp(legacy_root_hash)) {
        Ok(index) => {
          let row = page.body.rows[index].clone();
          self.validate_selected_unchanged()?;
          return Ok(Some(row));
        }
        Err(index) => {
          if index < page.body.rows.len() {
            self.validate_selected_unchanged()?;
            return Ok(None);
          }
        }
      }
    }
    self.validate_selected_unchanged()?;
    Ok(None)
  }

  fn validate_lookup_page(&self, page: &LegacyRootMapPageBodyV1, ordinal: u32) -> Result<(), LegacyRootMapOwnerErrorV1> {
    let zero = vec![0; self.algorithm.hash_length()];
    let previous = if ordinal == 0 {
      zero.clone()
    } else {
      legacy_root_map_page_identity_hash(
        self.algorithm,
        self.control.body.database_id,
        self.control.body.migration_id,
        u64::from(ordinal - 1),
      )?
    };
    let next = if ordinal + 1 == self.control.body.page_count {
      zero
    } else {
      legacy_root_map_page_identity_hash(
        self.algorithm,
        self.control.body.database_id,
        self.control.body.migration_id,
        u64::from(ordinal + 1),
      )?
    };
    if page.database_id != self.control.body.database_id
      || page.migration_id != self.control.body.migration_id
      || page.logical_database_id != self.control.body.logical_database_id
      || page.source_physical_instance_id != self.control.body.source_physical_instance_id
      || page.destination_physical_instance_id != self.control.body.destination_physical_instance_id
      || page.page_ordinal != u64::from(ordinal)
      || page.previous_page_hash != previous
      || page.next_page_hash != next
    {
      return Err(LegacyRootMapOwnerErrorV1::invalid(
        "migration_root_map_selected_page_identity",
        "selected root-map lookup page has inconsistent identity or linkage",
      ));
    }
    Ok(())
  }
}

fn page_identity(migration_id: [u8; 16], ordinal: u64) -> Vec<u8> {
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&migration_id);
  identity.extend_from_slice(&ordinal.to_le_bytes());
  identity
}

fn page_path(directory: &Path, ordinal: u32) -> PathBuf {
  directory.join(format!("page-{ordinal:08x}.alrp"))
}

fn inventory_workspace(workspace: &Path, maximum_bytes: u64, cancellation: &CancellationToken) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  validate_private_directory_readonly(workspace, "legacy root-map workspace")?;
  let runs = workspace.join("runs");
  let pages = workspace.join("pages");
  let mut entries = 0u64;
  let mut total = 0u64;
  for entry in
    fs::read_dir(workspace).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map workspace inventory", source })?
  {
    check_cancelled(cancellation)?;
    entries = observe_workspace_entry(entries)?;
    let entry = entry.map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map workspace inventory entry", source })?;
    let path = entry.path();
    if path == runs || path == pages {
      validate_private_directory_readonly(&path, "legacy root-map derived directory")?;
      continue;
    }
    let name = entry
      .file_name()
      .into_string()
      .map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(format!("root-map workspace filename is not UTF-8: {source:?}")))?;
    if !matches!(name.as_str(), "workspace.armw" | "rows.stage" | "closure.armc") {
      return Err(LegacyRootMapOwnerErrorV1::Workspace(format!("root-map workspace contains unknown top-level entry {name}")));
    }
    total = account_private_file(&path, "legacy root-map workspace file", total, maximum_bytes)?;
  }
  let (observed_entries, observed_total) =
    inventory_derived_directory(&runs, DerivedKindV1::Run, entries, total, maximum_bytes, cancellation)?;
  entries = observed_entries;
  total = observed_total;
  total = inventory_derived_directory(&pages, DerivedKindV1::Page, entries, total, maximum_bytes, cancellation)?.1;
  Ok(total)
}

fn remove_stale_pending_files(
  directory: &Path,
  role: &str,
  cancellation: &CancellationToken,
  entries: &mut u64,
) -> Result<(), LegacyRootMapOwnerErrorV1> {
  validate_private_directory(directory, role)?;
  let mut removed = false;
  for entry in
    fs::read_dir(directory).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map pending-file inventory", source })?
  {
    check_cancelled(cancellation)?;
    *entries = observe_workspace_entry(*entries)?;
    let entry = entry.map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map pending-file inventory entry", source })?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(format!("root-map pending filename is not UTF-8: {source:?}")))?;
    if !canonical_pending_name(&name) {
      continue;
    }
    let path = entry.path();
    let file = open_regular_file_no_follow(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
    validate_private_regular_file(&path, &file, "legacy root-map stale pending file")?;
    drop(file);
    fs::remove_file(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map stale pending-file removal", source })?;
    removed = true;
  }
  if removed {
    sync_directory_native(directory).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
  }
  Ok(())
}

#[derive(Clone, Copy)]
enum DerivedKindV1 {
  Run,
  Page,
}

fn inventory_derived_directory(
  directory: &Path,
  kind: DerivedKindV1,
  mut entries: u64,
  mut total: u64,
  maximum_bytes: u64,
  cancellation: &CancellationToken,
) -> Result<(u64, u64), LegacyRootMapOwnerErrorV1> {
  for entry in
    fs::read_dir(directory).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map derived inventory", source })?
  {
    check_cancelled(cancellation)?;
    entries = observe_workspace_entry(entries)?;
    let entry = entry.map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map derived inventory entry", source })?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(format!("root-map derived filename is not UTF-8: {source:?}")))?;
    let accepted = match kind {
      DerivedKindV1::Run => canonical_run_name(&name) || canonical_pending_name(&name),
      DerivedKindV1::Page => canonical_page_name(&name) || canonical_pending_name(&name),
    };
    if !accepted {
      return Err(LegacyRootMapOwnerErrorV1::Workspace(format!("root-map derived directory contains unknown entry {name}")));
    }
    total = account_private_file(&entry.path(), "legacy root-map derived file", total, maximum_bytes)?;
  }
  Ok((entries, total))
}

fn clear_derived_directory(directory: &Path, role: &str, cancellation: &CancellationToken) -> Result<(), LegacyRootMapOwnerErrorV1> {
  validate_private_directory(directory, role)?;
  let mut entries = 0u64;
  for entry in fs::read_dir(directory).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map derived cleanup", source })? {
    check_cancelled(cancellation)?;
    entries = observe_workspace_entry(entries)?;
    let entry = entry.map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map derived cleanup entry", source })?;
    let path = entry.path();
    let file = open_regular_file_no_follow(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
    validate_private_regular_file(&path, &file, role)?;
    drop(file);
    fs::remove_file(&path).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map derived file removal", source })?;
  }
  sync_directory_native(directory).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))
}

fn observe_workspace_entry(current: u64) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  let next =
    current.checked_add(1).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map workspace entry count overflowed".to_string()))?;
  if next > MAXIMUM_WORKSPACE_ENTRIES {
    return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
      "root-map workspace exceeds the bounded {MAXIMUM_WORKSPACE_ENTRIES}-entry inventory"
    )));
  }
  Ok(next)
}

fn initial_run_peak_derived_entries(staged_rows: u64, maximum_rows: u64) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  if maximum_rows == 0 {
    return Err(LegacyRootMapOwnerErrorV1::Capacity(
      "root-map run row limit must be nonzero when calculating workspace capacity".to_string(),
    ));
  }
  let initial_runs = if staged_rows == 0 {
    1
  } else {
    staged_rows
      .checked_add(maximum_rows - 1)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map initial run planning overflowed".to_string()))?
      / maximum_rows
  };
  if initial_runs > 1 {
    initial_runs
      .checked_add(1)
      .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map merge pending-file planning overflowed".to_string()))
  } else {
    Ok(initial_runs)
  }
}

fn page_peak_derived_entries(page_count: u64) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  page_count.checked_add(1).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map page workspace planning overflowed".to_string()))
}

fn ensure_workspace_entry_capacity(derived_entries: u64) -> Result<(), LegacyRootMapOwnerErrorV1> {
  let projected = SEALED_WORKSPACE_ENTRY_COUNT
    .checked_add(derived_entries)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map workspace entry planning overflowed".to_string()))?;
  if projected > MAXIMUM_WORKSPACE_ENTRIES {
    return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
      "root-map workspace requires {projected} entries, exceeding the {MAXIMUM_WORKSPACE_ENTRIES}-entry bound"
    )));
  }
  Ok(())
}

fn account_private_file(path: &Path, role: &str, total: u64, maximum_bytes: u64) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  let file = open_regular_file_no_follow(path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
  validate_private_regular_file(path, &file, role)?;
  let length =
    file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map inventory file metadata", source })?.len();
  let projected =
    total.checked_add(length).ok_or_else(|| LegacyRootMapOwnerErrorV1::Capacity("root-map workspace byte count overflowed".to_string()))?;
  if projected > maximum_bytes {
    return Err(LegacyRootMapOwnerErrorV1::Capacity(format!(
      "root-map workspace uses at least {projected} bytes, exceeding cap {maximum_bytes}"
    )));
  }
  Ok(projected)
}

fn write_private_immutable(target: &Path, bytes: &[u8], cancellation: &CancellationToken) -> Result<(), LegacyRootMapOwnerErrorV1> {
  check_cancelled(cancellation)?;
  let parent =
    target.parent().ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_path", "private target has no parent"))?;
  validate_private_directory(parent, "legacy root-map private-file parent")?;
  let pending = parent.join(format!(".root-map-{}.pending", uuid::Uuid::new_v4().simple()));
  let mut file = create_new_regular_file_no_follow(&pending).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
  secure_platform_private_regular_file(&pending)?;
  validate_private_regular_file(&pending, &file, "legacy root-map pending file")?;
  file.write_all(bytes).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map private-file write", source })?;
  sync_file_all_native(&file).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))?;
  drop(file);
  check_cancelled(cancellation)?;
  durable_install_new_native(&pending, target).map_err(|source| LegacyRootMapOwnerErrorV1::Durability(Box::new(source)))
}

fn read_private_file(path: &Path, maximum_bytes: usize, cancellation: &CancellationToken) -> Result<Vec<u8>, LegacyRootMapOwnerErrorV1> {
  check_cancelled(cancellation)?;
  let mut file = open_regular_file_no_follow(path).map_err(|source| LegacyRootMapOwnerErrorV1::Workspace(source.to_string()))?;
  validate_private_regular_file(path, &file, "legacy root-map private file")?;
  let length =
    file.metadata().map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map private-file metadata", source })?.len();
  let length =
    usize::try_from(length).map_err(|error| LegacyRootMapOwnerErrorV1::Capacity(format!("private-file length exceeds usize: {error}")))?;
  if length > maximum_bytes {
    return Err(LegacyRootMapOwnerErrorV1::Capacity(format!("root-map private file has {length} bytes, exceeding cap {maximum_bytes}")));
  }
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(length)
    .map_err(|error| LegacyRootMapOwnerErrorV1::Allocation(format!("private-file allocation failed: {error}")))?;
  bytes.resize(length, 0);
  file.read_exact(&mut bytes).map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map private-file read", source })?;
  let mut trailing = [0u8; 1];
  if file
    .read(&mut trailing)
    .map_err(|source| LegacyRootMapOwnerErrorV1::Io { operation: "root-map private-file trailing read", source })?
    != 0
  {
    return Err(LegacyRootMapOwnerErrorV1::invalid("migration_root_map_file_growth", "root-map private file changed while it was read"));
  }
  Ok(bytes)
}

fn canonical_run_name(name: &str) -> bool {
  let Some((pass, ordinal)) =
    name.strip_prefix("run-").and_then(|value| value.strip_suffix(".arun")).and_then(|value| value.split_once('-'))
  else {
    return false;
  };
  pass.len() == 8
    && ordinal.len() == 16
    && pass.bytes().chain(ordinal.bytes()).all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn canonical_page_name(name: &str) -> bool {
  name
    .strip_prefix("page-")
    .and_then(|value| value.strip_suffix(".alrp"))
    .is_some_and(|ordinal| ordinal.len() == 8 && ordinal.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
}

fn canonical_pending_name(name: &str) -> bool {
  name
    .strip_prefix(".root-map-")
    .and_then(|value| value.strip_suffix(".pending"))
    .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.simple().to_string() == value))
}

fn validate_publication_timestamp(timestamp_ms: u64) -> Result<(), LegacyRootMapOwnerErrorV1> {
  if timestamp_ms == 0 || timestamp_ms > i64::MAX as u64 {
    return Err(LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_time",
      "root-map publication timestamp must fit the portable signed millisecond domain",
    ));
  }
  Ok(())
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), LegacyRootMapOwnerErrorV1> {
  if cancellation.is_cancelled() {
    Err(LegacyRootMapOwnerErrorV1::Canceled)
  } else {
    Ok(())
  }
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn fixed_array<const N: usize>(bytes: &[u8], offset: usize, field: &'static str) -> Result<[u8; N], LegacyRootMapOwnerErrorV1> {
  let end = offset.checked_add(N).ok_or_else(|| {
    LegacyRootMapOwnerErrorV1::invalid(
      "migration_root_map_workspace_bounds",
      format!("{field} offset overflows the addressable input range"),
    )
  })?;
  let value = bytes
    .get(offset..end)
    .ok_or_else(|| LegacyRootMapOwnerErrorV1::invalid("migration_root_map_workspace_bounds", format!("{field} is out of bounds")))?;
  value.try_into().map_err(|source| {
    LegacyRootMapOwnerErrorV1::invalid("migration_root_map_workspace_bounds", format!("{field} has an invalid fixed width: {source}"))
  })
}

fn array_16(bytes: &[u8], offset: usize) -> Result<[u8; 16], LegacyRootMapOwnerErrorV1> {
  fixed_array(bytes, offset, "16-byte field")
}

fn array_32(bytes: &[u8], offset: usize) -> Result<[u8; 32], LegacyRootMapOwnerErrorV1> {
  fixed_array(bytes, offset, "32-byte field")
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, LegacyRootMapOwnerErrorV1> {
  Ok(u16::from_le_bytes(fixed_array(bytes, offset, "u16 field")?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, LegacyRootMapOwnerErrorV1> {
  Ok(u32::from_le_bytes(fixed_array(bytes, offset, "u32 field")?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, LegacyRootMapOwnerErrorV1> {
  Ok(u64::from_le_bytes(fixed_array(bytes, offset, "u64 field")?))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
#[path = "../../../spec/engine/migration_root_map_owner_internal_spec.rs"]
mod migration_root_map_owner_internal_spec;
