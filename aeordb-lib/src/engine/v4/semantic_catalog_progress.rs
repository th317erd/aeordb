//! Catalog-only admission of unfinished compiler work; no task resume permit.
use super::*;
use super::super::semantic_mutation_control::{SemanticMutationPhaseV1, decode_semantic_mutation_checkpoint};
use super::admission::{CatalogCandidateAdmissionV1, validate_catalog_closure};

/// Verified catalog counts and typed closure, including precisely accounted
/// candidate dependencies. This is not a completed semantic state, source-
/// cursor proof, staging pin, namespace admission or task ownership permit.
pub struct AdmittedSemanticCatalogProgressV1 {
  request: SemanticCatalogCompilationRequestV1,
  phase: SemanticMutationPhaseV1,
  catalog_root: Vec<u8>,
  records: u64,
  nodes: u64,
  dependencies: u64,
  configurations: u64,
  pruning_root: Option<Vec<u8>>,
  pruning_records: u64,
  pruning_nodes: u64,
  _memory: MemoryReservation,
}

impl AdmittedSemanticCatalogProgressV1 {
  pub(super) fn validate_continuation_request(&self, request: SemanticCatalogCompilationRequestV1) -> Result<()> {
    if request.hash_algorithm != self.request.hash_algorithm
      || request.expected_configuration_count != self.request.expected_configuration_count
      || request.required_capabilities != self.request.required_capabilities
    {
      return Err(invalid("semantic_catalog_progress_request", "continuation request differs from admitted catalog progress"));
    }
    self._memory.check_admission().map_err(|error| resource("semantic_catalog_memory", error.to_string()))
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.request.hash_algorithm
  }

  pub const fn phase(&self) -> SemanticMutationPhaseV1 {
    self.phase
  }

  pub const fn configuration_count(&self) -> u64 {
    self.configurations
  }

  pub const fn dependency_count(&self) -> u64 {
    self.dependencies
  }

  pub fn catalog(&self) -> SemanticCatalogSnapshotV1<'_> {
    SemanticCatalogSnapshotV1 { root_object_id: Some(&self.catalog_root), record_count: self.records, node_count: self.nodes }
  }

  pub fn pruning_candidates(&self) -> SemanticCatalogSnapshotV1<'_> {
    SemanticCatalogSnapshotV1 {
      root_object_id: self.pruning_root.as_deref(),
      record_count: self.pruning_records,
      node_count: self.pruning_nodes,
    }
  }
}

/// Inspect one exact ASMC catalog snapshot using the existing checkpoint
/// decoder and compiler graph rules. The caller owns bounded source reads and
/// retained physical protection. Source/cursor/fence checks remain separate.
pub fn admit_semantic_catalog_progress_v1(
  request: SemanticCatalogCompilationRequestV1,
  checkpoint_bytes: &[u8],
  registry: &CompiledParserRegistryV1,
  source: &dyn SemanticCatalogObjectSourceV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<AdmittedSemanticCatalogProgressV1> {
  let mut compiler = Compiler::new(request, registry, memory, is_cancelled)?;
  let checkpoint = decode_semantic_mutation_checkpoint(checkpoint_bytes, request.hash_algorithm).map_err(format_error)?;
  compiler.check()?;
  if !matches!(checkpoint.phase, SemanticMutationPhaseV1::Compiling | SemanticMutationPhaseV1::Pruning) {
    return Err(invalid("semantic_catalog_progress_phase", "catalog progress admission requires compiling or pruning work"));
  }
  if checkpoint.compiler_fingerprint != semantic_compiler_fingerprint_v1(request.hash_algorithm)
    || checkpoint.semantic_registry_fingerprint
      != embedded_system_family_registry(request.hash_algorithm).map_err(format_error)?.semantic_projection_fingerprint
  {
    return Err(invalid("semantic_catalog_base_profile", "checkpoint compiler or semantic registry profile is not supported"));
  }
  if checkpoint.expected_configuration_count != request.expected_configuration_count {
    return Err(invalid("semantic_catalog_progress_configuration_count", "checkpoint final count differs from the captured request"));
  }
  if checkpoint.phase == SemanticMutationPhaseV1::Pruning && checkpoint.configuration_count != checkpoint.expected_configuration_count {
    return Err(corrupt("semantic_catalog_progress_configuration_count", "pruning began before the final configuration count"));
  }
  if checkpoint.catalog_root.is_none() {
    return Err(invalid("semantic_catalog_progress_empty", "post-registry progress requires a real catalog"));
  }
  compiler.validate_definition(2, registry.projection())?;
  compiler.root = checkpoint.catalog_root.map(copy_bytes).transpose()?;
  compiler.records = checkpoint.record_count;
  compiler.nodes = checkpoint.node_count;
  compiler.dependencies = checkpoint.dependency_count;
  compiler.configurations = checkpoint.configuration_count;
  validate_catalog_closure(
    &mut compiler,
    registry,
    source,
    Some(CatalogCandidateAdmissionV1 {
      snapshot: SemanticCatalogSnapshotV1 {
        root_object_id: checkpoint.pruning_catalog_root,
        record_count: checkpoint.pruning_record_count,
        node_count: checkpoint.pruning_node_count,
      },
      require_unused: checkpoint.phase == SemanticMutationPhaseV1::Pruning,
    }),
  )?;
  let pruning_root = checkpoint.pruning_catalog_root.map(copy_bytes).transpose()?;
  compiler.check()?;
  Ok(AdmittedSemanticCatalogProgressV1 {
    request,
    phase: checkpoint.phase,
    catalog_root: compiler.root.ok_or_else(|| corrupt("semantic_catalog_root", "validated catalog root disappeared"))?,
    records: compiler.records,
    nodes: compiler.nodes,
    dependencies: compiler.dependencies,
    configurations: compiler.configurations,
    pruning_root,
    pruning_records: checkpoint.pruning_record_count,
    pruning_nodes: checkpoint.pruning_node_count,
    _memory: compiler.reservation,
  })
}
