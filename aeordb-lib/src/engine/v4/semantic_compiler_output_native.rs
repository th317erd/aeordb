//! Completed-source admission is distinct from unfinished progress admission.
use super::*;
use crate::engine::v4::namespace::decode_semantic_object;

/// Read-only proof of complete output projections. It grants neither admission
/// of a NamespaceRoot nor permission to activate a task or release its pins.
pub(in crate::engine::v4::first_authority) struct NativeSemanticCompilerOutputV1<'a> {
  _capture: &'a NativeSemanticMutationInventoryV1<'a>,
  _compiled: CompiledSemanticCatalogV1,
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub(in crate::engine::v4::first_authority) fn admit_captured_semantic_compiler_output(
    &self,
    task_id: &[u8; 16],
    checkpoint_sequence: u64,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<NativeSemanticCompilerOutputV1<'_>> {
    let (_, compiled) = self.with_validated_captured_semantic_source_union(
      task_id,
      checkpoint_sequence,
      bounds.sources,
      |operation, base, checkpoint_bytes, _, base_count| operation.admit_completed_output(base, checkpoint_bytes, base_count, bounds),
      || {},
    )?;
    Ok(NativeSemanticCompilerOutputV1 { _capture: self, _compiled: compiled })
  }
}

impl<A: NamespaceReadAdmissionV1> RetainedSourceUnionOperationV1<'_, '_, '_, A> {
  fn admit_completed_output(
    &self,
    base: &NamespaceSemanticBindingV1,
    checkpoint_bytes: &[u8],
    base_count: u64,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<CompiledSemanticCatalogV1> {
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
    let result = (|| -> UnionResult<CompiledSemanticCatalogV1> {
      let algorithm = self.catalog.algorithm();
      let checkpoint =
        decode_semantic_mutation_checkpoint(checkpoint_bytes, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
      if checkpoint.phase != SemanticMutationPhaseV1::Ready {
        return Err(invalid("semantic_compiler_output_phase", "completed output admission requires Ready work").into());
      }
      let PreparedSemanticCompilerInputsV1 { request, registry, base_admission, base_registry, mode } =
        self.prepare_compiler_inputs(base, checkpoint.expected_configuration_count, base_count, bounds, &source)?;
      let state_id = checkpoint.semantic_state.ok_or_else(|| invalid("semantic_compiler_output_state", "Ready has no output state"))?;
      let compiled =
        admit_semantic_catalog_v1(request, state_id, &registry, &source, &capture.memory, &|| capture.cancellation.is_cancelled())?;
      let state = decode_semantic_object(&compiled.semantic_state().value, algorithm)
        .map_err(SemanticMutationObservationErrorV1::from)?
        .semantic_state
        .ok_or_else(|| invalid("semantic_compiler_output_state", "output is not a semantic state"))?;
      let SemanticAvailabilityV1::Complete {
        compiler_fingerprint,
        semantic_registry_fingerprint,
        catalog_root,
        catalog_record_count,
        catalog_node_count,
        dependency_count,
        ..
      } = &state.availability
      else {
        return Err(invalid("semantic_compiler_output_state", "output is not Complete").into());
      };
      if Some(catalog_root.as_slice()) != checkpoint.catalog_root
        || *catalog_record_count != checkpoint.record_count
        || *catalog_node_count != checkpoint.node_count
        || *dependency_count != checkpoint.dependency_count
        || compiler_fingerprint != checkpoint.compiler_fingerprint
        || semantic_registry_fingerprint != checkpoint.semantic_registry_fingerprint
      {
        return Err(invalid("semantic_compiler_output_binding", "completed state differs from its exact checkpoint").into());
      }
      let base_catalog = match &base.semantic_state.availability {
        SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, .. } if *catalog_record_count != 0 => {
          Some(SemanticCatalogSnapshotV1 {
            root_object_id: Some(catalog_root),
            record_count: *catalog_record_count,
            node_count: *catalog_node_count,
          })
        }
        _ => None,
      };
      self.verify_prefix(PrefixVerificationV1 {
        checkpoint: &checkpoint,
        base_tree: &base.root.namespace_tree_root,
        catalog: SemanticCatalogSnapshotV1 {
          root_object_id: checkpoint.catalog_root,
          record_count: checkpoint.record_count,
          node_count: checkpoint.node_count,
        },
        base_catalog,
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
      Ok(compiled)
    })();
    source.finish(result)
  }
}
