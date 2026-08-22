//! Bounded native bridge from durable Compact tasks to immutable v4 authority.

use std::sync::Arc;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner};
use crate::engine::{EngineError, HashAlgorithm};

use super::first_authority::{FirstAuthorityPublicationErrorV1, IndexActivePointerPublicationErrorV1, V4FirstAuthorityPublisher};
use super::gc_retirement::{RetirementJournalOwnerErrorV1, RetirementJournalReplacementAdmissionErrorV1};
use super::header_publication::DatabaseHeaderPublicationErrorV4;
use super::hash::digest_parts;
use super::index_artifact::{ActivePointerKindV1, IndexManifestBodyV1, IndexManifestKindV1, decode_index_manifest};
use super::index_batch_application::{
  FrozenIndexCompactionApplicationOutcomeV1, IndexArtifactCompactionApplicationRequestV1, IndexBatchApplicationErrorV1,
  IndexBatchArtifactOverlayLimitsV1, IndexBatchArtifactReadErrorV1, IndexBatchArtifactSourceV1, OrderedPageOrdinalLookupRequestV1,
  OrderedPagePathLookupLimitsV1, SparseIndexArtifactOverlayV1, apply_index_artifact_compaction_v1, load_ordered_page_ordinal_path_v1,
  source_role_root,
};
use super::index_compaction_runtime::{
  IndexArtifactCompactionExecutionOutcomeV1, IndexArtifactCompactionExecutionRequestV1, IndexRuntimeCompactionErrorClassV1,
  IndexRuntimeCompactionErrorV1, IndexRuntimeCompactionExecutorV1,
};
use super::index_copy_on_write::{ArtifactDirectoryPathV1, default_index_directory_layout_v1, default_index_page_layout_v1};
use super::index_generation_authority::{
  FrozenIndexGenerationPublicationErrorV1, FrozenIndexGenerationPublicationRequestV1, publish_frozen_index_application_v1,
};
use super::index_generation_publication::{
  IndexGenerationPublicationFailureBoundaryV1, IndexGenerationPublicationLimitsV1, IndexGenerationPublicationModeV1,
};
use super::index_page::{OrderedIndexRoleV1, decode_ordered_page};
use super::index_producer_source::{IndexSemanticScopeLimitsV1, IndexSemanticScopeReadErrorClassV1, IndexSemanticScopeReadErrorV1};
use super::index_recovery_store::SharedRetirementJournalOwnerV1;
use super::index_semantic_source::{CatalogIndexSemanticScopeSourceV1, IndexCompactionSemanticInventoryRequestV1};

const DEFAULT_COMPACTION_WORKING_BYTES: u64 = 192 * 1_024 * 1_024;
const MANIFEST_READ_CAP: usize = 1_024 * 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeIndexCompactionOptionsV1 {
  semantic_limits: IndexSemanticScopeLimitsV1,
  overlay_limits: IndexBatchArtifactOverlayLimitsV1,
  path_limits: OrderedPagePathLookupLimitsV1,
  publication_limits: IndexGenerationPublicationLimitsV1,
  maximum_working_bytes: u64,
}

impl NativeIndexCompactionOptionsV1 {
  pub fn engine_default(semantic_limits: IndexSemanticScopeLimitsV1) -> Result<Self, IndexRuntimeCompactionErrorV1> {
    Self::new(
      semantic_limits,
      IndexBatchArtifactOverlayLimitsV1::default(),
      OrderedPagePathLookupLimitsV1::default(),
      IndexGenerationPublicationLimitsV1::new(4_095, 64 * 1_024 * 1_024)
        .map_err(|error| corrupt("native_compaction_publication_limits", error.to_string()))?,
      DEFAULT_COMPACTION_WORKING_BYTES,
    )
  }

  pub fn new(
    semantic_limits: IndexSemanticScopeLimitsV1,
    overlay_limits: IndexBatchArtifactOverlayLimitsV1,
    path_limits: OrderedPagePathLookupLimitsV1,
    publication_limits: IndexGenerationPublicationLimitsV1,
    maximum_working_bytes: u64,
  ) -> Result<Self, IndexRuntimeCompactionErrorV1> {
    let path_bytes =
      u64::try_from(path_limits.maximum_input_bytes()).map_err(|error| corrupt("native_compaction_working_bytes", error.to_string()))?;
    let overlay_bytes = u64::try_from(overlay_limits.maximum_retained_bytes())
      .map_err(|error| corrupt("native_compaction_working_bytes", error.to_string()))?;
    let publication_bytes = u64::try_from(publication_limits.maximum_total_bytes())
      .map_err(|error| corrupt("native_compaction_working_bytes", error.to_string()))?;
    let minimum = path_bytes
      .checked_mul(2)
      .and_then(|bytes| bytes.checked_add(overlay_bytes))
      .and_then(|bytes| bytes.checked_add(publication_bytes))
      .ok_or_else(|| corrupt("native_compaction_working_bytes", "compaction working-set formula overflowed"))?;
    if maximum_working_bytes < minimum || maximum_working_bytes > DEFAULT_COMPACTION_WORKING_BYTES {
      return Err(corrupt(
        "native_compaction_working_bytes",
        format!("compaction working-set limit {maximum_working_bytes} is outside {minimum}..={DEFAULT_COMPACTION_WORKING_BYTES}"),
      ));
    }
    Ok(Self { semantic_limits, overlay_limits, path_limits, publication_limits, maximum_working_bytes })
  }

  pub const fn semantic_limits(self) -> IndexSemanticScopeLimitsV1 {
    self.semantic_limits
  }

  pub const fn maximum_working_bytes(self) -> u64 {
    self.maximum_working_bytes
  }
}

pub struct NativeIndexCompactionExecutorV1<'executor, 'source> {
  database_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  publisher: Arc<V4FirstAuthorityPublisher>,
  retirement_owner: SharedRetirementJournalOwnerV1,
  memory: Arc<MemoryCoordinator>,
  semantic_source: &'executor CatalogIndexSemanticScopeSourceV1<'source>,
  options: NativeIndexCompactionOptionsV1,
}

impl<'executor, 'source> NativeIndexCompactionExecutorV1<'executor, 'source> {
  pub fn new(
    database_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    publisher: Arc<V4FirstAuthorityPublisher>,
    retirement_owner: SharedRetirementJournalOwnerV1,
    memory: Arc<MemoryCoordinator>,
    semantic_source: &'executor CatalogIndexSemanticScopeSourceV1<'source>,
    options: NativeIndexCompactionOptionsV1,
  ) -> Result<Self, IndexRuntimeCompactionErrorV1> {
    if database_id == [0; 16] {
      return Err(corrupt("native_compaction_database", "native compaction database identity is zero"));
    }
    Ok(Self { database_id, hash_algorithm, publisher, retirement_owner, memory, semantic_source, options })
  }

  fn execute_bounded(
    &self,
    request: IndexArtifactCompactionExecutionRequestV1<'_>,
  ) -> Result<IndexArtifactCompactionExecutionOutcomeV1, IndexRuntimeCompactionErrorV1> {
    if (request.is_cancelled)() {
      return Err(cancelled("native_compaction_cancelled", "artifact compaction was cancelled before memory admission"));
    }
    if request.operation_id == [0; 16] || request.publication_sequence == 0 || request.now_ms == 0 {
      return Err(corrupt(
        "native_compaction_request",
        "artifact compaction operation, publication sequence, and timestamp must be nonzero",
      ));
    }
    let _working = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, self.options.maximum_working_bytes, AdmissionClass::Maintenance)
      .map_err(|error| retryable("native_compaction_memory_pressure", error.to_string()))?;
    let inventory = self
      .semantic_source
      .resolve_compaction_inventory(IndexCompactionSemanticInventoryRequestV1 {
        semantic_state_root: request.semantic_state_root,
        maintenance_scope: request.scope,
        limits: self.options.semantic_limits,
        is_cancelled: request.is_cancelled,
      })
      .map_err(map_semantic_error)?;
    if inventory.semantic_state_root() != request.semantic_state_root {
      return Err(corrupt("native_compaction_semantic_identity", "resolved semantic inventory returned another semantic-state root"));
    }

    for scope in inventory.scopes() {
      if let Some(source) = self.load_selected_manifest(ActivePointerKindV1::ScopeCatalog, scope.scope_id())? {
        if let Some(outcome) =
          self.try_compact_manifest(request, &source, &[], &[OrderedIndexRoleV1::ScopeOrdinal, OrderedIndexRoleV1::ScopeReverse])?
        {
          return Ok(outcome);
        }
      }
      for value_store in scope.value_stores() {
        let mut selected_fields = Vec::new();
        selected_fields
          .try_reserve_exact(value_store.field_index_ids().len())
          .map_err(|error| retryable("native_compaction_field_reservation", error.to_string()))?;
        for field_id in value_store.field_index_ids() {
          if let Some(field) = self.load_selected_manifest(ActivePointerKindV1::FieldIndex, field_id)? {
            selected_fields.push(field);
          }
        }
        let mut by_value_manifest: Vec<(Vec<u8>, Vec<usize>)> = Vec::new();
        by_value_manifest
          .try_reserve_exact(selected_fields.len())
          .map_err(|error| retryable("native_compaction_dependency_reservation", error.to_string()))?;
        for (index, field_bytes) in selected_fields.iter().enumerate() {
          let field = decode_index_manifest(field_bytes, self.hash_algorithm).map_err(map_format_error)?;
          let IndexManifestBodyV1::FieldIndex(body) = field.details else {
            return Err(corrupt("native_compaction_field_manifest", "selected FieldIndex pointer names another manifest kind"));
          };
          if !value_store.field_index_ids().iter().any(|field_id| field_id.as_slice() == field.owner_id) {
            return Err(corrupt("native_compaction_field_owner", "selected FieldIndex manifest is outside its semantic inventory"));
          }
          if let Some((_, dependents)) = by_value_manifest.iter_mut().find(|(manifest, _)| manifest == body.value_store_manifest) {
            dependents.try_reserve(1).map_err(|error| retryable("native_compaction_dependency_reservation", error.to_string()))?;
            dependents.push(index);
          } else {
            let mut manifest = Vec::new();
            manifest
              .try_reserve_exact(body.value_store_manifest.len())
              .map_err(|error| retryable("native_compaction_dependency_reservation", error.to_string()))?;
            manifest.extend_from_slice(body.value_store_manifest);
            let mut dependents = Vec::new();
            dependents.try_reserve_exact(1).map_err(|error| retryable("native_compaction_dependency_reservation", error.to_string()))?;
            dependents.push(index);
            by_value_manifest.push((manifest, dependents));
          }
        }
        for (value_manifest_key, dependent_indices) in by_value_manifest {
          let value_source = self.read_manifest(&value_manifest_key)?;
          let value_manifest = decode_index_manifest(&value_source, self.hash_algorithm).map_err(map_format_error)?;
          if value_manifest.kind != IndexManifestKindV1::ValueStore || value_manifest.owner_id != value_store.value_store_id() {
            return Err(corrupt("native_compaction_value_owner", "selected FieldIndex dependency names a foreign ValueStore manifest"));
          }
          let dependents: Vec<&[u8]> = dependent_indices.iter().map(|index| selected_fields[*index].as_slice()).collect();
          if let Some(outcome) = self.try_compact_manifest(
            request,
            &value_source,
            &dependents,
            &[OrderedIndexRoleV1::Value, OrderedIndexRoleV1::ValueDocumentState],
          )? {
            return Ok(outcome);
          }
        }
        for field_source in selected_fields {
          if let Some(outcome) = self.try_compact_manifest(
            request,
            &field_source,
            &[],
            &[OrderedIndexRoleV1::Posting, OrderedIndexRoleV1::IndexDocumentState],
          )? {
            return Ok(outcome);
          }
        }
      }
    }
    Ok(IndexArtifactCompactionExecutionOutcomeV1::Complete { published_owners: 0, publication_bytes: 0 })
  }

  fn load_selected_manifest(&self, kind: ActivePointerKindV1, owner_id: &[u8]) -> Result<Option<Vec<u8>>, IndexRuntimeCompactionErrorV1> {
    let pair = self.publisher.load_index_active_pointer_pair(&self.database_id, kind, owner_id).map_err(map_authority_read_error)?;
    let Some(selected) = pair.selected else {
      return Ok(None);
    };
    let bytes = self.read_manifest(&selected.target_manifest_hash)?;
    let manifest = decode_index_manifest(&bytes, self.hash_algorithm).map_err(map_format_error)?;
    let expected_kind = match kind {
      ActivePointerKindV1::ScopeCatalog => IndexManifestKindV1::ScopeCatalog,
      ActivePointerKindV1::FieldIndex => IndexManifestKindV1::FieldIndex,
      ActivePointerKindV1::FieldNvt => IndexManifestKindV1::FieldNvt,
    };
    if manifest.kind != expected_kind
      || manifest.owner_id != owner_id
      || manifest.generation != selected.generation
      || manifest.key != selected.target_manifest_hash
    {
      return Err(corrupt("native_compaction_selected_manifest", "selected active pointer and immutable manifest disagree"));
    }
    Ok(Some(bytes))
  }

  fn read_manifest(&self, key: &[u8]) -> Result<Vec<u8>, IndexRuntimeCompactionErrorV1> {
    self
      .publisher
      .load_index_artifact_bounded(key, MANIFEST_READ_CAP)
      .map_err(map_authority_read_error)?
      .ok_or_else(|| corrupt("native_compaction_manifest_missing", format!("selected manifest {} is absent", hex::encode(key))))
  }

  fn try_compact_manifest(
    &self,
    request: IndexArtifactCompactionExecutionRequestV1<'_>,
    source_manifest: &[u8],
    dependent_field_manifests: &[&[u8]],
    roles: &[OrderedIndexRoleV1],
  ) -> Result<Option<IndexArtifactCompactionExecutionOutcomeV1>, IndexRuntimeCompactionErrorV1> {
    let source = decode_index_manifest(source_manifest, self.hash_algorithm).map_err(map_format_error)?;
    let dependent_generation = dependent_field_manifests.iter().try_fold(source.generation, |generation, bytes| {
      decode_index_manifest(bytes, self.hash_algorithm).map(|manifest| generation.max(manifest.generation)).map_err(map_format_error)
    })?;
    let generation =
      dependent_generation.checked_add(1).ok_or_else(|| corrupt("native_compaction_generation", "compaction generation overflowed"))?;
    for role in roles {
      if (request.is_cancelled)() {
        return Err(cancelled("native_compaction_cancelled", "artifact compaction was cancelled before candidate selection"));
      }
      let Some(root_key) = source_role_root(&source.details, *role).map_err(map_application_error)? else {
        continue;
      };
      let mut artifact_source = FirstAuthorityIndexBatchArtifactSourceV1 { publisher: self.publisher.as_ref() };
      let overlay = SparseIndexArtifactOverlayV1::new(self.hash_algorithm, self.options.overlay_limits).map_err(map_application_error)?;
      let seed = compaction_seed(
        self.hash_algorithm,
        request.operation_id,
        request.publication_sequence,
        source.owner_id,
        source.key.as_slice(),
        *role,
      )?;
      let first = load_ordered_page_ordinal_path_v1(
        &OrderedPageOrdinalLookupRequestV1 {
          hash_algorithm: self.hash_algorithm,
          root_key,
          owner_id: source.owner_id,
          role: *role,
          page_ordinal: 0,
          load_posting_neighbors: *role == OrderedIndexRoleV1::Posting,
          limits: self.options.path_limits,
        },
        &overlay,
        &mut artifact_source,
        request.is_cancelled,
      )
      .map_err(map_application_error)?;
      if first.page_count() < 2 {
        continue;
      }
      let page_ordinal = seed % (first.page_count() - 1);
      let current = if page_ordinal == 0 {
        first
      } else {
        drop(first);
        load_ordered_page_ordinal_path_v1(
          &OrderedPageOrdinalLookupRequestV1 {
            hash_algorithm: self.hash_algorithm,
            root_key,
            owner_id: source.owner_id,
            role: *role,
            page_ordinal,
            load_posting_neighbors: *role == OrderedIndexRoleV1::Posting,
            limits: self.options.path_limits,
          },
          &overlay,
          &mut artifact_source,
          request.is_cancelled,
        )
        .map_err(map_application_error)?
      };
      let next = load_ordered_page_ordinal_path_v1(
        &OrderedPageOrdinalLookupRequestV1 {
          hash_algorithm: self.hash_algorithm,
          root_key,
          owner_id: source.owner_id,
          role: *role,
          page_ordinal: page_ordinal + 1,
          load_posting_neighbors: *role == OrderedIndexRoleV1::Posting,
          limits: self.options.path_limits,
        },
        &overlay,
        &mut artifact_source,
        request.is_cancelled,
      )
      .map_err(map_application_error)?;
      if current.page_count() != next.page_count() {
        return Err(corrupt("native_compaction_page_count", "two ordinal lookups observed different selected root page counts"));
      }

      let current_directories = collect_directories(&current, DirectorySetV1::Current)?;
      let next_directories = collect_directories(&next, DirectorySetV1::Current)?;
      let previous_directories = collect_directories(&current, DirectorySetV1::Previous)?;
      let outward_next_directories = collect_directories(&next, DirectorySetV1::Next)?;
      let mut path_pages = Vec::new();
      let mut path_directories = Vec::new();
      if let Some(previous) = current.previous_posting_page() {
        path_pages.push(previous);
        path_directories.push(previous_directories);
      }
      path_pages.push(current.page());
      path_directories.push(current_directories);
      path_pages.push(next.page());
      path_directories.push(next_directories);
      if let Some(outward_next) = next.next_posting_page() {
        path_pages.push(outward_next);
        path_directories.push(outward_next_directories);
      }
      let path_keys = path_pages
        .iter()
        .map(|page| decode_ordered_page(page, self.hash_algorithm).map(|decoded| decoded.key))
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_format_error)?;
      let paths = path_keys
        .iter()
        .zip(&path_directories)
        .map(|(key, directories)| ArtifactDirectoryPathV1 { source_page_key: key, directories })
        .collect::<Vec<_>>();
      let source_pages = [current.page(), next.page()];
      let outcome = apply_index_artifact_compaction_v1(
        &IndexArtifactCompactionApplicationRequestV1 {
          hash_algorithm: self.hash_algorithm,
          coordinator_id: request.operation_id,
          batch_id: request.publication_sequence,
          attempt_id: seed.max(1),
          generation,
          source_manifest,
          dependent_field_manifests,
          role: *role,
          source_pages: &source_pages,
          previous_posting_page: current.previous_posting_page(),
          next_posting_page: next.next_posting_page(),
          paths: &paths,
          tombstone_drop_proof: None,
          overlay_limits: self.options.overlay_limits,
          page_layout: default_index_page_layout_v1(),
          directory_layout: default_index_directory_layout_v1(),
        },
        request.is_cancelled,
      )
      .map_err(map_application_error)?;
      let FrozenIndexCompactionApplicationOutcomeV1::Publication(plan) = outcome else {
        continue;
      };
      let mut retirement = self.retirement_owner.lock().map_err(|error| {
        corrupt("native_compaction_retirement_lock", format!("shared retirement journal owner lock is poisoned: {error}"))
      })?;
      let receipt = publish_frozen_index_application_v1(
        self.publisher.as_ref(),
        &mut retirement,
        FrozenIndexGenerationPublicationRequestV1 {
          database_id: &self.database_id,
          hash_algorithm: self.hash_algorithm,
          plan: &plan,
          mode: IndexGenerationPublicationModeV1::Soft,
          limits: self.options.publication_limits,
          publication_timestamp_ms: request.now_ms,
          monotonic_now_ms: request.now_ms,
        },
        request.is_cancelled,
      )
      .map_err(map_publication_error)?;
      let publication_bytes = plan
        .prepared_artifacts()
        .map(|artifact| artifact.value.len())
        .chain(plan.owner_plans().iter().map(|owner| owner.successor_manifest().value.len()))
        .chain(receipt.pointer_receipts.iter().map(|pointer| pointer.pointer_bytes.len()))
        .try_fold(0u64, |total, length| {
          let length = u64::try_from(length)
            .map_err(|error| corrupt("native_compaction_publication_bytes", format!("publication length conversion failed: {error}")))?;
          total
            .checked_add(length)
            .ok_or_else(|| corrupt("native_compaction_publication_bytes", "compaction publication byte accounting overflowed"))
        })?;
      let published_owners = u32::try_from(receipt.pointer_receipts.len())
        .map_err(|error| corrupt("native_compaction_publication_owners", error.to_string()))?;
      return Ok(Some(IndexArtifactCompactionExecutionOutcomeV1::Progress { published_owners, publication_bytes }));
    }
    Ok(None)
  }
}

impl IndexRuntimeCompactionExecutorV1 for NativeIndexCompactionExecutorV1<'_, '_> {
  fn execute(
    &self,
    request: IndexArtifactCompactionExecutionRequestV1<'_>,
  ) -> Result<IndexArtifactCompactionExecutionOutcomeV1, IndexRuntimeCompactionErrorV1> {
    self.execute_bounded(request)
  }
}

struct FirstAuthorityIndexBatchArtifactSourceV1<'publisher> {
  publisher: &'publisher V4FirstAuthorityPublisher,
}

impl IndexBatchArtifactSourceV1 for FirstAuthorityIndexBatchArtifactSourceV1<'_> {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<Vec<u8>, IndexBatchArtifactReadErrorV1> {
    match self.publisher.load_index_artifact_bounded(key, maximum_bytes) {
      Ok(Some(bytes)) => Ok(bytes),
      Ok(None) => Err(IndexBatchArtifactReadErrorV1::Missing),
      Err(error) => Err(map_artifact_read_error(error)),
    }
  }
}

#[derive(Clone, Copy)]
enum DirectorySetV1 {
  Current,
  Previous,
  Next,
}

fn collect_directories(
  loaded: &super::index_batch_application::LoadedOrderedPageOrdinalPathV1,
  set: DirectorySetV1,
) -> Result<Vec<&[u8]>, IndexRuntimeCompactionErrorV1> {
  let count = match set {
    DirectorySetV1::Current => loaded.directory_count(),
    DirectorySetV1::Previous => loaded.previous_directory_count(),
    DirectorySetV1::Next => loaded.next_directory_count(),
  };
  let mut directories = Vec::new();
  directories.try_reserve_exact(count).map_err(|error| retryable("native_compaction_path_reservation", error.to_string()))?;
  for index in 0..count {
    let directory = match set {
      DirectorySetV1::Current => loaded.directory(index),
      DirectorySetV1::Previous => loaded.previous_directory(index),
      DirectorySetV1::Next => loaded.next_directory(index),
    }
    .ok_or_else(|| corrupt("native_compaction_path_shape", "loaded ordinal path lost a retained directory"))?;
    directories.push(directory);
  }
  Ok(directories)
}

fn compaction_seed(
  hash_algorithm: HashAlgorithm,
  operation_id: [u8; 16],
  publication_sequence: u64,
  owner_id: &[u8],
  manifest_key: &[u8],
  role: OrderedIndexRoleV1,
) -> Result<u64, IndexRuntimeCompactionErrorV1> {
  let digest = digest_parts(
    hash_algorithm,
    &[b"aeordb.index.compaction.window.v1\0", &operation_id, &publication_sequence.to_le_bytes(), owner_id, manifest_key, &[role.id()]],
  );
  let prefix =
    digest.get(..8).ok_or_else(|| corrupt("native_compaction_seed", "configured hash algorithm produced fewer than eight digest bytes"))?;
  let mut bytes = [0; 8];
  bytes.copy_from_slice(prefix);
  Ok(u64::from_le_bytes(bytes))
}

fn map_semantic_error(error: IndexSemanticScopeReadErrorV1) -> IndexRuntimeCompactionErrorV1 {
  let class = match error.class() {
    IndexSemanticScopeReadErrorClassV1::Cancelled => IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection,
    IndexSemanticScopeReadErrorClassV1::Retryable => IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection,
    IndexSemanticScopeReadErrorClassV1::Corrupt => IndexRuntimeCompactionErrorClassV1::Corrupt,
  };
  IndexRuntimeCompactionErrorV1::new(class, error.code(), error.context())
}

fn map_application_error(error: IndexBatchApplicationErrorV1) -> IndexRuntimeCompactionErrorV1 {
  let class = match error {
    IndexBatchApplicationErrorV1::Cancelled => IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection,
    IndexBatchApplicationErrorV1::SourcePressure(_)
    | IndexBatchApplicationErrorV1::SourceOperational(_)
    | IndexBatchApplicationErrorV1::OverlayCount
    | IndexBatchApplicationErrorV1::OverlayBytes
    | IndexBatchApplicationErrorV1::Allocation(_) => IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection,
    IndexBatchApplicationErrorV1::MissingArtifact { .. }
    | IndexBatchApplicationErrorV1::SourceCorrupt(_)
    | IndexBatchApplicationErrorV1::Malformed(_)
    | IndexBatchApplicationErrorV1::InvalidLimits(_)
    | IndexBatchApplicationErrorV1::OverlayConflict => IndexRuntimeCompactionErrorClassV1::Corrupt,
  };
  IndexRuntimeCompactionErrorV1::new(class, error.code(), error.to_string())
}

fn map_publication_error(error: FrozenIndexGenerationPublicationErrorV1) -> IndexRuntimeCompactionErrorV1 {
  let class = if error.failure_boundary() != IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained {
    IndexRuntimeCompactionErrorClassV1::CommitUnknown
  } else {
    match &error {
      FrozenIndexGenerationPublicationErrorV1::Cancelled { .. } => IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection,
      FrozenIndexGenerationPublicationErrorV1::InvalidPlan { code: "index_generation_source_superseded", .. } => {
        IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection
      }
      FrozenIndexGenerationPublicationErrorV1::InvalidPlan { .. } | FrozenIndexGenerationPublicationErrorV1::Format { .. } => {
        IndexRuntimeCompactionErrorClassV1::Corrupt
      }
      FrozenIndexGenerationPublicationErrorV1::Authority { source, .. } => classify_authority_publication_error(source),
      FrozenIndexGenerationPublicationErrorV1::ActivePointer { source, .. } => classify_active_pointer_publication_error(source),
    }
  };
  IndexRuntimeCompactionErrorV1::new(class, error.code(), error.to_string())
}

fn classify_active_pointer_publication_error(error: &IndexActivePointerPublicationErrorV1) -> IndexRuntimeCompactionErrorClassV1 {
  match error {
    IndexActivePointerPublicationErrorV1::Committed { .. } => IndexRuntimeCompactionErrorClassV1::CommitUnknown,
    IndexActivePointerPublicationErrorV1::Invalid { .. } => IndexRuntimeCompactionErrorClassV1::Corrupt,
    IndexActivePointerPublicationErrorV1::Authority(source) => classify_authority_publication_error(source),
    IndexActivePointerPublicationErrorV1::RetirementAdmission(source) => classify_retirement_admission_error(source),
    IndexActivePointerPublicationErrorV1::RetirementOwner(source) => classify_retirement_owner_error(source),
  }
}

fn classify_authority_publication_error(error: &FirstAuthorityPublicationErrorV1) -> IndexRuntimeCompactionErrorClassV1 {
  match error {
    FirstAuthorityPublicationErrorV1::Committed { .. } => IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection,
    FirstAuthorityPublicationErrorV1::Engine(
      EngineError::IoError(_)
      | EngineError::ResourceExhausted(_)
      | EngineError::DurabilityFailure(_)
      | EngineError::PostMutationDurabilityFailure(_)
      | EngineError::ShuttingDown,
    ) => IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection,
    FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled(_)) => IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection,
    FirstAuthorityPublicationErrorV1::Header(
      DatabaseHeaderPublicationErrorV4::Native(_) | DatabaseHeaderPublicationErrorV4::Durability(_),
    ) => IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection,
    FirstAuthorityPublicationErrorV1::Invalid { .. }
    | FirstAuthorityPublicationErrorV1::Format(_)
    | FirstAuthorityPublicationErrorV1::Engine(_)
    | FirstAuthorityPublicationErrorV1::Header(_)
    | FirstAuthorityPublicationErrorV1::StateLockPoisoned => IndexRuntimeCompactionErrorClassV1::Corrupt,
  }
}

fn classify_retirement_admission_error(error: &RetirementJournalReplacementAdmissionErrorV1) -> IndexRuntimeCompactionErrorClassV1 {
  match error {
    RetirementJournalReplacementAdmissionErrorV1::Preflight(_) => IndexRuntimeCompactionErrorClassV1::Corrupt,
    RetirementJournalReplacementAdmissionErrorV1::Journal { source, .. } => classify_retirement_owner_error(source),
  }
}

fn classify_retirement_owner_error(error: &RetirementJournalOwnerErrorV1) -> IndexRuntimeCompactionErrorClassV1 {
  match error {
    RetirementJournalOwnerErrorV1::Canceled => IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection,
    RetirementJournalOwnerErrorV1::Memory(_) | RetirementJournalOwnerErrorV1::Sink { .. } => {
      IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection
    }
    RetirementJournalOwnerErrorV1::InvalidOptions(_)
    | RetirementJournalOwnerErrorV1::ClockRegression
    | RetirementJournalOwnerErrorV1::RecordOrder
    | RetirementJournalOwnerErrorV1::ArithmeticOverflow
    | RetirementJournalOwnerErrorV1::ReceiptMismatch { .. }
    | RetirementJournalOwnerErrorV1::BufferedRollbackOwner
    | RetirementJournalOwnerErrorV1::BufferedRollbackState
    | RetirementJournalOwnerErrorV1::Format(_)
    | RetirementJournalOwnerErrorV1::Failed => IndexRuntimeCompactionErrorClassV1::Corrupt,
  }
}

fn map_authority_read_error(error: FirstAuthorityPublicationErrorV1) -> IndexRuntimeCompactionErrorV1 {
  match error {
    FirstAuthorityPublicationErrorV1::Engine(EngineError::IoError(source)) => retryable("native_compaction_read_io", source.to_string()),
    FirstAuthorityPublicationErrorV1::Engine(EngineError::ResourceExhausted(context)) => {
      retryable("native_compaction_read_pressure", context)
    }
    FirstAuthorityPublicationErrorV1::Engine(EngineError::ShuttingDown) => {
      retryable("native_compaction_read_shutdown", "storage engine is shutting down")
    }
    FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled(context)) => cancelled("native_compaction_cancelled", context),
    source @ FirstAuthorityPublicationErrorV1::Header(
      DatabaseHeaderPublicationErrorV4::Native(_) | DatabaseHeaderPublicationErrorV4::Durability(_),
    ) => retryable("native_compaction_read_io", source.to_string()),
    source => corrupt(source.code(), source.to_string()),
  }
}

fn map_artifact_read_error(error: FirstAuthorityPublicationErrorV1) -> IndexBatchArtifactReadErrorV1 {
  match error {
    FirstAuthorityPublicationErrorV1::Engine(EngineError::IoError(source)) => {
      IndexBatchArtifactReadErrorV1::Operational(source.to_string())
    }
    FirstAuthorityPublicationErrorV1::Engine(EngineError::ResourceExhausted(context)) => {
      IndexBatchArtifactReadErrorV1::ResourcePressure(context)
    }
    FirstAuthorityPublicationErrorV1::Engine(EngineError::ShuttingDown) => {
      IndexBatchArtifactReadErrorV1::Operational("storage engine is shutting down".to_owned())
    }
    source @ FirstAuthorityPublicationErrorV1::Header(
      DatabaseHeaderPublicationErrorV4::Native(_) | DatabaseHeaderPublicationErrorV4::Durability(_),
    ) => IndexBatchArtifactReadErrorV1::Operational(source.to_string()),
    FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled(_)) => IndexBatchArtifactReadErrorV1::Cancelled,
    FirstAuthorityPublicationErrorV1::Invalid { code: "immutable_index_value_exceeds_cap", message } => {
      IndexBatchArtifactReadErrorV1::ResourcePressure(message)
    }
    source => IndexBatchArtifactReadErrorV1::Corrupt(source.to_string()),
  }
}

fn map_format_error(error: super::reader::FormatError) -> IndexRuntimeCompactionErrorV1 {
  corrupt(error.code(), error.to_string())
}

fn retryable(code: &'static str, context: impl Into<String>) -> IndexRuntimeCompactionErrorV1 {
  IndexRuntimeCompactionErrorV1::new(IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection, code, context)
}

fn cancelled(code: &'static str, context: impl Into<String>) -> IndexRuntimeCompactionErrorV1 {
  IndexRuntimeCompactionErrorV1::new(IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection, code, context)
}

fn corrupt(code: &'static str, context: impl Into<String>) -> IndexRuntimeCompactionErrorV1 {
  IndexRuntimeCompactionErrorV1::new(IndexRuntimeCompactionErrorClassV1::Corrupt, code, context)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn boundary_error(code: &'static str, boundary: IndexGenerationPublicationFailureBoundaryV1) -> FrozenIndexGenerationPublicationErrorV1 {
    FrozenIndexGenerationPublicationErrorV1::InvalidPlan { code, message: "injected publication failure".to_owned(), boundary }
  }

  #[test]
  fn native_publication_error_mapping_never_retries_malformed_or_post_pointer_authority() {
    let malformed = map_publication_error(boundary_error(
      "index_generation_invalid_plan",
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    ));
    assert_eq!(malformed.class(), IndexRuntimeCompactionErrorClassV1::Corrupt);

    let superseded = map_publication_error(boundary_error(
      "index_generation_source_superseded",
      IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    ));
    assert_eq!(superseded.class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);

    let pressure = map_publication_error(FrozenIndexGenerationPublicationErrorV1::Authority {
      source: FirstAuthorityPublicationErrorV1::Engine(EngineError::ResourceExhausted("injected pressure".to_owned())),
      boundary: IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    });
    assert_eq!(pressure.class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);

    let cancelled = map_publication_error(FrozenIndexGenerationPublicationErrorV1::Cancelled {
      boundary: IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    });
    assert_eq!(cancelled.class(), IndexRuntimeCompactionErrorClassV1::CancelledBeforeSelection);

    let shutdown = map_authority_read_error(FirstAuthorityPublicationErrorV1::Engine(EngineError::ShuttingDown));
    assert_eq!(shutdown.class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);
    assert!(matches!(
      map_artifact_read_error(FirstAuthorityPublicationErrorV1::Engine(EngineError::ShuttingDown)),
      IndexBatchArtifactReadErrorV1::Operational(_)
    ));

    for boundary in [
      IndexGenerationPublicationFailureBoundaryV1::PointerCommitUnknown,
      IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
    ] {
      let uncertain = map_publication_error(boundary_error("index_generation_invalid_plan", boundary));
      assert_eq!(uncertain.class(), IndexRuntimeCompactionErrorClassV1::CommitUnknown);
    }
  }
}
