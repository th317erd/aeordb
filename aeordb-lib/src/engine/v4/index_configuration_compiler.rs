//! Compile one captured corrected configuration without publication authority.
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;

use super::config_value::{CanonicalConfigValueV1 as Value, CanonicalValueBounds, encode_canonical_value};
use super::dependency::{DependencyRecordV1, decode_dependency_table, encode_dependency_record};
use super::field_definition::EncodedFieldIndexDefinitionV1;
use super::index_configuration_source::{self, ConfigurationSource, FieldSource, SelectorSource};
use super::index_definition_compiler::{IndexDefinitionCompilationRequestV1, compile_index_definitions_v1};
use super::namespace::{EncodedSemanticDefinitionObjectV1, encode_semantic_definition_object};
use super::parser_context_compiler::{
  ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
};
use super::parser_registry_compiler::{CompiledParserRegistryV1, ParserAliasSnapshotV1, SemanticCompilationErrorV1};
use super::reader::{FormatError, MalformedInputClass};
use super::scope::{EncodedScopeDefinitionV1, ScopeDefinitionWriteV1, ScopeMatchingMode, encode_scope_definition};
use super::source_selector_compiler::{SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1};
use super::value_store::EncodedValueStoreDefinitionV1;

const SOURCE_PATH: &str = "<index-configuration>";
const FIXED_WORKSPACE: usize = 8 << 20;
pub(super) type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;

/// All resolution borrows belong to the same captured deployment snapshot.
/// Artifact verification and later activation conflict checks belong to its owner.
pub trait IndexConfigurationAliasSnapshotV1: ParserAliasSnapshotV1 {
  fn resolve_mapper_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>>;
}

#[derive(Clone, Copy)]
pub struct IndexConfigurationCompilationRequestV1<'a> {
  pub source: &'a [u8],
  pub owner_path: &'a str,
  pub registry: &'a CompiledParserRegistryV1,
  pub hash_algorithm: HashAlgorithm,
  pub maximum_source_bytes: usize,
  /// Aggregate source/output and overlapping child-compiler admission. Already
  /// captured inputs (including registry ownership) retain their caller's lease.
  pub maximum_workspace_bytes: usize,
}

pub struct CompiledConfigurationFieldV1 {
  field_name: String,
  value_store: EncodedValueStoreDefinitionV1,
  field_indexes: Vec<EncodedFieldIndexDefinitionV1>,
}

impl CompiledConfigurationFieldV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }
  pub fn value_store(&self) -> &EncodedValueStoreDefinitionV1 {
    &self.value_store
  }
  pub fn field_indexes(&self) -> &[EncodedFieldIndexDefinitionV1] {
    &self.field_indexes
  }
}

pub struct CompiledIndexConfigurationV1 {
  scope: EncodedScopeDefinitionV1,
  projection: EncodedSemanticDefinitionObjectV1,
  fields: Vec<CompiledConfigurationFieldV1>,
  dependencies: Vec<EncodedSemanticDefinitionObjectV1>,
  _memory: MemoryReservation,
}

impl CompiledIndexConfigurationV1 {
  pub fn scope(&self) -> &EncodedScopeDefinitionV1 {
    &self.scope
  }
  pub fn projection(&self) -> &EncodedSemanticDefinitionObjectV1 {
    &self.projection
  }
  pub fn fields(&self) -> &[CompiledConfigurationFieldV1] {
    &self.fields
  }
  pub fn dependencies(&self) -> &[EncodedSemanticDefinitionObjectV1] {
    &self.dependencies
  }
}

pub fn compile_index_configuration_v1(
  request: IndexConfigurationCompilationRequestV1<'_>,
  snapshot: &dyn IndexConfigurationAliasSnapshotV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledIndexConfigurationV1> {
  cancelled(is_cancelled)?;
  if request.source.len() > request.maximum_source_bytes {
    return Err(resource("source exceeds its capture limit"));
  }
  // Before strict JSON parsing or path normalization: one node per raw byte
  // charged at the shared conservative 768-byte retained-node bound. The fixed
  // allowance covers scope/projection metadata and bounded compiler scratch.
  // Owner normalization also holds a growable vector of borrowed path segments,
  // a NUL-stripped copy and joined output; charge32x its raw bytes before work.
  // Generic serde/BTreeMap allocations are not claimed universally fallible.
  let workspace = request
    .source
    .len()
    .checked_mul(768)
    .and_then(|bytes| bytes.checked_add(request.owner_path.len().checked_mul(32)?))
    .and_then(|bytes| bytes.checked_add(FIXED_WORKSPACE))
    .ok_or_else(|| resource("source workspace overflow"))?;
  if workspace > request.maximum_workspace_bytes {
    return Err(resource("source workspace exceeds its operational limit"));
  }
  let mut reservation =
    memory.reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Workload).map_err(|error| resource(error.to_string()))?;
  check(&reservation, is_cancelled)?;
  let mut source = index_configuration_source::parse(request.source)?;
  check(&reservation, is_cancelled)?;
  let owner_path = normalize_path(request.owner_path);
  let glob = source.glob.as_deref().map(normalize_glob).transpose()?;
  let scope = encode_scope_definition(
    ScopeDefinitionWriteV1 {
      mode: if glob.is_some() { ScopeMatchingMode::RelativePathGlob } else { ScopeMatchingMode::DirectChildren },
      owner_path: &owner_path,
      glob: glob.as_deref(),
    },
    request.hash_algorithm,
  )
  .map_err(format_error)?;
  // Sorted canonical names allow one compact output per field, without a
  // whole-world catalog load or quadratic search through earlier field rows.
  source.rows.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
  let parser = if source.rows.iter().any(|row| !matches!(row.source, SelectorSource::Metadata)) {
    source.parser.as_deref().map(|alias| snapshot.resolve_parser_alias(alias)?.ok_or_else(|| unavailable(alias))).transpose()?
  } else {
    None
  };
  check(&reservation, is_cancelled)?;
  let mut mappers: BTreeMap<&str, DependencyRecordV1<'_>> = BTreeMap::new();
  let mut fields: Vec<CompiledConfigurationFieldV1> = allocate(source.rows.len())?;
  let mut dependencies = BTreeMap::new();
  for row in &source.rows {
    check(&reservation, is_cancelled)?;
    let mapper = if let SelectorSource::Mapper { alias, .. } = &row.source {
      match mappers.entry(alias.as_str()) {
        Entry::Occupied(entry) => Some(entry.get().clone()),
        Entry::Vacant(entry) => {
          let dependency = snapshot.resolve_mapper_alias(alias)?.ok_or_else(|| unavailable(alias))?;
          check(&reservation, is_cancelled)?;
          Some(entry.insert(dependency).clone())
        }
      }
    } else {
      None
    };
    let field = compile_field(
      row,
      &source,
      parser.as_ref(),
      mapper,
      &mut FieldCompilation {
        scope: &scope,
        request: &request,
        memory,
        reservation: &mut reservation,
        dependencies: &mut dependencies,
        is_cancelled,
      },
    )?;
    if let Some(previous) = fields.last_mut().filter(|previous| previous.field_name == field.field_name) {
      if previous.value_store != field.value_store {
        if previous.value_store.value_store_id == field.value_store.value_store_id {
          return Err(operational("different complete ValueStore definitions have the same identity"));
        }
        return Err(invalid("one field and scope cannot have conflicting ValueStore definitions"));
      }
      previous.field_indexes.try_reserve_exact(field.field_indexes.len()).map_err(|error| resource(error.to_string()))?;
      previous.field_indexes.extend(field.field_indexes);
    } else {
      fields.push(field);
    }
  }
  for field in &mut fields {
    check(&reservation, is_cancelled)?;
    field.field_indexes.sort_unstable_by(|left, right| left.index_id.cmp(&right.index_id));
    if field.field_indexes.windows(2).any(|pair| pair[0].index_id == pair[1].index_id && pair[0].value != pair[1].value) {
      return Err(operational("different complete field definitions have the same identity"));
    }
    field.field_indexes.dedup_by(|left, right| left.index_id == right.index_id);
  }
  check(&reservation, is_cancelled)?;
  let projection = projection(&scope, &fields, request.hash_algorithm)?;
  let mut dependency_objects = allocate(dependencies.len())?;
  dependency_objects.extend(dependencies.into_values());
  check(&reservation, is_cancelled)?;
  Ok(CompiledIndexConfigurationV1 { scope, projection, fields, dependencies: dependency_objects, _memory: reservation })
}

struct FieldCompilation<'a, 'source> {
  scope: &'a EncodedScopeDefinitionV1,
  request: &'a IndexConfigurationCompilationRequestV1<'source>,
  memory: &'a MemoryCoordinator,
  reservation: &'a mut MemoryReservation,
  dependencies: &'a mut BTreeMap<(u16, Vec<u8>), EncodedSemanticDefinitionObjectV1>,
  is_cancelled: &'a dyn Fn() -> bool,
}

fn compile_field(
  row: &FieldSource,
  configuration: &ConfigurationSource,
  parser: Option<&DependencyRecordV1<'_>>,
  mapper: Option<DependencyRecordV1<'_>>,
  compilation: &mut FieldCompilation<'_, '_>,
) -> Result<CompiledConfigurationFieldV1> {
  let FieldCompilation { scope, request, memory, reservation, dependencies, is_cancelled } = compilation;
  let metadata = matches!(row.source, SelectorSource::Metadata);
  let context_source = if metadata {
    ParserContextSourceV1::Metadata
  } else if let Some(dependency) = parser {
    ParserContextSourceV1::Explicit { dependency: dependency.clone(), policy: &configuration.wasm }
  } else {
    ParserContextSourceV1::Automatic {
      registry: request.registry,
      registry_policy: (!request.registry.entries().is_empty()).then_some(&configuration.wasm),
      raw_json_policy: &configuration.raw_json,
      native_suite_policy: &configuration.native_suite,
    }
  };
  let selector_dependency = match &row.source {
    SelectorSource::Metadata => ParserSelectorDependencyV1::None,
    SelectorSource::JsonPath(_) => ParserSelectorDependencyV1::JsonPath,
    SelectorSource::Mapper { .. } => ParserSelectorDependencyV1::Mapper(mapper.ok_or_else(|| operational("captured mapper disappeared"))?),
  };
  let context = compile_parser_context_v1(
    ParserContextCompilationRequestV1 {
      source: context_source,
      selector_dependency,
      maximum_workspace_bytes: remaining_workspace(reservation, request.maximum_workspace_bytes, 0)?,
    },
    memory,
    is_cancelled,
  )?;
  let source = match &row.source {
    SelectorSource::Metadata => SourceSelectorInputV1::Metadata,
    SelectorSource::JsonPath(values) => SourceSelectorInputV1::JsonPath(values.as_deref()),
    SelectorSource::Mapper { arguments, policy, .. } => SourceSelectorInputV1::Mapper {
      dependency_ordinal: context.selector_dependency_ordinal().ok_or_else(|| operational("mapper ordinal is missing"))?,
      arguments: arguments.as_deref(),
      policy,
    },
  };
  let source = compile_source_selector_v1(
    SourceSelectorCompilationRequestV1 {
      field_name: &row.name,
      source,
      maximum_source_bytes: request.maximum_source_bytes,
      maximum_workspace_bytes: remaining_workspace(reservation, request.maximum_workspace_bytes, context.workspace_bytes())?,
    },
    memory,
    is_cancelled,
  )?;
  let source_workspace =
    context.workspace_bytes().checked_add(source.workspace_bytes()).ok_or_else(|| resource("child workspace accounting overflow"))?;
  let definitions = compile_index_definitions_v1(
    IndexDefinitionCompilationRequestV1 {
      scope_id: &scope.scope_id,
      source: &source,
      parser_context: &context,
      source_limits: row.limits,
      indexes: &row.indexes,
      hash_algorithm: request.hash_algorithm,
      maximum_workspace_bytes: remaining_workspace(reservation, request.maximum_workspace_bytes, source_workspace)?,
    },
    memory,
    is_cancelled,
  )?;
  // Child output still owns its lease. Acquire parent charge BEFORE copying it;
  // then all three transient compiler leases drop at this field boundary.
  let retained = definitions
    .field_indexes()
    .iter()
    .try_fold(definitions.value_store().value.len(), |bytes, field| bytes.checked_add(field.value.len()))
    .and_then(|bytes| bytes.checked_add(context.dependencies().len()))
    .and_then(|bytes| bytes.checked_mul(8))
    .and_then(|bytes| bytes.checked_add(4096))
    .ok_or_else(|| resource("retained definitions charge overflow"))?;
  let transient =
    source_workspace.checked_add(definitions.workspace_bytes()).ok_or_else(|| resource("child workspace accounting overflow"))?;
  if retained > remaining_workspace(reservation, request.maximum_workspace_bytes, transient)? {
    return Err(resource("retained definitions exceed the compiler workspace limit"));
  }
  reservation.grow(retained as u64).map_err(|error| resource(error.to_string()))?;
  check(reservation, is_cancelled)?;
  let mut field_indexes = allocate(definitions.field_indexes().len())?;
  for field in definitions.field_indexes() {
    field_indexes.push(EncodedFieldIndexDefinitionV1 { index_id: copy(&field.index_id)?, value: copy(&field.value)? });
  }
  for dependency in decode_dependency_table(context.dependencies()).map_err(format_error)?.records {
    check(reservation, is_cancelled)?;
    let class = if dependency.kind == 1 { 6 } else { 7 };
    let bytes = encode_dependency_record(&dependency).map_err(format_error)?;
    let object = encode_semantic_definition_object(class, &bytes, request.hash_algorithm).map_err(format_error)?;
    let key = (class, copy(&object.semantic_id)?);
    match dependencies.entry(key) {
      Entry::Occupied(entry) if entry.get() != &object => {
        return Err(operational("different dependency definitions have the same complete identity"))
      }
      Entry::Occupied(_) => {}
      Entry::Vacant(entry) => {
        entry.insert(object);
      }
    }
  }
  let mut field_name = String::new();
  field_name.try_reserve_exact(source.field_name().len()).map_err(|error| resource(error.to_string()))?;
  field_name.push_str(source.field_name());
  let value_store = EncodedValueStoreDefinitionV1 {
    value_store_id: copy(&definitions.value_store().value_store_id)?,
    value: copy(&definitions.value_store().value)?,
  };
  check(reservation, is_cancelled)?;
  Ok(CompiledConfigurationFieldV1 { field_name, value_store, field_indexes })
}

fn projection(
  scope: &EncodedScopeDefinitionV1,
  fields: &[CompiledConfigurationFieldV1],
  algorithm: HashAlgorithm,
) -> Result<EncodedSemanticDefinitionObjectV1> {
  let mut projected_fields = BTreeMap::new();
  for field in fields {
    let mut indexes = allocate(field.field_indexes.len())?;
    for index in &field.field_indexes {
      indexes.push(Value::Bytes(copy(&index.index_id)?));
    }
    let members = BTreeMap::from([
      ("indexes".into(), Value::Array(indexes)),
      ("value_store_id".into(), Value::Bytes(copy(&field.value_store.value_store_id)?)),
    ]);
    projected_fields.insert(field.field_name.clone(), Value::Map(members));
  }
  let value = Value::Map(BTreeMap::from([
    ("fields".into(), Value::Map(projected_fields)),
    ("scope_id".into(), Value::Bytes(copy(&scope.scope_id)?)),
  ]));
  let value = encode_canonical_value(&value, CanonicalValueBounds::CONFIG).map_err(format_error)?;
  encode_semantic_definition_object(1, &value, algorithm).map_err(format_error)
}

fn normalize_glob(glob: &str) -> Result<String> {
  let mut result = String::new();
  result.try_reserve_exact(glob.len()).map_err(|error| resource(error.to_string()))?;
  for segment in glob.split('/').filter(|segment| !segment.is_empty()) {
    if matches!(segment, "." | "..") || segment.contains('\0') {
      return Err(invalid("glob contains a dot segment or NUL"));
    }
    if !result.is_empty() {
      result.push('/');
    }
    result.push_str(segment);
  }
  if result.is_empty() {
    return Err(invalid("glob must contain at least one segment"));
  }
  Ok(result)
}

fn remaining_workspace(reservation: &MemoryReservation, maximum: usize, transient_bytes: u64) -> Result<usize> {
  let used = reservation.bytes().checked_add(transient_bytes).ok_or_else(|| resource("compiler workspace accounting overflow"))?;
  let used = usize::try_from(used).map_err(|error| resource(error.to_string()))?;
  maximum.checked_sub(used).ok_or_else(|| resource("overlapping compiler workspaces exceed the operational limit"))
}

pub(super) fn allocate<T>(count: usize) -> Result<Vec<T>> {
  let mut values = Vec::new();
  values.try_reserve_exact(count).map_err(|error| resource(error.to_string()))?;
  Ok(values)
}

fn copy(bytes: &[u8]) -> Result<Vec<u8>> {
  let mut value = allocate(bytes.len())?;
  value.extend_from_slice(bytes);
  Ok(value)
}

fn check(reservation: &MemoryReservation, is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  cancelled(is_cancelled)?;
  reservation.check_admission().map_err(|error| resource(error.to_string()))
}

fn cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  if is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  Ok(())
}

fn format_error(error: FormatError) -> SemanticCompilationErrorV1 {
  if error.class() == MalformedInputClass::AllocationAmplification {
    resource(error.to_string())
  } else {
    invalid(error.to_string())
  }
}

pub(super) fn invalid(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: SOURCE_PATH, message: message.into() }
}

pub(super) fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: SOURCE_PATH, message: message.into() }
}

fn operational(message: &str) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Operational { path: SOURCE_PATH, message: message.into() }
}

fn unavailable(alias: &str) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::DependencyUnavailable {
    path: SOURCE_PATH,
    message: format!("alias {alias:?} is absent from the captured snapshot"),
  }
}

/// Only new-v1 bootstrap consumes this explicit source. Existing controls are
/// never rewritten. Profile conformance must bind this recipe before activation.
pub fn default_index_configuration_v1() -> &'static [u8] {
  br#"{"$v":1,"glob":"**/*","indexes":[
    {"name":"@content_type","type":"typed_exact_blake3_v1"},
    {"name":"@created_at","type":"timestamp_ms_order_v1"},
    {"name":"@extension","type":"utf8_binary_order_v1"},
    {"name":"@filename","type":["utf8_binary_order_v1","unicode_trigram_v1","soundex_ascii_v1","double_metaphone_primary_ascii_v1","double_metaphone_alt_ascii_v1"]},
    {"name":"@hash","type":"typed_exact_blake3_v1"},
    {"name":"@path","type":["utf8_binary_order_v1","unicode_trigram_v1"]},
    {"name":"@size","type":"u64_order_v1"},
    {"name":"@updated_at","type":"timestamp_ms_order_v1"},
    {"name":"metadata.duration","type":"f64_finite_order_v1","source":["metadata","duration_seconds"]},
    {"name":"metadata.format","type":"utf8_binary_order_v1","source":["metadata","format"]},
    {"name":"text","type":"unicode_trigram_v1"},
    {"name":"title","type":["utf8_binary_order_v1","unicode_trigram_v1"]}
  ]}"#
}
