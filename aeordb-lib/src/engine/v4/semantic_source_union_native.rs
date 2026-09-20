//! Complete source preparation under one captured view; no durable task grant.
#[path = "semantic_source_union_validation.rs"]
mod validation;
pub use validation::{NativeSemanticSourceUnionValidationBoundsV1, SemanticSourceUnionValidationSummaryV1};
pub use validation::{NativeSemanticCompilerProgressBoundsV1, NativeSemanticCompilerProgressV1, SemanticCompilerConstructionModeV1};
use super::*;
use super::super::super::source_base::NativeSemanticSourceBaseV1;
use crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use crate::engine::v4::semantic_mutation_control::{
  fingerprint_semantic_mutation_sources_v1, SemanticMutationSourceFingerprintRequestV1, SemanticMutationSourceFingerprintV1,
  SemanticMutationSourceIdentityV1,
};
use crate::engine::v4::semantic_source_capture::{
  build_semantic_source_catalog_pair_v1, visit_semantic_source_aliases_v1, SemanticSourceAliasKindV1, SemanticSourceAliasRequestV1,
  SemanticSourceCatalogBuildRequestV1, SemanticSourceCatalogPairRowV1, SemanticSourceCatalogPairV1, SemanticSourcePathWorkspaceBoundsV1,
  SemanticSourcePathWorkspaceBuilderV1, SemanticSourcePathWorkspaceV1,
};
use crate::engine::v4::plugin_identity::{plugin_alias_path_v1, ALIAS_MAX_LENGTH};
use std::cmp::Ordering;
use std::path::Path;

type UnionResult<T> = Result<T, NativeSemanticSourceUnionErrorV1>;
const GLOBAL_INDEXES: &str = "/.aeordb-config/indexes.json";
const GLOBAL_PARSERS: &str = "/.aeordb-config/parsers.json";

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceReplacementV1<'a> {
  pub path: &'a str,
  /// None is explicit deletion. Omitted paths retain their captured current value.
  pub file_record_id: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticSourceUnionBoundsV1 {
  /// Internal read/work budgets span both trees and all preparation passes.
  /// Source chunk limits also apply to selected plugin inputs. Caller callback
  /// I/O, publication, work and retained sink buffers need separate accounting.
  pub namespace: NativeSemanticNamespaceSourceBoundsV1,
  pub paths: SemanticSourcePathWorkspaceBoundsV1,
  pub maximum_plugin_module_bytes: usize,
  pub maximum_plugin_workspace_bytes: usize,
  pub maximum_alias_occurrences: u64,
  pub maximum_alias_workspace_bytes: usize,
  pub maximum_catalog_workspace_bytes: usize,
  pub maximum_catalog_node_pairs: u64,
  pub maximum_catalog_output_bytes: u64,
  pub maximum_fingerprint_workspace_bytes: usize,
}

pub struct NativeSemanticSourceUnionRequestV1<'a> {
  pub expected_base_root: &'a [u8],
  pub requested_directory_root: &'a [u8],
  /// Strict path order; present revisions must already exist in this capture.
  pub replacements: &'a [NativeSemanticSourceReplacementV1<'a>],
  pub workspace_parent: &'a Path,
  pub bounds: NativeSemanticSourceUnionBoundsV1,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeSemanticSourceUnionErrorV1 {
  #[error(transparent)]
  CatalogCompilation(#[from] crate::engine::v4::semantic_catalog_compiler::SemanticCatalogCompilationErrorV1),
  #[error(transparent)]
  Catalog(#[from] crate::engine::v4::semantic_catalog::SemanticCatalogReadErrorV1),
  #[error(transparent)]
  RootAuthority(#[from] crate::engine::v4::root_authority::RootAuthorityReadError),
  #[error(transparent)]
  Source(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Namespace(#[from] NativeSemanticNamespaceSourceErrorV1),
  #[error(transparent)]
  Plugin(#[from] NativeSemanticPluginSourceErrorV1),
  #[error(transparent)]
  Compilation(#[from] SemanticCompilationErrorV1),
  #[error(transparent)]
  Publication(#[from] NativeSemanticSourcePublicationErrorV1),
  #[error(transparent)]
  ControlPublication(#[from] NativeSemanticSourceControlPublicationErrorV1),
}

pub struct NativeSemanticSourceUnionV1<'a> {
  capture: &'a NativeSemanticMutationInventoryV1<'a>,
  base: NativeSemanticSourceBaseV1<'a>,
  requested_directory_root: Vec<u8>,
  catalogs: SemanticSourceCatalogPairV1,
  fingerprint: SemanticMutationSourceFingerprintV1,
  requested_configuration_count: u64,
  _memory: MemoryReservation,
}

impl NativeSemanticSourceUnionV1<'_> {
  pub fn captured_header(&self) -> &DatabaseHeaderV4 {
    &self.capture.header.selected.header
  }
  pub fn base_authority(&self) -> &SelectedSemanticAuthorityV1 {
    &self.base.authority
  }
  pub fn generation_selection(&self) -> &LoadedMutableSystemControlV1 {
    &self.base.generation
  }
  pub fn requested_directory_root(&self) -> &[u8] {
    &self.requested_directory_root
  }
  pub fn catalogs(&self) -> &SemanticSourceCatalogPairV1 {
    &self.catalogs
  }
  pub fn fingerprint(&self) -> &SemanticMutationSourceFingerprintV1 {
    &self.fingerprint
  }
  pub fn requested_configuration_count(&self) -> u64 {
    self.requested_configuration_count
  }
  pub(in crate::engine::v4::first_authority) fn captured_inventory(&self) -> &NativeSemanticMutationInventoryV1<'_> {
    self.capture
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  /// Callbacks may stage bytes while this capture holds protection, but every
  /// callback remains provisional until this whole operation succeeds. Caller
  /// owns its sink/backing-storage accounting. Completion is not durable task
  /// retention, resume permission, namespace publication or activation.
  pub fn prepare_semantic_source_union(
    &self,
    request: NativeSemanticSourceUnionRequestV1<'_>,
    mut emit_catalog_pair: impl FnMut(&[u8], &[u8]) -> Result<(), NativeSemanticSourceUnionErrorV1>,
    mut visit_sources: impl FnMut(
      &str,
      Option<&NativeProtectedSemanticSourceV1<'_>>,
      Option<&NativeProtectedSemanticSourceV1<'_>>,
    ) -> Result<(), NativeSemanticSourceUnionErrorV1>,
  ) -> Result<NativeSemanticSourceUnionV1<'_>, NativeSemanticSourceUnionErrorV1> {
    let operation = SourceUnionOperationV1 {
      namespace: NamespaceSourceOperationV1::new(
        self,
        NativeSemanticNamespaceSourceRequestV1 { tree_root: request.requested_directory_root, bounds: request.bounds.namespace },
      )?,
      replacements: request.replacements,
      bounds: request.bounds,
    };
    operation.validate()?;
    let base = self.read_source_base_from_lookup(request.expected_base_root, &operation.namespace.lookup, || {})?;
    // Iterator heads, copied identities/paths and result metadata are separate
    // from each decoded source's own reservation and the builders' workspaces.
    let maximum_path = request.bounds.paths.maximum_path_bytes.max(request.bounds.namespace.maximum_path_bytes);
    let metadata = (8 * (maximum_path + operation.namespace.algorithm().hash_length()) + 4096) as u64;
    let mut memory =
      self.memory.reserve(MemoryOwner::Task, metadata, AdmissionClass::Maintenance).map_err(SemanticMutationObservationErrorV1::from)?;
    let requested_directory_root = copy_namespace_bytes(request.requested_directory_root)?;
    let mut paths =
      SemanticSourcePathWorkspaceBuilderV1::new(request.workspace_parent, request.bounds.paths, &self.memory, &self.cancellation)?;
    for path in [GLOBAL_INDEXES, GLOBAL_PARSERS] {
      paths.append_path(path)?;
    }
    for replacement in request.replacements {
      paths.append_path(replacement.path)?;
    }
    let mut alias_count = 0;
    let mut requested_configuration_count = 0u64;
    for (path, kind) in
      [(GLOBAL_INDEXES, SemanticSourceAliasKindV1::IndexConfiguration), (GLOBAL_PARSERS, SemanticSourceAliasKindV1::ParserRegistry)]
    {
      for requested in [false, true] {
        let source = operation.read_selected(path, requested, operation.source_bounds(path)?)?;
        if requested && path == GLOBAL_INDEXES && source.is_some() {
          requested_configuration_count = 1;
        }
        operation.discover_aliases(kind, source.as_ref().map(|source| source.body()), &mut alias_count, &mut paths)?;
      }
    }
    let namespace_count = {
      let mut namespaces =
        NamespaceSourcePairCursorV1::new(&operation.namespace, &base.authority.namespace_tree_root, &requested_directory_root)?;
      let mut count = 0u64;
      while let Some(pair) = namespaces.next_pair(&operation.namespace)? {
        count = count.checked_add(1).ok_or_else(|| resource("semantic_source_union_count", "namespace union count overflowed"))?;
        if pair.requested.is_some() {
          requested_configuration_count = requested_configuration_count
            .checked_add(1)
            .ok_or_else(|| resource("semantic_source_union_count", "requested configuration count overflowed"))?;
        }
        for source in [pair.base.as_ref(), pair.requested.as_ref()].into_iter().flatten() {
          operation.discover_aliases(SemanticSourceAliasKindV1::IndexConfiguration, Some(source.body()), &mut alias_count, &mut paths)?;
        }
      }
      count
    };
    let paths = paths.finish()?;
    let catalogs = operation.build_catalogs(&paths, &mut emit_catalog_pair, &mut visit_sources)?;
    let fingerprint = operation.fingerprint(&paths, &base.authority.namespace_tree_root, &requested_directory_root, namespace_count)?;
    drop(paths);
    operation.namespace.check()?;
    let retained = requested_directory_root.capacity() as u64 + std::mem::size_of::<NativeSemanticSourceUnionV1<'_>>() as u64;
    memory.shrink(metadata - retained).map_err(SemanticMutationObservationErrorV1::from)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(NativeSemanticSourceUnionV1 {
      capture: self,
      base,
      requested_directory_root,
      catalogs,
      fingerprint,
      requested_configuration_count,
      _memory: memory,
    })
  }
}

struct SourceUnionOperationV1<'a, 'r> {
  namespace: NamespaceSourceOperationV1<'a>,
  replacements: &'r [NativeSemanticSourceReplacementV1<'r>],
  bounds: NativeSemanticSourceUnionBoundsV1,
}

impl<'a> SourceUnionOperationV1<'a, '_> {
  fn plugin_bounds(&self) -> NativeSemanticPluginSourceBoundsV1 {
    NativeSemanticPluginSourceBoundsV1 {
      maximum_module_bytes: self.bounds.maximum_plugin_module_bytes,
      maximum_chunk_entity_bytes: self.bounds.namespace.sources.maximum_chunk_entity_bytes,
      maximum_source_chunks: self.bounds.namespace.sources.maximum_chunks,
      maximum_read_bytes: self.bounds.namespace.sources.maximum_read_bytes,
      maximum_workspace_bytes: self.bounds.maximum_plugin_workspace_bytes,
    }
  }

  fn validate(&self) -> UnionResult<()> {
    super::super::plugin_sources::validate_plugin_source_bounds(self.plugin_bounds())?;
    if self.bounds.paths.maximum_path_bytes == 0
      || self.bounds.paths.maximum_path_bytes > u16::MAX as usize
      || self.replacements.len() as u64 > self.bounds.paths.maximum_input_paths
    {
      return Err(resource("semantic_source_union_replacements", "replacement paths exceed the admitted count or path limit").into());
    }
    let mut previous: Option<&str> = None;
    for replacement in self.replacements {
      self.namespace.check()?;
      self.namespace.lookup.charge_work(1).map_err(map_namespace_read_error)?;
      validate_source_path(replacement.path, self.namespace.algorithm())?;
      if replacement.path.len() > self.bounds.paths.maximum_path_bytes {
        return Err(resource("semantic_source_union_replacements", "replacement path exceeds its byte limit").into());
      }
      if previous.is_some_and(|previous| previous.as_bytes() >= replacement.path.as_bytes()) {
        return Err(invalid("semantic_source_union_replacement_order", "replacement paths must be strictly ordered and unique").into());
      }
      if let Some(revision) = replacement.file_record_id {
        if revision.len() != self.namespace.algorithm().hash_length() || revision.iter().all(|byte| *byte == 0) {
          return Err(
            invalid("semantic_source_union_replacement_identity", "replacement revision must be nonzero and selected-hash width").into(),
          );
        }
      }
      previous = Some(replacement.path);
    }
    self.namespace.check()?;
    Ok(())
  }

  fn source_bounds(&self, path: &str) -> Result<NativeSemanticSourceReadBoundsV1, SemanticMutationObservationErrorV1> {
    let maximum_body_bytes = match SystemFamilyPolicyResolverV1::embedded(self.namespace.algorithm())?
      .policy(SystemFamilySubjectV1::Path(path), "captured source union")?
    {
      SystemFamilyPolicyDecisionV1::Known { family_id: 0x0031, .. } => ALIAS_MAX_LENGTH,
      SystemFamilyPolicyDecisionV1::Known { family_id: 0x0032, .. } => self.bounds.maximum_plugin_module_bytes,
      _ => self.bounds.namespace.sources.maximum_body_bytes,
    };
    Ok(NativeSemanticSourceReadBoundsV1 { maximum_body_bytes, ..self.bounds.namespace.sources })
  }

  fn read_selected(
    &self,
    path: &str,
    requested: bool,
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<Option<NativeProtectedSemanticSourceV1<'a>>, SemanticMutationObservationErrorV1> {
    self.namespace.check()?;
    let replacement = if requested {
      let index = self.replacements.partition_point(|entry| entry.path.as_bytes() < path.as_bytes());
      self.replacements.get(index).filter(|entry| entry.path == path)
    } else {
      None
    };
    let revision = match replacement {
      Some(NativeSemanticSourceReplacementV1 { file_record_id: None, .. }) => return Ok(None),
      Some(replacement) => replacement.file_record_id,
      None => None,
    };
    let capture = self.namespace.capture;
    let source = capture.read_source_from_lookup(path, revision, bounds, &self.namespace.lookup, || {})?;
    if revision.is_some() && source.is_none() {
      return Err(invalid("semantic_source_retained_missing", "retained protected source is absent from the captured snapshot"));
    }
    self.namespace.check()?;
    Ok(source)
  }

  fn discover_aliases(
    &self,
    kind: SemanticSourceAliasKindV1,
    source: Option<&[u8]>,
    count: &mut u64,
    paths: &mut SemanticSourcePathWorkspaceBuilderV1,
  ) -> UnionResult<()> {
    let mut failure = None;
    let result = visit_semantic_source_aliases_v1(
      SemanticSourceAliasRequestV1 {
        kind,
        source,
        maximum_source_bytes: self.bounds.namespace.sources.maximum_body_bytes,
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
          self.namespace.check()?;
          let alias_path = plugin_alias_path_v1(alias).map_err(SemanticMutationObservationErrorV1::from)?;
          paths.append_path(&alias_path)?;
          // References on either side require BOTH selected aliases/modules,
          // including a replacement whose last configuration reference vanished.
          for requested in [false, true] {
            let pair = self.namespace.capture.read_plugin_sources_with_selected_reader(
              alias,
              self.plugin_bounds(),
              |path, bounds| self.read_selected(path, requested, bounds),
              || {},
            )?;
            if let Some(pair) = pair {
              paths.append_path(&pair.artifact_source().record().path)?;
            }
          }
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

  fn build_catalogs(
    &self,
    paths: &SemanticSourcePathWorkspaceV1,
    emit: &mut impl FnMut(&[u8], &[u8]) -> UnionResult<()>,
    visit: &mut impl FnMut(&str, Option<&NativeProtectedSemanticSourceV1<'_>>, Option<&NativeProtectedSemanticSourceV1<'_>>) -> UnionResult<()>,
  ) -> UnionResult<SemanticSourceCatalogPairV1> {
    let mut cursor = paths.open_cursor()?;
    let mut source_failure = None;
    let mut emit_failure = None;
    let rows = std::iter::from_fn(|| {
      let row = (|| -> UnionResult<Option<SemanticSourceCatalogPairRowV1>> {
        self.namespace.check()?;
        let Some(path) = cursor.next_path()? else { return Ok(None) };
        let bounds = self.source_bounds(path.as_str())?;
        let base = self.read_selected(path.as_str(), false, bounds)?;
        let requested = self.read_selected(path.as_str(), true, bounds)?;
        visit(path.as_str(), base.as_ref(), requested.as_ref())?;
        self.namespace.check()?;
        Ok(Some(SemanticSourceCatalogPairRowV1 {
          path: copy_union_path(path.as_str())?,
          base_file_record_id: base.as_ref().map(|source| copy_namespace_bytes(source.revision())).transpose()?,
          requested_file_record_id: requested.as_ref().map(|source| copy_namespace_bytes(source.revision())).transpose()?,
        }))
      })();
      bridge_failure(row, &mut source_failure).transpose()
    });
    let result = build_semantic_source_catalog_pair_v1(
      SemanticSourceCatalogBuildRequestV1 {
        database_id: self.namespace.capture.header.selected.header.database_id,
        hash_algorithm: self.namespace.algorithm(),
        expected_path_count: paths.path_count(),
        maximum_path_bytes: self.bounds.paths.maximum_path_bytes,
        maximum_workspace_bytes: self.bounds.maximum_catalog_workspace_bytes,
        maximum_node_pairs: self.bounds.maximum_catalog_node_pairs,
        maximum_output_bytes: self.bounds.maximum_catalog_output_bytes,
      },
      rows,
      &mut |left, right| {
        let result = (|| -> UnionResult<()> {
          self.namespace.check()?;
          emit(left, right)?;
          self.namespace.check()?;
          Ok(())
        })();
        bridge_failure(result, &mut emit_failure)
      },
      &self.namespace.capture.memory,
      &|| self.namespace.capture.cancellation.is_cancelled(),
    );
    finish_bridge(result, source_failure.or(emit_failure))
  }

  fn fingerprint(
    &self,
    paths: &SemanticSourcePathWorkspaceV1,
    base_root: &[u8],
    requested_root: &[u8],
    namespace_count: u64,
  ) -> UnionResult<SemanticMutationSourceFingerprintV1> {
    let mut protected = paths.open_cursor()?;
    let mut namespaces = NamespaceSourcePairCursorV1::new(&self.namespace, base_root, requested_root)?;
    let mut protected_next = protected.next_path()?;
    let mut namespace_next = namespaces.next_pair(&self.namespace)?;
    let mut failure = None;
    let rows = std::iter::from_fn(|| {
      let row = (|| -> UnionResult<Option<SemanticMutationSourceIdentityV1>> {
        self.namespace.check()?;
        let protected_first = match (&protected_next, &namespace_next) {
          (None, None) => return Ok(None),
          (Some(_), None) => true,
          (None, Some(_)) => false,
          (Some(protected), Some(namespace)) => match protected.as_str().as_bytes().cmp(namespace.path.as_bytes()) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => return Err(invalid("semantic_source_union_overlap", "namespace and protected families overlap").into()),
          },
        };
        let row = if protected_first {
          let path = protected_next
            .take()
            .ok_or_else(|| invalid("semantic_source_union_cursor", "protected source cursor lost its selected row"))?;
          let source = self.read_selected(path.as_str(), false, self.source_bounds(path.as_str())?)?;
          let row = SemanticMutationSourceIdentityV1 {
            path: copy_union_path(path.as_str())?,
            file_record_id: source.as_ref().map(|source| copy_namespace_bytes(source.revision())).transpose()?,
          };
          protected_next = protected.next_path()?;
          row
        } else {
          let pair = namespace_next
            .take()
            .ok_or_else(|| invalid("semantic_source_union_cursor", "namespace source cursor lost its selected row"))?;
          let row = SemanticMutationSourceIdentityV1 {
            path: pair.path,
            file_record_id: pair.base.as_ref().map(|source| copy_namespace_bytes(source.revision())).transpose()?,
          };
          namespace_next = namespaces.next_pair(&self.namespace)?;
          row
        };
        self.namespace.check()?;
        Ok(Some(row))
      })();
      bridge_failure(row, &mut failure).transpose()
    });
    let expected_record_count = paths
      .path_count()
      .checked_add(namespace_count)
      .ok_or_else(|| resource("semantic_source_union_count", "complete source count overflowed"))?;
    let result = fingerprint_semantic_mutation_sources_v1(
      SemanticMutationSourceFingerprintRequestV1 {
        hash_algorithm: self.namespace.algorithm(),
        expected_record_count,
        maximum_path_bytes: self.bounds.paths.maximum_path_bytes.max(self.bounds.namespace.maximum_path_bytes),
        maximum_workspace_bytes: self.bounds.maximum_fingerprint_workspace_bytes,
      },
      rows,
      &self.namespace.capture.memory,
      &|| self.namespace.capture.cancellation.is_cancelled(),
    );
    finish_bridge(result, failure)
  }
}

struct NamespaceSourcePairV1<'a> {
  path: String,
  base: Option<NativeSemanticNamespaceSourceV1<'a>>,
  requested: Option<NativeSemanticNamespaceSourceV1<'a>>,
}

struct NamespaceSourcePairCursorV1<'a> {
  base: NamespaceSourceStateV1,
  requested: NamespaceSourceStateV1,
  base_next: Option<NativeSemanticNamespaceSourceV1<'a>>,
  requested_next: Option<NativeSemanticNamespaceSourceV1<'a>>,
  base_ended: bool,
  requested_ended: bool,
  _memory: MemoryReservation,
}

impl<'a> NamespaceSourcePairCursorV1<'a> {
  fn new<A: NamespaceReadAdmissionV1>(operation: &NamespaceSourceOperationV1<'a, A>, base: &[u8], requested: &[u8]) -> UnionResult<Self> {
    let scratch = namespace_seek_workspace_bytes_v1(
      operation.bounds.maximum_path_bytes as u64,
      operation.bounds.maximum_path_depth as u64,
      operation.bounds.maximum_btree_depth as u64,
      operation.algorithm().hash_length() as u64,
    )
    .ok_or_else(|| resource("semantic_namespace_source_memory", "paired namespace workspace accounting overflowed"))?;
    // The operation owns one traversal reservation; account for the second
    // simultaneous state without increasing the standalone cursor's admission.
    let memory = operation
      .capture
      .memory
      .reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(Self {
      base: NamespaceSourceStateV1::new(operation, base)?,
      requested: NamespaceSourceStateV1::new(operation, requested)?,
      base_next: None,
      requested_next: None,
      base_ended: false,
      requested_ended: false,
      _memory: memory,
    })
  }

  fn next_pair<A: NamespaceReadAdmissionV1>(
    &mut self,
    operation: &NamespaceSourceOperationV1<'a, A>,
  ) -> UnionResult<Option<NamespaceSourcePairV1<'a>>> {
    operation.check()?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    if self.base_next.is_none() && !self.base_ended {
      self.base_next = self.base.next_source(operation)?;
      self.base_ended = self.base_next.is_none();
    }
    if self.requested_next.is_none() && !self.requested_ended {
      self.requested_next = self.requested.next_source(operation)?;
      self.requested_ended = self.requested_next.is_none();
    }
    let (path, take_base, take_requested) = match (&self.base_next, &self.requested_next) {
      (None, None) => {
        operation.check()?;
        return Ok(None);
      }
      (Some(base), None) => (base.record().path.as_str(), true, false),
      (None, Some(requested)) => (requested.record().path.as_str(), false, true),
      (Some(base), Some(requested)) => match base.record().path.as_bytes().cmp(requested.record().path.as_bytes()) {
        Ordering::Less => (base.record().path.as_str(), true, false),
        Ordering::Greater => (requested.record().path.as_str(), false, true),
        Ordering::Equal => (base.record().path.as_str(), true, true),
      },
    };
    let path = copy_union_path(path)?;
    let base = if take_base { self.base_next.take() } else { None };
    let requested = if take_requested { self.requested_next.take() } else { None };
    operation.check()?;
    Ok(Some(NamespaceSourcePairV1 { path, base, requested }))
  }
}

fn copy_union_path(path: &str) -> Result<String, SemanticMutationObservationErrorV1> {
  let mut result = String::new();
  result.try_reserve_exact(path.len()).map_err(namespace_allocation)?;
  result.push_str(path);
  Ok(result)
}

fn bridge_failure<T>(
  result: UnionResult<T>,
  failure: &mut Option<NativeSemanticSourceUnionErrorV1>,
) -> Result<T, SemanticCompilationErrorV1> {
  result.map_err(|error| {
    *failure = Some(error);
    SemanticCompilationErrorV1::Operational {
      path: "<native-semantic-source-union>",
      message: "source union operation failed; original cause retained".into(),
    }
  })
}

fn finish_bridge<T>(result: Result<T, SemanticCompilationErrorV1>, failure: Option<NativeSemanticSourceUnionErrorV1>) -> UnionResult<T> {
  match failure {
    Some(error) => Err(error),
    None => result.map_err(NativeSemanticSourceUnionErrorV1::from),
  }
}
