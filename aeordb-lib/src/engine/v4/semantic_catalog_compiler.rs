//! Bounded composition and incremental updates, without HEAD selection.
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::admission::BinaryCapabilityProfileV1;
use super::index_configuration_compiler::CompiledIndexConfigurationV1;
use super::namespace::{
  EncodedSemanticDefinitionObjectV1, EncodedSemanticObjectV1, SemanticAvailabilityV1, SemanticCatalogRecordV1, SemanticStateWriteV1,
  decode_semantic_definition_record, decode_semantic_object, encode_semantic_definition_object, encode_semantic_state_object,
};
use super::parser_registry_compiler::{CompiledParserRegistryV1, SemanticCompilationErrorV1};
use super::reader::{FormatError, MalformedInputClass};
use super::scope::decode_scope_definition;
use super::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1,
};
use super::semantic_catalog_mutation::{
  SemanticCatalogMutationPlanV1, SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1,
  plan_semantic_catalog_mutation_v1, semantic_catalog_mutation_workspace_bytes_v1,
};
use super::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
use super::system_family::embedded_system_family_registry;

// Includes one definition wrapper/read-back, bounded traversal stack, control
// key, result and staging scratch. The COW planner and each supplied compiled
// configuration retain their separate leases. Never retain a whole-set map.
const STAGING_WORKSPACE_BYTES: usize = 32 << 20;
const STAGING_BATCH_BYTES: usize = 8 << 20;
type Result<T> = std::result::Result<T, SemanticCatalogCompilationErrorV1>;

#[derive(Debug, thiserror::Error)]
pub enum SemanticCatalogCompilationErrorV1 {
  #[error(transparent)]
  Input(#[from] SemanticCompilationErrorV1),
  #[error(transparent)]
  Catalog(#[from] SemanticCatalogReadErrorV1),
  #[error("{code}: {message}")]
  InvalidInput { code: &'static str, message: String },
}

/// Stores only immutable, unselected objects through the existing publication
/// owner. Publication must durably finish before returning. Reads must validate
/// identity and enforce kind-specific caps before allocation; this compiler's
/// after-read size checks cannot repair an unbounded source implementation.
/// The owner retains staging roots/pins until activation or safe discard.
pub trait SemanticCatalogStagingStoreV1: SemanticCatalogObjectSourceV1 {
  fn publish_semantic_objects(&mut self, objects: &[EncodedSemanticObjectV1]) -> std::result::Result<(), SemanticCatalogReadErrorV1>;
}

#[derive(Clone, Copy, Debug)]
pub struct SemanticCatalogCompilationRequestV1 {
  pub hash_algorithm: HashAlgorithm,
  pub expected_configuration_count: u64,
  pub required_capabilities: [u8; 32],
  pub maximum_workspace_bytes: usize,
}

pub struct CompiledSemanticCatalogV1 {
  semantic_state: EncodedSemanticObjectV1,
  hash_algorithm: HashAlgorithm,
  // In-memory proof metadata, never a second persisted encoding. Preserve the
  // already-owned root so an incremental update need not decode its own state.
  catalog_root: Option<Vec<u8>>,
  record_count: u64,
  node_count: u64,
  dependency_count: u64,
  configuration_count: u64,
  _memory: MemoryReservation,
}

impl CompiledSemanticCatalogV1 {
  pub fn semantic_state(&self) -> &EncodedSemanticObjectV1 {
    &self.semantic_state
  }

  pub const fn configuration_count(&self) -> u64 {
    self.configuration_count
  }
}

/// Compose a new catalog from an exact counted stream, one configuration at a
/// time. Its owner must compile every item against the supplied captured
/// registry and deployment snapshot. Existing catalogs are not incrementally
/// modified by this function. Errors leave only unselected immutable objects;
/// no completed result or namespace authority is returned on partial failure.
pub fn compile_semantic_catalog_v1(
  request: SemanticCatalogCompilationRequestV1,
  registry: &CompiledParserRegistryV1,
  configurations: impl IntoIterator<Item = std::result::Result<CompiledIndexConfigurationV1, SemanticCompilationErrorV1>>,
  store: &mut dyn SemanticCatalogStagingStoreV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledSemanticCatalogV1> {
  let mut compiler = Compiler::new(request, registry, memory, is_cancelled)?;
  compiler.registry(registry, store)?;
  let mut configurations = configurations.into_iter();
  loop {
    compiler.check()?;
    let next = configurations.next();
    compiler.check()?;
    let Some(configuration) = next else { break };
    let configuration = configuration?;
    if compiler.configurations >= request.expected_configuration_count {
      return Err(invalid("semantic_catalog_configuration_count", "source enumerated more configurations than captured"));
    }
    compiler.configuration(&configuration, store)?;
    compiler.configurations += 1;
    // The compiled input drops before requesting the next item.
  }
  if compiler.configurations != request.expected_configuration_count {
    return Err(invalid("semantic_catalog_configuration_count", "source ended before its captured configuration count"));
  }
  compiler.validate_closure(store)?;
  compiler.finish(store)
}

struct Compiler<'a> {
  request: SemanticCatalogCompilationRequestV1,
  memory: &'a MemoryCoordinator,
  reservation: MemoryReservation,
  is_cancelled: &'a dyn Fn() -> bool,
  registry_projection_id: &'a [u8],
  root: Option<Vec<u8>>,
  records: u64,
  nodes: u64,
  dependencies: u64,
  configurations: u64,
}

impl<'a> Compiler<'a> {
  fn new(
    request: SemanticCatalogCompilationRequestV1,
    registry: &'a CompiledParserRegistryV1,
    memory: &'a MemoryCoordinator,
    is_cancelled: &'a dyn Fn() -> bool,
  ) -> Result<Self> {
    cancelled(is_cancelled)?;
    let required = semantic_catalog_mutation_workspace_bytes_v1(request.hash_algorithm)?
      .checked_add(STAGING_WORKSPACE_BYTES)
      .ok_or_else(|| resource("semantic_catalog_workspace", "combined workspace overflow"))?;
    if required > request.maximum_workspace_bytes {
      return Err(resource("semantic_catalog_workspace", "staging and COW workspace exceed the caller's limit"));
    }
    let supported = BinaryCapabilityProfileV1::current().supported_reader_capabilities.into_bytes();
    if request.required_capabilities.iter().zip(supported).any(|(required, supported)| required & !supported != 0) {
      return Err(invalid("semantic_catalog_capabilities", "new compilation cannot require unsupported reader capabilities"));
    }
    let reservation = memory
      .reserve(MemoryOwner::Task, STAGING_WORKSPACE_BYTES as u64, AdmissionClass::Workload)
      .map_err(|error| resource("semantic_catalog_memory", error.to_string()))?;
    let compiler = Self {
      request,
      memory,
      reservation,
      is_cancelled,
      registry_projection_id: &registry.projection().semantic_id,
      root: None,
      records: 0,
      nodes: 0,
      dependencies: 0,
      configurations: 0,
    };
    compiler.check()?;
    Ok(compiler)
  }
  fn check(&self) -> Result<()> {
    cancelled(self.is_cancelled)?;
    self.reservation.check_admission().map_err(|error| resource("semantic_catalog_memory", error.to_string()))
  }

  fn registry(&mut self, registry: &CompiledParserRegistryV1, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    self.validate_definition(2, registry.projection())?;
    for entry in registry.entries() {
      self.check()?;
      let definition = encode_semantic_definition_object(6, entry.dependency_bytes(), self.request.hash_algorithm).map_err(format_error)?;
      self.upsert(6, &definition.semantic_id, &definition, store)?;
    }
    self.upsert(2, b"\x02\x00/.aeordb-config/parsers.json", registry.projection(), store)
  }

  fn configuration(&mut self, configuration: &CompiledIndexConfigurationV1, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    self.check()?;
    if configuration.registry_projection_id() != self.registry_projection_id {
      return Err(invalid("semantic_catalog_configuration_registry", "configuration was compiled against another registry projection"));
    }
    let scope = decode_scope_definition(&configuration.scope().value, self.request.hash_algorithm).map_err(format_error)?;
    let owner = configuration_owner(scope.owner_path)?;
    self.validate_definition(1, configuration.projection())?;
    // Probe only the one control binding. Replacing a duplicate control could
    // leave its previously emitted value/field bindings behind; reject it.
    let prospective = self.plan(1, &owner, configuration.projection(), store)?;
    if prospective.record_count() == self.records {
      return Err(invalid("semantic_catalog_duplicate_configuration", "captured stream repeats a configuration owner"));
    }
    drop(prospective);
    for dependency in configuration.dependencies() {
      self.check()?;
      let decoded = decode_semantic_definition_record(&dependency.object.value, self.request.hash_algorithm).map_err(format_error)?;
      if !matches!(decoded.class, 6 | 7) {
        return Err(invalid("semantic_catalog_dependency_class", "compiled dependency is not an executable or native definition"));
      }
      self.upsert(decoded.class, &dependency.semantic_id, dependency, store)?;
    }
    let scope = encode_semantic_definition_object(3, &configuration.scope().value, self.request.hash_algorithm).map_err(format_error)?;
    self.upsert(3, &scope.semantic_id, &scope, store)?;
    for field in configuration.fields() {
      self.check()?;
      let value = encode_semantic_definition_object(4, &field.value_store().value, self.request.hash_algorithm).map_err(format_error)?;
      self.upsert(4, &value.semantic_id, &value, store)?;
      for index in field.field_indexes() {
        self.check()?;
        let field = encode_semantic_definition_object(5, &index.value, self.request.hash_algorithm).map_err(format_error)?;
        self.upsert(5, &field.semantic_id, &field, store)?;
      }
    }
    self.upsert(1, &owner, configuration.projection(), store)
  }

  fn validate_definition(&self, class: u16, definition: &EncodedSemanticDefinitionObjectV1) -> Result<()> {
    self.check()?;
    let decoded = decode_semantic_definition_record(&definition.object.value, self.request.hash_algorithm).map_err(format_error)?;
    let rebuilt = encode_semantic_definition_object(class, decoded.definition, self.request.hash_algorithm).map_err(format_error)?;
    if decoded.class != class || &rebuilt != definition {
      return Err(invalid(
        "semantic_catalog_definition_identity",
        "compiled definition disagrees with its class or selected hash algorithm",
      ));
    }
    self.check()
  }

  fn plan(
    &self,
    class: u16,
    owner: &[u8],
    definition: &EncodedSemanticDefinitionObjectV1,
    store: &dyn SemanticCatalogStagingStoreV1,
  ) -> Result<SemanticCatalogMutationPlanV1> {
    self.plan_mutation(
      SemanticCatalogSnapshotV1 { root_object_id: self.root.as_deref(), record_count: self.records, node_count: self.nodes },
      SemanticCatalogMutationV1::Upsert(SemanticCatalogRecordV1 {
        record_kind: class,
        owner_key: owner,
        semantic_id: &definition.semantic_id,
        definition_object_id: &definition.object.object_id,
      }),
      store,
    )
  }

  fn plan_mutation(
    &self,
    snapshot: SemanticCatalogSnapshotV1<'_>,
    mutation: SemanticCatalogMutationV1<'_>,
    store: &dyn SemanticCatalogStagingStoreV1,
  ) -> Result<SemanticCatalogMutationPlanV1> {
    self.check()?;
    Ok(plan_semantic_catalog_mutation_v1(
      SemanticCatalogMutationRequestV1 {
        hash_algorithm: self.request.hash_algorithm,
        snapshot,
        mutation,
        maximum_workspace_bytes: self.request.maximum_workspace_bytes - STAGING_WORKSPACE_BYTES,
      },
      store,
      self.memory,
      self.is_cancelled,
    )?)
  }

  fn upsert(
    &mut self,
    class: u16,
    owner: &[u8],
    definition: &EncodedSemanticDefinitionObjectV1,
    store: &mut dyn SemanticCatalogStagingStoreV1,
  ) -> Result<()> {
    self.validate_definition(class, definition)?;
    let plan = self.plan(class, owner, definition, store)?;
    let added = plan.record_count() > self.records;
    if !added && !plan.is_unchanged() {
      return Err(invalid("semantic_catalog_identity_conflict", "one complete semantic identity names conflicting definition objects"));
    }
    let dependencies = if added && matches!(class, 6 | 7) {
      self.dependencies.checked_add(1).ok_or_else(|| resource("semantic_catalog_counts", "dependency count overflow"))?
    } else {
      self.dependencies
    };
    let root = plan.root_object_id().map(copy_bytes).transpose()?;
    self.publish(std::slice::from_ref(&definition.object), store)?;
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

  fn publish(&self, objects: &[EncodedSemanticObjectV1], store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    self.check()?;
    let bytes = objects
      .iter()
      .try_fold(0usize, |bytes, object| bytes.checked_add(object.value.len()))
      .ok_or_else(|| resource("semantic_catalog_staging_bytes", "staging batch length overflow"))?;
    if bytes > STAGING_BATCH_BYTES {
      return Err(resource("semantic_catalog_staging_bytes", "batch exceeds admitted staging scratch"));
    }
    store.publish_semantic_objects(objects)?;
    self.check()?;
    for object in objects {
      let kind = decode_semantic_object(&object.value, self.request.hash_algorithm).map_err(format_error)?.kind_id;
      let readback = store
        .load_semantic_object(kind, &object.object_id)?
        .ok_or_else(|| corrupt("semantic_catalog_staging_missing", "published immutable object is missing on read-back"))?;
      if readback != object.value {
        return Err(corrupt("semantic_catalog_staging_mismatch", "published immutable object differs on read-back"));
      }
      self.check()?;
    }
    Ok(())
  }

  fn validate_closure(&self, store: &dyn SemanticCatalogStagingStoreV1) -> Result<()> {
    self.check()?;
    let root = self.root.as_deref().ok_or_else(|| corrupt("semantic_catalog_root", "compiled registry did not produce a catalog"))?;
    let reader = SemanticCatalogReaderV1::new(self.request.hash_algorithm, store);
    let mut dependencies = 0u64;
    let stats =
      reader.walk_catalog(root, SemanticCatalogTraversalBoundsV1::new(self.records, self.nodes)?, self.is_cancelled, |record| {
        self.check().map_err(catalog_check_error)?;
        reader.with_definition(record, self.is_cancelled, |bytes| {
          let rebuilt = encode_semantic_definition_object(record.record_kind, bytes, self.request.hash_algorithm)
            .map_err(|error| catalog_check_error(format_error(error)))?;
          if rebuilt.semantic_id != record.semantic_id || rebuilt.object.object_id != record.definition_object_id {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "semantic_catalog_definition_closure",
              "read-back definition does not match its binding",
            ));
          }
          Ok(())
        })?;
        if matches!(record.record_kind, 6 | 7) {
          dependencies = dependencies
            .checked_add(1)
            .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_counts", "dependency count overflow"))?;
        }
        Ok(())
      })?;
    if stats.records != self.records || stats.nodes != self.nodes || dependencies != self.dependencies {
      return Err(corrupt("semantic_catalog_counts", "final closure counts disagree with staged catalog"));
    }
    Ok(())
  }

  fn finish(self, store: &mut dyn SemanticCatalogStagingStoreV1) -> Result<CompiledSemanticCatalogV1> {
    self.check()?;
    let root = self.root.as_deref().ok_or_else(|| corrupt("semantic_catalog_root", "compiled registry did not produce a catalog"))?;
    // Classes3..7 are keyed by complete IDs. Class2 is a singleton; every
    // class1 projection includes its uniquely owned ScopeId, and duplicate
    // configuration owners were rejected. Therefore each remaining binding
    // names one distinct definition object, without an unbounded identity set.
    let semantic_state = encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: self.request.required_capabilities,
        availability: SemanticAvailabilityV1::Complete {
          compiler_fingerprint: copy_bytes(semantic_compiler_fingerprint_v1(self.request.hash_algorithm))?,
          semantic_registry_fingerprint: copy_bytes(
            &embedded_system_family_registry(self.request.hash_algorithm).map_err(format_error)?.semantic_projection_fingerprint,
          )?,
          catalog_root: copy_bytes(root)?,
          catalog_record_count: self.records,
          catalog_node_count: self.nodes,
          definition_count: self.records,
          dependency_count: self.dependencies,
        },
      },
      self.request.hash_algorithm,
    )
    .map_err(format_error)?;
    self.publish(std::slice::from_ref(&semantic_state), store)?;
    self.check()?;
    Ok(CompiledSemanticCatalogV1 {
      semantic_state,
      hash_algorithm: self.request.hash_algorithm,
      catalog_root: self.root,
      record_count: self.records,
      node_count: self.nodes,
      dependency_count: self.dependencies,
      configuration_count: self.configurations,
      _memory: self.reservation,
    })
  }
}

fn configuration_owner(path: &str) -> Result<Vec<u8>> {
  let directory = path.trim_end_matches('/').as_bytes();
  let suffix = b"/.aeordb-config/indexes.json";
  let length = directory
    .len()
    .checked_add(suffix.len())
    .and_then(|value| value.checked_add(2))
    .ok_or_else(|| invalid("semantic_catalog_configuration_path", "configuration path length overflow"))?;
  if length > 65_537 {
    return Err(invalid("semantic_catalog_configuration_path", "configuration path exceeds the frozen owner-key limit"));
  }
  let mut owner = Vec::new();
  owner.try_reserve_exact(length).map_err(|error| resource("semantic_catalog_allocation", error.to_string()))?;
  owner.extend_from_slice(&1u16.to_le_bytes());
  owner.extend_from_slice(directory);
  owner.extend_from_slice(suffix);
  Ok(owner)
}

fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
  let mut result = Vec::new();
  result.try_reserve_exact(bytes.len()).map_err(|error| resource("semantic_catalog_allocation", error.to_string()))?;
  result.extend_from_slice(bytes);
  Ok(result)
}

fn cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  if is_cancelled() {
    return Err(SemanticCatalogReadErrorV1::cancelled("semantic_catalog_cancelled", "catalog compilation was cancelled").into());
  }
  Ok(())
}

fn format_error(error: FormatError) -> SemanticCatalogCompilationErrorV1 {
  if error.is_allocation_failure() || error.class() == MalformedInputClass::AllocationAmplification {
    resource(error.code(), error.to_string())
  } else {
    corrupt(error.code(), error.to_string())
  }
}

fn catalog_check_error(error: SemanticCatalogCompilationErrorV1) -> SemanticCatalogReadErrorV1 {
  match error {
    SemanticCatalogCompilationErrorV1::Catalog(error) => error,
    other => SemanticCatalogReadErrorV1::corrupt("semantic_catalog_validation", other.to_string()),
  }
}

fn invalid(code: &'static str, message: impl Into<String>) -> SemanticCatalogCompilationErrorV1 {
  SemanticCatalogCompilationErrorV1::InvalidInput { code, message: message.into() }
}

fn resource(code: &'static str, message: impl Into<String>) -> SemanticCatalogCompilationErrorV1 {
  SemanticCatalogReadErrorV1::resource(code, message).into()
}

fn corrupt(code: &'static str, message: impl Into<String>) -> SemanticCatalogCompilationErrorV1 {
  SemanticCatalogReadErrorV1::corrupt(code, message).into()
}

#[cfg(test)]
#[path = "../../../spec/engine/semantic_catalog_compiler_boundary_spec.rs"]
mod boundary_tests;

#[path = "semantic_catalog_update.rs"]
mod update;
pub use update::{SemanticCatalogConfigurationMutationV1, SemanticCatalogUpdateRequestV1, update_semantic_catalog_v1};

#[path = "semantic_catalog_admission.rs"]
mod admission;
pub use admission::admit_semantic_catalog_v1;

#[path = "semantic_catalog_progress.rs"]
mod progress;
pub use progress::{AdmittedSemanticCatalogProgressV1, admit_semantic_catalog_progress_v1};
