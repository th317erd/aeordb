use crate::core::HashProfile;
use crate::dependency::{self, DependencyRecord};
use crate::parser::{self, Candidate, ParserPlan};
use crate::policy::{self, PolicyKind};
use crate::selector::{self, Segment};

const DEFINITION_HEADER_LENGTH: usize = 32;
const FIXED_BODY_WITHOUT_SCOPE: usize = 80;
const MAX_DEFINITION_LENGTH: usize = 512 * 1_024;
const MAX_FIELD_NAME_LENGTH: usize = 4 * 1_024;
const MAX_SELECTOR_LENGTH: usize = 4 * 1_024;
const MAX_PARSER_PLAN_LENGTH: usize = 128 * 1_024;
const MAX_DEPENDENCY_TABLE_LENGTH: usize = 256 * 1_024;

#[derive(Clone, Copy)]
pub enum ValueStoreFormat {
  ValueStoreDefinitionV1,
}

impl ValueStoreFormat {
  pub fn id(self) -> &'static str {
    "value-store-definition-v1"
  }

  pub fn family(self) -> &'static str {
    "ValueStoreDefinitionV1"
  }
}

#[derive(Clone)]
pub struct ValueStoreFixtureCase {
  pub id: &'static str,
  pub format: ValueStoreFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone)]
struct ValueStoreDefinition {
  scope_id: Vec<u8>,
  field_name: String,
  selector: Vec<u8>,
  parser_plan: Vec<u8>,
  dependencies: Vec<u8>,
  source_value_codec: u16,
  metadata_source_semantics: u16,
  source_selector_semantics: u16,
  parser_resolution_semantics: u16,
  missing_semantics: u16,
  null_semantics: u16,
  extraction_error_semantics: u16,
  multi_value_ordering: u16,
  duplicate_value_semantics: u16,
  unindexable_semantics: u16,
  max_source_values_per_document: u32,
  max_canonical_source_bytes_per_document: u64,
  max_document_input_bytes: u64,
  max_selector_work_items_per_document: u64,
  max_selector_examined_bytes_per_document: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct DecodedValueStore {
  field_name: String,
  selector_kind: u16,
  dependency_count: usize,
}

pub fn fixture_cases() -> Vec<ValueStoreFixtureCase> {
  let mut cases = Vec::with_capacity(14);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (suffix, definition, expected, relation) in fixture_definitions(profile) {
      let bytes = build_definition(profile, &definition).expect("fixture ValueStore definition must encode");
      cases.push(ValueStoreFixtureCase {
        id: fixture_id(profile, suffix),
        format: ValueStoreFormat::ValueStoreDefinitionV1,
        profile,
        expected,
        relation,
        canonical_key: Some(value_store_id(profile, &bytes)),
        bytes,
      });
    }
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_definition(profile, bytes) {
    Ok(value_store) => (
      format!(
        "value-store:field={}:selector={}:dependencies={}",
        value_store.field_name, value_store.selector_kind, value_store.dependency_count
      ),
      Some(value_store_id(profile, bytes)),
    ),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let hash_width = profile.width();
  vec![
    "definition +0x000 len 32: AVST canonical-definition envelope".to_string(),
    format!("definition +0x020 len {hash_width}: ScopeId"),
    format!("definition +0x{:03x} len 80: child lengths, semantic IDs, limits, and reserve", 32 + hash_width),
    format!("definition +0x{:03x}: field name followed by selector, parser plan, and dependency table", 112 + hash_width),
    format!("definition total bytes: {}", bytes.len()),
  ]
}

fn fixture_definitions(profile: HashProfile) -> Vec<(&'static str, ValueStoreDefinition, &'static str, Option<&'static str>)> {
  vec![
    (
      "metadata-hash-corrected",
      metadata_definition(profile, 1),
      "value-store:field=@hash:selector=1:dependencies=0",
      Some("semantic-family:corrected-metadata"),
    ),
    (
      "metadata-created-at-legacy",
      metadata_definition(profile, 2),
      "value-store:field=@created_at:selector=1:dependencies=0",
      Some("semantic-family:legacy-metadata-v0"),
    ),
    (
      "json-corrected",
      json_definition(profile, false, false),
      "value-store:field=messages:selector=2:dependencies=5",
      Some("semantic-family:corrected-json"),
    ),
    (
      "mapper-corrected",
      mapper_definition(profile, false),
      "value-store:field=summary:selector=3:dependencies=2",
      Some("semantic-family:corrected-mapper"),
    ),
    (
      "json-legacy",
      json_definition(profile, true, false),
      "value-store:field=messages:selector=2:dependencies=5",
      Some("semantic-family:migration-json-v0"),
    ),
    (
      "mapper-legacy",
      mapper_definition(profile, true),
      "value-store:field=summary:selector=3:dependencies=2",
      Some("semantic-family:migration-mapper-v0"),
    ),
    (
      "always-missing-legacy",
      json_definition(profile, true, true),
      "value-store:field=legacy_missing:selector=4:dependencies=4",
      Some("semantic-family:migration-always-missing-v0"),
    ),
  ]
}

fn metadata_definition(profile: HashProfile, family: u16) -> ValueStoreDefinition {
  let (metadata_id, field_name) = if family == 1 { (8, "@hash") } else { (6, "@created_at") };
  let parser_plan = parser::build_plan(&ParserPlan {
    kind: 1,
    resolution_semantics: 0,
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: Vec::new(),
  })
  .unwrap();
  ValueStoreDefinition {
    scope_id: sequence_bytes(profile.width(), if family == 1 { 0x11 } else { 0x12 }),
    field_name: field_name.to_string(),
    selector: selector::build_metadata(metadata_id).unwrap(),
    parser_plan,
    dependencies: dependency::build_table(&[]).unwrap(),
    source_value_codec: family,
    metadata_source_semantics: family,
    source_selector_semantics: 1,
    parser_resolution_semantics: family,
    missing_semantics: 1,
    null_semantics: family,
    extraction_error_semantics: 1,
    multi_value_ordering: 1,
    duplicate_value_semantics: 1,
    unindexable_semantics: 1,
    max_source_values_per_document: 1,
    max_canonical_source_bytes_per_document: if family == 1 { 4 * 1_024 } else { u64::MAX },
    max_document_input_bytes: 0,
    max_selector_work_items_per_document: 0,
    max_selector_examined_bytes_per_document: 0,
  }
}

fn json_definition(profile: HashProfile, legacy: bool, always_missing: bool) -> ValueStoreDefinition {
  let dependencies = json_dependencies(legacy, !always_missing);
  let dependency_bytes = dependency::build_table(&dependencies).unwrap();
  let parser_plan = if legacy {
    parser::build_plan(&ParserPlan {
      kind: 3,
      resolution_semantics: 2,
      mime_semantics: 2,
      no_match_semantics: 2,
      mime_dependency_ordinal: 4,
      candidates: vec![
        parser::legacy_wasm_candidate(1, b"Text/Plain; charset=UTF-8".to_vec()),
        parser::native_candidate(3, 3),
        parser::native_candidate(4, 2),
      ],
    })
    .unwrap()
  } else {
    parser::build_plan(&ParserPlan {
      kind: 3,
      resolution_semantics: 1,
      mime_semantics: 1,
      no_match_semantics: 1,
      mime_dependency_ordinal: 4,
      candidates: vec![
        parser::wasm_candidate(2, 1, b"text/plain".to_vec()),
        parser::native_candidate(3, 3),
        parser::native_candidate(4, 2),
      ],
    })
    .unwrap()
  };
  let selector = if always_missing {
    selector::build_always_missing()
  } else {
    selector::build_json_path(&[
      Segment::ObjectKey("messages".to_string()),
      Segment::FanOut,
      Segment::Regex { pattern: "user".to_string(), case_insensitive: true },
    ])
    .unwrap()
  };
  let family = if legacy { 2 } else { 1 };
  ValueStoreDefinition {
    scope_id: sequence_bytes(profile.width(), if legacy { 0x22 } else { 0x21 }),
    field_name: if always_missing { "legacy_missing" } else { "messages" }.to_string(),
    selector,
    parser_plan,
    dependencies: dependency_bytes,
    source_value_codec: family,
    metadata_source_semantics: 0,
    source_selector_semantics: 1,
    parser_resolution_semantics: family,
    missing_semantics: 1,
    null_semantics: family,
    extraction_error_semantics: 1,
    multi_value_ordering: 1,
    duplicate_value_semantics: 1,
    unindexable_semantics: 1,
    max_source_values_per_document: if legacy { u32::MAX } else { 1_024 },
    max_canonical_source_bytes_per_document: if legacy { u64::MAX } else { 4 * 1_024 * 1_024 },
    max_document_input_bytes: if legacy { u64::MAX } else { 64 * 1_024 * 1_024 },
    max_selector_work_items_per_document: if always_missing {
      0
    } else if legacy {
      u64::MAX
    } else {
      1_000_000
    },
    max_selector_examined_bytes_per_document: if always_missing {
      0
    } else if legacy {
      u64::MAX
    } else {
      64 * 1_024 * 1_024
    },
  }
}

fn mapper_definition(profile: HashProfile, legacy: bool) -> ValueStoreDefinition {
  let parser_dependency = wasm_dependency(1, legacy, "/org/aeordev/aeordb/plugins/fixture-parser");
  let mapper_dependency = wasm_dependency(2, legacy, "/org/aeordev/aeordb/plugins/fixture-mapper");
  let dependencies = vec![parser_dependency, mapper_dependency];
  let parser_plan = parser::build_plan(&ParserPlan {
    kind: 2,
    resolution_semantics: if legacy { 2 } else { 1 },
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: vec![Candidate {
      kind: 1,
      match_semantics: 0,
      dependency_ordinal: 1,
      match_bytes: Vec::new(),
      policy: policy::build_policy(if legacy { PolicyKind::LegacyWasm } else { PolicyKind::PureWasm }),
    }],
  })
  .unwrap();
  let selector = selector::build_mapper(if legacy { 1 } else { 2 }, 2, &selector::canonical_null()).unwrap();
  let family = if legacy { 2 } else { 1 };
  ValueStoreDefinition {
    scope_id: sequence_bytes(profile.width(), if legacy { 0x32 } else { 0x31 }),
    field_name: "summary".to_string(),
    selector,
    parser_plan,
    dependencies: dependency::build_table(&dependencies).unwrap(),
    source_value_codec: family,
    metadata_source_semantics: 0,
    source_selector_semantics: 1,
    parser_resolution_semantics: family,
    missing_semantics: 1,
    null_semantics: family,
    extraction_error_semantics: 1,
    multi_value_ordering: 1,
    duplicate_value_semantics: 1,
    unindexable_semantics: 1,
    max_source_values_per_document: if legacy { u32::MAX } else { 128 },
    max_canonical_source_bytes_per_document: if legacy { u64::MAX } else { 4 * 1_024 * 1_024 },
    max_document_input_bytes: if legacy { u64::MAX } else { 64 * 1_024 * 1_024 },
    max_selector_work_items_per_document: 0,
    max_selector_examined_bytes_per_document: 0,
  }
}

fn json_dependencies(legacy: bool, include_selector: bool) -> Vec<DependencyRecord> {
  let mut records = vec![
    wasm_dependency(1, legacy, "/org/aeordev/aeordb/plugins/fixture-text-parser"),
    native_dependency(1, "/org/aeordev/aeordb/native/native-suite-v1"),
    native_dependency(1, "/org/aeordev/aeordb/native/raw-json-v1"),
    native_dependency(3, "/org/aeordev/aeordb/native/mime-router-v1"),
  ];
  if include_selector {
    records.push(native_dependency(4, "/org/aeordev/aeordb/native/aeor-regex-v1"));
  }
  records
}

fn wasm_dependency(role: u16, legacy: bool, id: &str) -> DependencyRecord {
  DependencyRecord {
    kind: 1,
    role,
    flags: 0x04,
    abi: match (role, legacy) {
      (1, false) => 3,
      (1, true) => 1,
      (2, false) => 4,
      (2, true) => 2,
      _ => unreachable!("fixture WASM roles are parser or mapper"),
    },
    executor_profile: if legacy { 3 } else { 2 },
    fingerprint_semantics: 1,
    artifact_kind: 1,
    artifact_length: 4_096,
    fingerprint: dependency::digest32(format!("{id}:{}", if legacy { "legacy" } else { "corrected" }).as_bytes()),
    dependency_id: id.to_string(),
    version: "1.0.0".to_string(),
  }
}

fn native_dependency(role: u16, id: &str) -> DependencyRecord {
  DependencyRecord {
    kind: 2,
    role,
    flags: 0,
    abi: 0,
    executor_profile: 1,
    fingerprint_semantics: 2,
    artifact_kind: 0,
    artifact_length: 0,
    fingerprint: dependency::digest32(format!("{id}:semantic-conformance-v1").as_bytes()),
    dependency_id: id.to_string(),
    version: "1.0.0".to_string(),
  }
}

fn build_definition(profile: HashProfile, definition: &ValueStoreDefinition) -> Result<Vec<u8>, &'static str> {
  let hash_width = profile.width();
  if definition.scope_id.len() != hash_width || definition.scope_id.iter().all(|byte| *byte == 0) {
    return Err("value_store_scope_id");
  }
  let field_length = definition.field_name.len();
  let selector_length = definition.selector.len();
  let parser_length = definition.parser_plan.len();
  let dependency_length = definition.dependencies.len();
  let total_length = DEFINITION_HEADER_LENGTH
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(FIXED_BODY_WITHOUT_SCOPE))
    .and_then(|length| length.checked_add(field_length))
    .and_then(|length| length.checked_add(selector_length))
    .and_then(|length| length.checked_add(parser_length))
    .and_then(|length| length.checked_add(dependency_length))
    .ok_or("value_store_length")?;
  if total_length > MAX_DEFINITION_LENGTH {
    return Err("value_store_length");
  }
  let mut value = vec![0u8; DEFINITION_HEADER_LENGTH];
  value[0..4].copy_from_slice(b"AVST");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, DEFINITION_HEADER_LENGTH as u16);
  put_u32(&mut value, 8, total_length as u32);
  value.extend_from_slice(&definition.scope_id);
  let fixed_start = value.len();
  value.resize(fixed_start + FIXED_BODY_WITHOUT_SCOPE, 0);
  put_u32(&mut value, fixed_start, field_length as u32);
  put_u32(&mut value, fixed_start + 4, selector_length as u32);
  put_u32(&mut value, fixed_start + 8, parser_length as u32);
  put_u32(&mut value, fixed_start + 12, dependency_length as u32);
  put_u16(&mut value, fixed_start + 16, definition.source_value_codec);
  put_u16(&mut value, fixed_start + 18, definition.metadata_source_semantics);
  put_u16(&mut value, fixed_start + 20, definition.source_selector_semantics);
  put_u16(&mut value, fixed_start + 22, definition.parser_resolution_semantics);
  put_u16(&mut value, fixed_start + 24, definition.missing_semantics);
  put_u16(&mut value, fixed_start + 26, definition.null_semantics);
  put_u16(&mut value, fixed_start + 28, definition.extraction_error_semantics);
  put_u16(&mut value, fixed_start + 30, definition.multi_value_ordering);
  put_u16(&mut value, fixed_start + 32, definition.duplicate_value_semantics);
  put_u16(&mut value, fixed_start + 34, definition.unindexable_semantics);
  put_u32(&mut value, fixed_start + 36, definition.max_source_values_per_document);
  put_u64(&mut value, fixed_start + 48, definition.max_canonical_source_bytes_per_document);
  put_u64(&mut value, fixed_start + 56, definition.max_document_input_bytes);
  put_u64(&mut value, fixed_start + 64, definition.max_selector_work_items_per_document);
  put_u64(&mut value, fixed_start + 72, definition.max_selector_examined_bytes_per_document);
  value.extend_from_slice(definition.field_name.as_bytes());
  value.extend_from_slice(&definition.selector);
  value.extend_from_slice(&definition.parser_plan);
  value.extend_from_slice(&definition.dependencies);
  decode_definition(profile, &value)?;
  Ok(value)
}

fn decode_definition(profile: HashProfile, value: &[u8]) -> Result<DecodedValueStore, &'static str> {
  let hash_width = profile.width();
  let minimum_length = DEFINITION_HEADER_LENGTH + hash_width + FIXED_BODY_WITHOUT_SCOPE + 1;
  if value.len() < minimum_length || value.len() > MAX_DEFINITION_LENGTH {
    return Err("value_store_length");
  }
  if &value[0..4] != b"AVST"
    || read_u16(value, 4)? != 1
    || read_u16(value, 6)? as usize != DEFINITION_HEADER_LENGTH
    || read_u32(value, 8)? as usize != value.len()
  {
    return Err("value_store_envelope");
  }
  if read_u32(value, 12)? != 0 || value[16..32].iter().any(|byte| *byte != 0) {
    return Err("value_store_reserved");
  }
  if value[32..32 + hash_width].iter().all(|byte| *byte == 0) {
    return Err("value_store_scope_id");
  }
  let fixed_start = 32 + hash_width;
  if value[fixed_start + 40..fixed_start + 48].iter().any(|byte| *byte != 0) {
    return Err("value_store_reserved");
  }
  let field_length = read_u32(value, fixed_start)? as usize;
  let selector_length = read_u32(value, fixed_start + 4)? as usize;
  let parser_length = read_u32(value, fixed_start + 8)? as usize;
  let dependency_length = read_u32(value, fixed_start + 12)? as usize;
  if !(1..=MAX_FIELD_NAME_LENGTH).contains(&field_length)
    || !(32..=MAX_SELECTOR_LENGTH).contains(&selector_length)
    || !(48..=MAX_PARSER_PLAN_LENGTH).contains(&parser_length)
    || !(32..=MAX_DEPENDENCY_TABLE_LENGTH).contains(&dependency_length)
  {
    return Err("value_store_child_length");
  }
  let field_start = fixed_start + FIXED_BODY_WITHOUT_SCOPE;
  let field_end = checked_end(field_start, field_length, value.len())?;
  let selector_end = checked_end(field_end, selector_length, value.len())?;
  let parser_end = checked_end(selector_end, parser_length, value.len())?;
  let dependency_end = checked_end(parser_end, dependency_length, value.len())?;
  if dependency_end != value.len() {
    return Err("value_store_length_formula");
  }
  let field_name = std::str::from_utf8(&value[field_start..field_end]).map_err(|_| "value_store_field_utf8")?.to_string();
  if field_name.as_bytes().contains(&0) {
    return Err("value_store_field_name");
  }

  let selector = selector::decode_selector(&value[field_end..selector_end]).map_err(|_| "value_store_selector")?;
  let parser = parser::decode_plan(&value[selector_end..parser_end]).map_err(|_| "value_store_parser")?;
  let dependencies = dependency::decode_table(&value[parser_end..dependency_end]).map_err(|_| "value_store_dependencies")?;

  let source_value_codec = read_u16(value, fixed_start + 16)?;
  let metadata_source_semantics = read_u16(value, fixed_start + 18)?;
  let source_selector_semantics = read_u16(value, fixed_start + 20)?;
  let parser_resolution_semantics = read_u16(value, fixed_start + 22)?;
  let missing_semantics = read_u16(value, fixed_start + 24)?;
  let null_semantics = read_u16(value, fixed_start + 26)?;
  let extraction_error_semantics = read_u16(value, fixed_start + 28)?;
  let multi_value_ordering = read_u16(value, fixed_start + 30)?;
  let duplicate_value_semantics = read_u16(value, fixed_start + 32)?;
  let unindexable_semantics = read_u16(value, fixed_start + 34)?;
  let max_source_values = read_u32(value, fixed_start + 36)?;
  let max_canonical_bytes = read_u64(value, fixed_start + 48)?;
  let max_document_input = read_u64(value, fixed_start + 56)?;
  let max_selector_work = read_u64(value, fixed_start + 64)?;
  let max_selector_examined = read_u64(value, fixed_start + 72)?;

  if source_selector_semantics != 1
    || missing_semantics != 1
    || extraction_error_semantics != 1
    || multi_value_ordering != 1
    || duplicate_value_semantics != 1
    || unindexable_semantics != 1
    || max_source_values == 0
    || max_canonical_bytes == 0
  {
    return Err("value_store_common_semantics");
  }
  let family = match (source_value_codec, null_semantics) {
    (1, 1) => 1,
    (2, 2) => 2,
    _ => return Err("value_store_semantic_family"),
  };
  if parser_resolution_semantics != family {
    return Err("value_store_semantic_family");
  }
  validate_field_selector_and_limits(
    &field_name,
    family,
    metadata_source_semantics,
    &selector,
    &parser,
    max_source_values,
    max_canonical_bytes,
    max_document_input,
    max_selector_work,
    max_selector_examined,
  )?;
  validate_dependencies(family, &selector, &parser, &dependencies.records)?;

  Ok(DecodedValueStore { field_name, selector_kind: selector.kind, dependency_count: dependencies.records.len() })
}

#[allow(clippy::too_many_arguments)]
fn validate_field_selector_and_limits(
  field_name: &str,
  family: u16,
  metadata_source_semantics: u16,
  selector: &selector::DecodedSelector,
  parser: &parser::DecodedPlan,
  max_source_values: u32,
  max_canonical_bytes: u64,
  max_document_input: u64,
  max_selector_work: u64,
  max_selector_examined: u64,
) -> Result<(), &'static str> {
  if family == 1 && (max_source_values == u32::MAX || max_canonical_bytes == u64::MAX) {
    return Err("value_store_corrected_limit");
  }
  match selector.kind {
    1 => {
      let metadata_id = selector.metadata_id.ok_or("value_store_metadata_selector")?;
      if metadata_source_semantics != family
        || parser.kind != 1
        || field_name != metadata_field_name(metadata_id)
        || max_document_input != 0
        || max_selector_work != 0
        || max_selector_examined != 0
      {
        return Err("value_store_metadata_context");
      }
    }
    2 => {
      if metadata_source_semantics != 0
        || parser.kind == 1
        || parser.resolution_semantics != family
        || field_name.starts_with('@')
        || max_document_input == 0
        || max_selector_work == 0
        || max_selector_examined == 0
      {
        return Err("value_store_json_context");
      }
      if family == 1 && [max_document_input, max_selector_work, max_selector_examined].contains(&u64::MAX) {
        return Err("value_store_corrected_limit");
      }
    }
    3 => {
      if metadata_source_semantics != 0
        || parser.kind == 1
        || parser.resolution_semantics != family
        || selector.mapper_contract != if family == 1 { 2 } else { 1 }
        || field_name.starts_with('@')
        || max_document_input == 0
        || max_selector_work != 0
        || max_selector_examined != 0
      {
        return Err("value_store_mapper_context");
      }
      if family == 1 && max_document_input == u64::MAX {
        return Err("value_store_corrected_limit");
      }
    }
    4 => {
      if family != 2
        || metadata_source_semantics != 0
        || parser.kind == 1
        || parser.resolution_semantics != 2
        || field_name.starts_with('@')
        || max_document_input == 0
        || max_selector_work != 0
        || max_selector_examined != 0
      {
        return Err("value_store_always_missing_context");
      }
    }
    _ => unreachable!("selector decoder rejects unknown kinds"),
  }
  Ok(())
}

fn validate_dependencies(
  family: u16,
  selector: &selector::DecodedSelector,
  parser: &parser::DecodedPlan,
  dependencies: &[DependencyRecord],
) -> Result<(), &'static str> {
  let mut used = vec![false; dependencies.len()];
  if parser.kind == 3 {
    let mime = dependency_at(dependencies, parser.mime_dependency_ordinal)?;
    require_native_role(mime, 3)?;
    used[parser.mime_dependency_ordinal as usize - 1] = true;
  }
  for (((candidate_kind, ordinal), match_semantics), policy_kind) in
    parser.candidate_dependencies.iter().zip(&parser.candidate_match_semantics).zip(&parser.candidate_policy_kinds)
  {
    let dependency = dependency_at(dependencies, *ordinal)?;
    match *candidate_kind {
      1 | 2 => {
        require_wasm_role(dependency, 1, family, *match_semantics)?;
        if !matches!((family, policy_kind), (1, PolicyKind::PureWasm) | (2, PolicyKind::LegacyWasm)) {
          return Err("value_store_candidate_policy");
        }
      }
      3 | 4 => {
        require_native_role(dependency, 1)?;
        if *policy_kind != PolicyKind::Native {
          return Err("value_store_candidate_policy");
        }
      }
      _ => return Err("value_store_candidate_dependency"),
    }
    used[*ordinal as usize - 1] = true;
  }
  match selector.kind {
    2 => {
      let selector_dependencies: Vec<usize> = dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, dependency)| (dependency.kind == 2 && dependency.role == 4).then_some(index))
        .collect();
      if selector_dependencies.len() != 1 {
        return Err("value_store_selector_dependency");
      }
      require_native_role(&dependencies[selector_dependencies[0]], 4)?;
      used[selector_dependencies[0]] = true;
    }
    3 => {
      let ordinal = selector.dependency_ordinal.ok_or("value_store_mapper_dependency")?;
      let dependency = dependency_at(dependencies, ordinal)?;
      require_wasm_role(dependency, 2, family, family)?;
      used[ordinal as usize - 1] = true;
    }
    1 | 4 => {}
    _ => unreachable!("selector decoder rejects unknown kinds"),
  }
  if used.iter().any(|used| !used) {
    return Err("value_store_unused_dependency");
  }
  Ok(())
}

fn dependency_at(dependencies: &[DependencyRecord], ordinal: u32) -> Result<&DependencyRecord, &'static str> {
  ordinal.checked_sub(1).and_then(|index| dependencies.get(index as usize)).ok_or("value_store_dependency_ordinal")
}

fn require_native_role(dependency: &DependencyRecord, role: u16) -> Result<(), &'static str> {
  if dependency.kind != 2
    || dependency.role != role
    || dependency.abi != 0
    || dependency.executor_profile != 1
    || dependency.artifact_kind != 0
    || dependency.artifact_length != 0
  {
    return Err("value_store_native_dependency");
  }
  Ok(())
}

fn require_wasm_role(dependency: &DependencyRecord, role: u16, family: u16, match_semantics: u16) -> Result<(), &'static str> {
  let expected_abi = match (role, family) {
    (1, 1) => 3,
    (1, 2) => 1,
    (2, 1) => 4,
    (2, 2) => 2,
    _ => return Err("value_store_wasm_dependency"),
  };
  if dependency.kind != 1
    || dependency.role != role
    || dependency.abi != expected_abi
    || dependency.executor_profile != if family == 1 { 2 } else { 3 }
    || dependency.artifact_kind != 1
    || dependency.artifact_length == 0
    || (role == 1 && match_semantics != if family == 1 { 1 } else { 2 } && match_semantics != 0)
  {
    return Err("value_store_wasm_dependency");
  }
  Ok(())
}

fn metadata_field_name(metadata_id: u16) -> &'static str {
  match metadata_id {
    1 => "@path",
    2 => "@filename",
    3 => "@extension",
    4 => "@content_type",
    5 => "@size",
    6 => "@created_at",
    7 => "@updated_at",
    8 => "@hash",
    _ => unreachable!("selector decoder rejects unknown metadata IDs"),
  }
}

fn value_store_id(profile: HashProfile, bytes: &[u8]) -> String {
  hex::encode(value_store_id_bytes(profile, bytes))
}

pub(crate) fn value_store_id_bytes(profile: HashProfile, bytes: &[u8]) -> Vec<u8> {
  let mut preimage = b"aeordb.index.value-store-definition.v1\0".to_vec();
  preimage.extend_from_slice(bytes);
  profile.digest(&preimage)
}

pub(crate) fn validate_value_store_definition(profile: HashProfile, bytes: &[u8]) -> Result<(), &'static str> {
  decode_definition(profile, bytes).map(|_| ())
}

pub(crate) fn sample_value_store_definition_for_scope(profile: HashProfile, scope_id: &[u8]) -> Vec<u8> {
  let mut definition = metadata_definition(profile, 1);
  definition.scope_id = scope_id.to_vec();
  build_definition(profile, &definition).expect("sample ValueStore definition with exact ScopeId")
}

fn checked_end(start: usize, length: usize, available: usize) -> Result<usize, &'static str> {
  let end = start.checked_add(length).ok_or("value_store_length_overflow")?;
  if end > available {
    return Err("value_store_child_truncated");
  }
  Ok(end)
}

fn sequence_bytes(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn fixture_id(profile: HashProfile, suffix: &str) -> &'static str {
  match (profile, suffix) {
    (HashProfile::Blake3_256, "metadata-hash-corrected") => "avst-blake3-256-metadata-hash-corrected-valid",
    (HashProfile::Blake3_256, "metadata-created-at-legacy") => "avst-blake3-256-metadata-created-at-legacy-valid",
    (HashProfile::Blake3_256, "json-corrected") => "avst-blake3-256-json-corrected-valid",
    (HashProfile::Blake3_256, "mapper-corrected") => "avst-blake3-256-mapper-corrected-valid",
    (HashProfile::Blake3_256, "json-legacy") => "avst-blake3-256-json-legacy-valid",
    (HashProfile::Blake3_256, "mapper-legacy") => "avst-blake3-256-mapper-legacy-valid",
    (HashProfile::Blake3_256, "always-missing-legacy") => "avst-blake3-256-always-missing-legacy-valid",
    (HashProfile::Sha512, "metadata-hash-corrected") => "avst-sha512-metadata-hash-corrected-valid",
    (HashProfile::Sha512, "metadata-created-at-legacy") => "avst-sha512-metadata-created-at-legacy-valid",
    (HashProfile::Sha512, "json-corrected") => "avst-sha512-json-corrected-valid",
    (HashProfile::Sha512, "mapper-corrected") => "avst-sha512-mapper-corrected-valid",
    (HashProfile::Sha512, "json-legacy") => "avst-sha512-json-legacy-valid",
    (HashProfile::Sha512, "mapper-legacy") => "avst-sha512-mapper-legacy-valid",
    (HashProfile::Sha512, "always-missing-legacy") => "avst-sha512-always-missing-legacy-valid",
    _ => unreachable!("fixture suffixes are fixed"),
  }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  Ok(u16::from_le_bytes(bytes.get(offset..offset + 2).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  Ok(u32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  Ok(u64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn value_store_fixtures_match_expected_context_and_ids() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {}", case.id);
    }
  }

  #[test]
  fn envelope_lengths_reserves_scope_and_children_fail_closed() {
    let case = fixture_cases().remove(0);
    for length in [0, 32, 112 + case.profile.width()] {
      assert_eq!(decode_definition(case.profile, &case.bytes[..length]).err(), Some("value_store_length"));
    }
    let mut reserved = case.bytes.clone();
    reserved[16] = 1;
    assert_eq!(decode_definition(case.profile, &reserved).err(), Some("value_store_reserved"));
    let mut fixed_reserved = case.bytes.clone();
    fixed_reserved[32 + case.profile.width() + 40] = 1;
    assert_eq!(decode_definition(case.profile, &fixed_reserved).err(), Some("value_store_reserved"));
    let mut scope = case.bytes.clone();
    scope[32..32 + case.profile.width()].fill(0);
    assert_eq!(decode_definition(case.profile, &scope).err(), Some("value_store_scope_id"));
    let mut selector_length = case.bytes;
    put_u32(&mut selector_length, 32 + case.profile.width() + 4, (MAX_SELECTOR_LENGTH + 1) as u32);
    assert_eq!(decode_definition(case.profile, &selector_length).err(), Some("value_store_child_length"));
  }

  #[test]
  fn metadata_name_parser_and_semantic_family_must_agree() {
    let corrected = fixture_cases().remove(0);
    let fixed = 32 + corrected.profile.width();
    let mut metadata = corrected.bytes.clone();
    let field_start = fixed + FIXED_BODY_WITHOUT_SCOPE;
    metadata[field_start..field_start + 5].copy_from_slice(b"@path");
    assert_eq!(decode_definition(corrected.profile, &metadata).err(), Some("value_store_metadata_context"));

    let mut mixed = corrected.bytes.clone();
    put_u16(&mut mixed, fixed + 26, 2);
    assert_eq!(decode_definition(corrected.profile, &mixed).err(), Some("value_store_semantic_family"));

    let mut input = corrected.bytes;
    put_u64(&mut input, fixed + 56, 1);
    assert_eq!(decode_definition(corrected.profile, &input).err(), Some("value_store_metadata_context"));
  }

  #[test]
  fn corrected_limits_reject_unbounded_sentinels_and_selector_context_mismatch() {
    let corrected = fixture_cases().remove(2);
    let fixed = 32 + corrected.profile.width();
    let mut unbounded = corrected.bytes.clone();
    put_u64(&mut unbounded, fixed + 64, u64::MAX);
    assert_eq!(decode_definition(corrected.profile, &unbounded).err(), Some("value_store_corrected_limit"));

    let mapper = fixture_cases().remove(3);
    let mapper_fixed = 32 + mapper.profile.width();
    let mut work = mapper.bytes;
    put_u64(&mut work, mapper_fixed + 64, 1);
    assert_eq!(decode_definition(mapper.profile, &work).err(), Some("value_store_mapper_context"));
  }

  #[test]
  fn dependency_ordinals_roles_and_unused_records_fail_closed() {
    let corrected = fixture_cases().remove(2);
    let fixed = 32 + corrected.profile.width();
    let field_length = read_u32(&corrected.bytes, fixed).unwrap() as usize;
    let selector_length = read_u32(&corrected.bytes, fixed + 4).unwrap() as usize;
    let parser_start = fixed + FIXED_BODY_WITHOUT_SCOPE + field_length + selector_length;
    let mut ordinal = corrected.bytes.clone();
    put_u32(&mut ordinal, parser_start + 48 + 8, 99);
    assert_eq!(decode_definition(corrected.profile, &ordinal).err(), Some("value_store_dependency_ordinal"));

    let first_candidate_length = read_u32(&corrected.bytes, parser_start + 48).unwrap() as usize;
    let raw_json_candidate = parser_start + 48 + first_candidate_length;
    let mut wrong_role = corrected.bytes.clone();
    put_u32(&mut wrong_role, raw_json_candidate + 8, 4);
    assert_eq!(decode_definition(corrected.profile, &wrong_role).err(), Some("value_store_native_dependency"));

    let mut definition = json_definition(corrected.profile, false, false);
    let mut dependencies = json_dependencies(false, true);
    dependencies.push(native_dependency(4, "/org/aeordev/aeordb/native/unused-selector-v1"));
    definition.dependencies = dependency::build_table(&dependencies).unwrap();
    assert_eq!(build_definition(corrected.profile, &definition).err(), Some("value_store_selector_dependency"));

    let mut mapper_with_unused = mapper_definition(corrected.profile, false);
    let mapper_dependencies = vec![
      wasm_dependency(1, false, "/org/aeordev/aeordb/plugins/fixture-parser"),
      wasm_dependency(2, false, "/org/aeordev/aeordb/plugins/fixture-mapper"),
      wasm_dependency(2, false, "/org/aeordev/aeordb/plugins/unused-mapper"),
    ];
    mapper_with_unused.dependencies = dependency::build_table(&mapper_dependencies).unwrap();
    assert_eq!(build_definition(corrected.profile, &mapper_with_unused).err(), Some("value_store_unused_dependency"));

    let mut mapper_wrong_role = mapper_definition(corrected.profile, false);
    mapper_wrong_role.selector = selector::build_mapper(2, 1, &selector::canonical_null()).unwrap();
    assert_eq!(build_definition(corrected.profile, &mapper_wrong_role).err(), Some("value_store_wasm_dependency"));

    let mut explicit = mapper_definition(corrected.profile, false);
    explicit.parser_plan = parser::build_plan(&ParserPlan {
      kind: 2,
      resolution_semantics: 1,
      mime_semantics: 0,
      no_match_semantics: 0,
      mime_dependency_ordinal: 0,
      candidates: vec![Candidate {
        kind: 1,
        match_semantics: 0,
        dependency_ordinal: 1,
        match_bytes: Vec::new(),
        policy: policy::build_policy(PolicyKind::LegacyWasm),
      }],
    })
    .unwrap();
    assert_eq!(build_definition(corrected.profile, &explicit).err(), Some("value_store_candidate_policy"));
  }

  #[test]
  fn malformed_field_and_nested_records_are_rejected_without_allocation_amplification() {
    let corrected = fixture_cases().remove(2);
    let fixed = 32 + corrected.profile.width();
    let field_start = fixed + FIXED_BODY_WITHOUT_SCOPE;
    let mut nul = corrected.bytes.clone();
    nul[field_start] = 0;
    assert_eq!(decode_definition(corrected.profile, &nul).err(), Some("value_store_field_name"));

    let mut parser_length = corrected.bytes.clone();
    put_u32(&mut parser_length, fixed + 8, u32::MAX);
    assert_eq!(decode_definition(corrected.profile, &parser_length).err(), Some("value_store_child_length"));

    let mut trailing = corrected.bytes;
    trailing.push(0);
    let trailing_length = trailing.len() as u32;
    put_u32(&mut trailing, 8, trailing_length);
    assert_eq!(decode_definition(corrected.profile, &trailing).err(), Some("value_store_length_formula"));

    let oversized = vec![0u8; MAX_DEFINITION_LENGTH + 1];
    assert_eq!(decode_definition(corrected.profile, &oversized).err(), Some("value_store_length"));
  }

  #[test]
  fn every_value_store_fixture_byte_is_structural_or_identity_protected() {
    for case in fixture_cases() {
      let original_digest = case.profile.digest(&case.bytes);
      for index in 0..case.bytes.len() {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 1;
        let observed = observe(case.profile, &mutated).0;
        assert!(
          observed.starts_with("error:") || case.profile.digest(&mutated) != original_digest,
          "fixture {} byte {index} was not protected",
          case.id
        );
      }
    }
  }
}
