//! Compile one corrected parser/selector context against exact immutable pins.
//! Ordinals are assigned only after complete-record deduplication and sorting.

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::dependency::{
  DependencyRecordV1, InvocationPolicyKind, InvocationPolicyV1, compare_records, decode_dependency_record_bytes, encode_dependency_record,
  encode_dependency_table, encode_invocation_policy,
};
use super::native_semantics::NativeSemanticComponentV1;
use super::parser_plan::{ParserCandidateKind, ParserCandidateV1, ParserPlanKind, ParserResolutionPlanV1, encode_parser_resolution_plan};
use super::parser_registry_compiler::{CompiledParserRegistryV1, SemanticCompilationErrorV1};
use super::reader::{FormatError, MalformedInputClass};

const WORKSPACE_BYTES: usize = 4 * 1024 * 1024;
const CONTEXT_PATH: &str = "<parser-context>";
type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;

#[derive(Clone)]
pub enum ParserContextSourceV1<'a> {
  Metadata,
  /// The source compiler resolves aliases under its captured deployment guard.
  Explicit {
    dependency: DependencyRecordV1<'a>,
    policy: &'a InvocationPolicyV1,
  },
  Automatic {
    registry: &'a CompiledParserRegistryV1,
    registry_policy: Option<&'a InvocationPolicyV1>,
    raw_json_policy: &'a InvocationPolicyV1,
    native_suite_policy: &'a InvocationPolicyV1,
  },
}

#[derive(Clone)]
pub enum ParserSelectorDependencyV1<'a> {
  None,
  JsonPath,
  Mapper(DependencyRecordV1<'a>),
}

#[derive(Clone)]
pub struct ParserContextCompilationRequestV1<'a> {
  pub source: ParserContextSourceV1<'a>,
  pub selector_dependency: ParserSelectorDependencyV1<'a>,
  pub maximum_workspace_bytes: usize,
}

pub struct CompiledParserContextV1 {
  parser_plan: Vec<u8>,
  dependencies: Vec<u8>,
  selector_dependency_ordinal: Option<u32>,
  _memory: MemoryReservation,
}

impl CompiledParserContextV1 {
  pub fn parser_plan(&self) -> &[u8] {
    &self.parser_plan
  }

  pub fn dependencies(&self) -> &[u8] {
    &self.dependencies
  }

  /// None for metadata; exact Regex or mapper ordinal for ordinary sources.
  pub const fn selector_dependency_ordinal(&self) -> Option<u32> {
    self.selector_dependency_ordinal
  }
}

/// Build a single closed parser/selector dependency recipe. The selector writer
/// consumes its final ordinal; the ValueStore writer checks the complete child
/// closure. This compiler neither executes plugins nor activates namespace roots.
pub fn compile_parser_context_v1(
  request: ParserContextCompilationRequestV1<'_>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledParserContextV1> {
  cancelled(is_cancelled)?;
  if request.maximum_workspace_bytes < WORKSPACE_BYTES {
    return Err(resource("context workspace exceeds the caller's limit"));
  }
  // At most512 registry records +3 native parser records +1 selector. Records
  // borrow captured strings; output caps are256KiB table and128KiB program.
  // This conservative fixed charge also covers codec validation and scratch.
  let reservation =
    memory.reserve(MemoryOwner::Task, WORKSPACE_BYTES as u64, AdmissionClass::Workload).map_err(|error| resource(error.to_string()))?;
  let metadata = matches!(request.source, ParserContextSourceV1::Metadata);
  if metadata != matches!(request.selector_dependency, ParserSelectorDependencyV1::None) {
    return Err(invalid("only metadata has no selector dependency; ordinary contexts require JSON or mapper selection"));
  }
  let selector = match &request.selector_dependency {
    ParserSelectorDependencyV1::None => None,
    ParserSelectorDependencyV1::JsonPath => Some(NativeSemanticComponentV1::RegexSelector.dependency_record()),
    ParserSelectorDependencyV1::Mapper(record) => {
      validate_wasm(record, 2)?;
      Some(record.clone())
    }
  };
  let record_count = match &request.source {
    ParserContextSourceV1::Metadata => 0,
    ParserContextSourceV1::Explicit { .. } => 1,
    ParserContextSourceV1::Automatic { registry, .. } => registry.entries().len() + 3,
  } + usize::from(selector.is_some());
  let mut records = allocate(record_count)?;
  if let Some(record) = &selector {
    records.push(record.clone());
  }
  match &request.source {
    ParserContextSourceV1::Metadata => {}
    ParserContextSourceV1::Explicit { dependency, policy } => {
      validate_wasm(dependency, 1)?;
      validate_policy(policy, true)?;
      records.push(dependency.clone());
    }
    ParserContextSourceV1::Automatic { registry, registry_policy, raw_json_policy, native_suite_policy } => {
      if registry.entries().is_empty() != registry_policy.is_none() {
        return Err(invalid("a registry invocation policy is required exactly when the registry has candidates"));
      }
      if let Some(policy) = registry_policy {
        validate_policy(policy, true)?;
      }
      validate_policy(raw_json_policy, false)?;
      validate_policy(native_suite_policy, false)?;
      for entry in registry.entries() {
        check(&reservation, is_cancelled)?;
        let record = decode_dependency_record_bytes(entry.dependency_bytes()).map_err(format_error)?;
        validate_wasm(&record, 1)?;
        records.push(record);
      }
      for component in [NativeSemanticComponentV1::MimeRouter, NativeSemanticComponentV1::RawJson, NativeSemanticComponentV1::NativeSuite] {
        records.push(component.dependency_record());
      }
    }
  }
  check(&reservation, is_cancelled)?;
  records.sort_unstable_by(compare_records);
  records.dedup();
  let dependencies = encode_dependency_table(&records).map_err(format_error)?;
  let selector_dependency_ordinal = selector.as_ref().map(|record| ordinal(&records, record)).transpose()?;
  let mut plan = ParserResolutionPlanV1 {
    kind: ParserPlanKind::None,
    resolution_semantics: 0,
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: Vec::new(),
  };
  match &request.source {
    ParserContextSourceV1::Metadata => {}
    ParserContextSourceV1::Explicit { dependency, policy } => {
      plan.kind = ParserPlanKind::ExplicitPlugin;
      plan.resolution_semantics = 1;
      plan.candidates = allocate(1)?;
      plan.candidates.push(candidate(ParserCandidateKind::Explicit, ordinal(&records, dependency)?, b"", policy));
    }
    ParserContextSourceV1::Automatic { registry, registry_policy, raw_json_policy, native_suite_policy } => {
      plan.kind = ParserPlanKind::Automatic;
      plan.resolution_semantics = 1;
      plan.mime_semantics = 1;
      plan.no_match_semantics = 1;
      plan.mime_dependency_ordinal = ordinal(&records, &NativeSemanticComponentV1::MimeRouter.dependency_record())?;
      plan.candidates = allocate(registry.entries().len() + 2)?;
      for entry in registry.entries() {
        check(&reservation, is_cancelled)?;
        let record = decode_dependency_record_bytes(entry.dependency_bytes()).map_err(format_error)?;
        let policy = registry_policy.ok_or_else(|| invalid("registry policy disappeared after preflight"))?;
        plan.candidates.push(candidate(ParserCandidateKind::Registry, ordinal(&records, &record)?, entry.essence().as_bytes(), policy));
      }
      plan.candidates.push(candidate(
        ParserCandidateKind::RawJson,
        ordinal(&records, &NativeSemanticComponentV1::RawJson.dependency_record())?,
        b"",
        raw_json_policy,
      ));
      plan.candidates.push(candidate(
        ParserCandidateKind::NativeSuite,
        ordinal(&records, &NativeSemanticComponentV1::NativeSuite.dependency_record())?,
        b"",
        native_suite_policy,
      ));
    }
  }
  check(&reservation, is_cancelled)?;
  let parser_plan = encode_parser_resolution_plan(&plan).map_err(format_error)?;
  check(&reservation, is_cancelled)?;
  Ok(CompiledParserContextV1 { parser_plan, dependencies, selector_dependency_ordinal, _memory: reservation })
}

fn candidate<'a>(
  kind: ParserCandidateKind,
  dependency_ordinal: u32,
  match_bytes: &'a [u8],
  policy: &InvocationPolicyV1,
) -> ParserCandidateV1<'a> {
  ParserCandidateV1 {
    kind,
    dependency_ordinal,
    match_bytes,
    match_semantics: if kind == ParserCandidateKind::Registry { 1 } else { 0 },
    policy: policy.clone(),
  }
}

fn ordinal(records: &[DependencyRecordV1<'_>], wanted: &DependencyRecordV1<'_>) -> Result<u32> {
  let position = records
    .binary_search_by(|record| compare_records(record, wanted))
    .map_err(|insertion_position| invalid(format!("captured dependency is missing at canonical table position {insertion_position}")))?;
  u32::try_from(position + 1).map_err(|error| resource(error.to_string()))
}

fn validate_wasm(record: &DependencyRecordV1<'_>, role: u16) -> Result<()> {
  if record.kind != 1 || record.role != role || record.flags != 4 || record.abi != role + 2 || record.executor_profile != 2 {
    return Err(SemanticCompilationErrorV1::DependencyUnavailable {
      path: CONTEXT_PATH,
      message: "dependency is not the corrected role/ABI/executor".into(),
    });
  }
  encode_dependency_record(record).map_err(format_error)?;
  Ok(())
}

pub(crate) fn validate_policy(policy: &InvocationPolicyV1, wasm: bool) -> Result<()> {
  let expected = if wasm { InvocationPolicyKind::PureWasm } else { InvocationPolicyKind::Native };
  if policy.kind != expected {
    return Err(invalid("invocation backend is not the corrected policy for this call site"));
  }
  encode_invocation_policy(policy).map_err(format_error)?;
  if wasm
    && (policy.max_request_bytes > 64 * 1024 * 1024
      || policy.max_response_bytes > 16 * 1024 * 1024
      || policy.max_linear_memory_bytes > 64 * 1024 * 1024
      || policy.max_fuel > 10_000_000)
  {
    return Err(invalid("corrected WASM invocation exceeds the frozen public semantic maximum"));
  }
  Ok(())
}

fn allocate<T>(capacity: usize) -> Result<Vec<T>> {
  let mut values = Vec::new();
  values.try_reserve_exact(capacity).map_err(|error| resource(error.to_string()))?;
  Ok(values)
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
  SemanticCompilationErrorV1::InvalidSource { path: CONTEXT_PATH, message: message.into() }
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: CONTEXT_PATH, message: message.into() }
}
