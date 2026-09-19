//! Incremental composition under the compiler's single staging owner.
use super::*;
use super::super::config_value::{BorrowedCanonicalValueV1, CanonicalValueBounds, borrow_canonical_value};
use super::super::dependency::encode_dependency_record;
use super::super::field_definition::decode_field_index_definition;
use super::super::scope::validate_canonical_absolute_path;
use super::super::semantic_catalog::walk_semantic_catalog_with_mutable_source_v1;
use super::super::value_store::decode_value_store_definition;

#[path = "semantic_catalog_continuation.rs"]
mod continuation;
pub use continuation::SemanticCatalogContinuationV1;

#[derive(Clone, Copy, Debug)]
pub struct SemanticCatalogUpdateRequestV1 {
  /// The configuration count describes the final candidate, not this stream.
  pub compilation: SemanticCatalogCompilationRequestV1,
  pub expected_mutation_count: u64,
}

pub enum SemanticCatalogConfigurationMutationV1 {
  Upsert(CompiledIndexConfigurationV1),
  /// Canonical absolute directory owning the configuration, not its file path.
  Remove(String),
}

/// Apply an exact ordered stream to an opaque, previously completed compiler
/// result. Repeated owners apply in order. Only the final unselected candidate
/// is returned; failures leave inert immutable objects and preserve the base.
///
/// The caller pins both the base and staged objects. This does not admit an
/// arbitrary persisted root or activate HEAD. A changed parser registry needs
/// fresh compilation, since unchanged configurations could otherwise retain
/// obsolete automatic-parser contexts.
pub fn update_semantic_catalog_v1(
  request: SemanticCatalogUpdateRequestV1,
  previous: &CompiledSemanticCatalogV1,
  registry: &CompiledParserRegistryV1,
  mutations: impl IntoIterator<Item = std::result::Result<SemanticCatalogConfigurationMutationV1, SemanticCompilationErrorV1>>,
  store: &mut dyn SemanticCatalogStagingStoreV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledSemanticCatalogV1> {
  let mut compiler = Compiler::new(request.compilation, registry, memory, is_cancelled)?;
  compiler.inherit_complete(previous, registry, store)?;
  let mut candidates = DependencyCandidates::default();
  let mut mutations = mutations.into_iter();
  let mut count = 0u64;
  loop {
    compiler.check()?;
    let next = mutations.next();
    compiler.check()?;
    let Some(mutation) = next else { break };
    let mutation = mutation?;
    if count >= request.expected_mutation_count {
      return Err(invalid("semantic_catalog_mutation_count", "source enumerated more mutations than captured"));
    }
    compiler.apply_configuration_mutation(mutation, &mut candidates, store)?;
    count += 1;
  }
  if count != request.expected_mutation_count || compiler.configurations != request.compilation.expected_configuration_count {
    return Err(invalid("semantic_catalog_mutation_count", "source mutation or final configuration count disagrees with capture"));
  }
  compiler.prune_dependencies(&mut candidates, store)?;
  // The opaque base proves the unchanged closure; checked ownership edges,
  // exact COW counts and readback prove its delta. Do not rescan all definitions
  // for a fieldless change. Persisted admission must establish the same proof
  // before it can ever issue this opaque result.
  compiler.finish(store)
}

impl Compiler<'_> {
  fn inherit_complete(
    &mut self,
    previous: &CompiledSemanticCatalogV1,
    registry: &CompiledParserRegistryV1,
    store: &dyn SemanticCatalogStagingStoreV1,
  ) -> Result<()> {
    if previous.hash_algorithm != self.request.hash_algorithm {
      return Err(invalid("semantic_catalog_base_algorithm", "base was compiled with another hash algorithm"));
    }
    self.root = previous.catalog_root.as_deref().map(copy_bytes).transpose()?;
    self.records = previous.record_count;
    self.nodes = previous.node_count;
    self.dependencies = previous.dependency_count;
    self.configurations = previous.configuration_count;
    self.validate_captured_registry(registry, store)
  }

  fn validate_captured_registry(&self, registry: &CompiledParserRegistryV1, store: &dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    self.validate_definition(2, registry.projection())?;
    let projection = self
      .definition(2, b"\x02\x00/.aeordb-config/parsers.json", store)?
      .ok_or_else(|| corrupt("semantic_catalog_registry", "compiler base has no registry binding"))?;
    let expected =
      decode_semantic_definition_record(&registry.projection().object.value, self.request.hash_algorithm).map_err(format_error)?;
    if projection != expected.definition {
      return Err(invalid("semantic_catalog_registry_changed", "incremental configuration changes require the captured registry"));
    }
    Ok(())
  }

  fn apply_configuration_mutation(
    &mut self,
    mutation: SemanticCatalogConfigurationMutationV1,
    candidates: &mut DependencyCandidates,
    store: &mut dyn SemanticCatalogStagingStoreV1,
  ) -> Result<()> {
    match mutation {
      SemanticCatalogConfigurationMutationV1::Upsert(configuration) => {
        if configuration.registry_projection_id() != self.registry_projection_id {
          return Err(invalid("semantic_catalog_configuration_registry", "configuration was compiled against another registry projection"));
        }
        let scope = decode_scope_definition(&configuration.scope().value, self.request.hash_algorithm).map_err(format_error)?;
        self.remove_configuration(scope.owner_path, candidates, store)?;
        self.configuration(&configuration, store)?;
        // These exact dependencies are now proven live without a catalog
        // scan. A later removal in this stream will nominate them again.
        for dependency in configuration.dependencies() {
          if candidates.root.is_none() {
            break;
          }
          let definition =
            decode_semantic_definition_record(&dependency.object.value, self.request.hash_algorithm).map_err(format_error)?;
          candidates.mutate(
            self,
            SemanticCatalogMutationV1::Remove { record_kind: definition.class, owner_key: &dependency.semantic_id },
            store,
          )?;
        }
        self.configurations =
          self.configurations.checked_add(1).ok_or_else(|| resource("semantic_catalog_counts", "configuration count overflow"))?;
      }
      SemanticCatalogConfigurationMutationV1::Remove(path) => self.remove_configuration(&path, candidates, store)?,
    }
    Ok(())
  }

  fn definition(&self, class: u16, owner: &[u8], store: &dyn SemanticCatalogStagingStoreV1) -> Result<Option<Vec<u8>>> {
    self.check()?;
    let Some(root) = self.root.as_deref() else { return Ok(None) };
    let reader = SemanticCatalogReaderV1::new(self.request.hash_algorithm, store);
    Ok(reader.with_record(
      root,
      SemanticCatalogTraversalBoundsV1::new(self.records, self.nodes)?,
      class,
      owner,
      self.is_cancelled,
      |record| {
        reader.with_definition(record, self.is_cancelled, |bytes| {
          let rebuilt = encode_semantic_definition_object(class, bytes, self.request.hash_algorithm)
            .map_err(|error| catalog_check_error(format_error(error)))?;
          if rebuilt.semantic_id != record.semantic_id || rebuilt.object.object_id != record.definition_object_id {
            return Err(catalog_check_error(corrupt(
              "semantic_catalog_definition_closure",
              "affected definition disagrees with its binding",
            )));
          }
          copy_bytes(bytes).map_err(catalog_check_error)
        })
      },
    )?)
  }

  fn required_definition(&self, class: u16, owner: &[u8], store: &dyn SemanticCatalogStagingStoreV1) -> Result<Vec<u8>> {
    self
      .definition(class, owner, store)?
      .ok_or_else(|| corrupt("semantic_catalog_definition_missing", "affected definition binding is missing"))
  }

  fn remove_configuration(
    &mut self,
    path: &str,
    candidates: &mut DependencyCandidates,
    store: &mut dyn SemanticCatalogStagingStoreV1,
  ) -> Result<()> {
    self.check()?;
    validate_canonical_absolute_path(path).map_err(|error| invalid("semantic_catalog_configuration_path", error.to_string()))?;
    let owner = configuration_owner(path)?;
    let Some(bytes) = self.definition(1, &owner, store)? else {
      return Ok(());
    };
    let projection = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).map_err(format_error)?;
    let (fields, scope) = two_members(projection, "fields", "scope_id")?;
    let scope_id = identifier(scope, self.request.hash_algorithm)?;
    let scope_bytes = self.required_definition(3, scope_id, store)?;
    let scope = decode_scope_definition(&scope_bytes, self.request.hash_algorithm).map_err(format_error)?;
    if scope.scope_id != scope_id || scope.owner_path != path {
      return Err(corrupt("semantic_catalog_configuration_owner", "projection scope does not belong to the requested configuration"));
    }
    for field in fields.map_entries().map_err(format_error)? {
      self.check()?;
      let (name, field) = field.map_err(format_error)?;
      let (indexes, value) = two_members(field, "indexes", "value_store_id")?;
      let value_id = identifier(value, self.request.hash_algorithm)?;
      let value_bytes = self.required_definition(4, value_id, store)?;
      let value = decode_value_store_definition(&value_bytes, self.request.hash_algorithm).map_err(format_error)?;
      if value.value_store_id != value_id || value.scope_id != scope_id || value.field_name != name {
        return Err(corrupt("semantic_catalog_value_owner", "projection field does not own its ValueStore"));
      }
      let mut previous_index: Option<&[u8]> = None;
      for index in indexes.array_entries().map_err(format_error)? {
        self.check()?;
        let index_id = identifier(index.map_err(format_error)?, self.request.hash_algorithm)?;
        if previous_index.is_some_and(|previous| previous >= index_id) {
          return Err(corrupt("semantic_catalog_index_order", "projection indexes are not strictly ordered"));
        }
        previous_index = Some(index_id);
        let index_bytes = self.required_definition(5, index_id, store)?;
        let index = decode_field_index_definition(&index_bytes, self.request.hash_algorithm).map_err(format_error)?;
        if index.index_id != index_id || index.value_store_id != value_id {
          return Err(corrupt("semantic_catalog_index_owner", "projection index does not belong to its ValueStore"));
        }
        self.remove_binding(5, index_id, store)?;
      }
      if previous_index.is_none() {
        return Err(corrupt("semantic_catalog_projection_schema", "compiler field has no indexes"));
      }
      visit_dependencies(4, &value_bytes, self.request.hash_algorithm, |class, definition| {
        self.check()?;
        let retained = self.required_definition(class, &definition.semantic_id, store)?;
        let expected = decode_semantic_definition_record(&definition.object.value, self.request.hash_algorithm).map_err(format_error)?;
        if retained != expected.definition {
          return Err(corrupt("semantic_catalog_dependency_closure", "ValueStore dependency differs from its catalog definition"));
        }
        candidates.mutate(
          self,
          SemanticCatalogMutationV1::Upsert(SemanticCatalogRecordV1 {
            record_kind: class,
            owner_key: &definition.semantic_id,
            semantic_id: &definition.semantic_id,
            definition_object_id: &definition.object.object_id,
          }),
          store,
        )
      })?;
      self.remove_binding(4, value_id, store)?;
    }
    self.remove_binding(3, scope_id, store)?;
    self.remove_binding(1, &owner, store)?;
    self.configurations =
      self.configurations.checked_sub(1).ok_or_else(|| corrupt("semantic_catalog_counts", "configuration count underflow"))?;
    Ok(())
  }

  fn remove_binding(&mut self, class: u16, owner: &[u8], store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    let plan = self.plan_mutation(
      SemanticCatalogSnapshotV1 { root_object_id: self.root.as_deref(), record_count: self.records, node_count: self.nodes },
      SemanticCatalogMutationV1::Remove { record_kind: class, owner_key: owner },
      store,
    )?;
    let removed =
      self.records.checked_sub(plan.record_count()).ok_or_else(|| corrupt("semantic_catalog_counts", "removal increased catalog count"))?;
    let dependencies = if matches!(class, 6 | 7) {
      self.dependencies.checked_sub(removed).ok_or_else(|| corrupt("semantic_catalog_counts", "dependency count underflow"))?
    } else {
      self.dependencies
    };
    let root = plan.root_object_id().map(copy_bytes).transpose()?;
    if !plan.objects().is_empty() {
      self.publish(plan.objects(), store)?;
    }
    self.check()?;
    self.root = root;
    self.records = plan.record_count();
    self.nodes = plan.node_count();
    self.dependencies = dependencies;
    Ok(())
  }

  fn prune_dependencies(&mut self, candidates: &mut DependencyCandidates, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    self.exclude_live_candidates(candidates, store)?;
    let Some(root) = candidates.root.as_deref() else {
      return self.check();
    };
    let is_cancelled = self.is_cancelled;
    walk_semantic_catalog_with_mutable_source_v1(
      self.request.hash_algorithm,
      store,
      root,
      SemanticCatalogTraversalBoundsV1::new(candidates.records, candidates.nodes)?,
      is_cancelled,
      |record, store| self.remove_binding(record.record_kind, record.owner_key, store).map_err(catalog_check_error),
    )?;
    self.check()
  }

  fn exclude_live_candidates(&self, candidates: &mut DependencyCandidates, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    if candidates.root.is_none() {
      return self.check();
    }
    let algorithm = self.request.hash_algorithm;
    let root = self.root.as_deref().ok_or_else(|| corrupt("semantic_catalog_root", "registry binding was removed"))?;
    walk_semantic_catalog_with_mutable_source_v1(
      algorithm,
      store,
      root,
      SemanticCatalogTraversalBoundsV1::new(self.records, self.nodes)?,
      self.is_cancelled,
      |record, store| {
        self.check().map_err(catalog_check_error)?;
        if !matches!(record.record_kind, 2 | 4) || candidates.root.is_none() {
          return Ok(());
        }
        // End the source borrow before staging auxiliary COW removals.
        let bytes = SemanticCatalogReaderV1::new(algorithm, store)
          .with_definition(record, self.is_cancelled, |bytes| copy_bytes(bytes).map_err(catalog_check_error))?;
        visit_dependencies(record.record_kind, &bytes, algorithm, |class, definition| {
          candidates.mutate(self, SemanticCatalogMutationV1::Remove { record_kind: class, owner_key: &definition.semantic_id }, store)
        })
        .map_err(catalog_check_error)
      },
    )?;
    self.check()
  }
}

/// Only actual dependency bindings removed by this update. This is an inert
/// auxiliary COW tree, not a persistent refcount or a whole-world resident set.
#[derive(Default)]
struct DependencyCandidates {
  root: Option<Vec<u8>>,
  records: u64,
  nodes: u64,
}

impl DependencyCandidates {
  fn mutate(
    &mut self,
    compiler: &Compiler<'_>,
    mutation: SemanticCatalogMutationV1<'_>,
    store: &mut dyn SemanticCatalogStagingStoreV1,
  ) -> Result<()> {
    let plan = compiler.plan_mutation(
      SemanticCatalogSnapshotV1 { root_object_id: self.root.as_deref(), record_count: self.records, node_count: self.nodes },
      mutation,
      store,
    )?;
    let root = plan.root_object_id().map(copy_bytes).transpose()?;
    if !plan.objects().is_empty() {
      compiler.publish(plan.objects(), store)?;
    }
    compiler.check()?;
    self.root = root;
    self.records = plan.record_count();
    self.nodes = plan.node_count();
    Ok(())
  }
}

pub(super) fn visit_dependencies(
  class: u16,
  bytes: &[u8],
  algorithm: HashAlgorithm,
  mut visit: impl FnMut(u16, &EncodedSemanticDefinitionObjectV1) -> Result<()>,
) -> Result<()> {
  match class {
    2 => {
      let registry = borrow_canonical_value(bytes, CanonicalValueBounds::CONFIG).map_err(format_error)?;
      for entry in registry.map_entries().map_err(format_error)? {
        let (_, value) = entry.map_err(format_error)?;
        let bytes = value.as_bytes().ok_or_else(|| corrupt("semantic_catalog_projection_schema", "registry dependency is not Bytes"))?;
        let definition = encode_semantic_definition_object(6, bytes, algorithm).map_err(format_error)?;
        visit(6, &definition)?;
      }
    }
    4 => {
      let value = decode_value_store_definition(bytes, algorithm).map_err(format_error)?;
      for dependency in value.dependencies.records {
        let class = if dependency.kind == 1 { 6 } else { 7 };
        let bytes = encode_dependency_record(&dependency).map_err(format_error)?;
        let definition = encode_semantic_definition_object(class, &bytes, algorithm).map_err(format_error)?;
        visit(class, &definition)?;
      }
    }
    _ => return Err(corrupt("semantic_catalog_dependency_parent", "only registry and ValueStore definitions own dependencies")),
  }
  Ok(())
}

pub(super) fn two_members<'a>(
  value: BorrowedCanonicalValueV1<'a>,
  first: &str,
  second: &str,
) -> Result<(BorrowedCanonicalValueV1<'a>, BorrowedCanonicalValueV1<'a>)> {
  let mut entries = value.map_entries().map_err(format_error)?;
  let mut next = |expected| -> Result<_> {
    let (name, value) = entries
      .next()
      .ok_or_else(|| corrupt("semantic_catalog_projection_schema", "required projection member is missing"))?
      .map_err(format_error)?;
    if name != expected {
      return Err(corrupt("semantic_catalog_projection_schema", "unexpected projection member"));
    }
    Ok(value)
  };
  let pair = (next(first)?, next(second)?);
  match entries.next() {
    None => Ok(pair),
    Some(Err(error)) => Err(format_error(error)),
    Some(Ok(_)) => Err(corrupt("semantic_catalog_projection_schema", "extra projection member")),
  }
}

pub(super) fn identifier(value: BorrowedCanonicalValueV1<'_>, algorithm: HashAlgorithm) -> Result<&[u8]> {
  let bytes = value.as_bytes().ok_or_else(|| corrupt("semantic_catalog_projection_schema", "projection ID is not Bytes"))?;
  if bytes.len() != algorithm.hash_length() || bytes.iter().all(|byte| *byte == 0) {
    return Err(corrupt("semantic_catalog_projection_schema", "projection ID is zero or has the wrong width"));
  }
  Ok(bytes)
}

#[cfg(test)]
#[path = "../../../spec/engine/semantic_catalog_update_boundary_spec.rs"]
mod boundary_tests;
