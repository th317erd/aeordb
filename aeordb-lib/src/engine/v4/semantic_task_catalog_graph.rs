//! Captured catalog/definition/archive retention; no executor admission.
use super::*;
use crate::engine::v4::dependency::decode_dependency_record_bytes;
use crate::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateV1, validated_semantic_identity};
use crate::engine::v4::plugin_artifact_identity::plugin_artifact_path_v1;
use crate::engine::v4::read_view_native::{SelectedSemanticExpectedCountsV1, validate_selected_semantic_walk};
use crate::engine::v4::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1, SemanticCatalogWalkStatsV1,
  validate_semantic_definition_identity_v1,
};

struct CapturedCatalogSource<'operation, 'capture, 'visitor> {
  operation: &'operation GraphOperation<'capture, 'visitor>,
  failure: RefCell<Option<SemanticTaskGraphErrorV1>>,
}

impl CapturedCatalogSource<'_, '_, '_> {
  fn bridge<T>(&self, result: Result<T>) -> std::result::Result<T, SemanticCatalogReadErrorV1> {
    match result {
      Ok(value) => Ok(value),
      Err(original) => {
        if self.failure.borrow().is_none() {
          *self.failure.borrow_mut() = Some(original);
        }
        Err(SemanticCatalogReadErrorV1::unavailable("semantic_task_graph_catalog_read", "captured catalog read failed"))
      }
    }
  }
}

impl SemanticCatalogObjectSourceV1 for CapturedCatalogSource<'_, '_, '_> {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> std::result::Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    self.bridge(self.operation.semantic_object(kind_id, object_id))
  }
}

impl GraphOperation<'_, '_> {
  pub(super) fn semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>> {
    self.check()?;
    let path = semantic_object_path(self.algorithm(), kind, identity)?;
    let cap = crate::engine::v4::semantic_store::semantic_object_cap(kind)?;
    let Some(loaded) =
      load_canonical_system_file_at_path(self.file(), &self.lookup, self.header(), &path, SEMANTIC_OBJECT_CONTENT_TYPE, cap)?
    else {
      return Ok(None);
    };
    self.check()?;
    let object = decode_semantic_object(&loaded.body, self.algorithm())?;
    if object.kind_id != kind || object.object_id != identity {
      return Err(invalid("semantic_task_graph_semantic_identity", "semantic object differs from its canonical captured path").into());
    }
    Ok(Some(loaded.body))
  }

  pub(super) fn walk_catalog(&self, root: &[u8], records: u64, nodes: u64) -> Result<SemanticCatalogWalkStatsV1> {
    self.check()?;
    let source = CapturedCatalogSource { operation: self, failure: RefCell::new(None) };
    let reader = SemanticCatalogReaderV1::new(self.algorithm(), &source);
    let bounds = SemanticCatalogTraversalBoundsV1::new(records, nodes)?;
    let cancelled = || self.capture.cancellation.is_cancelled();
    let result = reader.walk_catalog(root, bounds, &cancelled, |record| {
      source.bridge(self.lookup.step(1).map_err(Into::into))?;
      reader.with_definition(record, &cancelled, |definition| {
        // Share the existing class-specific validator with the byte writer;
        // no second projection, scope, field or dependency decoder is added.
        let actual = source.bridge(validated_semantic_identity(record.record_kind, definition, self.algorithm()).map_err(Into::into))?;
        if actual != record.semantic_id {
          return Err(SemanticCatalogReadErrorV1::corrupt(
            "semantic_definition_identity",
            "definition payload differs from its catalog semantic identity",
          ));
        }
        if record.record_kind >= 3 {
          validate_semantic_definition_identity_v1(record, &actual)?;
        }
        if record.record_kind == 6 {
          source.bridge(self.archive_dependency(definition))?;
        }
        source.bridge(self.check())
      })
    });
    match source.failure.into_inner() {
      Some(original) => Err(original),
      None => result.map_err(Into::into),
    }
  }

  pub(super) fn walk_state(&self, state: &SemanticStateV1) -> Result<()> {
    if let SemanticAvailabilityV1::Complete {
      catalog_root,
      catalog_record_count,
      catalog_node_count,
      definition_count,
      dependency_count,
      ..
    } = &state.availability
    {
      // Complete-empty states have no catalog edge and all counts are zero;
      // this relation has already been checked by the shared state decoder.
      if !catalog_root.iter().all(|byte| *byte == 0) {
        let stats = self.walk_catalog(catalog_root, *catalog_record_count, *catalog_node_count)?;
        validate_selected_semantic_walk(
          stats,
          SelectedSemanticExpectedCountsV1 {
            records: *catalog_record_count,
            nodes: *catalog_node_count,
            definitions: *definition_count,
            dependencies: *dependency_count,
          },
        )?;
      }
    }
    self.check()
  }

  pub(super) fn validate_output(&self, checkpoint: &SemanticMutationCheckpointV1<'_>, state: &SemanticStateV1) -> Result<()> {
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
      return Err(invalid("semantic_task_graph_output_incomplete", "task output cannot be content-only").into());
    };
    if Some(catalog_root.as_slice()) != checkpoint.catalog_root
      || *catalog_record_count != checkpoint.record_count
      || *catalog_node_count != checkpoint.node_count
      || *dependency_count != checkpoint.dependency_count
      || compiler_fingerprint != checkpoint.compiler_fingerprint
      || semantic_registry_fingerprint != checkpoint.semantic_registry_fingerprint
    {
      return Err(invalid("semantic_task_graph_output_binding", "task output state differs from its selected checkpoint").into());
    }
    Ok(())
  }

  fn archive_dependency(&self, definition: &[u8]) -> Result<()> {
    let dependency = decode_dependency_record_bytes(definition)?;
    let path = plugin_artifact_path_v1(&dependency.fingerprint)?;
    let bounds = NativeSemanticSourceReadBoundsV1 {
      maximum_body_bytes: self.bounds.sources.maximum_source_bytes,
      maximum_chunk_entity_bytes: self.bounds.sources.maximum_chunk_entity_bytes,
      maximum_chunks: self.bounds.sources.maximum_source_chunks,
      maximum_read_bytes: self.bounds.maximum_read_bytes,
    };
    let source = self
      .capture
      .read_source_from_lookup(&path, None, bounds, &self.lookup, || {})?
      .ok_or_else(|| invalid("semantic_task_graph_archive_missing", "retained executable dependency archive is absent"))?;
    if source.body().len() as u64 != dependency.artifact_length {
      return Err(invalid("semantic_task_graph_archive_identity", "retained executable archive has another length").into());
    }
    let mut fingerprint = blake3::Hasher::new();
    for chunk in source.body().chunks(64 << 10) {
      self.check()?;
      fingerprint.update(chunk);
    }
    if fingerprint.finalize().as_bytes() != &dependency.fingerprint {
      return Err(invalid("semantic_task_graph_archive_identity", "retained executable archive has another fingerprint").into());
    }
    // Archive retention does not require a mutable alias, today's executor,
    // corrected plugin manifest, or executable bytecode admission.
    self.check()
  }
}
