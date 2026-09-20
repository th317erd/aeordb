//! Captured task discovery is not a resume, root admission or GC closure permit.
#[path = "semantic_task_graph_native.rs"]
mod task_graph;
pub use task_graph::{NativeSemanticTaskGraphBoundsV1, SemanticTaskGraphErrorV1, SemanticTaskGraphSummaryV1};
#[path = "semantic_source_capture_staging.rs"]
mod source_capture_staging;
pub use source_capture_staging::NativeCapturedSemanticCheckpointRequestV1;
pub use source_capture_staging::{
  NativeSemanticSourceUnionStagingRequestV1, NativeStagedSemanticSourceUnionV1, SemanticSourceUnionStagingSummaryV1,
};
#[path = "semantic_source_control_staging.rs"]
mod source_control_staging;
pub use source_control_staging::{NativeSemanticSourceControlPublicationErrorV1, NativeSemanticSourceNodeStagingRequestV1};
#[path = "semantic_source_native.rs"]
mod protected_sources;
pub use protected_sources::{NativeSemanticSourceUnionValidationBoundsV1, SemanticSourceUnionValidationSummaryV1};
pub use protected_sources::{NativeSemanticCompilerProgressBoundsV1, NativeSemanticCompilerProgressV1, SemanticCompilerConstructionModeV1};
#[path = "semantic_source_base_native.rs"]
mod source_base;
pub use protected_sources::NativeSemanticNamespaceSourceCursorV1;
pub use protected_sources::{
  NativeSemanticSourceReplacementV1, NativeSemanticSourceUnionBoundsV1, NativeSemanticSourceUnionErrorV1,
  NativeSemanticSourceUnionRequestV1, NativeSemanticSourceUnionV1,
};
pub use protected_sources::{
  NativeSemanticNamespaceSourceBoundsV1, NativeSemanticNamespaceSourceErrorV1, NativeSemanticNamespaceSourceRequestV1,
  NativeSemanticNamespaceSourceSummaryV1, NativeSemanticNamespaceSourceV1,
};
pub use protected_sources::{NativeSemanticAliasSnapshotRequestV1, NativeSemanticAliasSnapshotV1};
pub use protected_sources::{NativeSemanticPluginSourceBoundsV1, NativeSemanticPluginSourceErrorV1, NativeSemanticPluginSourcesV1};
pub use protected_sources::{NativeProtectedSemanticSourceV1, NativeSemanticSourceReadBoundsV1, NativeSemanticSourcePublicationErrorV1};
#[path = "semantic_source_catalog_native.rs"]
mod source_catalog;
pub use source_catalog::{
  SemanticSourceLookupDispositionV1, NativeSemanticSourceCatalogBoundsV1, NativeSemanticSourceLookupV1, SemanticSourceCatalogSideV1,
  SemanticSourceCatalogSummaryV1,
};
use super::*;
use std::cell::Cell;
use crate::engine::kv_snapshot::ReadSnapshot;
use crate::engine::kv_pages::page_size;
use super::super::super::control_store::select_available_mutable_control_slots;
use super::super::super::reader::MalformedInputClass;
use super::super::super::system_control::CONTROL_ROOT;
use super::super::super::system_family::{SystemFamilyPolicyResolverV1, SystemFamilySubjectV1};

// A maximum-size raw module still has a WholeEntity header, key and checksum.
// This is an operational read ceiling, not a larger source-payload wire limit.
const MAXIMUM_ENTITY_BYTES: usize = (64 << 20) + 8192;
const CAPTURE_SCRATCH_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticMutationInventoryBoundsV1 {
  pub maximum_work: u64,
  pub maximum_entity_bytes: usize,
  pub maximum_read_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticMutationInventorySummaryV1 {
  pub tasks: u64,
  pub complete: bool,
}

pub struct NativeSemanticMutationInventoryV1<'a> {
  _protection: &'a NativeStagingProtectionV1<'a>,
  header: DatabaseHeaderObservationV4,
  snapshot: Arc<ReadSnapshot>,
  bounds: NativeSemanticMutationInventoryBoundsV1,
  scan_scratch_bytes: u64,
  cancellation: CancellationToken,
  memory: MemoryCoordinator,
  _memory: MemoryReservation,
}

impl NativeStagingProtectionV1<'_> {
  pub fn capture_semantic_mutation_inventory(
    &self,
    bounds: NativeSemanticMutationInventoryBoundsV1,
    memory: &MemoryCoordinator,
    cancellation: &CancellationToken,
  ) -> Result<NativeSemanticMutationInventoryV1<'_>, SemanticMutationObservationErrorV1> {
    check_cancelled(cancellation)?;
    if bounds.maximum_work == 0
      || bounds.maximum_read_bytes == 0
      || bounds.maximum_entity_bytes == 0
      || bounds.maximum_entity_bytes > MAXIMUM_ENTITY_BYTES
    {
      return Err(invalid("semantic_task_inventory_bounds", "captured inventory requires positive bounded work and entity/read limits"));
    }
    // One whole entity plus its FileRecord decode/keys, canonical A/B discovery
    // scratch and fixed header/page buffers. Returned per-task observations
    // retain their own existing reservation separately.
    let scratch = (bounds.maximum_entity_bytes as u64)
      .checked_mul(4)
      .and_then(|bytes| bytes.checked_add(4 * SystemControlKindV1::SemanticMutationTask.encoded_cap() as u64))
      .and_then(|bytes| bytes.checked_add(4 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP as u64 + CAPTURE_SCRATCH_BYTES))
      .ok_or_else(|| invalid("semantic_task_inventory_memory_bound", "captured inventory memory bound overflowed"))?;
    let mut reservation = memory.reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)?;
    let publisher = self.publisher();
    let (header, snapshot) = {
      let authority = publisher.root_state.lock().map_err(|poisoned| {
        drop(poisoned);
        FirstAuthorityPublicationErrorV1::StateLockPoisoned
      })?;
      check_cancelled(cancellation)?;
      reservation.check_admission()?;
      if authority.staging_accounting_failed || authority.active_staging_protections == 0 {
        return Err(invalid("semantic_task_inventory_protection", "captured inventory has no healthy staging protection"));
      }
      let header = publisher.observe()?;
      if header.selected.redundancy_degraded {
        return Err(invalid("semantic_task_inventory_header", "captured inventory requires a non-degraded selected header"));
      }
      let kv = publisher.lock_kv()?;
      validate_kv_header_alignment(&kv, &header.selected.header)?;
      let snapshot = kv.capture_settled_snapshot().map_err(FirstAuthorityPublicationErrorV1::from)?;
      if snapshot.hash_algo() != header.selected.header.hash_algorithm
        || snapshot.len() as u64 != header.selected.header.entry_count
        || snapshot.bucket_count() != kv.bucket_count()
      {
        return Err(invalid("semantic_task_inventory_snapshot", "captured snapshot disagrees with the selected physical owner"));
      }
      (header, snapshot)
    };
    // Charge retained buffer/NVT/page ownership outside the short capture lock.
    // Shared provider history continues to use its existing generation owner.
    reservation.grow(snapshot.memory_stats().total_bytes())?;
    let scan_scratch_bytes = scratch
      .checked_add(4 * page_size(header.selected.header.hash_algorithm.hash_length()) as u64)
      .ok_or_else(|| invalid("semantic_task_inventory_memory_bound", "captured inventory page scratch overflowed"))?;
    // Capture first proves that its configured scratch can be admitted. Keep
    // only the retained snapshot and small capture/header charge afterwards;
    // each active visit must own separate scratch, including nested callbacks.
    reservation.shrink(scratch - CAPTURE_SCRATCH_BYTES)?;
    check_cancelled(cancellation)?;
    Ok(NativeSemanticMutationInventoryV1 {
      _protection: self,
      header,
      snapshot,
      bounds,
      scan_scratch_bytes,
      cancellation: cancellation.clone(),
      memory: memory.clone(),
      _memory: reservation,
    })
  }
}

fn invalid(code: &'static str, message: &'static str) -> SemanticMutationObservationErrorV1 {
  SemanticMutationObservationErrorV1::Invalid { code, message }
}

struct CapturedEntityLookupV1<'a> {
  snapshot: &'a ReadSnapshot,
  header: &'a DatabaseHeaderV4,
  bounds: NativeSemanticMutationInventoryBoundsV1,
  cancellation: &'a CancellationToken,
  remaining_read_bytes: Cell<u64>,
}

impl FirstAuthorityEntityLookupV1 for CapturedEntityLookupV1<'_> {
  fn get(&self, key: &[u8]) -> Result<Option<KVEntry>, EngineError> {
    if self.cancellation.is_cancelled() {
      return Err(EngineError::Cancelled("captured semantic task lookup".to_string()));
    }
    self.snapshot.get(key)
  }

  fn hash_algo(&self) -> HashAlgorithm {
    self.header.hash_algorithm
  }

  fn admit_read(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(EngineError::Cancelled("captured semantic task read".to_string()).into());
    }
    let length = u64::from(locator.total_length);
    if length > self.bounds.maximum_entity_bytes as u64 {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "semantic_task_inventory_entity_bound",
        "captured entity exceeds the admitted byte bound",
      ));
    }
    self.validate_reference_extent(locator)?;
    let remaining = self.remaining_read_bytes.get().checked_sub(length).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid(
        "semantic_task_inventory_read_bound",
        "captured inventory exhausted its total read-byte bound",
      )
    })?;
    self.remaining_read_bytes.set(remaining);
    Ok(())
  }
}

impl CapturedEntityLookupV1<'_> {
  /// Geometry shared by full reads and opaque leaf references. This does not
  /// assert that the referenced payload has been read or integrity-verified.
  fn validate_reference_extent(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let length = u64::from(locator.total_length);
    let end = locator
      .offset
      .checked_add(length)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("semantic_task_inventory_extent", "captured entity extent overflowed"))?;
    let kv_end = self
      .header
      .kv_block_offset
      .checked_add(self.header.kv_block_length)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("semantic_task_inventory_extent", "captured KV extent overflowed"))?;
    if locator.offset < (2 * super::super::super::database_header::DATABASE_HEADER_V4_SLOT_LENGTH) as u64
      || end > self.header.hot_tail_offset
      || (locator.offset < kv_end && end > self.header.kv_block_offset)
    {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "semantic_task_inventory_extent",
        "captured entity is outside the selected data region",
      ));
    }
    Ok(())
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Stream provisional task observations. Only a complete successful result
  /// proves this captured current-KV inventory was exhausted; callbacks cannot
  /// grant GC closure or resume authority. Each simultaneous visit is admitted
  /// separately, without holding the publisher's root or KV mutex.
  pub fn visit(
    &self,
    mut visitor: impl FnMut(&SemanticMutationObservationV1) -> Result<bool, SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticMutationInventorySummaryV1, SemanticMutationObservationErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission()?;
    let scan_memory = self.memory.reserve(MemoryOwner::Task, self.scan_scratch_bytes, AdmissionClass::Maintenance)?;
    let lookup = CapturedEntityLookupV1 {
      snapshot: &self.snapshot,
      header: &self.header.selected.header,
      bounds: self.bounds,
      cancellation: &self.cancellation,
      remaining_read_bytes: Cell::new(self.bounds.maximum_read_bytes),
    };
    let mut tasks = 0u64;
    let mut failure = None;
    let result = self.snapshot.visit_captured_entries(&self.cancellation, self.bounds.maximum_work, |entry| {
      let result = (|| {
        scan_memory.check_admission()?;
        let Some(observation) = self.inspect_entry(&lookup, entry)? else {
          return Ok(true);
        };
        tasks = tasks.checked_add(1).ok_or_else(|| invalid("semantic_task_inventory_task_count", "captured task count overflowed"))?;
        visitor(&observation)
      })();
      match result {
        Ok(keep_scanning) => Ok(keep_scanning),
        Err(error) => {
          failure = Some(error);
          Ok(false)
        }
      }
    });
    if let Some(error) = failure {
      return Err(error);
    }
    let summary = result.map_err(FirstAuthorityPublicationErrorV1::from)?;
    check_cancelled(&self.cancellation)?;
    scan_memory.check_admission()?;
    Ok(SemanticMutationInventorySummaryV1 { tasks, complete: summary.complete })
  }

  fn inspect_entry(
    &self,
    lookup: &CapturedEntityLookupV1<'_>,
    locator: &KVEntry,
  ) -> Result<Option<SemanticMutationObservationV1>, SemanticMutationObservationErrorV1> {
    let header = &self.header.selected.header;
    let file = &self._protection.publisher().file;
    let bytes = read_entity_bounded(file, lookup, &locator.hash, self.bounds.maximum_entity_bytes, header.write_sequence_high_water)?
      .ok_or_else(|| invalid("semantic_task_inventory_missing", "captured live entry cannot be resolved from the same snapshot"))?;
    let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    // KV tags are not WholeEntity type IDs. Bind the frozen registries
    // explicitly instead of filtering on an unverified tag and hiding a task.
    let expected_tag = match entity.entry_type {
      EntryTypeV4::Chunk => kv_tag::CHUNK,
      EntryTypeV4::FileRecord => kv_tag::FILE_RECORD,
      EntryTypeV4::DirectoryIndex => kv_tag::DIRECTORY,
      EntryTypeV4::DeletionRecord => kv_tag::DELETION,
      EntryTypeV4::Snapshot => kv_tag::SNAPSHOT,
      EntryTypeV4::Void => kv_tag::VOID,
      EntryTypeV4::Fork => kv_tag::FORK,
      EntryTypeV4::Symlink => kv_tag::SYMLINK,
      EntryTypeV4::IndexArtifact => kv_tag::INDEX_ARTIFACT,
      EntryTypeV4::GcArtifact => kv_tag::GC_ARTIFACT,
    };
    if locator.type_flags != expected_tag {
      return Err(invalid("semantic_task_inventory_role", "captured KV role disagrees with its checked WholeEntity"));
    }
    if entity.entry_type != EntryTypeV4::FileRecord {
      return Ok(None);
    }
    if entity.compression_algorithm != CompressionAlgorithm::None {
      return Err(invalid("semantic_task_inventory_file_encoding", "captured FileRecord has unsupported compression"));
    }
    let record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), entity.entity_version)
      .map_err(FirstAuthorityPublicationErrorV1::from)?;
    SystemFamilyPolicyResolverV1::embedded(header.hash_algorithm)?
      .policy(SystemFamilySubjectV1::Path(&record.path), "captured semantic task discovery")?;
    let is_control_path =
      record.path == CONTROL_ROOT || record.path.strip_prefix(CONTROL_ROOT).is_some_and(|suffix| suffix.starts_with('/'));
    let is_control_content = record.content_type.as_deref() == Some(SYSTEM_CONTROL_CONTENT_TYPE);
    if !is_control_path && !is_control_content {
      return Ok(None);
    }
    if !is_control_path || !is_control_content || locator.hash != first_authority_file_path_hash(&record.path, header.hash_algorithm) {
      return Err(invalid("semantic_task_inventory_control_path", "system control path, content type and physical key disagree"));
    }
    self.inspect_control(lookup, &record.path)
  }

  fn inspect_control(
    &self,
    lookup: &CapturedEntityLookupV1<'_>,
    path: &str,
  ) -> Result<Option<SemanticMutationObservationV1>, SemanticMutationObservationErrorV1> {
    let (kind, slot, parent) = parse_control_path(path)?;
    let header = &self.header.selected.header;
    let file = &self._protection.publisher().file;
    if kind.is_immutable() {
      let loaded = load_canonical_system_file_at_path(file, lookup, header, path, SYSTEM_CONTROL_CONTENT_TYPE, kind.encoded_cap())?
        .ok_or_else(|| invalid("semantic_task_inventory_control_missing", "captured immutable control disappeared"))?;
      let control = decode_system_control(&loaded.body, header.hash_algorithm)?;
      if control.kind != kind || control.database_id != header.database_id || control.canonical_path_for_slot(slot)? != path {
        return Err(
          FirstAuthorityPublicationErrorV1::invalid(
            "immutable_system_control_stored_mismatch",
            "stored immutable control does not match its canonical kind, database, and identity",
          )
          .into(),
        );
      }
      return Ok(None);
    }
    let a_path = format!("{parent}/{}", SystemControlSlotV1::A.file_name());
    let b_path = format!("{parent}/{}", SystemControlSlotV1::B.file_name());
    let a = load_canonical_system_file_at_path(file, lookup, header, &a_path, SYSTEM_CONTROL_CONTENT_TYPE, kind.encoded_cap())?;
    let b = load_canonical_system_file_at_path(file, lookup, header, &b_path, SYSTEM_CONTROL_CONTENT_TYPE, kind.encoded_cap())?;
    let selected = select_available_mutable_control_slots(
      header.hash_algorithm,
      a.as_ref().map(|loaded| loaded.body.as_slice()),
      b.as_ref().map(|loaded| loaded.body.as_slice()),
    )?
    .ok_or_else(|| invalid("semantic_task_inventory_control_missing", "captured mutable control pair disappeared"))?;
    let selected_path = if selected.selected_slot == SystemControlSlotV1::A { &a_path } else { &b_path };
    if selected.control.kind != kind
      || selected.control.database_id != header.database_id
      || selected.control.canonical_path_for_slot(selected.selected_slot)? != *selected_path
    {
      return Err(invalid(
        "semantic_task_inventory_control_identity",
        "selected control identity does not bind its captured canonical path",
      ));
    }
    if kind != SystemControlKindV1::SemanticMutationTask || selected.selected_slot != slot {
      return Ok(None);
    }
    let task_id: &[u8; 16] = selected.control.identity.as_slice().try_into().map_err(|source| {
      FormatError::new(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "semantic_task_inventory_task_identity",
        format!("selected semantic task identity is not sixteen bytes: {source}"),
      )
    })?;
    let selected_bytes = if selected.selected_slot == SystemControlSlotV1::A { a.as_ref() } else { b.as_ref() }
      .ok_or_else(|| invalid("semantic_task_inventory_control_missing", "selected captured slot is absent"))?;
    let observation_memory = reserve_observation_memory(&self.memory)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(selected_bytes.body.len()).map_err(|source| {
      FormatError::allocation_failure("semantic_task_inventory_selection_allocation", format!("cannot retain selected task: {source}"))
    })?;
    bytes.extend_from_slice(&selected_bytes.body);
    let task = LoadedMutableSystemControlV1 {
      selected_slot: selected.selected_slot,
      control_sequence: selected.control.sequence,
      control_digest: mutable_system_control_digest(header.hash_algorithm, &bytes),
      redundancy_degraded: selected.redundancy_degraded,
      bytes,
    };
    complete_semantic_mutation_observation(
      file,
      lookup,
      self.header.clone(),
      Some(task),
      SemanticMutationObservationRequestV1 {
        database_id: &header.database_id,
        task_id,
        memory: &self.memory,
        cancellation: &self.cancellation,
      },
      observation_memory,
    )
    .map(Some)
  }
}

fn parse_control_path(path: &str) -> Result<(SystemControlKindV1, SystemControlSlotV1, &str), SemanticMutationObservationErrorV1> {
  let suffix = path
    .strip_prefix(CONTROL_ROOT)
    .and_then(|suffix| suffix.strip_prefix('/'))
    .ok_or_else(|| invalid("semantic_task_inventory_control_path", "invalid system-control root"))?;
  let mut parts = suffix.split('/');
  let (Some(kind_text), Some(digest), Some(slot_text)) = (parts.next(), parts.next(), parts.next()) else {
    return Err(invalid("semantic_task_inventory_control_path", "incomplete system-control kind, identity digest or slot"));
  };
  if parts.next().is_some()
    || kind_text.len() != 4
    || digest.len() != 64
    || !kind_text.bytes().chain(digest.bytes()).all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
  {
    return Err(invalid("semantic_task_inventory_control_path", "noncanonical system-control kind or identity digest"));
  }
  let kind_id = u16::from_str_radix(kind_text, 16).map_err(|source| {
    FormatError::new(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "semantic_task_inventory_control_path",
      format!("invalid system-control kind: {source}"),
    )
  })?;
  let kind = SystemControlKindV1::from_u16(kind_id)
    .ok_or_else(|| invalid("unknown_protected_system_family", "unknown system-control kind in captured inventory"))?;
  let slot = match slot_text {
    "a.ctrl" => SystemControlSlotV1::A,
    "b.ctrl" => SystemControlSlotV1::B,
    "i.ctrl" => SystemControlSlotV1::Immutable,
    _ => return Err(invalid("semantic_task_inventory_control_path", "unknown system-control slot name")),
  };
  if kind.is_immutable() != (slot == SystemControlSlotV1::Immutable) {
    return Err(invalid("semantic_task_inventory_control_path", "system-control slot disagrees with its mutability"));
  }
  let (parent, _) = path.rsplit_once('/').ok_or_else(|| invalid("semantic_task_inventory_control_path", "missing control parent"))?;
  Ok((kind, slot, parent))
}
