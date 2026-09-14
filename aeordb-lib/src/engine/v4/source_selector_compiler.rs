//! Corrected configuration-source normalization into the frozen selector codec.
//! Alias capture, whole-configuration parsing and activation belong to callers.

use serde_json::Value;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::config_value::{CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1, CanonicalValueBounds, canonicalize_json};
use super::dependency::InvocationPolicyV1;
use super::index_semantic_registry::metadata_source_registry;
use super::parser_context_compiler::validate_policy;
use super::parser_registry_compiler::SemanticCompilationErrorV1;
use super::reader::{FormatError, MalformedInputClass};
use super::source_selector::{
  JsonPathSegmentV1, REGEX_COMPILED_SIZE_LIMIT, REGEX_DFA_SIZE_LIMIT, SourceSelectorWriteV1, encode_source_selector,
};

const SOURCE_PATH: &str = "<source-selector>";
const FIXED_WORKSPACE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SEGMENTS: usize = 1024;
const MAX_REGEX_PATTERN_BYTES: usize = 65_536 - 32 - 8;
const REGEX_SYNTAX_BYTES_PER_INPUT_BYTE: usize = 768;
type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;

#[derive(Clone)]
pub enum SourceSelectorInputV1<'a> {
  Metadata,
  /// None is the same one-segment source as an explicit field-name string.
  JsonPath(Option<&'a [Value]>),
  Mapper {
    /// Assigned by the already compiled parser/selector dependency context.
    dependency_ordinal: u32,
    arguments: Option<&'a [u8]>,
    policy: &'a InvocationPolicyV1,
  },
}

#[derive(Clone)]
pub struct SourceSelectorCompilationRequestV1<'a> {
  pub field_name: &'a str,
  pub source: SourceSelectorInputV1<'a>,
  pub maximum_source_bytes: usize,
  pub maximum_workspace_bytes: usize,
}

pub struct CompiledSourceSelectorV1 {
  field_name: String,
  selector: Vec<u8>,
  _memory: MemoryReservation,
}

impl CompiledSourceSelectorV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub fn selector(&self) -> &[u8] {
    &self.selector
  }
}

pub fn compile_source_selector_v1(
  request: SourceSelectorCompilationRequestV1<'_>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledSourceSelectorV1> {
  cancelled(is_cancelled)?;
  if request.field_name.is_empty() || request.field_name.len() > 4096 || request.field_name.contains('\0') {
    return Err(invalid("field name must contain 1..4096 UTF-8 bytes without NUL"));
  }
  let canonical_name = if request.field_name == "@file_name" { "@filename" } else { request.field_name };
  let metadata = metadata_source_registry().iter().find(|entry| entry.field_name == canonical_name);
  if canonical_name.starts_with('@') && metadata.is_none() {
    return Err(invalid("unknown metadata field"));
  }
  if metadata.is_some() != matches!(request.source, SourceSelectorInputV1::Metadata) {
    return Err(invalid("metadata field and source kind disagree"));
  }
  let input_bytes = source_length(&request)?;
  if input_bytes > request.maximum_source_bytes {
    return Err(resource("source exceeds the caller's byte limit"));
  }
  let regex_workspace = regex_workspace_bytes(&request)?;
  let node_bytes = match &request.source {
    SourceSelectorInputV1::Mapper { arguments, .. } => arguments
      .map_or(0, <[u8]>::len)
      .checked_add(1)
      .and_then(|nodes| nodes.checked_mul(CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1 as usize))
      .ok_or_else(|| resource("canonical argument node charge overflow"))?,
    _ => 0,
  };
  // JSON can retain many tiny nodes. One node per raw byte is conservative;
  // charge each at the shared 768-byte bound before generic deserialization.
  // Regex syntax is charged separately: a compiled-program cap does not bound
  // parser/HIR workspaces. Four times input +8MiB covers retained strings,
  // encoder recursion/scratch, segment vectors and bounded compiled regexes. This does
  // not claim that serde's internal allocations are universally fallible.
  let workspace = input_bytes
    .checked_mul(4)
    .and_then(|bytes| bytes.checked_add(node_bytes))
    .and_then(|bytes| bytes.checked_add(regex_workspace))
    .and_then(|bytes| bytes.checked_add(FIXED_WORKSPACE_BYTES))
    .ok_or_else(|| resource("source workspace charge overflow"))?;
  if workspace > request.maximum_workspace_bytes {
    return Err(resource("source workspace exceeds the caller's limit"));
  }
  let reservation =
    memory.reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Workload).map_err(|error| resource(error.to_string()))?;
  check(&reservation, is_cancelled)?;
  let selector = match request.source {
    SourceSelectorInputV1::Metadata => {
      let entry = metadata.ok_or_else(|| invalid("metadata entry disappeared after preflight"))?;
      encode_source_selector(SourceSelectorWriteV1::Metadata { metadata_id: entry.id }).map_err(format_error)?
    }
    SourceSelectorInputV1::JsonPath(source) => {
      let count = source.map_or(1, <[Value]>::len);
      let mut segments = Vec::new();
      segments.try_reserve_exact(count).map_err(|error| resource(error.to_string()))?;
      if let Some(source) = source {
        for value in source {
          check(&reservation, is_cancelled)?;
          segments.push(match value {
            Value::String(text) => string_segment(text)?,
            Value::Number(number) => {
              JsonPathSegmentV1::NumericIndex(number.as_u64().ok_or_else(|| invalid("numeric source index must be an unsigned integer"))?)
            }
            _ => return Err(invalid("source segments must be strings or unsigned integers")),
          });
        }
      } else {
        segments.push(string_segment(canonical_name)?);
      }
      check(&reservation, is_cancelled)?;
      encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &segments }).map_err(format_error)?
    }
    SourceSelectorInputV1::Mapper { dependency_ordinal, arguments, policy } => {
      if dependency_ordinal == 0 {
        return Err(invalid("mapper requires a nonzero captured dependency ordinal"));
      }
      validate_policy(policy, true)?;
      let arguments = match arguments {
        Some(bytes) => bytes,
        None => b"null",
      };
      let arguments = canonicalize_json(arguments, CanonicalValueBounds::CONFIG).map_err(format_error)?;
      check(&reservation, is_cancelled)?;
      encode_source_selector(SourceSelectorWriteV1::PluginMapper { dependency_ordinal, mapper_contract: 2, arguments: &arguments, policy })
        .map_err(format_error)?
    }
  };
  check(&reservation, is_cancelled)?;
  let mut field_name = String::new();
  field_name.try_reserve_exact(canonical_name.len()).map_err(|error| resource(error.to_string()))?;
  field_name.push_str(canonical_name);
  check(&reservation, is_cancelled)?;
  Ok(CompiledSourceSelectorV1 { field_name, selector, _memory: reservation })
}

fn source_length(request: &SourceSelectorCompilationRequestV1<'_>) -> Result<usize> {
  let mut bytes = request.field_name.len();
  match &request.source {
    SourceSelectorInputV1::Metadata | SourceSelectorInputV1::JsonPath(None) => {}
    SourceSelectorInputV1::Mapper { arguments, .. } => {
      bytes = bytes.checked_add(arguments.map_or(0, <[u8]>::len)).ok_or_else(|| resource("source length overflow"))?;
    }
    SourceSelectorInputV1::JsonPath(Some(source)) => {
      if source.len() > MAX_SEGMENTS {
        return Err(resource("source exceeds 1024 segments"));
      }
      for value in *source {
        let length = match value {
          Value::String(text) => text.len(),
          Value::Number(number) if number.as_u64().is_some() => 8,
          _ => return Err(invalid("source segments must be strings or unsigned integers")),
        };
        bytes = bytes.checked_add(8).and_then(|bytes| bytes.checked_add(length)).ok_or_else(|| resource("source length overflow"))?;
        if bytes > request.maximum_source_bytes {
          return Err(resource("source exceeds the caller's byte limit"));
        }
      }
    }
  }
  Ok(bytes)
}

fn string_segment(text: &str) -> Result<JsonPathSegmentV1<'_>> {
  if text.is_empty() {
    return Ok(JsonPathSegmentV1::FanOut);
  }
  if let Some((pattern, flags)) = regex_parts(text) {
    let case_insensitive = flags.contains('i');
    match regex::RegexBuilder::new(pattern)
      .case_insensitive(case_insensitive)
      .size_limit(REGEX_COMPILED_SIZE_LIMIT)
      .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
      .build()
    {
      Ok(_) => return Ok(JsonPathSegmentV1::Regex { pattern, case_insensitive }),
      Err(error) => {
        // Syntax failure deliberately selects the original literal bytes.
        // A valid but over-budget pattern is a resource failure, not a key.
        if !matches!(error, regex::Error::Syntax(_)) {
          return Err(resource(error.to_string()));
        }
      }
    }
  }
  Ok(JsonPathSegmentV1::ObjectKey(text))
}

fn regex_parts(text: &str) -> Option<(&str, &str)> {
  text.strip_prefix('/').and_then(|body| body.rsplit_once('/'))
}

fn regex_workspace_bytes(request: &SourceSelectorCompilationRequestV1<'_>) -> Result<usize> {
  let mut maximum_pattern_bytes = 0;
  let mut inspect = |text: &str| -> Result<()> {
    if let Some((pattern, _flags)) = regex_parts(text) {
      // Even the shortest canonical framing cannot contain this pattern.
      // Syntax fallback retains a longer literal, so it cannot rescue it.
      if pattern.len() > MAX_REGEX_PATTERN_BYTES {
        return Err(resource("regex pattern cannot fit the complete selector byte limit"));
      }
      maximum_pattern_bytes = maximum_pattern_bytes.max(pattern.len());
    }
    Ok(())
  };
  match &request.source {
    SourceSelectorInputV1::JsonPath(None) => inspect(request.field_name)?,
    SourceSelectorInputV1::JsonPath(Some(source)) => {
      for value in *source {
        if let Value::String(text) = value {
          inspect(text)?;
        }
      }
    }
    SourceSelectorInputV1::Metadata | SourceSelectorInputV1::Mapper { .. } => {}
  }
  // Only one pattern is built at a time. Conservative per-input-byte space
  // includes AST/HIR nodes, strings, construction stacks and growth slack;
  // the fixed charge separately covers bounded automata and Unicode tables.
  maximum_pattern_bytes.checked_mul(REGEX_SYNTAX_BYTES_PER_INPUT_BYTE).ok_or_else(|| resource("regex syntax workspace overflow"))
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
