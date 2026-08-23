//! Production parser executor for the native v4 index runtime.

use std::collections::BTreeMap;
use std::fmt;
use std::mem::size_of;

use crate::engine::errors::EngineError;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_parsers::{
  CorrectedNativeParserErrorV1, CorrectedNativeParserLimitsV1, native_parser_claims_corrected, native_parser_claims_legacy,
  native_parser_expands_archive_corrected, parse_native, parse_native_corrected,
};
use crate::engine::path_utils::{file_name, normalize_path};
use crate::engine::storage_engine::StorageEngine;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use super::config_value::{
  CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1, CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value,
};
use super::dependency::{DependencyRecordV1, DependencyTableV1, InvocationPolicyV1};
use super::index_producer_collector::{
  IndexParserDeterministicFailureV1, IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1,
  IndexParserOutcomeV1,
};
use super::parser_plan::{ParserCandidateKind, ParserCandidateV1, ParserPlanKind};

const MIME_ROUTER_ID: &str = "/org/aeordev/aeordb/native/mime-router-v1";
const RAW_JSON_ID: &str = "/org/aeordev/aeordb/native/raw-json-v1";
const NATIVE_SUITE_ID: &str = "/org/aeordev/aeordb/native/native-suite-v1";
const NATIVE_VERSION: &str = "1.0.0";
const BODY_FIXED_BYTES: u64 = 4 * 1_024;
const PARSER_WORKSPACE_MULTIPLIER: u64 = 4;
const CORRECTED_ARCHIVE_EXPANSION_MULTIPLIER: u64 = 4;
const CORRECTED_ARCHIVE_WORKSPACE_MULTIPLIER: u64 = 4;

pub struct NativeIndexParserExecutorV1<'engine> {
  engine: &'engine StorageEngine,
}

impl<'engine> NativeIndexParserExecutorV1<'engine> {
  pub const fn new(engine: &'engine StorageEngine) -> Self {
    Self { engine }
  }

  fn parse_automatic(&self, request: &IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    let plan = request.parser_plan();
    require_native_dependency(request.dependencies(), plan.mime_dependency_ordinal, MIME_ROUTER_ID, 3)?;
    let stored_content_type = request.file_record().content_type.as_deref();
    let corrected = plan.resolution_semantics == 1;
    let mime_essence = corrected.then(|| corrected_mime_essence(stored_content_type)).flatten();

    for candidate in plan.candidates.iter().filter(|candidate| candidate.kind == ParserCandidateKind::Registry) {
      let matched = if corrected {
        mime_essence.as_deref().is_some_and(|essence| essence.as_bytes() == candidate.match_bytes)
      } else {
        stored_content_type.is_some_and(|content_type| content_type.as_bytes() == candidate.match_bytes)
      };
      if matched {
        let dependency = dependency_at(request.dependencies(), candidate.dependency_ordinal)?;
        return Err(IndexParserExecutionErrorV1::dependency_unavailable(
          "native_parser_wasm_unavailable",
          format!(
            "exact parser dependency {} selected for path {} by stored MIME {:?} and corrected essence {:?} is not installed in the v4 executor",
            dependency.dependency_id,
            request.path(),
            stored_content_type,
            mime_essence,
          ),
        ));
      }
    }

    let raw = plan
      .candidates
      .iter()
      .find(|candidate| candidate.kind == ParserCandidateKind::RawJson)
      .ok_or_else(|| host_failure("native_parser_plan", "automatic parser plan has no raw-JSON candidate"))?;
    require_native_dependency(request.dependencies(), raw.dependency_ordinal, RAW_JSON_ID, 1)?;
    let native = plan
      .candidates
      .iter()
      .find(|candidate| candidate.kind == ParserCandidateKind::NativeSuite)
      .ok_or_else(|| host_failure("native_parser_plan", "automatic parser plan has no native-suite candidate"))?;
    require_native_dependency(request.dependencies(), native.dependency_ordinal, NATIVE_SUITE_ID, 1)?;

    let filename = required_filename(request)?;
    let extension = if corrected { corrected_extension(filename) } else { None };
    let native_claims = if corrected {
      native_parser_claims_corrected(mime_essence.as_deref(), extension.as_deref())
    } else {
      let content_type = content_type_or_empty(stored_content_type);
      native_parser_claims_legacy(content_type, filename, request.path())
    };
    let expands_archive = corrected && native_parser_expands_archive_corrected(mime_essence.as_deref(), extension.as_deref());
    let workspace_bytes =
      parser_workspace_bytes(request.file_record().total_size, corrected, expands_archive, &raw.policy, &native.policy)?;
    let body = self.read_body(request, workspace_bytes)?;
    match parse_raw_json(&body, raw, corrected, is_json_media_type(mime_essence.as_deref(), stored_content_type, corrected))? {
      RawJsonAttemptV1::Parsed(value) => return Ok(IndexParserOutcomeV1::Parsed(value)),
      RawJsonAttemptV1::Deterministic(outcome) => return Ok(outcome),
      RawJsonAttemptV1::NotClaimed => {}
    }
    if !native_claims {
      return Ok(IndexParserOutcomeV1::NotApplicable);
    }
    self.parse_native_candidate(request, native, &body, mime_essence.as_deref(), extension.as_deref(), corrected)
  }

  fn parse_native_candidate(
    &self,
    request: &IndexParserExecutionRequestV1<'_>,
    candidate: &ParserCandidateV1<'_>,
    body: &[u8],
    mime_essence: Option<&str>,
    extension: Option<&str>,
    corrected: bool,
  ) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    check_cancelled(request, "native_parser_cancelled_before_native")?;
    let filename = required_filename(request)?;
    let stored_content_type = content_type_or_empty(request.file_record().content_type.as_deref());
    let parsed = if corrected {
      parse_native_corrected(
        body,
        mime_essence,
        extension,
        filename,
        stored_content_type,
        request.file_record().total_size,
        CorrectedNativeParserLimitsV1::new(
          corrected_archive_expansion_bytes(&candidate.policy)?,
          candidate.policy.max_response_bytes,
          candidate.policy.max_structure_nodes,
          candidate.policy.max_scalar_bytes,
          candidate.policy.max_container_members,
        ),
      )
    } else {
      parse_native(body, stored_content_type, filename, request.path(), request.file_record().total_size)
        .map(|result| result.map_err(CorrectedNativeParserErrorV1::Malformed))
    };
    check_cancelled(request, "native_parser_cancelled_after_native")?;
    match parsed {
      Some(Ok(value)) => finish_native_value(value, &candidate.policy, corrected),
      Some(Err(CorrectedNativeParserErrorV1::PolicyLimit { observed })) if corrected => {
        deterministic_policy("native_parser_expansion_limit", observed)
      }
      Some(Err(CorrectedNativeParserErrorV1::Host(context))) if corrected => Err(host_failure("native_parser_corrected_host", context)),
      Some(Err(CorrectedNativeParserErrorV1::Malformed(error))) if corrected => {
        drop(error);
        deterministic_malformed("native_parser_rejected", body.len() as u64)
      }
      Some(Err(error)) => {
        // Migration v0 intentionally preserves the old silent native-parser skip.
        drop(error);
        Ok(IndexParserOutcomeV1::NotApplicable)
      }
      None => Ok(IndexParserOutcomeV1::NotApplicable),
    }
  }

  fn read_body(
    &self,
    request: &IndexParserExecutionRequestV1<'_>,
    workspace_bytes: u64,
  ) -> Result<ParserBodyV1, IndexParserExecutionErrorV1> {
    check_cancelled(request, "native_parser_cancelled_before_body")?;
    let record = request.file_record();
    if request.maximum_document_input_bytes() == 0 {
      return Err(host_failure("native_parser_input_authority", "parser request has no document-input authority"));
    }
    let expected_size = usize::try_from(record.total_size)
      .map_err(|error| host_failure("native_parser_input_platform", format!("document size does not fit this platform: {error}")))?;
    let retained_bytes = record
      .total_size
      .checked_mul(2)
      .and_then(|bytes| bytes.checked_add(BODY_FIXED_BYTES))
      .ok_or_else(|| host_failure("native_parser_body_accounting", "body reservation overflowed"))?;
    let reservation = self
      .engine
      .memory_coordinator()
      .reserve(MemoryOwner::StreamingRead, retained_bytes.max(1), AdmissionClass::Maintenance)
      .map_err(map_body_memory_error)?;
    let workspace = self
      .engine
      .memory_coordinator()
      .reserve(MemoryOwner::ParserPlugin, workspace_bytes.max(1), AdmissionClass::Maintenance)
      .map_err(map_parser_memory_error)?;
    let mut body = Vec::new();
    body
      .try_reserve_exact(expected_size)
      .map_err(|error| host_failure("native_parser_body_allocation", format!("cannot reserve bounded file body: {error}")))?;
    let mut hasher = self.engine.hash_algo().incremental_hasher().map_err(map_engine_error)?;
    for chunk_hash in &record.chunk_hashes {
      check_cancelled(request, "native_parser_cancelled_during_body")?;
      if chunk_hash.len() != self.engine.hash_algo().hash_length() {
        return Err(host_failure("native_parser_chunk_hash", "FileRecord contains a foreign-width chunk hash"));
      }
      let remaining = expected_size
        .checked_sub(body.len())
        .ok_or_else(|| host_failure("native_parser_body_length", "decoded body already exceeds the FileRecord size"))?;
      let chunk = self
        .engine
        .read_chunk_verified_including_deleted_bounded(chunk_hash, remaining)
        .map_err(map_engine_error)?
        .ok_or_else(|| host_failure("native_parser_chunk_missing", format!("required chunk {} is missing", hex::encode(chunk_hash))))?;
      let next =
        body.len().checked_add(chunk.len()).ok_or_else(|| host_failure("native_parser_body_length", "decoded body length overflowed"))?;
      if next > expected_size {
        return Err(host_failure("native_parser_body_length", "decoded body exceeds the FileRecord size"));
      }
      hasher.update(&chunk);
      body.extend_from_slice(&chunk);
    }
    if body.len() != expected_size {
      return Err(host_failure(
        "native_parser_body_length",
        format!("decoded body has {} bytes; FileRecord declares {expected_size}", body.len()),
      ));
    }
    if !record.content_hash.is_empty()
      && (record.content_hash.len() != self.engine.hash_algo().hash_length() || hasher.finalize() != record.content_hash)
    {
      return Err(host_failure("native_parser_content_hash", "decoded body does not match the FileRecord whole-content hash"));
    }
    check_cancelled(request, "native_parser_cancelled_after_body")?;
    Ok(ParserBodyV1 { bytes: body, _body_reservation: reservation, _workspace_reservation: workspace })
  }
}

impl IndexParserExecutorV1 for NativeIndexParserExecutorV1<'_> {
  fn parse(&self, request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    validate_request(self.engine, &request)?;
    check_cancelled(&request, "native_parser_cancelled")?;
    if request.file_record().total_size > request.maximum_document_input_bytes() || request.file_record().total_size > u32::MAX as u64 {
      return deterministic_policy("document_input_limit", request.file_record().total_size);
    }
    match request.parser_plan().kind {
      ParserPlanKind::None => Err(host_failure("native_parser_plan", "parser executor received a parser-free plan")),
      ParserPlanKind::ExplicitPlugin => {
        let candidate = request
          .parser_plan()
          .candidates
          .first()
          .ok_or_else(|| host_failure("native_parser_plan", "explicit parser plan has no candidate"))?;
        unavailable_wasm_candidate(request.dependencies(), candidate)
      }
      ParserPlanKind::Automatic => self.parse_automatic(&request),
    }
  }
}

fn corrected_archive_expansion_bytes(policy: &InvocationPolicyV1) -> Result<u64, IndexParserExecutionErrorV1> {
  policy
    .max_response_bytes
    .checked_mul(CORRECTED_ARCHIVE_EXPANSION_MULTIPLIER)
    .ok_or_else(|| host_failure("native_parser_expansion_accounting", "corrected archive expansion limit overflowed"))
}

fn parser_workspace_bytes(
  body_bytes: u64,
  corrected: bool,
  expands_archive: bool,
  raw_policy: &InvocationPolicyV1,
  native_policy: &InvocationPolicyV1,
) -> Result<u64, IndexParserExecutionErrorV1> {
  let body_workspace = body_bytes
    .checked_mul(PARSER_WORKSPACE_MULTIPLIER)
    .and_then(|bytes| bytes.checked_add(BODY_FIXED_BYTES))
    .ok_or_else(|| host_failure("native_parser_workspace_accounting", "body-relative parser workspace overflowed"))?;
  if !corrected {
    return Ok(body_workspace);
  }
  let archive_workspace = if expands_archive {
    corrected_archive_expansion_bytes(native_policy)?
      .checked_mul(CORRECTED_ARCHIVE_WORKSPACE_MULTIPLIER)
      .and_then(|bytes| bytes.checked_add(BODY_FIXED_BYTES))
      .ok_or_else(|| host_failure("native_parser_workspace_accounting", "corrected archive workspace overflowed"))?
  } else {
    0
  };
  let json_workspace = raw_policy
    .max_structure_nodes
    .checked_mul(CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1)
    .and_then(|bytes| bytes.checked_add(raw_policy.max_response_bytes))
    .and_then(|bytes| bytes.checked_add(BODY_FIXED_BYTES))
    .ok_or_else(|| host_failure("native_parser_workspace_accounting", "corrected JSON workspace overflowed"))?;
  Ok(body_workspace.max(archive_workspace).max(json_workspace))
}

struct ParserBodyV1 {
  bytes: Vec<u8>,
  _body_reservation: MemoryReservation,
  _workspace_reservation: MemoryReservation,
}

impl std::ops::Deref for ParserBodyV1 {
  type Target = [u8];

  fn deref(&self) -> &Self::Target {
    &self.bytes
  }
}

enum RawJsonAttemptV1 {
  Parsed(CanonicalConfigValueV1),
  NotClaimed,
  Deterministic(IndexParserOutcomeV1),
}

fn parse_raw_json(
  body: &[u8],
  candidate: &ParserCandidateV1<'_>,
  corrected: bool,
  json_media_type: bool,
) -> Result<RawJsonAttemptV1, IndexParserExecutionErrorV1> {
  if corrected {
    match parse_corrected_json(body, &candidate.policy) {
      Ok(value) => match enforce_policy(&value, &candidate.policy) {
        Ok(()) => Ok(RawJsonAttemptV1::Parsed(value)),
        Err(PolicyCheckErrorV1::Limit(observed)) => {
          Ok(RawJsonAttemptV1::Deterministic(deterministic_policy("raw_json_policy_limit", observed)?))
        }
        Err(PolicyCheckErrorV1::Host(context)) => Err(host_failure("raw_json_policy_host", context)),
      },
      Err(CorrectedJsonParseErrorV1::PolicyLimit(observed)) => {
        Ok(RawJsonAttemptV1::Deterministic(deterministic_policy("raw_json_policy_limit", observed)?))
      }
      Err(CorrectedJsonParseErrorV1::Host(context)) => Err(host_failure("raw_json_parse_host", context)),
      Err(CorrectedJsonParseErrorV1::Malformed(error)) => {
        if json_media_type || (json_root_recognized(body) && matches!(error.classify(), serde_json::error::Category::Data)) {
          Ok(RawJsonAttemptV1::Deterministic(deterministic_malformed_outcome("raw_json_malformed", body.len() as u64)?))
        } else {
          drop(error);
          Ok(RawJsonAttemptV1::NotClaimed)
        }
      }
    }
  } else {
    let value = match serde_json::from_slice::<serde_json::Value>(body) {
      Ok(value) => value,
      Err(error) => {
        // Migration v0 intentionally preserves serde's old syntax fallthrough.
        drop(error);
        return Ok(RawJsonAttemptV1::NotClaimed);
      }
    };
    let value = canonical_from_serde(value)?;
    match enforce_policy(&value, &candidate.policy) {
      Ok(()) => Ok(RawJsonAttemptV1::Parsed(value)),
      Err(PolicyCheckErrorV1::Limit(observed)) => {
        Ok(RawJsonAttemptV1::Deterministic(deterministic_policy("raw_json_policy_limit", observed)?))
      }
      Err(PolicyCheckErrorV1::Host(context)) => Err(host_failure("raw_json_policy_host", context)),
    }
  }
}

enum CorrectedJsonParseErrorV1 {
  PolicyLimit(u64),
  Host(String),
  Malformed(serde_json::Error),
}

struct CorrectedJsonStateV1<'policy> {
  policy: &'policy InvocationPolicyV1,
  nodes: u64,
  policy_limit: Option<u64>,
  host_failure: Option<String>,
}

impl CorrectedJsonStateV1<'_> {
  fn maximum_depth(&self) -> u64 {
    u64::from(self.policy.max_structure_depth.min(self.policy.max_value_stack_height).min(self.policy.max_recursion_depth))
  }

  fn enter<E: de::Error>(&mut self, depth: u64) -> Result<(), E> {
    self.nodes = match self.nodes.checked_add(1) {
      Some(nodes) => nodes,
      None => return self.reject_policy(u64::MAX, "corrected JSON node count overflowed"),
    };
    if self.nodes > self.policy.max_structure_nodes || depth > self.maximum_depth() {
      return self.reject_policy(self.nodes.max(depth), "corrected JSON exceeds the structural policy");
    }
    Ok(())
  }

  fn check_scalar<E: de::Error>(&mut self, length: usize) -> Result<(), E> {
    if length as u64 > self.policy.max_scalar_bytes {
      return self.reject_policy(length as u64, "corrected JSON scalar exceeds the policy");
    }
    Ok(())
  }

  fn reject_policy<T, E: de::Error>(&mut self, observed: u64, message: &'static str) -> Result<T, E> {
    self.policy_limit = Some(match self.policy_limit {
      Some(previous) => previous.max(observed),
      None => observed,
    });
    Err(E::custom(message))
  }

  fn reject_host<T, E: de::Error>(&mut self, context: String) -> Result<T, E> {
    self.host_failure = Some(context);
    Err(E::custom("corrected JSON allocation failed"))
  }
}

fn parse_corrected_json(body: &[u8], policy: &InvocationPolicyV1) -> Result<CanonicalConfigValueV1, CorrectedJsonParseErrorV1> {
  let mut state = CorrectedJsonStateV1 { policy, nodes: 0, policy_limit: None, host_failure: None };
  let mut deserializer = serde_json::Deserializer::from_slice(body);
  let result = CorrectedJsonSeedV1 { state: &mut state, depth: 1 }.deserialize(&mut deserializer).and_then(|value| {
    deserializer.end()?;
    Ok(value)
  });
  match result {
    Ok(value) => Ok(value),
    Err(error) => match (state.policy_limit, state.host_failure) {
      (Some(observed), _) => Err(CorrectedJsonParseErrorV1::PolicyLimit(observed)),
      (None, Some(context)) => Err(CorrectedJsonParseErrorV1::Host(context)),
      (None, None) => Err(CorrectedJsonParseErrorV1::Malformed(error)),
    },
  }
}

struct CorrectedJsonSeedV1<'state, 'policy> {
  state: &'state mut CorrectedJsonStateV1<'policy>,
  depth: u64,
}

impl<'de> DeserializeSeed<'de> for CorrectedJsonSeedV1<'_, '_> {
  type Value = CanonicalConfigValueV1;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: de::Deserializer<'de>,
  {
    self.state.enter(self.depth)?;
    deserializer.deserialize_any(CorrectedJsonVisitorV1 { state: self.state, depth: self.depth })
  }
}

struct CorrectedJsonVisitorV1<'state, 'policy> {
  state: &'state mut CorrectedJsonStateV1<'policy>,
  depth: u64,
}

impl<'de> Visitor<'de> for CorrectedJsonVisitorV1<'_, '_> {
  type Value = CanonicalConfigValueV1;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a corrected canonical JSON value")
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Null)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Null)
  }

  fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Boolean(value))
  }

  fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Signed(value))
  }

  fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
    if value <= i64::MAX as u64 {
      Ok(CanonicalConfigValueV1::Signed(value as i64))
    } else {
      Ok(CanonicalConfigValueV1::Unsigned(value))
    }
  }

  fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    if !value.is_finite() || value.to_bits() == (-0.0f64).to_bits() {
      return Err(E::custom("canonical JSON number must be finite and encode zero positively"));
    }
    Ok(CanonicalConfigValueV1::FloatBits(value.to_bits()))
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    self.state.check_scalar(value.len())?;
    let mut owned = String::new();
    if let Err(error) = owned.try_reserve_exact(value.len()) {
      return self.state.reject_host(format!("cannot reserve corrected JSON scalar: {error}"));
    }
    owned.push_str(value);
    Ok(CanonicalConfigValueV1::String(owned))
  }

  fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    self.state.check_scalar(value.len())?;
    Ok(CanonicalConfigValueV1::String(value))
  }

  fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    let mut values = Vec::new();
    while let Some(value) =
      sequence.next_element_seed(CorrectedJsonSeedV1 { state: &mut *self.state, depth: self.depth.saturating_add(1) })?
    {
      if values.len() == self.state.policy.max_container_members as usize {
        return self.state.reject_policy(values.len() as u64 + 1, "corrected JSON array exceeds the member policy");
      }
      if let Err(error) = values.try_reserve(1) {
        return self.state.reject_host(format!("cannot grow corrected JSON array: {error}"));
      }
      values.push(value);
    }
    Ok(CanonicalConfigValueV1::Array(values))
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut values = BTreeMap::new();
    while let Some(key) = map.next_key_seed(CorrectedJsonStringSeedV1 { state: &mut *self.state })? {
      if values.len() == self.state.policy.max_container_members as usize {
        return self.state.reject_policy(values.len() as u64 + 1, "corrected JSON map exceeds the member policy");
      }
      if values.contains_key(&key) {
        return Err(de::Error::custom(format!("duplicate canonical JSON key {key}")));
      }
      let value = map.next_value_seed(CorrectedJsonSeedV1 { state: &mut *self.state, depth: self.depth.saturating_add(1) })?;
      values.insert(key, value);
    }
    Ok(CanonicalConfigValueV1::Map(values))
  }
}

struct CorrectedJsonStringSeedV1<'state, 'policy> {
  state: &'state mut CorrectedJsonStateV1<'policy>,
}

impl<'de> DeserializeSeed<'de> for CorrectedJsonStringSeedV1<'_, '_> {
  type Value = String;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: de::Deserializer<'de>,
  {
    deserializer.deserialize_string(CorrectedJsonStringVisitorV1 { state: self.state })
  }
}

struct CorrectedJsonStringVisitorV1<'state, 'policy> {
  state: &'state mut CorrectedJsonStateV1<'policy>,
}

impl<'de> Visitor<'de> for CorrectedJsonStringVisitorV1<'_, '_> {
  type Value = String;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a corrected JSON object key")
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    self.state.check_scalar(value.len())?;
    let mut owned = String::new();
    if let Err(error) = owned.try_reserve_exact(value.len()) {
      return self.state.reject_host(format!("cannot reserve corrected JSON key: {error}"));
    }
    owned.push_str(value);
    Ok(owned)
  }

  fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    self.state.check_scalar(value.len())?;
    Ok(value)
  }
}

fn finish_native_value(
  value: serde_json::Value,
  policy: &InvocationPolicyV1,
  corrected: bool,
) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
  let value = if corrected {
    match canonical_from_serde_corrected(value, policy) {
      Ok(value) => value,
      Err(PolicyCheckErrorV1::Limit(observed)) => return deterministic_policy("native_parser_policy_limit", observed),
      Err(PolicyCheckErrorV1::Host(context)) => return Err(host_failure("native_parser_output_convert", context)),
    }
  } else {
    canonical_from_serde(value)?
  };
  match enforce_policy(&value, policy) {
    Ok(()) => Ok(IndexParserOutcomeV1::Parsed(value)),
    Err(PolicyCheckErrorV1::Limit(observed)) => deterministic_policy("native_parser_policy_limit", observed),
    Err(PolicyCheckErrorV1::Host(context)) => Err(host_failure("native_parser_policy_host", context)),
  }
}

fn canonical_from_serde(value: serde_json::Value) -> Result<CanonicalConfigValueV1, IndexParserExecutionErrorV1> {
  let bytes = serde_json::to_vec(&value)
    .map_err(|error| host_failure("native_parser_output_encode", format!("cannot encode native parser output: {error}")))?;
  serde_json::from_slice::<CanonicalConfigValueV1>(&bytes)
    .map_err(|error| host_failure("native_parser_output_decode", format!("cannot canonicalize native parser output: {error}")))
}

fn canonical_from_serde_corrected(
  value: serde_json::Value,
  policy: &InvocationPolicyV1,
) -> Result<CanonicalConfigValueV1, PolicyCheckErrorV1> {
  let mut nodes = 0u64;
  canonical_from_serde_corrected_at(value, policy, 1, &mut nodes)
}

fn canonical_from_serde_corrected_at(
  value: serde_json::Value,
  policy: &InvocationPolicyV1,
  depth: u64,
  nodes: &mut u64,
) -> Result<CanonicalConfigValueV1, PolicyCheckErrorV1> {
  *nodes = nodes.checked_add(1).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))?;
  let maximum_depth = u64::from(policy.max_structure_depth.min(policy.max_value_stack_height).min(policy.max_recursion_depth));
  if *nodes > policy.max_structure_nodes || depth > maximum_depth {
    return Err(PolicyCheckErrorV1::Limit((*nodes).max(depth)));
  }
  match value {
    serde_json::Value::Null => Ok(CanonicalConfigValueV1::Null),
    serde_json::Value::Bool(value) => Ok(CanonicalConfigValueV1::Boolean(value)),
    serde_json::Value::Number(value) => {
      if let Some(value) = value.as_i64() {
        return Ok(CanonicalConfigValueV1::Signed(value));
      }
      if let Some(value) = value.as_u64() {
        return Ok(CanonicalConfigValueV1::Unsigned(value));
      }
      let value =
        value.as_f64().ok_or_else(|| PolicyCheckErrorV1::Host("native parser produced an unrepresentable JSON number".to_string()))?;
      if !value.is_finite() || value.to_bits() == (-0.0f64).to_bits() {
        return Err(PolicyCheckErrorV1::Host("native parser produced a noncanonical JSON number".to_string()));
      }
      Ok(CanonicalConfigValueV1::FloatBits(value.to_bits()))
    }
    serde_json::Value::String(value) => {
      if value.len() as u64 > policy.max_scalar_bytes {
        return Err(PolicyCheckErrorV1::Limit(value.len() as u64));
      }
      Ok(CanonicalConfigValueV1::String(value))
    }
    serde_json::Value::Array(values) => {
      if values.len() > policy.max_container_members as usize {
        return Err(PolicyCheckErrorV1::Limit(values.len() as u64));
      }
      let mut canonical = Vec::new();
      canonical
        .try_reserve_exact(values.len())
        .map_err(|error| PolicyCheckErrorV1::Host(format!("cannot reserve corrected native array: {error}")))?;
      for value in values {
        canonical.push(canonical_from_serde_corrected_at(value, policy, depth.saturating_add(1), nodes)?);
      }
      Ok(CanonicalConfigValueV1::Array(canonical))
    }
    serde_json::Value::Object(values) => {
      if values.len() > policy.max_container_members as usize {
        return Err(PolicyCheckErrorV1::Limit(values.len() as u64));
      }
      let mut canonical = BTreeMap::new();
      for (key, value) in values {
        if key.len() as u64 > policy.max_scalar_bytes {
          return Err(PolicyCheckErrorV1::Limit(key.len() as u64));
        }
        canonical.insert(key, canonical_from_serde_corrected_at(value, policy, depth.saturating_add(1), nodes)?);
      }
      Ok(CanonicalConfigValueV1::Map(canonical))
    }
  }
}

enum PolicyCheckErrorV1 {
  Limit(u64),
  Host(String),
}

fn enforce_policy(value: &CanonicalConfigValueV1, policy: &InvocationPolicyV1) -> Result<(), PolicyCheckErrorV1> {
  let maximum_depth = u64::from(policy.max_structure_depth.min(policy.max_value_stack_height).min(policy.max_recursion_depth));
  let mut stack = vec![(value, 1u64)];
  let mut nodes = 0u64;
  let mut encoded_bytes = 0u64;
  while let Some((value, depth)) = stack.pop() {
    nodes = nodes.checked_add(1).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))?;
    if nodes > policy.max_structure_nodes || depth > maximum_depth {
      return Err(PolicyCheckErrorV1::Limit(nodes.max(depth)));
    }
    let own_bytes = match value {
      CanonicalConfigValueV1::Null | CanonicalConfigValueV1::Boolean(_) => 5,
      CanonicalConfigValueV1::Signed(_) | CanonicalConfigValueV1::Unsigned(_) | CanonicalConfigValueV1::FloatBits(_) => 13,
      CanonicalConfigValueV1::String(value) => {
        if value.len() as u64 > policy.max_scalar_bytes {
          return Err(PolicyCheckErrorV1::Limit(value.len() as u64));
        }
        5u64.checked_add(value.len() as u64).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))?
      }
      CanonicalConfigValueV1::Bytes(value) => {
        if value.len() as u64 > policy.max_scalar_bytes {
          return Err(PolicyCheckErrorV1::Limit(value.len() as u64));
        }
        5u64.checked_add(value.len() as u64).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))?
      }
      CanonicalConfigValueV1::Array(values) => {
        if values.len() > policy.max_container_members as usize {
          return Err(PolicyCheckErrorV1::Limit(values.len() as u64));
        }
        stack
          .try_reserve(values.len())
          .map_err(|error| PolicyCheckErrorV1::Host(format!("cannot reserve bounded parser value stack: {error}")))?;
        stack.extend(values.iter().rev().map(|value| (value, depth.saturating_add(1))));
        9
      }
      CanonicalConfigValueV1::Map(values) => {
        if values.len() > policy.max_container_members as usize {
          return Err(PolicyCheckErrorV1::Limit(values.len() as u64));
        }
        let key_bytes = values.keys().try_fold(0u64, |total, key| {
          if key.len() as u64 > policy.max_scalar_bytes {
            return Err(PolicyCheckErrorV1::Limit(key.len() as u64));
          }
          total.checked_add(4).and_then(|bytes| bytes.checked_add(key.len() as u64)).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))
        })?;
        stack
          .try_reserve(values.len())
          .map_err(|error| PolicyCheckErrorV1::Host(format!("cannot reserve bounded parser value stack: {error}")))?;
        stack.extend(values.values().rev().map(|value| (value, depth.saturating_add(1))));
        9u64.checked_add(key_bytes).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))?
      }
    };
    encoded_bytes = encoded_bytes.checked_add(own_bytes).ok_or(PolicyCheckErrorV1::Limit(u64::MAX))?;
    if encoded_bytes > policy.max_response_bytes {
      return Err(PolicyCheckErrorV1::Limit(encoded_bytes));
    }
  }
  Ok(())
}

fn deterministic_policy(code: &'static str, observed: u64) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
  let evidence = evidence(code)?;
  Ok(IndexParserOutcomeV1::DeterministicUnindexable(IndexParserDeterministicFailureV1::parser_output_contract(
    evidence, observed, observed,
  )))
}

fn deterministic_malformed(code: &'static str, work: u64) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
  deterministic_malformed_outcome(code, work)
}

fn deterministic_malformed_outcome(code: &'static str, work: u64) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
  let evidence = evidence(code)?;
  Ok(IndexParserOutcomeV1::DeterministicUnindexable(IndexParserDeterministicFailureV1::malformed_document(evidence, work)))
}

fn evidence(code: &'static str) -> Result<Vec<u8>, IndexParserExecutionErrorV1> {
  encode_canonical_value(&CanonicalConfigValueV1::String(code.to_string()), CanonicalValueBounds::CONFIG)
    .map_err(|error| host_failure("native_parser_evidence", error.to_string()))
}

fn validate_request(engine: &StorageEngine, request: &IndexParserExecutionRequestV1<'_>) -> Result<(), IndexParserExecutionErrorV1> {
  let hash_width = engine.hash_algo().hash_length();
  if request.namespace_root().len() != hash_width
    || request.record_revision_hash().len() != hash_width
    || request.namespace_root().iter().all(|byte| *byte == 0)
    || request.record_revision_hash().iter().all(|byte| *byte == 0)
    || request.path() == "/"
    || !request.path().starts_with('/')
    || normalize_path(request.path()) != request.path()
  {
    return Err(host_failure("native_parser_request", "parser request has invalid root, revision, or path identity"));
  }
  Ok(())
}

fn unavailable_wasm_candidate(
  dependencies: &DependencyTableV1<'_>,
  candidate: &ParserCandidateV1<'_>,
) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
  let dependency = dependency_at(dependencies, candidate.dependency_ordinal)?;
  Err(IndexParserExecutionErrorV1::dependency_unavailable(
    "native_parser_wasm_unavailable",
    format!("exact parser dependency {} is not installed in the v4 executor", dependency.dependency_id),
  ))
}

fn require_native_dependency<'a>(
  dependencies: &'a DependencyTableV1<'a>,
  ordinal: u32,
  expected_id: &'static str,
  expected_role: u16,
) -> Result<&'a DependencyRecordV1<'a>, IndexParserExecutionErrorV1> {
  let dependency = dependency_at(dependencies, ordinal)?;
  let expected_fingerprint =
    native_fingerprint(expected_id).ok_or_else(|| host_failure("native_parser_dependency", "unknown native ID"))?;
  if dependency.kind != 2
    || dependency.role != expected_role
    || dependency.abi != 0
    || dependency.executor_profile != 1
    || dependency.fingerprint_semantics != 2
    || dependency.artifact_kind != 0
    || dependency.artifact_length != 0
    || dependency.flags != 0
    || dependency.dependency_id != expected_id
    || dependency.version != NATIVE_VERSION
    || dependency.fingerprint != expected_fingerprint
  {
    return Err(IndexParserExecutionErrorV1::dependency_unavailable(
      "native_parser_dependency_unavailable",
      format!("native semantic dependency {expected_id} does not match this executor"),
    ));
  }
  Ok(dependency)
}

fn dependency_at<'a>(
  dependencies: &'a DependencyTableV1<'a>,
  ordinal: u32,
) -> Result<&'a DependencyRecordV1<'a>, IndexParserExecutionErrorV1> {
  let zero_based = ordinal
    .checked_sub(1)
    .ok_or_else(|| host_failure("native_parser_dependency_ordinal", "dependency ordinal is zero or not representable"))?;
  let index = usize::try_from(zero_based)
    .map_err(|error| host_failure("native_parser_dependency_ordinal", format!("dependency ordinal does not fit this platform: {error}")))?;
  dependencies
    .records
    .get(index)
    .ok_or_else(|| host_failure("native_parser_dependency_ordinal", format!("dependency ordinal {ordinal} is outside the table")))
}

fn native_fingerprint(id: &str) -> Option<[u8; 32]> {
  let bytes: &[u8] = match id {
    MIME_ROUTER_ID => b"/org/aeordev/aeordb/native/mime-router-v1:semantic-conformance-v1",
    RAW_JSON_ID => b"/org/aeordev/aeordb/native/raw-json-v1:semantic-conformance-v1",
    NATIVE_SUITE_ID => b"/org/aeordev/aeordb/native/native-suite-v1:semantic-conformance-v1",
    _ => return None,
  };
  Some(*blake3::hash(bytes).as_bytes())
}

#[allow(clippy::drop_non_drop)]
fn corrected_mime_essence(content_type: Option<&str>) -> Option<String> {
  let value = content_type?.trim_matches(|character| matches!(character, ' ' | '\t'));
  if value.is_empty() {
    return None;
  }
  let parsed = match value.parse::<mime::Mime>() {
    Ok(parsed) => parsed,
    Err(error) => {
      drop(error);
      return None;
    }
  };
  let essence = parsed.essence_str().to_ascii_lowercase();
  let (type_name, subtype_name) = essence.split_once('/')?;
  if type_name == "*"
    || subtype_name == "*"
    || type_name.is_empty()
    || subtype_name.is_empty()
    || type_name.len() > 127
    || subtype_name.len() > 127
    || !type_name.bytes().all(restricted_name_byte)
    || !subtype_name.bytes().all(restricted_name_byte)
  {
    return None;
  }
  Some(essence)
}

fn required_filename<'a>(request: &'a IndexParserExecutionRequestV1<'_>) -> Result<&'a str, IndexParserExecutionErrorV1> {
  file_name(request.path()).ok_or_else(|| host_failure("native_parser_request", "parser request path has no final filename segment"))
}

fn restricted_name_byte(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-')
}

fn corrected_extension(filename: &str) -> Option<String> {
  let (_, extension) = filename.rsplit_once('.')?;
  (!extension.is_empty()).then(|| extension.to_ascii_lowercase())
}

#[allow(clippy::manual_unwrap_or_default)]
fn content_type_or_empty(content_type: Option<&str>) -> &str {
  // Absence is the frozen legacy parser input, not a swallowed failure.
  match content_type {
    Some(content_type) => content_type,
    None => "",
  }
}

fn is_json_media_type(corrected_essence: Option<&str>, stored: Option<&str>, corrected: bool) -> bool {
  if !corrected {
    return stored == Some("application/json");
  }
  corrected_essence.is_some_and(|essence| {
    essence == "application/json"
      || essence.strip_prefix("application/").is_some_and(|subtype| subtype.len() > 5 && subtype.ends_with("+json"))
  })
}

fn json_root_recognized(body: &[u8]) -> bool {
  body
    .iter()
    .copied()
    .find(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    .is_some_and(|byte| matches!(byte, b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'))
}

fn check_cancelled(request: &IndexParserExecutionRequestV1<'_>, code: &'static str) -> Result<(), IndexParserExecutionErrorV1> {
  if (request.is_cancelled())() {
    return Err(IndexParserExecutionErrorV1::cancelled(code, "native parser execution was cancelled"));
  }
  Ok(())
}

fn map_body_memory_error(error: MemoryCoordinatorError) -> IndexParserExecutionErrorV1 {
  host_failure("native_parser_body_memory_pressure", error.to_string())
}

fn map_parser_memory_error(error: MemoryCoordinatorError) -> IndexParserExecutionErrorV1 {
  host_failure("native_parser_workspace_memory_pressure", error.to_string())
}

fn map_engine_error(error: EngineError) -> IndexParserExecutionErrorV1 {
  match error {
    EngineError::Cancelled(context) => IndexParserExecutionErrorV1::cancelled("native_parser_storage_cancelled", context),
    error => host_failure("native_parser_storage", error.to_string()),
  }
}

fn host_failure(code: &'static str, context: impl Into<String>) -> IndexParserExecutionErrorV1 {
  IndexParserExecutionErrorV1::host_failure(code, context)
}

const _: () = assert!(size_of::<NativeIndexParserExecutorV1<'static>>() <= size_of::<usize>());
