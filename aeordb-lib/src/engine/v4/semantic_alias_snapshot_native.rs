//! Prepared per-source compiler borrows, not complete task or executor authority.
use super::*;
use crate::engine::v4::index_configuration_compiler::IndexConfigurationAliasSnapshotV1;
use crate::engine::v4::parser_registry_compiler::ParserAliasSnapshotV1;
use crate::engine::v4::semantic_source_capture::{visit_semantic_source_aliases_v1, SemanticSourceAliasRequestV1};

const SNAPSHOT_PATH: &str = "<native-semantic-alias-snapshot>";
const SNAPSHOT_FIXED_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticAliasSnapshotRequestV1<'a> {
  /// Immutable caller-owned configuration, not namespace capture authority.
  pub source: SemanticSourceAliasRequestV1<'a>,
  /// The read-byte budget covers all unique alias/module pairs cumulatively.
  pub plugins: NativeSemanticPluginSourceBoundsV1,
  pub maximum_snapshot_bytes: usize,
}

struct PreparedAliasV1 {
  alias: String,
  requested_roles: u8,
  parser: Option<Vec<u8>>,
  mapper: Option<Vec<u8>>,
}

/// Current sources from one capture only. Requested replacements and retained
/// catalog sides require their own adapter; this is neither their substitute nor
/// complete source-union, namespace, durable task or executor availability proof.
pub struct NativeSemanticAliasSnapshotV1<'a> {
  capture: &'a NativeSemanticMutationInventoryV1<'a>,
  aliases: Vec<PreparedAliasV1>,
  _memory: MemoryReservation,
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub fn prepare_current_semantic_alias_snapshot(
    &self,
    request: NativeSemanticAliasSnapshotRequestV1<'_>,
  ) -> Result<NativeSemanticAliasSnapshotV1<'_>, NativeSemanticPluginSourceErrorV1> {
    self.prepare_current_semantic_alias_snapshot_with_observer(request, || {})
  }

  pub(crate) fn prepare_current_semantic_alias_snapshot_with_observer(
    &self,
    request: NativeSemanticAliasSnapshotRequestV1<'_>,
    before_complete: impl FnOnce(),
  ) -> Result<NativeSemanticAliasSnapshotV1<'_>, NativeSemanticPluginSourceErrorV1> {
    check_cancelled(&self.cancellation)?;
    self._memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    validate_plugin_source_bounds(request.plugins)?;
    if request.maximum_snapshot_bytes < SNAPSHOT_FIXED_BYTES {
      return Err(snapshot_resource("snapshot metadata exceeds its retained-byte ceiling").into());
    }
    // Count before allocating the table. The schema owner accounts for its AST;
    // both passes see the same immutable bytes and preserve its exact policies.
    let mut name_bytes = 0usize;
    let occurrences = visit_semantic_source_aliases_v1(
      request.source,
      &mut |_, alias| {
        name_bytes = name_bytes.checked_add(alias.len()).ok_or_else(|| snapshot_resource("alias-name byte count overflow"))?;
        Ok(())
      },
      &self.memory,
      &|| self.cancellation.is_cancelled(),
    )?;
    let count = usize::try_from(occurrences).map_err(|source| snapshot_resource(source.to_string()))?;
    let table_bytes = count
      .checked_mul(std::mem::size_of::<PreparedAliasV1>())
      .and_then(|bytes| bytes.checked_add(name_bytes))
      .and_then(|bytes| bytes.checked_add(SNAPSHOT_FIXED_BYTES))
      .filter(|bytes| *bytes <= request.maximum_snapshot_bytes)
      .ok_or_else(|| snapshot_resource("prepared alias table exceeds its retained-byte ceiling"))?;
    let mut reservation = self
      .memory
      .reserve(MemoryOwner::Task, table_bytes as u64, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let mut aliases = Vec::new();
    aliases.try_reserve_exact(count).map_err(|source| snapshot_resource(source.to_string()))?;
    if aliases.capacity() != count {
      return Err(snapshot_resource("alias table allocation exceeds its admitted capacity").into());
    }
    let repeated_count = visit_semantic_source_aliases_v1(
      request.source,
      &mut |role, alias| {
        if aliases.len() == count {
          return Err(snapshot_operational("alias discovery exceeded its immutable first-pass count"));
        }
        let mut name = String::new();
        name.try_reserve_exact(alias.len()).map_err(|source| snapshot_resource(source.to_string()))?;
        if name.capacity() != alias.len() {
          return Err(snapshot_resource("alias name allocation exceeds its admitted capacity"));
        }
        name.push_str(alias);
        aliases.push(PreparedAliasV1 { alias: name, requested_roles: role_bit(role), parser: None, mapper: None });
        Ok(())
      },
      &self.memory,
      &|| self.cancellation.is_cancelled(),
    )?;
    if repeated_count != occurrences || aliases.len() != count {
      return Err(snapshot_operational("alias discovery differs between immutable source passes").into());
    }
    aliases.sort_unstable_by(|left, right| left.alias.as_bytes().cmp(right.alias.as_bytes()));
    aliases.dedup_by(|removed, retained| {
      if removed.alias != retained.alias {
        return false;
      }
      retained.requested_roles |= removed.requested_roles;
      true
    });
    // Retain the admitted occurrence capacity conservatively after deduplication.
    // No module bodies or captured FileRecords remain in the finished table.
    let lookup = self.source_lookup(plugin_source_read_bounds(request.plugins));
    for alias in &mut aliases {
      check_snapshot(self, &reservation)?;
      let pair = self.read_protected_plugin_sources_from_lookup(&alias.alias, request.plugins, &lookup, || {})?;
      if let Some(pair) = pair {
        for (role, destination) in
          [(SemanticSourceAliasRoleV1::Parser, &mut alias.parser), (SemanticSourceAliasRoleV1::Mapper, &mut alias.mapper)]
        {
          if alias.requested_roles & role_bit(role) != 0 {
            if let Some(bytes) = pair.dependency_bytes(role) {
              *destination = Some(copy_dependency(bytes, &mut reservation, request.maximum_snapshot_bytes)?);
            }
          }
          check_snapshot(self, &reservation)?;
        }
      }
    }
    before_complete();
    check_snapshot(self, &reservation)?;
    Ok(NativeSemanticAliasSnapshotV1 { capture: self, aliases, _memory: reservation })
  }
}

impl NativeSemanticAliasSnapshotV1<'_> {
  fn resolve(&self, alias: &str, role: SemanticSourceAliasRoleV1) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    check_snapshot(self.capture, &self._memory)?;
    let position = self.aliases.binary_search_by(|row| row.alias.as_bytes().cmp(alias.as_bytes())).map_err(|position| {
      snapshot_operational(format!("requested alias was not prepared for this source (insertion position {position})"))
    })?;
    let row = &self.aliases[position];
    if row.requested_roles & role_bit(role) == 0 {
      return Err(snapshot_operational("requested alias role was not prepared for this source"));
    }
    let bytes = match role {
      SemanticSourceAliasRoleV1::Parser => row.parser.as_deref(),
      SemanticSourceAliasRoleV1::Mapper => row.mapper.as_deref(),
    };
    let record = bytes.map(decode_dependency_record_bytes).transpose().map_err(|source| snapshot_operational(source.to_string()))?;
    check_snapshot(self.capture, &self._memory)?;
    Ok(record)
  }
}

impl ParserAliasSnapshotV1 for NativeSemanticAliasSnapshotV1<'_> {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(alias, SemanticSourceAliasRoleV1::Parser)
  }
}

impl IndexConfigurationAliasSnapshotV1 for NativeSemanticAliasSnapshotV1<'_> {
  fn resolve_mapper_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(alias, SemanticSourceAliasRoleV1::Mapper)
  }
}

fn copy_dependency(bytes: &[u8], reservation: &mut MemoryReservation, maximum: usize) -> Result<Vec<u8>, SemanticCompilationErrorV1> {
  reservation
    .bytes()
    .checked_add(bytes.len() as u64)
    .filter(|total| *total <= maximum as u64)
    .ok_or_else(|| snapshot_resource("prepared dependency records exceed the retained-byte ceiling"))?;
  reservation.grow(bytes.len() as u64).map_err(|source| snapshot_resource(source.to_string()))?;
  let mut copy = Vec::new();
  copy.try_reserve_exact(bytes.len()).map_err(|source| snapshot_resource(source.to_string()))?;
  if copy.capacity() != bytes.len() {
    return Err(snapshot_resource("dependency allocation exceeds its admitted capacity"));
  }
  copy.extend_from_slice(bytes);
  Ok(copy)
}

fn role_bit(role: SemanticSourceAliasRoleV1) -> u8 {
  match role {
    SemanticSourceAliasRoleV1::Parser => 1,
    SemanticSourceAliasRoleV1::Mapper => 2,
  }
}

fn check_snapshot(
  capture: &NativeSemanticMutationInventoryV1<'_>,
  reservation: &MemoryReservation,
) -> Result<(), SemanticCompilationErrorV1> {
  if capture.cancellation.is_cancelled() {
    return Err(SemanticCompilationErrorV1::Cancelled);
  }
  reservation.check_admission().map_err(|source| snapshot_resource(source.to_string()))
}

fn snapshot_resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: SNAPSHOT_PATH, message: message.into() }
}

fn snapshot_operational(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Operational { path: SNAPSHOT_PATH, message: message.into() }
}
