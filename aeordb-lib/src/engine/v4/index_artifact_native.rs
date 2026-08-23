//! Captured-header native authority for selected immutable index pages.

use std::fmt;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::HashAlgorithm;

use super::admission::CapabilitySetV1;
use super::database_header::SelectedDatabaseHeaderV4;
use super::entity::checked_whole_entity_encoded_length;
use super::first_authority::{FirstAuthorityPublicationErrorV1, V4FirstAuthorityPublisher};
use super::index_artifact::{ImmutableIndexArtifactKindV1, IndexManifestV1, decode_index_manifest, validate_correctness_manifest_chain};
use super::index_artifact_cursor::{
  ArtifactCursorReadErrorV1, ArtifactCursorSourceV1, ArtifactPageCursorErrorV1, ArtifactPageCursorLimitsV1, ArtifactPageCursorRequestV1,
  ArtifactPageCursorRootV1, ArtifactPageNeighborModeV1, ArtifactPageSeekV1, LoadedArtifactLeafCursorV1, LoadedArtifactPageCursorV1,
  RetainedArtifactBytesV1, load_artifact_leaf_cursor_v1, load_artifact_page_cursor_v1,
};
use super::index_coverage_registry::{
  IndexCoverageNvtDescriptorV1, field_definition_fingerprint, field_dependency_fingerprint, scope_definition_fingerprint,
  scope_dependency_fingerprint,
};
use super::index_manifest::IndexManifestBodyV1;
use super::index_nvt::{
  NvtBasisStatusV1, NvtTileLeafValidatorV1, PinnedFieldNvtV1, coordinate_cell, decode_nvt_tile, validate_field_nvt_basis_v1,
};
use super::index_page::{OrderedIndexRoleV1, compare_order_keys, decode_artifact_directory, decode_ordered_page};
use super::query_planner::QueryPlanningCoverageGenerationV1;

const MANIFEST_READ_MAXIMUM_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSelectedArtifactCursorErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  Unavailable,
  Corrupt,
  Cancelled,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{code}: {context}")]
pub struct NativeSelectedArtifactCursorErrorV1 {
  class: NativeSelectedArtifactCursorErrorClassV1,
  code: &'static str,
  context: String,
}

impl NativeSelectedArtifactCursorErrorV1 {
  fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedArtifactCursorErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedArtifactCursorErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedArtifactCursorErrorClassV1::Unavailable, code, context: context.into() }
  }

  fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedArtifactCursorErrorClassV1::Corrupt, code, context: context.into() }
  }

  fn cancelled() -> Self {
    Self {
      class: NativeSelectedArtifactCursorErrorClassV1::Cancelled,
      code: "selected_artifact_cursor_cancelled",
      context: "selected artifact cursor was cancelled".to_owned(),
    }
  }

  pub const fn class(&self) -> NativeSelectedArtifactCursorErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

pub struct NativeSelectedArtifactPageCursorV1 {
  selected_root: Vec<u8>,
  coverage_source_root: Vec<u8>,
  manifest_hash: Vec<u8>,
  owner_id: Vec<u8>,
  root_key: Vec<u8>,
  generation: u64,
  role: OrderedIndexRoleV1,
  cursor: LoadedArtifactPageCursorV1,
  _artifact_memory: Vec<MemoryReservation>,
  _workspace_memory: MemoryReservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSelectedPostingSeekSourceV1 {
  NvtHint,
  ExactDirectory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSelectedNvtFallbackReasonV1 {
  Absent,
  Corrupt,
  Unavailable,
  ResourceLimit,
  StalePageHint,
  MissingPredecessor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSelectedNvtFallbackV1 {
  reason: NativeSelectedNvtFallbackReasonV1,
  diagnostic_code: Option<&'static str>,
}

impl NativeSelectedNvtFallbackV1 {
  pub const fn reason(&self) -> NativeSelectedNvtFallbackReasonV1 {
    self.reason
  }

  pub const fn diagnostic_code(&self) -> Option<&'static str> {
    self.diagnostic_code
  }
}

pub struct NativeSelectedPostingPageV1 {
  cursor: NativeSelectedArtifactPageCursorV1,
  source: NativeSelectedPostingSeekSourceV1,
  nvt_fallback: Option<NativeSelectedNvtFallbackV1>,
}

impl fmt::Debug for NativeSelectedPostingPageV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("NativeSelectedPostingPageV1")
      .field("cursor", &self.cursor)
      .field("source", &self.source)
      .field("nvt_fallback", &self.nvt_fallback)
      .finish()
  }
}

impl NativeSelectedPostingPageV1 {
  pub const fn cursor(&self) -> &NativeSelectedArtifactPageCursorV1 {
    &self.cursor
  }

  pub const fn source(&self) -> NativeSelectedPostingSeekSourceV1 {
    self.source
  }

  pub const fn nvt_fallback(&self) -> Option<&NativeSelectedNvtFallbackV1> {
    self.nvt_fallback.as_ref()
  }
}

impl fmt::Debug for NativeSelectedArtifactPageCursorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("NativeSelectedArtifactPageCursorV1")
      .field("selected_root", &hex::encode(&self.selected_root))
      .field("coverage_source_root", &hex::encode(&self.coverage_source_root))
      .field("manifest_hash", &hex::encode(&self.manifest_hash))
      .field("owner_id", &hex::encode(&self.owner_id))
      .field("root_key", &hex::encode(&self.root_key))
      .field("generation", &self.generation)
      .field("role", &self.role)
      .field("page_ordinal", &self.cursor.page_ordinal())
      .finish_non_exhaustive()
  }
}

impl NativeSelectedArtifactPageCursorV1 {
  pub fn selected_root(&self) -> &[u8] {
    &self.selected_root
  }

  pub fn coverage_source_root(&self) -> &[u8] {
    &self.coverage_source_root
  }

  pub fn manifest_hash(&self) -> &[u8] {
    &self.manifest_hash
  }

  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }

  pub fn root_key(&self) -> &[u8] {
    &self.root_key
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn role(&self) -> OrderedIndexRoleV1 {
    self.role
  }

  pub const fn cursor(&self) -> &LoadedArtifactPageCursorV1 {
    &self.cursor
  }
}

struct AccountedArtifactBytesV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

#[derive(Debug)]
struct SelectedDirectoryClosureV1 {
  root_key: Vec<u8>,
  owner_id: Vec<u8>,
  generation: u64,
  expected_live_count: u64,
  expected_tombstone_count: u64,
  expected_page_count: u64,
  expected_logical_bytes: Option<u64>,
  expected_first_page_id: Option<u64>,
  expected_last_page_id: Option<u64>,
  expected_next_page_id: Option<u64>,
}

struct CapturedNativeArtifactCursorSourceV1<'a> {
  publisher: &'a V4FirstAuthorityPublisher,
  captured: &'a SelectedDatabaseHeaderV4,
  cancellation: &'a CancellationToken,
  memory: &'a MemoryCoordinator,
  reservations: Vec<MemoryReservation>,
}

impl ArtifactCursorSourceV1 for CapturedNativeArtifactCursorSourceV1<'_> {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    match load_accounted_artifact(self.publisher, self.captured, key, maximum_bytes, self.cancellation, self.memory) {
      Ok(Some(loaded)) => {
        self.reservations.push(loaded._memory);
        Ok(RetainedArtifactBytesV1::from_bytes(loaded.bytes))
      }
      Ok(None) => Err(ArtifactCursorReadErrorV1::Missing),
      Err(error) => Err(map_native_source_error(error)),
    }
  }
}

#[derive(Clone, Copy)]
pub(crate) struct NativeSelectedArtifactLoadRequestV1<'a> {
  pub publisher: &'a V4FirstAuthorityPublisher,
  pub memory: &'a MemoryCoordinator,
  pub captured: &'a SelectedDatabaseHeaderV4,
  pub supported_reader_capabilities: CapabilitySetV1,
  pub selected_root: &'a [u8],
  pub selected_generation: &'a QueryPlanningCoverageGenerationV1,
  pub role: OrderedIndexRoleV1,
  pub seek: ArtifactPageSeekV1<'a>,
  pub neighbors: ArtifactPageNeighborModeV1,
  pub limits: ArtifactPageCursorLimitsV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeSelectedPostingSeekLoadRequestV1<'a> {
  pub publisher: &'a V4FirstAuthorityPublisher,
  pub memory: &'a MemoryCoordinator,
  pub captured: &'a SelectedDatabaseHeaderV4,
  pub supported_reader_capabilities: CapabilitySetV1,
  pub selected_root: &'a [u8],
  pub selected_generation: &'a QueryPlanningCoverageGenerationV1,
  pub nvt_descriptor: Option<&'a IndexCoverageNvtDescriptorV1>,
  pub target_coordinate: u64,
  pub target_posting_position: &'a [u8],
  pub neighbors: ArtifactPageNeighborModeV1,
  pub limits: ArtifactPageCursorLimitsV1,
  pub cancellation: &'a CancellationToken,
}

pub(crate) fn load_native_selected_artifact_page_cursor_v1(
  request: NativeSelectedArtifactLoadRequestV1<'_>,
) -> Result<Option<NativeSelectedArtifactPageCursorV1>, NativeSelectedArtifactCursorErrorV1> {
  let NativeSelectedArtifactLoadRequestV1 {
    publisher,
    memory,
    captured,
    supported_reader_capabilities,
    selected_root,
    selected_generation,
    role,
    seek,
    neighbors,
    limits,
    cancellation,
  } = request;
  require_not_cancelled(cancellation)?;
  validate_selected_generation_envelope(captured.header.hash_algorithm, selected_root, selected_generation)?;
  let workspace_memory = reserve_cursor_workspace(memory, limits)?;
  let Some(closure) =
    load_selected_directory_closure(publisher, memory, captured, supported_reader_capabilities, selected_generation, role, cancellation)?
  else {
    return Ok(None);
  };
  load_native_selected_artifact_page_cursor_from_closure_v1(
    publisher,
    memory,
    captured,
    selected_root,
    selected_generation,
    role,
    seek,
    neighbors,
    limits,
    cancellation,
    &closure,
    workspace_memory,
  )
}

#[allow(clippy::too_many_arguments)]
fn load_native_selected_artifact_page_cursor_from_closure_v1(
  publisher: &V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  captured: &SelectedDatabaseHeaderV4,
  selected_root: &[u8],
  selected_generation: &QueryPlanningCoverageGenerationV1,
  role: OrderedIndexRoleV1,
  seek: ArtifactPageSeekV1<'_>,
  neighbors: ArtifactPageNeighborModeV1,
  limits: ArtifactPageCursorLimitsV1,
  cancellation: &CancellationToken,
  closure: &SelectedDirectoryClosureV1,
  workspace_memory: MemoryReservation,
) -> Result<Option<NativeSelectedArtifactPageCursorV1>, NativeSelectedArtifactCursorErrorV1> {
  require_not_cancelled(cancellation)?;
  let mut source = CapturedNativeArtifactCursorSourceV1 { publisher, captured, cancellation, memory, reservations: Vec::new() };
  let request = ArtifactPageCursorRequestV1 {
    root: ArtifactPageCursorRootV1 {
      hash_algorithm: captured.header.hash_algorithm,
      root_key: &closure.root_key,
      owner_id: &closure.owner_id,
      role,
      maximum_generation: closure.generation,
      expected_summary: None,
    },
    seek,
    neighbors,
    limits,
  };
  let Some(cursor) = load_artifact_page_cursor_v1(&request, &mut source, &|| cancellation.is_cancelled()).map_err(map_cursor_error)? else {
    return Ok(None);
  };
  validate_manifest_root_closure(captured.header.hash_algorithm, role, closure, &cursor)?;
  require_not_cancelled(cancellation)?;
  Ok(Some(NativeSelectedArtifactPageCursorV1 {
    selected_root: copy_bytes(selected_root, "selected root")?,
    coverage_source_root: copy_bytes(&selected_generation.source_namespace_root, "coverage source root")?,
    manifest_hash: copy_bytes(&selected_generation.manifest_hash, "selected manifest")?,
    owner_id: copy_bytes(&closure.owner_id, "selected artifact owner")?,
    root_key: copy_bytes(&closure.root_key, "selected artifact root")?,
    generation: closure.generation,
    role,
    cursor,
    _artifact_memory: source.reservations,
    _workspace_memory: workspace_memory,
  }))
}

pub(crate) fn load_native_selected_posting_seek_v1(
  request: NativeSelectedPostingSeekLoadRequestV1<'_>,
) -> Result<Option<NativeSelectedPostingPageV1>, NativeSelectedArtifactCursorErrorV1> {
  require_not_cancelled(request.cancellation)?;
  compare_order_keys(
    request.captured.header.hash_algorithm,
    OrderedIndexRoleV1::Posting,
    request.target_posting_position,
    request.target_posting_position,
  )
  .map_err(|error| NativeSelectedArtifactCursorErrorV1::invalid("selected_posting_target", error.to_string()))?;
  validate_selected_generation_envelope(request.captured.header.hash_algorithm, request.selected_root, request.selected_generation)?;
  let mut workspace_memory = Some(reserve_cursor_workspace(request.memory, request.limits)?);
  let Some(closure) = load_selected_directory_closure(
    request.publisher,
    request.memory,
    request.captured,
    request.supported_reader_capabilities,
    request.selected_generation,
    OrderedIndexRoleV1::Posting,
    request.cancellation,
  )?
  else {
    return Ok(None);
  };
  let hint = match request.nvt_descriptor {
    Some(descriptor) => match select_native_nvt_predecessor_page_id_v1(&request, &closure, descriptor) {
      Ok(Some(page_id)) => Ok(page_id),
      Ok(None) => Err(nvt_fallback(NativeSelectedNvtFallbackReasonV1::MissingPredecessor, None)),
      Err(error) => Err(map_nvt_fallback_error(error)?),
    },
    None => Err(nvt_fallback(NativeSelectedNvtFallbackReasonV1::Absent, None)),
  };
  let fallback = match hint {
    Ok(page_id) => {
      let workspace = workspace_memory.take().ok_or_else(|| {
        NativeSelectedArtifactCursorErrorV1::corrupt(
          "selected_posting_workspace_state",
          "selected Posting hint attempt lost its admitted workspace reservation",
        )
      })?;
      let hinted = load_native_selected_artifact_page_cursor_from_closure_v1(
        request.publisher,
        request.memory,
        request.captured,
        request.selected_root,
        request.selected_generation,
        OrderedIndexRoleV1::Posting,
        ArtifactPageSeekV1::PageId(page_id),
        request.neighbors,
        request.limits,
        request.cancellation,
        &closure,
        workspace,
      );
      match hinted {
        Ok(Some(cursor)) => {
          let page = decode_ordered_page(cursor.cursor().page(), request.captured.header.hash_algorithm)
            .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_posting_hint_page", error.to_string()))?;
          if compare_order_keys(
            request.captured.header.hash_algorithm,
            OrderedIndexRoleV1::Posting,
            page.lower_fence,
            request.target_posting_position,
          )
          .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_posting_hint_fence", error.to_string()))?
            != std::cmp::Ordering::Greater
          {
            return Ok(Some(NativeSelectedPostingPageV1 {
              cursor,
              source: NativeSelectedPostingSeekSourceV1::NvtHint,
              nvt_fallback: None,
            }));
          }
          drop(cursor);
          nvt_fallback(NativeSelectedNvtFallbackReasonV1::StalePageHint, None)
        }
        Ok(None) => nvt_fallback(NativeSelectedNvtFallbackReasonV1::StalePageHint, None),
        Err(error) if error.class() == NativeSelectedArtifactCursorErrorClassV1::ResourceLimit => {
          nvt_fallback(NativeSelectedNvtFallbackReasonV1::ResourceLimit, Some(error.code()))
        }
        Err(error) => return Err(error),
      }
    }
    Err(reason) => reason,
  };
  let workspace_memory = match workspace_memory {
    Some(workspace) => workspace,
    None => reserve_cursor_workspace(request.memory, request.limits)?,
  };
  let exact = load_native_selected_artifact_page_cursor_from_closure_v1(
    request.publisher,
    request.memory,
    request.captured,
    request.selected_root,
    request.selected_generation,
    OrderedIndexRoleV1::Posting,
    ArtifactPageSeekV1::OrderPredecessor(request.target_posting_position),
    request.neighbors,
    request.limits,
    request.cancellation,
    &closure,
    workspace_memory,
  )?;
  Ok(exact.map(|cursor| NativeSelectedPostingPageV1 {
    cursor,
    source: NativeSelectedPostingSeekSourceV1::ExactDirectory,
    nvt_fallback: Some(fallback),
  }))
}

fn reserve_cursor_workspace(
  memory: &MemoryCoordinator,
  limits: ArtifactPageCursorLimitsV1,
) -> Result<MemoryReservation, NativeSelectedArtifactCursorErrorV1> {
  let workspace_bytes = u64::try_from(limits.maximum_input_bytes())
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_workspace_bytes", error.to_string()))?;
  memory
    .reserve(MemoryOwner::Query, workspace_bytes, AdmissionClass::Workload)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_workspace_memory", error.to_string()))
}

fn select_native_nvt_predecessor_page_id_v1(
  request: &NativeSelectedPostingSeekLoadRequestV1<'_>,
  closure: &SelectedDirectoryClosureV1,
  descriptor: &IndexCoverageNvtDescriptorV1,
) -> Result<Option<u64>, NativeSelectedArtifactCursorErrorV1> {
  let field = pinned_field_index_from_closure(request, closure)?;
  let nvt_bytes = load_required_manifest(
    request.publisher,
    request.memory,
    request.captured,
    descriptor.manifest_hash(),
    "selected FieldNvt hint manifest",
    request.cancellation,
  )?;
  let nvt_manifest = decode_index_manifest(&nvt_bytes.bytes, request.captured.header.hash_algorithm)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_manifest", error.to_string()))?;
  require_manifest_capabilities(&nvt_manifest, request.supported_reader_capabilities)?;
  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&nvt_bytes.bytes)) else {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_nvt_basis",
      "selected FieldNvt does not match the exact selected Posting generation",
    ));
  };
  validate_native_nvt_descriptor(descriptor, &basis)?;
  let Some(root_key) = basis.tile_directory_root else {
    return Ok(None);
  };
  let target_cell = coordinate_cell(request.target_coordinate, basis.resolution)
    .ok_or_else(|| NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_resolution", "selected FieldNvt resolution is zero"))?;
  let tile_cells = u64::from(basis.tile_cells);
  let target_tile_start = target_cell / tile_cells * tile_cells;
  let mut source = CapturedNativeArtifactCursorSourceV1 {
    publisher: request.publisher,
    captured: request.captured,
    cancellation: request.cancellation,
    memory: request.memory,
    reservations: Vec::new(),
  };
  let validator = NvtTileLeafValidatorV1 { basis: &basis };
  let mut lookup_cell = target_tile_start;
  for attempt in 0..2 {
    let lookup_key = lookup_cell.to_le_bytes();
    let Some(cursor) = load_native_nvt_tile_cursor_v1(request, root_key, &validator, &mut source, &lookup_key)? else {
      return Ok(None);
    };
    let tile = decode_nvt_tile(cursor.leaf(), request.captured.header.hash_algorithm)
      .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_tile", error.to_string()))?;
    if tile.tile_start_cell > lookup_cell {
      return Ok(None);
    }
    let relative_cell = if tile.tile_start_cell == target_tile_start {
      u32::try_from(target_cell - tile.tile_start_cell)
        .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_relative_cell", error.to_string()))?
    } else {
      tile.tile_cell_count - 1
    };
    if let Some(page_id) = tile.predecessor_entry(relative_cell).and_then(|entry| entry.predecessor_page_id) {
      return Ok(Some(page_id));
    }
    if attempt == 1 || tile.tile_start_cell == 0 {
      return Ok(None);
    }
    lookup_cell = tile.tile_start_cell - 1;
  }
  Ok(None)
}

fn pinned_field_index_from_closure<'a>(
  request: &'a NativeSelectedPostingSeekLoadRequestV1<'a>,
  closure: &'a SelectedDirectoryClosureV1,
) -> Result<super::index_nvt::PinnedFieldIndexV1<'a>, NativeSelectedArtifactCursorErrorV1> {
  let first_page_id = closure.expected_first_page_id.ok_or_else(|| {
    NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_field_basis", "selected Posting closure has no first PageId")
  })?;
  let last_page_id = closure.expected_last_page_id.ok_or_else(|| {
    NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_field_basis", "selected Posting closure has no last PageId")
  })?;
  let next_page_id = closure.expected_next_page_id.ok_or_else(|| {
    NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_field_basis", "selected Posting closure has no next PageId")
  })?;
  Ok(super::index_nvt::PinnedFieldIndexV1 {
    hash_algorithm: request.captured.header.hash_algorithm,
    manifest_key: copy_bytes(&request.selected_generation.manifest_hash, "selected NVT FieldIndex manifest")?,
    owner_id: &closure.owner_id,
    generation: closure.generation,
    source_head_hash: &request.selected_generation.source_namespace_root,
    posting_directory_root: Some(&closure.root_key),
    first_page_id,
    last_page_id,
    next_page_id,
    posting_page_count: closure.expected_page_count,
    live_posting_count: closure.expected_live_count,
    posting_tombstone_count: closure.expected_tombstone_count,
    live_canonical_posting_bytes: closure.expected_logical_bytes.ok_or_else(|| {
      NativeSelectedArtifactCursorErrorV1::corrupt("selected_nvt_field_basis", "selected Posting closure has no logical-byte summary")
    })?,
  })
}

fn load_native_nvt_tile_cursor_v1(
  request: &NativeSelectedPostingSeekLoadRequestV1<'_>,
  root_key: &[u8],
  validator: &NvtTileLeafValidatorV1<'_>,
  source: &mut CapturedNativeArtifactCursorSourceV1<'_>,
  lookup_key: &[u8],
) -> Result<Option<LoadedArtifactLeafCursorV1>, NativeSelectedArtifactCursorErrorV1> {
  load_artifact_leaf_cursor_v1(
    &ArtifactPageCursorRequestV1 {
      root: ArtifactPageCursorRootV1 {
        hash_algorithm: request.captured.header.hash_algorithm,
        root_key,
        owner_id: request.selected_generation.owner_id.as_slice(),
        role: OrderedIndexRoleV1::NvtTile,
        maximum_generation: validator.basis.generation,
        expected_summary: None,
      },
      seek: ArtifactPageSeekV1::OrderPredecessor(lookup_key),
      neighbors: ArtifactPageNeighborModeV1::None,
      limits: request.limits,
    },
    source,
    validator,
    &|| request.cancellation.is_cancelled(),
  )
  .map_err(map_cursor_error)
}

fn validate_native_nvt_descriptor(
  descriptor: &IndexCoverageNvtDescriptorV1,
  basis: &PinnedFieldNvtV1<'_>,
) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  if descriptor.manifest_hash() != basis.manifest_key
    || descriptor.generation() != basis.generation
    || descriptor.resolution() != basis.resolution
    || descriptor.tile_cells() != basis.tile_cells
    || descriptor.tile_directory_root() != basis.tile_directory_root
  {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_nvt_descriptor",
      "coverage-registry NVT descriptor disagrees with its captured immutable manifest",
    ));
  }
  Ok(())
}

fn map_nvt_fallback_error(
  error: NativeSelectedArtifactCursorErrorV1,
) -> Result<NativeSelectedNvtFallbackV1, NativeSelectedArtifactCursorErrorV1> {
  let reason = match error.class() {
    NativeSelectedArtifactCursorErrorClassV1::ResourceLimit => NativeSelectedNvtFallbackReasonV1::ResourceLimit,
    NativeSelectedArtifactCursorErrorClassV1::Unavailable => NativeSelectedNvtFallbackReasonV1::Unavailable,
    NativeSelectedArtifactCursorErrorClassV1::InvalidRequest | NativeSelectedArtifactCursorErrorClassV1::Corrupt => {
      NativeSelectedNvtFallbackReasonV1::Corrupt
    }
    NativeSelectedArtifactCursorErrorClassV1::Cancelled => return Err(error),
  };
  Ok(nvt_fallback(reason, Some(error.code())))
}

const fn nvt_fallback(reason: NativeSelectedNvtFallbackReasonV1, diagnostic_code: Option<&'static str>) -> NativeSelectedNvtFallbackV1 {
  NativeSelectedNvtFallbackV1 { reason, diagnostic_code }
}

fn load_selected_directory_closure(
  publisher: &V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  captured: &SelectedDatabaseHeaderV4,
  supported_reader_capabilities: CapabilitySetV1,
  selected_generation: &QueryPlanningCoverageGenerationV1,
  role: OrderedIndexRoleV1,
  cancellation: &CancellationToken,
) -> Result<Option<SelectedDirectoryClosureV1>, NativeSelectedArtifactCursorErrorV1> {
  let selected_bytes =
    load_required_manifest(publisher, memory, captured, &selected_generation.manifest_hash, "selected generation manifest", cancellation)?;
  let selected = decode_index_manifest(&selected_bytes.bytes, captured.header.hash_algorithm)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_manifest", error.to_string()))?;
  validate_selected_manifest_identity(&selected, selected_generation)?;
  require_manifest_capabilities(&selected, supported_reader_capabilities)?;
  match (&selected.details, role) {
    (IndexManifestBodyV1::ScopeCatalog(body), OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ScopeReverse) => {
      let definition_fingerprint = scope_definition_fingerprint(captured.header.hash_algorithm, body.scope_definition);
      let dependency_fingerprint = scope_dependency_fingerprint(captured.header.hash_algorithm);
      validate_selected_fingerprints(selected_generation, &definition_fingerprint, &dependency_fingerprint)?;
      let (root_key, tombstone_count, page_count) = if role == OrderedIndexRoleV1::ScopeOrdinal {
        (body.ordinal_directory_root, body.retained_tombstone_count, body.ordinal_page_count)
      } else {
        (body.reverse_directory_root, 0, body.reverse_page_count)
      };
      root_key
        .map(|root_key| {
          Ok(SelectedDirectoryClosureV1 {
            root_key: copy_bytes(root_key, "scope directory root")?,
            owner_id: copy_bytes(selected.owner_id, "scope manifest owner")?,
            generation: selected.generation,
            expected_live_count: body.live_document_count,
            expected_tombstone_count: tombstone_count,
            expected_page_count: page_count,
            expected_logical_bytes: None,
            expected_first_page_id: None,
            expected_last_page_id: None,
            expected_next_page_id: None,
          })
        })
        .transpose()
    }
    (IndexManifestBodyV1::FieldIndex(field_body), OrderedIndexRoleV1::Posting | OrderedIndexRoleV1::IndexDocumentState) => {
      let value_bytes = load_required_manifest(
        publisher,
        memory,
        captured,
        field_body.value_store_manifest,
        "selected ValueStore dependency",
        cancellation,
      )?;
      let value = decode_index_manifest(&value_bytes.bytes, captured.header.hash_algorithm)
        .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_value_manifest", error.to_string()))?;
      require_manifest_capabilities(&value, supported_reader_capabilities)?;
      let IndexManifestBodyV1::ValueStore(value_body) = &value.details else {
        return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
          "selected_artifact_value_manifest_kind",
          "selected FieldIndex dependency is not a ValueStore manifest",
        ));
      };
      let scope_bytes = load_required_manifest(
        publisher,
        memory,
        captured,
        value_body.scope_catalog_manifest,
        "selected ScopeCatalog dependency",
        cancellation,
      )?;
      let scope = decode_index_manifest(&scope_bytes.bytes, captured.header.hash_algorithm)
        .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_scope_manifest", error.to_string()))?;
      require_manifest_capabilities(&scope, supported_reader_capabilities)?;
      validate_correctness_manifest_chain(&scope, &value, &selected, captured.header.hash_algorithm)
        .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_manifest_chain", error.to_string()))?;
      let definition_fingerprint = field_definition_fingerprint(captured.header.hash_algorithm, field_body.field_index_definition);
      let dependency_fingerprint = field_dependency_fingerprint(captured.header.hash_algorithm, scope.owner_id, value.owner_id);
      validate_selected_fingerprints(selected_generation, &definition_fingerprint, &dependency_fingerprint)?;
      let (root_key, live_count, tombstone_count, page_count, logical_bytes, first_page_id, last_page_id, next_page_id) =
        if role == OrderedIndexRoleV1::Posting {
          (
            field_body.posting_directory_root,
            field_body.live_posting_count,
            field_body.posting_tombstone_count,
            field_body.posting_page_count,
            Some(field_body.live_canonical_posting_bytes),
            Some(field_body.first_page_id),
            Some(field_body.last_page_id),
            Some(field_body.next_page_id),
          )
        } else {
          (
            field_body.document_state_directory_root,
            field_body.unindexable_document_count,
            field_body.state_tombstone_count,
            field_body.state_page_count,
            None,
            None,
            None,
            None,
          )
        };
      root_key
        .map(|root_key| {
          Ok(SelectedDirectoryClosureV1 {
            root_key: copy_bytes(root_key, "field directory root")?,
            owner_id: copy_bytes(selected.owner_id, "field manifest owner")?,
            generation: selected.generation,
            expected_live_count: live_count,
            expected_tombstone_count: tombstone_count,
            expected_page_count: page_count,
            expected_logical_bytes: logical_bytes,
            expected_first_page_id: first_page_id,
            expected_last_page_id: last_page_id,
            expected_next_page_id: next_page_id,
          })
        })
        .transpose()
    }
    _ => Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_artifact_manifest_role",
      "selected generation manifest does not own the requested ordered role",
    )),
  }
}

fn validate_selected_generation_envelope(
  hash_algorithm: HashAlgorithm,
  selected_root: &[u8],
  selected_generation: &QueryPlanningCoverageGenerationV1,
) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  if selected_root.len() != hash_width
    || selected_root.iter().all(|byte| *byte == 0)
    || selected_generation.source_namespace_root.len() != hash_width
    || selected_generation.source_namespace_root.iter().all(|byte| *byte == 0)
    || selected_generation.generation == 0
    || selected_generation.owner_id.len() != hash_width
    || selected_generation.owner_id.iter().all(|byte| *byte == 0)
    || selected_generation.manifest_hash.len() != hash_width
    || selected_generation.manifest_hash.iter().all(|byte| *byte == 0)
    || selected_generation.coverage_epoch_id.iter().all(|byte| *byte == 0)
    || selected_generation.coverage_publication_sequence == 0
    || selected_generation.definition_fingerprint.len() != hash_width
    || selected_generation.dependency_fingerprint.len() != hash_width
  {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_artifact_generation_identity",
      "selected generation does not bind one complete selected-root database identity",
    ));
  }
  Ok(())
}

fn validate_selected_manifest_identity(
  manifest: &IndexManifestV1<'_>,
  selected_generation: &QueryPlanningCoverageGenerationV1,
) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  let Some(coverage) = manifest.details.coverage() else {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_artifact_manifest_coverage",
      "selected generation manifest has no correctness coverage version",
    ));
  };
  if manifest.key != selected_generation.manifest_hash
    || manifest.owner_id != selected_generation.owner_id
    || manifest.generation != selected_generation.generation
    || coverage.source_namespace_root != selected_generation.source_namespace_root
    || coverage.coverage_epoch_id != selected_generation.coverage_epoch_id
    || coverage.coverage_publication_sequence != selected_generation.coverage_publication_sequence
  {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_artifact_manifest_identity",
      "selected generation disagrees with its captured immutable manifest",
    ));
  }
  Ok(())
}

fn validate_selected_fingerprints(
  selected_generation: &QueryPlanningCoverageGenerationV1,
  definition_fingerprint: &[u8],
  dependency_fingerprint: &[u8],
) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  if selected_generation.definition_fingerprint != definition_fingerprint
    || selected_generation.dependency_fingerprint != dependency_fingerprint
  {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_artifact_generation_fingerprint",
      "selected generation definition or dependency fingerprint disagrees with its captured manifest chain",
    ));
  }
  Ok(())
}

fn require_manifest_capabilities(
  manifest: &IndexManifestV1<'_>,
  supported_reader_capabilities: CapabilitySetV1,
) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  let required = match &manifest.details {
    IndexManifestBodyV1::ScopeCatalog(body) => body.required_reader_capabilities,
    IndexManifestBodyV1::ValueStore(body) => body.required_reader_capabilities,
    IndexManifestBodyV1::FieldIndex(body) => body.required_reader_capabilities,
    IndexManifestBodyV1::FieldNvt(body) => body.required_reader_capabilities,
  };
  let required = CapabilitySetV1::from_bytes(required)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_required_capabilities", error.to_string()))?;
  if !required.difference(supported_reader_capabilities).is_empty() {
    return Err(NativeSelectedArtifactCursorErrorV1::unavailable(
      "selected_artifact_reader_capabilities",
      "selected manifest requires unsupported reader capabilities",
    ));
  }
  Ok(())
}

fn validate_manifest_root_closure(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  closure: &SelectedDirectoryClosureV1,
  cursor: &LoadedArtifactPageCursorV1,
) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  let root = cursor.directory(0).ok_or_else(|| {
    NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_root_path", "artifact cursor returned no root directory")
  })?;
  let root = decode_artifact_directory(root, hash_algorithm)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_root_decode", error.to_string()))?;
  if root.key != closure.root_key
    || root.owner_id != closure.owner_id
    || root.role != role
    || root.generation > closure.generation
    || root.live_count != closure.expected_live_count
    || root.tombstone_count != closure.expected_tombstone_count
    || root.page_count != closure.expected_page_count
    || closure.expected_logical_bytes.is_some_and(|expected| root.logical_bytes != expected)
  {
    return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
      "selected_artifact_root_manifest_closure",
      "selected ArtifactDirectory root disagrees with its captured manifest summary",
    ));
  }
  if role == OrderedIndexRoleV1::Posting {
    let page = decode_ordered_page(cursor.page(), hash_algorithm)
      .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_page_decode", error.to_string()))?;
    if cursor.page_ordinal() == 0 && Some(page.page_id) != closure.expected_first_page_id
      || cursor.page_ordinal().checked_add(1) == Some(cursor.root_page_count()) && Some(page.page_id) != closure.expected_last_page_id
    {
      return Err(NativeSelectedArtifactCursorErrorV1::corrupt(
        "selected_artifact_page_endpoint",
        "selected Posting page endpoint disagrees with its captured FieldIndex manifest",
      ));
    }
  }
  Ok(())
}

fn load_required_manifest(
  publisher: &V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  captured: &SelectedDatabaseHeaderV4,
  key: &[u8],
  context: &'static str,
  cancellation: &CancellationToken,
) -> Result<AccountedArtifactBytesV1, NativeSelectedArtifactCursorErrorV1> {
  debug_assert_eq!(ImmutableIndexArtifactKindV1::FieldIndexManifest.maximum_encoded_length(), MANIFEST_READ_MAXIMUM_BYTES);
  load_accounted_artifact(publisher, captured, key, MANIFEST_READ_MAXIMUM_BYTES, cancellation, memory)?
    .ok_or_else(|| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_manifest_missing", format!("{context} is missing")))
}

fn load_accounted_artifact(
  publisher: &V4FirstAuthorityPublisher,
  captured: &SelectedDatabaseHeaderV4,
  key: &[u8],
  maximum_value_length: usize,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<Option<AccountedArtifactBytesV1>, NativeSelectedArtifactCursorErrorV1> {
  require_not_cancelled(cancellation)?;
  let maximum_entity_length = checked_whole_entity_encoded_length(captured.header.hash_algorithm, key.len(), maximum_value_length)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_read_bound", error.to_string()))?;
  let maximum_entity_length = u64::try_from(maximum_entity_length)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_read_bound", error.to_string()))?;
  let mut reservation = memory
    .reserve(MemoryOwner::Query, maximum_entity_length, AdmissionClass::Workload)
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_read_memory", error.to_string()))?;
  let loaded = publisher
    .load_index_artifact_at_captured_header(captured, key, maximum_value_length, cancellation)
    .map_err(map_first_authority_error)?;
  let Some(bytes) = loaded else {
    return Ok(None);
  };
  let retained = u64::try_from(bytes.capacity())
    .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_retained_bytes", error.to_string()))?;
  if retained > reservation.bytes() {
    reservation
      .grow(retained - reservation.bytes())
      .map_err(|error| NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_retained_memory", error.to_string()))?;
  } else {
    reservation
      .shrink(reservation.bytes() - retained)
      .map_err(|error| NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_memory_accounting", error.to_string()))?;
  }
  require_not_cancelled(cancellation)?;
  Ok(Some(AccountedArtifactBytesV1 { bytes, _memory: reservation }))
}

fn map_first_authority_error(error: FirstAuthorityPublicationErrorV1) -> NativeSelectedArtifactCursorErrorV1 {
  match error {
    FirstAuthorityPublicationErrorV1::Invalid { code: "captured_authority_cancelled", .. } => {
      NativeSelectedArtifactCursorErrorV1::cancelled()
    }
    FirstAuthorityPublicationErrorV1::Invalid {
      code: "immutable_index_value_exceeds_cap" | "immutable_index_read_allocation",
      message,
    } => NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_source_pressure", message),
    FirstAuthorityPublicationErrorV1::Invalid { code, message } => NativeSelectedArtifactCursorErrorV1::corrupt(code, message),
    FirstAuthorityPublicationErrorV1::Format(error) => NativeSelectedArtifactCursorErrorV1::corrupt(error.code(), error.to_string()),
    FirstAuthorityPublicationErrorV1::Engine(error) => {
      NativeSelectedArtifactCursorErrorV1::unavailable("selected_artifact_storage", error.to_string())
    }
    FirstAuthorityPublicationErrorV1::Header(error) => NativeSelectedArtifactCursorErrorV1::unavailable(error.code(), error.to_string()),
    FirstAuthorityPublicationErrorV1::StateLockPoisoned => {
      NativeSelectedArtifactCursorErrorV1::unavailable("selected_artifact_authority_lock", "first-authority state lock is poisoned")
    }
    FirstAuthorityPublicationErrorV1::Committed { code, message, .. } => NativeSelectedArtifactCursorErrorV1::unavailable(code, message),
  }
}

fn map_native_source_error(error: NativeSelectedArtifactCursorErrorV1) -> ArtifactCursorReadErrorV1 {
  match error.class() {
    NativeSelectedArtifactCursorErrorClassV1::InvalidRequest | NativeSelectedArtifactCursorErrorClassV1::Corrupt => {
      ArtifactCursorReadErrorV1::Corrupt(error.to_string())
    }
    NativeSelectedArtifactCursorErrorClassV1::ResourceLimit => ArtifactCursorReadErrorV1::ResourcePressure(error.to_string()),
    NativeSelectedArtifactCursorErrorClassV1::Unavailable => ArtifactCursorReadErrorV1::Operational(error.to_string()),
    NativeSelectedArtifactCursorErrorClassV1::Cancelled => ArtifactCursorReadErrorV1::Cancelled,
  }
}

fn map_cursor_error(error: ArtifactPageCursorErrorV1) -> NativeSelectedArtifactCursorErrorV1 {
  match error {
    ArtifactPageCursorErrorV1::Cancelled => NativeSelectedArtifactCursorErrorV1::cancelled(),
    ArtifactPageCursorErrorV1::SourcePressure(context) | ArtifactPageCursorErrorV1::Allocation(context) => {
      NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_cursor_pressure", context)
    }
    ArtifactPageCursorErrorV1::SourceOperational(context) => {
      NativeSelectedArtifactCursorErrorV1::unavailable("selected_artifact_cursor_source", context)
    }
    ArtifactPageCursorErrorV1::InvalidLimits(context) => {
      NativeSelectedArtifactCursorErrorV1::invalid("selected_artifact_cursor_limits", context)
    }
    ArtifactPageCursorErrorV1::MissingArtifact { key } => {
      NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_cursor_missing", format!("immutable artifact {key} is missing"))
    }
    ArtifactPageCursorErrorV1::SourceCorrupt(context) => {
      NativeSelectedArtifactCursorErrorV1::corrupt("selected_artifact_cursor_source", context)
    }
    ArtifactPageCursorErrorV1::Malformed(error) => NativeSelectedArtifactCursorErrorV1::corrupt(error.code(), error.to_string()),
  }
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), NativeSelectedArtifactCursorErrorV1> {
  if cancellation.is_cancelled() {
    Err(NativeSelectedArtifactCursorErrorV1::cancelled())
  } else {
    Ok(())
  }
}

fn copy_bytes(value: &[u8], context: &'static str) -> Result<Vec<u8>, NativeSelectedArtifactCursorErrorV1> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(value.len()).map_err(|error| {
    NativeSelectedArtifactCursorErrorV1::resource("selected_artifact_receipt_allocation", format!("{context}: {error}"))
  })?;
  copy.extend_from_slice(value);
  Ok(copy)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn optional_nvt_failure_mapping_preserves_unavailability_and_cancellation() {
    let unavailable =
      map_nvt_fallback_error(NativeSelectedArtifactCursorErrorV1::unavailable("selected_nvt_test_source", "test-only NVT source outage"))
        .unwrap();
    assert_eq!(unavailable.reason(), NativeSelectedNvtFallbackReasonV1::Unavailable);
    assert_eq!(unavailable.diagnostic_code(), Some("selected_nvt_test_source"));

    let cancelled = map_nvt_fallback_error(NativeSelectedArtifactCursorErrorV1::cancelled()).unwrap_err();
    assert_eq!(cancelled.class(), NativeSelectedArtifactCursorErrorClassV1::Cancelled);
  }
}
