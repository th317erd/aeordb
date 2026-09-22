//! Retained source-position admission, not durable task or activation authority.
#[path = "semantic_compiler_batch_native.rs"]
mod compiler_batch;
#[path = "semantic_compiler_output_native.rs"]
mod compiler_output;
use super::*;
use super::super::super::super::super::task_graph::load_captured_semantic_object;
use crate::engine::v4::index_configuration_compiler::{
  CompiledIndexConfigurationV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use crate::engine::v4::namespace::{EncodedSemanticDefinitionObjectV1, SemanticAvailabilityV1, decode_semantic_definition_record};
use crate::engine::v4::parser_registry_compiler::{CompiledParserRegistryV1, ParserRegistryCompilationRequestV1, compile_parser_registry_v1};
use crate::engine::v4::root_authority::NamespaceSemanticBindingV1;
use crate::engine::v4::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1,
};
use crate::engine::v4::semantic_catalog_compiler::{
  AdmittedSemanticCatalogProgressV1, CompiledSemanticCatalogV1, SemanticCatalogCompilationRequestV1, admit_semantic_catalog_progress_v1,
  admit_semantic_catalog_v1, configuration_owner,
};
use crate::engine::v4::semantic_catalog_mutation::SemanticCatalogSnapshotV1;
use crate::engine::v4::semantic_mutation_control::{SemanticMutationCheckpointV1, SemanticMutationCursorV1, SemanticMutationPhaseV1};
use std::cell::RefCell;

// A bounded canonical object plus overlapping physical/decode projections,
// record framing, owner keys and comparison scratch. Compiler traversal and
// reachability retain their existing separate reservation.
const SEMANTIC_DECODE_WORKSPACE_BYTES: usize = 16 << 20;
type CatalogParts =
  (SemanticCatalogCompilationRequestV1, CompiledParserRegistryV1, AdmittedSemanticCatalogProgressV1, SemanticCompilerConstructionModeV1);

// Retained source/compiler inputs only. No task, checkpoint or HEAD authority.
pub(in crate::engine::v4::first_authority) struct PreparedSemanticCompilerInputsV1 {
  pub(in crate::engine::v4::first_authority) request: SemanticCatalogCompilationRequestV1,
  pub(in crate::engine::v4::first_authority) registry: CompiledParserRegistryV1,
  pub(in crate::engine::v4::first_authority) base_admission: Option<CompiledSemanticCatalogV1>,
  base_registry: Option<CompiledParserRegistryV1>,
  pub(in crate::engine::v4::first_authority) mode: SemanticCompilerConstructionModeV1,
}

struct PrefixConfigurationSourceV1<'a> {
  catalog_root: &'a [u8],
  body: Option<&'a [u8]>,
  owner: &'a str,
  maximum_source_bytes: usize,
}

struct PrefixVerificationV1<'a> {
  checkpoint: &'a SemanticMutationCheckpointV1<'a>,
  base_tree: &'a [u8],
  catalog: SemanticCatalogSnapshotV1<'a>,
  base_catalog: Option<SemanticCatalogSnapshotV1<'a>>,
  registry: &'a CompiledParserRegistryV1,
  base_registry: Option<&'a CompiledParserRegistryV1>,
  mode: SemanticCompilerConstructionModeV1,
  source: &'a dyn SemanticCatalogObjectSourceV1,
  bounds: NativeSemanticCompilerProgressBoundsV1,
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticCompilerProgressBoundsV1 {
  pub sources: NativeSemanticSourceUnionValidationBoundsV1,
  pub maximum_compiler_workspace_bytes: usize,
  pub maximum_alias_snapshot_bytes: usize,
  pub maximum_semantic_decode_workspace_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticCompilerConstructionModeV1 {
  Fresh,
  Incremental,
}

/// One checked source prefix tied to its captured inventory. This does not
/// select a task, retain it across restart, fence a writer or activate a root.
pub struct NativeSemanticCompilerProgressV1<'a> {
  _capture: &'a NativeSemanticMutationInventoryV1<'a>,
  registry: CompiledParserRegistryV1,
  progress: AdmittedSemanticCatalogProgressV1,
  request: SemanticCatalogCompilationRequestV1,
  mode: SemanticCompilerConstructionModeV1,
  sources: SemanticSourceUnionValidationSummaryV1,
}

impl NativeSemanticCompilerProgressV1<'_> {
  pub fn phase(&self) -> SemanticMutationPhaseV1 {
    self.progress.phase()
  }
  pub fn configuration_count(&self) -> u64 {
    self.progress.configuration_count()
  }
  pub const fn construction_mode(&self) -> SemanticCompilerConstructionModeV1 {
    self.mode
  }
  pub const fn sources(&self) -> &SemanticSourceUnionValidationSummaryV1 {
    &self.sources
  }
  /// Consuming this wrapper removes its source-proof boundary. The returned
  /// existing catalog primitives carry no task/retention/activation authority.
  pub fn into_catalog_parts(self) -> (SemanticCatalogCompilationRequestV1, CompiledParserRegistryV1, AdmittedSemanticCatalogProgressV1) {
    (self.request, self.registry, self.progress)
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub fn admit_captured_semantic_compiler_progress(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<NativeSemanticCompilerProgressV1<'_>> {
    self.admit_captured_semantic_compiler_progress_observed(task_id, checkpoint_sequence, bounds, || {})
  }

  pub(in crate::engine::v4::first_authority) fn admit_captured_semantic_compiler_progress_observed(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticCompilerProgressBoundsV1,
    before_complete: impl FnOnce(),
  ) -> UnionResult<NativeSemanticCompilerProgressV1<'_>> {
    let (sources, (request, registry, progress, mode)) = self.with_validated_captured_semantic_source_union(
      task_id,
      checkpoint_sequence,
      bounds.sources,
      |operation, base, checkpoint, _, base_count| operation.admit_prefix(base, checkpoint, base_count, bounds),
      before_complete,
    )?;
    Ok(NativeSemanticCompilerProgressV1 { _capture: self, registry, progress, request, mode, sources })
  }

  // Source preparation only: Captured is deliberately still invalid as progress.
  // The caller's bounded publication reservation owns the returned companion copy.
  pub(in crate::engine::v4::first_authority) fn prepare_captured_semantic_compiler_inputs(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    expected_checkpoint: &[u8],
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<(PreparedSemanticCompilerInputsV1, Vec<u8>)> {
    let (_, prepared) = self.with_validated_captured_semantic_source_union(
      task_id,
      checkpoint_sequence,
      bounds.sources,
      |operation, base, checkpoint_bytes, companion, base_count| {
        let algorithm = operation.catalog.algorithm();
        let checkpoint =
          decode_semantic_mutation_checkpoint(checkpoint_bytes, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
        if checkpoint_bytes != expected_checkpoint || checkpoint.phase != SemanticMutationPhaseV1::Captured {
          return Err(invalid("semantic_task_work_checkpoint", "compiler start requires the exact selected Captured checkpoint").into());
        }
        if checkpoint.compiler_fingerprint != crate::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1(algorithm)
          || checkpoint.semantic_registry_fingerprint
            != crate::engine::v4::system_family::embedded_system_family_registry(algorithm)
              .map_err(SemanticMutationObservationErrorV1::from)?
              .semantic_projection_fingerprint
        {
          return Err(invalid("semantic_catalog_base_profile", "checkpoint compiler or semantic registry profile is not supported").into());
        }
        if bounds.maximum_semantic_decode_workspace_bytes < SEMANTIC_DECODE_WORKSPACE_BYTES {
          return Err(resource("semantic_compiler_prefix_workspace", "semantic decode scratch exceeds its operational ceiling").into());
        }
        let decode = self
          .memory
          .reserve(MemoryOwner::Task, SEMANTIC_DECODE_WORKSPACE_BYTES as u64, AdmissionClass::Maintenance)
          .map_err(SemanticMutationObservationErrorV1::from)?;
        let source = PrefixObjectSource { catalog: operation.catalog, capture: self, decode: &decode, failure: RefCell::new(None) };
        let result = operation.prepare_compiler_inputs(base, checkpoint.expected_configuration_count, base_count, bounds, &source);
        let inputs = source.finish(result)?;
        operation.catalog.check()?;
        decode.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
        Ok((inputs, copy_namespace_bytes(companion)?))
      },
      || {},
    )?;
    Ok(prepared)
  }
}

struct PrefixObjectSource<'operation, 'capture, 'observer> {
  catalog: &'operation CatalogReadOperationV1<'capture, 'observer>,
  capture: &'capture NativeSemanticMutationInventoryV1<'capture>,
  decode: &'operation MemoryReservation,
  failure: RefCell<Option<SemanticMutationObservationErrorV1>>,
}

impl PrefixObjectSource<'_, '_, '_> {
  fn finish<T>(&self, result: UnionResult<T>) -> UnionResult<T> {
    match self.failure.borrow_mut().take() {
      Some(original) => Err(original.into()),
      None => result,
    }
  }
}

impl SemanticCatalogObjectSourceV1 for PrefixObjectSource<'_, '_, '_> {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    let result = load_captured_semantic_object(
      &self.capture._protection.publisher().file,
      self.catalog.lookup(),
      &self.capture.header.selected.header,
      kind_id,
      object_id,
      || -> Result<(), SemanticMutationObservationErrorV1> {
        self.catalog.check()?;
        self.decode.check_admission()?;
        Ok(())
      },
    );
    match result {
      Ok(value) => Ok(value),
      Err(error) => {
        let original = match error {
          SemanticMutationObservationErrorV1::Authority(source) => catalog_read_error(source),
          error => error,
        };
        if self.failure.borrow().is_none() {
          *self.failure.borrow_mut() = Some(original);
        }
        Err(SemanticCatalogReadErrorV1::unavailable(
          "semantic_compiler_prefix_read",
          "captured semantic read failed; original cause retained",
        ))
      }
    }
  }
}

impl<A: NamespaceReadAdmissionV1> RetainedSourceUnionOperationV1<'_, '_, '_, A> {
  fn admit_prefix(
    &self,
    base: &NamespaceSemanticBindingV1,
    checkpoint_bytes: &[u8],
    base_count: u64,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<CatalogParts> {
    self.catalog.check()?;
    if bounds.maximum_semantic_decode_workspace_bytes < SEMANTIC_DECODE_WORKSPACE_BYTES {
      return Err(resource("semantic_compiler_prefix_workspace", "semantic decode scratch exceeds its operational ceiling").into());
    }
    let capture = self.namespace.capture;
    let decode = capture
      .memory
      .reserve(MemoryOwner::Task, SEMANTIC_DECODE_WORKSPACE_BYTES as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let source = PrefixObjectSource { catalog: self.catalog, capture, decode: &decode, failure: RefCell::new(None) };
    let result = (|| -> UnionResult<CatalogParts> {
      let checkpoint = decode_semantic_mutation_checkpoint(checkpoint_bytes, self.catalog.algorithm())
        .map_err(SemanticMutationObservationErrorV1::from)?;
      if !matches!(checkpoint.phase, SemanticMutationPhaseV1::Compiling | SemanticMutationPhaseV1::Pruning) {
        return Err(invalid("semantic_catalog_progress_phase", "catalog progress admission requires compiling or pruning work").into());
      }
      let PreparedSemanticCompilerInputsV1 { request, registry, base_admission, base_registry, mode } =
        self.prepare_compiler_inputs(base, checkpoint.expected_configuration_count, base_count, bounds, &source)?;
      let cancelled = || capture.cancellation.is_cancelled();
      let base_snapshot = match &base.semantic_state.availability {
        SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, .. } if *catalog_record_count != 0 => {
          Some(SemanticCatalogSnapshotV1 {
            root_object_id: Some(catalog_root),
            record_count: *catalog_record_count,
            node_count: *catalog_node_count,
          })
        }
        _ => None,
      };
      let progress = admit_semantic_catalog_progress_v1(request, checkpoint_bytes, &registry, &source, &capture.memory, &cancelled)?;
      self.verify_prefix(PrefixVerificationV1 {
        checkpoint: &checkpoint,
        base_tree: &base.root.namespace_tree_root,
        catalog: progress.catalog(),
        base_catalog: base_snapshot,
        registry: &registry,
        base_registry: base_registry.as_ref(),
        mode,
        source: &source,
        bounds,
      })?;
      self.catalog.check()?;
      self.namespace.check()?;
      decode.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
      drop(base_admission);
      Ok((request, registry, progress, mode))
    })();
    source.finish(result)
  }

  fn prepare_compiler_inputs(
    &self,
    base: &NamespaceSemanticBindingV1,
    expected_configuration_count: u64,
    base_count: u64,
    bounds: NativeSemanticCompilerProgressBoundsV1,
    source: &dyn SemanticCatalogObjectSourceV1,
  ) -> UnionResult<PreparedSemanticCompilerInputsV1> {
    let registry = self.compile_registry(self.manifest.requested_source_catalog, bounds)?;
    let request = SemanticCatalogCompilationRequestV1 {
      hash_algorithm: self.catalog.algorithm(),
      expected_configuration_count,
      required_capabilities: base.semantic_state.required_capabilities,
      maximum_workspace_bytes: bounds.maximum_compiler_workspace_bytes,
    };
    let capture = self.namespace.capture;
    let cancelled = || capture.cancellation.is_cancelled();
    let mut base_registry = None;
    let mut base_admission = None;
    let mut mode = SemanticCompilerConstructionModeV1::Fresh;
    if let SemanticAvailabilityV1::Complete { catalog_record_count, .. } = &base.semantic_state.availability {
      if *catalog_record_count == 0 {
        if base_count != 0 {
          return Err(invalid("semantic_compiler_prefix_empty_base", "Complete-empty base has retained configurations").into());
        }
      } else {
        let compiled = self.compile_registry(self.manifest.base_source_catalog, bounds)?;
        // Even a changed registry must first prove the nonempty Complete base.
        base_admission = Some(admit_semantic_catalog_v1(
          SemanticCatalogCompilationRequestV1 { expected_configuration_count: base_count, ..request },
          &base.semantic_state.object_id,
          &compiled,
          source,
          &capture.memory,
          &cancelled,
        )?);
        if compiled.projection() == registry.projection() {
          mode = SemanticCompilerConstructionModeV1::Incremental;
        }
        base_registry = Some(compiled);
      }
    }
    Ok(PreparedSemanticCompilerInputsV1 { request, registry, base_admission, base_registry, mode })
  }

  fn compile_registry(&self, root: &[u8], bounds: NativeSemanticCompilerProgressBoundsV1) -> UnionResult<CompiledParserRegistryV1> {
    let selected = self.selected(root, GLOBAL_PARSERS)?;
    let body = selected.as_ref().map(NativeProtectedSemanticSourceV1::body);
    let aliases =
      self.prefix_aliases(root, SemanticSourceAliasKindV1::ParserRegistry, body, self.bounds.catalog.maximum_source_bytes, bounds)?;
    Ok(compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: body,
        hash_algorithm: self.catalog.algorithm(),
        maximum_source_bytes: self.bounds.catalog.maximum_source_bytes,
        maximum_workspace_bytes: bounds.maximum_compiler_workspace_bytes,
      },
      &aliases,
      &self.namespace.capture.memory,
      &|| self.namespace.capture.cancellation.is_cancelled(),
    )?)
  }

  fn selected(&self, root: &[u8], path: &str) -> UnionResult<Option<NativeProtectedSemanticSourceV1<'_>>> {
    let selected = self.catalog.read_selected(root, path, self.catalog.source_bounds())?;
    if selected.disposition() == SemanticSourceLookupDispositionV1::Unlisted {
      return Err(
        invalid("semantic_source_catalog_unlisted", "required compiler input is not listed in its retained source catalog").into(),
      );
    }
    Ok(selected.into_source())
  }

  fn prefix_aliases(
    &self,
    root: &[u8],
    kind: SemanticSourceAliasKindV1,
    body: Option<&[u8]>,
    maximum_source_bytes: usize,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<NativeSemanticAliasSnapshotV1<'_>> {
    let capture = self.namespace.capture;
    self.catalog.check()?;
    let snapshot = capture.prepare_semantic_alias_snapshot_with_selected_reader(
      NativeSemanticAliasSnapshotRequestV1 {
        source: SemanticSourceAliasRequestV1 {
          kind,
          source: body,
          maximum_source_bytes,
          maximum_workspace_bytes: self.bounds.maximum_alias_workspace_bytes,
          maximum_alias_occurrences: self.bounds.maximum_alias_occurrences,
        },
        plugins: self.plugin_bounds(),
        maximum_snapshot_bytes: bounds.maximum_alias_snapshot_bytes,
      },
      |alias| {
        capture.read_plugin_sources_with_selected_reader(
          alias,
          self.plugin_bounds(),
          |path, source_bounds| {
            let selected = self.catalog.read_selected(root, path, source_bounds)?;
            if selected.disposition() == SemanticSourceLookupDispositionV1::Unlisted {
              return Err(invalid(
                "semantic_source_catalog_unlisted",
                "required compiler input is not listed in its retained source catalog",
              ));
            }
            Ok(selected.into_source())
          },
          || {},
        )
      },
      || {},
    )?;
    self.catalog.check()?;
    Ok(snapshot)
  }

  fn compile_configuration(
    &self,
    input: PrefixConfigurationSourceV1<'_>,
    registry: &CompiledParserRegistryV1,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<Option<CompiledIndexConfigurationV1>> {
    let PrefixConfigurationSourceV1 { catalog_root: root, body: source, owner, maximum_source_bytes } = input;
    let Some(source) = source else { return Ok(None) };
    let aliases = self.prefix_aliases(root, SemanticSourceAliasKindV1::IndexConfiguration, Some(source), maximum_source_bytes, bounds)?;
    Ok(Some(compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source,
        owner_path: owner,
        registry,
        hash_algorithm: self.catalog.algorithm(),
        maximum_source_bytes,
        maximum_workspace_bytes: bounds.maximum_compiler_workspace_bytes,
      },
      &aliases,
      &self.namespace.capture.memory,
      &|| self.namespace.capture.cancellation.is_cancelled(),
    )?))
  }

  fn verify_prefix(&self, input: PrefixVerificationV1<'_>) -> UnionResult<()> {
    let PrefixVerificationV1 { checkpoint, base_tree, catalog, base_catalog, registry, base_registry, mode, source, bounds } = input;
    let reader = SemanticCatalogReaderV1::new(self.catalog.algorithm(), source);
    let mut expected_count = 0u64;
    let mut check_owner =
      |owner: &str, base_body: Option<&[u8]>, requested_body: Option<&[u8]>, processed: bool, source_bytes: usize| -> UnionResult<()> {
        self.catalog.check()?;
        self.namespace.lookup.charge_work(1).map_err(map_namespace_read_error)?;
        let previous = match base_registry {
          Some(base_registry) => self.compile_configuration(
            PrefixConfigurationSourceV1 {
              catalog_root: self.manifest.base_source_catalog,
              body: base_body,
              owner,
              maximum_source_bytes: source_bytes,
            },
            base_registry,
            bounds,
          )?,
          None => None,
        };
        if let Some(base_catalog) = base_catalog {
          self.compare_projection(&reader, base_catalog, owner, previous.as_ref().map(CompiledIndexConfigurationV1::projection))?;
        }
        let requested = if processed {
          self.compile_configuration(
            PrefixConfigurationSourceV1 {
              catalog_root: self.manifest.requested_source_catalog,
              body: requested_body,
              owner,
              maximum_source_bytes: source_bytes,
            },
            registry,
            bounds,
          )?
        } else {
          None
        };
        let expected = if processed {
          requested.as_ref()
        } else if mode == SemanticCompilerConstructionModeV1::Incremental {
          previous.as_ref()
        } else {
          None
        };
        self.compare_projection(&reader, catalog, owner, expected.map(CompiledIndexConfigurationV1::projection))?;
        if expected.is_some() {
          expected_count = increment_count(expected_count)?;
        }
        Ok(())
      };
    let pruning = matches!(checkpoint.phase, SemanticMutationPhaseV1::Pruning | SemanticMutationPhaseV1::Ready);
    let cursor_owner = match checkpoint.cursor {
      SemanticMutationCursorV1::ConfigurationOwner(owner) => Some(owner),
      _ => None,
    };
    let cursor_path = cursor_owner.filter(|owner| *owner != "/").map(configuration_owner).transpose()?;
    let mut cursor_found = cursor_path.is_none();
    {
      let base = self.selected(self.manifest.base_source_catalog, GLOBAL_INDEXES)?;
      let requested = self.selected(self.manifest.requested_source_catalog, GLOBAL_INDEXES)?;
      check_owner(
        "/",
        base.as_ref().map(NativeProtectedSemanticSourceV1::body),
        requested.as_ref().map(NativeProtectedSemanticSourceV1::body),
        pruning || cursor_owner.is_some(),
        self.bounds.catalog.maximum_source_bytes,
      )?;
    }
    let mut namespaces = NamespaceSourcePairCursorV1::new(self.namespace, base_tree, self.manifest.staged_directory_root)?;
    while let Some(pair) = namespaces.next_pair(self.namespace)? {
      let owner = pair
        .path
        .strip_suffix("/.aeordb-config/indexes.json")
        .filter(|owner| !owner.is_empty())
        .ok_or_else(|| invalid("semantic_compiler_prefix_owner", "namespace configuration has no nonroot owner"))?;
      let processed = match cursor_path.as_ref() {
        Some(path) => {
          cursor_found |= pair.path.as_bytes() == &path[2..];
          pair.path.as_bytes() <= &path[2..]
        }
        None => false,
      };
      check_owner(
        owner,
        pair.base.as_ref().map(NativeSemanticNamespaceSourceV1::body),
        pair.requested.as_ref().map(NativeSemanticNamespaceSourceV1::body),
        pruning || processed,
        self.bounds.namespace.sources.maximum_body_bytes,
      )?;
    }
    if !cursor_found {
      return Err(invalid("semantic_compiler_prefix_cursor", "saved configuration owner is absent from the retained source union").into());
    }
    if expected_count != checkpoint.configuration_count {
      return Err(invalid("semantic_compiler_prefix_projection", "partial catalog contains unexpected configuration bindings").into());
    }
    Ok(())
  }

  fn compare_projection(
    &self,
    reader: &SemanticCatalogReaderV1<'_>,
    catalog: SemanticCatalogSnapshotV1<'_>,
    owner: &str,
    expected: Option<&EncodedSemanticDefinitionObjectV1>,
  ) -> UnionResult<()> {
    let root = catalog.root_object_id.ok_or_else(|| invalid("semantic_compiler_prefix_projection", "admitted catalog root is absent"))?;
    let owner = configuration_owner(owner)?;
    let expected_body = expected
      .map(|definition| decode_semantic_definition_record(&definition.object.value, self.catalog.algorithm()))
      .transpose()
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let cancelled = || self.namespace.capture.cancellation.is_cancelled();
    let found = reader.with_record(
      root,
      SemanticCatalogTraversalBoundsV1::new(catalog.record_count, catalog.node_count)?,
      1,
      &owner,
      &cancelled,
      |record| {
        let Some(expected) = expected else { return Ok(false) };
        if record.semantic_id != expected.semantic_id || record.definition_object_id != expected.object.object_id {
          return Ok(false);
        }
        reader.with_definition(record, &cancelled, |body| Ok(expected_body.as_ref().is_some_and(|expected| body == expected.definition)))
      },
    )?;
    if !matches!((expected, found), (None, None) | (Some(_), Some(true))) {
      return Err(invalid("semantic_compiler_prefix_projection", "catalog configuration differs from its retained source position").into());
    }
    self.catalog.check()?;
    Ok(())
  }
}
