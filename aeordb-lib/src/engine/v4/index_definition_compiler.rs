//! Complete corrected definitions from captured source/parser compiler outputs.
//! Public JSON ingress, semantic profiles and atomic activation belong to callers.

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::field_definition::{
  ConverterDefinitionWriteV1, EncodedFieldIndexDefinitionV1, FieldIndexDefinitionWriteV1, encode_converter_definition,
  encode_field_index_definition,
};
use super::parser_context_compiler::CompiledParserContextV1;
use super::parser_registry_compiler::SemanticCompilationErrorV1;
use super::reader::{FormatError, MalformedInputClass};
use super::source_selector::{SourceSelectorKind, decode_source_selector};
use super::source_selector_compiler::CompiledSourceSelectorV1;
use super::value_store::{EncodedValueStoreDefinitionV1, ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily, encode_value_store_definition};

const SOURCE_PATH: &str = "<index-definition>";
const FIXED_WORKSPACE_BYTES: usize = 2 * 1024 * 1024;
const INDEX_WORKSPACE_BYTES: usize = 1024;
type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;

#[derive(Clone, Copy, Default)]
pub struct SourceDefinitionLimitsInputV1 {
  pub max_source_values_per_document: Option<u32>,
  pub max_canonical_source_bytes_per_document: Option<u64>,
  pub max_document_input_bytes: Option<u64>,
  pub max_selector_work_items_per_document: Option<u64>,
  pub max_selector_examined_bytes_per_document: Option<u64>,
}

#[derive(Clone, Copy, Default)]
pub struct ConverterDefinitionLimitsInputV1 {
  pub max_input_bytes: Option<u64>,
  pub max_output_values: Option<u32>,
  pub max_output_value_bytes: Option<u32>,
  pub max_total_output_bytes: Option<u64>,
}

#[derive(Clone, Copy, Default)]
pub struct FieldDefinitionLimitsInputV1 {
  pub max_terms_per_document: Option<u32>,
  pub max_postings_per_document: Option<u32>,
  pub max_canonical_posting_bytes_per_document: Option<u64>,
  pub max_query_recheck_value_bytes: Option<u64>,
}

#[derive(Clone, Copy)]
pub struct CorrectedIndexInputV1 {
  pub converter_id: u16,
  pub converter_limits: ConverterDefinitionLimitsInputV1,
  pub field_limits: FieldDefinitionLimitsInputV1,
}

#[derive(Clone)]
pub struct IndexDefinitionCompilationRequestV1<'a> {
  pub scope_id: &'a [u8],
  pub source: &'a CompiledSourceSelectorV1,
  pub parser_context: &'a CompiledParserContextV1,
  pub source_limits: SourceDefinitionLimitsInputV1,
  pub indexes: &'a [CorrectedIndexInputV1],
  pub hash_algorithm: HashAlgorithm,
  pub maximum_workspace_bytes: usize,
}

pub struct CompiledIndexDefinitionsV1 {
  value_store: EncodedValueStoreDefinitionV1,
  field_indexes: Vec<EncodedFieldIndexDefinitionV1>,
  _memory: MemoryReservation,
}

impl CompiledIndexDefinitionsV1 {
  pub(crate) fn workspace_bytes(&self) -> u64 {
    self._memory.bytes()
  }

  pub fn value_store(&self) -> &EncodedValueStoreDefinitionV1 {
    &self.value_store
  }

  pub fn field_indexes(&self) -> &[EncodedFieldIndexDefinitionV1] {
    &self.field_indexes
  }
}

pub struct DefaultMetadataIndexV1 {
  pub field_name: &'static str,
  pub converter_ids: &'static [u16],
}

/// Round 11's corrected defaults, in canonical field-name order. The profile
/// owner compiles these under its captured scope and task admission.
pub fn default_metadata_indexes_v1() -> &'static [DefaultMetadataIndexV1] {
  &[
    DefaultMetadataIndexV1 { field_name: "@content_type", converter_ids: &[1] },
    DefaultMetadataIndexV1 { field_name: "@created_at", converter_ids: &[7] },
    DefaultMetadataIndexV1 { field_name: "@extension", converter_ids: &[3] },
    DefaultMetadataIndexV1 { field_name: "@filename", converter_ids: &[3, 9, 10, 11, 12] },
    DefaultMetadataIndexV1 { field_name: "@hash", converter_ids: &[1] },
    DefaultMetadataIndexV1 { field_name: "@path", converter_ids: &[3, 9] },
    DefaultMetadataIndexV1 { field_name: "@size", converter_ids: &[4] },
    DefaultMetadataIndexV1 { field_name: "@updated_at", converter_ids: &[7] },
  ]
}

pub fn compile_index_definitions_v1(
  request: IndexDefinitionCompilationRequestV1<'_>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledIndexDefinitionsV1> {
  cancelled(is_cancelled)?;
  if request.scope_id.len() != request.hash_algorithm.hash_length() || request.scope_id.iter().all(|byte| *byte == 0) {
    return Err(invalid("ScopeId must be nonzero and have the selected database hash width"));
  }
  if request.indexes.is_empty() {
    return Err(invalid("a field requires at least one corrected index definition"));
  }
  // The child readers validate regexes again. Charge their syntax workspace,
  // not only retained wire bytes, independently of the source compiler lease.
  // Corrected converters have no parameters; 1024 bytes per input covers each
  // field record, identities, output-vector entry and transient converter bytes.
  let workspace = request
    .source
    .selector()
    .len()
    .checked_mul(768)
    .and_then(|bytes| bytes.checked_add(request.source.field_name().len().checked_mul(8)?))
    .and_then(|bytes| bytes.checked_add(request.parser_context.parser_plan().len().checked_mul(8)?))
    .and_then(|bytes| bytes.checked_add(request.parser_context.dependencies().len().checked_mul(8)?))
    .and_then(|bytes| bytes.checked_add(request.indexes.len().checked_mul(INDEX_WORKSPACE_BYTES)?))
    .and_then(|bytes| bytes.checked_add(FIXED_WORKSPACE_BYTES))
    .ok_or_else(|| resource("definition compiler workspace overflow"))?;
  if workspace > request.maximum_workspace_bytes {
    return Err(resource("definition compiler workspace exceeds its operational limit"));
  }
  let reservation =
    memory.reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Workload).map_err(|error| resource(error.to_string()))?;
  check(&reservation, is_cancelled)?;
  for index in request.indexes {
    if !(1..=12).contains(&index.converter_id) {
      return Err(invalid("new definitions require a permanent corrected converter ID from 1 through 12"));
    }
  }
  let selector = decode_source_selector(request.source.selector()).map_err(format_error)?;
  if selector.kind == SourceSelectorKind::AlwaysMissingV0 {
    return Err(invalid("migration-only source selectors cannot author corrected definitions"));
  }
  let metadata = selector.kind == SourceSelectorKind::Metadata;
  let json = selector.kind == SourceSelectorKind::JsonPath;
  let limits = request.source_limits;
  let source = ValueStoreDefinitionWriteV1 {
    scope_id: request.scope_id,
    field_name: request.source.field_name(),
    semantic_family: ValueStoreSemanticFamily::CorrectedV1,
    max_source_values_per_document: limit_u32(limits.max_source_values_per_document, 1024, "source values per document")?,
    max_canonical_source_bytes_per_document: limit(
      limits.max_canonical_source_bytes_per_document,
      8 << 20,
      8 << 20,
      "source bytes per document",
    )?,
    max_document_input_bytes: applicable_limit(limits.max_document_input_bytes, !metadata, 64 << 20, 1 << 30, "document input bytes")?,
    max_selector_work_items_per_document: applicable_limit(
      limits.max_selector_work_items_per_document,
      json,
      1_000_000,
      1_000_000,
      "selector work items",
    )?,
    max_selector_examined_bytes_per_document: applicable_limit(
      limits.max_selector_examined_bytes_per_document,
      json,
      64 << 20,
      64 << 20,
      "selector examined bytes",
    )?,
    selector: request.source.selector(),
    parser_plan: request.parser_context.parser_plan(),
    dependencies: request.parser_context.dependencies(),
  };
  check(&reservation, is_cancelled)?;
  let value_store = encode_value_store_definition(source, request.hash_algorithm).map_err(format_error)?;
  let mut field_indexes = Vec::new();
  field_indexes.try_reserve_exact(request.indexes.len()).map_err(|error| resource(error.to_string()))?;
  for index in request.indexes {
    check(&reservation, is_cancelled)?;
    let limits = index.converter_limits;
    let converter = encode_converter_definition(
      ConverterDefinitionWriteV1 {
        converter_id: index.converter_id,
        max_input_bytes: limit(limits.max_input_bytes, 1 << 20, 1 << 20, "converter input bytes")?,
        max_output_values: limit_u32(limits.max_output_values, 65536, "converter output values")?,
        max_output_value_bytes: limit_u32(limits.max_output_value_bytes, 1 << 20, "converter output value bytes")?,
        max_total_output_bytes: limit(limits.max_total_output_bytes, 4 << 20, 4 << 20, "converter total output bytes")?,
        parameters: &[],
      },
      request.hash_algorithm,
    )
    .map_err(format_error)?;
    check(&reservation, is_cancelled)?;
    let limits = index.field_limits;
    let field = encode_field_index_definition(
      FieldIndexDefinitionWriteV1 {
        value_store_id: &value_store.value_store_id,
        converter_definition: &converter.value,
        max_terms_per_document: limit_u32(limits.max_terms_per_document, 65536, "field terms per document")?,
        max_postings_per_document: limit_u32(limits.max_postings_per_document, 65536, "field postings per document")?,
        max_canonical_posting_bytes_per_document: limit(
          limits.max_canonical_posting_bytes_per_document,
          8 << 20,
          8 << 20,
          "field posting bytes per document",
        )?,
        max_query_recheck_value_bytes: limit(limits.max_query_recheck_value_bytes, 8 << 20, 8 << 20, "field query recheck bytes")?,
      },
      request.hash_algorithm,
    )
    .map_err(format_error)?;
    field_indexes.push(field);
  }
  check(&reservation, is_cancelled)?;
  field_indexes.sort_unstable_by(|left, right| left.index_id.cmp(&right.index_id));
  if field_indexes.windows(2).any(|pair| pair[0].index_id == pair[1].index_id && pair[0].value != pair[1].value) {
    return Err(SemanticCompilationErrorV1::Operational {
      path: SOURCE_PATH,
      message: "different complete definitions have the same FieldIndexId".into(),
    });
  }
  field_indexes.dedup_by(|left, right| left.index_id == right.index_id);
  check(&reservation, is_cancelled)?;
  Ok(CompiledIndexDefinitionsV1 { value_store, field_indexes, _memory: reservation })
}

fn applicable_limit(value: Option<u64>, applicable: bool, default: u64, maximum: u64, name: &'static str) -> Result<u64> {
  if applicable {
    return limit(value, default, maximum, name);
  }
  if value.is_some_and(|value| value != 0) {
    return Err(invalid(format!("{name} must be zero or absent when inapplicable")));
  }
  Ok(0)
}

fn limit_u32(value: Option<u32>, maximum: u32, name: &'static str) -> Result<u32> {
  let value = limit(value.map(u64::from), u64::from(maximum), u64::from(maximum), name)?;
  u32::try_from(value).map_err(|error| invalid(error.to_string()))
}

fn limit(value: Option<u64>, default: u64, maximum: u64, name: &'static str) -> Result<u64> {
  let value = match value {
    Some(value) => value,
    None => default,
  };
  if value == 0 || value > maximum {
    return Err(invalid(format!("{name} must be from 1 through {maximum}")));
  }
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

fn invalid(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: SOURCE_PATH, message: message.into() }
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: SOURCE_PATH, message: message.into() }
}
