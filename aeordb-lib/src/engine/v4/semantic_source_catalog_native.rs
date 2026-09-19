//! Read-only captured catalog observations, never publication or GC permits.
use super::*;
use std::cell::RefCell;
use crate::engine::directory_entry::ChildEntry;
use crate::engine::v4::namespace_seek::{namespace_seek_workspace_bytes_v1, seek_namespace_child_v1, NamespaceSeekFailureV1};
use crate::engine::v4::semantic_source_capture::{decode_semantic_source_capture_binding_v1, decode_semantic_source_capture_v1};
#[path = "semantic_source_catalog_cursor.rs"]
mod cursor;
use cursor::{load_catalog_node, SourceCatalogCursorV1};

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceCatalogBoundsV1 {
  pub maximum_depth: usize,
  pub maximum_work: u64,
  pub maximum_read_bytes: u64,
  pub maximum_source_bytes: usize,
  pub maximum_chunk_entity_bytes: usize,
  pub maximum_source_chunks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticSourceCatalogSideV1 {
  Base,
  Requested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticSourceLookupDispositionV1 {
  Unlisted,
  Absent,
  Present,
}

/// Private fields keep disposition/source consistent without an additional
/// heap allocation for the already-accounted source observation.
pub struct NativeSemanticSourceLookupV1<'a> {
  disposition: SemanticSourceLookupDispositionV1,
  source: Option<NativeProtectedSemanticSourceV1<'a>>,
}

impl<'a> NativeSemanticSourceLookupV1<'a> {
  pub fn disposition(&self) -> SemanticSourceLookupDispositionV1 {
    self.disposition
  }
  pub fn source(&self) -> Option<&NativeProtectedSemanticSourceV1<'a>> {
    self.source.as_ref()
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticSourceCatalogSummaryV1 {
  pub paths: u64,
  pub base_nodes: u64,
  pub requested_nodes: u64,
  pub complete: bool,
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Prepare compiler borrows from one ASCM/ASMC-bound catalog side. Unlisted
  /// inputs refuse; explicit absence never falls back to current aliases.
  /// The lesser catalog/plugin read-byte ceiling covers controls, catalog paths
  /// and all unique alias/module reads cumulatively. Per-body/chunk ceilings
  /// remain the intersection of both owners' limits. This does not prove source
  /// union completeness, supplied configuration identity or task ownership.
  pub fn prepare_captured_semantic_alias_snapshot(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    side: SemanticSourceCatalogSideV1,
    request: NativeSemanticAliasSnapshotRequestV1<'_>,
    bounds: NativeSemanticSourceCatalogBoundsV1,
  ) -> Result<NativeSemanticAliasSnapshotV1<'_>, NativeSemanticPluginSourceErrorV1> {
    let bounds = NativeSemanticSourceCatalogBoundsV1 {
      maximum_read_bytes: bounds.maximum_read_bytes.min(request.plugins.maximum_read_bytes),
      ..bounds
    };
    let operation = CatalogReadOperationV1::new(self, bounds, None)?;
    let companion = operation.load_companion(task_id, checkpoint_sequence)?;
    let manifest =
      decode_semantic_source_capture_v1(&companion.bytes, operation.algorithm()).map_err(SemanticMutationObservationErrorV1::from)?;
    let root = match side {
      SemanticSourceCatalogSideV1::Base => manifest.base_source_catalog,
      SemanticSourceCatalogSideV1::Requested => manifest.requested_source_catalog,
    };
    let prepared = self.prepare_semantic_alias_snapshot_with_selected_reader(
      request,
      |alias| {
        self.read_plugin_sources_with_selected_reader(
          alias,
          request.plugins,
          |path, source_bounds| {
            protected_sources::validate_source_path(path, operation.algorithm())?;
            let selected = operation.read_selected(root, path, source_bounds)?;
            if selected.disposition == SemanticSourceLookupDispositionV1::Unlisted {
              return Err(invalid(
                "semantic_source_catalog_unlisted",
                "required compiler input is not listed in its retained source catalog",
              ));
            }
            Ok(selected.source)
          },
          || {},
        )
      },
      || {},
    )?;
    operation.check()?;
    Ok(prepared)
  }

  /// Stream provisional physical entries for one complete source-capture branch.
  /// This is not selected task, namespace, resume or complete GC authority.
  /// Entries may repeat and are visited before payload validation. Only complete
  /// success validates the whole branch; an error invalidates all prior visits.
  pub fn visit_captured_source_physical_entries(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticSourceCatalogBoundsV1,
    mut visitor: impl FnMut(&KVEntry) -> Result<(), SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticSourceCatalogSummaryV1, SemanticMutationObservationErrorV1> {
    let observer = CatalogPhysicalVisitorV1 {
      visitor: RefCell::new(&mut visitor),
      failure: RefCell::new(None),
      cancellation: &self.cancellation,
      memory: &self._memory,
    };
    let result =
      self.visit_captured_protected_source_pairs_observed(task_id, checkpoint_sequence, bounds, Some(&observer), |_, _, _| Ok(true));
    match observer.failure.into_inner() {
      Some(original) => Err(original),
      None => result,
    }
  }

  pub fn read_captured_protected_source(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    side: SemanticSourceCatalogSideV1,
    path: &str,
    bounds: NativeSemanticSourceCatalogBoundsV1,
  ) -> Result<NativeSemanticSourceLookupV1<'_>, SemanticMutationObservationErrorV1> {
    let operation = CatalogReadOperationV1::new(self, bounds, None)?;
    protected_sources::validate_source_path(path, operation.algorithm())?;
    let companion = operation.load_companion(task_id, checkpoint_sequence)?;
    let manifest = decode_semantic_source_capture_v1(&companion.bytes, operation.algorithm())?;
    let root = match side {
      SemanticSourceCatalogSideV1::Base => manifest.base_source_catalog,
      SemanticSourceCatalogSideV1::Requested => manifest.requested_source_catalog,
    };
    operation.read_selected(root, path, operation.source_bounds())
  }

  /// Callbacks are provisional until complete=true. Even a complete paired
  /// observation is not compiler-union, namespace, GC or current-owner proof.
  pub fn visit_captured_protected_source_pairs(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticSourceCatalogBoundsV1,
    visitor: impl FnMut(
      &str,
      Option<&NativeProtectedSemanticSourceV1<'_>>,
      Option<&NativeProtectedSemanticSourceV1<'_>>,
    ) -> Result<bool, SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticSourceCatalogSummaryV1, SemanticMutationObservationErrorV1> {
    self.visit_captured_protected_source_pairs_observed(task_id, checkpoint_sequence, bounds, None, visitor)
  }

  fn visit_captured_protected_source_pairs_observed(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticSourceCatalogBoundsV1,
    observer: Option<&dyn CatalogPhysicalEntryObserverV1>,
    mut visitor: impl FnMut(
      &str,
      Option<&NativeProtectedSemanticSourceV1<'_>>,
      Option<&NativeProtectedSemanticSourceV1<'_>>,
    ) -> Result<bool, SemanticMutationObservationErrorV1>,
  ) -> Result<SemanticSourceCatalogSummaryV1, SemanticMutationObservationErrorV1> {
    let operation = CatalogReadOperationV1::new(self, bounds, observer)?;
    let companion = operation.load_companion(task_id, checkpoint_sequence)?;
    let manifest = decode_semantic_source_capture_v1(&companion.bytes, operation.algorithm())?;
    let mut base = SourceCatalogCursorV1::new(manifest.base_source_catalog, bounds.maximum_depth)?;
    let mut requested = SourceCatalogCursorV1::new(manifest.requested_source_catalog, bounds.maximum_depth)?;
    let mut paths = 0u64;
    let mut index_present = false;
    let mut parser_present = false;
    loop {
      operation.check()?;
      let pair = (base.next_row(&operation)?, requested.next_row(&operation)?);
      let (left, right) = match pair {
        (None, None) => break,
        (Some(left), Some(right)) if left.name == right.name => (left, right),
        _ => return Err(invalid("semantic_source_catalog_paths", "base and requested source catalogs enumerate different paths")),
      };
      paths = paths.checked_add(1).ok_or_else(|| invalid("semantic_source_catalog_counts", "source path count overflowed"))?;
      if paths > manifest.protected_path_count
        || base.nodes > manifest.base_catalog_node_count
        || requested.nodes > manifest.requested_catalog_node_count
      {
        return Err(invalid("semantic_source_catalog_counts", "visited source catalogs exceed their declared counts"));
      }
      index_present |= left.name == "/.aeordb-config/indexes.json";
      parser_present |= left.name == "/.aeordb-config/parsers.json";
      let base_source = operation.read_row(&left)?;
      let requested_source = operation.read_row(&right)?;
      operation.check()?;
      let keep_going = visitor(&left.name, base_source.as_ref(), requested_source.as_ref())?;
      operation.check()?;
      if !keep_going {
        return Ok(SemanticSourceCatalogSummaryV1 { paths, base_nodes: base.nodes, requested_nodes: requested.nodes, complete: false });
      }
    }
    operation.check()?;
    if paths != manifest.protected_path_count
      || base.nodes != manifest.base_catalog_node_count
      || requested.nodes != manifest.requested_catalog_node_count
    {
      return Err(invalid("semantic_source_catalog_counts", "complete source catalogs disagree with their declared counts"));
    }
    if !index_present || !parser_present {
      return Err(invalid("semantic_source_catalog_required_paths", "source capture omits the root index or parser input"));
    }
    Ok(SemanticSourceCatalogSummaryV1 { paths, base_nodes: base.nodes, requested_nodes: requested.nodes, complete: true })
  }
}

trait CatalogPhysicalEntryObserverV1 {
  fn observe(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1>;
}

type CatalogPhysicalCallbackV1<'a> = dyn FnMut(&KVEntry) -> Result<(), SemanticMutationObservationErrorV1> + 'a;

struct CatalogPhysicalVisitorV1<'a> {
  visitor: RefCell<&'a mut CatalogPhysicalCallbackV1<'a>>,
  failure: RefCell<Option<SemanticMutationObservationErrorV1>>,
  cancellation: &'a CancellationToken,
  memory: &'a MemoryReservation,
}

impl CatalogPhysicalEntryObserverV1 for CatalogPhysicalVisitorV1<'_> {
  fn observe(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let result: Result<(), SemanticMutationObservationErrorV1> = (|| {
      check_cancelled(self.cancellation)?;
      self.memory.check_admission()?;
      {
        let mut visitor = self.visitor.borrow_mut();
        visitor(locator)?;
      }
      check_cancelled(self.cancellation)?;
      self.memory.check_admission()?;
      Ok(())
    })();
    if let Err(original) = result {
      *self.failure.borrow_mut() = Some(original);
      // The existing physical read trait carries its own error type. Preserve
      // the actual callback/admission cause for the public boundary above.
      return Err(FirstAuthorityPublicationErrorV1::invalid("semantic_source_physical_visitor", "physical entry visitor refused"));
    }
    Ok(())
  }
}

struct CatalogLookupV1<'a, 'observer> {
  captured: CapturedEntityLookupV1<'a>,
  remaining_work: Cell<u64>,
  observer: Option<&'observer dyn CatalogPhysicalEntryObserverV1>,
}

impl CatalogLookupV1<'_, '_> {
  fn charge_work(&self) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let remaining = self.remaining_work.get().checked_sub(1).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid("semantic_source_catalog_work_bound", "source catalog exhausted its cumulative work bound")
    })?;
    self.remaining_work.set(remaining);
    Ok(())
  }
}

impl FirstAuthorityEntityLookupV1 for CatalogLookupV1<'_, '_> {
  fn get(&self, key: &[u8]) -> Result<Option<KVEntry>, EngineError> {
    self.captured.get(key)
  }
  fn hash_algo(&self) -> HashAlgorithm {
    self.captured.hash_algo()
  }
  fn admit_read(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    self.charge_work()?;
    self.captured.admit_read(locator)?;
    if let Some(observer) = self.observer {
      observer.observe(locator)?;
    }
    Ok(())
  }
}

struct CatalogReadOperationV1<'a, 'observer> {
  capture: &'a NativeSemanticMutationInventoryV1<'a>,
  bounds: NativeSemanticSourceCatalogBoundsV1,
  lookup: CatalogLookupV1<'a, 'observer>,
  _memory: MemoryReservation,
}

struct CatalogCompanionV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

impl<'a, 'observer> CatalogReadOperationV1<'a, 'observer> {
  fn new(
    capture: &'a NativeSemanticMutationInventoryV1<'a>,
    bounds: NativeSemanticSourceCatalogBoundsV1,
    observer: Option<&'observer dyn CatalogPhysicalEntryObserverV1>,
  ) -> Result<Self, SemanticMutationObservationErrorV1> {
    check_cancelled(&capture.cancellation)?;
    capture._memory.check_admission()?;
    if bounds.maximum_depth == 0
      || bounds.maximum_depth > 256
      || bounds.maximum_work == 0
      || bounds.maximum_read_bytes == 0
      || bounds.maximum_source_bytes > 64 << 20
      || bounds.maximum_chunk_entity_bytes == 0
      || bounds.maximum_chunk_entity_bytes > MAXIMUM_ENTITY_BYTES
      || bounds.maximum_source_chunks == 0
    {
      return Err(invalid("semantic_source_catalog_bounds", "source catalogs require valid operational depth, work and byte limits"));
    }
    let header = &capture.header.selected.header;
    let workspace = namespace_seek_workspace_bytes_v1(65_535, 1, bounds.maximum_depth as u64, header.hash_algorithm.hash_length() as u64)
      .and_then(|bytes| bytes.checked_mul(2))
      .ok_or_else(|| invalid("semantic_source_catalog_bounds", "source catalog workspace overflowed"))?;
    let memory = capture.memory.reserve(MemoryOwner::Task, workspace, AdmissionClass::Maintenance)?;
    let lookup = CatalogLookupV1 {
      captured: CapturedEntityLookupV1 {
        snapshot: &capture.snapshot,
        header,
        cancellation: &capture.cancellation,
        bounds: NativeSemanticMutationInventoryBoundsV1 {
          maximum_work: bounds.maximum_work,
          maximum_entity_bytes: MAXIMUM_ENTITY_BYTES,
          maximum_read_bytes: bounds.maximum_read_bytes,
        },
        remaining_read_bytes: Cell::new(bounds.maximum_read_bytes),
      },
      remaining_work: Cell::new(bounds.maximum_work),
      observer,
    };
    Ok(Self { capture, bounds, lookup, _memory: memory })
  }

  fn algorithm(&self) -> HashAlgorithm {
    self.capture.header.selected.header.hash_algorithm
  }

  fn check(&self) -> Result<(), SemanticMutationObservationErrorV1> {
    check_cancelled(&self.capture.cancellation)?;
    self._memory.check_admission()?;
    Ok(())
  }

  fn load_companion(&self, task_id: &[u8; 16], sequence: u64) -> Result<CatalogCompanionV1, SemanticMutationObservationErrorV1> {
    self.check()?;
    if task_id.iter().all(|byte| *byte == 0) || sequence == 0 {
      return Err(invalid("semantic_source_catalog_identity", "source capture requires a nonzero task and checkpoint sequence"));
    }
    let header = &self.capture.header.selected.header;
    for bit in [
      crate::engine::v4::contract_generated::capability_bit::SEMANTIC_MUTATION_TASK_V1,
      crate::engine::v4::contract_generated::capability_bit::SEMANTIC_SOURCE_CAPTURE_V1,
    ] {
      let index = usize::from(bit / 8);
      let mask = 1u8 << (bit % 8);
      if header.required_reader_capabilities[index] & mask == 0 || header.required_writer_capabilities[index] & mask == 0 {
        return Err(invalid(
          "semantic_source_catalog_capability",
          "source capture controls require reader and writer capability declarations",
        ));
      }
    }
    let mut identity = [0; 24];
    identity[..16].copy_from_slice(task_id);
    identity[16..].copy_from_slice(&sequence.to_le_bytes());
    let memory = self.capture.memory.reserve(
      MemoryOwner::Task,
      4 * (SystemControlKindV1::SemanticSourceCapture.encoded_cap() + SystemControlKindV1::SemanticMutationCheckpoint.encoded_cap()) as u64
        + 8 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP as u64,
      AdmissionClass::Maintenance,
    )?;
    let file = &self.capture._protection.publisher().file;
    let companion = load_immutable_system_control_file(file, &self.lookup, header, SystemControlKindV1::SemanticSourceCapture, &identity)
      .map_err(catalog_read_error)?
      .ok_or_else(|| invalid("semantic_source_catalog_capture_missing", "requested source capture companion is missing"))?;
    self.check()?;
    let checkpoint =
      load_immutable_system_control_file(file, &self.lookup, header, SystemControlKindV1::SemanticMutationCheckpoint, &identity)
        .map_err(catalog_read_error)?
        .ok_or_else(|| invalid("semantic_source_catalog_checkpoint_missing", "source capture checkpoint is missing"))?;
    decode_semantic_source_capture_binding_v1(&companion.bytes, &checkpoint.bytes, self.algorithm())?;
    self.check()?;
    memory.check_admission()?;
    Ok(CatalogCompanionV1 { bytes: companion.bytes, _memory: memory })
  }

  fn read_selected(
    &self,
    root: &[u8],
    path: &str,
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<NativeSemanticSourceLookupV1<'a>, SemanticMutationObservationErrorV1> {
    let selected = seek_namespace_child_v1(root, path, true, self.bounds.maximum_depth, |hash, lower, upper, _| {
      load_catalog_node(self, hash, lower, upper)
    })?;
    self.check()?;
    let Some(row) = selected.filter(|row| row.name == path) else {
      return Ok(NativeSemanticSourceLookupV1 { disposition: SemanticSourceLookupDispositionV1::Unlisted, source: None });
    };
    self.lookup.charge_work().map_err(catalog_read_error)?;
    let source = self.read_row_bounded(&row, bounds)?;
    self.check()?;
    let disposition = if source.is_some() { SemanticSourceLookupDispositionV1::Present } else { SemanticSourceLookupDispositionV1::Absent };
    Ok(NativeSemanticSourceLookupV1 { disposition, source })
  }

  fn source_bounds(&self) -> NativeSemanticSourceReadBoundsV1 {
    NativeSemanticSourceReadBoundsV1 {
      maximum_body_bytes: self.bounds.maximum_source_bytes,
      maximum_chunk_entity_bytes: self.bounds.maximum_chunk_entity_bytes,
      maximum_chunks: self.bounds.maximum_source_chunks,
      maximum_read_bytes: self.bounds.maximum_read_bytes,
    }
  }

  fn read_row(&self, row: &ChildEntry) -> Result<Option<NativeProtectedSemanticSourceV1<'a>>, SemanticMutationObservationErrorV1> {
    self.read_row_bounded(row, self.source_bounds())
  }

  fn read_row_bounded(
    &self,
    row: &ChildEntry,
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<Option<NativeProtectedSemanticSourceV1<'a>>, SemanticMutationObservationErrorV1> {
    self.check()?;
    if row.hash.iter().all(|byte| *byte == 0) {
      return Ok(None);
    }
    let bounds = NativeSemanticSourceReadBoundsV1 {
      maximum_body_bytes: self.bounds.maximum_source_bytes.min(bounds.maximum_body_bytes),
      maximum_chunk_entity_bytes: self.bounds.maximum_chunk_entity_bytes.min(bounds.maximum_chunk_entity_bytes),
      maximum_chunks: self.bounds.maximum_source_chunks.min(bounds.maximum_chunks),
      maximum_read_bytes: self.bounds.maximum_read_bytes.min(bounds.maximum_read_bytes),
    };
    self.capture.read_source_from_lookup(&row.name, Some(&row.hash), bounds, &self.lookup, || {}).map_err(|error| match error {
      SemanticMutationObservationErrorV1::Authority(source) => catalog_read_error(source),
      other => other,
    })
  }
}

fn catalog_read_error(source: FirstAuthorityPublicationErrorV1) -> SemanticMutationObservationErrorV1 {
  let code = match source.code() {
    "semantic_source_catalog_work_bound" => "semantic_source_catalog_work_bound",
    "semantic_task_inventory_read_bound" => "semantic_source_catalog_read_bound",
    "semantic_task_inventory_entity_bound" | "first_authority_locator_exceeds_cap" => "semantic_source_catalog_entity_bound",
    "first_authority_readback_allocation" | "first_authority_system_file_allocation" => "semantic_source_catalog_allocation",
    _ => return source.into(),
  };
  SemanticMutationObservationErrorV1::ResourceRead { code, source }
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, SemanticMutationObservationErrorV1> {
  let mut result = Vec::new();
  result
    .try_reserve_exact(bytes.len())
    .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", source })?;
  result.extend_from_slice(bytes);
  Ok(result)
}

fn copy_path(path: &str) -> Result<String, SemanticMutationObservationErrorV1> {
  let mut result = String::new();
  result
    .try_reserve_exact(path.len())
    .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", source })?;
  result.push_str(path);
  Ok(result)
}

impl From<NamespaceSeekFailureV1> for SemanticMutationObservationErrorV1 {
  fn from(source: NamespaceSeekFailureV1) -> Self {
    match source {
      NamespaceSeekFailureV1::Allocation(source) => Self::Allocation { code: "semantic_source_catalog_allocation", source },
      NamespaceSeekFailureV1::Depth => {
        Self::Resource { code: "semantic_source_catalog_depth", message: "source catalog exceeds its operational depth" }
      }
      NamespaceSeekFailureV1::Cycle => invalid("semantic_source_catalog_cycle", "source catalog contains an ancestor cycle"),
      NamespaceSeekFailureV1::InvalidChild(source) => FirstAuthorityPublicationErrorV1::from(source).into(),
      NamespaceSeekFailureV1::ParentShape | NamespaceSeekFailureV1::ChildIndex | NamespaceSeekFailureV1::PathOverflow => {
        invalid("semantic_source_catalog_shape", "invalid source catalog traversal shape")
      }
    }
  }
}
