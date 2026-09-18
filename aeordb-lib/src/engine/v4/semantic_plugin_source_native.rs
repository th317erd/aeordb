//! Exact captured source identity, never corrected executor availability.
use super::*;
#[path = "semantic_alias_snapshot_native.rs"]
mod prepared;
pub use prepared::{NativeSemanticAliasSnapshotRequestV1, NativeSemanticAliasSnapshotV1};
use crate::engine::v4::dependency::{decode_dependency_record_bytes, encode_dependency_record, DependencyRecordV1};
use crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use crate::engine::v4::plugin_artifact_identity::{
  inspect_plugin_artifact_identity_v1, plugin_artifact_path_v1, PluginArtifactIdentityRequestV1,
  WORKSPACE_BYTES as IDENTITY_WORKSPACE_BYTES,
};
use crate::engine::v4::plugin_identity::{decode_plugin_alias_v1, plugin_alias_path_v1, ALIAS_MAX_LENGTH};
use crate::engine::v4::reader::FormatError;
use crate::engine::v4::semantic_source_capture::SemanticSourceAliasRoleV1;

// Two bounded dependency records (at most8896 bytes), two short paths and
// result/encoding metadata. Source bodies are independently charged by their
// existing reader; the inspector's temporary workspace is admitted separately.
const PAIR_WORKSPACE_BYTES: usize = 32 << 10;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticPluginSourceBoundsV1 {
  pub maximum_module_bytes: usize,
  pub maximum_chunk_entity_bytes: usize,
  pub maximum_source_chunks: u64,
  pub maximum_read_bytes: u64,
  pub maximum_workspace_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeSemanticPluginSourceErrorV1 {
  #[error(transparent)]
  Source(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Identity(#[from] SemanticCompilationErrorV1),
}

/// Original source values retain the captured KV and live staging protection.
/// Role records name the required executor; they do not admit its execution.
pub struct NativeSemanticPluginSourcesV1<'a> {
  alias: NativeProtectedSemanticSourceV1<'a>,
  artifact: NativeProtectedSemanticSourceV1<'a>,
  parser: Option<Vec<u8>>,
  mapper: Option<Vec<u8>>,
  _memory: MemoryReservation,
}

impl<'a> NativeSemanticPluginSourcesV1<'a> {
  pub fn alias_source(&self) -> &NativeProtectedSemanticSourceV1<'a> {
    &self.alias
  }
  pub fn artifact_source(&self) -> &NativeProtectedSemanticSourceV1<'a> {
    &self.artifact
  }
  pub fn dependency_bytes(&self, role: SemanticSourceAliasRoleV1) -> Option<&[u8]> {
    match role {
      SemanticSourceAliasRoleV1::Parser => self.parser.as_deref(),
      SemanticSourceAliasRoleV1::Mapper => self.mapper.as_deref(),
    }
  }
  pub fn dependency_record(&self, role: SemanticSourceAliasRoleV1) -> Result<Option<DependencyRecordV1<'_>>, FormatError> {
    self.dependency_bytes(role).map(decode_dependency_record_bytes).transpose()
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub fn read_protected_plugin_sources(
    &self,
    alias: &str,
    bounds: NativeSemanticPluginSourceBoundsV1,
  ) -> Result<Option<NativeSemanticPluginSourcesV1<'_>>, NativeSemanticPluginSourceErrorV1> {
    self.read_protected_plugin_sources_with_observer(alias, bounds, || {})
  }

  pub(crate) fn read_protected_plugin_sources_with_observer(
    &self,
    alias: &str,
    bounds: NativeSemanticPluginSourceBoundsV1,
    before_complete: impl FnOnce(),
  ) -> Result<Option<NativeSemanticPluginSourcesV1<'_>>, NativeSemanticPluginSourceErrorV1> {
    let lookup = self.source_lookup(plugin_source_read_bounds(bounds));
    self.read_protected_plugin_sources_from_lookup(alias, bounds, &lookup, before_complete)
  }

  fn read_protected_plugin_sources_from_lookup(
    &self,
    alias: &str,
    bounds: NativeSemanticPluginSourceBoundsV1,
    lookup: &impl FirstAuthorityEntityLookupV1,
    before_complete: impl FnOnce(),
  ) -> Result<Option<NativeSemanticPluginSourcesV1<'_>>, NativeSemanticPluginSourceErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    validate_plugin_source_bounds(bounds)?;
    let memory = self
      .memory
      .reserve(MemoryOwner::Task, PAIR_WORKSPACE_BYTES as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let alias_path = plugin_alias_path_v1(alias).map_err(SemanticMutationObservationErrorV1::from)?;
    let source_bounds = plugin_source_read_bounds(bounds);
    // One captured lookup and one cumulative physical-read counter for both
    // sources. Per-source chunk ceilings never reset the paired byte budget.
    let Some(alias_source) = self.read_source_from_lookup(
      &alias_path,
      None,
      NativeSemanticSourceReadBoundsV1 { maximum_body_bytes: ALIAS_MAX_LENGTH, ..source_bounds },
      lookup,
      || {},
    )?
    else {
      before_complete();
      check_cancelled(&self.cancellation)?;
      memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
      return Ok(None);
    };
    let alias_record = decode_plugin_alias_v1(alias_source.body(), &alias_path).map_err(SemanticMutationObservationErrorV1::from)?;
    if alias_record.artifact_length > bounds.maximum_module_bytes as u64 {
      return Err(resource("semantic_plugin_source_module_bound", "declared plugin module exceeds its body limit").into());
    }
    let artifact_path = plugin_artifact_path_v1(alias_record.artifact_fingerprint).map_err(SemanticMutationObservationErrorV1::from)?;
    let artifact = self
      .read_source_from_lookup(&artifact_path, None, source_bounds, lookup, || {})?
      .ok_or_else(|| invalid("semantic_plugin_source_module_missing", "captured plugin alias references an absent raw module"))?;
    let identity = inspect_plugin_artifact_identity_v1(
      PluginArtifactIdentityRequestV1 {
        alias_bytes: alias_source.body(),
        alias_path: &alias_path,
        artifact_path: &artifact_path,
        module_bytes: artifact.body(),
        maximum_module_bytes: bounds.maximum_module_bytes,
        maximum_workspace_bytes: bounds.maximum_workspace_bytes - PAIR_WORKSPACE_BYTES,
      },
      &self.memory,
      &|| self.cancellation.is_cancelled(),
    )?;
    let mut parser = None;
    let mut mapper = None;
    for role in identity.manifest().roles() {
      check_cancelled(&self.cancellation)?;
      memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
      let encoded = encode_dependency_record(&DependencyRecordV1 {
        kind: 1,
        role: role.role,
        flags: 4,
        abi: role.abi,
        executor_profile: 2,
        fingerprint_semantics: 1,
        artifact_kind: 1,
        artifact_length: identity.alias().artifact_length,
        fingerprint: *identity.alias().artifact_fingerprint,
        dependency_id: identity.manifest().plugin_id,
        version: identity.manifest().version,
      })
      .map_err(SemanticMutationObservationErrorV1::from)?;
      match role.role {
        1 => parser = Some(encoded),
        2 => mapper = Some(encoded),
        _ => return Err(invalid("semantic_plugin_source_role", "validated manifest contains an unknown dependency role").into()),
      }
    }
    drop(identity);
    before_complete();
    check_cancelled(&self.cancellation)?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(Some(NativeSemanticPluginSourcesV1 { alias: alias_source, artifact, parser, mapper, _memory: memory }))
  }
}

fn plugin_source_read_bounds(bounds: NativeSemanticPluginSourceBoundsV1) -> NativeSemanticSourceReadBoundsV1 {
  NativeSemanticSourceReadBoundsV1 {
    maximum_body_bytes: bounds.maximum_module_bytes,
    maximum_chunk_entity_bytes: bounds.maximum_chunk_entity_bytes,
    maximum_chunks: bounds.maximum_source_chunks,
    maximum_read_bytes: bounds.maximum_read_bytes,
  }
}

fn validate_plugin_source_bounds(bounds: NativeSemanticPluginSourceBoundsV1) -> Result<(), NativeSemanticPluginSourceErrorV1> {
  if !(1..=MAXIMUM_SOURCE_BODY_BYTES).contains(&bounds.maximum_module_bytes)
    || !(1..=MAXIMUM_CHUNK_ENTITY_BYTES).contains(&bounds.maximum_chunk_entity_bytes)
    || bounds.maximum_source_chunks == 0
    || bounds.maximum_read_bytes == 0
  {
    return Err(invalid("semantic_plugin_source_bounds", "captured plugin pair requires valid bounded work and byte limits").into());
  }
  if bounds.maximum_workspace_bytes < PAIR_WORKSPACE_BYTES + IDENTITY_WORKSPACE_BYTES {
    return Err(resource("semantic_plugin_source_workspace", "captured plugin pair exceeds its workspace limit").into());
  }
  Ok(())
}
