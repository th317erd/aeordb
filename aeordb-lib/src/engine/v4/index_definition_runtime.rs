use std::error::Error;
use std::fmt;
use std::mem::size_of;

use crate::engine::HashAlgorithm;

use super::config_value::{CanonicalValueBounds, decode_canonical_value};
use super::field_definition::{FieldIndexDefinitionV1, decode_field_index_definition};
use super::index_converter::{CompiledPostingKeyV1, ConverterRuntimeV1, IndexSemanticErrorClassV1};
use super::index_semantic_registry::{StrategyRegistryEntryV1, strategy_registry_entry};
use super::index_source::ValueStoreRuntimeV1;
use super::value_store::{ValueStoreDefinitionV1, ValueStoreSemanticFamily, decode_value_store_definition};

const INDEX_RUNTIME_ACCOUNTING_FIXED_BYTES: u64 = 64 * 1_024;
const INDEX_COMPILE_ACCOUNTING_FIXED_BYTES: u64 = 16 * 1_024;
const INDEX_COMPILE_CANONICAL_BYTE_MULTIPLIER: u64 = 64;
const INDEX_COMPILE_POSTING_BYTE_MULTIPLIER: u64 = 4;
const INDEX_COMPILE_POSTING_ENTRY_BYTES: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexDefinitionErrorClassV1 {
  IdentityMismatch,
  SemanticMismatch,
  UnsupportedDefinition,
  InvalidSourceValue,
  ResourceLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDefinitionErrorV1 {
  class: IndexDefinitionErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexDefinitionErrorV1 {
  pub fn class(&self) -> IndexDefinitionErrorClassV1 {
    self.class
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for IndexDefinitionErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for IndexDefinitionErrorV1 {}

pub type IndexDefinitionResultV1<T> = Result<T, IndexDefinitionErrorV1>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledDocumentValueV1 {
  pub source_value_ordinal: u32,
  pub canonical_value: Vec<u8>,
  pub postings: Vec<CompiledPostingKeyV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledIndexDocumentV1 {
  pub values: Vec<CompiledDocumentValueV1>,
  pub posting_count: u32,
  pub canonical_posting_bytes: u64,
  pub query_recheck_value_bytes: u64,
}

#[derive(Debug)]
pub struct IndexDefinitionRuntimeV1<'value, 'field> {
  value_store: ValueStoreRuntimeV1<'value>,
  field_definition: FieldIndexDefinitionV1<'field>,
  converter: ConverterRuntimeV1<'field>,
  strategy: &'static StrategyRegistryEntryV1,
}

impl<'value, 'field> IndexDefinitionRuntimeV1<'value, 'field> {
  pub fn from_encoded(
    value_store_value: &'value [u8],
    field_definition_value: &'field [u8],
    hash_algorithm: HashAlgorithm,
  ) -> IndexDefinitionResultV1<Self> {
    let (value_store_definition, field_definition) = decode_definitions(value_store_value, field_definition_value, hash_algorithm)?;
    Self::from_definitions(value_store_definition, field_definition, hash_algorithm.hash_length())
  }

  pub(crate) fn from_encoded_bounded(
    value_store_value: &'value [u8],
    field_definition_value: &'field [u8],
    hash_algorithm: HashAlgorithm,
    maximum_retained_bytes: u64,
  ) -> IndexDefinitionResultV1<Self> {
    let (value_store_definition, field_definition) = decode_definitions(value_store_value, field_definition_value, hash_algorithm)?;
    let retained_bytes = Self::maximum_retained_bytes_for_definitions(&value_store_definition, &field_definition)?;
    if retained_bytes > maximum_retained_bytes {
      return Err(accounting_error("compiled index runtime exceeds its remaining admitted memory"));
    }
    Self::from_definitions(value_store_definition, field_definition, hash_algorithm.hash_length())
  }

  pub(crate) fn from_definitions(
    value_store_definition: ValueStoreDefinitionV1<'value>,
    field_definition: FieldIndexDefinitionV1<'field>,
    hash_width: usize,
  ) -> IndexDefinitionResultV1<Self> {
    if field_definition.value_store_id != value_store_definition.value_store_id {
      return Err(error(
        IndexDefinitionErrorClassV1::IdentityMismatch,
        "index_value_store_identity_mismatch",
        "FieldIndexDefinitionV1 does not name the supplied complete ValueStoreDefinitionV1",
      ));
    }
    let corrected_value_store = value_store_definition.semantic_family == ValueStoreSemanticFamily::CorrectedV1;
    if field_definition.corrected != corrected_value_store || field_definition.converter.corrected != corrected_value_store {
      return Err(error(
        IndexDefinitionErrorClassV1::SemanticMismatch,
        "index_semantic_family_mismatch",
        "ValueStore, strategy, and converter semantic families disagree",
      ));
    }
    let strategy = strategy_registry_entry(field_definition.strategy_id, field_definition.corrected).ok_or_else(|| {
      error(
        IndexDefinitionErrorClassV1::UnsupportedDefinition,
        "index_strategy_unavailable",
        format!("strategy {} is not present in the permanent runtime registry", field_definition.strategy_id),
      )
    })?;
    if strategy.name != field_definition.strategy_name
      || strategy.operations != field_definition.operations
      || !strategy.supports_converter(field_definition.converter.converter_id)
    {
      return Err(error(
        IndexDefinitionErrorClassV1::SemanticMismatch,
        "index_strategy_closure_mismatch",
        "strategy identity, operation mask, or converter closure disagrees with the permanent registry",
      ));
    }
    let converter = ConverterRuntimeV1::from_definition(field_definition.converter.clone()).map_err(|source| {
      error(
        IndexDefinitionErrorClassV1::UnsupportedDefinition,
        "index_converter_unavailable",
        format!("{}: {}", source.code(), source.context()),
      )
    })?;
    let value_store = ValueStoreRuntimeV1::from_definition(value_store_definition, hash_width).map_err(|source| {
      error(
        IndexDefinitionErrorClassV1::UnsupportedDefinition,
        "index_source_runtime_unavailable",
        format!("{}: {}", source.code(), source.context()),
      )
    })?;
    Ok(Self { value_store, field_definition, converter, strategy })
  }

  pub fn index_id(&self) -> &[u8] {
    &self.field_definition.index_id
  }

  pub fn value_store_id(&self) -> &[u8] {
    &self.value_store.definition().value_store_id
  }

  pub fn value_store(&self) -> &ValueStoreRuntimeV1<'value> {
    &self.value_store
  }

  pub fn field_definition(&self) -> &FieldIndexDefinitionV1<'field> {
    &self.field_definition
  }

  pub fn converter(&self) -> &ConverterRuntimeV1<'field> {
    &self.converter
  }

  pub fn strategy(&self) -> &'static StrategyRegistryEntryV1 {
    self.strategy
  }

  pub fn supports_operation(&self, bit: u8) -> bool {
    bit < 64 && self.strategy.operations & (1u64 << bit) != 0
  }

  /// Conservative retained-memory bound for one compiled definition runtime.
  pub(crate) fn maximum_retained_bytes(&self) -> IndexDefinitionResultV1<u64> {
    Self::maximum_retained_bytes_for_definitions(self.value_store.definition(), &self.field_definition)
  }

  fn maximum_retained_bytes_for_definitions(
    value_store: &ValueStoreDefinitionV1<'_>,
    field_definition: &FieldIndexDefinitionV1<'_>,
  ) -> IndexDefinitionResultV1<u64> {
    let value_store_bytes =
      ValueStoreRuntimeV1::maximum_retained_bytes_for_definition(value_store).map_err(|source| accounting_error(source.to_string()))?;
    value_store_bytes
      .checked_add(INDEX_RUNTIME_ACCOUNTING_FIXED_BYTES)
      .and_then(|bytes| bytes.checked_add(size_of::<Self>() as u64))
      .and_then(|bytes| bytes.checked_add(field_definition.index_id.capacity() as u64))
      .and_then(|bytes| bytes.checked_add(field_definition.converter.converter_fingerprint.capacity() as u64))
      .ok_or_else(|| accounting_error("compiled index runtime retained-byte bound overflowed"))
  }

  /// Conservative peak workspace for compiling one canonical source value.
  ///
  /// The bound covers canonical decode/copies, token folding, fallibly-grown
  /// posting/deduplication tables, and the returned compiled document while it
  /// is inspected by the query executor.
  pub(crate) fn maximum_compile_source_value_bytes(&self, canonical_value_bytes: u64) -> IndexDefinitionResultV1<u64> {
    let converter = self.converter.definition();
    let maximum_postings = if self.converter.registry().tokenizing {
      canonical_value_bytes.saturating_mul(3).saturating_add(1).min(u64::from(converter.max_output_values))
    } else {
      1
    }
    .min(u64::from(self.field_definition.max_terms_per_document))
    .min(u64::from(self.field_definition.max_postings_per_document));
    let maximum_posting_key_bytes =
      canonical_value_bytes.saturating_mul(4).saturating_add(64).min(u64::from(converter.max_output_value_bytes));
    let maximum_posting_bytes = converter
      .max_total_output_bytes
      .min(self.field_definition.max_canonical_posting_bytes_per_document)
      .min(maximum_postings.saturating_mul(maximum_posting_key_bytes));

    INDEX_COMPILE_ACCOUNTING_FIXED_BYTES
      .checked_add(
        canonical_value_bytes
          .checked_mul(INDEX_COMPILE_CANONICAL_BYTE_MULTIPLIER)
          .ok_or_else(|| accounting_error("canonical source-value workspace bound overflowed"))?,
      )
      .and_then(|bytes| bytes.checked_add(maximum_postings.checked_mul(INDEX_COMPILE_POSTING_ENTRY_BYTES)?))
      .and_then(|bytes| bytes.checked_add(maximum_posting_bytes.checked_mul(INDEX_COMPILE_POSTING_BYTE_MULTIPLIER)?))
      .and_then(|bytes| bytes.checked_add(size_of::<CompiledIndexDocumentV1>() as u64))
      .ok_or_else(|| accounting_error("compiled source-value workspace bound overflowed"))
  }

  pub fn compile_source_values(&self, canonical_values: &[Vec<u8>]) -> IndexDefinitionResultV1<CompiledIndexDocumentV1> {
    if canonical_values.len() > self.value_store.definition().max_source_values_per_document as usize {
      return Err(error(
        IndexDefinitionErrorClassV1::ResourceLimit,
        "index_source_value_count_limit",
        "caller supplied more source values than the complete ValueStore definition permits",
      ));
    }
    let mut values = Vec::new();
    values.try_reserve_exact(canonical_values.len()).map_err(|source| {
      error(
        IndexDefinitionErrorClassV1::ResourceLimit,
        "index_source_value_reserve",
        format!("cannot reserve bounded source-value output: {source}"),
      )
    })?;
    let mut posting_count = 0u32;
    let mut canonical_posting_bytes = 0u64;
    let mut query_recheck_value_bytes = 0u64;

    for (source_value_ordinal, canonical_value) in canonical_values.iter().enumerate() {
      let source_value_ordinal = match u32::try_from(source_value_ordinal) {
        Ok(source_value_ordinal) => source_value_ordinal,
        Err(source) => {
          return Err(error(
            IndexDefinitionErrorClassV1::ResourceLimit,
            "index_source_ordinal_limit",
            format!("source value ordinal exceeds u32: {source}"),
          ));
        }
      };
      let value = decode_canonical_value(canonical_value, CanonicalValueBounds::SOURCE_VALUE)
        .map_err(|source| error(IndexDefinitionErrorClassV1::InvalidSourceValue, "index_source_value_invalid", source.to_string()))?;
      let compiled = self.converter.compile_source_value(&value).map_err(|source| {
        let class = match source.class() {
          IndexSemanticErrorClassV1::UnsupportedDefinition => IndexDefinitionErrorClassV1::UnsupportedDefinition,
          IndexSemanticErrorClassV1::InvalidSourceValue => IndexDefinitionErrorClassV1::InvalidSourceValue,
          IndexSemanticErrorClassV1::ResourceLimit => IndexDefinitionErrorClassV1::ResourceLimit,
          IndexSemanticErrorClassV1::MalformedPostingKey => IndexDefinitionErrorClassV1::SemanticMismatch,
        };
        error(class, source.code(), source.context())
      })?;
      if compiled.canonical_value != *canonical_value {
        return Err(error(
          IndexDefinitionErrorClassV1::SemanticMismatch,
          "index_source_value_rewritten",
          "converter changed an already-canonical ValueStore source value",
        ));
      }

      let compiled_posting_count = match u32::try_from(compiled.postings.len()) {
        Ok(compiled_posting_count) => compiled_posting_count,
        Err(source) => {
          return Err(error(
            IndexDefinitionErrorClassV1::ResourceLimit,
            "index_posting_count_limit",
            format!("one source value emitted more than u32 postings: {source}"),
          ));
        }
      };
      posting_count = posting_count
        .checked_add(compiled_posting_count)
        .ok_or_else(|| error(IndexDefinitionErrorClassV1::ResourceLimit, "index_posting_count_overflow", "posting count overflow"))?;
      query_recheck_value_bytes = query_recheck_value_bytes.checked_add(canonical_value.len() as u64).ok_or_else(|| {
        error(IndexDefinitionErrorClassV1::ResourceLimit, "index_recheck_bytes_overflow", "query recheck value bytes overflow")
      })?;
      for posting in &compiled.postings {
        canonical_posting_bytes = canonical_posting_bytes.checked_add(posting.posting_key.len() as u64).ok_or_else(|| {
          error(IndexDefinitionErrorClassV1::ResourceLimit, "index_posting_bytes_overflow", "canonical posting bytes overflow")
        })?;
      }

      if posting_count > self.field_definition.max_terms_per_document || posting_count > self.field_definition.max_postings_per_document {
        return Err(error(
          IndexDefinitionErrorClassV1::ResourceLimit,
          "index_posting_count_limit",
          "compiled posting count exceeds this FieldIndex definition",
        ));
      }
      if canonical_posting_bytes > self.field_definition.max_canonical_posting_bytes_per_document {
        return Err(error(
          IndexDefinitionErrorClassV1::ResourceLimit,
          "index_posting_bytes_limit",
          "compiled posting keys exceed this FieldIndex definition",
        ));
      }
      if query_recheck_value_bytes > self.field_definition.max_query_recheck_value_bytes {
        return Err(error(
          IndexDefinitionErrorClassV1::ResourceLimit,
          "index_recheck_bytes_limit",
          "canonical query recheck values exceed this FieldIndex definition",
        ));
      }
      values.push(CompiledDocumentValueV1 { source_value_ordinal, canonical_value: compiled.canonical_value, postings: compiled.postings });
    }

    Ok(CompiledIndexDocumentV1 { values, posting_count, canonical_posting_bytes, query_recheck_value_bytes })
  }
}

fn error(class: IndexDefinitionErrorClassV1, code: &'static str, context: impl Into<String>) -> IndexDefinitionErrorV1 {
  IndexDefinitionErrorV1 { class, code, context: context.into() }
}

fn decode_definitions<'value, 'field>(
  value_store_value: &'value [u8],
  field_definition_value: &'field [u8],
  hash_algorithm: HashAlgorithm,
) -> IndexDefinitionResultV1<(ValueStoreDefinitionV1<'value>, FieldIndexDefinitionV1<'field>)> {
  let value_store_definition = decode_value_store_definition(value_store_value, hash_algorithm).map_err(|source| {
    error(
      IndexDefinitionErrorClassV1::UnsupportedDefinition,
      "index_value_store_definition_invalid",
      format!("{}: {}", source.code(), source.context()),
    )
  })?;
  let field_definition = decode_field_index_definition(field_definition_value, hash_algorithm).map_err(|source| {
    error(
      IndexDefinitionErrorClassV1::UnsupportedDefinition,
      "index_field_definition_invalid",
      format!("{}: {}", source.code(), source.context()),
    )
  })?;
  Ok((value_store_definition, field_definition))
}

fn accounting_error(context: impl Into<String>) -> IndexDefinitionErrorV1 {
  error(IndexDefinitionErrorClassV1::ResourceLimit, "index_runtime_memory_accounting", context)
}
