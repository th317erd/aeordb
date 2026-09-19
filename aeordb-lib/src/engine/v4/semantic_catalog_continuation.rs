//! Catalog-only continuation under the existing compiler and COW owner.
use super::*;
use super::super::super::semantic_mutation_control::SemanticMutationPhaseV1;

/// Unselected catalog work. Each consuming operation returns only a whole
/// configuration or pruning step; failure drops its lease and cannot expose
/// reusable half-updated state. The owner must retain immutable staging objects
/// and bind both catalog roots together when selecting a durable checkpoint.
/// This supplies no source-position, task-fence, GC-retention or HEAD authority.
pub struct SemanticCatalogContinuationV1<'a> {
  compiler: Compiler<'a>,
  candidates: DependencyCandidates,
  phase: SemanticMutationPhaseV1,
}

impl<'a> SemanticCatalogContinuationV1<'a> {
  pub fn start(
    request: SemanticCatalogCompilationRequestV1,
    registry: &'a CompiledParserRegistryV1,
    store: &mut dyn SemanticCatalogStagingStoreV1,
    memory: &'a MemoryCoordinator,
    is_cancelled: &'a dyn Fn() -> bool,
  ) -> Result<Self> {
    let mut compiler = Compiler::new(request, registry, memory, is_cancelled)?;
    compiler.registry(registry, store)?;
    compiler.check()?;
    Ok(Self { compiler, candidates: DependencyCandidates::default(), phase: SemanticMutationPhaseV1::Compiling })
  }

  pub fn from_complete(
    request: SemanticCatalogCompilationRequestV1,
    previous: &CompiledSemanticCatalogV1,
    registry: &'a CompiledParserRegistryV1,
    store: &dyn SemanticCatalogStagingStoreV1,
    memory: &'a MemoryCoordinator,
    is_cancelled: &'a dyn Fn() -> bool,
  ) -> Result<Self> {
    let mut compiler = Compiler::new(request, registry, memory, is_cancelled)?;
    compiler.inherit_complete(previous, registry, store)?;
    compiler.check()?;
    Ok(Self { compiler, candidates: DependencyCandidates::default(), phase: SemanticMutationPhaseV1::Compiling })
  }

  /// Consume catalog-only admission, rebind its exact registry, and admit a
  /// fresh lease to the supplied coordinator. No old lease is silently moved
  /// to a different budget. Restart still needs the enclosing source proof.
  pub fn from_progress(
    request: SemanticCatalogCompilationRequestV1,
    progress: AdmittedSemanticCatalogProgressV1,
    registry: &'a CompiledParserRegistryV1,
    store: &dyn SemanticCatalogStagingStoreV1,
    memory: &'a MemoryCoordinator,
    is_cancelled: &'a dyn Fn() -> bool,
  ) -> Result<Self> {
    cancelled(is_cancelled)?;
    progress.validate_continuation_request(request)?;
    let mut compiler = Compiler::new(request, registry, memory, is_cancelled)?;
    let catalog = progress.catalog();
    compiler.root = catalog.root_object_id.map(copy_bytes).transpose()?;
    compiler.records = catalog.record_count;
    compiler.nodes = catalog.node_count;
    compiler.dependencies = progress.dependency_count();
    compiler.configurations = progress.configuration_count();
    let snapshot = progress.pruning_candidates();
    let candidates = DependencyCandidates {
      root: snapshot.root_object_id.map(copy_bytes).transpose()?,
      records: snapshot.record_count,
      nodes: snapshot.node_count,
    };
    compiler.validate_captured_registry(registry, store)?;
    compiler.check()?;
    let phase = progress.phase();
    drop(progress);
    Ok(Self { compiler, candidates, phase })
  }

  pub const fn phase(&self) -> SemanticMutationPhaseV1 {
    self.phase
  }

  pub const fn configuration_count(&self) -> u64 {
    self.compiler.configurations
  }

  pub const fn dependency_count(&self) -> u64 {
    self.compiler.dependencies
  }

  pub fn catalog(&self) -> SemanticCatalogSnapshotV1<'_> {
    SemanticCatalogSnapshotV1 {
      root_object_id: self.compiler.root.as_deref(),
      record_count: self.compiler.records,
      node_count: self.compiler.nodes,
    }
  }

  pub fn pruning_candidates(&self) -> SemanticCatalogSnapshotV1<'_> {
    SemanticCatalogSnapshotV1 {
      root_object_id: self.candidates.root.as_deref(),
      record_count: self.candidates.records,
      node_count: self.candidates.nodes,
    }
  }

  pub fn apply(mut self, mutation: SemanticCatalogConfigurationMutationV1, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<Self> {
    self.require_phase(SemanticMutationPhaseV1::Compiling)?;
    self.compiler.apply_configuration_mutation(mutation, &mut self.candidates, store)?;
    self.compiler.check()?;
    Ok(self)
  }

  /// One complete live-dependency exclusion pass precedes Pruning. Until this
  /// returns, only the preceding Compiling checkpoint can be selected/resumed.
  pub fn finish_configurations(mut self, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<Self> {
    self.require_phase(SemanticMutationPhaseV1::Compiling)?;
    if self.compiler.configurations != self.compiler.request.expected_configuration_count {
      return Err(invalid("semantic_catalog_configuration_count", "final configuration count disagrees with capture"));
    }
    self.compiler.exclude_live_candidates(&mut self.candidates, store)?;
    self.compiler.check()?;
    self.phase = SemanticMutationPhaseV1::Pruning;
    Ok(self)
  }

  /// Remove one remaining dependency and its nomination. A failed second
  /// publication never returns the half-updated first root to its caller.
  pub fn prune_one(mut self, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<Self> {
    self.require_phase(SemanticMutationPhaseV1::Pruning)?;
    let root =
      self.candidates.root.as_deref().ok_or_else(|| invalid("semantic_catalog_pruning_empty", "no dependency remains to prune"))?;
    let reader = SemanticCatalogReaderV1::new(self.compiler.request.hash_algorithm, store);
    let (class, owner) = reader.with_first_record(
      root,
      SemanticCatalogTraversalBoundsV1::new(self.candidates.records, self.candidates.nodes)?,
      self.compiler.is_cancelled,
      |candidate| {
        self.compiler.check().map_err(catalog_check_error)?;
        if !matches!(candidate.record_kind, 6 | 7) || candidate.owner_key != candidate.semantic_id {
          return Err(catalog_check_error(corrupt("semantic_catalog_pruning_binding", "candidate is not a complete dependency binding")));
        }
        let catalog_root = self
          .compiler
          .root
          .as_deref()
          .ok_or_else(|| catalog_check_error(corrupt("semantic_catalog_root", "registry binding was removed")))?;
        let matches = reader.with_record(
          catalog_root,
          SemanticCatalogTraversalBoundsV1::new(self.compiler.records, self.compiler.nodes)?,
          candidate.record_kind,
          candidate.owner_key,
          self.compiler.is_cancelled,
          |record| Ok(record.semantic_id == candidate.semantic_id && record.definition_object_id == candidate.definition_object_id),
        )?;
        if matches != Some(true) {
          return Err(catalog_check_error(corrupt("semantic_catalog_pruning_binding", "candidate differs from its main catalog binding")));
        }
        Ok((candidate.record_kind, copy_bytes(candidate.owner_key).map_err(catalog_check_error)?))
      },
    )?;
    self.compiler.remove_binding(class, &owner, store)?;
    self.candidates.mutate(&self.compiler, SemanticCatalogMutationV1::Remove { record_kind: class, owner_key: &owner }, store)?;
    self.compiler.check()?;
    Ok(self)
  }

  pub fn finish(self, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<CompiledSemanticCatalogV1> {
    self.require_phase(SemanticMutationPhaseV1::Pruning)?;
    if self.candidates.root.is_some() {
      return Err(invalid("semantic_catalog_pruning_incomplete", "remaining dependency candidates prevent completion"));
    }
    self.compiler.finish(store)
  }

  fn require_phase(&self, expected: SemanticMutationPhaseV1) -> Result<()> {
    self.compiler.check()?;
    if self.phase != expected {
      return Err(invalid("semantic_catalog_continuation_phase", "operation is not valid in the current catalog phase"));
    }
    Ok(())
  }
}
