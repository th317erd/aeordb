//! Bounded immutable metadata registry for selected v4 index generations.
//!
//! The registry is not a selector and never retains index pages or manifest
//! bodies. It validates metadata chosen by the existing A/B authority, then
//! atomically swaps a compact snapshot for later query planning.

use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, Mutex, RwLock, TryLockError};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use super::first_authority::{
  FirstAuthorityPublicationErrorV1, LoadedIndexActivePointerPairV1, LoadedIndexActivePointerV1, V4FirstAuthorityPublisher,
};
use super::hash::digest_parts;
use super::index_artifact::{
  ActivePointerKindV1, ImmutableIndexArtifactKindV1, IndexManifestBodyV1, decode_active_pointer, decode_index_manifest,
  validate_correctness_manifest_chain,
};
use super::index_coverage_planner::{IndexCoverageGenerationHealthV1, IndexCoverageGenerationV1};
use super::index_nvt::{NvtBasisStatusV1, NvtFallbackReasonV1, pin_field_index_v1, validate_field_nvt_basis_v1};

const MANIFEST_READ_CAP: usize = 1_048_576;
const MANIFEST_READ_RESERVATION_BYTES: u64 = MANIFEST_READ_CAP as u64 + 4 * 1_024;
const POINTER_SELECTION_TRANSIENT_BYTES: u64 = 9 * 1_024 * 1_024;
const RETAINED_SNAPSHOT_ALLOCATION_ALLOWANCE: u64 = 256;
const RETAINED_ENTRY_FIXED_ALLOWANCE: u64 = 512;
const RETAINED_ENTRY_HASH_ALLOWANCE: u64 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexCoverageRegistryOptionsV1 {
  maximum_entries: usize,
  maximum_retained_bytes: u64,
}

impl IndexCoverageRegistryOptionsV1 {
  pub fn new(maximum_entries: usize, maximum_retained_bytes: u64) -> Result<Self, IndexCoverageRegistryErrorV1> {
    if maximum_entries == 0 || maximum_retained_bytes == 0 {
      return Err(IndexCoverageRegistryErrorV1::invalid(
        "index_coverage_registry_options",
        "registry entry and retained-byte bounds must be nonzero",
      ));
    }
    Ok(Self { maximum_entries, maximum_retained_bytes })
  }

  pub const fn maximum_entries(self) -> usize {
    self.maximum_entries
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexCoverageRegistryOwnerKindV1 {
  ScopeCatalog,
  FieldIndex,
}

impl IndexCoverageRegistryOwnerKindV1 {
  const fn pointer_kind(self) -> ActivePointerKindV1 {
    match self {
      Self::ScopeCatalog => ActivePointerKindV1::ScopeCatalog,
      Self::FieldIndex => ActivePointerKindV1::FieldIndex,
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoverageRegistryOwnerRequestV1 {
  kind: IndexCoverageRegistryOwnerKindV1,
  owner_id: Vec<u8>,
  health: IndexCoverageGenerationHealthV1,
}

impl IndexCoverageRegistryOwnerRequestV1 {
  pub fn new(
    kind: IndexCoverageRegistryOwnerKindV1,
    owner_id: Vec<u8>,
    health: IndexCoverageGenerationHealthV1,
  ) -> Result<Self, IndexCoverageRegistryErrorV1> {
    if owner_id.is_empty() || owner_id.iter().all(|byte| *byte == 0) {
      return Err(IndexCoverageRegistryErrorV1::invalid(
        "index_coverage_registry_owner",
        "registry owner identity must be nonempty and nonzero",
      ));
    }
    Ok(Self { kind, owner_id, health })
  }

  pub const fn kind(&self) -> IndexCoverageRegistryOwnerKindV1 {
    self.kind
  }

  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexCoverageRegistryUnavailableReasonV1 {
  NoSelectedGeneration,
  CorruptSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoverageRegistryGenerationV1 {
  generation: u64,
  owner_id: Vec<u8>,
  manifest_hash: Vec<u8>,
  pointer_sequence: u64,
  source_namespace_root: Vec<u8>,
  coverage_epoch_id: [u8; 16],
  coverage_publication_sequence: u64,
  definition_fingerprint: Vec<u8>,
  dependency_fingerprint: Vec<u8>,
  health: IndexCoverageGenerationHealthV1,
}

impl IndexCoverageRegistryGenerationV1 {
  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }

  pub fn manifest_hash(&self) -> &[u8] {
    &self.manifest_hash
  }

  pub const fn pointer_sequence(&self) -> u64 {
    self.pointer_sequence
  }

  pub fn source_namespace_root(&self) -> &[u8] {
    &self.source_namespace_root
  }

  pub const fn coverage_epoch_id(&self) -> &[u8; 16] {
    &self.coverage_epoch_id
  }

  pub const fn coverage_publication_sequence(&self) -> u64 {
    self.coverage_publication_sequence
  }

  pub fn definition_fingerprint(&self) -> &[u8] {
    &self.definition_fingerprint
  }

  pub fn dependency_fingerprint(&self) -> &[u8] {
    &self.dependency_fingerprint
  }

  pub const fn health(&self) -> IndexCoverageGenerationHealthV1 {
    self.health
  }

  pub fn as_planning_generation(&self) -> IndexCoverageGenerationV1<'_> {
    IndexCoverageGenerationV1 {
      generation: self.generation,
      owner_id: &self.owner_id,
      manifest_hash: &self.manifest_hash,
      source_namespace_root: &self.source_namespace_root,
      coverage_epoch_id: &self.coverage_epoch_id,
      coverage_publication_sequence: self.coverage_publication_sequence,
      definition_fingerprint: &self.definition_fingerprint,
      dependency_fingerprint: &self.dependency_fingerprint,
      health: self.health,
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexCoverageRegistrySelectionV1 {
  Selected(IndexCoverageRegistryGenerationV1),
  Unavailable(IndexCoverageRegistryUnavailableReasonV1),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexCoverageNvtUnavailableReasonV1 {
  Absent,
  CorruptSelection,
  CorruptManifest,
  SourceUnavailable,
  IncompatibleOwner,
  StalePostingGeneration,
  StaleSourceRoot,
  SelectionChanged,
  ResourceLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoverageNvtDescriptorV1 {
  manifest_hash: Vec<u8>,
  generation: u64,
  resolution: u64,
  tile_cells: u32,
  tile_directory_root: Option<Vec<u8>>,
}

impl IndexCoverageNvtDescriptorV1 {
  pub fn manifest_hash(&self) -> &[u8] {
    &self.manifest_hash
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn resolution(&self) -> u64 {
    self.resolution
  }

  pub const fn tile_cells(&self) -> u32 {
    self.tile_cells
  }

  pub fn tile_directory_root(&self) -> Option<&[u8]> {
    self.tile_directory_root.as_deref()
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexCoverageNvtStatusV1 {
  NotApplicable,
  Usable(IndexCoverageNvtDescriptorV1),
  Unavailable(IndexCoverageNvtUnavailableReasonV1),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCoverageRegistryEntryV1 {
  kind: IndexCoverageRegistryOwnerKindV1,
  owner_id: Vec<u8>,
  selection: IndexCoverageRegistrySelectionV1,
  nvt_status: IndexCoverageNvtStatusV1,
}

impl IndexCoverageRegistryEntryV1 {
  pub const fn kind(&self) -> IndexCoverageRegistryOwnerKindV1 {
    self.kind
  }

  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }

  pub const fn selection(&self) -> &IndexCoverageRegistrySelectionV1 {
    &self.selection
  }

  pub const fn nvt_status(&self) -> &IndexCoverageNvtStatusV1 {
    &self.nvt_status
  }
}

pub struct IndexCoverageRegistrySnapshotV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  entries: Vec<IndexCoverageRegistryEntryV1>,
  retained_bytes: u64,
  _reservation: MemoryReservation,
}

impl fmt::Debug for IndexCoverageRegistrySnapshotV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("IndexCoverageRegistrySnapshotV1")
      .field("hash_algorithm", &self.hash_algorithm)
      .field("database_id", &self.database_id)
      .field("entries", &self.entries)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl IndexCoverageRegistrySnapshotV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }

  pub fn entries(&self) -> &[IndexCoverageRegistryEntryV1] {
    &self.entries
  }

  pub fn entry(&self, kind: IndexCoverageRegistryOwnerKindV1, owner_id: &[u8]) -> Option<&IndexCoverageRegistryEntryV1> {
    let index = self.entries.partition_point(|entry| (entry.kind, entry.owner_id.as_slice()) < (kind, owner_id));
    self.entries.get(index).filter(|entry| entry.kind == kind && entry.owner_id == owner_id)
  }
}

pub struct IndexCoverageRegistryV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  options: IndexCoverageRegistryOptionsV1,
  memory: Arc<MemoryCoordinator>,
  refresh: Mutex<()>,
  snapshot: RwLock<Arc<IndexCoverageRegistrySnapshotV1>>,
}

impl IndexCoverageRegistryV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    database_id: [u8; 16],
    options: IndexCoverageRegistryOptionsV1,
    memory: Arc<MemoryCoordinator>,
  ) -> Result<Self, IndexCoverageRegistryErrorV1> {
    if database_id.iter().all(|byte| *byte == 0) {
      return Err(IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_database", "registry database identity must be nonzero"));
    }
    let retained_bytes = snapshot_retained_bound(hash_algorithm, 0)?;
    require_snapshot_bound(options, retained_bytes)?;
    let reservation = memory.reserve(MemoryOwner::IndexCleanCache, retained_bytes, AdmissionClass::Cache)?;
    let snapshot = Arc::new(IndexCoverageRegistrySnapshotV1 {
      hash_algorithm,
      database_id,
      entries: Vec::new(),
      retained_bytes,
      _reservation: reservation,
    });
    Ok(Self { hash_algorithm, database_id, options, memory, refresh: Mutex::new(()), snapshot: RwLock::new(snapshot) })
  }

  pub fn snapshot(&self) -> Result<Arc<IndexCoverageRegistrySnapshotV1>, IndexCoverageRegistryErrorV1> {
    self
      .snapshot
      .read()
      .map(|snapshot| Arc::clone(&snapshot))
      .map_err(|error| IndexCoverageRegistryErrorV1::Poisoned { message: error.to_string() })
  }

  pub fn refresh(
    &self,
    source: &mut dyn IndexCoverageRegistrySourceV1,
    requests: &[IndexCoverageRegistryOwnerRequestV1],
    cancellation: &CancellationToken,
  ) -> Result<Arc<IndexCoverageRegistrySnapshotV1>, IndexCoverageRegistryErrorV1> {
    let _refresh = match self.refresh.try_lock() {
      Ok(guard) => guard,
      Err(TryLockError::WouldBlock) => return Err(IndexCoverageRegistryErrorV1::RefreshBusy),
      Err(TryLockError::Poisoned(error)) => {
        return Err(IndexCoverageRegistryErrorV1::Poisoned { message: error.to_string() });
      }
    };
    require_not_cancelled(cancellation)?;
    if source.hash_algorithm() != self.hash_algorithm || source.database_id() != self.database_id {
      return Err(IndexCoverageRegistryErrorV1::invalid(
        "index_coverage_registry_source_authority",
        "registry source belongs to another database authority",
      ));
    }
    validate_requests(self.hash_algorithm, self.options, requests)?;
    let retained_bytes = snapshot_retained_bound(self.hash_algorithm, requests.len())?;
    require_snapshot_bound(self.options, retained_bytes)?;
    let reservation = self.memory.reserve(MemoryOwner::IndexCleanCache, retained_bytes, AdmissionClass::Cache)?;
    let mut entries = Vec::new();
    entries.try_reserve_exact(requests.len()).map_err(|error| {
      IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_allocation", format!("registry entry allocation failed: {error}"))
    })?;
    for request in requests {
      require_not_cancelled(cancellation)?;
      entries.push(self.load_entry(source, request, cancellation)?);
    }
    require_not_cancelled(cancellation)?;
    let next = Arc::new(IndexCoverageRegistrySnapshotV1 {
      hash_algorithm: self.hash_algorithm,
      database_id: self.database_id,
      entries,
      retained_bytes,
      _reservation: reservation,
    });
    let mut selected = self.snapshot.write().map_err(|error| IndexCoverageRegistryErrorV1::Poisoned { message: error.to_string() })?;
    require_not_cancelled(cancellation)?;
    *selected = Arc::clone(&next);
    Ok(next)
  }

  fn load_entry(
    &self,
    source: &mut dyn IndexCoverageRegistrySourceV1,
    request: &IndexCoverageRegistryOwnerRequestV1,
    cancellation: &CancellationToken,
  ) -> Result<IndexCoverageRegistryEntryV1, IndexCoverageRegistryErrorV1> {
    let kind = request.kind.pointer_kind();
    let initial = load_pair_bounded(&self.memory, source, kind, &request.owner_id, cancellation)?;
    let selected = validate_pair(&initial, kind, &request.owner_id, self.hash_algorithm)?;
    let Some(pointer) = selected else {
      let rechecked = load_pair_bounded(&self.memory, source, kind, &request.owner_id, cancellation)?;
      require_stable_pair(&initial, &rechecked)?;
      let corrupt =
        initial.structurally_invalid_slots.iter().any(|invalid| *invalid) || initial.closure_invalid_slots.iter().any(|invalid| *invalid);
      return Ok(IndexCoverageRegistryEntryV1 {
        kind: request.kind,
        owner_id: request.owner_id.clone(),
        selection: IndexCoverageRegistrySelectionV1::Unavailable(if corrupt {
          IndexCoverageRegistryUnavailableReasonV1::CorruptSelection
        } else {
          IndexCoverageRegistryUnavailableReasonV1::NoSelectedGeneration
        }),
        nvt_status: if request.kind == IndexCoverageRegistryOwnerKindV1::FieldIndex {
          IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::Absent)
        } else {
          IndexCoverageNvtStatusV1::NotApplicable
        },
      });
    };
    let entry = match request.kind {
      IndexCoverageRegistryOwnerKindV1::ScopeCatalog => {
        let manifest = load_manifest_bounded(&self.memory, source, &pointer.target_manifest_hash, cancellation)?;
        let manifest = decode_index_manifest(&manifest.bytes, self.hash_algorithm)
          .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_scope_manifest", error.to_string()))?;
        let IndexManifestBodyV1::ScopeCatalog(body) = &manifest.details else {
          return Err(IndexCoverageRegistryErrorV1::corrupt(
            "index_coverage_scope_manifest_kind",
            "selected ScopeCatalog pointer resolves to another manifest kind",
          ));
        };
        require_manifest_pointer(pointer, &manifest.key, manifest.owner_id, manifest.generation)?;
        require_readable_capabilities(body.required_reader_capabilities)?;
        let generation = selected_generation(
          self.hash_algorithm,
          pointer,
          &body.coverage,
          body.scope_definition,
          scope_dependency_fingerprint(self.hash_algorithm),
          combined_health(request.health, initial.repair_required),
          scope_definition_fingerprint(self.hash_algorithm, body.scope_definition),
        )?;
        IndexCoverageRegistryEntryV1 {
          kind: request.kind,
          owner_id: request.owner_id.clone(),
          selection: IndexCoverageRegistrySelectionV1::Selected(generation),
          nvt_status: IndexCoverageNvtStatusV1::NotApplicable,
        }
      }
      IndexCoverageRegistryOwnerKindV1::FieldIndex => {
        let field_bytes = load_manifest_bounded(&self.memory, source, &pointer.target_manifest_hash, cancellation)?;
        let field = decode_index_manifest(&field_bytes.bytes, self.hash_algorithm)
          .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_field_manifest", error.to_string()))?;
        let IndexManifestBodyV1::FieldIndex(field_body) = &field.details else {
          return Err(IndexCoverageRegistryErrorV1::corrupt(
            "index_coverage_field_manifest_kind",
            "selected FieldIndex pointer resolves to another manifest kind",
          ));
        };
        require_manifest_pointer(pointer, &field.key, field.owner_id, field.generation)?;
        let value_bytes = load_manifest_bounded(&self.memory, source, field_body.value_store_manifest, cancellation)?;
        let value = decode_index_manifest(&value_bytes.bytes, self.hash_algorithm)
          .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_value_manifest", error.to_string()))?;
        let IndexManifestBodyV1::ValueStore(value_body) = &value.details else {
          return Err(IndexCoverageRegistryErrorV1::corrupt(
            "index_coverage_value_manifest_kind",
            "FieldIndex dependency resolves to another manifest kind",
          ));
        };
        let scope_bytes = load_manifest_bounded(&self.memory, source, value_body.scope_catalog_manifest, cancellation)?;
        let scope = decode_index_manifest(&scope_bytes.bytes, self.hash_algorithm)
          .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_scope_manifest", error.to_string()))?;
        validate_correctness_manifest_chain(&scope, &value, &field, self.hash_algorithm)
          .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_manifest_chain", error.to_string()))?;
        let IndexManifestBodyV1::ScopeCatalog(scope_body) = &scope.details else {
          return Err(IndexCoverageRegistryErrorV1::corrupt(
            "index_coverage_scope_manifest_kind",
            "ValueStore dependency resolves to another manifest kind",
          ));
        };
        for capabilities in
          [field_body.required_reader_capabilities, value_body.required_reader_capabilities, scope_body.required_reader_capabilities]
        {
          require_readable_capabilities(capabilities)?;
        }
        let dependency_fingerprint = field_dependency_fingerprint(self.hash_algorithm, scope.owner_id, value.owner_id);
        let generation = selected_generation(
          self.hash_algorithm,
          pointer,
          &field_body.coverage,
          field_body.field_index_definition,
          dependency_fingerprint,
          combined_health(request.health, initial.repair_required),
          field_definition_fingerprint(self.hash_algorithm, field_body.field_index_definition),
        )?;
        let nvt_status = self.load_nvt_status(source, pointer, &field_bytes.bytes, cancellation)?;
        IndexCoverageRegistryEntryV1 {
          kind: request.kind,
          owner_id: request.owner_id.clone(),
          selection: IndexCoverageRegistrySelectionV1::Selected(generation),
          nvt_status,
        }
      }
    };
    let rechecked = load_pair_bounded(&self.memory, source, kind, &request.owner_id, cancellation)?;
    require_stable_pair(&initial, &rechecked)?;
    Ok(entry)
  }

  fn load_nvt_status(
    &self,
    source: &mut dyn IndexCoverageRegistrySourceV1,
    field_pointer: &LoadedIndexActivePointerV1,
    field_manifest_bytes: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<IndexCoverageNvtStatusV1, IndexCoverageRegistryErrorV1> {
    let initial = match load_pair_bounded(&self.memory, source, ActivePointerKindV1::FieldNvt, &field_pointer.owner_id, cancellation) {
      Ok(pair) => pair,
      Err(IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Unavailable { .. })) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::SourceUnavailable));
      }
      Err(IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Corrupt { .. })) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::CorruptSelection));
      }
      Err(IndexCoverageRegistryErrorV1::Memory(_)) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::ResourceLimit));
      }
      Err(error) => return Err(error),
    };
    let selected = match validate_pair(&initial, ActivePointerKindV1::FieldNvt, &field_pointer.owner_id, self.hash_algorithm) {
      Ok(selected) => selected,
      Err(IndexCoverageRegistryErrorV1::Corrupt { .. }) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::CorruptSelection));
      }
      Err(error) => return Err(error),
    };
    let Some(pointer) = selected else {
      return Ok(IndexCoverageNvtStatusV1::Unavailable(
        if initial.structurally_invalid_slots.iter().any(|value| *value) || initial.closure_invalid_slots.iter().any(|value| *value) {
          IndexCoverageNvtUnavailableReasonV1::CorruptSelection
        } else {
          IndexCoverageNvtUnavailableReasonV1::Absent
        },
      ));
    };
    let bytes = match load_manifest_bounded(&self.memory, source, &pointer.target_manifest_hash, cancellation) {
      Ok(bytes) => bytes,
      Err(IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Unavailable { .. })) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::SourceUnavailable));
      }
      Err(IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Corrupt { .. })) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::CorruptManifest));
      }
      Err(IndexCoverageRegistryErrorV1::Corrupt { .. }) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::CorruptManifest));
      }
      Err(IndexCoverageRegistryErrorV1::Memory(_)) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::ResourceLimit));
      }
      Err(error) => return Err(error),
    };
    let field = pin_field_index_v1(field_manifest_bytes, self.hash_algorithm)
      .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_field_pin", error.to_string()))?;
    let status = match validate_field_nvt_basis_v1(&field, Some(&bytes.bytes)) {
      NvtBasisStatusV1::Usable(nvt) => {
        if nvt.manifest_key != pointer.target_manifest_hash || nvt.generation != pointer.generation {
          IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::CorruptManifest)
        } else {
          IndexCoverageNvtStatusV1::Usable(IndexCoverageNvtDescriptorV1 {
            manifest_hash: nvt.manifest_key,
            generation: nvt.generation,
            resolution: nvt.resolution,
            tile_cells: nvt.tile_cells,
            tile_directory_root: nvt.tile_directory_root.map(ToOwned::to_owned),
          })
        }
      }
      NvtBasisStatusV1::Unavailable(fallback) => IndexCoverageNvtStatusV1::Unavailable(match fallback.reason {
        NvtFallbackReasonV1::Absent => IndexCoverageNvtUnavailableReasonV1::Absent,
        NvtFallbackReasonV1::IncompatibleOwner => IndexCoverageNvtUnavailableReasonV1::IncompatibleOwner,
        NvtFallbackReasonV1::StalePostingGeneration => IndexCoverageNvtUnavailableReasonV1::StalePostingGeneration,
        NvtFallbackReasonV1::StaleSourceHead => IndexCoverageNvtUnavailableReasonV1::StaleSourceRoot,
        NvtFallbackReasonV1::ResourceLimit => IndexCoverageNvtUnavailableReasonV1::ResourceLimit,
        NvtFallbackReasonV1::Corrupt | NvtFallbackReasonV1::StalePageHint | NvtFallbackReasonV1::MissingPredecessor => {
          IndexCoverageNvtUnavailableReasonV1::CorruptManifest
        }
      }),
    };
    let rechecked = match load_pair_bounded(&self.memory, source, ActivePointerKindV1::FieldNvt, &field_pointer.owner_id, cancellation) {
      Ok(pair) => pair,
      Err(IndexCoverageRegistryErrorV1::Cancelled) => return Err(IndexCoverageRegistryErrorV1::Cancelled),
      Err(IndexCoverageRegistryErrorV1::Memory(_)) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::ResourceLimit));
      }
      Err(IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Unavailable { .. })) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::SourceUnavailable));
      }
      Err(IndexCoverageRegistryErrorV1::Source(IndexCoverageRegistrySourceErrorV1::Corrupt { .. })) => {
        return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::CorruptSelection));
      }
      Err(error @ IndexCoverageRegistryErrorV1::Invalid { .. })
      | Err(error @ IndexCoverageRegistryErrorV1::Corrupt { .. })
      | Err(error @ IndexCoverageRegistryErrorV1::SelectionChanged)
      | Err(error @ IndexCoverageRegistryErrorV1::RefreshBusy)
      | Err(error @ IndexCoverageRegistryErrorV1::Poisoned { .. }) => return Err(error),
    };
    if initial != rechecked {
      return Ok(IndexCoverageNvtStatusV1::Unavailable(IndexCoverageNvtUnavailableReasonV1::SelectionChanged));
    }
    Ok(status)
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum IndexCoverageRegistrySourceErrorV1 {
  #[error("registry source is corrupt at {code}: {message}")]
  Corrupt { code: &'static str, message: String },
  #[error("registry source is unavailable at {code}: {message}")]
  Unavailable { code: &'static str, message: String },
}

impl IndexCoverageRegistrySourceErrorV1 {
  pub fn corrupt(code: &'static str, message: impl Into<String>) -> Self {
    Self::Corrupt { code, message: message.into() }
  }

  pub fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
    Self::Unavailable { code, message: message.into() }
  }
}

pub trait IndexCoverageRegistrySourceV1 {
  fn hash_algorithm(&self) -> HashAlgorithm;
  fn database_id(&self) -> [u8; 16];
  fn load_active_pointer_pair(
    &mut self,
    kind: ActivePointerKindV1,
    owner_id: &[u8],
  ) -> Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistrySourceErrorV1>;
  fn load_artifact_bounded(
    &mut self,
    key: &[u8],
    maximum_value_length: usize,
  ) -> Result<Option<Vec<u8>>, IndexCoverageRegistrySourceErrorV1>;
}

pub struct FirstAuthorityIndexCoverageRegistrySourceV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
}

impl FirstAuthorityIndexCoverageRegistrySourceV1 {
  pub fn new(publisher: Arc<V4FirstAuthorityPublisher>) -> Result<Self, IndexCoverageRegistrySourceErrorV1> {
    let observation = publisher.observe().map_err(map_first_authority_source_error)?;
    let header = &observation.selected.header;
    if header.database_id.iter().all(|byte| *byte == 0) {
      return Err(IndexCoverageRegistrySourceErrorV1::corrupt(
        "index_coverage_source_database",
        "selected first authority has a zero database identity",
      ));
    }
    Ok(Self { publisher, hash_algorithm: header.hash_algorithm, database_id: header.database_id })
  }
}

impl IndexCoverageRegistrySourceV1 for FirstAuthorityIndexCoverageRegistrySourceV1 {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  fn load_active_pointer_pair(
    &mut self,
    kind: ActivePointerKindV1,
    owner_id: &[u8],
  ) -> Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistrySourceErrorV1> {
    self.publisher.load_index_active_pointer_pair(&self.database_id, kind, owner_id).map_err(map_first_authority_source_error)
  }

  fn load_artifact_bounded(
    &mut self,
    key: &[u8],
    maximum_value_length: usize,
  ) -> Result<Option<Vec<u8>>, IndexCoverageRegistrySourceErrorV1> {
    self.publisher.load_index_artifact_bounded(key, maximum_value_length).map_err(map_first_authority_source_error)
  }
}

#[derive(Debug, Error)]
pub enum IndexCoverageRegistryErrorV1 {
  #[error("index coverage registry construction was cancelled")]
  Cancelled,
  #[error("index coverage registry rejected {code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("index coverage registry found corrupt selected state at {code}: {message}")]
  Corrupt { code: &'static str, message: String },
  #[error("index coverage registry source failed: {0}")]
  Source(#[from] IndexCoverageRegistrySourceErrorV1),
  #[error("index coverage registry selected pointer changed during refresh")]
  SelectionChanged,
  #[error("index coverage registry refresh already has an active owner")]
  RefreshBusy,
  #[error("index coverage registry memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("index coverage registry lock is poisoned: {message}")]
  Poisoned { message: String },
}

impl IndexCoverageRegistryErrorV1 {
  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn corrupt(code: &'static str, message: impl Into<String>) -> Self {
    Self::Corrupt { code, message: message.into() }
  }
}

struct LoadedManifestBytesV1 {
  bytes: Vec<u8>,
  _reservation: MemoryReservation,
}

fn load_pair_bounded(
  memory: &MemoryCoordinator,
  source: &mut dyn IndexCoverageRegistrySourceV1,
  kind: ActivePointerKindV1,
  owner_id: &[u8],
  cancellation: &CancellationToken,
) -> Result<LoadedIndexActivePointerPairV1, IndexCoverageRegistryErrorV1> {
  require_not_cancelled(cancellation)?;
  let _reservation = memory.reserve(MemoryOwner::IndexCleanCache, POINTER_SELECTION_TRANSIENT_BYTES, AdmissionClass::Maintenance)?;
  let pair = source.load_active_pointer_pair(kind, owner_id)?;
  require_not_cancelled(cancellation)?;
  Ok(pair)
}

fn load_manifest_bounded(
  memory: &MemoryCoordinator,
  source: &mut dyn IndexCoverageRegistrySourceV1,
  key: &[u8],
  cancellation: &CancellationToken,
) -> Result<LoadedManifestBytesV1, IndexCoverageRegistryErrorV1> {
  require_not_cancelled(cancellation)?;
  let maximum = ImmutableIndexArtifactKindV1::FieldIndexManifest.maximum_encoded_length();
  debug_assert_eq!(maximum, MANIFEST_READ_CAP);
  let reservation = memory.reserve(MemoryOwner::IndexCleanCache, MANIFEST_READ_RESERVATION_BYTES, AdmissionClass::Maintenance)?;
  let bytes = source.load_artifact_bounded(key, maximum)?.ok_or_else(|| {
    IndexCoverageRegistryErrorV1::corrupt("index_coverage_manifest_missing", "selected manifest closure is missing an immutable artifact")
  })?;
  if bytes.len() > maximum {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_manifest_oversize",
      "registry source returned manifest bytes above its requested bound",
    ));
  }
  require_not_cancelled(cancellation)?;
  Ok(LoadedManifestBytesV1 { bytes, _reservation: reservation })
}

fn validate_pair<'a>(
  pair: &'a LoadedIndexActivePointerPairV1,
  kind: ActivePointerKindV1,
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> Result<Option<&'a LoadedIndexActivePointerV1>, IndexCoverageRegistryErrorV1> {
  let Some(selected) = pair.selected.as_ref() else {
    if pair.repair_required {
      return Err(IndexCoverageRegistryErrorV1::corrupt(
        "index_coverage_pair_repair_without_selection",
        "active-pointer pair requires repair without selecting a closure-valid slot",
      ));
    }
    if pair.slots.iter().any(Option::is_some) {
      return Err(IndexCoverageRegistryErrorV1::corrupt(
        "index_coverage_pair_unselected_slot",
        "active-pointer pair retained a structurally valid slot without selecting one",
      ));
    }
    return Ok(None);
  };
  if selected.kind != kind
    || selected.owner_id != owner_id
    || selected.generation == 0
    || selected.pointer_sequence == 0
    || selected.write_sequence == 0
    || selected.selected_slot > 1
    || selected.target_manifest_hash.len() != hash_algorithm.hash_length()
    || selected.target_manifest_hash.iter().all(|byte| *byte == 0)
  {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_selected_pointer",
      "selected active pointer disagrees with the requested owner or hash profile",
    ));
  }
  if pair.structurally_invalid_slots[usize::from(selected.selected_slot)] || pair.closure_invalid_slots[usize::from(selected.selected_slot)]
  {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_selected_slot_flags",
      "selected active-pointer slot is also marked structurally or closure invalid",
    ));
  }
  let decoded = decode_active_pointer(&selected.bytes, hash_algorithm)
    .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_selected_pointer_bytes", error.to_string()))?;
  if decoded.kind != selected.kind
    || decoded.owner_id != selected.owner_id
    || decoded.generation != selected.generation
    || decoded.slot != selected.selected_slot
    || decoded.sequence != selected.pointer_sequence
    || decoded.target_manifest_hash != selected.target_manifest_hash
  {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_selected_pointer_bytes",
      "selected pointer metadata disagrees with its canonical stored bytes",
    ));
  }
  if pair.slots.get(usize::from(selected.selected_slot)).and_then(Option::as_ref) != Some(selected) {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_selected_slot",
      "selected pointer does not match its retained stable slot",
    ));
  }
  Ok(Some(selected))
}

fn require_stable_pair(
  initial: &LoadedIndexActivePointerPairV1,
  rechecked: &LoadedIndexActivePointerPairV1,
) -> Result<(), IndexCoverageRegistryErrorV1> {
  if initial != rechecked {
    return Err(IndexCoverageRegistryErrorV1::SelectionChanged);
  }
  Ok(())
}

fn require_manifest_pointer(
  pointer: &LoadedIndexActivePointerV1,
  manifest_key: &[u8],
  manifest_owner: &[u8],
  manifest_generation: u64,
) -> Result<(), IndexCoverageRegistryErrorV1> {
  if pointer.target_manifest_hash != manifest_key || pointer.owner_id != manifest_owner || pointer.generation != manifest_generation {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_manifest_pointer",
      "selected pointer does not identify the decoded manifest",
    ));
  }
  Ok(())
}

fn selected_generation(
  hash_algorithm: HashAlgorithm,
  pointer: &LoadedIndexActivePointerV1,
  coverage: &super::index_artifact::CoverageVersionV1<'_>,
  definition: &[u8],
  dependency_fingerprint: Vec<u8>,
  health: IndexCoverageGenerationHealthV1,
  definition_fingerprint: Vec<u8>,
) -> Result<IndexCoverageRegistryGenerationV1, IndexCoverageRegistryErrorV1> {
  let coverage_epoch_id: [u8; 16] = coverage
    .coverage_epoch_id
    .try_into()
    .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_epoch", format!("coverage epoch width is invalid: {error}")))?;
  if coverage.source_namespace_root.len() != hash_algorithm.hash_length()
    || coverage.source_namespace_root.iter().all(|byte| *byte == 0)
    || coverage.coverage_publication_sequence == 0
  {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_version",
      "selected manifest coverage root or publication sequence is invalid",
    ));
  }
  if definition.is_empty()
    || definition_fingerprint.len() != hash_algorithm.hash_length()
    || dependency_fingerprint.len() != hash_algorithm.hash_length()
  {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_fingerprint",
      "selected definition or derived planning fingerprints are invalid",
    ));
  }
  Ok(IndexCoverageRegistryGenerationV1 {
    generation: pointer.generation,
    owner_id: pointer.owner_id.clone(),
    manifest_hash: pointer.target_manifest_hash.clone(),
    pointer_sequence: pointer.pointer_sequence,
    source_namespace_root: coverage.source_namespace_root.to_vec(),
    coverage_epoch_id,
    coverage_publication_sequence: coverage.coverage_publication_sequence,
    definition_fingerprint,
    dependency_fingerprint,
    health,
  })
}

pub fn scope_definition_fingerprint(hash_algorithm: HashAlgorithm, definition: &[u8]) -> Vec<u8> {
  digest_parts(hash_algorithm, &[b"aeordb.index-coverage.scope-definition.v1\0", definition])
}

pub fn field_definition_fingerprint(hash_algorithm: HashAlgorithm, definition: &[u8]) -> Vec<u8> {
  digest_parts(hash_algorithm, &[b"aeordb.index-coverage.field-definition.v1\0", definition])
}

pub fn scope_dependency_fingerprint(hash_algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(hash_algorithm, &[b"aeordb.index-coverage.scope-dependencies.v1\0"])
}

pub fn field_dependency_fingerprint(hash_algorithm: HashAlgorithm, scope_owner_id: &[u8], value_owner_id: &[u8]) -> Vec<u8> {
  digest_parts(hash_algorithm, &[b"aeordb.index-coverage.field-dependencies.v1\0", scope_owner_id, value_owner_id])
}

fn combined_health(requested: IndexCoverageGenerationHealthV1, repair_required: bool) -> IndexCoverageGenerationHealthV1 {
  if requested == IndexCoverageGenerationHealthV1::Degraded || repair_required {
    IndexCoverageGenerationHealthV1::Degraded
  } else {
    IndexCoverageGenerationHealthV1::Healthy
  }
}

fn require_readable_capabilities(required: [u8; 32]) -> Result<(), IndexCoverageRegistryErrorV1> {
  let required = CapabilitySetV1::from_bytes(required)
    .map_err(|error| IndexCoverageRegistryErrorV1::corrupt("index_coverage_manifest_capabilities", error.to_string()))?;
  if !required.difference(BinaryCapabilityProfileV1::current().supported_reader_capabilities).is_empty() {
    return Err(IndexCoverageRegistryErrorV1::corrupt(
      "index_coverage_manifest_capabilities",
      "selected manifest requires unsupported reader capabilities",
    ));
  }
  Ok(())
}

fn validate_requests(
  hash_algorithm: HashAlgorithm,
  options: IndexCoverageRegistryOptionsV1,
  requests: &[IndexCoverageRegistryOwnerRequestV1],
) -> Result<(), IndexCoverageRegistryErrorV1> {
  if requests.len() > options.maximum_entries {
    return Err(IndexCoverageRegistryErrorV1::invalid(
      "index_coverage_registry_entry_count",
      "registry request count exceeds the configured bound",
    ));
  }
  let hash_width = hash_algorithm.hash_length();
  for request in requests {
    if request.owner_id.len() != hash_width || request.owner_id.iter().all(|byte| *byte == 0) {
      return Err(IndexCoverageRegistryErrorV1::invalid(
        "index_coverage_registry_owner_width",
        "registry owner identity disagrees with the database hash profile",
      ));
    }
  }
  if requests.windows(2).any(|pair| (pair[0].kind, pair[0].owner_id.as_slice()) >= (pair[1].kind, pair[1].owner_id.as_slice())) {
    return Err(IndexCoverageRegistryErrorV1::invalid(
      "index_coverage_registry_owner_order",
      "registry owner requests must be strictly ordered and unique",
    ));
  }
  Ok(())
}

fn snapshot_retained_bound(hash_algorithm: HashAlgorithm, entries: usize) -> Result<u64, IndexCoverageRegistryErrorV1> {
  let fixed = u64::try_from(size_of::<IndexCoverageRegistrySnapshotV1>())
    .map_err(|error| {
      IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", format!("registry snapshot size exceeds u64: {error}"))
    })?
    .checked_add(RETAINED_SNAPSHOT_ALLOCATION_ALLOWANCE)
    .ok_or_else(|| IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", "registry snapshot allowance overflowed"))?;
  let hash_bytes = u64::try_from(hash_algorithm.hash_length())
    .map_err(|error| IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", format!("hash width exceeds u64: {error}")))?;
  let per_entry = RETAINED_ENTRY_FIXED_ALLOWANCE
    .checked_add(
      RETAINED_ENTRY_HASH_ALLOWANCE
        .checked_mul(hash_bytes)
        .ok_or_else(|| IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", "registry entry hash allowance overflowed"))?,
    )
    .ok_or_else(|| IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", "registry entry allowance overflowed"))?;
  fixed
    .checked_add(
      per_entry
        .checked_mul(u64::try_from(entries).map_err(|error| {
          IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", format!("registry entry count exceeds u64: {error}"))
        })?)
        .ok_or_else(|| IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", "registry retained-byte bound overflowed"))?,
    )
    .ok_or_else(|| IndexCoverageRegistryErrorV1::invalid("index_coverage_registry_size", "registry retained-byte total overflowed"))
}

fn require_snapshot_bound(options: IndexCoverageRegistryOptionsV1, retained_bytes: u64) -> Result<(), IndexCoverageRegistryErrorV1> {
  if retained_bytes > options.maximum_retained_bytes {
    return Err(IndexCoverageRegistryErrorV1::invalid(
      "index_coverage_registry_bytes",
      "registry snapshot exceeds the configured retained-byte bound",
    ));
  }
  Ok(())
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), IndexCoverageRegistryErrorV1> {
  if cancellation.is_cancelled() {
    return Err(IndexCoverageRegistryErrorV1::Cancelled);
  }
  Ok(())
}

fn map_first_authority_source_error(error: FirstAuthorityPublicationErrorV1) -> IndexCoverageRegistrySourceErrorV1 {
  let code = error.code();
  let message = error.to_string();
  match error {
    FirstAuthorityPublicationErrorV1::Invalid { .. } if first_authority_read_failure_is_operational(code) => {
      IndexCoverageRegistrySourceErrorV1::unavailable(code, message)
    }
    FirstAuthorityPublicationErrorV1::Invalid { .. } | FirstAuthorityPublicationErrorV1::Format(_) => {
      IndexCoverageRegistrySourceErrorV1::corrupt(code, message)
    }
    FirstAuthorityPublicationErrorV1::Committed { .. }
    | FirstAuthorityPublicationErrorV1::Engine(_)
    | FirstAuthorityPublicationErrorV1::Header(_)
    | FirstAuthorityPublicationErrorV1::StateLockPoisoned => IndexCoverageRegistrySourceErrorV1::unavailable(code, message),
  }
}

fn first_authority_read_failure_is_operational(code: &str) -> bool {
  matches!(
    code,
    "first_authority_readback_allocation"
      | "first_authority_readback_io"
      | "immutable_index_read_allocation"
      | "index_active_pointer_read_allocation"
      | "index_active_pointer_read_io"
  )
}
