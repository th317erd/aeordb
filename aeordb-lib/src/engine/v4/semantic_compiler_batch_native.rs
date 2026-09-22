//! One admitted retained-source operation per bounded configuration/pruning batch.
use super::*;
use crate::engine::v4::first_authority::NativeSemanticTaskCompilerAdvanceRequestV1;
use crate::engine::v4::namespace::{EncodedNamespaceRootV1, NamespaceRootWriteV1, encode_namespace_root};
use crate::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogConfigurationMutationV1, SemanticCatalogContinuationV1, SemanticCatalogStagingStoreV1,
};
use crate::engine::v4::semantic_catalog_native::NativeSemanticCatalogStagingStoreV1;
use crate::engine::v4::semantic_mutation_control::encode_semantic_mutation_checkpoint;

/// Derived, unselected work. The enclosing task owner retains publication scratch
/// and must guard candidate/pair publication and freshly admit the selected graph.
pub(in crate::engine::v4::first_authority) struct NativeSemanticCompilerBatchV1 {
  pub(in crate::engine::v4::first_authority) checkpoint: Vec<u8>,
  pub(in crate::engine::v4::first_authority) companion: Vec<u8>,
  pub(in crate::engine::v4::first_authority) candidate: Option<EncodedNamespaceRootV1>,
  pub(in crate::engine::v4::first_authority) phase: SemanticMutationPhaseV1,
  pub(in crate::engine::v4::first_authority) configuration_steps: u64,
  pub(in crate::engine::v4::first_authority) pruning_steps: u64,
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub(in crate::engine::v4::first_authority) fn advance_captured_semantic_compiler(
    &self,
    identity: (&[u8; 16], u64, u64, &[u8]),
    request: NativeSemanticTaskCompilerAdvanceRequestV1,
    before_writes: impl FnOnce() -> UnionResult<()>,
  ) -> UnionResult<NativeSemanticCompilerBatchV1> {
    let (task_id, checkpoint_sequence, reserved_sequence, expected_checkpoint) = identity;
    let (_, batch) = self.with_validated_captured_semantic_source_union(
      task_id,
      checkpoint_sequence,
      request.compiler_bounds.sources,
      |operation, base, checkpoint_bytes, companion, base_count| {
        if checkpoint_bytes != expected_checkpoint {
          return Err(invalid("semantic_task_work_checkpoint", "compiler continuation requires the exact selected checkpoint").into());
        }
        let algorithm = operation.catalog.algorithm();
        let checkpoint =
          decode_semantic_mutation_checkpoint(checkpoint_bytes, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
        let (compilation, registry, progress, _) = operation.admit_prefix(base, checkpoint_bytes, base_count, request.compiler_bounds)?;
        before_writes()?;
        let mut store = NativeSemanticCatalogStagingStoreV1::new(
          self._protection,
          self.header.selected.header.database_id,
          request.publication_timestamp_ms,
          &self.cancellation,
        )?;
        let cancelled = || self.cancellation.is_cancelled();
        let mut continuation =
          SemanticCatalogContinuationV1::from_progress(compilation, progress, &registry, &store, &self.memory, &cancelled)?;
        let mut configuration_steps = 0u64;
        let mut pruning_steps = 0u64;
        let mut last_owner = match checkpoint.cursor {
          SemanticMutationCursorV1::ConfigurationOwner(owner) => Some(copy_union_path(owner)?),
          _ => None,
        };
        if continuation.phase() == SemanticMutationPhaseV1::Compiling {
          // Global absence is a real source position. Counts alone cannot establish
          // exhaustion, especially for deletions and absent global configuration.
          if last_owner.is_none() {
            let source = operation.selected(operation.manifest.requested_source_catalog, GLOBAL_INDEXES)?;
            continuation = operation.apply_requested_configuration(
              continuation,
              &registry,
              &mut store,
              PrefixConfigurationSourceV1 {
                catalog_root: operation.manifest.requested_source_catalog,
                body: source.as_ref().map(NativeProtectedSemanticSourceV1::body),
                owner: "/",
                maximum_source_bytes: operation.bounds.catalog.maximum_source_bytes,
              },
              request.compiler_bounds,
            )?;
            last_owner = Some(copy_union_path("/")?);
            configuration_steps += 1;
          }
          let saved_path = match checkpoint.cursor {
            SemanticMutationCursorV1::ConfigurationOwner(owner) if owner != "/" => Some(configuration_owner(owner)?),
            _ => None,
          };
          let mut exhausted = true;
          let mut namespaces = NamespaceSourcePairCursorV1::new(
            operation.namespace,
            &base.root.namespace_tree_root,
            operation.manifest.staged_directory_root,
          )?;
          while let Some(pair) = namespaces.next_pair(operation.namespace)? {
            if saved_path.as_ref().is_some_and(|path| pair.path.as_bytes() <= &path[2..]) {
              continue;
            }
            // One lookahead proves more input exists; do not compile it or advance
            // the saved owner when the batch has reached its exact step ceiling.
            if configuration_steps == request.maximum_configuration_steps {
              exhausted = false;
              break;
            }
            let owner = pair
              .path
              .strip_suffix("/.aeordb-config/indexes.json")
              .filter(|owner| !owner.is_empty())
              .ok_or_else(|| invalid("semantic_compiler_prefix_owner", "namespace configuration has no nonroot owner"))?;
            continuation = operation.apply_requested_configuration(
              continuation,
              &registry,
              &mut store,
              PrefixConfigurationSourceV1 {
                catalog_root: operation.manifest.requested_source_catalog,
                body: pair.requested.as_ref().map(NativeSemanticNamespaceSourceV1::body),
                owner,
                maximum_source_bytes: operation.bounds.namespace.sources.maximum_body_bytes,
              },
              request.compiler_bounds,
            )?;
            last_owner = Some(copy_union_path(owner)?);
            configuration_steps = increment_count(configuration_steps)?;
          }
          drop(namespaces);
          if exhausted {
            continuation = continuation.finish_configurations(&mut store)?;
            last_owner = None;
          }
        }
        while continuation.phase() == SemanticMutationPhaseV1::Pruning
          && continuation.pruning_candidates().root_object_id.is_some()
          && pruning_steps < request.maximum_pruning_steps
        {
          continuation = continuation.prune_one(&mut store)?;
          pruning_steps = increment_count(pruning_steps)?;
        }
        let catalog = continuation.catalog();
        let catalog_root = catalog.root_object_id.map(copy_namespace_bytes).transpose()?;
        let (records, nodes) = (catalog.record_count, catalog.node_count);
        let pruning = continuation.pruning_candidates();
        let pruning_root = pruning.root_object_id.map(copy_namespace_bytes).transpose()?;
        let (pruning_records, pruning_nodes) = (pruning.record_count, pruning.node_count);
        let configurations = continuation.configuration_count();
        let dependencies = continuation.dependency_count();
        let mut phase = continuation.phase();
        let completed = if phase == SemanticMutationPhaseV1::Pruning && pruning_root.is_none() {
          phase = SemanticMutationPhaseV1::Ready;
          Some(continuation.finish(&mut store)?)
        } else {
          drop(continuation);
          None
        };
        let candidate = completed
          .as_ref()
          .map(|completed| {
            encode_namespace_root(
              &NamespaceRootWriteV1 {
                required_capabilities: base.root.required_capabilities,
                namespace_tree_root: copy_namespace_bytes(operation.manifest.staged_directory_root)?,
                semantic_state_root: copy_namespace_bytes(&completed.semantic_state().object_id)?,
              },
              algorithm,
            )
            .map_err(SemanticMutationObservationErrorV1::from)
          })
          .transpose()?;
        let checkpoint = SemanticMutationCheckpointV1 {
          checkpoint_sequence: reserved_sequence,
          phase,
          cursor: match last_owner.as_deref() {
            Some(owner) => SemanticMutationCursorV1::ConfigurationOwner(owner),
            None => SemanticMutationCursorV1::None,
          },
          configuration_count: configurations,
          dependency_count: dependencies,
          catalog_root: catalog_root.as_deref(),
          record_count: records,
          node_count: nodes,
          pruning_catalog_root: pruning_root.as_deref(),
          pruning_record_count: pruning_records,
          pruning_node_count: pruning_nodes,
          semantic_state: completed.as_ref().map(|result| result.semantic_state().object_id.as_slice()),
          candidate_namespace_root: candidate.as_ref().map(|root| root.root_hash.as_slice()),
          ..checkpoint
        };
        let encoded = encode_semantic_mutation_checkpoint(&checkpoint, algorithm).map_err(SemanticMutationObservationErrorV1::from)?;
        operation.catalog.check()?;
        operation.namespace.check()?;
        Ok(NativeSemanticCompilerBatchV1 {
          checkpoint: encoded,
          companion: copy_namespace_bytes(companion)?,
          candidate,
          phase,
          configuration_steps,
          pruning_steps,
        })
      },
      || {},
    )?;
    Ok(batch)
  }
}

impl<A: NamespaceReadAdmissionV1> RetainedSourceUnionOperationV1<'_, '_, '_, A> {
  fn apply_requested_configuration<'compiler>(
    &self,
    continuation: SemanticCatalogContinuationV1<'compiler>,
    registry: &CompiledParserRegistryV1,
    store: &mut dyn SemanticCatalogStagingStoreV1,
    source: PrefixConfigurationSourceV1<'_>,
    bounds: NativeSemanticCompilerProgressBoundsV1,
  ) -> UnionResult<SemanticCatalogContinuationV1<'compiler>> {
    let owner = source.owner;
    let mutation = match self.compile_configuration(source, registry, bounds)? {
      Some(configuration) => SemanticCatalogConfigurationMutationV1::Upsert(configuration),
      None => SemanticCatalogConfigurationMutationV1::Remove(copy_union_path(owner)?),
    };
    self.namespace.lookup.charge_work(1).map_err(map_namespace_read_error)?;
    Ok(continuation.apply(mutation, store)?)
  }
}
