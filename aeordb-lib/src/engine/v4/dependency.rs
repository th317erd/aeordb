use std::cmp::Ordering;

use super::reader::{FormatError, FormatResult, MalformedInputClass};

const POLICY_LENGTH: usize = 128;
const WASM32_ADDRESS_SPACE: u64 = 1u64 << 32;
const WASM_PAGE_SIZE: u64 = 64 * 1_024;
const TABLE_HEADER_LENGTH: usize = 32;
const RECORD_HEADER_LENGTH: usize = 96;
const TABLE_MAX_LENGTH: usize = 256 * 1_024;
const MAX_RECORDS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationPolicyKind {
  Native,
  PureWasm,
  LegacyWasm,
}

impl InvocationPolicyKind {
  pub fn name(self) -> &'static str {
    match self {
      Self::Native => "native",
      Self::PureWasm => "pure-wasm32",
      Self::LegacyWasm => "legacy-wasm32",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationPolicyV1 {
  pub kind: InvocationPolicyKind,
  pub max_request_bytes: u64,
  pub max_response_bytes: u64,
  pub max_linear_memory_bytes: u64,
  pub max_fuel: u64,
  pub max_table_elements: u64,
  pub max_structure_nodes: u64,
  pub max_scalar_bytes: u64,
  pub max_structure_depth: u32,
  pub max_container_members: u32,
  pub max_wasm_instances: u32,
  pub max_wasm_memories: u32,
  pub max_wasm_tables: u32,
  pub max_value_stack_height: u32,
  pub max_recursion_depth: u32,
}

impl InvocationPolicyV1 {
  pub fn name(&self) -> &'static str {
    self.kind.name()
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyRecordV1<'a> {
  pub kind: u16,
  pub role: u16,
  pub flags: u32,
  pub abi: u16,
  pub executor_profile: u16,
  pub fingerprint_semantics: u16,
  pub artifact_kind: u16,
  pub artifact_length: u64,
  pub fingerprint: [u8; 32],
  pub dependency_id: &'a str,
  pub version: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyTableV1<'a> {
  pub records: Vec<DependencyRecordV1<'a>>,
}

pub fn decode_invocation_policy(value: &[u8]) -> FormatResult<InvocationPolicyV1> {
  if value.len() != POLICY_LENGTH {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "policy_length",
      format!("expected {POLICY_LENGTH}, got {}", value.len()),
    ));
  }
  if &value[..4] != b"AIVP"
    || u16_at(value, 4) != 1
    || u16_at(value, 6) as usize != POLICY_LENGTH
    || u32_at(value, 8) as usize != POLICY_LENGTH
  {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "policy_envelope", "expected fixed AIVP v1 envelope"));
  }
  if u32_at(value, 12) != 0 || value[108..].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "policy_reserved", "policy flags or reserve are nonzero"));
  }
  if u16_at(value, 20) != 1 || u16_at(value, 22) != 1 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "policy_semantics",
      format!("limit {}, structure {}", u16_at(value, 20), u16_at(value, 22)),
    ));
  }

  let policy = InvocationPolicyV1 {
    kind: InvocationPolicyKind::Native,
    max_request_bytes: u64_at(value, 24),
    max_response_bytes: u64_at(value, 32),
    max_linear_memory_bytes: u64_at(value, 40),
    max_fuel: u64_at(value, 48),
    max_table_elements: u64_at(value, 56),
    max_structure_nodes: u64_at(value, 64),
    max_scalar_bytes: u64_at(value, 72),
    max_structure_depth: u32_at(value, 80),
    max_container_members: u32_at(value, 84),
    max_wasm_instances: u32_at(value, 88),
    max_wasm_memories: u32_at(value, 92),
    max_wasm_tables: u32_at(value, 96),
    max_value_stack_height: u32_at(value, 100),
    max_recursion_depth: u32_at(value, 104),
  };
  if [policy.max_response_bytes, policy.max_structure_nodes, policy.max_scalar_bytes].contains(&0)
    || [policy.max_structure_depth, policy.max_container_members, policy.max_value_stack_height, policy.max_recursion_depth].contains(&0)
    || [policy.max_response_bytes, policy.max_structure_nodes, policy.max_scalar_bytes].contains(&u64::MAX)
    || [policy.max_structure_depth, policy.max_container_members, policy.max_value_stack_height, policy.max_recursion_depth]
      .contains(&u32::MAX)
  {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "policy_common_limits",
      "common structural limits must be finite and nonzero",
    ));
  }

  let kind = match (u16_at(value, 16), u16_at(value, 18)) {
    (1, 0) => {
      if policy.max_request_bytes != 0
        || [policy.max_linear_memory_bytes, policy.max_fuel, policy.max_table_elements].iter().any(|limit| *limit != 0)
        || [policy.max_wasm_instances, policy.max_wasm_memories, policy.max_wasm_tables].iter().any(|limit| *limit != 0)
      {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "policy_native_context",
          "native policy contains WASM-only limits",
        ));
      }
      InvocationPolicyKind::Native
    }
    (2, host @ (1 | 2)) => {
      if [policy.max_request_bytes, policy.max_linear_memory_bytes, policy.max_fuel, policy.max_table_elements].contains(&0)
        || [policy.max_request_bytes, policy.max_linear_memory_bytes, policy.max_fuel, policy.max_table_elements].contains(&u64::MAX)
        || [policy.max_wasm_instances, policy.max_wasm_memories, policy.max_wasm_tables].contains(&0)
        || [policy.max_wasm_instances, policy.max_wasm_memories, policy.max_wasm_tables].contains(&u32::MAX)
        || policy.max_linear_memory_bytes > WASM32_ADDRESS_SPACE
        || policy.max_linear_memory_bytes % WASM_PAGE_SIZE != 0
      {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "policy_wasm_context",
          "WASM resource limits are zero, unlimited, unaligned, or outside wasm32",
        ));
      }
      if host == 1 {
        InvocationPolicyKind::PureWasm
      } else {
        InvocationPolicyKind::LegacyWasm
      }
    }
    (1 | 2, host) => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "policy_host_profile", format!("host profile {host}")));
    }
    (backend, _) => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "policy_backend", format!("execution backend {backend}")));
    }
  };
  Ok(InvocationPolicyV1 { kind, ..policy })
}

pub fn decode_dependency_table(value: &[u8]) -> FormatResult<DependencyTableV1<'_>> {
  if value.len() < TABLE_HEADER_LENGTH {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "dependency_table_length", "dependency table header is truncated"));
  }
  if value.len() > TABLE_MAX_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "dependency_table_length",
      format!("{} bytes exceeds {TABLE_MAX_LENGTH}", value.len()),
    ));
  }
  if &value[..4] != b"ADPT" || u16_at(value, 4) != 1 || u16_at(value, 6) as usize != TABLE_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "dependency_table_envelope", "expected ADPT v1 with 32-byte header"));
  }
  if u32_at(value, 8) as usize != value.len() || u32_at(value, 20) as usize != value.len() - TABLE_HEADER_LENGTH {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "dependency_table_length_disagreement",
      format!("total {}, records {}", u32_at(value, 8), u32_at(value, 20)),
    ));
  }
  if u32_at(value, 12) != 0 || value[24..32].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "dependency_table_reserved", "table flags or reserve are nonzero"));
  }
  let count = u32_at(value, 16) as usize;
  if count > MAX_RECORDS {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "dependency_record_count",
      format!("{count} records exceeds {MAX_RECORDS}"),
    ));
  }

  let mut cursor = TABLE_HEADER_LENGTH;
  let mut previous: Option<DependencyRecordV1<'_>> = None;
  let mut records = Vec::with_capacity(count);
  for _ in 0..count {
    let (record, next) = decode_dependency_record(value, cursor)?;
    if previous.as_ref().is_some_and(|prior| compare_records(prior, &record) != Ordering::Less) {
      return Err(error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "dependency_record_order", "records are not strictly ordered"));
    }
    previous = Some(record.clone());
    records.push(record);
    cursor = next;
  }
  if cursor != value.len() {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "dependency_record_count_mismatch",
      format!("records end at {cursor}, table ends at {}", value.len()),
    ));
  }
  Ok(DependencyTableV1 { records })
}

fn decode_dependency_record(value: &[u8], start: usize) -> FormatResult<(DependencyRecordV1<'_>, usize)> {
  let header_end = start.checked_add(RECORD_HEADER_LENGTH).ok_or_else(|| length_error("dependency record header overflow"))?;
  if header_end > value.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "dependency_record_truncated", "record header is truncated"));
  }
  let total_length = u32_at(value, start) as usize;
  let end = start.checked_add(total_length).ok_or_else(|| length_error("dependency record end overflow"))?;
  let id_length = u32_at(value, start + 20) as usize;
  let version_length = u32_at(value, start + 24) as usize;
  let expected_length = RECORD_HEADER_LENGTH
    .checked_add(id_length)
    .and_then(|length| length.checked_add(version_length))
    .ok_or_else(|| length_error("dependency record length overflow"))?;
  if total_length != expected_length || end > value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "dependency_record_length",
      format!("declared {total_length}, expected {expected_length}, end {end}"),
    ));
  }
  if id_length == 0 || id_length > 4_096 || version_length > 256 {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "dependency_record_component_length",
      format!("ID {id_length}, version {version_length}"),
    ));
  }
  if u32_at(value, start + 28) != 0 || value[start + 72..header_end].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "dependency_record_reserved", "record reserve is nonzero"));
  }

  let id_end = header_end + id_length;
  let dependency_id = std::str::from_utf8(&value[header_end..id_end]).map_err(|source| {
    error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "dependency_id_utf8", format!("invalid UTF-8: {source}"))
  })?;
  let version = std::str::from_utf8(&value[id_end..end]).map_err(|source| {
    error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "dependency_version_utf8", format!("invalid UTF-8: {source}"))
  })?;
  let record = DependencyRecordV1 {
    kind: u16_at(value, start + 4),
    role: u16_at(value, start + 6),
    flags: u32_at(value, start + 8),
    abi: u16_at(value, start + 12),
    executor_profile: u16_at(value, start + 14),
    fingerprint_semantics: u16_at(value, start + 16),
    artifact_kind: u16_at(value, start + 18),
    artifact_length: u64_at(value, start + 32),
    fingerprint: value[start + 40..start + 72].try_into().expect("fixed dependency fingerprint"),
    dependency_id,
    version,
  };
  validate_dependency_record(&record)?;
  Ok((record, end))
}

fn validate_dependency_record(record: &DependencyRecordV1<'_>) -> FormatResult<()> {
  if record.flags & !0x07 != 0 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "dependency_flags", format!("flags {:#010x}", record.flags)));
  }
  if record.fingerprint.iter().all(|byte| *byte == 0) {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "dependency_zero_fingerprint", "fingerprint is zero"));
  }
  let version_absent = record.flags & 0x01 != 0;
  let opaque_id = record.flags & 0x02 != 0;
  let artifact_required = record.flags & 0x04 != 0;
  if version_absent != record.version.is_empty() || (!version_absent && !is_canonical_semver(record.version)) {
    return Err(error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "dependency_version", "version is not canonical SemVer"));
  }
  if !opaque_id && !is_canonical_dependency_id(record.dependency_id) {
    return Err(error(
      MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
      "dependency_id",
      "dependency ID is not a canonical absolute ID",
    ));
  }
  match record.kind {
    1 => {
      let corrected = matches!((record.role, record.abi, record.executor_profile), (1, 3, 2) | (2, 4, 2));
      let legacy = matches!((record.role, record.abi, record.executor_profile), (1, 1, 3) | (2, 2, 3));
      if (!corrected && !legacy)
        || record.fingerprint_semantics != 1
        || record.artifact_kind != 1
        || record.artifact_length == 0
        || !artifact_required
      {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "dependency_wasm_contract",
          "WASM role, ABI, executor, fingerprint, or artifact fields disagree",
        ));
      }
    }
    2 => {
      if !matches!(record.role, 1 | 3 | 4)
        || record.abi != 0
        || record.executor_profile != 1
        || record.fingerprint_semantics != 2
        || record.artifact_kind != 0
        || record.artifact_length != 0
        || record.flags != 0
      {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "dependency_native_contract",
          "native role, ABI, executor, fingerprint, artifact, or flags disagree",
        ));
      }
    }
    kind => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "dependency_kind", format!("kind {kind}")));
    }
  }
  Ok(())
}

fn compare_records(left: &DependencyRecordV1<'_>, right: &DependencyRecordV1<'_>) -> Ordering {
  (
    left.kind,
    left.role,
    left.dependency_id.as_bytes(),
    u8::from(!left.version.is_empty()),
    left.version.as_bytes(),
    left.abi,
    left.executor_profile,
    left.fingerprint_semantics,
    left.fingerprint,
    left.artifact_kind,
    left.artifact_length,
    left.flags,
  )
    .cmp(&(
      right.kind,
      right.role,
      right.dependency_id.as_bytes(),
      u8::from(!right.version.is_empty()),
      right.version.as_bytes(),
      right.abi,
      right.executor_profile,
      right.fingerprint_semantics,
      right.fingerprint,
      right.artifact_kind,
      right.artifact_length,
      right.flags,
    ))
}

fn is_canonical_dependency_id(value: &str) -> bool {
  value.starts_with('/')
    && value.len() <= 4_096
    && value.split('/').skip(1).all(|segment| {
      !segment.is_empty() && !matches!(segment, "." | "..") && !segment.chars().any(|character| character == '\0' || character.is_control())
    })
}

fn is_canonical_semver(value: &str) -> bool {
  semver::Version::parse(value).is_ok_and(|version| version.to_string() == value)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("validated dependency bounds"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("validated dependency bounds"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("validated dependency bounds"))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "dependency_length_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
