//! Bounded canonical authority inventory for an offline v3 migration source.
//!
//! The collector runs under exclusive read-only engine maintenance, validates
//! every retained namespace root, and supplies the same exact rows to
//! preflight, base clone, and final authority reconciliation. It owns no
//! destination, migration state, service activation, or source mutation.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::migration_base_clone_execution::{
  MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedSourceV1, MigrationBaseCloneSeedV1, MigrationBaseCloneStreamClosureV1,
};
use super::migration_final_authority_reconciliation::{
  MigrationFinalAuthorityInventoryClosureV1, MigrationFinalAuthorityInventorySourceV1, MigrationFinalAuthoritySeedCountsV1,
  MigrationFinalAuthoritySeedV1, migration_final_authority_inventory_digest_v1,
};
use super::migration_final_reconciliation::count_strict_migration_tree_symlinks_v1;
use super::migration_preflight::{AuthorityInventoryCountsV1, SourceAuthorityInventoryV1};
use super::system_family::embedded_system_family_registry;
use crate::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use crate::engine::peer_connection::PeerConfig;
use crate::engine::system_store;
use crate::engine::task_queue::TaskQueue;
use crate::engine::version_access::resolve_entry_reference_at_version;
use crate::engine::version_manager::VersionManager;
use crate::engine::{EngineError, EngineResult, EntryType, StorageEngine};
use crate::plugins::{PluginManager, PluginType};

const MAXIMUM_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAXIMUM_COUNT_BOUND: u64 = 1_000_000;
const MAXIMUM_NAMESPACE_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAXIMUM_NAMESPACE_WORK_ITEMS: u64 = 1 << 40;
const MAXIMUM_DIRECTORY_DEPTH: usize = 1_000;
const PLUGIN_PAGE_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V3MigrationAuthorityInventoryLimitsV1 {
  pub maximum_roots: u64,
  pub maximum_peers: u64,
  pub maximum_tasks: u64,
  pub maximum_plugins: u64,
  pub maximum_namespace_memory_bytes: u64,
  pub maximum_namespace_work_items: u64,
  pub maximum_directory_depth: usize,
}

pub struct V3MigrationAuthorityInventoryRequestV1<'a> {
  pub source: &'a Arc<StorageEngine>,
  pub database_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub cancellation: &'a CancellationToken,
  pub acquisition_timeout: Duration,
  pub limits: V3MigrationAuthorityInventoryLimitsV1,
}

#[derive(Debug)]
pub struct V3MigrationAuthorityInventoryV1 {
  database_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  source_physical_identity: PlatformFileIdentityDescriptorV1,
  source_header_sequence: u64,
  source_root: Vec<u8>,
  source_publication_sequence: u64,
  rows: Vec<MigrationFinalAuthoritySeedV1>,
  seed_counts: MigrationFinalAuthoritySeedCountsV1,
  counts: AuthorityInventoryCountsV1,
  authority_digest: [u8; 32],
  system_family_registry_fingerprint: Vec<u8>,
}

impl V3MigrationAuthorityInventoryV1 {
  pub fn preflight_evidence(&self) -> SourceAuthorityInventoryV1 {
    SourceAuthorityInventoryV1 {
      complete: true,
      source_header_sequence: self.source_header_sequence,
      unresolved_family_count: 0,
      counts: self.counts,
      authority_digest: self.authority_digest,
      system_family_registry_fingerprint: self.system_family_registry_fingerprint.clone(),
    }
  }

  pub fn into_base_clone_stream(self) -> V3MigrationBaseCloneSeedStreamV1 {
    V3MigrationBaseCloneSeedStreamV1 {
      rows: self.rows.into_iter(),
      closure: Some(MigrationBaseCloneStreamClosureV1 {
        database_id: self.database_id,
        source_physical_instance_id: self.source_physical_instance_id,
        source_header_sequence: self.source_header_sequence,
        source_capture_head: self.source_root,
        source_authority_digest: self.authority_digest,
        source_authority_counts: self.counts,
      }),
    }
  }

  pub fn into_final_authority_stream(self) -> V3MigrationFinalAuthorityInventoryStreamV1 {
    let seed_count = self.rows.len() as u64;
    V3MigrationFinalAuthorityInventoryStreamV1 {
      rows: self.rows.into_iter(),
      closure: Some(MigrationFinalAuthorityInventoryClosureV1 {
        complete: true,
        database_id: self.database_id,
        source_physical_instance_id: self.source_physical_instance_id,
        source_physical_identity: self.source_physical_identity,
        source_header_sequence: self.source_header_sequence,
        frozen_source_root: self.source_root,
        frozen_source_publication_sequence: self.source_publication_sequence,
        unresolved_family_count: 0,
        source_authority_counts: self.counts,
        seed_counts: self.seed_counts,
        seed_count,
        authority_digest: self.authority_digest,
        system_family_registry_fingerprint: self.system_family_registry_fingerprint,
      }),
    }
  }
}

pub struct V3MigrationBaseCloneSeedStreamV1 {
  rows: std::vec::IntoIter<MigrationFinalAuthoritySeedV1>,
  closure: Option<MigrationBaseCloneStreamClosureV1>,
}

impl MigrationBaseCloneSeedSourceV1 for V3MigrationBaseCloneSeedStreamV1 {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationBaseCloneSeedV1>> {
    ensure_stream_open(self.closure.is_some(), "base clone")?;
    Ok(self.rows.next().map(|row| row.seed))
  }

  fn finish(&mut self) -> EngineResult<MigrationBaseCloneStreamClosureV1> {
    ensure_stream_exhausted(self.rows.len(), "base clone")?;
    self.closure.take().ok_or_else(|| EngineError::InvalidInput("v3 base-clone authority stream was already finished".to_string()))
  }
}

pub struct V3MigrationFinalAuthorityInventoryStreamV1 {
  rows: std::vec::IntoIter<MigrationFinalAuthoritySeedV1>,
  closure: Option<MigrationFinalAuthorityInventoryClosureV1>,
}

impl MigrationFinalAuthorityInventorySourceV1 for V3MigrationFinalAuthorityInventoryStreamV1 {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationFinalAuthoritySeedV1>> {
    ensure_stream_open(self.closure.is_some(), "final authority")?;
    Ok(self.rows.next())
  }

  fn finish(&mut self) -> EngineResult<MigrationFinalAuthorityInventoryClosureV1> {
    ensure_stream_exhausted(self.rows.len(), "final authority")?;
    self.closure.take().ok_or_else(|| EngineError::InvalidInput("v3 final-authority stream was already finished".to_string()))
  }
}

pub fn collect_v3_migration_authority_inventory_v1(
  request: V3MigrationAuthorityInventoryRequestV1<'_>,
) -> EngineResult<V3MigrationAuthorityInventoryV1> {
  validate_request(&request)?;
  let source = request.source.as_ref();
  let _maintenance = source.acquire_exclusive_read_only_engine_maintenance(
    "v3_migration_authority_inventory",
    request.acquisition_timeout,
    Some(request.cancellation),
  )?;
  check_cancelled(request.cancellation)?;
  let before = source.frozen_source_authority_snapshot()?;
  let source_physical_identity = source_identity(source)?;
  validate_root(source, &before.namespace_root, "HEAD")?;
  let registry = embedded_system_family_registry(before.hash_algorithm)
    .map_err(|error| EngineError::InvalidInput(format!("v3 migration SystemFamily registry is invalid: {error}")))?;

  let mut rows = Vec::new();
  rows.try_reserve(1).map_err(|error| allocation_error("HEAD seed", error))?;
  rows.push(MigrationFinalAuthoritySeedV1 {
    authority_identity: Vec::new(),
    source_write_sequence: before.hard_publication_frontier,
    system_family_id: None,
    logical_bytes: 0,
    seed: MigrationBaseCloneSeedV1 {
      kind: MigrationBaseCloneSeedKindV1::CurrentHead,
      path: "/".to_string(),
      entry_type: EntryType::DirectoryIndex,
      hash: before.namespace_root.clone(),
    },
  });

  let stats = source.stats()?;
  admit_count(stats.snapshot_count as u64, request.limits.maximum_roots, "snapshot roots")?;
  admit_count(stats.fork_count as u64, request.limits.maximum_roots, "fork roots")?;
  let versions = VersionManager::new(source);
  let mut snapshots = versions.list_snapshots()?;
  let mut forks = versions.list_forks()?;
  snapshots.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
  forks.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
  reject_duplicate_or_empty_names(snapshots.iter().map(|snapshot| snapshot.name.as_str()), "snapshot")?;
  reject_duplicate_or_empty_names(forks.iter().map(|fork| fork.name.as_str()), "fork")?;
  for snapshot in snapshots {
    check_cancelled(request.cancellation)?;
    validate_root(source, &snapshot.root_hash, "snapshot")?;
    push_root(
      &mut rows,
      request.limits.maximum_roots,
      MigrationBaseCloneSeedKindV1::Snapshot,
      snapshot.name.into_bytes(),
      snapshot.root_hash,
    )?;
  }
  for fork in forks {
    check_cancelled(request.cancellation)?;
    validate_root(source, &fork.root_hash, "fork")?;
    push_root(&mut rows, request.limits.maximum_roots, MigrationBaseCloneSeedKindV1::Fork, fork.name.into_bytes(), fork.root_hash)?;
  }

  let mut peers = system_store::get_peer_configs(source)?;
  admit_count(peers.len() as u64, request.limits.maximum_peers, "peer configurations")?;
  peers.sort_by_key(|peer| peer.node_id);
  reject_duplicate_peers(&peers)?;
  let mut sync_states = 0u64;
  for peer in &peers {
    check_cancelled(request.cancellation)?;
    let Some(state) = system_store::get_peer_sync_state(source, peer.node_id)? else {
      continue;
    };
    sync_states = checked_increment(sync_states, "sync-state count")?;
    let Some(encoded_root) = state.last_local_root_hash else {
      continue;
    };
    let root = decode_root(&encoded_root, before.hash_algorithm.hash_length(), peer.node_id)?;
    validate_root(source, &root, "sync pin")?;
    push_root(&mut rows, request.limits.maximum_roots, MigrationBaseCloneSeedKindV1::SyncPin, peer.node_id.to_be_bytes().to_vec(), root)?;
  }

  let tasks = TaskQueue::new(request.source.clone()).list_tasks()?;
  admit_count(tasks.len() as u64, request.limits.maximum_tasks, "task records")?;
  let (plugins, modules) = count_plugins(request.source, request.cancellation, request.limits.maximum_plugins)?;
  let symlinks = count_head_symlinks(source, &before.namespace_root, request.cancellation, request.limits)?;

  let mut seed_counts = MigrationFinalAuthoritySeedCountsV1::default();
  for row in &rows {
    match row.seed.kind {
      MigrationBaseCloneSeedKindV1::CurrentHead => seed_counts.current_heads += 1,
      MigrationBaseCloneSeedKindV1::Snapshot => seed_counts.snapshots += 1,
      MigrationBaseCloneSeedKindV1::Fork => seed_counts.forks += 1,
      MigrationBaseCloneSeedKindV1::SyncPin => seed_counts.sync_pins += 1,
      MigrationBaseCloneSeedKindV1::Maintenance => seed_counts.maintenance += 1,
      MigrationBaseCloneSeedKindV1::DetachedProtectedPath => seed_counts.detached_protected += 1,
    }
  }
  let roots = seed_counts.root_count()?;
  let history_roots = seed_counts
    .snapshots
    .checked_add(seed_counts.forks)
    .ok_or_else(|| EngineError::ResourceExhausted("v3 migration history-root count overflowed".to_string()))?;
  let counts = AuthorityInventoryCountsV1 {
    protected_families: u64::from(registry.family_count),
    modules,
    snapshots: seed_counts.snapshots,
    forks: seed_counts.forks,
    symlinks,
    history_roots,
    peers: peers.len() as u64,
    sync_states,
    tasks: tasks.len() as u64,
    plugins,
    roots,
  };
  let authority_digest = migration_final_authority_inventory_digest_v1(&rows, counts)
    .map_err(|error| EngineError::InvalidInput(format!("v3 migration authority digest failed: {error}")))?;

  check_cancelled(request.cancellation)?;
  let after_identity = source_identity(source)?;
  let after = source.frozen_source_authority_snapshot()?;
  if source_physical_identity != after_identity || before != after {
    return Err(EngineError::InvalidInput(
      "v3 migration source identity, header, HEAD, or hard-publication frontier changed during authority inventory".to_string(),
    ));
  }
  Ok(V3MigrationAuthorityInventoryV1 {
    database_id: request.database_id,
    source_physical_instance_id: request.source_physical_instance_id,
    source_physical_identity,
    source_header_sequence: before.header_sequence,
    source_root: before.namespace_root,
    source_publication_sequence: before.hard_publication_frontier,
    rows,
    seed_counts,
    counts,
    authority_digest,
    system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
  })
}

fn validate_request(request: &V3MigrationAuthorityInventoryRequestV1<'_>) -> EngineResult<()> {
  let limits = request.limits;
  if request.database_id.iter().all(|byte| *byte == 0)
    || request.source_physical_instance_id.iter().all(|byte| *byte == 0)
    || request.acquisition_timeout.is_zero()
    || request.acquisition_timeout > MAXIMUM_ACQUISITION_TIMEOUT
    || limits.maximum_roots == 0
    || limits.maximum_roots > MAXIMUM_COUNT_BOUND
    || limits.maximum_peers == 0
    || limits.maximum_peers > MAXIMUM_COUNT_BOUND
    || limits.maximum_tasks == 0
    || limits.maximum_tasks > MAXIMUM_COUNT_BOUND
    || limits.maximum_plugins == 0
    || limits.maximum_plugins > MAXIMUM_COUNT_BOUND
    || limits.maximum_namespace_memory_bytes == 0
    || limits.maximum_namespace_memory_bytes > MAXIMUM_NAMESPACE_MEMORY_BYTES
    || limits.maximum_namespace_work_items == 0
    || limits.maximum_namespace_work_items > MAXIMUM_NAMESPACE_WORK_ITEMS
    || limits.maximum_directory_depth == 0
    || limits.maximum_directory_depth > MAXIMUM_DIRECTORY_DEPTH
  {
    return Err(EngineError::InvalidInput("v3 migration authority inventory identities, timeout, or bounds are invalid".to_string()));
  }
  check_cancelled(request.cancellation)
}

fn validate_root(source: &StorageEngine, root: &[u8], role: &str) -> EngineResult<()> {
  if root.len() != source.hash_algo().hash_length() || root.iter().all(|byte| *byte == 0) {
    return Err(EngineError::InvalidInput(format!("v3 migration {role} root has an invalid hash")));
  }
  let (resolved, entry_type) = resolve_entry_reference_at_version(source, root, "/")?;
  if resolved != root || entry_type != EntryType::DirectoryIndex {
    return Err(EngineError::InvalidInput(format!("v3 migration {role} root is not a canonical DirectoryIndex")));
  }
  Ok(())
}

fn push_root(
  rows: &mut Vec<MigrationFinalAuthoritySeedV1>,
  maximum_roots: u64,
  kind: MigrationBaseCloneSeedKindV1,
  authority_identity: Vec<u8>,
  hash: Vec<u8>,
) -> EngineResult<()> {
  if rows.len() as u64 >= maximum_roots {
    return Err(EngineError::ResourceExhausted(format!("v3 migration root count exceeds configured maximum {maximum_roots}")));
  }
  rows.try_reserve(1).map_err(|error| allocation_error("authority root", error))?;
  rows.push(MigrationFinalAuthoritySeedV1 {
    authority_identity,
    source_write_sequence: 0,
    system_family_id: None,
    logical_bytes: 0,
    seed: MigrationBaseCloneSeedV1 { kind, path: "/".to_string(), entry_type: EntryType::DirectoryIndex, hash },
  });
  Ok(())
}

fn reject_duplicate_or_empty_names<'a>(names: impl Iterator<Item = &'a str>, role: &str) -> EngineResult<()> {
  let mut previous: Option<&str> = None;
  for name in names {
    if name.is_empty() || previous.is_some_and(|previous| previous.as_bytes() >= name.as_bytes()) {
      return Err(EngineError::InvalidInput(format!("v3 migration {role} identities are empty, duplicate, or noncanonical")));
    }
    previous = Some(name);
  }
  Ok(())
}

fn reject_duplicate_peers(peers: &[PeerConfig]) -> EngineResult<()> {
  if peers.iter().any(|peer| peer.node_id == 0) || peers.windows(2).any(|pair| pair[0].node_id >= pair[1].node_id) {
    return Err(EngineError::InvalidInput("v3 migration peer identities are zero or duplicate".to_string()));
  }
  Ok(())
}

fn decode_root(encoded: &str, width: usize, peer_node_id: u64) -> EngineResult<Vec<u8>> {
  let root = hex::decode(encoded)
    .map_err(|error| EngineError::InvalidInput(format!("v3 migration sync pin for peer {peer_node_id} is not hex: {error}")))?;
  if root.len() != width || root.iter().all(|byte| *byte == 0) {
    return Err(EngineError::InvalidInput(format!("v3 migration sync pin for peer {peer_node_id} has an invalid root hash")));
  }
  Ok(root)
}

fn count_plugins(source: &Arc<StorageEngine>, cancellation: &CancellationToken, maximum: u64) -> EngineResult<(u64, u64)> {
  let manager = PluginManager::new(source.clone());
  let mut offset = 0usize;
  let mut plugins = 0u64;
  let mut modules = 0u64;
  loop {
    check_cancelled(cancellation)?;
    let (keys, has_more) = system_store::list_plugin_keys_window(source, offset, PLUGIN_PAGE_SIZE)?;
    if keys.is_empty() {
      if has_more {
        return Err(EngineError::InvalidInput("v3 migration plugin listing made no progress".to_string()));
      }
      break;
    }
    offset =
      offset.checked_add(keys.len()).ok_or_else(|| EngineError::ResourceExhausted("v3 migration plugin offset overflowed".to_string()))?;
    for key in keys {
      plugins = checked_increment(plugins, "plugin count")?;
      admit_count(plugins, maximum, "plugins")?;
      let record = manager
        .get_plugin(&key)
        .map_err(|error| EngineError::InvalidInput(format!("v3 migration plugin '{key}' is invalid: {error}")))?
        .ok_or_else(|| EngineError::InvalidInput(format!("v3 migration plugin '{key}' disappeared during inventory")))?;
      if matches!(record.plugin_type, PluginType::Wasm | PluginType::Native) {
        modules = checked_increment(modules, "module count")?;
      }
    }
    if !has_more {
      break;
    }
  }
  Ok((plugins, modules))
}

fn count_head_symlinks(
  source: &StorageEngine,
  head: &[u8],
  cancellation: &CancellationToken,
  limits: V3MigrationAuthorityInventoryLimitsV1,
) -> EngineResult<u64> {
  let memory = source.memory_coordinator();
  count_strict_migration_tree_symlinks_v1(
    source,
    head,
    &memory,
    cancellation,
    limits.maximum_namespace_memory_bytes,
    limits.maximum_namespace_work_items,
    limits.maximum_directory_depth,
  )
  .map_err(|error| EngineError::InvalidInput(format!("v3 migration namespace inventory failed: {error}")))
}

fn source_identity(source: &StorageEngine) -> EngineResult<PlatformFileIdentityDescriptorV1> {
  platform_file_identity(source.database_path()).map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))
}

fn ensure_stream_open(open: bool, role: &str) -> EngineResult<()> {
  if open {
    Ok(())
  } else {
    Err(EngineError::InvalidInput(format!("v3 {role} authority stream was already finished")))
  }
}

fn ensure_stream_exhausted(remaining: usize, role: &str) -> EngineResult<()> {
  if remaining == 0 {
    Ok(())
  } else {
    Err(EngineError::InvalidInput(format!("v3 {role} authority stream has {remaining} unconsumed seeds")))
  }
}

fn admit_count(actual: u64, maximum: u64, role: &str) -> EngineResult<()> {
  if actual > maximum {
    Err(EngineError::ResourceExhausted(format!("v3 migration {role} count {actual} exceeds configured maximum {maximum}")))
  } else {
    Ok(())
  }
}

fn checked_increment(value: u64, role: &str) -> EngineResult<u64> {
  value.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted(format!("v3 migration {role} overflowed")))
}

fn check_cancelled(cancellation: &CancellationToken) -> EngineResult<()> {
  if cancellation.is_cancelled() {
    Err(EngineError::Cancelled("v3 migration authority inventory".to_string()))
  } else {
    Ok(())
  }
}

fn allocation_error(role: &str, error: std::collections::TryReserveError) -> EngineError {
  EngineError::ResourceExhausted(format!("v3 migration {role} allocation failed: {error}"))
}
