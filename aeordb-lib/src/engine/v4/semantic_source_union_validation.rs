//! Declared retained-source validation, not compiler or durable task authority.
use super::*;
use super::super::super::super::source_catalog::{catalog_read_error, CatalogReadOperationV1, SourceCatalogCursorV1};
use crate::engine::v4::root_authority::{decode_namespace_semantic_binding, NamespaceSemanticBindingInputV1};
use crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_checkpoint;
use crate::engine::v4::semantic_source_capture::{decode_semantic_source_capture_v1, SemanticSourceCaptureV1};

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceUnionValidationBoundsV1 {
  /// Its physical read-byte ceiling also includes all namespace reads.
  pub catalog: NativeSemanticSourceCatalogBoundsV1,
  /// Separately bounded namespace work; both trees and passes share this limit.
  pub namespace: NativeSemanticNamespaceSourceBoundsV1,
  pub maximum_plugin_module_bytes: usize,
  pub maximum_plugin_workspace_bytes: usize,
  pub maximum_alias_occurrences: u64,
  pub maximum_alias_workspace_bytes: usize,
  pub maximum_fingerprint_workspace_bytes: usize,
}

/// A completed read-only observation, not a source-position, task or GC permit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticSourceUnionValidationSummaryV1 {
  pub protected_paths: u64,
  pub namespace_paths: u64,
  pub base_configuration_count: u64,
  pub requested_configuration_count: u64,
  pub read_bytes: u64,
  pub catalog_work: u64,
  pub namespace_work: u64,
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub fn validate_captured_semantic_source_union(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticSourceUnionValidationBoundsV1,
  ) -> Result<SemanticSourceUnionValidationSummaryV1, NativeSemanticSourceUnionErrorV1> {
    self.validate_captured_semantic_source_union_observed(task_id, checkpoint_sequence, bounds, || {})
  }

  pub(in crate::engine::v4::first_authority) fn validate_captured_semantic_source_union_observed(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticSourceUnionValidationBoundsV1,
    before_complete: impl FnOnce(),
  ) -> UnionResult<SemanticSourceUnionValidationSummaryV1> {
    let catalog = CatalogReadOperationV1::new(self, bounds.catalog, None)?;
    let (companion, checkpoint_bytes) = catalog.load_companion_and_checkpoint(task_id, checkpoint_sequence)?;
    let manifest =
      decode_semantic_source_capture_v1(&companion.bytes, catalog.algorithm()).map_err(SemanticMutationObservationErrorV1::from)?;
    let checkpoint =
      decode_semantic_mutation_checkpoint(&checkpoint_bytes, catalog.algorithm()).map_err(SemanticMutationObservationErrorV1::from)?;
    let header = &self.header.selected.header;
    validate_captured_authority_header(&self.header.selected, &self.header.selected, manifest.base_namespace_root)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    // Canonical metadata loaders may encounter their framing caps before typed
    // validation. Reserve overlapping buffers, not a full directory tree.
    let semantic_cap = crate::engine::v4::semantic_store::semantic_object_cap(1).map_err(SemanticMutationObservationErrorV1::from)?;
    let metadata_bytes = 4 * (FIRST_AUTHORITY_NAMESPACE_ROOT_ENTITY_CAP + semantic_cap) as u64
      + 8 * (SystemControlKindV1::RootAdmissionCommit.encoded_cap() + FIRST_AUTHORITY_CONTROL_ENTITY_CAP) as u64
      + SOURCE_SCRATCH_BYTES;
    let metadata = self
      .memory
      .reserve(MemoryOwner::Task, metadata_bytes, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let parts = load_namespace_authority_parts_from_lookup(
      &self._protection.publisher().file,
      catalog.lookup(),
      &self.header.selected,
      manifest.base_namespace_root,
      &self.cancellation,
      |_| Ok(()),
    )
    .map_err(catalog_read_error)?
    .ok_or_else(|| invalid("semantic_source_union_base_missing", "retained source base NamespaceRoot is missing"))?;
    let base = decode_namespace_semantic_binding(
      NamespaceSemanticBindingInputV1 {
        expected_root_hash: manifest.base_namespace_root,
        expected_database_id: &header.database_id,
        root_entity: Some(&parts.root_entity),
        semantic_state_object: parts.semantic_state.as_ref().map(|loaded| loaded.body.as_slice()),
        admission_control: parts.admission.as_ref().map(|loaded| loaded.body.as_slice()),
      },
      catalog.algorithm(),
      header.write_sequence_high_water,
    )?;
    validate_captured_root_admission_sequence(&base.admission, header).map_err(SemanticMutationObservationErrorV1::from)?;
    drop(parts);
    catalog.check()?;
    metadata.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;

    let namespace = NamespaceSourceOperationV1::with_read_admission(
      self,
      NativeSemanticNamespaceSourceRequestV1 { tree_root: manifest.staged_directory_root, bounds: bounds.namespace },
      catalog.read_admission(),
    )?;
    let operation = RetainedSourceUnionOperationV1 { catalog: &catalog, namespace: &namespace, manifest: &manifest, bounds };
    super::super::super::plugin_sources::validate_plugin_source_bounds(operation.plugin_bounds())?;
    let mut aliases = 0u64;
    let mut base_configurations = 0u64;
    let mut requested_configurations = 0u64;
    let mut maximum_path_bytes = 1usize;
    let protected = catalog.visit_pairs(&manifest, |path, base, requested| -> UnionResult<bool> {
      maximum_path_bytes = maximum_path_bytes.max(path.len());
      let kind = match path {
        GLOBAL_INDEXES => {
          base_configurations = u64::from(base.is_some());
          requested_configurations = u64::from(requested.is_some());
          SemanticSourceAliasKindV1::IndexConfiguration
        }
        GLOBAL_PARSERS => SemanticSourceAliasKindV1::ParserRegistry,
        _ => return Ok(true),
      };
      for source in [base, requested] {
        operation.validate_aliases(kind, source.map(|source| source.body()), bounds.catalog.maximum_source_bytes, &mut aliases)?;
      }
      Ok(true)
    })?;
    let mut namespace_paths = 0u64;
    {
      let mut namespaces = NamespaceSourcePairCursorV1::new(&namespace, &base.root.namespace_tree_root, manifest.staged_directory_root)?;
      while let Some(pair) = namespaces.next_pair(&namespace)? {
        namespace_paths = increment_count(namespace_paths)?;
        maximum_path_bytes = maximum_path_bytes.max(pair.path.len());
        if pair.base.is_some() {
          base_configurations = increment_count(base_configurations)?;
        }
        if pair.requested.is_some() {
          requested_configurations = increment_count(requested_configurations)?;
        }
        for source in [pair.base.as_ref(), pair.requested.as_ref()].into_iter().flatten() {
          operation.validate_aliases(
            SemanticSourceAliasKindV1::IndexConfiguration,
            Some(source.body()),
            bounds.namespace.sources.maximum_body_bytes,
            &mut aliases,
          )?;
        }
      }
    }
    if requested_configurations != checkpoint.expected_configuration_count {
      return Err(
        invalid("semantic_source_union_configuration_count", "retained requested configurations disagree with the checkpoint count").into(),
      );
    }
    let fingerprint = operation.fingerprint(&base.root.namespace_tree_root, namespace_paths, maximum_path_bytes)?;
    if fingerprint.digest() != manifest.source_identity_fingerprint {
      return Err(
        invalid("semantic_source_union_fingerprint", "retained protected and namespace sources disagree with the captured fingerprint")
          .into(),
      );
    }
    before_complete();
    catalog.check()?;
    namespace.check()?;
    metadata.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    let (read_bytes, catalog_work) = catalog.statistics();
    Ok(SemanticSourceUnionValidationSummaryV1 {
      protected_paths: protected.paths,
      namespace_paths,
      base_configuration_count: base_configurations,
      requested_configuration_count: requested_configurations,
      read_bytes,
      catalog_work,
      namespace_work: bounds.namespace.maximum_work - namespace.lookup.remaining_work.get(),
    })
  }
}

struct RetainedSourceUnionOperationV1<'a, 'operation, 'manifest, A> {
  catalog: &'operation CatalogReadOperationV1<'a, 'operation>,
  namespace: &'operation NamespaceSourceOperationV1<'a, A>,
  manifest: &'manifest SemanticSourceCaptureV1<'manifest>,
  bounds: NativeSemanticSourceUnionValidationBoundsV1,
}

impl<A: NamespaceReadAdmissionV1> RetainedSourceUnionOperationV1<'_, '_, '_, A> {
  fn plugin_bounds(&self) -> NativeSemanticPluginSourceBoundsV1 {
    NativeSemanticPluginSourceBoundsV1 {
      maximum_module_bytes: self.bounds.maximum_plugin_module_bytes,
      maximum_chunk_entity_bytes: self.bounds.catalog.maximum_chunk_entity_bytes,
      maximum_source_chunks: self.bounds.catalog.maximum_source_chunks,
      maximum_read_bytes: self.bounds.catalog.maximum_read_bytes,
      maximum_workspace_bytes: self.bounds.maximum_plugin_workspace_bytes,
    }
  }

  fn validate_aliases(
    &self,
    kind: SemanticSourceAliasKindV1,
    source: Option<&[u8]>,
    maximum_source_bytes: usize,
    count: &mut u64,
  ) -> UnionResult<()> {
    let mut failure = None;
    let result = visit_semantic_source_aliases_v1(
      SemanticSourceAliasRequestV1 {
        kind,
        source,
        maximum_source_bytes,
        maximum_workspace_bytes: self.bounds.maximum_alias_workspace_bytes,
        maximum_alias_occurrences: self.bounds.maximum_alias_occurrences,
      },
      &mut |_, alias| {
        let result = (|| -> UnionResult<()> {
          *count = count
            .checked_add(1)
            .filter(|count| *count <= self.bounds.maximum_alias_occurrences)
            .ok_or_else(|| resource("semantic_source_union_alias_work", "source union exceeds its alias occurrence limit"))?;
          self.namespace.lookup.charge_work(1).map_err(map_namespace_read_error)?;
          self.catalog.check()?;
          for root in [self.manifest.base_source_catalog, self.manifest.requested_source_catalog] {
            self.namespace.capture.read_plugin_sources_with_selected_reader(
              alias,
              self.plugin_bounds(),
              |path, bounds| {
                let selected = self.catalog.read_selected(root, path, bounds)?;
                if selected.disposition() == SemanticSourceLookupDispositionV1::Unlisted {
                  return Err(invalid(
                    "semantic_source_catalog_unlisted",
                    "required compiler input is not listed in its retained source catalog",
                  ));
                }
                Ok(selected.into_source())
              },
              || {},
            )?;
          }
          self.catalog.check()?;
          self.namespace.check()?;
          Ok(())
        })();
        bridge_failure(result, &mut failure)
      },
      &self.namespace.capture.memory,
      &|| self.namespace.capture.cancellation.is_cancelled(),
    );
    finish_bridge(result, failure)?;
    Ok(())
  }

  fn fingerprint(
    &self,
    base_tree: &[u8],
    namespace_count: u64,
    maximum_path_bytes: usize,
  ) -> UnionResult<SemanticMutationSourceFingerprintV1> {
    let mut protected = SourceCatalogCursorV1::new(self.manifest.base_source_catalog, self.bounds.catalog.maximum_depth)?;
    let mut namespaces = NamespaceSourcePairCursorV1::new(self.namespace, base_tree, self.manifest.staged_directory_root)?;
    let mut protected_next = protected.next_row(self.catalog)?;
    let mut namespace_next = namespaces.next_pair(self.namespace)?;
    let mut failure = None;
    let rows = std::iter::from_fn(|| {
      let row = (|| -> UnionResult<Option<SemanticMutationSourceIdentityV1>> {
        self.catalog.check()?;
        self.namespace.check()?;
        let protected_first = match (&protected_next, &namespace_next) {
          (None, None) => return Ok(None),
          (Some(_), None) => true,
          (None, Some(_)) => false,
          (Some(protected), Some(namespace)) => match protected.name.as_bytes().cmp(namespace.path.as_bytes()) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => return Err(invalid("semantic_source_union_overlap", "namespace and protected families overlap").into()),
          },
        };
        let row = if protected_first {
          let source = protected_next
            .take()
            .ok_or_else(|| invalid("semantic_source_union_cursor", "protected source cursor lost its selected row"))?;
          let file_record_id = if source.hash.iter().all(|byte| *byte == 0) { None } else { Some(source.hash) };
          let row = SemanticMutationSourceIdentityV1 { path: source.name, file_record_id };
          protected_next = protected.next_row(self.catalog)?;
          row
        } else {
          let pair = namespace_next
            .take()
            .ok_or_else(|| invalid("semantic_source_union_cursor", "namespace source cursor lost its selected row"))?;
          let row = SemanticMutationSourceIdentityV1 {
            path: pair.path,
            file_record_id: pair.base.as_ref().map(|source| copy_namespace_bytes(source.revision())).transpose()?,
          };
          namespace_next = namespaces.next_pair(self.namespace)?;
          row
        };
        self.catalog.check()?;
        self.namespace.check()?;
        Ok(Some(row))
      })();
      bridge_failure(row, &mut failure).transpose()
    });
    let expected_record_count = self
      .manifest
      .protected_path_count
      .checked_add(namespace_count)
      .ok_or_else(|| resource("semantic_source_union_count", "complete source count overflowed"))?;
    let result = fingerprint_semantic_mutation_sources_v1(
      SemanticMutationSourceFingerprintRequestV1 {
        hash_algorithm: self.catalog.algorithm(),
        expected_record_count,
        maximum_path_bytes,
        maximum_workspace_bytes: self.bounds.maximum_fingerprint_workspace_bytes,
      },
      rows,
      &self.namespace.capture.memory,
      &|| self.namespace.capture.cancellation.is_cancelled(),
    );
    finish_bridge(result, failure)
  }
}

fn increment_count(count: u64) -> Result<u64, SemanticMutationObservationErrorV1> {
  count.checked_add(1).ok_or_else(|| resource("semantic_source_union_count", "configuration or namespace count overflowed"))
}
