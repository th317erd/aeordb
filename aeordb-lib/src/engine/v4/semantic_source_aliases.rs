//! Byte-only reference discovery, not compilation or complete source capture.
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use super::super::index_configuration_compiler::configuration_source_workspace_bytes;
use super::super::index_configuration_source::{self, SelectorSource};
use super::super::parser_registry_compiler::{parse_registry_source, registry_source_workspace_bytes, SemanticCompilationErrorV1};

type Result<T> = std::result::Result<T, SemanticCompilationErrorV1>;
type AliasVisitor<'a> = dyn FnMut(SemanticSourceAliasRoleV1, &str) -> Result<()> + 'a;
const ERROR_PATH: &str = "<semantic-source-aliases>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticSourceAliasKindV1 {
  ParserRegistry,
  IndexConfiguration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticSourceAliasRoleV1 {
  Parser,
  Mapper,
}

#[derive(Clone, Copy, Debug)]
pub struct SemanticSourceAliasRequestV1<'a> {
  pub kind: SemanticSourceAliasKindV1,
  /// None means independently proven absence, never a failed source read.
  pub source: Option<&'a [u8]>,
  pub maximum_source_bytes: usize,
  pub maximum_workspace_bytes: usize,
  pub maximum_alias_occurrences: u64,
}

/// Callback notifications are provisional. This count neither proves a complete
/// base/request union nor validates compilation, artifacts or executability.
pub fn visit_semantic_source_aliases_v1(
  request: SemanticSourceAliasRequestV1<'_>,
  visitor: &mut AliasVisitor<'_>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<u64> {
  cancelled(is_cancelled)?;
  let source_length = request.source.map_or(0, <[u8]>::len);
  if source_length > request.maximum_source_bytes {
    return Err(resource("alias source exceeds its capture limit"));
  }
  let workspace = match request.kind {
    SemanticSourceAliasKindV1::ParserRegistry => registry_source_workspace_bytes(source_length)?,
    // No owner-path normalization or compiler output is allocated here. The
    // existing conservative AST charge still precedes the shared parser.
    SemanticSourceAliasKindV1::IndexConfiguration => configuration_source_workspace_bytes(source_length, 0)?,
  };
  if workspace > request.maximum_workspace_bytes {
    return Err(resource("alias source workspace exceeds the caller's limit"));
  }
  let reservation =
    memory.reserve(MemoryOwner::Task, workspace as u64, AdmissionClass::Workload).map_err(|source| resource(source.to_string()))?;
  check(&reservation, is_cancelled)?;
  let mut count = 0u64;
  let mut emit = |role, alias: &str| -> Result<()> {
    check(&reservation, is_cancelled)?;
    let next = count
      .checked_add(1)
      .filter(|next| *next <= request.maximum_alias_occurrences)
      .ok_or_else(|| resource("alias occurrences exceed the caller's work limit"))?;
    visitor(role, alias)?;
    check(&reservation, is_cancelled)?;
    count = next;
    Ok(())
  };
  match request.kind {
    SemanticSourceAliasKindV1::ParserRegistry => {
      let entries = parse_registry_source(request.source)?;
      check(&reservation, is_cancelled)?;
      for (_, alias) in entries {
        emit(SemanticSourceAliasRoleV1::Parser, &alias)?;
      }
    }
    SemanticSourceAliasKindV1::IndexConfiguration => {
      if let Some(bytes) = request.source {
        let source = index_configuration_source::parse(bytes)?;
        check(&reservation, is_cancelled)?;
        if let Some(alias) = source.used_parser_alias() {
          emit(SemanticSourceAliasRoleV1::Parser, alias)?;
        }
        for row in source.rows {
          check(&reservation, is_cancelled)?;
          if let SelectorSource::Mapper { alias, .. } = row.source {
            emit(SemanticSourceAliasRoleV1::Mapper, &alias)?;
          }
        }
      }
    }
  }
  check(&reservation, is_cancelled)?;
  Ok(count)
}

fn check(reservation: &MemoryReservation, is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  cancelled(is_cancelled)?;
  reservation.check_admission().map_err(|source| resource(source.to_string()))
}

fn cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
  if is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  Ok(())
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: ERROR_PATH, message: message.into() }
}
