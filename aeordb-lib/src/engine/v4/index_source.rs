use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use regex::Regex;

use crate::engine::HashAlgorithm;
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::file_name;

use super::config_value::{
  CanonicalConfigValueV1, CanonicalValueBounds, canonical_value_to_json, decode_canonical_value, encode_canonical_value,
};
use super::dependency::DependencyRecordV1;
use super::parser_plan::ParserCandidateV1;
use super::source_selector::{JsonPathSegmentV1, REGEX_COMPILED_SIZE_LIMIT, REGEX_DFA_SIZE_LIMIT, SourceSelectorKind};
use super::value_store::{ValueStoreDefinitionV1, ValueStoreSemanticFamily, decode_value_store_definition};

const VALUE_STORE_RUNTIME_FIXED_BYTES: u64 = 64 * 1_024;
const VALUE_STORE_EXTRACT_WORKSPACE_FIXED_BYTES: u64 = 4 * 1_024;
const JSON_REGEX_MAX_ESCAPE_EXPANSION: u64 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOperationalErrorClassV1 {
  Cancelled,
  DependencyUnavailable,
  HostFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOperationalErrorV1 {
  class: SourceOperationalErrorClassV1,
  code: &'static str,
  context: String,
}

impl SourceOperationalErrorV1 {
  pub fn host_failure(code: &'static str, context: impl Into<String>) -> Self {
    operational_error(SourceOperationalErrorClassV1::HostFailure, code, context)
  }

  pub fn class(&self) -> SourceOperationalErrorClassV1 {
    self.class
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for SourceOperationalErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for SourceOperationalErrorV1 {}

pub type SourceOperationalResultV1<T> = Result<T, SourceOperationalErrorV1>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceExtractionV1 {
  Missing,
  Values(Vec<Vec<u8>>),
  DeterministicUnindexable { code: &'static str, context: String },
}

#[derive(Debug, Clone, Copy)]
pub struct SourceDocumentV1<'a> {
  pub file_record: &'a FileRecord,
  pub parsed_value: Option<&'a CanonicalConfigValueV1>,
}

pub struct PluginMapperRequestV1<'a> {
  pub dependency_ordinal: u32,
  pub arguments: &'a [u8],
  pub document: &'a CanonicalConfigValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginMapperOutcomeV1 {
  Missing,
  Values(Vec<Vec<u8>>),
  DeterministicRejection { code: &'static str, context: String },
}

pub trait PluginMapperExecutorV1: Send + Sync {
  fn invoke(&self, request: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1>;
}

#[derive(Debug)]
enum CompiledJsonPathSegmentV1 {
  ObjectKey(String),
  NumericIndex(u64),
  FanOut,
  Regex(Regex),
}

#[derive(Debug)]
enum SelectorFrameV1<'a> {
  Evaluate {
    value: &'a CanonicalConfigValueV1,
    segment_index: usize,
  },
  ArrayCandidates {
    values: &'a [CanonicalConfigValueV1],
    next_index: usize,
    segment_index: usize,
  },
  MapCandidates {
    iterator: std::iter::Peekable<std::collections::btree_map::Iter<'a, String, CanonicalConfigValueV1>>,
    segment_index: usize,
  },
}

enum RegexTextErrorV1 {
  Deterministic(SourceExtractionV1),
  Operational(SourceOperationalErrorV1),
}

#[derive(Debug)]
pub struct ValueStoreRuntimeV1<'a> {
  definition: ValueStoreDefinitionV1<'a>,
  hash_width: usize,
  json_segments: Vec<CompiledJsonPathSegmentV1>,
}

impl<'a> ValueStoreRuntimeV1<'a> {
  pub fn from_encoded(value: &'a [u8], hash_algorithm: HashAlgorithm) -> SourceOperationalResultV1<Self> {
    let definition = decode_value_store_definition(value, hash_algorithm).map_err(|source| {
      operational_error(
        SourceOperationalErrorClassV1::HostFailure,
        "value_store_definition_invalid",
        format!("{}: {}", source.code(), source.context()),
      )
    })?;
    Self::from_definition(definition, hash_algorithm.hash_length())
  }

  pub(crate) fn from_definition(definition: ValueStoreDefinitionV1<'a>, hash_width: usize) -> SourceOperationalResultV1<Self> {
    let mut json_segments = Vec::new();
    json_segments.try_reserve_exact(definition.selector.segments.len()).map_err(|source| {
      operational_error(
        SourceOperationalErrorClassV1::HostFailure,
        "selector_segment_reserve",
        format!("cannot reserve bounded compiled selector segments: {source}"),
      )
    })?;
    for segment in &definition.selector.segments {
      let segment = match segment {
        JsonPathSegmentV1::ObjectKey(key) => CompiledJsonPathSegmentV1::ObjectKey((*key).to_string()),
        JsonPathSegmentV1::NumericIndex(index) => CompiledJsonPathSegmentV1::NumericIndex(*index),
        JsonPathSegmentV1::FanOut => CompiledJsonPathSegmentV1::FanOut,
        JsonPathSegmentV1::Regex { pattern, case_insensitive } => {
          let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(*case_insensitive)
            .size_limit(REGEX_COMPILED_SIZE_LIMIT)
            .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
            .build()
            .map_err(|source| {
              operational_error(SourceOperationalErrorClassV1::HostFailure, "selector_regex_compile", source.to_string())
            })?;
          CompiledJsonPathSegmentV1::Regex(regex)
        }
      };
      json_segments.push(segment);
    }
    Ok(Self { definition, hash_width, json_segments })
  }

  pub(crate) fn maximum_retained_bytes_for_definition(definition: &ValueStoreDefinitionV1<'_>) -> SourceOperationalResultV1<u64> {
    let mut bytes = VALUE_STORE_RUNTIME_FIXED_BYTES;
    bytes = checked_runtime_add(bytes, definition.value_store_id.capacity(), "ValueStore identity")?;
    bytes = checked_runtime_array::<JsonPathSegmentV1<'_>>(bytes, definition.selector.segments.capacity(), "decoded selector segments")?;
    bytes = checked_runtime_array::<ParserCandidateV1<'_>>(bytes, definition.parser_plan.candidates.capacity(), "parser candidates")?;
    bytes = checked_runtime_array::<DependencyRecordV1<'_>>(bytes, definition.dependencies.records.capacity(), "dependency records")?;
    bytes = checked_runtime_array::<CompiledJsonPathSegmentV1>(bytes, definition.selector.segments.len(), "compiled selector segments")?;
    for segment in &definition.selector.segments {
      match segment {
        JsonPathSegmentV1::ObjectKey(key) => {
          bytes = checked_runtime_add(bytes, key.len(), "compiled selector object key")?;
        }
        JsonPathSegmentV1::Regex { pattern, .. } => {
          bytes = checked_runtime_add(bytes, pattern.len(), "compiled selector regex pattern")?;
          bytes = bytes
            .checked_add(REGEX_COMPILED_SIZE_LIMIT as u64)
            .and_then(|value| value.checked_add(REGEX_DFA_SIZE_LIMIT as u64))
            .ok_or_else(|| runtime_accounting_error("compiled selector regex memory overflowed"))?;
        }
        JsonPathSegmentV1::NumericIndex(_) | JsonPathSegmentV1::FanOut => {}
      }
    }
    Ok(bytes)
  }

  pub(crate) fn maximum_extract_workspace_bytes(&self) -> SourceOperationalResultV1<u64> {
    if self.definition.selector.kind != SourceSelectorKind::JsonPath {
      return Ok(0);
    }
    // The depth-first walker retains at most one iterator continuation and one
    // selected child per selector depth. Total work bounds runtime, not the
    // number of simultaneously live frames.
    let frame_count = u64::try_from(self.json_segments.len())
      .map_err(|source| runtime_accounting_error(format!("selector depth does not fit u64: {source}")))?
      .checked_mul(2)
      .and_then(|count| count.checked_add(1))
      .ok_or_else(|| runtime_accounting_error("selector frame count overflowed"))?;
    let frame_bytes = frame_count
      .checked_mul(std::mem::size_of::<SelectorFrameV1<'_>>() as u64)
      .ok_or_else(|| runtime_accounting_error("selector frame workspace overflowed"))?;
    let regex_bytes = if self.json_segments.iter().any(|segment| matches!(segment, CompiledJsonPathSegmentV1::Regex(_))) {
      let parser_response_bytes = self
        .definition
        .parser_plan
        .candidates
        .iter()
        .map(|candidate| candidate.policy.max_response_bytes)
        .max()
        .ok_or_else(|| runtime_accounting_error("JSON selector has no parser response bound"))?;
      let maximum_serialized_bytes = parser_response_bytes
        .checked_mul(JSON_REGEX_MAX_ESCAPE_EXPANSION)
        .ok_or_else(|| runtime_accounting_error("regex candidate serialization bound overflowed"))?;
      self.definition.max_selector_examined_bytes_per_document.min(maximum_serialized_bytes)
    } else {
      0
    };
    VALUE_STORE_EXTRACT_WORKSPACE_FIXED_BYTES
      .checked_add(frame_bytes)
      .and_then(|bytes| bytes.checked_add(regex_bytes))
      .ok_or_else(|| runtime_accounting_error("selector extraction workspace overflowed"))
  }

  pub fn definition(&self) -> &ValueStoreDefinitionV1<'a> {
    &self.definition
  }

  pub fn extract(
    &self,
    document: SourceDocumentV1<'_>,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> SourceOperationalResultV1<SourceExtractionV1> {
    if is_cancelled() {
      return Err(cancelled());
    }
    if self.definition.max_document_input_bytes > 0 && document.file_record.total_size > self.definition.max_document_input_bytes {
      return Ok(deterministic_unindexable(
        "source_document_input_limit",
        format!(
          "document input has {} bytes; this definition permits {}",
          document.file_record.total_size, self.definition.max_document_input_bytes
        ),
      ));
    }
    match self.definition.selector.kind {
      SourceSelectorKind::Metadata => Ok(self.extract_metadata(document.file_record)),
      SourceSelectorKind::JsonPath => {
        let parsed_value = document.parsed_value.ok_or_else(|| {
          operational_error(
            SourceOperationalErrorClassV1::DependencyUnavailable,
            "parsed_document_unavailable",
            "JSON-path extraction requires the exact parser result",
          )
        })?;
        self.extract_json_path(parsed_value, is_cancelled)
      }
      SourceSelectorKind::PluginMapper => self.extract_plugin_mapper(document, mapper, is_cancelled),
      SourceSelectorKind::AlwaysMissingV0 => Ok(SourceExtractionV1::Missing),
    }
  }

  fn extract_metadata(&self, record: &FileRecord) -> SourceExtractionV1 {
    let metadata_id = match self.definition.selector.metadata_id {
      Some(metadata_id) => metadata_id,
      None => return deterministic_unindexable("metadata_source_missing", "decoded metadata selector has no metadata ID"),
    };
    let value = match (self.definition.semantic_family, metadata_id) {
      (ValueStoreSemanticFamily::CorrectedV1, 1) => CanonicalConfigValueV1::String(record.path.clone()),
      (ValueStoreSemanticFamily::CorrectedV1, 2) => CanonicalConfigValueV1::String(file_name_or_empty(&record.path).to_string()),
      (ValueStoreSemanticFamily::CorrectedV1, 3) => CanonicalConfigValueV1::String(file_extension(&record.path).to_string()),
      (ValueStoreSemanticFamily::CorrectedV1, 4) => match &record.content_type {
        Some(value) => CanonicalConfigValueV1::String(value.clone()),
        None => CanonicalConfigValueV1::Null,
      },
      (ValueStoreSemanticFamily::CorrectedV1, 5) => CanonicalConfigValueV1::Unsigned(record.total_size),
      (ValueStoreSemanticFamily::CorrectedV1, 6) => CanonicalConfigValueV1::Signed(record.created_at),
      (ValueStoreSemanticFamily::CorrectedV1, 7) => CanonicalConfigValueV1::Signed(record.updated_at),
      (ValueStoreSemanticFamily::CorrectedV1, 8) => {
        if record.content_hash.len() != self.hash_width {
          return deterministic_unindexable(
            "file_record_migration_required",
            format!("full content hash has {} bytes; this definition requires {}", record.content_hash.len(), self.hash_width),
          );
        }
        CanonicalConfigValueV1::Bytes(record.content_hash.clone())
      }
      (ValueStoreSemanticFamily::MigrationV0, _) => {
        return self.extract_legacy_metadata(record, metadata_id);
      }
      (_, id) => return deterministic_unindexable("metadata_source_unknown", format!("unknown metadata ID {id}")),
    };
    self.encode_values(vec![value])
  }

  fn extract_legacy_metadata(&self, record: &FileRecord, metadata_id: u16) -> SourceExtractionV1 {
    let bytes = match metadata_id {
      1 => record.path.as_bytes().to_vec(),
      2 => file_name_or_empty(&record.path).as_bytes().to_vec(),
      3 => file_extension(&record.path).as_bytes().to_vec(),
      4 => match &record.content_type {
        Some(value) => value.as_bytes().to_vec(),
        None => Vec::new(),
      },
      5 => record.total_size.to_be_bytes().to_vec(),
      6 => record.created_at.to_be_bytes().to_vec(),
      7 => record.updated_at.to_be_bytes().to_vec(),
      8 => {
        if record.content_hash.len() != self.hash_width {
          return deterministic_unindexable(
            "file_record_migration_required",
            format!("full content hash has {} bytes; this definition requires {}", record.content_hash.len(), self.hash_width),
          );
        }
        hex::encode(&record.content_hash).into_bytes()
      }
      id => return deterministic_unindexable("metadata_source_unknown", format!("unknown metadata ID {id}")),
    };
    self.encode_values(vec![CanonicalConfigValueV1::Bytes(bytes)])
  }

  fn extract_json_path(
    &self,
    root: &CanonicalConfigValueV1,
    is_cancelled: &dyn Fn() -> bool,
  ) -> SourceOperationalResultV1<SourceExtractionV1> {
    let mut budget = SelectorBudgetV1::new(&self.definition);
    let mut stack = Vec::new();
    reserve_selector_frames(&mut stack, 1)?;
    stack.push(SelectorFrameV1::Evaluate { value: root, segment_index: 0 });
    let mut values = Vec::new();
    let mut total_value_bytes = 0u64;

    while let Some(frame) = stack.pop() {
      if is_cancelled() {
        return Err(cancelled());
      }
      match frame {
        SelectorFrameV1::Evaluate { value, segment_index } => {
          if segment_index == self.json_segments.len() {
            let canonical = match self.encode_selected_value(value) {
              Ok(canonical) => canonical,
              Err(outcome) => return Ok(outcome),
            };
            let next_count = values.len().checked_add(1).ok_or_else(|| {
              operational_error(SourceOperationalErrorClassV1::HostFailure, "source_value_count_overflow", "source value count overflow")
            })?;
            total_value_bytes = match total_value_bytes.checked_add(canonical.len() as u64) {
              Some(total) => total,
              None => return Ok(deterministic_unindexable("source_value_bytes_overflow", "canonical source byte count overflow")),
            };
            if next_count > self.definition.max_source_values_per_document as usize
              || total_value_bytes > self.definition.max_canonical_source_bytes_per_document
            {
              let code = if values.len() >= self.definition.max_source_values_per_document as usize {
                "source_value_count_limit"
              } else {
                "source_value_bytes_limit"
              };
              return Ok(deterministic_unindexable(code, "terminal source values exceed this ValueStore definition"));
            }
            values.try_reserve(1).map_err(|source| {
              operational_error(
                SourceOperationalErrorClassV1::HostFailure,
                "source_value_reserve",
                format!("cannot reserve bounded source value: {source}"),
              )
            })?;
            values.push(canonical);
            continue;
          }

          if let Err(outcome) = budget.charge_work(1) {
            return Ok(outcome);
          }
          let next_segment = segment_index + 1;
          match &self.json_segments[segment_index] {
            CompiledJsonPathSegmentV1::ObjectKey(key) => {
              if let CanonicalConfigValueV1::Map(map) = value {
                if let Some(child) = map.get(key) {
                  reserve_selector_frames(&mut stack, 1)?;
                  stack.push(SelectorFrameV1::Evaluate { value: child, segment_index: next_segment });
                }
              }
            }
            CompiledJsonPathSegmentV1::NumericIndex(index) => match value {
              CanonicalConfigValueV1::Array(array) => {
                let candidate_index = *index as usize;
                if candidate_index as u64 == *index {
                  if let Some(child) = array.get(candidate_index) {
                    reserve_selector_frames(&mut stack, 1)?;
                    stack.push(SelectorFrameV1::Evaluate { value: child, segment_index: next_segment });
                  }
                }
              }
              CanonicalConfigValueV1::Map(map) => {
                if let Some(child) = map.get(&index.to_string()) {
                  reserve_selector_frames(&mut stack, 1)?;
                  stack.push(SelectorFrameV1::Evaluate { value: child, segment_index: next_segment });
                }
              }
              _ => {}
            },
            CompiledJsonPathSegmentV1::FanOut | CompiledJsonPathSegmentV1::Regex(_) => match value {
              CanonicalConfigValueV1::Array(array) if !array.is_empty() => {
                reserve_selector_frames(&mut stack, 1)?;
                stack.push(SelectorFrameV1::ArrayCandidates { values: array, next_index: 0, segment_index });
              }
              CanonicalConfigValueV1::Map(map) if !map.is_empty() => {
                reserve_selector_frames(&mut stack, 1)?;
                stack.push(SelectorFrameV1::MapCandidates { iterator: map.iter().peekable(), segment_index });
              }
              _ => {}
            },
          }
        }
        SelectorFrameV1::ArrayCandidates { values: candidates, next_index, segment_index } => {
          let Some(candidate) = candidates.get(next_index) else {
            continue;
          };
          if let Err(outcome) = budget.charge_work(1) {
            return Ok(outcome);
          }
          let matches = match &self.json_segments[segment_index] {
            CompiledJsonPathSegmentV1::FanOut => true,
            CompiledJsonPathSegmentV1::Regex(regex) => {
              let text = match canonical_regex_text(candidate, budget.remaining_examined_bytes()) {
                Ok(text) => text,
                Err(RegexTextErrorV1::Deterministic(outcome)) => return Ok(outcome),
                Err(RegexTextErrorV1::Operational(error)) => return Err(error),
              };
              if let Err(outcome) = budget.charge_examined(text.len() as u64) {
                return Ok(outcome);
              }
              regex.is_match(&text)
            }
            _ => return Err(invalid_selector_frame()),
          };
          let has_more = next_index + 1 < candidates.len();
          reserve_selector_frames(&mut stack, usize::from(has_more) + usize::from(matches))?;
          if has_more {
            stack.push(SelectorFrameV1::ArrayCandidates { values: candidates, next_index: next_index + 1, segment_index });
          }
          if matches {
            stack.push(SelectorFrameV1::Evaluate { value: candidate, segment_index: segment_index + 1 });
          }
        }
        SelectorFrameV1::MapCandidates { mut iterator, segment_index } => {
          let Some((key, candidate)) = iterator.next() else {
            continue;
          };
          if let Err(outcome) = budget.charge_work(1) {
            return Ok(outcome);
          }
          let matches = match &self.json_segments[segment_index] {
            CompiledJsonPathSegmentV1::FanOut => true,
            CompiledJsonPathSegmentV1::Regex(regex) => {
              if let Err(outcome) = budget.charge_examined(key.len() as u64) {
                return Ok(outcome);
              }
              regex.is_match(key)
            }
            _ => return Err(invalid_selector_frame()),
          };
          let has_more = iterator.peek().is_some();
          reserve_selector_frames(&mut stack, usize::from(has_more) + usize::from(matches))?;
          if has_more {
            stack.push(SelectorFrameV1::MapCandidates { iterator, segment_index });
          }
          if matches {
            stack.push(SelectorFrameV1::Evaluate { value: candidate, segment_index: segment_index + 1 });
          }
        }
      }
    }

    if values.is_empty() {
      Ok(SourceExtractionV1::Missing)
    } else {
      Ok(SourceExtractionV1::Values(values))
    }
  }

  fn extract_plugin_mapper(
    &self,
    document: SourceDocumentV1<'_>,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> SourceOperationalResultV1<SourceExtractionV1> {
    let mapper = mapper.ok_or_else(|| {
      operational_error(
        SourceOperationalErrorClassV1::DependencyUnavailable,
        "plugin_mapper_unavailable",
        "the exact pinned plugin mapper executor is unavailable",
      )
    })?;
    let parsed_value = document.parsed_value.ok_or_else(|| {
      operational_error(
        SourceOperationalErrorClassV1::DependencyUnavailable,
        "parsed_document_unavailable",
        "plugin mapper extraction requires the exact parser result",
      )
    })?;
    if is_cancelled() {
      return Err(cancelled());
    }
    let dependency_ordinal = self.definition.selector.dependency_ordinal.ok_or_else(|| {
      operational_error(
        SourceOperationalErrorClassV1::HostFailure,
        "plugin_mapper_dependency_missing",
        "decoded mapper selector has no dependency ordinal",
      )
    })?;
    let arguments = self.definition.selector.arguments.ok_or_else(|| {
      operational_error(
        SourceOperationalErrorClassV1::HostFailure,
        "plugin_mapper_arguments_missing",
        "decoded mapper selector has no canonical arguments",
      )
    })?;
    let outcome = mapper.invoke(PluginMapperRequestV1 { dependency_ordinal, arguments, document: parsed_value })?;
    if is_cancelled() {
      return Err(cancelled());
    }
    match outcome {
      PluginMapperOutcomeV1::Missing => Ok(SourceExtractionV1::Missing),
      PluginMapperOutcomeV1::DeterministicRejection { code, context } => Ok(SourceExtractionV1::DeterministicUnindexable { code, context }),
      PluginMapperOutcomeV1::Values(values) => Ok(self.validate_mapper_values(values)),
    }
  }

  fn validate_mapper_values(&self, values: Vec<Vec<u8>>) -> SourceExtractionV1 {
    if values.is_empty() {
      return deterministic_unindexable("plugin_mapper_empty_values", "corrected mapper values must be nonempty or explicitly missing");
    }
    if values.len() > self.definition.max_source_values_per_document as usize {
      return deterministic_unindexable("source_value_count_limit", "mapper source-value count exceeds this ValueStore definition");
    }
    let mut total = 0u64;
    for value in &values {
      if let Err(source) = decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE) {
        return deterministic_unindexable("plugin_mapper_invalid_value", source.to_string());
      }
      total = match total.checked_add(value.len() as u64) {
        Some(total) => total,
        None => return deterministic_unindexable("source_value_bytes_overflow", "mapper source bytes overflow"),
      };
      if total > self.definition.max_canonical_source_bytes_per_document {
        return deterministic_unindexable("source_value_bytes_limit", "mapper source bytes exceed this ValueStore definition");
      }
    }
    SourceExtractionV1::Values(values)
  }

  fn encode_values(&self, values: Vec<CanonicalConfigValueV1>) -> SourceExtractionV1 {
    let mut encoded = Vec::with_capacity(values.len());
    let mut total = 0u64;
    for value in values {
      let value = match encode_canonical_value(&value, CanonicalValueBounds::SOURCE_VALUE) {
        Ok(value) => value,
        Err(source) => return deterministic_unindexable("source_value_encode", source.to_string()),
      };
      total = match total.checked_add(value.len() as u64) {
        Some(total) => total,
        None => return deterministic_unindexable("source_value_bytes_overflow", "metadata source bytes overflow"),
      };
      if encoded.len() + 1 > self.definition.max_source_values_per_document as usize {
        return deterministic_unindexable("source_value_count_limit", "metadata source-value count exceeds this ValueStore definition");
      }
      if total > self.definition.max_canonical_source_bytes_per_document {
        return deterministic_unindexable("source_value_bytes_limit", "metadata source value exceeds this ValueStore definition");
      }
      encoded.push(value);
    }
    SourceExtractionV1::Values(encoded)
  }

  fn encode_selected_value(&self, value: &CanonicalConfigValueV1) -> Result<Vec<u8>, SourceExtractionV1> {
    match self.definition.semantic_family {
      ValueStoreSemanticFamily::CorrectedV1 => encode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE)
        .map_err(|source| deterministic_unindexable("source_value_encode", source.to_string())),
      ValueStoreSemanticFamily::MigrationV0 => {
        let value = CanonicalConfigValueV1::Bytes(legacy_source_bytes(value)?);
        encode_canonical_value(&value, CanonicalValueBounds::SOURCE_VALUE)
          .map_err(|source| deterministic_unindexable("source_value_encode", source.to_string()))
      }
    }
  }
}

fn checked_runtime_array<T>(bytes: u64, count: usize, label: &'static str) -> SourceOperationalResultV1<u64> {
  let allocation =
    count.checked_mul(std::mem::size_of::<T>()).ok_or_else(|| runtime_accounting_error(format!("{label} allocation overflowed")))?;
  checked_runtime_add(bytes, allocation, label)
}

fn checked_runtime_add(bytes: u64, allocation: usize, label: &'static str) -> SourceOperationalResultV1<u64> {
  let allocation = u64::try_from(allocation).map_err(|source| runtime_accounting_error(format!("{label} does not fit u64: {source}")))?;
  bytes.checked_add(allocation).ok_or_else(|| runtime_accounting_error(format!("{label} memory overflowed")))
}

fn runtime_accounting_error(context: impl Into<String>) -> SourceOperationalErrorV1 {
  operational_error(SourceOperationalErrorClassV1::HostFailure, "value_store_runtime_accounting", context)
}

struct SelectorBudgetV1 {
  work: u64,
  examined_bytes: u64,
  maximum_work: u64,
  maximum_examined_bytes: u64,
}

impl SelectorBudgetV1 {
  fn new(definition: &ValueStoreDefinitionV1<'_>) -> Self {
    Self {
      work: 0,
      examined_bytes: 0,
      maximum_work: definition.max_selector_work_items_per_document,
      maximum_examined_bytes: definition.max_selector_examined_bytes_per_document,
    }
  }

  fn charge_work(&mut self, amount: u64) -> Result<(), SourceExtractionV1> {
    self.work =
      self.work.checked_add(amount).ok_or_else(|| deterministic_unindexable("selector_work_overflow", "selector work counter overflow"))?;
    if self.work > self.maximum_work {
      return Err(deterministic_unindexable("selector_work_limit", "JSON selector exceeded its work-item limit"));
    }
    Ok(())
  }

  fn charge_examined(&mut self, amount: u64) -> Result<(), SourceExtractionV1> {
    self.examined_bytes = self
      .examined_bytes
      .checked_add(amount)
      .ok_or_else(|| deterministic_unindexable("selector_examined_bytes_overflow", "selector examined-byte counter overflow"))?;
    if self.examined_bytes > self.maximum_examined_bytes {
      return Err(deterministic_unindexable("selector_examined_bytes_limit", "JSON selector exceeded its examined-byte limit"));
    }
    Ok(())
  }

  fn remaining_examined_bytes(&self) -> usize {
    let remaining = self.maximum_examined_bytes.saturating_sub(self.examined_bytes);
    if usize::BITS >= u64::BITS || remaining <= usize::MAX as u64 {
      remaining as usize
    } else {
      usize::MAX
    }
  }
}

fn canonical_regex_text(value: &CanonicalConfigValueV1, maximum_length: usize) -> Result<Cow<'_, str>, RegexTextErrorV1> {
  if let CanonicalConfigValueV1::String(value) = value {
    if value.len() > maximum_length {
      return Err(RegexTextErrorV1::Deterministic(deterministic_unindexable(
        "selector_examined_bytes_limit",
        "array regex candidate exceeds remaining examined-byte budget",
      )));
    }
    return Ok(Cow::Borrowed(value));
  }
  let length = compact_json_length(value, maximum_length).map_err(RegexTextErrorV1::Deterministic)?;
  let mut text = String::new();
  text.try_reserve_exact(length).map_err(|source| {
    RegexTextErrorV1::Operational(operational_error(
      SourceOperationalErrorClassV1::HostFailure,
      "selector_regex_text_reserve",
      format!("cannot reserve bounded regex candidate text: {source}"),
    ))
  })?;
  write_compact_json(value, &mut text).map_err(RegexTextErrorV1::Deterministic)?;
  debug_assert_eq!(text.len(), length);
  Ok(Cow::Owned(text))
}

fn compact_json_length(value: &CanonicalConfigValueV1, maximum_length: usize) -> Result<usize, SourceExtractionV1> {
  let length = match value {
    CanonicalConfigValueV1::Null => 4,
    CanonicalConfigValueV1::Boolean(false) => 5,
    CanonicalConfigValueV1::Boolean(true) => 4,
    CanonicalConfigValueV1::Signed(value) => value.to_string().len(),
    CanonicalConfigValueV1::Unsigned(value) => value.to_string().len(),
    CanonicalConfigValueV1::FloatBits(bits) => serde_json::to_string(&f64::from_bits(*bits))
      .map_err(|source| deterministic_unindexable("selector_regex_value", source.to_string()))?
      .len(),
    CanonicalConfigValueV1::String(value) => escaped_json_string_length(value, maximum_length)?,
    CanonicalConfigValueV1::Bytes(_) => {
      return Err(deterministic_unindexable("selector_regex_value", "canonical byte strings do not have a JSON representation"));
    }
    CanonicalConfigValueV1::Array(values) => {
      let mut length = 2usize;
      for (index, value) in values.iter().enumerate() {
        if index > 0 {
          length = checked_json_length_add(length, 1, maximum_length)?;
        }
        length = checked_json_length_add(length, compact_json_length(value, maximum_length)?, maximum_length)?;
      }
      length
    }
    CanonicalConfigValueV1::Map(values) => {
      let mut length = 2usize;
      for (index, (key, value)) in values.iter().enumerate() {
        if index > 0 {
          length = checked_json_length_add(length, 1, maximum_length)?;
        }
        length = checked_json_length_add(length, escaped_json_string_length(key, maximum_length)?, maximum_length)?;
        length = checked_json_length_add(length, 1, maximum_length)?;
        length = checked_json_length_add(length, compact_json_length(value, maximum_length)?, maximum_length)?;
      }
      length
    }
  };
  if length > maximum_length {
    return Err(regex_text_limit());
  }
  Ok(length)
}

fn escaped_json_string_length(value: &str, maximum_length: usize) -> Result<usize, SourceExtractionV1> {
  let mut length = 2usize;
  for character in value.chars() {
    let encoded_length = match character {
      '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
      '\u{0000}'..='\u{001f}' => 6,
      character => character.len_utf8(),
    };
    length = checked_json_length_add(length, encoded_length, maximum_length)?;
  }
  Ok(length)
}

fn checked_json_length_add(current: usize, additional: usize, maximum_length: usize) -> Result<usize, SourceExtractionV1> {
  let length = current
    .checked_add(additional)
    .ok_or_else(|| deterministic_unindexable("selector_examined_bytes_overflow", "regex candidate JSON length overflow"))?;
  if length > maximum_length {
    return Err(regex_text_limit());
  }
  Ok(length)
}

fn regex_text_limit() -> SourceExtractionV1 {
  deterministic_unindexable("selector_examined_bytes_limit", "array regex candidate exceeds remaining examined-byte budget")
}

fn write_compact_json(value: &CanonicalConfigValueV1, output: &mut String) -> Result<(), SourceExtractionV1> {
  match value {
    CanonicalConfigValueV1::Null => output.push_str("null"),
    CanonicalConfigValueV1::Boolean(false) => output.push_str("false"),
    CanonicalConfigValueV1::Boolean(true) => output.push_str("true"),
    CanonicalConfigValueV1::Signed(value) => output.push_str(&value.to_string()),
    CanonicalConfigValueV1::Unsigned(value) => output.push_str(&value.to_string()),
    CanonicalConfigValueV1::FloatBits(bits) => {
      let value = serde_json::to_string(&f64::from_bits(*bits))
        .map_err(|source| deterministic_unindexable("selector_regex_value", source.to_string()))?;
      output.push_str(&value);
    }
    CanonicalConfigValueV1::String(value) => write_json_string(value, output),
    CanonicalConfigValueV1::Bytes(_) => {
      return Err(deterministic_unindexable("selector_regex_value", "canonical byte strings do not have a JSON representation"));
    }
    CanonicalConfigValueV1::Array(values) => {
      output.push('[');
      for (index, value) in values.iter().enumerate() {
        if index > 0 {
          output.push(',');
        }
        write_compact_json(value, output)?;
      }
      output.push(']');
    }
    CanonicalConfigValueV1::Map(values) => {
      output.push('{');
      for (index, (key, value)) in values.iter().enumerate() {
        if index > 0 {
          output.push(',');
        }
        write_json_string(key, output);
        output.push(':');
        write_compact_json(value, output)?;
      }
      output.push('}');
    }
  }
  Ok(())
}

fn write_json_string(value: &str, output: &mut String) {
  const HEX: &[u8; 16] = b"0123456789abcdef";
  output.push('"');
  for character in value.chars() {
    match character {
      '"' => output.push_str("\\\""),
      '\\' => output.push_str("\\\\"),
      '\u{0008}' => output.push_str("\\b"),
      '\u{000c}' => output.push_str("\\f"),
      '\n' => output.push_str("\\n"),
      '\r' => output.push_str("\\r"),
      '\t' => output.push_str("\\t"),
      '\u{0000}'..='\u{001f}' => {
        let value = character as u8;
        output.push_str("\\u00");
        output.push(HEX[(value >> 4) as usize] as char);
        output.push(HEX[(value & 0x0f) as usize] as char);
      }
      character => output.push(character),
    }
  }
  output.push('"');
}

fn legacy_source_bytes(value: &CanonicalConfigValueV1) -> Result<Vec<u8>, SourceExtractionV1> {
  match value {
    CanonicalConfigValueV1::Null => Ok(Vec::new()),
    CanonicalConfigValueV1::Boolean(value) => Ok(vec![u8::from(*value)]),
    CanonicalConfigValueV1::Signed(value) => Ok(value.to_be_bytes().to_vec()),
    CanonicalConfigValueV1::Unsigned(value) => Ok(value.to_be_bytes().to_vec()),
    CanonicalConfigValueV1::FloatBits(value) => Ok(value.to_be_bytes().to_vec()),
    CanonicalConfigValueV1::String(value) => Ok(value.as_bytes().to_vec()),
    CanonicalConfigValueV1::Bytes(value) => Ok(value.clone()),
    CanonicalConfigValueV1::Array(_) | CanonicalConfigValueV1::Map(_) => {
      let canonical = encode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE)
        .map_err(|source| deterministic_unindexable("legacy_source_encode", source.to_string()))?;
      canonical_value_to_json(&canonical, CanonicalValueBounds::SOURCE_VALUE, CanonicalValueBounds::SOURCE_VALUE.maximum_value_length)
        .map_err(|source| deterministic_unindexable("legacy_source_json", source.to_string()))
    }
  }
}

fn file_extension(path: &str) -> &str {
  let Some(name) = file_name(path) else {
    return "";
  };
  let Some((_, extension)) = name.rsplit_once('.') else {
    return "";
  };
  extension
}

fn file_name_or_empty(path: &str) -> &str {
  let Some(name) = file_name(path) else {
    return "";
  };
  name
}

fn reserve_selector_frames(stack: &mut Vec<SelectorFrameV1<'_>>, additional: usize) -> SourceOperationalResultV1<()> {
  stack.try_reserve(additional).map_err(|source| {
    operational_error(
      SourceOperationalErrorClassV1::HostFailure,
      "selector_stack_reserve",
      format!("cannot reserve bounded selector traversal state: {source}"),
    )
  })
}

fn invalid_selector_frame() -> SourceOperationalErrorV1 {
  operational_error(
    SourceOperationalErrorClassV1::HostFailure,
    "selector_frame_kind",
    "selector traversal frame disagrees with its decoded segment",
  )
}

fn deterministic_unindexable(code: &'static str, context: impl Into<String>) -> SourceExtractionV1 {
  SourceExtractionV1::DeterministicUnindexable { code, context: context.into() }
}

fn cancelled() -> SourceOperationalErrorV1 {
  operational_error(SourceOperationalErrorClassV1::Cancelled, "source_extraction_cancelled", "source extraction was cancelled")
}

fn operational_error(class: SourceOperationalErrorClassV1, code: &'static str, context: impl Into<String>) -> SourceOperationalErrorV1 {
  SourceOperationalErrorV1 { class, code, context: context.into() }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn json_regex_workspace_accounts_for_worst_case_escape_expansion() {
    let encoded = include_bytes!("../../../spec/fixtures/v4/value-store-definition-v1/avst-blake3-256-json-corrected-valid.bin");
    let runtime = ValueStoreRuntimeV1::from_encoded(encoded, HashAlgorithm::Blake3_256).unwrap();
    let frame_count = runtime.json_segments.len() as u64 * 2 + 1;
    let frame_bytes = frame_count * std::mem::size_of::<SelectorFrameV1<'_>>() as u64;

    assert_eq!(
      runtime.maximum_extract_workspace_bytes().unwrap(),
      VALUE_STORE_EXTRACT_WORKSPACE_FIXED_BYTES + frame_bytes + 6 * 4 * 1_024 * 1_024,
    );
  }
}
