use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::engine::batch_commit::{BufferedFile, commit_buffered_files_with_kind};
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::namespace_mutation::NamespaceMutationKind;
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::GenericDataPathSelection;
use crate::engine::SystemFamilyPolicyResolver;

/// The 8 crudlify flag positions and their canonical letters.
const CRUDLIFY_LETTERS: [char; 8] = ['c', 'r', 'u', 'd', 'l', 'i', 'f', 'y'];

const PERMISSION_DOCUMENT_MAX_BYTES: u64 = 1024 * 1024;
const PERMISSION_DOCUMENT_WORKSPACE_BYTES: u64 = 2 * PERMISSION_DOCUMENT_MAX_BYTES;
const PERMISSION_BATCH_MAX_BYTES: u64 = 64 * 1024 * 1024;
const PERMISSION_REQUEST_MAX_BYTES: u64 = 16 * 1024 * 1024;
const SHARE_LINK_UPDATE_MAX: usize = 65_536;

/// Permission link connecting a group to a path with crudlify flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionLink {
  pub group: String,
  /// 8 chars crudlify: "crudlify", "cr......" etc.
  pub allow: String,
  /// 8 chars deny flags.
  pub deny: String,
  /// Optional allow flags for non-members of this group.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub others_allow: Option<String>,
  /// Optional deny flags for non-members of this group.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub others_deny: Option<String>,
  /// When set, this link only applies to entries whose filename matches
  /// this exact pattern within the directory. When absent, applies to
  /// everything in the directory (current behavior).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub path_pattern: Option<String>,
}

/// Permissions for a directory path, stored as an `.aeordb-permissions` JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPermissions {
  pub links: Vec<PermissionLink>,
}

impl PathPermissions {
  /// Serialize to JSON bytes for storage. Delegates to the
  /// `JsonVersioned` impl so the `"$v"` field is injected.
  pub fn serialize(&self) -> Vec<u8> {
    <Self as crate::engine::schema_version::JsonVersioned>::serialize_versioned(self)
  }

  /// Deserialize from JSON bytes. Reads `"$v"` first and dispatches.
  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    <Self as crate::engine::schema_version::JsonVersioned>::deserialize_versioned(data)
  }

  /// Decode permission bytes that already came from database authority.
  /// Parse/version failures here describe corrupt stored state, not bad HTTP
  /// input, and must therefore never be reported as a client-side `400`.
  pub fn deserialize_stored(data: &[u8], path: &str) -> EngineResult<Self> {
    Self::deserialize(data)
      .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("malformed stored permission authority at {path}: {error}") })
  }
}

crate::impl_json_versioned_v0!(PathPermissions);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionGrantResult {
  pub paths: Vec<String>,
  pub changed_paths: Vec<String>,
  pub changed_permission_files: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionRevokeResult {
  Revoked,
  PermissionFileNotFound,
  LinkNotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionTargetKind {
  File,
  Directory,
}

/// Typed read-modify-write authority for `.aeordb-permissions` documents.
///
/// Grant and revoke operations retain one namespace guard from selected-root
/// path classification through the final hard-acknowledged batch. Immutable
/// chunks may be staged by the shared buffered writer, but no permission
/// FileRecord, locator, root, cache invalidation, or event is visible before
/// the complete batch acknowledges.
pub struct PermissionStore<'a> {
  engine: &'a StorageEngine,
}

impl<'a> PermissionStore<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    Self { engine }
  }

  pub fn grant_paths(
    &self,
    ctx: &RequestContext,
    paths: Vec<String>,
    groups: Vec<String>,
    allow: String,
  ) -> EngineResult<PermissionGrantResult> {
    validate_permission_flags(&allow)?;
    let request_bytes = permission_request_bytes(&paths, &groups, &allow)?;
    let request_workspace_bytes = request_bytes
      .checked_mul(2)
      .ok_or_else(|| EngineError::InvalidInput("Permission request workspace byte count overflow".to_string()))?;
    let mut memory = OperationMemoryBudget::new(
      self.engine,
      "permission batch",
      MemoryOwner::KvWriteBuffers,
      AdmissionClass::Workload,
      request_workspace_bytes,
      None,
    )?;
    let paths = unique_normalized_paths(paths)?;
    let groups = unique_groups(groups)?;
    validate_link_update_count(paths.len(), groups.len())?;

    let _authority = self.engine.namespace_write_guard()?;
    let ops = DirectoryOps::new(self.engine);
    let policy = SystemFamilyPolicyResolver::new(self.engine.hash_algo())?;
    let mut targets = BTreeMap::<String, Vec<(String, Option<String>)>>::new();

    for path in &paths {
      require_shareable_path(&policy, path)?;
      let kind = permission_target_kind(&ops, path)?.ok_or_else(|| EngineError::NotFound(format!("Path not found: {path}")))?;
      let (permission_directory, pattern) = match kind {
        PermissionTargetKind::File => {
          let parent = parent_path(path).unwrap_or_else(|| "/".to_string());
          let name =
            file_name(path).ok_or_else(|| EngineError::InvalidInput(format!("Shared file path has no filename: {path}")))?.to_string();
          (parent, Some(name))
        }
        PermissionTargetKind::Directory => (path.clone(), None),
      };
      targets.entry(permission_file_path(&permission_directory)).or_default().push((path.clone(), pattern));
    }

    let mut changed_files = Vec::new();
    let mut changed_path_set = HashSet::new();
    let mut changed_file_bytes = 0u64;
    for (permission_path, patterns) in targets {
      let memory_checkpoint = memory.checkpoint();
      memory.reserve(PERMISSION_DOCUMENT_WORKSPACE_BYTES, "permission document workspace admission failed")?;
      let mut permissions = load_permission_document(&ops, &permission_path)?.unwrap_or(PathPermissions { links: Vec::new() });
      let mut changed = false;

      for (requested_path, pattern) in patterns {
        let mut path_changed = false;
        for group in &groups {
          match permissions.links.iter_mut().find(|link| link.group == *group && link.path_pattern == pattern) {
            Some(link) if link.allow == allow => {}
            Some(link) => {
              link.allow.clone_from(&allow);
              changed = true;
              path_changed = true;
            }
            None => {
              permissions.links.push(PermissionLink {
                group: group.clone(),
                allow: allow.clone(),
                deny: "........".to_string(),
                others_allow: None,
                others_deny: None,
                path_pattern: pattern.clone(),
              });
              changed = true;
              path_changed = true;
            }
          }
        }
        if path_changed {
          changed_path_set.insert(requested_path);
        }
      }

      if !changed {
        memory.release_to(memory_checkpoint, "permission document workspace release failed")?;
        continue;
      }
      let data = permissions.serialize();
      validate_permission_document_size(&permission_path, data.len())?;
      let data_bytes = u64::try_from(data.len())
        .map_err(|_| EngineError::InvalidInput("Permission batch aggregate byte count exceeds this platform".to_string()))?;
      changed_file_bytes = validate_permission_batch_size(changed_file_bytes, data_bytes)?;
      memory.release(PERMISSION_DOCUMENT_WORKSPACE_BYTES - data_bytes, "permission document workspace retention adjustment failed")?;
      changed_files.push(BufferedFile { path: permission_path, data, content_type: Some("application/json".to_string()) });
    }

    let changed_permission_files = changed_files.len();
    if !changed_files.is_empty() {
      commit_buffered_files_with_kind(self.engine, ctx, changed_files, NamespaceMutationKind::SystemWrite)?;
    }
    let changed_paths = paths.iter().filter(|path| changed_path_set.contains(*path)).cloned().collect();
    Ok(PermissionGrantResult { paths, changed_paths, changed_permission_files })
  }

  pub fn revoke_path(
    &self,
    ctx: &RequestContext,
    path: &str,
    group: &str,
    path_pattern: Option<&str>,
  ) -> EngineResult<PermissionRevokeResult> {
    if group.is_empty() {
      return Err(EngineError::InvalidInput("Permission group cannot be empty".to_string()));
    }
    let normalized = normalize_path(path);
    let mut memory =
      OperationMemoryBudget::new(self.engine, "permission revoke", MemoryOwner::KvWriteBuffers, AdmissionClass::Workload, 0, None)?;
    memory.reserve(PERMISSION_DOCUMENT_WORKSPACE_BYTES, "permission document workspace admission failed")?;
    let _authority = self.engine.namespace_write_guard()?;
    let ops = DirectoryOps::new(self.engine);
    let policy = SystemFamilyPolicyResolver::new(self.engine.hash_algo())?;
    require_shareable_path(&policy, &normalized)?;

    let permission_directory = match path_pattern {
      Some(pattern) => {
        let name = file_name(&normalized)
          .ok_or_else(|| EngineError::InvalidInput(format!("File-specific shared path has no filename: {normalized}")))?;
        if name != pattern {
          return Err(EngineError::InvalidInput(format!(
            "File-specific path pattern '{pattern}' does not match shared path filename '{name}'"
          )));
        }
        parent_path(&normalized).unwrap_or_else(|| "/".to_string())
      }
      None => match permission_target_kind(&ops, &normalized)? {
        Some(PermissionTargetKind::File) => parent_path(&normalized).unwrap_or_else(|| "/".to_string()),
        Some(PermissionTargetKind::Directory) | None => normalized,
      },
    };
    let permission_path = permission_file_path(&permission_directory);
    let Some(mut permissions) = load_permission_document(&ops, &permission_path)? else {
      return Ok(PermissionRevokeResult::PermissionFileNotFound);
    };
    let original_len = permissions.links.len();
    permissions.links.retain(|link| !(link.group == group && link.path_pattern.as_deref() == path_pattern));
    if permissions.links.len() == original_len {
      return Ok(PermissionRevokeResult::LinkNotFound);
    }

    let data = permissions.serialize();
    validate_permission_document_size(&permission_path, data.len())?;
    commit_buffered_files_with_kind(
      self.engine,
      ctx,
      vec![BufferedFile { path: permission_path, data, content_type: Some("application/json".to_string()) }],
      NamespaceMutationKind::SystemWrite,
    )?;
    Ok(PermissionRevokeResult::Revoked)
  }
}

pub fn validate_permission_flags(flags: &str) -> EngineResult<()> {
  if flags.len() != CRUDLIFY_LETTERS.len() {
    return Err(EngineError::InvalidInput(format!("permissions must be exactly {} characters (crudlify pattern)", CRUDLIFY_LETTERS.len())));
  }
  for (index, character) in flags.chars().enumerate() {
    let expected = CRUDLIFY_LETTERS[index];
    if character != expected && character != '.' {
      return Err(EngineError::InvalidInput(format!(
        "invalid permission character '{character}' at position {index}: expected '{expected}' or '.'"
      )));
    }
  }
  Ok(())
}

fn unique_normalized_paths(paths: Vec<String>) -> EngineResult<Vec<String>> {
  if paths.is_empty() {
    return Err(EngineError::InvalidInput("At least one path is required".to_string()));
  }
  let mut seen = HashSet::with_capacity(paths.len());
  let mut unique = Vec::with_capacity(paths.len());
  for path in paths {
    let normalized = normalize_path(&path);
    if seen.insert(normalized.clone()) {
      unique.push(normalized);
    }
  }
  Ok(unique)
}

fn unique_groups(groups: Vec<String>) -> EngineResult<Vec<String>> {
  if groups.is_empty() {
    return Err(EngineError::InvalidInput("At least one user or group is required".to_string()));
  }
  let mut seen = HashSet::with_capacity(groups.len());
  let mut unique = Vec::with_capacity(groups.len());
  for group in groups {
    if group.is_empty() {
      return Err(EngineError::InvalidInput("Permission group cannot be empty".to_string()));
    }
    if seen.insert(group.clone()) {
      unique.push(group);
    }
  }
  Ok(unique)
}

fn validate_link_update_count(paths: usize, groups: usize) -> EngineResult<()> {
  let updates = paths.checked_mul(groups).ok_or_else(|| EngineError::InvalidInput("Share request link count overflow".to_string()))?;
  if updates > SHARE_LINK_UPDATE_MAX {
    return Err(EngineError::InvalidInput(format!(
      "Share request expands to {updates} permission links, exceeding the {SHARE_LINK_UPDATE_MAX}-link limit"
    )));
  }
  Ok(())
}

fn permission_target_kind(ops: &DirectoryOps<'_>, path: &str) -> EngineResult<Option<PermissionTargetKind>> {
  if ops.get_metadata(path)?.is_some() {
    return Ok(Some(PermissionTargetKind::File));
  }
  match ops.list_directory_strict(path) {
    Ok(_) => Ok(Some(PermissionTargetKind::Directory)),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(error) => Err(error),
  }
}

fn permission_file_path(directory: &str) -> String {
  if directory == "/" || directory.ends_with('/') {
    format!("{directory}.aeordb-permissions")
  } else {
    format!("{directory}/.aeordb-permissions")
  }
}

fn load_permission_document(ops: &DirectoryOps<'_>, path: &str) -> EngineResult<Option<PathPermissions>> {
  match ops.read_file_buffered_bounded(path, PERMISSION_DOCUMENT_MAX_BYTES) {
    Ok(data) => PathPermissions::deserialize_stored(&data, path).map(Some),
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(EngineError::ResourceExhausted(message)) => Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("stored permission authority at {path} exceeds the {PERMISSION_DOCUMENT_MAX_BYTES}-byte limit: {message}"),
    }),
    Err(error) => Err(error),
  }
}

fn validate_permission_document_size(path: &str, size: usize) -> EngineResult<()> {
  if size as u64 > PERMISSION_DOCUMENT_MAX_BYTES {
    return Err(EngineError::InvalidInput(format!(
      "Permission authority at {path} would be {size} bytes, exceeding the {PERMISSION_DOCUMENT_MAX_BYTES}-byte limit"
    )));
  }
  Ok(())
}

fn validate_permission_batch_size(current: u64, next: u64) -> EngineResult<u64> {
  let total =
    current.checked_add(next).ok_or_else(|| EngineError::InvalidInput("Permission batch aggregate byte count overflow".to_string()))?;
  if total > PERMISSION_BATCH_MAX_BYTES {
    return Err(EngineError::InvalidInput(format!(
      "Permission batch aggregate output would be {total} bytes, exceeding the {PERMISSION_BATCH_MAX_BYTES}-byte limit"
    )));
  }
  Ok(total)
}

fn permission_request_bytes(paths: &[String], groups: &[String], allow: &str) -> EngineResult<u64> {
  let mut total = 0u64;
  for length in paths.iter().map(String::len).chain(groups.iter().map(String::len)).chain(std::iter::once(allow.len())) {
    let length = u64::try_from(length)
      .map_err(|_| EngineError::InvalidInput("Permission request metadata exceeds this platform's address space".to_string()))?;
    total = validate_permission_request_size(total, length)?;
  }
  Ok(total)
}

fn validate_permission_request_size(current: u64, next: u64) -> EngineResult<u64> {
  let total =
    current.checked_add(next).ok_or_else(|| EngineError::InvalidInput("Permission request metadata byte count overflow".to_string()))?;
  if total > PERMISSION_REQUEST_MAX_BYTES {
    return Err(EngineError::InvalidInput(format!(
      "Permission request metadata would be {total} bytes, exceeding the {PERMISSION_REQUEST_MAX_BYTES}-byte limit"
    )));
  }
  Ok(total)
}

fn require_shareable_path(policy: &SystemFamilyPolicyResolver, path: &str) -> EngineResult<()> {
  match policy.generic_data_path_selection(path)? {
    GenericDataPathSelection::Include => Ok(()),
    GenericDataPathSelection::Conceal | GenericDataPathSelection::StructuralContainer => {
      Err(EngineError::InvalidInput(format!("Cannot share owner-specific path: {path}")))
    }
  }
}

/// Parse a crudlify flag string into an array of 8 tri-state flags.
///
/// Each position maps to a crudlify operation:
///   0=create, 1=read, 2=update, 3=delete, 4=list, 5=invoke, 6=configure, 7=deploy
///
/// A letter at the correct position means `Some(true)` (set).
/// A dot `.` means `None` (no opinion).
/// Any other character at a position is treated as `None`.
pub fn parse_crudlify_flags(flags: &str) -> [Option<bool>; 8] {
  let mut result = [None; 8];
  let chars: Vec<char> = flags.chars().collect();

  for (index, expected_letter) in CRUDLIFY_LETTERS.iter().enumerate() {
    if index < chars.len() && chars[index] == *expected_letter {
      result[index] = Some(true);
    }
  }

  result
}

/// Merge source flags into target using union semantics.
/// Any `Some(true)` in source wins over `None` in target.
pub fn merge_flags(target: &mut [Option<bool>; 8], source: &[Option<bool>; 8]) {
  for index in 0..8 {
    if source[index] == Some(true) {
      target[index] = Some(true);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_all_set() {
    let flags = parse_crudlify_flags("crudlify");
    for flag in &flags {
      assert_eq!(*flag, Some(true));
    }
  }

  #[test]
  fn test_parse_all_dots() {
    let flags = parse_crudlify_flags("........");
    for flag in &flags {
      assert_eq!(*flag, None);
    }
  }

  #[test]
  fn test_parse_mixed() {
    let flags = parse_crudlify_flags("cr..l..y");
    assert_eq!(flags[0], Some(true)); // c
    assert_eq!(flags[1], Some(true)); // r
    assert_eq!(flags[2], None); // u
    assert_eq!(flags[3], None); // d
    assert_eq!(flags[4], Some(true)); // l
    assert_eq!(flags[5], None); // i
    assert_eq!(flags[6], None); // f
    assert_eq!(flags[7], Some(true)); // y
  }

  #[test]
  fn test_parse_empty_string() {
    let flags = parse_crudlify_flags("");
    for flag in &flags {
      assert_eq!(*flag, None);
    }
  }

  #[test]
  fn test_merge_flags_union() {
    let mut target = [None, Some(true), None, None, None, None, None, None];
    let source = [Some(true), None, Some(true), None, None, None, None, None];
    merge_flags(&mut target, &source);
    assert_eq!(target[0], Some(true));
    assert_eq!(target[1], Some(true));
    assert_eq!(target[2], Some(true));
    assert_eq!(target[3], None);
  }

  #[test]
  fn test_serialize_deserialize_roundtrip() {
    let permissions = PathPermissions {
      links: vec![
        PermissionLink {
          group: "engineers".to_string(),
          allow: "crudli..".to_string(),
          deny: "........".to_string(),
          others_allow: None,
          others_deny: None,
          path_pattern: None,
        },
        PermissionLink {
          group: "security".to_string(),
          allow: "crudlify".to_string(),
          deny: "........".to_string(),
          others_allow: Some("........".to_string()),
          others_deny: Some("crudlify".to_string()),
          path_pattern: None,
        },
      ],
    };

    let bytes = permissions.serialize();
    let deserialized = PathPermissions::deserialize(&bytes).unwrap();
    assert_eq!(deserialized.links.len(), 2);
    assert_eq!(deserialized.links[0].group, "engineers");
    assert_eq!(deserialized.links[1].others_deny.as_deref(), Some("crudlify"));
  }

  #[test]
  fn permission_batch_size_accepts_exact_limit_and_rejects_larger_or_overflowing_totals() {
    assert_eq!(validate_permission_batch_size(PERMISSION_BATCH_MAX_BYTES - 1, 1).unwrap(), PERMISSION_BATCH_MAX_BYTES);

    let over_limit = validate_permission_batch_size(PERMISSION_BATCH_MAX_BYTES, 1).unwrap_err();
    assert!(matches!(over_limit, EngineError::InvalidInput(message) if message.contains("aggregate")));

    let overflow = validate_permission_batch_size(u64::MAX, 1).unwrap_err();
    assert!(matches!(overflow, EngineError::InvalidInput(message) if message.contains("overflow")));
  }

  #[test]
  fn permission_request_size_accepts_exact_limit_and_rejects_larger_or_overflowing_totals() {
    assert_eq!(validate_permission_request_size(PERMISSION_REQUEST_MAX_BYTES - 1, 1).unwrap(), PERMISSION_REQUEST_MAX_BYTES);

    let over_limit = validate_permission_request_size(PERMISSION_REQUEST_MAX_BYTES, 1).unwrap_err();
    assert!(matches!(over_limit, EngineError::InvalidInput(message) if message.contains("request metadata")));

    let overflow = validate_permission_request_size(u64::MAX, 1).unwrap_err();
    assert!(matches!(overflow, EngineError::InvalidInput(message) if message.contains("overflow")));
  }
}
