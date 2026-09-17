//! Strict corrected source schema. Legacy aliases remain in the v0 adapter.
use std::collections::BTreeMap;

use super::config_value::{CanonicalConfigValueV1 as Value, CanonicalValueBounds, canonical_value_to_json, encode_canonical_value};
use super::dependency::{InvocationPolicyKind, InvocationPolicyV1};
use super::index_configuration_compiler::{Result, allocate, invalid, resource};
use super::index_definition_compiler::{
  ConverterDefinitionLimitsInputV1, CorrectedIndexInputV1, FieldDefinitionLimitsInputV1, SourceDefinitionLimitsInputV1,
};
use super::index_semantic_registry::converter_registry;
use super::parser_context_compiler::validate_policy;

type Object = BTreeMap<String, Value>;

pub(super) struct ConfigurationSource {
  pub glob: Option<String>,
  pub parser: Option<String>,
  pub wasm: InvocationPolicyV1,
  pub raw_json: InvocationPolicyV1,
  pub native_suite: InvocationPolicyV1,
  pub rows: Vec<FieldSource>,
}

impl ConfigurationSource {
  pub(super) fn used_parser_alias(&self) -> Option<&str> {
    if self.rows.iter().any(|row| !matches!(row.source, SelectorSource::Metadata)) {
      self.parser.as_deref()
    } else {
      None
    }
  }
}

pub(super) struct FieldSource {
  pub name: String,
  pub source: SelectorSource,
  pub limits: SourceDefinitionLimitsInputV1,
  pub indexes: Vec<CorrectedIndexInputV1>,
}

pub(super) enum SelectorSource {
  Metadata,
  JsonPath(Option<Vec<serde_json::Value>>),
  Mapper { alias: String, arguments: Option<Vec<u8>>, policy: InvocationPolicyV1 },
}

pub(super) fn parse(source: &[u8]) -> Result<ConfigurationSource> {
  // The caller admits the entire AST before this strict duplicate-detecting
  // visitor runs. Do not parse through serde_json::Value and lose duplicate keys.
  let source: Value = serde_json::from_slice(source).map_err(|error| invalid(error.to_string()))?;
  let mut source = object(source)?;
  if unsigned(required(&mut source, "$v")?)? != 1 {
    return Err(invalid("corrected index configuration requires integer $v: 1"));
  }
  let rows = array(required(&mut source, "indexes")?)?;
  let glob = source.remove("glob").map(string).transpose()?;
  let parser = source.remove("parser").map(alias).transpose()?;
  if let Some(value) = source.remove("logging") {
    if !matches!(value, Value::Boolean(_)) {
      return Err(invalid("logging must be boolean"));
    }
  }
  if let Some(value) = source.remove("compression") {
    string(value)?;
  }
  let memory_limit = source.remove("parser_memory_limit").map(string).transpose()?;
  let mut policies = optional_object(&mut source, "parser_policies")?;
  let mut wasm = optional_object(&mut policies, "wasm")?;
  if let Some(limit) = memory_limit {
    let limit = crate::engine::index_config::parse_parser_memory_limit(&limit).map_err(|error| invalid(error.to_string()))? as u64;
    if let Some(explicit) = wasm.get("max_linear_memory_bytes") {
      if unsigned(explicit.clone())? != limit {
        return Err(invalid("parser memory limit spellings disagree"));
      }
    } else {
      wasm.insert("max_linear_memory_bytes".into(), Value::Unsigned(limit));
    }
  }
  let wasm = policy(wasm, true)?;
  let raw_json = policy(optional_object(&mut policies, "raw_json")?, false)?;
  let native_suite = policy(optional_object(&mut policies, "native_suite")?, false)?;
  exhausted(policies)?;
  exhausted(source)?;
  let mut compiled = allocate(rows.len())?;
  for row in rows {
    compiled.push(field(row)?);
  }
  Ok(ConfigurationSource { glob, parser, wasm, raw_json, native_suite, rows: compiled })
}

fn field(value: Value) -> Result<FieldSource> {
  let mut row = object(value)?;
  let mut name = string(required(&mut row, "name")?)?;
  if name == "@file_name" {
    name = "@filename".into();
  }
  let converters = match required(&mut row, "type")? {
    Value::String(name) => vec![Value::String(name)],
    value => array(value)?,
  };
  if converters.is_empty() {
    return Err(invalid("type must contain at least one corrected converter"));
  }
  let source = selector(&name, row.remove("source"))?;
  let limits = source_limits(optional_object(&mut row, "source_limits")?)?;
  let converter_limits = converter_limits(optional_object(&mut row, "converter_limits")?)?;
  let field_limits = field_limits(optional_object(&mut row, "field_limits")?)?;
  exhausted(row)?;
  let mut indexes = allocate(converters.len())?;
  for name in converters {
    let name = string(name)?;
    let entry = converter_registry()
      .iter()
      .find(|entry| entry.corrected && entry.name == name)
      .ok_or_else(|| invalid(format!("unknown corrected converter {name:?}")))?;
    indexes.push(CorrectedIndexInputV1 { converter_id: entry.id, converter_limits, field_limits });
  }
  Ok(FieldSource { name, source, limits, indexes })
}

fn selector(name: &str, source: Option<Value>) -> Result<SelectorSource> {
  if name.starts_with('@') {
    if source.is_some() {
      return Err(invalid("metadata fields have a fixed source and cannot override it"));
    }
    return Ok(SelectorSource::Metadata);
  }
  match source {
    None => Ok(SelectorSource::JsonPath(None)),
    Some(Value::Array(values)) => {
      let mut segments = allocate(values.len())?;
      for value in values {
        segments.push(match value {
          Value::String(value) => serde_json::Value::String(value),
          value => serde_json::Value::Number(unsigned(value)?.into()),
        });
      }
      Ok(SelectorSource::JsonPath(Some(segments)))
    }
    Some(value) => {
      let mut source = object(value)?;
      let alias = alias(required(&mut source, "plugin")?)?;
      let arguments = source
        .remove("args")
        .map(|value| {
          let bytes = encode_canonical_value(&value, CanonicalValueBounds::CONFIG).map_err(|error| resource(error.to_string()))?;
          canonical_value_to_json(&bytes, CanonicalValueBounds::CONFIG, 256 << 10).map_err(|error| resource(error.to_string()))
        })
        .transpose()?;
      let policy = policy(optional_object(&mut source, "policy")?, true)?;
      exhausted(source)?;
      Ok(SelectorSource::Mapper { alias, arguments, policy })
    }
  }
}

fn source_limits(mut values: Object) -> Result<SourceDefinitionLimitsInputV1> {
  let limits = SourceDefinitionLimitsInputV1 {
    max_source_values_per_document: optional_number(&mut values, "max_source_values_per_document")?,
    max_canonical_source_bytes_per_document: optional_number(&mut values, "max_canonical_source_bytes_per_document")?,
    max_document_input_bytes: optional_number(&mut values, "max_document_input_bytes")?,
    max_selector_work_items_per_document: optional_number(&mut values, "max_selector_work_items_per_document")?,
    max_selector_examined_bytes_per_document: optional_number(&mut values, "max_selector_examined_bytes_per_document")?,
  };
  exhausted(values)?;
  Ok(limits)
}

fn converter_limits(mut values: Object) -> Result<ConverterDefinitionLimitsInputV1> {
  let limits = ConverterDefinitionLimitsInputV1 {
    max_input_bytes: optional_number(&mut values, "max_input_bytes")?,
    max_output_values: optional_number(&mut values, "max_output_values")?,
    max_output_value_bytes: optional_number(&mut values, "max_output_value_bytes")?,
    max_total_output_bytes: optional_number(&mut values, "max_total_output_bytes")?,
  };
  exhausted(values)?;
  Ok(limits)
}

fn field_limits(mut values: Object) -> Result<FieldDefinitionLimitsInputV1> {
  let limits = FieldDefinitionLimitsInputV1 {
    max_terms_per_document: optional_number(&mut values, "max_terms_per_document")?,
    max_postings_per_document: optional_number(&mut values, "max_postings_per_document")?,
    max_canonical_posting_bytes_per_document: optional_number(&mut values, "max_canonical_posting_bytes_per_document")?,
    max_query_recheck_value_bytes: optional_number(&mut values, "max_query_recheck_value_bytes")?,
  };
  exhausted(values)?;
  Ok(limits)
}

fn policy(mut values: Object, wasm: bool) -> Result<InvocationPolicyV1> {
  let policy = InvocationPolicyV1 {
    kind: if wasm { InvocationPolicyKind::PureWasm } else { InvocationPolicyKind::Native },
    max_request_bytes: number(&mut values, "max_request_bytes", if wasm { 64 << 20 } else { 0 })?,
    max_response_bytes: number(&mut values, "max_response_bytes", 16 << 20)?,
    max_linear_memory_bytes: number(&mut values, "max_linear_memory_bytes", if wasm { 64 << 20 } else { 0 })?,
    max_fuel: number(&mut values, "max_fuel", if wasm { 10_000_000 } else { 0 })?,
    max_table_elements: number(&mut values, "max_table_elements", if wasm { 65536 } else { 0 })?,
    max_structure_nodes: number(&mut values, "max_structure_nodes", 65536)?,
    max_scalar_bytes: number(&mut values, "max_scalar_bytes", 1 << 20)?,
    max_structure_depth: number(&mut values, "max_structure_depth", 32)?,
    max_container_members: number(&mut values, "max_container_members", 65535)?,
    max_wasm_instances: number(&mut values, "max_wasm_instances", u32::from(wasm))?,
    max_wasm_memories: number(&mut values, "max_wasm_memories", u32::from(wasm))?,
    max_wasm_tables: number(&mut values, "max_wasm_tables", u32::from(wasm))?,
    max_value_stack_height: number(&mut values, "max_value_stack_height", 4096)?,
    max_recursion_depth: number(&mut values, "max_recursion_depth", 256)?,
  };
  exhausted(values)?;
  // Policy encoding uses a fixed stack array, with no fallible allocation;
  // codec resource-class errors here mean invalid semantic limits, not OOM.
  validate_policy(&policy, wasm).map_err(|error| invalid(error.to_string()))?;
  Ok(policy)
}

fn number<T: TryFrom<u64>>(values: &mut Object, key: &'static str, default: T) -> Result<T>
where
  T::Error: std::fmt::Display,
{
  match optional_number(values, key)? {
    Some(value) => Ok(value),
    None => Ok(default),
  }
}

fn optional_number<T: TryFrom<u64>>(values: &mut Object, key: &'static str) -> Result<Option<T>>
where
  T::Error: std::fmt::Display,
{
  values
    .remove(key)
    .map(|value| T::try_from(unsigned(value)?).map_err(|error| invalid(format!("{key} exceeds its integer width: {error}"))))
    .transpose()
}

fn unsigned(value: Value) -> Result<u64> {
  match value {
    Value::Signed(value) if value >= 0 => Ok(value as u64),
    Value::Unsigned(value) => Ok(value),
    _ => Err(invalid("expected an unsigned JSON integer")),
  }
}

fn object(value: Value) -> Result<Object> {
  match value {
    Value::Map(value) => Ok(value),
    _ => Err(invalid("expected an object")),
  }
}

fn optional_object(values: &mut Object, key: &'static str) -> Result<Object> {
  match values.remove(key) {
    Some(value) => object(value),
    None => Ok(Object::new()),
  }
}

fn array(value: Value) -> Result<Vec<Value>> {
  match value {
    Value::Array(value) => Ok(value),
    _ => Err(invalid("expected an array")),
  }
}

fn string(value: Value) -> Result<String> {
  match value {
    Value::String(value) => Ok(value),
    _ => Err(invalid("expected a string")),
  }
}

fn alias(value: Value) -> Result<String> {
  let alias = string(value)?;
  if alias.is_empty() || alias.len() > 4096 || alias.chars().any(char::is_control) {
    return Err(invalid("alias must contain 1..4096 UTF-8 bytes without control characters"));
  }
  Ok(alias)
}

fn required(values: &mut Object, key: &'static str) -> Result<Value> {
  values.remove(key).ok_or_else(|| invalid(format!("missing required member {key}")))
}

fn exhausted(values: Object) -> Result<()> {
  if let Some((key, _)) = values.first_key_value() {
    return Err(invalid(format!("unknown member {key:?}")));
  }
  Ok(())
}
