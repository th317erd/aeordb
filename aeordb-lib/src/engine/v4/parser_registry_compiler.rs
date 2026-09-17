//! Corrected registry source -> exact captured parser pins -> class-2 projection.
//! No v0 fallback, live deployment lookup, executor installation or root activation.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use super::dependency::{DependencyRecordV1, encode_dependency_record};
use super::mime_router::corrected_mime_essence;
use super::namespace::{EncodedSemanticDefinitionObjectV1, encode_semantic_definition_object};

const REGISTRY_PATH: &str = "/.aeordb-config/parsers.json";
const MAX_ENTRIES: usize = 512;
const MAX_ALIAS_BYTES: usize = 4096;
const FIXED_WORKSPACE: usize = 8 * 1024 * 1024;
const ALLOCATION_ERROR_PREFIX: &str = "registry allocation refused: ";

#[derive(Debug, thiserror::Error)]
pub enum SemanticCompilationErrorV1 {
  #[error("{path}: invalid semantic configuration: {message}")]
  InvalidSource { path: &'static str, message: String },
  #[error("{path}: dependency unavailable: {message}")]
  DependencyUnavailable { path: &'static str, message: String },
  #[error("{path}: semantic compilation operational failure: {message}")]
  Operational { path: &'static str, message: String },
  #[error("{path}: semantic compilation resource limit: {message}")]
  Resource { path: &'static str, message: String },
  #[error("semantic compilation cancelled")]
  Cancelled,
}

/// One captured configuration/deployment snapshot, not mutable HEAD lookups.
/// The owner must retain identical alias-to-record resolution for this borrow,
/// and verify exact archived artifacts/executor availability before activation.
/// Errors propagate: an I/O/integrity failure must never be returned as absence.
pub trait ParserAliasSnapshotV1 {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1>;
}

#[derive(Clone, Copy, Debug)]
pub struct ParserRegistryCompilationRequestV1<'a> {
  /// None means a proven missing file. Source read/integrity errors are handled
  /// by the captured source owner, before calling the compiler.
  pub source: Option<&'a [u8]>,
  pub hash_algorithm: HashAlgorithm,
  /// Operational admission bounds, excluded from semantic projection identity.
  pub maximum_source_bytes: usize,
  pub maximum_workspace_bytes: usize,
}

pub struct CompiledParserRegistryEntryV1 {
  essence: String,
  dependency_bytes: Vec<u8>,
}

impl CompiledParserRegistryEntryV1 {
  pub fn essence(&self) -> &str {
    &self.essence
  }

  pub fn dependency_bytes(&self) -> &[u8] {
    &self.dependency_bytes
  }
}

/// Exact pins in raw-UTF8 essence order, with the shared memory charge retained
/// until the caller has published or discarded the result. No detached output.
pub struct CompiledParserRegistryV1 {
  entries: Vec<CompiledParserRegistryEntryV1>,
  projection: EncodedSemanticDefinitionObjectV1,
  _memory: MemoryReservation,
}

impl CompiledParserRegistryV1 {
  pub fn entries(&self) -> &[CompiledParserRegistryEntryV1] {
    &self.entries
  }

  pub fn projection(&self) -> &EncodedSemanticDefinitionObjectV1 {
    &self.projection
  }
}

pub fn compile_parser_registry_v1(
  request: ParserRegistryCompilationRequestV1<'_>,
  snapshot: &dyn ParserAliasSnapshotV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledParserRegistryV1, SemanticCompilationErrorV1> {
  check_cancelled(is_cancelled)?;
  let source_length = request.source.map_or(0, <[u8]>::len);
  if source_length > request.maximum_source_bytes {
    return Err(resource("source bytes exceed the caller's capture limit"));
  }
  // Decoding scratch scales only with the admitted source, never the database.
  // Fixed allowance covers 512 aliases + maximum-size dependency records,
  // bounded maps/metadata, canonical output and its enclosing semantic object.
  let workspace = registry_source_workspace_bytes(source_length)?;
  if workspace > request.maximum_workspace_bytes {
    return Err(resource("registry workspace exceeds the caller's limit"));
  }
  let reservation = memory
    .reserve(MemoryOwner::Task, u64::try_from(workspace).map_err(|error| resource(error.to_string()))?, AdmissionClass::Workload)
    .map_err(|error| resource(error.to_string()))?;
  let source = parse_registry_source(request.source)?;
  check(&reservation, is_cancelled)?;
  let mut entries = Vec::new();
  entries.try_reserve_exact(source.len()).map_err(|error| resource(error.to_string()))?;
  let mut projection_length = 9usize; // canonical map frame and member count
  for (essence, alias) in source {
    check(&reservation, is_cancelled)?;
    let dependency = snapshot
      .resolve_parser_alias(&alias)?
      .ok_or_else(|| unavailable(format!("parser alias {alias:?} is absent from the captured snapshot")))?;
    check(&reservation, is_cancelled)?;
    // Structural retainability of an unknown/legacy executor does not authorize
    // emitting it as a corrected parser. Archive byte verification stays with
    // the deployment owner; no module execution happens in this compiler.
    if dependency.kind != 1 || dependency.role != 1 || dependency.flags != 4 || dependency.abi != 3 || dependency.executor_profile != 2 {
      return Err(unavailable(format!("parser alias {alias:?} does not resolve to the corrected parser ABI/executor")));
    }
    let dependency_bytes = encode_dependency_record(&dependency).map_err(|error| unavailable(error.to_string()))?;
    projection_length = projection_length
      .checked_add(4 + essence.len() + 5 + dependency_bytes.len())
      .ok_or_else(|| resource("registry projection length overflow"))?;
    if projection_length > CanonicalValueBounds::CONFIG.maximum_value_length {
      return Err(resource("compiled registry projection exceeds the frozen 256 KiB bound"));
    }
    entries.push(CompiledParserRegistryEntryV1 { essence, dependency_bytes });
  }
  check(&reservation, is_cancelled)?;
  let values = entries
    .iter()
    .map(|entry| (entry.essence.clone(), CanonicalConfigValueV1::Bytes(entry.dependency_bytes.clone())))
    .collect::<BTreeMap<_, _>>();
  let bytes = encode_canonical_value(&CanonicalConfigValueV1::Map(values), CanonicalValueBounds::CONFIG)
    .map_err(|error| resource(error.to_string()))?;
  let projection = encode_semantic_definition_object(2, &bytes, request.hash_algorithm).map_err(|error| resource(error.to_string()))?;
  check(&reservation, is_cancelled)?;
  Ok(CompiledParserRegistryV1 { entries, projection, _memory: reservation })
}

pub(super) fn registry_source_workspace_bytes(source_length: usize) -> Result<usize, SemanticCompilationErrorV1> {
  source_length.checked_mul(4).and_then(|bytes| bytes.checked_add(FIXED_WORKSPACE)).ok_or_else(|| resource("registry workspace overflow"))
}

// Both consumers admit the source/AST workspace before entering this owner.
pub(super) fn parse_registry_source(source: Option<&[u8]>) -> Result<Vec<(String, String)>, SemanticCompilationErrorV1> {
  let Some(bytes) = source else { return Ok(Vec::new()) };
  let source: RegistrySource = serde_json::from_slice(bytes).map_err(|error| {
    let message = error.to_string();
    // serde's generic visitor error cannot carry our typed operational
    // error. Only this visitor emits this prefix; JSON/schema errors do not.
    if message.starts_with(ALLOCATION_ERROR_PREFIX) {
      resource(message)
    } else {
      invalid(message)
    }
  })?;
  if source.version != 1 {
    return Err(invalid("corrected registry requires integer $v: 1; legacy maps require the migration adapter"));
  }
  Ok(source.parsers.0)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrySource {
  #[serde(rename = "$v")]
  version: u16,
  parsers: RegistryEntries,
}

struct RegistryEntries(Vec<(String, String)>);

impl<'de> Deserialize<'de> for RegistryEntries {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    deserializer.deserialize_map(RegistryVisitor)
  }
}

struct RegistryVisitor;

impl<'de> Visitor<'de> for RegistryVisitor {
  type Value = RegistryEntries;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("an object with at most 512 parameter-free MIME keys and parser aliases")
  }

  fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
    let mut entries = Vec::new();
    while let Some(key) = map.next_key::<String>()? {
      if entries.len() == MAX_ENTRIES {
        return Err(de::Error::custom("registry exceeds 512 entries"));
      }
      if key.contains(';') {
        return Err(de::Error::custom("registry keys cannot contain media-type parameters"));
      }
      let essence =
        corrected_mime_essence(Some(&key)).ok_or_else(|| de::Error::custom("registry key is not a valid media-type essence"))?;
      if essence == "application/json" {
        return Err(de::Error::custom("application/json is reserved; use an explicit per-scope parser"));
      }
      let alias = map.next_value::<String>()?;
      if alias.is_empty() || alias.len() > MAX_ALIAS_BYTES || alias.chars().any(char::is_control) {
        return Err(de::Error::custom("parser alias must contain 1..4096 UTF-8 bytes without NUL or control characters"));
      }
      entries.try_reserve(1).map_err(|error| de::Error::custom(format!("{ALLOCATION_ERROR_PREFIX}{error}")))?;
      entries.push((essence, alias));
    }
    entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
      return Err(de::Error::custom("duplicate source or normalized registry MIME key"));
    }
    Ok(RegistryEntries(entries))
  }
}

fn check(reservation: &MemoryReservation, is_cancelled: &dyn Fn() -> bool) -> Result<(), SemanticCompilationErrorV1> {
  check_cancelled(is_cancelled)?;
  reservation.check_admission().map_err(|error| resource(error.to_string()))
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<(), SemanticCompilationErrorV1> {
  if is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  Ok(())
}

fn invalid(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: REGISTRY_PATH, message: message.into() }
}

fn unavailable(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::DependencyUnavailable { path: REGISTRY_PATH, message: message.into() }
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: REGISTRY_PATH, message: message.into() }
}
