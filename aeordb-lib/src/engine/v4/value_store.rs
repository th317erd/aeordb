use crate::engine::HashAlgorithm;

use super::dependency::{DependencyRecordV1, DependencyTableV1, InvocationPolicyKind, decode_dependency_table};
use super::hash::digest_parts;
use super::index_semantic_registry::metadata_source_registry_entry;
use super::parser_plan::{ParserCandidateKind, ParserPlanKind, ParserResolutionPlanV1, decode_parser_resolution_plan};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::source_selector::{SourceSelectorKind, SourceSelectorV1, decode_source_selector};

const DEFINITION_HEADER_LENGTH: usize = 32;
const FIXED_BODY_WITHOUT_SCOPE: usize = 80;
const MAX_DEFINITION_LENGTH: usize = 512 * 1_024;
const MAX_FIELD_NAME_LENGTH: usize = 4 * 1_024;
const MAX_SELECTOR_LENGTH: usize = 4 * 1_024;
const MAX_PARSER_PLAN_LENGTH: usize = 128 * 1_024;
const MAX_DEPENDENCY_TABLE_LENGTH: usize = 256 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueStoreSemanticFamily {
  CorrectedV1,
  MigrationV0,
}

impl ValueStoreSemanticFamily {
  fn id(self) -> u16 {
    match self {
      Self::CorrectedV1 => 1,
      Self::MigrationV0 => 2,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueStoreDefinitionV1<'a> {
  pub value_store_id: Vec<u8>,
  pub scope_id: &'a [u8],
  pub field_name: &'a str,
  pub semantic_family: ValueStoreSemanticFamily,
  pub metadata_source_semantics: u16,
  pub source_selector_semantics: u16,
  pub missing_semantics: u16,
  pub extraction_error_semantics: u16,
  pub multi_value_ordering: u16,
  pub duplicate_value_semantics: u16,
  pub unindexable_semantics: u16,
  pub max_source_values_per_document: u32,
  pub max_canonical_source_bytes_per_document: u64,
  pub max_document_input_bytes: u64,
  pub max_selector_work_items_per_document: u64,
  pub max_selector_examined_bytes_per_document: u64,
  pub selector: SourceSelectorV1<'a>,
  pub parser_plan: ParserResolutionPlanV1<'a>,
  pub dependencies: DependencyTableV1<'a>,
}

pub fn decode_value_store_definition(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ValueStoreDefinitionV1<'_>> {
  let hash_width = hash_algorithm.hash_length();
  if value.len() > MAX_DEFINITION_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "value_store_exceeds_cap",
      format!("{} bytes exceeds {MAX_DEFINITION_LENGTH}", value.len()),
    ));
  }
  let minimum_length = DEFINITION_HEADER_LENGTH
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(FIXED_BODY_WITHOUT_SCOPE + 1))
    .ok_or_else(|| length_error("minimum definition length overflow"))?;
  if value.len() < minimum_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "value_store_truncated",
      format!("{} bytes is shorter than {minimum_length}", value.len()),
    ));
  }
  if &value[..4] != b"AVST" || u16_at(value, 4)? != 1 || u16_at(value, 6)? as usize != DEFINITION_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "value_store_envelope", "expected AVST v1 with 32-byte header"));
  }
  if u32_at(value, 8)? as usize != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "value_store_total_length",
      format!("declared {}, got {}", u32_at(value, 8)?, value.len()),
    ));
  }
  if u32_at(value, 12)? != 0 || value[16..32].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "value_store_reserved", "flags or definition reserve are nonzero"));
  }

  let scope_end = DEFINITION_HEADER_LENGTH.checked_add(hash_width).ok_or_else(|| length_error("scope end overflow"))?;
  let scope_id = &value[DEFINITION_HEADER_LENGTH..scope_end];
  if scope_id.iter().all(|byte| *byte == 0) {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "value_store_scope_id", "ScopeId is all zero"));
  }
  let fixed_start = scope_end;
  if value[fixed_start + 40..fixed_start + 48].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "value_store_body_reserved", "body reserve is nonzero"));
  }

  let field_length = u32_at(value, fixed_start)? as usize;
  let selector_length = u32_at(value, fixed_start + 4)? as usize;
  let parser_length = u32_at(value, fixed_start + 8)? as usize;
  let dependency_length = u32_at(value, fixed_start + 12)? as usize;
  validate_child_length(field_length, 1, MAX_FIELD_NAME_LENGTH, "field name")?;
  validate_child_length(selector_length, 32, MAX_SELECTOR_LENGTH, "source selector")?;
  validate_child_length(parser_length, 48, MAX_PARSER_PLAN_LENGTH, "parser plan")?;
  validate_child_length(dependency_length, 32, MAX_DEPENDENCY_TABLE_LENGTH, "dependency table")?;

  let field_start = fixed_start.checked_add(FIXED_BODY_WITHOUT_SCOPE).ok_or_else(|| length_error("field start overflow"))?;
  let field_end = checked_end(field_start, field_length, value.len(), "field")?;
  let selector_end = checked_end(field_end, selector_length, value.len(), "selector")?;
  let parser_end = checked_end(selector_end, parser_length, value.len(), "parser")?;
  let dependency_end = checked_end(parser_end, dependency_length, value.len(), "dependencies")?;
  if dependency_end != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "value_store_length_formula",
      format!("children end at {dependency_end}, definition ends at {}", value.len()),
    ));
  }
  let field_name = std::str::from_utf8(&value[field_start..field_end]).map_err(|source| {
    error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "value_store_field_utf8", format!("invalid UTF-8: {source}"))
  })?;
  if field_name.as_bytes().contains(&0) {
    return Err(error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "value_store_field_name", "field name contains NUL"));
  }

  let selector = decode_source_selector(&value[field_end..selector_end])?;
  let parser_plan = decode_parser_resolution_plan(&value[selector_end..parser_end])?;
  let dependencies = decode_dependency_table(&value[parser_end..dependency_end])?;

  let source_value_codec = u16_at(value, fixed_start + 16)?;
  let metadata_source_semantics = u16_at(value, fixed_start + 18)?;
  let source_selector_semantics = u16_at(value, fixed_start + 20)?;
  let parser_resolution_semantics = u16_at(value, fixed_start + 22)?;
  let missing_semantics = u16_at(value, fixed_start + 24)?;
  let null_semantics = u16_at(value, fixed_start + 26)?;
  let extraction_error_semantics = u16_at(value, fixed_start + 28)?;
  let multi_value_ordering = u16_at(value, fixed_start + 30)?;
  let duplicate_value_semantics = u16_at(value, fixed_start + 32)?;
  let unindexable_semantics = u16_at(value, fixed_start + 34)?;
  let max_source_values_per_document = u32_at(value, fixed_start + 36)?;
  let max_canonical_source_bytes_per_document = u64_at(value, fixed_start + 48)?;
  let max_document_input_bytes = u64_at(value, fixed_start + 56)?;
  let max_selector_work_items_per_document = u64_at(value, fixed_start + 64)?;
  let max_selector_examined_bytes_per_document = u64_at(value, fixed_start + 72)?;

  if source_selector_semantics != 1
    || missing_semantics != 1
    || extraction_error_semantics != 1
    || multi_value_ordering != 1
    || duplicate_value_semantics != 1
    || unindexable_semantics != 1
  {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "value_store_common_semantics",
      "one or more common semantic IDs are unknown",
    ));
  }
  if max_source_values_per_document == 0 || max_canonical_source_bytes_per_document == 0 {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "value_store_common_limits",
      "source-value count and canonical-byte limits must be nonzero",
    ));
  }
  let semantic_family = match (source_value_codec, null_semantics) {
    (1, 1) => ValueStoreSemanticFamily::CorrectedV1,
    (2, 2) => ValueStoreSemanticFamily::MigrationV0,
    _ => {
      return Err(error(
        MalformedInputClass::CrossRecordClosureMismatch,
        "value_store_semantic_family",
        "source codec and null semantics disagree",
      ));
    }
  };
  if parser_resolution_semantics != semantic_family.id() {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "value_store_semantic_family",
      "parent parser semantics disagree with the selected family",
    ));
  }

  validate_field_selector_and_limits(
    field_name,
    semantic_family,
    metadata_source_semantics,
    &selector,
    &parser_plan,
    max_source_values_per_document,
    max_canonical_source_bytes_per_document,
    max_document_input_bytes,
    max_selector_work_items_per_document,
    max_selector_examined_bytes_per_document,
  )?;
  validate_dependencies(semantic_family, &selector, &parser_plan, &dependencies.records)?;

  let value_store_id = digest_parts(hash_algorithm, &[b"aeordb.index.value-store-definition.v1\0", value]);
  Ok(ValueStoreDefinitionV1 {
    value_store_id,
    scope_id,
    field_name,
    semantic_family,
    metadata_source_semantics,
    source_selector_semantics,
    missing_semantics,
    extraction_error_semantics,
    multi_value_ordering,
    duplicate_value_semantics,
    unindexable_semantics,
    max_source_values_per_document,
    max_canonical_source_bytes_per_document,
    max_document_input_bytes,
    max_selector_work_items_per_document,
    max_selector_examined_bytes_per_document,
    selector,
    parser_plan,
    dependencies,
  })
}

#[allow(clippy::too_many_arguments)]
fn validate_field_selector_and_limits(
  field_name: &str,
  family: ValueStoreSemanticFamily,
  metadata_source_semantics: u16,
  selector: &SourceSelectorV1<'_>,
  parser: &ParserResolutionPlanV1<'_>,
  max_source_values: u32,
  max_canonical_bytes: u64,
  max_document_input: u64,
  max_selector_work: u64,
  max_selector_examined: u64,
) -> FormatResult<()> {
  if family == ValueStoreSemanticFamily::CorrectedV1 && (max_source_values == u32::MAX || max_canonical_bytes == u64::MAX) {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "value_store_corrected_limit",
      "corrected source limits must be finite",
    ));
  }
  match selector.kind {
    SourceSelectorKind::Metadata => {
      let metadata_id = selector.metadata_id.ok_or_else(|| closure_error("metadata selector omits metadata ID"))?;
      let metadata_field_name = metadata_source_registry_entry(metadata_id)
        .map(|entry| entry.field_name)
        .ok_or_else(|| closure_error("metadata selector names an unknown metadata ID"))?;
      if metadata_source_semantics != family.id()
        || parser.kind != ParserPlanKind::None
        || field_name != metadata_field_name
        || max_document_input != 0
        || max_selector_work != 0
        || max_selector_examined != 0
      {
        return Err(closure_error("metadata field, selector, parser, semantics, or limits disagree"));
      }
    }
    SourceSelectorKind::JsonPath => {
      if metadata_source_semantics != 0
        || parser.kind == ParserPlanKind::None
        || parser.resolution_semantics != family.id()
        || field_name.starts_with('@')
        || max_document_input == 0
        || max_selector_work == 0
        || max_selector_examined == 0
      {
        return Err(closure_error("JSON field, selector, parser, semantics, or limits disagree"));
      }
      if family == ValueStoreSemanticFamily::CorrectedV1
        && [max_document_input, max_selector_work, max_selector_examined].contains(&u64::MAX)
      {
        return Err(error(
          MalformedInputClass::AllocationAmplification,
          "value_store_corrected_limit",
          "corrected JSON limits must be finite",
        ));
      }
    }
    SourceSelectorKind::PluginMapper => {
      let expected_contract = if family == ValueStoreSemanticFamily::CorrectedV1 { 2 } else { 1 };
      if metadata_source_semantics != 0
        || parser.kind == ParserPlanKind::None
        || parser.resolution_semantics != family.id()
        || selector.mapper_contract != expected_contract
        || field_name.starts_with('@')
        || max_document_input == 0
        || max_selector_work != 0
        || max_selector_examined != 0
      {
        return Err(closure_error("mapper field, selector, parser, semantics, or limits disagree"));
      }
      if family == ValueStoreSemanticFamily::CorrectedV1 && max_document_input == u64::MAX {
        return Err(error(
          MalformedInputClass::AllocationAmplification,
          "value_store_corrected_limit",
          "corrected mapper input limit must be finite",
        ));
      }
    }
    SourceSelectorKind::AlwaysMissingV0 => {
      if family != ValueStoreSemanticFamily::MigrationV0
        || metadata_source_semantics != 0
        || parser.kind == ParserPlanKind::None
        || parser.resolution_semantics != 2
        || field_name.starts_with('@')
        || max_document_input == 0
        || max_selector_work != 0
        || max_selector_examined != 0
      {
        return Err(closure_error("always-missing selector is outside its migration-only context"));
      }
    }
  }
  Ok(())
}

fn validate_dependencies(
  family: ValueStoreSemanticFamily,
  selector: &SourceSelectorV1<'_>,
  parser: &ParserResolutionPlanV1<'_>,
  dependencies: &[DependencyRecordV1<'_>],
) -> FormatResult<()> {
  let mut used = vec![false; dependencies.len()];
  if parser.kind == ParserPlanKind::Automatic {
    let mime = dependency_at(dependencies, parser.mime_dependency_ordinal)?;
    require_native_role(mime, 3)?;
    used[parser.mime_dependency_ordinal as usize - 1] = true;
  }
  for candidate in &parser.candidates {
    let dependency = dependency_at(dependencies, candidate.dependency_ordinal)?;
    match candidate.kind {
      ParserCandidateKind::Explicit | ParserCandidateKind::Registry => {
        require_wasm_role(dependency, 1, family, candidate.match_semantics)?;
        if !matches!(
          (family, candidate.policy.kind),
          (ValueStoreSemanticFamily::CorrectedV1, InvocationPolicyKind::PureWasm)
            | (ValueStoreSemanticFamily::MigrationV0, InvocationPolicyKind::LegacyWasm)
        ) {
          return Err(closure_error("candidate invocation policy disagrees with ValueStore family"));
        }
      }
      ParserCandidateKind::RawJson | ParserCandidateKind::NativeSuite => {
        require_native_role(dependency, 1)?;
        if candidate.policy.kind != InvocationPolicyKind::Native {
          return Err(closure_error("native parser candidate does not use a native policy"));
        }
      }
    }
    used[candidate.dependency_ordinal as usize - 1] = true;
  }
  match selector.kind {
    SourceSelectorKind::JsonPath => {
      let selector_dependencies: Vec<_> = dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, dependency)| (dependency.kind == 2 && dependency.role == 4).then_some(index))
        .collect();
      if selector_dependencies.len() != 1 {
        return Err(closure_error("JSON selector does not have exactly one AeorRegex dependency"));
      }
      require_native_role(&dependencies[selector_dependencies[0]], 4)?;
      used[selector_dependencies[0]] = true;
    }
    SourceSelectorKind::PluginMapper => {
      let ordinal = selector.dependency_ordinal.ok_or_else(|| closure_error("mapper selector omits dependency ordinal"))?;
      let dependency = dependency_at(dependencies, ordinal)?;
      require_wasm_role(dependency, 2, family, family.id())?;
      used[ordinal as usize - 1] = true;
    }
    SourceSelectorKind::Metadata | SourceSelectorKind::AlwaysMissingV0 => {}
  }
  if used.iter().any(|used| !used) {
    return Err(closure_error("dependency table contains an unused record"));
  }
  Ok(())
}

fn dependency_at<'records, 'bytes>(
  dependencies: &'records [DependencyRecordV1<'bytes>],
  ordinal: u32,
) -> FormatResult<&'records DependencyRecordV1<'bytes>> {
  ordinal
    .checked_sub(1)
    .and_then(|index| dependencies.get(index as usize))
    .ok_or_else(|| closure_error(format!("dependency ordinal {ordinal} is unresolved")))
}

fn require_native_role(dependency: &DependencyRecordV1<'_>, role: u16) -> FormatResult<()> {
  if dependency.kind != 2
    || dependency.role != role
    || dependency.abi != 0
    || dependency.executor_profile != 1
    || dependency.artifact_kind != 0
    || dependency.artifact_length != 0
  {
    return Err(closure_error(format!("dependency {} is not native role {role}", dependency.dependency_id)));
  }
  Ok(())
}

fn require_wasm_role(
  dependency: &DependencyRecordV1<'_>,
  role: u16,
  family: ValueStoreSemanticFamily,
  match_semantics: u16,
) -> FormatResult<()> {
  let expected_abi = match (role, family) {
    (1, ValueStoreSemanticFamily::CorrectedV1) => 3,
    (1, ValueStoreSemanticFamily::MigrationV0) => 1,
    (2, ValueStoreSemanticFamily::CorrectedV1) => 4,
    (2, ValueStoreSemanticFamily::MigrationV0) => 2,
    _ => return Err(closure_error("unsupported WASM dependency role/family")),
  };
  let expected_executor = if family == ValueStoreSemanticFamily::CorrectedV1 { 2 } else { 3 };
  if dependency.kind != 1
    || dependency.role != role
    || dependency.abi != expected_abi
    || dependency.executor_profile != expected_executor
    || dependency.artifact_kind != 1
    || dependency.artifact_length == 0
    || (role == 1 && match_semantics != family.id() && match_semantics != 0)
  {
    return Err(closure_error(format!("dependency {} is not the required WASM role", dependency.dependency_id)));
  }
  Ok(())
}

fn validate_child_length(length: usize, minimum: usize, maximum: usize, name: &'static str) -> FormatResult<()> {
  if length > maximum {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "value_store_child_exceeds_cap",
      format!("{name} length {length} exceeds {maximum}"),
    ));
  }
  if length < minimum {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "value_store_child_too_short",
      format!("{name} length {length} is shorter than {minimum}"),
    ));
  }
  Ok(())
}

fn checked_end(start: usize, length: usize, available: usize, name: &'static str) -> FormatResult<usize> {
  let end = start.checked_add(length).ok_or_else(|| length_error(format!("{name} end overflow")))?;
  if end > available {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "value_store_child_truncated",
      format!("{name} ends at {end}, only {available} bytes exist"),
    ));
  }
  Ok(end)
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes
    .get(offset..offset + 2)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "value_store_u16_truncated", format!("u16 at {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked value-store u16 length")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes
    .get(offset..offset + 4)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "value_store_u32_truncated", format!("u32 at {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked value-store u32 length")))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let raw = bytes
    .get(offset..offset + 8)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "value_store_u64_truncated", format!("u64 at {offset}")))?;
  Ok(u64::from_le_bytes(raw.try_into().expect("checked value-store u64 length")))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "value_store_length_overflow", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "value_store_closure", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
