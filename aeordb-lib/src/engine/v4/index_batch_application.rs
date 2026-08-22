//! Bounded sparse reads and publication-ready immutable application of frozen index batches.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;

use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, IndexManifestV1, IndexManifestWriteV1, decode_immutable_index_artifact,
  decode_index_manifest, encode_index_manifest,
};
use super::index_coordinator::{
  FrozenIndexBatchV1, IndexMembershipOwnerClassV1, IndexMembershipStateV1, IndexMutationOperationV1, PublishedIndexMembershipTransitionV1,
  PublishedIndexMutationV1,
};
use super::index_copy_on_write::{
  ArtifactDirectoryMutationRequestV1, ArtifactDirectoryPathV1, INDEX_COPY_ON_WRITE_MAXIMUM_COMPOSITE_STEPS_V1,
  IndexCopyOnWriteBootstrapRequestV1, IndexCopyOnWriteClosureRequestV1, IndexCopyOnWriteClosureSummaryV1,
  IndexCopyOnWriteCompositeClosureRequestV1, IndexCopyOnWriteCompositeStepV1, IndexDirectoryLayoutV1, IndexMutationCommitmentV1,
  IndexPageLayoutV1, OrderedPageBatchMutationRequestV1, OrderedPageCompactionWindowRequestV1, OrderedPageMutationKindV1,
  OrderedPageMutationPlanV1, TombstoneDropProofV1, bootstrap_ordered_index_v1, bind_logical_page_endpoints_v1,
  compact_ordered_page_window_v1, mutate_ordered_page_batch_for_publication_v1, rewrite_artifact_directory_paths_v1,
  validate_index_application_layouts_v1, validate_index_copy_on_write_closure_v1, validate_index_copy_on_write_composite_closure_v1,
};
use super::index_generation_publication::{INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1};
use super::index_manifest::{
  CoverageVersionV1, FieldIndexManifestBodyV1, IndexManifestBodyV1, ScopeCatalogManifestBodyV1, ValueStoreManifestBodyV1,
};
use super::index_page::{
  ArtifactDirectoryEntryV1, ArtifactDirectoryNodeV1, OrderedIndexRoleV1, OrderedPageV1, compare_order_keys, decode_artifact_directory,
  decode_ordered_page, decode_ordered_record, ordered_record_order_key, validate_posting_page_link,
};
use super::reader::{FormatError, MalformedInputClass};

pub const INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1: usize = 16;
pub const INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1: usize = 32 * 1_024 * 1_024;
pub const INDEX_BATCH_MAXIMUM_TRANSITIONS_V1: usize = 16 * 1_024 * 1_024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IndexBatchArtifactReadErrorV1 {
  #[error("immutable index artifact is missing")]
  Missing,
  #[error("immutable index artifact read was cancelled")]
  Cancelled,
  #[error("immutable index artifact read exceeded resource limits: {0}")]
  ResourcePressure(String),
  #[error("immutable index artifact read failed: {0}")]
  Operational(String),
}

pub trait IndexBatchArtifactSourceV1 {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<Vec<u8>, IndexBatchArtifactReadErrorV1>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexBatchApplicationErrorV1 {
  #[error("index batch operation was cancelled")]
  Cancelled,
  #[error("immutable index artifact {key} is missing")]
  MissingArtifact { key: String },
  #[error("immutable index artifact source exceeded resource limits: {0}")]
  SourcePressure(String),
  #[error("immutable index artifact source failed: {0}")]
  SourceOperational(String),
  #[error("malformed immutable index state: {0}")]
  Malformed(FormatError),
  #[error("invalid index batch limits: {0}")]
  InvalidLimits(String),
  #[error("index batch successor overlay exceeds its artifact-count limit")]
  OverlayCount,
  #[error("index batch successor overlay exceeds its retained-byte limit")]
  OverlayBytes,
  #[error("index batch successor overlay contains a conflicting immutable key")]
  OverlayConflict,
  #[error("index batch allocation failed: {0}")]
  Allocation(String),
}

impl IndexBatchApplicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Cancelled => "index_batch_cancelled",
      Self::MissingArtifact { .. } => "index_batch_artifact_missing",
      Self::SourcePressure(_) => "index_batch_source_pressure",
      Self::SourceOperational(_) => "index_batch_source_operational",
      Self::Malformed(error) => error.code(),
      Self::InvalidLimits(_) => "index_batch_invalid_limits",
      Self::OverlayCount => "index_batch_overlay_count",
      Self::OverlayBytes => "index_batch_overlay_bytes",
      Self::OverlayConflict => "index_batch_overlay_conflict",
      Self::Allocation(_) => "index_batch_allocation",
    }
  }
}

impl From<FormatError> for IndexBatchApplicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Malformed(source)
  }
}

#[derive(Debug, Clone)]
pub struct IndexManifestSuccessorRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub source_manifest: &'a [u8],
  pub generation: u64,
  pub parent_manifest_key: Option<&'a [u8]>,
  pub coverage: CoverageVersionV1<'a>,
  pub next_document_ordinal: Option<u64>,
  pub mutations: &'a [PublishedIndexMutationV1],
  pub transitions: &'a [PublishedIndexMembershipTransitionV1],
  pub role_summaries: &'a [IndexCopyOnWriteClosureSummaryV1],
}

#[derive(Debug, Clone, Copy)]
pub struct FrozenIndexOwnerSourceV1<'a> {
  pub source_manifest: &'a [u8],
  pub next_document_ordinal: Option<u64>,
}

#[derive(Clone)]
pub struct FrozenIndexBatchApplicationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub coverage: CoverageVersionV1<'a>,
  pub batch: &'a FrozenIndexBatchV1,
  pub owner_sources: &'a [FrozenIndexOwnerSourceV1<'a>],
  pub overlay_limits: IndexBatchArtifactOverlayLimitsV1,
  pub path_limits: OrderedPagePathLookupLimitsV1,
  pub page_layout: IndexPageLayoutV1,
  pub directory_layout: IndexDirectoryLayoutV1,
}

#[derive(Clone, Copy)]
pub struct IndexArtifactCompactionApplicationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub attempt_id: u64,
  pub generation: u64,
  pub source_manifest: &'a [u8],
  pub dependent_field_manifests: &'a [&'a [u8]],
  pub role: OrderedIndexRoleV1,
  pub source_pages: &'a [&'a [u8]],
  pub previous_posting_page: Option<&'a [u8]>,
  pub next_posting_page: Option<&'a [u8]>,
  pub paths: &'a [ArtifactDirectoryPathV1<'a>],
  pub tombstone_drop_proof: Option<&'a TombstoneDropProofV1<'a>>,
  pub overlay_limits: IndexBatchArtifactOverlayLimitsV1,
  pub page_layout: IndexPageLayoutV1,
  pub directory_layout: IndexDirectoryLayoutV1,
}

#[derive(Debug)]
pub enum FrozenIndexCompactionApplicationOutcomeV1 {
  Unchanged,
  Publication(FrozenIndexBatchApplicationPlanV1),
}

#[derive(Debug)]
pub struct FrozenIndexOwnerApplicationPlanV1 {
  owner_id: Vec<u8>,
  owner_class: IndexMembershipOwnerClassV1,
  source_manifest_key: Vec<u8>,
  source_generation: u64,
  successor_manifest: EncodedImmutableIndexArtifactV1,
  dependency_range: Range<usize>,
  role_summaries: Vec<IndexCopyOnWriteClosureSummaryV1>,
}

impl FrozenIndexOwnerApplicationPlanV1 {
  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }

  pub const fn owner_class(&self) -> IndexMembershipOwnerClassV1 {
    self.owner_class
  }

  pub fn source_manifest_key(&self) -> &[u8] {
    &self.source_manifest_key
  }

  pub const fn source_generation(&self) -> u64 {
    self.source_generation
  }

  pub fn successor_manifest(&self) -> &EncodedImmutableIndexArtifactV1 {
    &self.successor_manifest
  }

  pub fn dependency_range(&self) -> Range<usize> {
    self.dependency_range.clone()
  }

  pub fn role_summaries(&self) -> &[IndexCopyOnWriteClosureSummaryV1] {
    &self.role_summaries
  }
}

#[derive(Debug)]
pub struct FrozenIndexBatchApplicationPlanV1 {
  coordinator_id: [u8; 16],
  batch_id: u64,
  attempt_id: u64,
  generation: u64,
  overlay: SparseIndexArtifactOverlayV1,
  owner_plans: Vec<FrozenIndexOwnerApplicationPlanV1>,
}

impl FrozenIndexBatchApplicationPlanV1 {
  pub const fn coordinator_id(&self) -> [u8; 16] {
    self.coordinator_id
  }

  pub const fn batch_id(&self) -> u64 {
    self.batch_id
  }

  pub const fn attempt_id(&self) -> u64 {
    self.attempt_id
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub fn prepared_artifacts(&self) -> impl ExactSizeIterator<Item = &EncodedImmutableIndexArtifactV1> {
    self.overlay.prepared_artifacts()
  }

  pub fn owner_plans(&self) -> &[FrozenIndexOwnerApplicationPlanV1] {
    &self.owner_plans
  }
}

struct PendingFrozenOwnerApplicationV1<'a> {
  source_manifest: &'a [u8],
  source_manifest_key: Vec<u8>,
  owner_id: Vec<u8>,
  owner_class: IndexMembershipOwnerClassV1,
  source_generation: u64,
  next_document_ordinal: Option<u64>,
  mutation_range: Range<usize>,
  transition_range: Range<usize>,
  dependency_range: Range<usize>,
  role_summaries: Vec<IndexCopyOnWriteClosureSummaryV1>,
}

pub fn apply_frozen_index_batch_v1(
  request: &FrozenIndexBatchApplicationRequestV1<'_>,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<FrozenIndexBatchApplicationPlanV1, IndexBatchApplicationErrorV1> {
  validate_frozen_application_request(request)?;
  check_cancelled(is_cancelled)?;
  let mut overlay = SparseIndexArtifactOverlayV1::new(request.hash_algorithm, request.overlay_limits)?;
  let mut pending = Vec::new();
  pending
    .try_reserve_exact(request.owner_sources.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("owner application reservation failed: {error}")))?;
  let mut mutation_index = 0usize;
  let mut transition_index = 0usize;
  let mut previous_owner_id: Option<&[u8]> = None;
  for owner_source in request.owner_sources {
    check_cancelled(is_cancelled)?;
    let manifest = decode_index_manifest(owner_source.source_manifest, request.hash_algorithm)?;
    if previous_owner_id.is_some_and(|owner_id| owner_id >= manifest.owner_id) {
      return Err(manifest_closure_error("frozen owner sources are not in strict canonical owner order"));
    }
    if request.batch.records().get(mutation_index).is_some_and(|mutation| mutation.index_id() < manifest.owner_id)
      || request.batch.transitions().get(transition_index).is_some_and(|transition| transition.owner_id() < manifest.owner_id)
    {
      return Err(manifest_closure_error("frozen batch has an owner without a source manifest"));
    }
    let mutation_start = mutation_index;
    while request.batch.records().get(mutation_index).is_some_and(|mutation| mutation.index_id() == manifest.owner_id) {
      mutation_index += 1;
    }
    let transition_start = transition_index;
    while request.batch.transitions().get(transition_index).is_some_and(|transition| transition.owner_id() == manifest.owner_id) {
      transition_index += 1;
    }
    if mutation_start == mutation_index && transition_start == transition_index {
      return Err(manifest_closure_error("frozen owner source has no mutation or membership transition"));
    }
    if request.generation <= manifest.generation {
      return Err(manifest_closure_error("frozen application generation is not newer than every source manifest"));
    }
    let owner_class = manifest_owner_class(&manifest.details)?;
    let mutations = &request.batch.records()[mutation_start..mutation_index];
    let transitions = &request.batch.transitions()[transition_start..transition_index];
    let validation_request = IndexManifestSuccessorRequestV1 {
      hash_algorithm: request.hash_algorithm,
      source_manifest: owner_source.source_manifest,
      generation: request.generation,
      parent_manifest_key: source_parent_manifest_key(&manifest.details),
      coverage: request.coverage.clone(),
      next_document_ordinal: owner_source.next_document_ordinal,
      mutations,
      transitions,
      role_summaries: &[],
    };
    let transition_deltas = validate_manifest_transitions(&validation_request, manifest.owner_id, owner_class, is_cancelled)?;
    let mutation_closure = validate_manifest_mutations(&validation_request, manifest.owner_id, owner_class, is_cancelled)?;
    if transition_deltas.required_roles & !mutation_closure.role_mask != 0 {
      return Err(manifest_closure_error("membership-changing transitions are missing their required ordered-record mutations"));
    }
    let dependency_start = overlay.artifact_count();
    let role_summaries = apply_owner_roles(request, &manifest, mutations, &mut overlay, source, is_cancelled)?;
    let dependency_end = overlay.artifact_count();
    pending.push(PendingFrozenOwnerApplicationV1 {
      source_manifest: owner_source.source_manifest,
      source_manifest_key: manifest.key,
      owner_id: clone_bytes(manifest.owner_id, "frozen owner identity")?,
      owner_class,
      source_generation: manifest.generation,
      next_document_ordinal: owner_source.next_document_ordinal,
      mutation_range: mutation_start..mutation_index,
      transition_range: transition_start..transition_index,
      dependency_range: dependency_start..dependency_end,
      role_summaries,
    });
    previous_owner_id = Some(manifest.owner_id);
  }
  if mutation_index != request.batch.records().len() || transition_index != request.batch.transitions().len() {
    return Err(manifest_closure_error("frozen batch has an owner without a source manifest"));
  }

  let owner_plans = synthesize_frozen_owner_plans(request, pending, is_cancelled)?;
  Ok(FrozenIndexBatchApplicationPlanV1 {
    coordinator_id: request.batch.coordinator_id(),
    batch_id: request.batch.batch_id(),
    attempt_id: request.batch.attempt_id(),
    generation: request.generation,
    overlay,
    owner_plans,
  })
}

pub fn apply_index_artifact_compaction_v1(
  request: &IndexArtifactCompactionApplicationRequestV1<'_>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<FrozenIndexCompactionApplicationOutcomeV1, IndexBatchApplicationErrorV1> {
  validate_compaction_application_request(request)?;
  check_cancelled(is_cancelled)?;
  let source = decode_index_manifest(request.source_manifest, request.hash_algorithm)?;
  let owner_class = manifest_owner_class(&source.details)?;
  if request.generation <= source.generation || source_role_root(&source.details, request.role)?.is_none() {
    return Err(manifest_closure_error("compaction generation must advance a source manifest that owns the selected ordered role"));
  }
  validate_compaction_dependents(request, &source, owner_class, is_cancelled)?;

  let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm: request.hash_algorithm,
    source_pages: request.source_pages,
    previous_posting_page: request.previous_posting_page,
    next_posting_page: request.next_posting_page,
    generation: request.generation,
    next_page_id: source_next_page_id(&source.details),
    tombstone_drop_proof: request.tombstone_drop_proof,
    layout: request.page_layout,
  })?;
  if page_plan.is_unchanged() {
    return Ok(FrozenIndexCompactionApplicationOutcomeV1::Unchanged);
  }
  check_cancelled(is_cancelled)?;

  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    page_plan: &page_plan,
    paths: request.paths,
    layout: request.directory_layout,
  })?;
  let closure_sources = compaction_closure_sources(request, &page_plan)?;
  let mut summary = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    initial_next_page_id: source_next_page_id(&source.details),
    applied_mutations: None,
    source_pages: &closure_sources,
    paths: request.paths,
    page_plan: &page_plan,
    directory_plan: &directory_plan,
    page_layout: request.page_layout,
    directory_layout: request.directory_layout,
  })?;
  if request.role == OrderedIndexRoleV1::Posting {
    let (first_page_id, last_page_id) = compacted_posting_endpoints(request, &source.details, &page_plan, &summary)?;
    if summary.root_key.is_some() {
      bind_logical_page_endpoints_v1(&mut summary, first_page_id, last_page_id)?;
    }
  }
  check_cancelled(is_cancelled)?;

  let mut overlay = SparseIndexArtifactOverlayV1::new(request.hash_algorithm, request.overlay_limits)?;
  for replacement in page_plan.replacements {
    for artifact in replacement.artifacts {
      overlay.insert(artifact)?;
    }
  }
  for artifact in directory_plan.artifacts {
    overlay.insert(artifact)?;
  }
  let dependency_end = overlay.artifact_count();
  let source_parent = source_parent_manifest_key(&source.details);
  let successor_manifest = synthesize_compacted_index_manifest_v1(
    request.hash_algorithm,
    request.source_manifest,
    request.generation,
    source_parent,
    &summary,
    is_cancelled,
  )?;
  let mut owner_plans = Vec::new();
  owner_plans
    .try_reserve_exact(1usize.saturating_add(request.dependent_field_manifests.len()))
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("compaction owner-plan reservation failed: {error}")))?;
  let mut role_summaries = Vec::new();
  role_summaries
    .try_reserve_exact(1)
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("compaction role-summary reservation failed: {error}")))?;
  role_summaries.push(summary);
  owner_plans.push(FrozenIndexOwnerApplicationPlanV1 {
    owner_id: clone_bytes(source.owner_id, "compaction owner identity")?,
    owner_class,
    source_manifest_key: source.key,
    source_generation: source.generation,
    successor_manifest,
    dependency_range: 0..dependency_end,
    role_summaries,
  });
  append_compaction_dependent_fields(request, &mut owner_plans, is_cancelled)?;
  Ok(FrozenIndexCompactionApplicationOutcomeV1::Publication(FrozenIndexBatchApplicationPlanV1 {
    coordinator_id: request.coordinator_id,
    batch_id: request.batch_id,
    attempt_id: request.attempt_id,
    generation: request.generation,
    overlay,
    owner_plans,
  }))
}

fn validate_frozen_application_request(request: &FrozenIndexBatchApplicationRequestV1<'_>) -> Result<(), IndexBatchApplicationErrorV1> {
  validate_successor_coverage(&request.coverage, request.hash_algorithm)?;
  IndexBatchArtifactOverlayLimitsV1::new(request.overlay_limits.maximum_artifacts(), request.overlay_limits.maximum_retained_bytes())?;
  OrderedPagePathLookupLimitsV1::new(request.path_limits.maximum_directory_depth(), request.path_limits.maximum_input_bytes())?;
  validate_index_application_layouts_v1(request.page_layout, request.directory_layout)?;
  if request.generation == 0
    || request.owner_sources.is_empty()
    || request.owner_sources.len() > INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1
    || request.batch.records().is_empty() && request.batch.transitions().is_empty()
  {
    return Err(IndexBatchApplicationErrorV1::InvalidLimits(
      "frozen application generation, owner-source count, or batch contents are invalid".to_string(),
    ));
  }
  Ok(())
}

fn validate_compaction_application_request(
  request: &IndexArtifactCompactionApplicationRequestV1<'_>,
) -> Result<(), IndexBatchApplicationErrorV1> {
  IndexBatchArtifactOverlayLimitsV1::new(request.overlay_limits.maximum_artifacts(), request.overlay_limits.maximum_retained_bytes())?;
  validate_index_application_layouts_v1(request.page_layout, request.directory_layout)?;
  if request.coordinator_id == [0; 16]
    || request.batch_id == 0
    || request.attempt_id == 0
    || request.generation == 0
    || request.source_pages.is_empty()
    || request.source_pages.len() > 2
    || request.paths.is_empty()
    || request.paths.len() > 4
    || request.role == OrderedIndexRoleV1::NvtTile
    || request.dependent_field_manifests.len() >= INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1
  {
    return Err(IndexBatchApplicationErrorV1::InvalidLimits(
      "compaction identity, window, path, role, or dependent-manifest bounds are invalid".to_string(),
    ));
  }
  Ok(())
}

fn validate_compaction_dependents(
  request: &IndexArtifactCompactionApplicationRequestV1<'_>,
  source: &IndexManifestV1<'_>,
  owner_class: IndexMembershipOwnerClassV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if owner_class != IndexMembershipOwnerClassV1::ValueStore {
    if !request.dependent_field_manifests.is_empty() {
      return Err(manifest_closure_error("only ValueStore compaction may carry dependent FieldIndex manifests"));
    }
    return Ok(());
  }
  if request.dependent_field_manifests.is_empty() {
    return Err(manifest_closure_error(
      "ValueStore compaction requires every dependent selected FieldIndex manifest so the successor remains reachable",
    ));
  }
  let source_coverage = manifest_coverage(&source.details)?;
  let mut previous_owner: Option<&[u8]> = None;
  for dependent_bytes in request.dependent_field_manifests {
    check_cancelled(is_cancelled)?;
    let dependent = decode_index_manifest(dependent_bytes, request.hash_algorithm)?;
    let IndexManifestBodyV1::FieldIndex(body) = &dependent.details else {
      return Err(manifest_closure_error("a ValueStore compaction dependent is not a FieldIndex manifest"));
    };
    if body.value_store_manifest != source.key
      || &body.coverage != source_coverage
      || dependent.generation >= request.generation
      || previous_owner.is_some_and(|owner| owner >= dependent.owner_id)
    {
      return Err(manifest_closure_error(
        "ValueStore dependent FieldIndex manifests are stale, foreign, coverage-incompatible, or not in strict owner order",
      ));
    }
    previous_owner = Some(dependent.owner_id);
  }
  Ok(())
}

fn compaction_closure_sources<'a>(
  request: &'a IndexArtifactCompactionApplicationRequestV1<'a>,
  page_plan: &OrderedPageMutationPlanV1,
) -> Result<Vec<&'a [u8]>, IndexBatchApplicationErrorV1> {
  let candidate_count = request
    .source_pages
    .len()
    .checked_add(usize::from(request.previous_posting_page.is_some()))
    .and_then(|count| count.checked_add(usize::from(request.next_posting_page.is_some())))
    .ok_or_else(|| manifest_closure_error("compaction source candidate count overflowed"))?;
  let mut candidates = Vec::new();
  candidates
    .try_reserve_exact(candidate_count)
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("compaction source candidate reservation failed: {error}")))?;
  candidates.extend_from_slice(request.source_pages);
  candidates.extend(request.previous_posting_page);
  candidates.extend(request.next_posting_page);
  let mut sources = Vec::new();
  sources
    .try_reserve_exact(page_plan.replacements.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("compaction closure-source reservation failed: {error}")))?;
  for replacement in &page_plan.replacements {
    let mut selected = None;
    for candidate in &candidates {
      let page = decode_ordered_page(candidate, request.hash_algorithm)?;
      if page.key == replacement.source_key {
        if selected.replace(*candidate).is_some() {
          return Err(manifest_closure_error("compaction source candidates contain a duplicate immutable page"));
        }
      }
    }
    sources.push(selected.ok_or_else(|| manifest_closure_error("compaction replacement has no exact supplied source page"))?);
  }
  Ok(sources)
}

fn compacted_posting_endpoints(
  request: &IndexArtifactCompactionApplicationRequestV1<'_>,
  source: &IndexManifestBodyV1<'_>,
  page_plan: &OrderedPageMutationPlanV1,
  summary: &IndexCopyOnWriteClosureSummaryV1,
) -> Result<(u64, u64), IndexBatchApplicationErrorV1> {
  let IndexManifestBodyV1::FieldIndex(body) = source else {
    return Err(manifest_closure_error("Posting compaction source is not a FieldIndex manifest"));
  };
  if summary.root_key.is_none() {
    return Ok((0, 0));
  }
  let mut primary_keys = Vec::new();
  primary_keys
    .try_reserve_exact(request.source_pages.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("compaction endpoint key reservation failed: {error}")))?;
  for source_page in request.source_pages {
    primary_keys.push(decode_ordered_page(source_page, request.hash_algorithm)?.key);
  }
  let mut retained_window_page_id = None;
  for replacement in &page_plan.replacements {
    if !primary_keys.iter().any(|key| *key == replacement.source_key) {
      continue;
    }
    for artifact in &replacement.artifacts {
      let page = decode_ordered_page(&artifact.value, request.hash_algorithm)?;
      if retained_window_page_id.replace(page.page_id).is_some() {
        return Err(manifest_closure_error("compaction window produced more than one retained endpoint page"));
      }
    }
  }
  let previous_page_id =
    request.previous_posting_page.map(|page| decode_ordered_page(page, request.hash_algorithm).map(|page| page.page_id)).transpose()?;
  let next_page_id =
    request.next_posting_page.map(|page| decode_ordered_page(page, request.hash_algorithm).map(|page| page.page_id)).transpose()?;
  let first_page_id = if page_plan.retired_page_ids.contains(&body.first_page_id) {
    retained_window_page_id.or(next_page_id).unwrap_or(0)
  } else {
    body.first_page_id
  };
  let last_page_id = if page_plan.retired_page_ids.contains(&body.last_page_id) {
    retained_window_page_id.or(previous_page_id).unwrap_or(0)
  } else {
    body.last_page_id
  };
  if first_page_id == 0 || last_page_id == 0 {
    return Err(manifest_closure_error("populated compacted Posting root has no logical first or last PageId"));
  }
  Ok((first_page_id, last_page_id))
}

fn append_compaction_dependent_fields(
  request: &IndexArtifactCompactionApplicationRequestV1<'_>,
  owner_plans: &mut Vec<FrozenIndexOwnerApplicationPlanV1>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if request.dependent_field_manifests.is_empty() {
    return Ok(());
  }
  let value_successor_key = clone_bytes(
    &owner_plans.first().ok_or_else(|| manifest_closure_error("ValueStore compaction has no successor owner plan"))?.successor_manifest.key,
    "compacted ValueStore successor key",
  )?;
  for source_bytes in request.dependent_field_manifests {
    check_cancelled(is_cancelled)?;
    let source = decode_index_manifest(source_bytes, request.hash_algorithm)?;
    let coverage = manifest_coverage(&source.details)?.clone();
    let successor_manifest = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm: request.hash_algorithm,
        source_manifest: source_bytes,
        generation: request.generation,
        parent_manifest_key: Some(&value_successor_key),
        coverage,
        next_document_ordinal: None,
        mutations: &[],
        transitions: &[],
        role_summaries: &[],
      },
      is_cancelled,
    )?;
    owner_plans.push(FrozenIndexOwnerApplicationPlanV1 {
      owner_id: clone_bytes(source.owner_id, "dependent FieldIndex owner identity")?,
      owner_class: IndexMembershipOwnerClassV1::FieldIndex,
      source_manifest_key: source.key,
      source_generation: source.generation,
      successor_manifest,
      dependency_range: 0..0,
      role_summaries: Vec::new(),
    });
  }
  Ok(())
}

fn apply_owner_roles(
  request: &FrozenIndexBatchApplicationRequestV1<'_>,
  source_manifest: &IndexManifestV1<'_>,
  mutations: &[PublishedIndexMutationV1],
  overlay: &mut SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<IndexCopyOnWriteClosureSummaryV1>, IndexBatchApplicationErrorV1> {
  let mut summaries = Vec::new();
  summaries
    .try_reserve_exact(ordered_role_count(&source_manifest.details))
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("role-summary reservation failed: {error}")))?;
  let mut mutation_index = 0usize;
  let mut next_page_id = source_next_page_id(&source_manifest.details);
  while mutation_index < mutations.len() {
    check_cancelled(is_cancelled)?;
    let role = mutations[mutation_index].role();
    let role_start = mutation_index;
    while mutations.get(mutation_index).is_some_and(|mutation| mutation.role() == role) {
      mutation_index += 1;
    }
    let role_mutations = &mutations[role_start..mutation_index];
    let summary = if let Some(source_root_key) = source_role_root(&source_manifest.details, role)? {
      let (first_page_id, last_page_id) = source_role_page_boundaries(&source_manifest.details, role);
      apply_present_role(
        request,
        source_manifest.owner_id,
        role,
        source_manifest.generation,
        next_page_id,
        first_page_id,
        last_page_id,
        source_root_key,
        role_mutations,
        overlay,
        source,
        is_cancelled,
      )?
    } else {
      let role_mutations = published_mutation_kinds(request.hash_algorithm, role_mutations, is_cancelled)?;
      let bootstrap = bootstrap_ordered_index_v1(&IndexCopyOnWriteBootstrapRequestV1 {
        hash_algorithm: request.hash_algorithm,
        owner_id: source_manifest.owner_id,
        role,
        generation: request.generation,
        initial_next_page_id: next_page_id,
        mutations: &role_mutations,
        page_layout: request.page_layout,
        directory_layout: request.directory_layout,
      })?;
      for artifact in bootstrap.artifacts {
        overlay.insert(artifact)?;
      }
      bootstrap.summary
    };
    next_page_id = summary.next_page_id;
    summaries.push(summary);
  }
  Ok(summaries)
}

#[allow(clippy::too_many_arguments)]
fn apply_present_role(
  request: &FrozenIndexBatchApplicationRequestV1<'_>,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  source_generation: u64,
  initial_next_page_id: u64,
  mut first_page_id: u64,
  mut last_page_id: u64,
  source_root_key: &[u8],
  mutations: &[PublishedIndexMutationV1],
  overlay: &mut SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<IndexCopyOnWriteClosureSummaryV1, IndexBatchApplicationErrorV1> {
  validate_sparse_metadata_budget(mutations.len(), request.directory_layout.maximum_workspace_bytes)?;
  let semantic_indices = semantic_mutation_indices(request.hash_algorithm, role, mutations, is_cancelled)?;
  let ranges =
    plan_present_role_ranges(request, owner_id, role, source_root_key, mutations, &semantic_indices, overlay, source, is_cancelled)?;
  let step_count = ranges.len();
  let step_offset = u64::try_from(step_count.saturating_sub(1))
    .map_err(|error| manifest_closure_error(format!("sparse application step count is not representable: {error}")))?;
  let first_generation = request
    .generation
    .checked_sub(step_offset)
    .filter(|generation| *generation > source_generation)
    .ok_or_else(|| manifest_closure_error("successor generation has insufficient room for its sparse application steps"))?;
  let mut current_root_key = clone_bytes(source_root_key, "present-role source root")?;
  let mut next_page_id = initial_next_page_id;
  let mut summaries = Vec::new();
  let mut step_mutations = Vec::new();
  summaries
    .try_reserve_exact(ranges.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("sparse closure reservation failed: {error}")))?;
  step_mutations
    .try_reserve_exact(ranges.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("sparse mutation partition reservation failed: {error}")))?;

  for (step_index, range) in ranges.into_iter().enumerate() {
    check_cancelled(is_cancelled)?;
    let generation = first_generation
      .checked_add(
        u64::try_from(step_index)
          .map_err(|error| manifest_closure_error(format!("sparse application step index is not representable: {error}")))?,
      )
      .ok_or_else(|| manifest_closure_error("sparse application generation overflowed"))?;
    let mut semantic_kinds = Vec::new();
    semantic_kinds
      .try_reserve_exact(range.len())
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("semantic mutation reservation failed: {error}")))?;
    let mut frozen_indices = Vec::new();
    frozen_indices
      .try_reserve_exact(range.len())
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("frozen mutation index reservation failed: {error}")))?;
    for (range_index, semantic_index) in semantic_indices[range.clone()].iter().enumerate() {
      if range_index % 4_096 == 0 {
        check_cancelled(is_cancelled)?;
      }
      let mutation = &mutations[*semantic_index];
      semantic_kinds.push(published_mutation_kind(request.hash_algorithm, mutation)?);
      frozen_indices.push(*semantic_index);
    }
    sort_frozen_mutation_indices(mutations, &mut frozen_indices, is_cancelled)?;
    let mut frozen_kinds = Vec::new();
    frozen_kinds
      .try_reserve_exact(frozen_indices.len())
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("frozen mutation reservation failed: {error}")))?;
    for mutation_index in frozen_indices {
      frozen_kinds.push(published_mutation_kind(request.hash_algorithm, &mutations[mutation_index])?);
    }

    let first_mutation = &mutations[semantic_indices[range.start]];
    let path = load_ordered_page_path_v1(
      &OrderedPagePathLookupRequestV1 {
        hash_algorithm: request.hash_algorithm,
        root_key: &current_root_key,
        owner_id,
        role,
        order_key: first_mutation.order_key(),
        load_posting_successor: role == OrderedIndexRoleV1::Posting,
        limits: request.path_limits,
      },
      overlay,
      source,
      is_cancelled,
    )?;
    validate_planned_range_against_path(request.hash_algorithm, role, mutations, &semantic_indices[range], &path)?;
    let page_plan = mutate_ordered_page_batch_for_publication_v1(&OrderedPageBatchMutationRequestV1 {
      hash_algorithm: request.hash_algorithm,
      source_page: path.page(),
      next_posting_page: path.next_posting_page(),
      generation,
      next_page_id,
      mutations: &semantic_kinds,
      layout: request.page_layout,
    })?;
    if role == OrderedIndexRoleV1::Posting {
      let source_page = decode_ordered_page(path.page(), request.hash_algorithm)?;
      let replacement =
        page_plan.replacements.first().ok_or_else(|| manifest_closure_error("publication page mutation produced no replacement"))?;
      let first_output = replacement
        .artifacts
        .first()
        .ok_or_else(|| manifest_closure_error("posting mutation retired a page without an endpoint replacement"))?;
      let last_output = replacement
        .artifacts
        .last()
        .ok_or_else(|| manifest_closure_error("posting mutation retired a page without an endpoint replacement"))?;
      if source_page.page_id == first_page_id {
        first_page_id = decode_ordered_page(&first_output.value, request.hash_algorithm)?.page_id;
      }
      if source_page.page_id == last_page_id {
        last_page_id = decode_ordered_page(&last_output.value, request.hash_algorithm)?.page_id;
      }
    }
    let main_directories = collect_path_directories(&path, false)?;
    let next_directories = collect_path_directories(&path, true)?;
    let mut paths = Vec::new();
    let mut source_pages = Vec::new();
    paths
      .try_reserve_exact(page_plan.replacements.len())
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("copy-on-write path reservation failed: {error}")))?;
    source_pages
      .try_reserve_exact(page_plan.replacements.len())
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("copy-on-write source-page reservation failed: {error}")))?;
    let main_replacement =
      page_plan.replacements.first().ok_or_else(|| manifest_closure_error("publication page mutation produced no replacement"))?;
    paths.push(ArtifactDirectoryPathV1 { source_page_key: &main_replacement.source_key, directories: &main_directories });
    source_pages.push(path.page());
    if page_plan.replacements.len() == 2 {
      let next_page =
        path.next_posting_page().ok_or_else(|| manifest_closure_error("posting relink replacement has no loaded successor page"))?;
      let next_replacement = &page_plan.replacements[1];
      paths.push(ArtifactDirectoryPathV1 { source_page_key: &next_replacement.source_key, directories: &next_directories });
      source_pages.push(next_page);
    } else if page_plan.replacements.len() != 1 {
      return Err(manifest_closure_error("one sparse page step produced an unsupported replacement count"));
    }
    let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
      hash_algorithm: request.hash_algorithm,
      generation,
      page_plan: &page_plan,
      paths: &paths,
      layout: request.directory_layout,
    })?;
    let mut summary = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
      hash_algorithm: request.hash_algorithm,
      generation,
      initial_next_page_id: next_page_id,
      applied_mutations: Some(&frozen_kinds),
      source_pages: &source_pages,
      paths: &paths,
      page_plan: &page_plan,
      directory_plan: &directory_plan,
      page_layout: request.page_layout,
      directory_layout: request.directory_layout,
    })?;
    if role == OrderedIndexRoleV1::Posting {
      bind_logical_page_endpoints_v1(&mut summary, first_page_id, last_page_id)?;
    }
    next_page_id = summary.next_page_id;
    if step_index + 1 < step_count {
      current_root_key = clone_bytes(
        summary
          .root_key
          .as_deref()
          .ok_or_else(|| manifest_closure_error("an intermediate sparse application step retired its role root"))?,
        "intermediate sparse root",
      )?;
    }
    for replacement in page_plan.replacements {
      for artifact in replacement.artifacts {
        overlay.insert(artifact)?;
      }
    }
    for artifact in directory_plan.artifacts {
      overlay.insert(artifact)?;
    }
    summaries.push(summary);
    step_mutations.push(frozen_kinds);
  }

  if summaries.len() == 1 {
    return summaries.pop().ok_or_else(|| manifest_closure_error("single sparse application summary disappeared"));
  }
  let mut steps = Vec::new();
  steps
    .try_reserve_exact(summaries.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("composite step reservation failed: {error}")))?;
  for (summary, mutations) in summaries.iter().zip(&step_mutations) {
    steps.push(IndexCopyOnWriteCompositeStepV1 { closure: summary, applied_mutations: mutations });
  }
  let all_mutations = published_mutation_kinds(request.hash_algorithm, mutations, is_cancelled)?;
  Ok(validate_index_copy_on_write_composite_closure_v1(&IndexCopyOnWriteCompositeClosureRequestV1 {
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    initial_next_page_id,
    source_root_key,
    applied_mutations: &all_mutations,
    steps: &steps,
    maximum_workspace_bytes: request.directory_layout.maximum_workspace_bytes,
  })?)
}

fn validate_sparse_metadata_budget(mutation_count: usize, maximum_workspace_bytes: usize) -> Result<(), IndexBatchApplicationErrorV1> {
  let mutation_metadata = mutation_count
    .checked_mul(size_of::<usize>() + 4 * size_of::<OrderedPageMutationKindV1<'_>>())
    .ok_or_else(|| manifest_closure_error("sparse mutation metadata bytes overflowed"))?;
  let maximum_steps = mutation_count.min(INDEX_COPY_ON_WRITE_MAXIMUM_COMPOSITE_STEPS_V1);
  let step_metadata = maximum_steps
    .checked_mul(
      size_of::<Range<usize>>()
        + size_of::<IndexCopyOnWriteClosureSummaryV1>()
        + size_of::<Vec<OrderedPageMutationKindV1<'_>>>()
        + size_of::<IndexCopyOnWriteCompositeStepV1<'_>>(),
    )
    .ok_or_else(|| manifest_closure_error("sparse step metadata bytes overflowed"))?;
  let total =
    mutation_metadata.checked_add(step_metadata).ok_or_else(|| manifest_closure_error("sparse application metadata bytes overflowed"))?;
  if total > maximum_workspace_bytes {
    return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "index_batch_application_metadata",
      format!("{total} sparse application metadata bytes exceed the {maximum_workspace_bytes}-byte workspace cap"),
    )));
  }
  Ok(())
}

fn sort_frozen_mutation_indices(
  mutations: &[PublishedIndexMutationV1],
  indices: &mut [usize],
  is_cancelled: &dyn Fn() -> bool,
) -> Result<(), IndexBatchApplicationErrorV1> {
  for (iteration, root) in (0..indices.len() / 2).rev().enumerate() {
    if iteration % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    sift_frozen_mutation_heap(mutations, indices, root, indices.len())?;
  }
  for (iteration, end) in (1..indices.len()).rev().enumerate() {
    if iteration % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    indices.swap(0, end);
    sift_frozen_mutation_heap(mutations, indices, 0, end)?;
  }
  if indices.windows(2).any(|pair| mutations[pair[0]].order_key() >= mutations[pair[1]].order_key()) {
    return Err(manifest_closure_error("sparse step mutations are not unique in frozen byte order"));
  }
  Ok(())
}

fn sift_frozen_mutation_heap(
  mutations: &[PublishedIndexMutationV1],
  indices: &mut [usize],
  mut root: usize,
  end: usize,
) -> Result<(), IndexBatchApplicationErrorV1> {
  loop {
    let Some(left) = root.checked_mul(2).and_then(|value| value.checked_add(1)) else {
      return Err(manifest_closure_error("frozen mutation heap index overflowed"));
    };
    if left >= end {
      return Ok(());
    }
    let mut greatest = root;
    if mutations[indices[greatest]].order_key() < mutations[indices[left]].order_key() {
      greatest = left;
    }
    let right = left + 1;
    if right < end && mutations[indices[greatest]].order_key() < mutations[indices[right]].order_key() {
      greatest = right;
    }
    if greatest == root {
      return Ok(());
    }
    indices.swap(root, greatest);
    root = greatest;
  }
}

fn semantic_mutation_indices(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  mutations: &[PublishedIndexMutationV1],
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<usize>, IndexBatchApplicationErrorV1> {
  let mut indices = Vec::new();
  indices
    .try_reserve_exact(mutations.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("semantic mutation-index reservation failed: {error}")))?;
  indices.extend(0..mutations.len());
  for (iteration, root) in (0..indices.len() / 2).rev().enumerate() {
    if iteration % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    sift_semantic_mutation_heap(hash_algorithm, role, mutations, &mut indices, root, mutations.len())?;
  }
  for (iteration, end) in (1..indices.len()).rev().enumerate() {
    if iteration % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    indices.swap(0, end);
    sift_semantic_mutation_heap(hash_algorithm, role, mutations, &mut indices, 0, end)?;
  }
  for pair in indices.windows(2) {
    if compare_order_keys(hash_algorithm, role, mutations[pair[0]].order_key(), mutations[pair[1]].order_key())? != Ordering::Less {
      return Err(manifest_closure_error("frozen role mutations are not semantically unique"));
    }
  }
  Ok(indices)
}

fn sift_semantic_mutation_heap(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  mutations: &[PublishedIndexMutationV1],
  indices: &mut [usize],
  mut root: usize,
  end: usize,
) -> Result<(), IndexBatchApplicationErrorV1> {
  loop {
    let Some(left) = root.checked_mul(2).and_then(|value| value.checked_add(1)) else {
      return Err(manifest_closure_error("semantic mutation heap index overflowed"));
    };
    if left >= end {
      return Ok(());
    }
    let mut greatest = root;
    if compare_order_keys(hash_algorithm, role, mutations[indices[greatest]].order_key(), mutations[indices[left]].order_key())?
      == Ordering::Less
    {
      greatest = left;
    }
    let right = left + 1;
    if right < end
      && compare_order_keys(hash_algorithm, role, mutations[indices[greatest]].order_key(), mutations[indices[right]].order_key())?
        == Ordering::Less
    {
      greatest = right;
    }
    if greatest == root {
      return Ok(());
    }
    indices.swap(root, greatest);
    root = greatest;
  }
}

#[allow(clippy::too_many_arguments)]
fn plan_present_role_ranges(
  request: &FrozenIndexBatchApplicationRequestV1<'_>,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  source_root_key: &[u8],
  mutations: &[PublishedIndexMutationV1],
  semantic_indices: &[usize],
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<Range<usize>>, IndexBatchApplicationErrorV1> {
  let mut ranges = Vec::new();
  let mut start = 0usize;
  while start < semantic_indices.len() {
    check_cancelled(is_cancelled)?;
    if ranges.len() >= INDEX_COPY_ON_WRITE_MAXIMUM_COMPOSITE_STEPS_V1 {
      return Err(manifest_closure_error("sparse application exceeds the composite-step cap"));
    }
    let mutation = &mutations[semantic_indices[start]];
    let path = load_ordered_page_path_v1(
      &OrderedPagePathLookupRequestV1 {
        hash_algorithm: request.hash_algorithm,
        root_key: source_root_key,
        owner_id,
        role,
        order_key: mutation.order_key(),
        load_posting_successor: false,
        limits: request.path_limits,
      },
      overlay,
      source,
      is_cancelled,
    )?;
    let page = decode_ordered_page(path.page(), request.hash_algorithm)?;
    let terminal = path_is_terminal(request.hash_algorithm, role, &path)?;
    let mut end = start + 1;
    if terminal {
      end = semantic_indices.len();
    } else {
      while end < semantic_indices.len()
        && compare_order_keys(request.hash_algorithm, role, mutations[semantic_indices[end]].order_key(), page.upper_fence)?
          != Ordering::Greater
      {
        if (end - start).is_multiple_of(4_096) {
          check_cancelled(is_cancelled)?;
        }
        end += 1;
      }
    }
    ranges.try_reserve(1).map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("sparse range reservation failed: {error}")))?;
    ranges.push(start..end);
    start = end;
  }
  Ok(ranges)
}

fn validate_planned_range_against_path(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  mutations: &[PublishedIndexMutationV1],
  semantic_indices: &[usize],
  path: &LoadedOrderedPagePathV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  let last = semantic_indices.last().ok_or_else(|| manifest_closure_error("planned sparse range is empty"))?;
  let page = decode_ordered_page(path.page(), hash_algorithm)?;
  if !path_is_terminal(hash_algorithm, role, path)?
    && compare_order_keys(hash_algorithm, role, mutations[*last].order_key(), page.upper_fence)? == Ordering::Greater
  {
    return Err(manifest_closure_error("planned sparse mutations no longer select one current page"));
  }
  Ok(())
}

fn path_is_terminal(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  path: &LoadedOrderedPagePathV1,
) -> Result<bool, IndexBatchApplicationErrorV1> {
  let root = path.directory(0).ok_or_else(|| manifest_closure_error("ordered page path has no root directory"))?;
  let root = decode_artifact_directory(root, hash_algorithm)?;
  let page = decode_ordered_page(path.page(), hash_algorithm)?;
  Ok(compare_order_keys(hash_algorithm, role, page.upper_fence, root.upper_fence)? == Ordering::Equal)
}

fn collect_path_directories(path: &LoadedOrderedPagePathV1, successor: bool) -> Result<Vec<&[u8]>, IndexBatchApplicationErrorV1> {
  let count = if successor { path.next_directory_count() } else { path.directory_count() };
  let mut directories = Vec::new();
  directories
    .try_reserve_exact(count)
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("directory-slice reservation failed: {error}")))?;
  for index in 0..count {
    let directory = if successor { path.next_directory(index) } else { path.directory(index) }
      .ok_or_else(|| manifest_closure_error("loaded ordered path lost a directory"))?;
    directories.push(directory);
  }
  Ok(directories)
}

fn published_mutation_kinds<'a>(
  hash_algorithm: HashAlgorithm,
  mutations: &'a [PublishedIndexMutationV1],
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<OrderedPageMutationKindV1<'a>>, IndexBatchApplicationErrorV1> {
  let mut kinds = Vec::new();
  kinds
    .try_reserve_exact(mutations.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("ordered mutation reservation failed: {error}")))?;
  for (index, mutation) in mutations.iter().enumerate() {
    if index % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    kinds.push(published_mutation_kind(hash_algorithm, mutation)?);
  }
  Ok(kinds)
}

fn published_mutation_kind<'a>(
  hash_algorithm: HashAlgorithm,
  mutation: &'a PublishedIndexMutationV1,
) -> Result<OrderedPageMutationKindV1<'a>, IndexBatchApplicationErrorV1> {
  let decoded = decode_ordered_record(mutation.encoded_record(), hash_algorithm, mutation.role())?;
  Ok(manifest_mutation_kind(mutation, decoded.tombstone))
}

fn source_parent_manifest_key<'a>(source: &'a IndexManifestBodyV1<'a>) -> Option<&'a [u8]> {
  match source {
    IndexManifestBodyV1::ScopeCatalog(_) | IndexManifestBodyV1::FieldNvt(_) => None,
    IndexManifestBodyV1::ValueStore(body) => Some(body.scope_catalog_manifest),
    IndexManifestBodyV1::FieldIndex(body) => Some(body.value_store_manifest),
  }
}

fn manifest_coverage<'a>(source: &'a IndexManifestBodyV1<'a>) -> Result<&'a CoverageVersionV1<'a>, IndexBatchApplicationErrorV1> {
  match source {
    IndexManifestBodyV1::ScopeCatalog(body) => Ok(&body.coverage),
    IndexManifestBodyV1::ValueStore(body) => Ok(&body.coverage),
    IndexManifestBodyV1::FieldIndex(body) => Ok(&body.coverage),
    IndexManifestBodyV1::FieldNvt(_) => Err(manifest_closure_error("FieldNvt manifests do not carry ordered-data coverage")),
  }
}

fn source_role_root<'a>(
  source: &'a IndexManifestBodyV1<'a>,
  role: OrderedIndexRoleV1,
) -> Result<Option<&'a [u8]>, IndexBatchApplicationErrorV1> {
  match (source, role) {
    (IndexManifestBodyV1::ScopeCatalog(body), OrderedIndexRoleV1::ScopeOrdinal) => Ok(body.ordinal_directory_root),
    (IndexManifestBodyV1::ScopeCatalog(body), OrderedIndexRoleV1::ScopeReverse) => Ok(body.reverse_directory_root),
    (IndexManifestBodyV1::ValueStore(body), OrderedIndexRoleV1::Value) => Ok(body.value_directory_root),
    (IndexManifestBodyV1::ValueStore(body), OrderedIndexRoleV1::ValueDocumentState) => Ok(body.document_state_directory_root),
    (IndexManifestBodyV1::FieldIndex(body), OrderedIndexRoleV1::Posting) => Ok(body.posting_directory_root),
    (IndexManifestBodyV1::FieldIndex(body), OrderedIndexRoleV1::IndexDocumentState) => Ok(body.document_state_directory_root),
    _ => Err(manifest_closure_error("ordered mutation role does not belong to its source manifest")),
  }
}

fn source_role_page_boundaries(source: &IndexManifestBodyV1<'_>, role: OrderedIndexRoleV1) -> (u64, u64) {
  match (source, role) {
    (IndexManifestBodyV1::FieldIndex(body), OrderedIndexRoleV1::Posting) => (body.first_page_id, body.last_page_id),
    _ => (0, 0),
  }
}

fn ordered_role_count(source: &IndexManifestBodyV1<'_>) -> usize {
  match source {
    IndexManifestBodyV1::ScopeCatalog(_) | IndexManifestBodyV1::ValueStore(_) | IndexManifestBodyV1::FieldIndex(_) => 2,
    IndexManifestBodyV1::FieldNvt(_) => 0,
  }
}

fn owner_class_rank(owner_class: IndexMembershipOwnerClassV1) -> u8 {
  match owner_class {
    IndexMembershipOwnerClassV1::ScopeCatalog => 0,
    IndexMembershipOwnerClassV1::ValueStore => 1,
    IndexMembershipOwnerClassV1::FieldIndex => 2,
  }
}

fn expected_parent_owner_class(owner_class: IndexMembershipOwnerClassV1) -> Option<IndexMembershipOwnerClassV1> {
  match owner_class {
    IndexMembershipOwnerClassV1::ScopeCatalog => None,
    IndexMembershipOwnerClassV1::ValueStore => Some(IndexMembershipOwnerClassV1::ScopeCatalog),
    IndexMembershipOwnerClassV1::FieldIndex => Some(IndexMembershipOwnerClassV1::ValueStore),
  }
}

fn synthesize_frozen_owner_plans(
  request: &FrozenIndexBatchApplicationRequestV1<'_>,
  mut pending: Vec<PendingFrozenOwnerApplicationV1<'_>>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<FrozenIndexOwnerApplicationPlanV1>, IndexBatchApplicationErrorV1> {
  pending.sort_unstable_by(|left, right| {
    owner_class_rank(left.owner_class).cmp(&owner_class_rank(right.owner_class)).then(left.owner_id.cmp(&right.owner_id))
  });
  let mut source_classes: HashMap<Vec<u8>, IndexMembershipOwnerClassV1> = HashMap::new();
  source_classes
    .try_reserve(pending.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("source-manifest map reservation failed: {error}")))?;
  for owner in &pending {
    if source_classes.insert(clone_bytes(&owner.source_manifest_key, "source-manifest map key")?, owner.owner_class).is_some() {
      return Err(manifest_closure_error("two frozen owners use one source manifest key"));
    }
  }
  let mut successor_keys: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
  successor_keys
    .try_reserve(pending.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor-manifest map reservation failed: {error}")))?;
  let mut output = Vec::new();
  output
    .try_reserve_exact(pending.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("owner-plan reservation failed: {error}")))?;
  for owner in pending {
    check_cancelled(is_cancelled)?;
    let source = decode_index_manifest(owner.source_manifest, request.hash_algorithm)?;
    let source_parent = source_parent_manifest_key(&source.details);
    let parent_manifest_key = if let Some(source_parent) = source_parent {
      if let Some(parent_class) = source_classes.get(source_parent) {
        if Some(*parent_class) != expected_parent_owner_class(owner.owner_class) {
          return Err(manifest_closure_error("same-batch parent source manifest has the wrong owner class"));
        }
        Some(
          successor_keys
            .get(source_parent)
            .ok_or_else(|| manifest_closure_error("same-batch parent successor was not synthesized before its child"))?
            .as_slice(),
        )
      } else {
        Some(source_parent)
      }
    } else {
      None
    };
    let successor_manifest = synthesize_successor_index_manifest_v1(
      &IndexManifestSuccessorRequestV1 {
        hash_algorithm: request.hash_algorithm,
        source_manifest: owner.source_manifest,
        generation: request.generation,
        parent_manifest_key,
        coverage: request.coverage.clone(),
        next_document_ordinal: owner.next_document_ordinal,
        mutations: &request.batch.records()[owner.mutation_range.clone()],
        transitions: &request.batch.transitions()[owner.transition_range.clone()],
        role_summaries: &owner.role_summaries,
      },
      is_cancelled,
    )?;
    successor_keys.insert(
      clone_bytes(&owner.source_manifest_key, "successor-manifest source key")?,
      clone_bytes(&successor_manifest.key, "successor-manifest key")?,
    );
    output.push(FrozenIndexOwnerApplicationPlanV1 {
      owner_id: owner.owner_id,
      owner_class: owner.owner_class,
      source_manifest_key: owner.source_manifest_key,
      source_generation: owner.source_generation,
      successor_manifest,
      dependency_range: owner.dependency_range,
      role_summaries: owner.role_summaries,
    });
  }
  Ok(output)
}

pub fn synthesize_successor_index_manifest_v1(
  request: &IndexManifestSuccessorRequestV1<'_>,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<EncodedImmutableIndexArtifactV1, IndexBatchApplicationErrorV1> {
  check_cancelled(is_cancelled)?;
  let source = decode_index_manifest(request.source_manifest, request.hash_algorithm)?;
  if request.generation == 0 || request.generation <= source.generation {
    return Err(manifest_closure_error("successor generation is not newer than the source manifest generation"));
  }
  validate_successor_coverage(&request.coverage, request.hash_algorithm)?;
  let owner_class = manifest_owner_class(&source.details)?;
  let transition_deltas = validate_manifest_transitions(request, source.owner_id, owner_class, is_cancelled)?;
  let mutation_closure = validate_manifest_mutations(request, source.owner_id, owner_class, is_cancelled)?;
  if transition_deltas.required_roles & !mutation_closure.role_mask != 0 {
    return Err(manifest_closure_error("membership-changing transitions are missing their required ordered-record mutations"));
  }
  let required_roles = mutation_closure.role_mask | transition_deltas.required_roles;
  validate_manifest_role_summaries(request, &source.details, source.owner_id, owner_class, required_roles, &mutation_closure)?;
  check_cancelled(is_cancelled)?;

  encode_successor_index_manifest_v1(request, &source, &transition_deltas)
}

fn synthesize_compacted_index_manifest_v1(
  hash_algorithm: HashAlgorithm,
  source_manifest: &[u8],
  generation: u64,
  parent_manifest_key: Option<&[u8]>,
  summary: &IndexCopyOnWriteClosureSummaryV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<EncodedImmutableIndexArtifactV1, IndexBatchApplicationErrorV1> {
  check_cancelled(is_cancelled)?;
  let source = decode_index_manifest(source_manifest, hash_algorithm)?;
  if generation <= source.generation || summary.mutation_commitment.is_some() {
    return Err(manifest_closure_error(
      "compaction manifest generation must advance its source and carry an unbound maintenance COW summary",
    ));
  }
  let coverage = manifest_coverage(&source.details)?.clone();
  let role_summaries = std::slice::from_ref(summary);
  let request = IndexManifestSuccessorRequestV1 {
    hash_algorithm,
    source_manifest,
    generation,
    parent_manifest_key,
    coverage,
    next_document_ordinal: match &source.details {
      IndexManifestBodyV1::ScopeCatalog(body) => Some(body.next_document_ordinal),
      IndexManifestBodyV1::ValueStore(_) | IndexManifestBodyV1::FieldIndex(_) | IndexManifestBodyV1::FieldNvt(_) => None,
    },
    mutations: &[],
    transitions: &[],
    role_summaries,
  };
  let owner_class = manifest_owner_class(&source.details)?;
  let mutation_closure = ManifestMutationClosureV1 { role_mask: 0, commitments: std::array::from_fn(|_| None) };
  validate_manifest_role_summaries(&request, &source.details, source.owner_id, owner_class, role_bit(summary.role), &mutation_closure)?;
  check_cancelled(is_cancelled)?;
  encode_successor_index_manifest_v1(&request, &source, &ManifestTransitionDeltasV1::default())
}

fn encode_successor_index_manifest_v1(
  request: &IndexManifestSuccessorRequestV1<'_>,
  source: &IndexManifestV1<'_>,
  transition_deltas: &ManifestTransitionDeltasV1,
) -> Result<EncodedImmutableIndexArtifactV1, IndexBatchApplicationErrorV1> {
  let body = match &source.details {
    IndexManifestBodyV1::ScopeCatalog(body) => {
      let successor = synthesize_scope_manifest_body(request, body, transition_deltas)?;
      IndexManifestBodyV1::ScopeCatalog(successor)
    }
    IndexManifestBodyV1::ValueStore(body) => {
      let successor = synthesize_value_manifest_body(request, body, transition_deltas)?;
      IndexManifestBodyV1::ValueStore(successor)
    }
    IndexManifestBodyV1::FieldIndex(body) => {
      let successor = synthesize_field_manifest_body(request, body, transition_deltas)?;
      IndexManifestBodyV1::FieldIndex(successor)
    }
    IndexManifestBodyV1::FieldNvt(_) => return Err(manifest_closure_error("FieldNvt manifests are not ordered-batch owners")),
  };
  encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    owner_id: source.owner_id,
    body,
  })
  .map_err(Into::into)
}

#[derive(Debug, Default)]
struct ManifestTransitionDeltasV1 {
  live_additions: u64,
  live_removals: u64,
  unindexable_additions: u64,
  unindexable_removals: u64,
  required_roles: u16,
  maximum_document_ordinal: u64,
}

struct ManifestMutationClosureV1 {
  role_mask: u16,
  commitments: [Option<Vec<u8>>; 8],
}

impl ManifestMutationClosureV1 {
  fn commitment(&self, role: OrderedIndexRoleV1) -> Option<&[u8]> {
    self.commitments.get(usize::from(role.id())).and_then(Option::as_deref)
  }
}

fn validate_successor_coverage(
  coverage: &CoverageVersionV1<'_>,
  hash_algorithm: HashAlgorithm,
) -> Result<(), IndexBatchApplicationErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  if coverage.source_namespace_root.len() != hash_width
    || coverage.source_namespace_root.iter().all(|byte| *byte == 0)
    || coverage.coverage_epoch_id.len() != 16
    || coverage.coverage_epoch_id.iter().all(|byte| *byte == 0)
    || coverage.coverage_publication_sequence == 0
  {
    return Err(manifest_closure_error("successor coverage identity is incomplete or disagrees with the hash profile"));
  }
  Ok(())
}

fn synthesize_scope_manifest_body<'a>(
  request: &'a IndexManifestSuccessorRequestV1<'a>,
  source: &'a ScopeCatalogManifestBodyV1<'a>,
  deltas: &ManifestTransitionDeltasV1,
) -> Result<ScopeCatalogManifestBodyV1<'a>, IndexBatchApplicationErrorV1> {
  if request.parent_manifest_key.is_some() {
    return Err(manifest_closure_error("ScopeCatalog successor must not name a parent manifest"));
  }
  let next_document_ordinal = request
    .next_document_ordinal
    .ok_or_else(|| manifest_closure_error("ScopeCatalog successor is missing its document-ordinal high-water"))?;
  let minimum_next_document_ordinal =
    deltas.maximum_document_ordinal.checked_add(1).ok_or_else(|| manifest_closure_error("ScopeCatalog document ordinal overflowed"))?;
  if next_document_ordinal < source.next_document_ordinal || next_document_ordinal < minimum_next_document_ordinal {
    return Err(manifest_closure_error("ScopeCatalog document-ordinal high-water regressed or does not cover the batch"));
  }
  let live_document_count =
    apply_counter_delta(source.live_document_count, deltas.live_additions, deltas.live_removals, "scope live-document count")?;
  let ordinal = role_summary(request, OrderedIndexRoleV1::ScopeOrdinal);
  let reverse = role_summary(request, OrderedIndexRoleV1::ScopeReverse);
  if ordinal.is_some_and(|summary| summary.live_count != live_document_count)
    || reverse.is_some_and(|summary| summary.live_count != live_document_count || summary.tombstone_count != 0)
  {
    return Err(manifest_closure_error("ScopeCatalog role summaries disagree with exact live-document membership"));
  }
  Ok(ScopeCatalogManifestBodyV1 {
    required_reader_capabilities: source.required_reader_capabilities,
    coverage: request.coverage.clone(),
    next_document_ordinal,
    ordinal_directory_root: successor_root(ordinal, source.ordinal_directory_root),
    reverse_directory_root: successor_root(reverse, source.reverse_directory_root),
    live_document_count,
    retained_tombstone_count: ordinal.map_or(source.retained_tombstone_count, |summary| summary.tombstone_count),
    ordinal_page_count: ordinal.map_or(source.ordinal_page_count, |summary| summary.page_count),
    reverse_page_count: reverse.map_or(source.reverse_page_count, |summary| summary.page_count),
    scope_definition: source.scope_definition,
  })
}

fn synthesize_value_manifest_body<'a>(
  request: &'a IndexManifestSuccessorRequestV1<'a>,
  source: &'a ValueStoreManifestBodyV1<'a>,
  deltas: &ManifestTransitionDeltasV1,
) -> Result<ValueStoreManifestBodyV1<'a>, IndexBatchApplicationErrorV1> {
  if request.next_document_ordinal.is_some() {
    return Err(manifest_closure_error("ValueStore successor must not carry a ScopeCatalog ordinal high-water"));
  }
  let parent = require_parent_manifest(request)?;
  let value_document_count =
    apply_counter_delta(source.value_document_count, deltas.live_additions, deltas.live_removals, "value-store distinct-document count")?;
  let unindexable_document_count = apply_counter_delta(
    source.unindexable_document_count,
    deltas.unindexable_additions,
    deltas.unindexable_removals,
    "value-store unindexable-document count",
  )?;
  let values = role_summary(request, OrderedIndexRoleV1::Value);
  let states = role_summary(request, OrderedIndexRoleV1::ValueDocumentState);
  if states.is_some_and(|summary| summary.live_count != unindexable_document_count) {
    return Err(manifest_closure_error("ValueStore state summary disagrees with exact unindexable membership"));
  }
  let next_page_id = final_next_page_id(request, source.next_page_id);
  Ok(ValueStoreManifestBodyV1 {
    required_reader_capabilities: source.required_reader_capabilities,
    coverage: request.coverage.clone(),
    scope_catalog_manifest: parent,
    value_directory_root: successor_root(values, source.value_directory_root),
    document_state_directory_root: successor_root(states, source.document_state_directory_root),
    next_page_id,
    value_page_count: values.map_or(source.value_page_count, |summary| summary.page_count),
    state_page_count: states.map_or(source.state_page_count, |summary| summary.page_count),
    value_document_count,
    unindexable_document_count,
    live_value_count: values.map_or(source.live_value_count, |summary| summary.live_count),
    value_tombstone_count: values.map_or(source.value_tombstone_count, |summary| summary.tombstone_count),
    state_tombstone_count: states.map_or(source.state_tombstone_count, |summary| summary.tombstone_count),
    live_canonical_value_bytes: values.map_or(source.live_canonical_value_bytes, |summary| summary.logical_bytes),
    value_store_definition: source.value_store_definition,
  })
}

fn synthesize_field_manifest_body<'a>(
  request: &'a IndexManifestSuccessorRequestV1<'a>,
  source: &'a FieldIndexManifestBodyV1<'a>,
  deltas: &ManifestTransitionDeltasV1,
) -> Result<FieldIndexManifestBodyV1<'a>, IndexBatchApplicationErrorV1> {
  if request.next_document_ordinal.is_some() {
    return Err(manifest_closure_error("FieldIndex successor must not carry a ScopeCatalog ordinal high-water"));
  }
  let parent = require_parent_manifest(request)?;
  let posting_document_count =
    apply_counter_delta(source.posting_document_count, deltas.live_additions, deltas.live_removals, "field-index distinct-document count")?;
  let unindexable_document_count = apply_counter_delta(
    source.unindexable_document_count,
    deltas.unindexable_additions,
    deltas.unindexable_removals,
    "field-index unindexable-document count",
  )?;
  let postings = role_summary(request, OrderedIndexRoleV1::Posting);
  let states = role_summary(request, OrderedIndexRoleV1::IndexDocumentState);
  if states.is_some_and(|summary| summary.live_count != unindexable_document_count) {
    return Err(manifest_closure_error("FieldIndex state summary disagrees with exact unindexable membership"));
  }
  let posting_root = successor_root(postings, source.posting_directory_root);
  let (first_page_id, last_page_id) = match postings {
    Some(summary) if summary.root_key.is_some() => (summary.first_page_id(), summary.last_page_id()),
    Some(_) => (0, 0),
    None => (source.first_page_id, source.last_page_id),
  };
  let next_page_id = final_next_page_id(request, source.next_page_id);
  Ok(FieldIndexManifestBodyV1 {
    required_reader_capabilities: source.required_reader_capabilities,
    coverage: request.coverage.clone(),
    value_store_manifest: parent,
    posting_directory_root: posting_root,
    document_state_directory_root: successor_root(states, source.document_state_directory_root),
    first_page_id,
    last_page_id,
    next_page_id,
    posting_page_count: postings.map_or(source.posting_page_count, |summary| summary.page_count),
    state_page_count: states.map_or(source.state_page_count, |summary| summary.page_count),
    live_posting_count: postings.map_or(source.live_posting_count, |summary| summary.live_count),
    posting_tombstone_count: postings.map_or(source.posting_tombstone_count, |summary| summary.tombstone_count),
    posting_document_count,
    unindexable_document_count,
    state_tombstone_count: states.map_or(source.state_tombstone_count, |summary| summary.tombstone_count),
    live_canonical_posting_bytes: postings.map_or(source.live_canonical_posting_bytes, |summary| summary.logical_bytes),
    field_index_definition: source.field_index_definition,
  })
}

fn role_summary<'a>(
  request: &'a IndexManifestSuccessorRequestV1<'a>,
  role: OrderedIndexRoleV1,
) -> Option<&'a IndexCopyOnWriteClosureSummaryV1> {
  request.role_summaries.iter().find(|summary| summary.role == role)
}

fn successor_root<'a>(summary: Option<&'a IndexCopyOnWriteClosureSummaryV1>, source_root: Option<&'a [u8]>) -> Option<&'a [u8]> {
  match summary {
    Some(summary) => summary.root_key.as_deref(),
    None => source_root,
  }
}

fn require_parent_manifest<'a>(request: &'a IndexManifestSuccessorRequestV1<'a>) -> Result<&'a [u8], IndexBatchApplicationErrorV1> {
  let parent =
    request.parent_manifest_key.ok_or_else(|| manifest_closure_error("successor manifest is missing its parent manifest key"))?;
  if parent.len() != request.hash_algorithm.hash_length() || parent.iter().all(|byte| *byte == 0) {
    return Err(manifest_closure_error("successor parent manifest key disagrees with the hash profile"));
  }
  Ok(parent)
}

fn final_next_page_id(request: &IndexManifestSuccessorRequestV1<'_>, source_next_page_id: u64) -> u64 {
  request.role_summaries.last().map_or(source_next_page_id, |summary| summary.next_page_id)
}

fn source_next_page_id(source: &IndexManifestBodyV1<'_>) -> u64 {
  match source {
    IndexManifestBodyV1::ScopeCatalog(_) | IndexManifestBodyV1::FieldNvt(_) => 0,
    IndexManifestBodyV1::ValueStore(body) => body.next_page_id,
    IndexManifestBodyV1::FieldIndex(body) => body.next_page_id,
  }
}

fn source_role_root_matches(source: &IndexManifestBodyV1<'_>, role: OrderedIndexRoleV1, expected: Option<&[u8]>) -> bool {
  match (source, role) {
    (IndexManifestBodyV1::ScopeCatalog(body), OrderedIndexRoleV1::ScopeOrdinal) => body.ordinal_directory_root == expected,
    (IndexManifestBodyV1::ScopeCatalog(body), OrderedIndexRoleV1::ScopeReverse) => body.reverse_directory_root == expected,
    (IndexManifestBodyV1::ValueStore(body), OrderedIndexRoleV1::Value) => body.value_directory_root == expected,
    (IndexManifestBodyV1::ValueStore(body), OrderedIndexRoleV1::ValueDocumentState) => body.document_state_directory_root == expected,
    (IndexManifestBodyV1::FieldIndex(body), OrderedIndexRoleV1::Posting) => body.posting_directory_root == expected,
    (IndexManifestBodyV1::FieldIndex(body), OrderedIndexRoleV1::IndexDocumentState) => body.document_state_directory_root == expected,
    _ => false,
  }
}

fn apply_counter_delta(current: u64, additions: u64, removals: u64, context: &'static str) -> Result<u64, IndexBatchApplicationErrorV1> {
  current
    .checked_sub(removals)
    .and_then(|remaining| remaining.checked_add(additions))
    .ok_or_else(|| manifest_closure_error(format!("{context} underflowed or overflowed")))
}

fn checked_increment(value: u64, context: &'static str) -> Result<u64, IndexBatchApplicationErrorV1> {
  value.checked_add(1).ok_or_else(|| manifest_closure_error(format!("{context} overflowed")))
}

fn role_bit(role: OrderedIndexRoleV1) -> u16 {
  1u16 << role.id()
}

fn live_transition_role_mask(owner_class: IndexMembershipOwnerClassV1) -> u16 {
  match owner_class {
    IndexMembershipOwnerClassV1::ScopeCatalog => role_bit(OrderedIndexRoleV1::ScopeOrdinal) | role_bit(OrderedIndexRoleV1::ScopeReverse),
    IndexMembershipOwnerClassV1::ValueStore => role_bit(OrderedIndexRoleV1::Value),
    IndexMembershipOwnerClassV1::FieldIndex => role_bit(OrderedIndexRoleV1::Posting),
  }
}

fn unindexable_transition_role_mask(owner_class: IndexMembershipOwnerClassV1) -> Result<u16, IndexBatchApplicationErrorV1> {
  match owner_class {
    IndexMembershipOwnerClassV1::ValueStore => Ok(role_bit(OrderedIndexRoleV1::ValueDocumentState)),
    IndexMembershipOwnerClassV1::FieldIndex => Ok(role_bit(OrderedIndexRoleV1::IndexDocumentState)),
    IndexMembershipOwnerClassV1::ScopeCatalog => Err(manifest_closure_error("ScopeCatalog membership cannot be unindexable")),
  }
}

fn manifest_owner_class(body: &IndexManifestBodyV1<'_>) -> Result<IndexMembershipOwnerClassV1, IndexBatchApplicationErrorV1> {
  match body {
    IndexManifestBodyV1::ScopeCatalog(_) => Ok(IndexMembershipOwnerClassV1::ScopeCatalog),
    IndexManifestBodyV1::ValueStore(_) => Ok(IndexMembershipOwnerClassV1::ValueStore),
    IndexManifestBodyV1::FieldIndex(_) => Ok(IndexMembershipOwnerClassV1::FieldIndex),
    IndexManifestBodyV1::FieldNvt(_) => Err(manifest_closure_error("FieldNvt manifests have no ordered membership owner")),
  }
}

fn validate_manifest_mutations(
  request: &IndexManifestSuccessorRequestV1<'_>,
  owner_id: &[u8],
  owner_class: IndexMembershipOwnerClassV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<ManifestMutationClosureV1, IndexBatchApplicationErrorV1> {
  validate_transition_count(request.transitions.len())?;
  let mut observed_shapes_by_transition = Vec::new();
  observed_shapes_by_transition
    .try_reserve_exact(request.transitions.len())
    .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("transition role closure reservation failed: {error}")))?;
  observed_shapes_by_transition.resize(request.transitions.len(), 0u16);
  let mut commitments: [Option<IndexMutationCommitmentV1>; 8] = std::array::from_fn(|_| None);
  let mut role_mask = 0u16;
  let mut previous_role = 0u8;
  let mut previous_order_key: Option<&[u8]> = None;
  for (mutation_index, mutation) in request.mutations.iter().enumerate() {
    if mutation_index > 0 && mutation_index % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    if mutation.index_id() != owner_id
      || mutation.role().owner_class() != owner_class.id()
      || mutation.publication_sequence() == 0
      || mutation.publication_sequence() > request.coverage.coverage_publication_sequence
      || mutation.operation_id() == [0; 16]
      || mutation.role() == OrderedIndexRoleV1::NvtTile
    {
      return Err(manifest_closure_error("mutation owner, role, or publication identity disagrees with the successor manifest"));
    }
    let role = mutation.role().id();
    if role < previous_role || (role == previous_role && previous_order_key.is_some_and(|key| key >= mutation.order_key())) {
      return Err(manifest_closure_error("manifest mutations are not in strict canonical owner/role/order-key order"));
    }
    let decoded = decode_ordered_record(mutation.encoded_record(), request.hash_algorithm, mutation.role())?;
    let canonical_order_key = ordered_record_order_key(&decoded)?;
    if canonical_order_key != mutation.order_key() {
      return Err(manifest_closure_error("mutation order key does not match its canonical ordered record"));
    }
    if mutation.operation() == IndexMutationOperationV1::RemoveExisting
      && (mutation.role() != OrderedIndexRoleV1::ScopeReverse || decoded.tombstone)
    {
      return Err(manifest_closure_error("remove-existing is legal only for a live ScopeReverse record"));
    }
    let transition_index =
      match request.transitions.binary_search_by_key(&decoded.document_ordinal, |transition| transition.document_ordinal()) {
        Ok(index) => index,
        Err(insertion_index) => {
          return Err(manifest_closure_error(format!(
            "mutation has no exact owner/document membership transition; canonical insertion index is {insertion_index}",
          )));
        }
      };
    let transition = &request.transitions[transition_index];
    if transition.publication_sequence() != mutation.publication_sequence() || transition.operation_id() != mutation.operation_id() {
      return Err(manifest_closure_error("mutation publication identity disagrees with its owner/document transition"));
    }
    validate_mutation_transition_shape(mutation, &decoded, transition)?;
    let kind = manifest_mutation_kind(mutation, decoded.tombstone);
    let commitment = commitments
      .get_mut(usize::from(mutation.role().id()))
      .ok_or_else(|| manifest_closure_error("ordered-record role has no mutation-commitment slot"))?;
    commitment.get_or_insert_with(|| IndexMutationCommitmentV1::new(request.hash_algorithm)).push(kind)?;
    role_mask |= role_bit(mutation.role());
    observed_shapes_by_transition[transition_index] |= mutation_shape_bit(mutation, decoded.tombstone)?;
    previous_role = role;
    previous_order_key = Some(mutation.order_key());
  }
  if request.transitions.iter().zip(observed_shapes_by_transition).try_fold(false, |missing, (transition, observed)| {
    let required = transition_shape_mask(owner_class, transition.before(), transition.after())?;
    Ok::<bool, IndexBatchApplicationErrorV1>(missing || required & !observed != 0)
  })? {
    return Err(manifest_closure_error("one or more membership transitions are missing their exact ordered-record mutation shapes"));
  }
  Ok(ManifestMutationClosureV1 { role_mask, commitments: commitments.map(|commitment| commitment.map(IndexMutationCommitmentV1::finish)) })
}

fn manifest_mutation_kind<'a>(mutation: &'a PublishedIndexMutationV1, tombstone: bool) -> OrderedPageMutationKindV1<'a> {
  if mutation.operation() == IndexMutationOperationV1::RemoveExisting {
    OrderedPageMutationKindV1::RemoveExisting(mutation.encoded_record())
  } else if tombstone {
    OrderedPageMutationKindV1::TombstoneExisting(mutation.encoded_record())
  } else {
    OrderedPageMutationKindV1::UpsertLive(mutation.encoded_record())
  }
}

const MUTATION_SHAPE_LIVE_UPSERT: u8 = 0;
const MUTATION_SHAPE_TOMBSTONE_UPSERT: u8 = 1;
const MUTATION_SHAPE_REMOVE_EXISTING: u8 = 2;

fn mutation_shape(role: OrderedIndexRoleV1, shape: u8) -> Result<u16, IndexBatchApplicationErrorV1> {
  let bit = match (role, shape) {
    (OrderedIndexRoleV1::ScopeOrdinal, MUTATION_SHAPE_LIVE_UPSERT) => 0,
    (OrderedIndexRoleV1::ScopeOrdinal, MUTATION_SHAPE_TOMBSTONE_UPSERT) => 1,
    (OrderedIndexRoleV1::ScopeReverse, MUTATION_SHAPE_LIVE_UPSERT) => 2,
    (OrderedIndexRoleV1::ScopeReverse, MUTATION_SHAPE_REMOVE_EXISTING) => 3,
    (OrderedIndexRoleV1::Value, MUTATION_SHAPE_LIVE_UPSERT) => 4,
    (OrderedIndexRoleV1::Value, MUTATION_SHAPE_TOMBSTONE_UPSERT) => 5,
    (OrderedIndexRoleV1::ValueDocumentState, MUTATION_SHAPE_LIVE_UPSERT) => 6,
    (OrderedIndexRoleV1::ValueDocumentState, MUTATION_SHAPE_TOMBSTONE_UPSERT) => 7,
    (OrderedIndexRoleV1::Posting, MUTATION_SHAPE_LIVE_UPSERT) => 8,
    (OrderedIndexRoleV1::Posting, MUTATION_SHAPE_TOMBSTONE_UPSERT) => 9,
    (OrderedIndexRoleV1::IndexDocumentState, MUTATION_SHAPE_LIVE_UPSERT) => 10,
    (OrderedIndexRoleV1::IndexDocumentState, MUTATION_SHAPE_TOMBSTONE_UPSERT) => 11,
    _ => return Err(manifest_closure_error("ordered-record role has an impossible membership mutation shape")),
  };
  Ok(1u16 << bit)
}

fn mutation_shape_bit(mutation: &PublishedIndexMutationV1, tombstone: bool) -> Result<u16, IndexBatchApplicationErrorV1> {
  let shape = match mutation.operation() {
    IndexMutationOperationV1::RemoveExisting => MUTATION_SHAPE_REMOVE_EXISTING,
    IndexMutationOperationV1::Upsert if tombstone => MUTATION_SHAPE_TOMBSTONE_UPSERT,
    IndexMutationOperationV1::Upsert => MUTATION_SHAPE_LIVE_UPSERT,
  };
  mutation_shape(mutation.role(), shape)
}

fn validate_mutation_transition_shape(
  mutation: &PublishedIndexMutationV1,
  decoded: &super::index_page::OrderedRecordV1<'_>,
  transition: &PublishedIndexMembershipTransitionV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  let before = transition.before();
  let after = transition.after();
  let valid = match mutation.role() {
    OrderedIndexRoleV1::ScopeOrdinal => mutation.operation() == IndexMutationOperationV1::Upsert && decoded.tombstone != after.live,
    OrderedIndexRoleV1::ScopeReverse => match mutation.operation() {
      IndexMutationOperationV1::RemoveExisting => before.live,
      IndexMutationOperationV1::Upsert => after.live && !decoded.tombstone,
    },
    OrderedIndexRoleV1::Value | OrderedIndexRoleV1::Posting => {
      mutation.operation() == IndexMutationOperationV1::Upsert
        && if !before.live && after.live {
          !decoded.tombstone
        } else if before.live && !after.live {
          decoded.tombstone
        } else {
          before.live && after.live
        }
    }
    OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => {
      mutation.operation() == IndexMutationOperationV1::Upsert
        && if after.unindexable {
          !decoded.tombstone
        } else if before.unindexable {
          decoded.tombstone
        } else {
          false
        }
    }
    OrderedIndexRoleV1::NvtTile => false,
  };
  if !valid {
    return Err(manifest_closure_error("ordered-record mutation shape contradicts its exact owner/document membership transition"));
  }
  Ok(())
}

fn transition_shape_mask(
  owner_class: IndexMembershipOwnerClassV1,
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
) -> Result<u16, IndexBatchApplicationErrorV1> {
  let mut shapes = 0u16;
  match owner_class {
    IndexMembershipOwnerClassV1::ScopeCatalog => {
      if after.live {
        shapes |= mutation_shape(OrderedIndexRoleV1::ScopeOrdinal, MUTATION_SHAPE_LIVE_UPSERT)?;
        shapes |= mutation_shape(OrderedIndexRoleV1::ScopeReverse, MUTATION_SHAPE_LIVE_UPSERT)?;
      } else if before.live {
        shapes |= mutation_shape(OrderedIndexRoleV1::ScopeOrdinal, MUTATION_SHAPE_TOMBSTONE_UPSERT)?;
        shapes |= mutation_shape(OrderedIndexRoleV1::ScopeReverse, MUTATION_SHAPE_REMOVE_EXISTING)?;
      }
    }
    IndexMembershipOwnerClassV1::ValueStore => {
      shapes |= semantic_transition_shape_mask(before, after, OrderedIndexRoleV1::Value, OrderedIndexRoleV1::ValueDocumentState)?;
    }
    IndexMembershipOwnerClassV1::FieldIndex => {
      shapes |= semantic_transition_shape_mask(before, after, OrderedIndexRoleV1::Posting, OrderedIndexRoleV1::IndexDocumentState)?;
    }
  }
  Ok(shapes)
}

fn semantic_transition_shape_mask(
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
  live_role: OrderedIndexRoleV1,
  unindexable_role: OrderedIndexRoleV1,
) -> Result<u16, IndexBatchApplicationErrorV1> {
  let mut shapes = 0u16;
  if after.live {
    shapes |= mutation_shape(live_role, MUTATION_SHAPE_LIVE_UPSERT)?;
  } else if before.live {
    shapes |= mutation_shape(live_role, MUTATION_SHAPE_TOMBSTONE_UPSERT)?;
  }
  if after.unindexable {
    shapes |= mutation_shape(unindexable_role, MUTATION_SHAPE_LIVE_UPSERT)?;
  } else if before.unindexable {
    shapes |= mutation_shape(unindexable_role, MUTATION_SHAPE_TOMBSTONE_UPSERT)?;
  }
  Ok(shapes)
}

fn validate_manifest_transitions(
  request: &IndexManifestSuccessorRequestV1<'_>,
  owner_id: &[u8],
  owner_class: IndexMembershipOwnerClassV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<ManifestTransitionDeltasV1, IndexBatchApplicationErrorV1> {
  validate_transition_count(request.transitions.len())?;
  let mut deltas = ManifestTransitionDeltasV1::default();
  for (transition_index, transition) in request.transitions.iter().enumerate() {
    if transition_index > 0 && transition_index % 4_096 == 0 {
      check_cancelled(is_cancelled)?;
    }
    if transition.owner_id() != owner_id
      || transition.owner_class() != owner_class
      || transition.publication_sequence() == 0
      || transition.publication_sequence() > request.coverage.coverage_publication_sequence
      || transition.operation_id() == [0; 16]
      || transition.document_ordinal() == 0
      || transition.document_ordinal() <= deltas.maximum_document_ordinal
      || transition.before().live && transition.before().unindexable
      || transition.after().live && transition.after().unindexable
      || owner_class == IndexMembershipOwnerClassV1::ScopeCatalog && (transition.before().unindexable || transition.after().unindexable)
    {
      return Err(manifest_closure_error("membership transitions are not a strict canonical closure for the manifest owner"));
    }
    let transition_roles = transition_role_mask(owner_class, transition.before(), transition.after())?;
    if transition.before().live != transition.after().live {
      if transition.after().live {
        deltas.live_additions = checked_increment(deltas.live_additions, "live membership additions")?;
      } else {
        deltas.live_removals = checked_increment(deltas.live_removals, "live membership removals")?;
      }
    }
    if transition.before().unindexable != transition.after().unindexable {
      if transition.after().unindexable {
        deltas.unindexable_additions = checked_increment(deltas.unindexable_additions, "unindexable membership additions")?;
      } else {
        deltas.unindexable_removals = checked_increment(deltas.unindexable_removals, "unindexable membership removals")?;
      }
    }
    deltas.required_roles |= transition_roles;
    deltas.maximum_document_ordinal = transition.document_ordinal();
  }
  Ok(deltas)
}

fn transition_role_mask(
  owner_class: IndexMembershipOwnerClassV1,
  before: super::index_coordinator::IndexMembershipStateV1,
  after: super::index_coordinator::IndexMembershipStateV1,
) -> Result<u16, IndexBatchApplicationErrorV1> {
  let mut roles = 0u16;
  if before.live != after.live {
    roles |= live_transition_role_mask(owner_class);
  }
  if before.unindexable != after.unindexable {
    roles |= unindexable_transition_role_mask(owner_class)?;
  }
  Ok(roles)
}

fn validate_transition_count(count: usize) -> Result<(), IndexBatchApplicationErrorV1> {
  if count > INDEX_BATCH_MAXIMUM_TRANSITIONS_V1 {
    return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "index_batch_transition_count",
      format!("{count} transitions exceed the {INDEX_BATCH_MAXIMUM_TRANSITIONS_V1}-transition hard cap"),
    )));
  }
  Ok(())
}

fn validate_manifest_role_summaries(
  request: &IndexManifestSuccessorRequestV1<'_>,
  source: &IndexManifestBodyV1<'_>,
  owner_id: &[u8],
  owner_class: IndexMembershipOwnerClassV1,
  required_roles: u16,
  mutation_closure: &ManifestMutationClosureV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  let mut observed_roles = 0u16;
  let mut previous_role = 0u8;
  let mut next_page_id = source_next_page_id(source);
  for summary in request.role_summaries {
    if summary.owner_id != owner_id
      || summary.role.owner_class() != owner_class.id()
      || summary.role == OrderedIndexRoleV1::NvtTile
      || summary.generation != request.generation
      || summary.role.id() <= previous_role
      || !source_role_root_matches(source, summary.role, summary.source_root_key.as_deref())
    {
      return Err(manifest_closure_error("COW role summary owner, role, generation, order, or source root is invalid"));
    }
    if summary.mutation_commitment.as_deref() != mutation_closure.commitment(summary.role) {
      return Err(manifest_closure_error("COW role summary is not bound to the exact frozen-batch mutation set"));
    }
    if summary.role.uses_page_id() {
      if summary.initial_next_page_id != next_page_id || summary.next_page_id < summary.initial_next_page_id {
        return Err(manifest_closure_error("COW role summaries do not form one nonoverlapping PageID allocation chain"));
      }
      if summary.root_key.is_some()
        && (summary.minimum_page_id == 0
          || summary.maximum_page_id >= summary.next_page_id
          || summary.first_page_id() < summary.minimum_page_id
          || summary.first_page_id() > summary.maximum_page_id
          || summary.last_page_id() < summary.minimum_page_id
          || summary.last_page_id() > summary.maximum_page_id)
      {
        return Err(manifest_closure_error("COW role summary PageID bounds exceed the successor high-water"));
      }
      if summary.root_key.is_none() && (summary.first_page_id() != 0 || summary.last_page_id() != 0) {
        return Err(manifest_closure_error("retired COW role summary carries logical PageID endpoints"));
      }
      next_page_id = summary.next_page_id;
    } else if summary.initial_next_page_id != 0
      || summary.next_page_id != 0
      || summary.minimum_page_id != 0
      || summary.maximum_page_id != 0
      || summary.first_page_id() != 0
      || summary.last_page_id() != 0
    {
      return Err(manifest_closure_error("non-PageID COW role summary carries PageID state"));
    }
    observed_roles |= role_bit(summary.role);
    previous_role = summary.role.id();
  }
  if observed_roles != required_roles {
    return Err(manifest_closure_error("mutation and transition roles do not have one exact COW summary each"));
  }
  Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBatchArtifactOverlayLimitsV1 {
  maximum_artifacts: usize,
  maximum_retained_bytes: usize,
}

impl IndexBatchArtifactOverlayLimitsV1 {
  pub fn new(maximum_artifacts: usize, maximum_retained_bytes: usize) -> Result<Self, IndexBatchApplicationErrorV1> {
    if maximum_artifacts == 0 || maximum_artifacts > INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "artifact count {maximum_artifacts} is outside 1..={INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1}"
      )));
    }
    if maximum_retained_bytes == 0 || maximum_retained_bytes > INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "retained bytes {maximum_retained_bytes} are outside 1..={INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1}"
      )));
    }
    Ok(Self { maximum_artifacts, maximum_retained_bytes })
  }

  pub fn maximum_artifacts(self) -> usize {
    self.maximum_artifacts
  }

  pub fn maximum_retained_bytes(self) -> usize {
    self.maximum_retained_bytes
  }
}

impl Default for IndexBatchArtifactOverlayLimitsV1 {
  fn default() -> Self {
    Self { maximum_artifacts: INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, maximum_retained_bytes: INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1 }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedPagePathLookupLimitsV1 {
  maximum_directory_depth: usize,
  maximum_input_bytes: usize,
}

impl OrderedPagePathLookupLimitsV1 {
  pub fn new(maximum_directory_depth: usize, maximum_input_bytes: usize) -> Result<Self, IndexBatchApplicationErrorV1> {
    if maximum_directory_depth == 0 || maximum_directory_depth > INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "directory depth {maximum_directory_depth} is outside 1..={INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1}"
      )));
    }
    if maximum_input_bytes == 0 || maximum_input_bytes > INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "path input bytes {maximum_input_bytes} are outside 1..={INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1}"
      )));
    }
    Ok(Self { maximum_directory_depth, maximum_input_bytes })
  }

  pub fn maximum_directory_depth(self) -> usize {
    self.maximum_directory_depth
  }

  pub fn maximum_input_bytes(self) -> usize {
    self.maximum_input_bytes
  }
}

impl Default for OrderedPagePathLookupLimitsV1 {
  fn default() -> Self {
    Self { maximum_directory_depth: INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1, maximum_input_bytes: INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1 }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct OrderedPagePathLookupRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub root_key: &'a [u8],
  pub owner_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub order_key: &'a [u8],
  pub load_posting_successor: bool,
  pub limits: OrderedPagePathLookupLimitsV1,
}

#[derive(Clone, Debug)]
enum RetainedArtifactBytesV1 {
  Prepared(Arc<EncodedImmutableIndexArtifactV1>),
  Source(Arc<Vec<u8>>),
}

impl RetainedArtifactBytesV1 {
  fn value(&self) -> &[u8] {
    match self {
      Self::Prepared(artifact) => &artifact.value,
      Self::Source(value) => value,
    }
  }
}

#[derive(Debug)]
pub struct SparseIndexArtifactOverlayV1 {
  hash_algorithm: HashAlgorithm,
  limits: IndexBatchArtifactOverlayLimitsV1,
  artifacts: Vec<Arc<EncodedImmutableIndexArtifactV1>>,
  by_key: HashMap<Vec<u8>, usize>,
  retained_bytes: usize,
}

impl SparseIndexArtifactOverlayV1 {
  pub fn new(hash_algorithm: HashAlgorithm, limits: IndexBatchArtifactOverlayLimitsV1) -> Result<Self, IndexBatchApplicationErrorV1> {
    IndexBatchArtifactOverlayLimitsV1::new(limits.maximum_artifacts(), limits.maximum_retained_bytes())?;
    Ok(Self { hash_algorithm, limits, artifacts: Vec::new(), by_key: HashMap::new(), retained_bytes: 0 })
  }

  pub fn insert(&mut self, artifact: EncodedImmutableIndexArtifactV1) -> Result<bool, IndexBatchApplicationErrorV1> {
    let decoded = decode_immutable_index_artifact(
      &artifact.value,
      self.hash_algorithm,
      ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length(),
    )?;
    let kind = ImmutableIndexArtifactKindV1::from_u16(decoded.kind).ok_or_else(|| {
      IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "index_batch_overlay_kind",
        "prepared immutable artifact kind is unknown",
      ))
    })?;
    if decoded.key != artifact.key || artifact.value.len() > kind.maximum_encoded_length() {
      return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "index_batch_overlay_identity",
        "prepared immutable artifact key or kind is invalid",
      )));
    }
    if let Some(index) = self.by_key.get(artifact.key.as_slice()) {
      if self.artifacts[*index].value == artifact.value {
        return Ok(false);
      }
      return Err(IndexBatchApplicationErrorV1::OverlayConflict);
    }
    if self.artifacts.len() >= self.limits.maximum_artifacts() {
      return Err(IndexBatchApplicationErrorV1::OverlayCount);
    }
    let retained = checked_overlay_artifact_bytes(&artifact)?;
    let projected = self.retained_bytes.checked_add(retained).ok_or(IndexBatchApplicationErrorV1::OverlayBytes)?;
    if projected > self.limits.maximum_retained_bytes() {
      return Err(IndexBatchApplicationErrorV1::OverlayBytes);
    }
    self
      .artifacts
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor artifact reservation failed: {error}")))?;
    self
      .by_key
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor lookup reservation failed: {error}")))?;
    let index = self.artifacts.len();
    let key = clone_bytes(&artifact.key, "successor lookup key")?;
    self.artifacts.push(Arc::new(artifact));
    self.by_key.insert(key, index);
    self.retained_bytes = projected;
    Ok(true)
  }

  pub fn artifact_count(&self) -> usize {
    self.artifacts.len()
  }

  pub fn retained_bytes(&self) -> usize {
    self.retained_bytes
  }

  pub fn prepared_artifacts(&self) -> impl ExactSizeIterator<Item = &EncodedImmutableIndexArtifactV1> {
    self.artifacts.iter().map(Arc::as_ref)
  }

  fn get(&self, key: &[u8]) -> Option<RetainedArtifactBytesV1> {
    self.by_key.get(key).map(|index| RetainedArtifactBytesV1::Prepared(Arc::clone(&self.artifacts[*index])))
  }
}

#[derive(Debug)]
pub struct LoadedOrderedPagePathV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  page: RetainedArtifactBytesV1,
  next_posting: Option<LoadedPostingSuccessorV1>,
  input_bytes: usize,
}

impl LoadedOrderedPagePathV1 {
  pub fn directory_count(&self) -> usize {
    self.directories.len()
  }

  pub fn directory(&self, index: usize) -> Option<&[u8]> {
    self.directories.get(index).map(RetainedArtifactBytesV1::value)
  }

  pub fn page(&self) -> &[u8] {
    self.page.value()
  }

  pub fn next_posting_page(&self) -> Option<&[u8]> {
    self.next_posting.as_ref().map(|next| next.page.value())
  }

  pub fn next_directory_count(&self) -> usize {
    self.next_posting.as_ref().map_or(0, |next| next.directories.len())
  }

  pub fn next_directory(&self, index: usize) -> Option<&[u8]> {
    self.next_posting.as_ref().and_then(|next| next.directories.get(index)).map(RetainedArtifactBytesV1::value)
  }

  pub fn input_bytes(&self) -> usize {
    self.input_bytes
  }
}

#[derive(Debug)]
struct LoadedPostingSuccessorV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  page: RetainedArtifactBytesV1,
}

#[derive(Debug)]
struct TraversedPagePathV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  selected_entries: Vec<usize>,
  page: RetainedArtifactBytesV1,
}

#[derive(Debug)]
struct OwnedDirectoryEntryExpectationV1 {
  lower_fence: Vec<u8>,
  upper_fence: Vec<u8>,
  child_hash: Vec<u8>,
  child_generation: u64,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: u64,
  minimum_page_id: u64,
  maximum_page_id: u64,
}

impl OwnedDirectoryEntryExpectationV1 {
  fn from_entry(entry: &ArtifactDirectoryEntryV1<'_>) -> Result<Self, IndexBatchApplicationErrorV1> {
    Ok(Self {
      lower_fence: clone_bytes(entry.lower_fence, "directory child lower fence")?,
      upper_fence: clone_bytes(entry.upper_fence, "directory child upper fence")?,
      child_hash: clone_bytes(entry.child_hash, "directory child hash")?,
      child_generation: entry.child_generation,
      live_count: entry.live_count,
      tombstone_count: entry.tombstone_count,
      page_count: entry.page_count,
      logical_bytes: entry.logical_bytes,
      minimum_page_id: entry.minimum_page_id,
      maximum_page_id: entry.maximum_page_id,
    })
  }
}

#[derive(Debug)]
struct PathInputBudgetV1 {
  maximum_bytes: usize,
  retained_bytes: usize,
  observed_keys: Vec<Vec<u8>>,
}

impl PathInputBudgetV1 {
  fn new(maximum_bytes: usize) -> Self {
    Self { maximum_bytes, retained_bytes: 0, observed_keys: Vec::new() }
  }

  fn observe(&mut self, key: &[u8], value_length: usize) -> Result<(), IndexBatchApplicationErrorV1> {
    if self.observed_keys.iter().any(|observed| observed == key) {
      return Ok(());
    }
    let next = self
      .retained_bytes
      .checked_add(key.len())
      .and_then(|bytes| bytes.checked_add(value_length))
      .ok_or_else(|| IndexBatchApplicationErrorV1::InvalidLimits("path retained-byte count overflowed".to_string()))?;
    if next > self.maximum_bytes {
      return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "index_batch_path_input_bytes",
        format!("{next} path bytes exceed the {}-byte operation cap", self.maximum_bytes),
      )));
    }
    self
      .observed_keys
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("path key reservation failed: {error}")))?;
    self.observed_keys.push(clone_bytes(key, "path budget key")?);
    self.retained_bytes = next;
    Ok(())
  }

  fn remaining_bytes(&self, key: &[u8]) -> usize {
    if self.observed_keys.iter().any(|observed| observed == key) {
      return self.maximum_bytes;
    }
    self.maximum_bytes.saturating_sub(self.retained_bytes).saturating_sub(key.len())
  }
}

pub fn load_ordered_page_path_v1(
  request: &OrderedPagePathLookupRequestV1<'_>,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<LoadedOrderedPagePathV1, IndexBatchApplicationErrorV1> {
  validate_lookup_request(request, overlay)?;
  check_cancelled(is_cancelled)?;
  let mut budget = PathInputBudgetV1::new(request.limits.maximum_input_bytes());
  let traversed = descend_to_order_key(request, overlay, source, is_cancelled, &mut budget)?;
  let next_posting = if request.role == OrderedIndexRoleV1::Posting && request.load_posting_successor {
    load_posting_successor(request, &traversed, overlay, source, is_cancelled, &mut budget)?
  } else {
    None
  };
  Ok(LoadedOrderedPagePathV1 { directories: traversed.directories, page: traversed.page, next_posting, input_bytes: budget.retained_bytes })
}

fn descend_to_order_key(
  request: &OrderedPagePathLookupRequestV1<'_>,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<TraversedPagePathV1, IndexBatchApplicationErrorV1> {
  let mut directories = Vec::new();
  let mut selected_entries = Vec::new();
  let mut current_key = clone_bytes(request.root_key, "root key")?;
  let mut expected_level = None;
  let mut expected_child = None;
  loop {
    check_cancelled(is_cancelled)?;
    if directories.len() >= request.limits.maximum_directory_depth() {
      return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "index_batch_path_depth",
        "ordered-page path exceeds its directory-depth limit",
      )));
    }
    let retained = load_artifact(&current_key, overlay, source, is_cancelled, budget)?;
    let directory = decode_artifact_directory(retained.value(), request.hash_algorithm)?;
    validate_directory_identity(&directory, &current_key, request.owner_id, request.role, expected_level)?;
    if let Some(expected) = expected_child.as_ref() {
      validate_directory_child(&directory, expected)?;
    }
    let selected = select_directory_entry(&directory, request.hash_algorithm, request.role, request.order_key)?;
    let entry = &directory.entries[selected];
    directories
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("directory path reservation failed: {error}")))?;
    selected_entries
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("directory index reservation failed: {error}")))?;
    directories.push(retained.clone());
    selected_entries.push(selected);
    if directory.level == 0 {
      let page = load_artifact(entry.child_hash, overlay, source, is_cancelled, budget)?;
      let decoded_page = decode_ordered_page(page.value(), request.hash_algorithm)?;
      validate_leaf_page(&decoded_page, entry, request.owner_id, request.role)?;
      return Ok(TraversedPagePathV1 { directories, selected_entries, page });
    }
    current_key = clone_bytes(entry.child_hash, "directory child key")?;
    expected_level = Some(directory.level - 1);
    expected_child = Some(OwnedDirectoryEntryExpectationV1::from_entry(entry)?);
  }
}

fn load_posting_successor(
  request: &OrderedPagePathLookupRequestV1<'_>,
  traversed: &TraversedPagePathV1,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<Option<LoadedPostingSuccessorV1>, IndexBatchApplicationErrorV1> {
  let current_page = decode_ordered_page(traversed.page.value(), request.hash_algorithm)?;
  let successor = locate_logical_successor(request, traversed, overlay, source, is_cancelled, budget)?;
  match successor {
    None if current_page.next_page_id == 0 => Ok(None),
    None => Err(closure_error("posting page names a successor absent from its artifact directory")),
    Some(_successor) if current_page.next_page_id == 0 => {
      Err(closure_error("posting artifact directory has a successor for a terminal posting page"))
    }
    Some(successor) => {
      let next = decode_ordered_page(successor.page.value(), request.hash_algorithm)?;
      if next.page_id != current_page.next_page_id {
        return Err(closure_error("posting artifact-directory successor does not match the page next-link"));
      }
      validate_posting_page_link(&current_page, &next, request.hash_algorithm)?;
      Ok(Some(successor))
    }
  }
}

fn locate_logical_successor(
  request: &OrderedPagePathLookupRequestV1<'_>,
  traversed: &TraversedPagePathV1,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<Option<LoadedPostingSuccessorV1>, IndexBatchApplicationErrorV1> {
  for directory_index in (0..traversed.directories.len()).rev() {
    let directory = decode_artifact_directory(traversed.directories[directory_index].value(), request.hash_algorithm)?;
    let selected = traversed.selected_entries[directory_index];
    let Some(next_index) = selected.checked_add(1).filter(|index| *index < directory.entries.len()) else {
      continue;
    };
    let mut successor_directories = traversed.directories[..=directory_index].to_vec();
    let entry = &directory.entries[next_index];
    if directory.level == 0 {
      let page = load_artifact(entry.child_hash, overlay, source, is_cancelled, budget)?;
      let decoded = decode_ordered_page(page.value(), request.hash_algorithm)?;
      validate_leaf_page(&decoded, entry, request.owner_id, request.role)?;
      return Ok(Some(LoadedPostingSuccessorV1 { directories: successor_directories, page }));
    }

    let mut current_key = clone_bytes(entry.child_hash, "successor directory key")?;
    let mut expected_level = directory.level - 1;
    let mut expected_child = OwnedDirectoryEntryExpectationV1::from_entry(entry)?;
    loop {
      check_cancelled(is_cancelled)?;
      if successor_directories.len() >= request.limits.maximum_directory_depth() {
        return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
          MalformedInputClass::AllocationAmplification,
          "index_batch_path_depth",
          "posting successor path exceeds its directory-depth limit",
        )));
      }
      let retained = load_artifact(&current_key, overlay, source, is_cancelled, budget)?;
      let child = decode_artifact_directory(retained.value(), request.hash_algorithm)?;
      validate_directory_identity(&child, &current_key, request.owner_id, request.role, Some(expected_level))?;
      validate_directory_child(&child, &expected_child)?;
      successor_directories
        .try_reserve(1)
        .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor path reservation failed: {error}")))?;
      successor_directories.push(retained.clone());
      let first = child.entries.first().ok_or_else(|| closure_error("posting successor directory is empty"))?;
      if child.level == 0 {
        let page = load_artifact(first.child_hash, overlay, source, is_cancelled, budget)?;
        let decoded = decode_ordered_page(page.value(), request.hash_algorithm)?;
        validate_leaf_page(&decoded, first, request.owner_id, request.role)?;
        return Ok(Some(LoadedPostingSuccessorV1 { directories: successor_directories, page }));
      }
      current_key = clone_bytes(first.child_hash, "successor child key")?;
      expected_level = child.level - 1;
      expected_child = OwnedDirectoryEntryExpectationV1::from_entry(first)?;
    }
  }
  Ok(None)
}

fn load_artifact(
  key: &[u8],
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<RetainedArtifactBytesV1, IndexBatchApplicationErrorV1> {
  check_cancelled(is_cancelled)?;
  let retained = if let Some(retained) = overlay.get(key) {
    retained
  } else {
    let value = source.read_immutable_artifact(key, budget.remaining_bytes(key)).map_err(|error| map_source_error(key, error))?;
    check_cancelled(is_cancelled)?;
    RetainedArtifactBytesV1::Source(Arc::new(value))
  };
  budget.observe(key, retained.value().len())?;
  Ok(retained)
}

fn validate_lookup_request(
  request: &OrderedPagePathLookupRequestV1<'_>,
  overlay: &SparseIndexArtifactOverlayV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  OrderedPagePathLookupLimitsV1::new(request.limits.maximum_directory_depth(), request.limits.maximum_input_bytes())?;
  let hash_width = request.hash_algorithm.hash_length();
  if overlay.hash_algorithm != request.hash_algorithm
    || request.root_key.len() != hash_width
    || request.root_key.iter().all(|byte| *byte == 0)
    || request.owner_id.len() != hash_width
    || request.owner_id.iter().all(|byte| *byte == 0)
    || request.role == OrderedIndexRoleV1::NvtTile
  {
    return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "index_batch_lookup_identity",
      "lookup hash profile, root, owner, or ordered role is invalid",
    )));
  }
  compare_order_keys(request.hash_algorithm, request.role, request.order_key, request.order_key)?;
  Ok(())
}

fn validate_directory_identity(
  directory: &ArtifactDirectoryNodeV1<'_>,
  expected_key: &[u8],
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  expected_level: Option<u16>,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if directory.key != expected_key
    || directory.owner_id != owner_id
    || directory.role != role
    || expected_level.is_some_and(|level| directory.level != level)
  {
    return Err(closure_error("artifact directory key, owner, role, or level disagrees with its selected path"));
  }
  Ok(())
}

fn validate_leaf_page(
  page: &OrderedPageV1<'_>,
  descriptor: &ArtifactDirectoryEntryV1<'_>,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if page.key != descriptor.child_hash
    || page.owner_id != owner_id
    || page.role != role
    || page.generation != descriptor.child_generation
    || page.lower_fence != descriptor.lower_fence
    || page.upper_fence != descriptor.upper_fence
    || u64::from(page.live_count) != descriptor.live_count
    || u64::from(page.tombstone_count) != descriptor.tombstone_count
    || page.logical_live_bytes != descriptor.logical_bytes
    || descriptor.page_count != 1
    || page.page_id != descriptor.minimum_page_id
    || page.page_id != descriptor.maximum_page_id
  {
    return Err(closure_error("ordered page disagrees with its exact artifact-directory descriptor"));
  }
  Ok(())
}

fn validate_directory_child(
  directory: &ArtifactDirectoryNodeV1<'_>,
  descriptor: &OwnedDirectoryEntryExpectationV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if directory.key != descriptor.child_hash
    || directory.generation != descriptor.child_generation
    || directory.lower_fence != descriptor.lower_fence
    || directory.upper_fence != descriptor.upper_fence
    || directory.live_count != descriptor.live_count
    || directory.tombstone_count != descriptor.tombstone_count
    || directory.page_count != descriptor.page_count
    || directory.logical_bytes != descriptor.logical_bytes
    || directory.minimum_page_id != descriptor.minimum_page_id
    || directory.maximum_page_id != descriptor.maximum_page_id
  {
    return Err(closure_error("artifact directory disagrees with its exact parent descriptor"));
  }
  Ok(())
}

fn select_directory_entry(
  directory: &ArtifactDirectoryNodeV1<'_>,
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  order_key: &[u8],
) -> Result<usize, IndexBatchApplicationErrorV1> {
  for (index, entry) in directory.entries.iter().enumerate() {
    if compare_order_keys(hash_algorithm, role, order_key, entry.upper_fence)? != std::cmp::Ordering::Greater {
      return Ok(index);
    }
  }
  directory.entries.len().checked_sub(1).ok_or_else(|| closure_error("artifact directory is empty"))
}

fn checked_overlay_artifact_bytes(artifact: &EncodedImmutableIndexArtifactV1) -> Result<usize, IndexBatchApplicationErrorV1> {
  size_of::<Arc<EncodedImmutableIndexArtifactV1>>()
    .checked_add(size_of::<(Vec<u8>, usize)>())
    .and_then(|bytes| bytes.checked_add(artifact.key.len().checked_mul(2)?))
    .and_then(|bytes| bytes.checked_add(artifact.value.len()))
    .ok_or(IndexBatchApplicationErrorV1::OverlayBytes)
}

fn clone_bytes(value: &[u8], context: &'static str) -> Result<Vec<u8>, IndexBatchApplicationErrorV1> {
  let mut cloned = Vec::new();
  cloned.try_reserve_exact(value.len()).map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("{context}: {error}")))?;
  cloned.extend_from_slice(value);
  Ok(cloned)
}

fn map_source_error(key: &[u8], error: IndexBatchArtifactReadErrorV1) -> IndexBatchApplicationErrorV1 {
  match error {
    IndexBatchArtifactReadErrorV1::Missing => IndexBatchApplicationErrorV1::MissingArtifact { key: hex::encode(key) },
    IndexBatchArtifactReadErrorV1::Cancelled => IndexBatchApplicationErrorV1::Cancelled,
    IndexBatchArtifactReadErrorV1::ResourcePressure(context) => IndexBatchApplicationErrorV1::SourcePressure(context),
    IndexBatchArtifactReadErrorV1::Operational(context) => IndexBatchApplicationErrorV1::SourceOperational(context),
  }
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<(), IndexBatchApplicationErrorV1> {
  if is_cancelled() {
    Err(IndexBatchApplicationErrorV1::Cancelled)
  } else {
    Ok(())
  }
}

fn closure_error(context: impl Into<String>) -> IndexBatchApplicationErrorV1 {
  IndexBatchApplicationErrorV1::Malformed(FormatError::new(
    MalformedInputClass::CrossRecordClosureMismatch,
    "index_batch_path_closure",
    context,
  ))
}

fn manifest_closure_error(context: impl Into<String>) -> IndexBatchApplicationErrorV1 {
  IndexBatchApplicationErrorV1::Malformed(FormatError::new(
    MalformedInputClass::CrossRecordClosureMismatch,
    "index_batch_manifest_closure",
    context,
  ))
}
