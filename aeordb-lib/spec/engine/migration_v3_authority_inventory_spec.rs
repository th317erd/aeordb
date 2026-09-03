use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::peer_connection::PeerConfig;
use aeordb::engine::sync_engine::PeerSyncState;
use aeordb::engine::system_store::{PeerConfigStore, store_peer_sync_state};
use aeordb::engine::task_queue::TaskQueue;
use aeordb::engine::v4::migration_base_clone_execution::{MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedSourceV1};
use aeordb::engine::v4::migration_final_authority_reconciliation::MigrationFinalAuthorityInventorySourceV1;
use aeordb::engine::v4::migration_v3_authority_inventory::{
  V3MigrationAuthorityInventoryLimitsV1, V3MigrationAuthorityInventoryRequestV1, collect_v3_migration_authority_inventory_v1,
};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{RequestContext, StorageEngine};
use tokio_util::sync::CancellationToken;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn limits() -> V3MigrationAuthorityInventoryLimitsV1 {
  V3MigrationAuthorityInventoryLimitsV1 {
    maximum_roots: 64,
    maximum_peers: 64,
    maximum_tasks: 1_024,
    maximum_plugins: 64,
    maximum_namespace_memory_bytes: 64 * 1024 * 1024,
    maximum_namespace_work_items: 100_000,
    maximum_directory_depth: 128,
  }
}

fn collect<'a>(
  source: &'a Arc<StorageEngine>,
  cancellation: &'a CancellationToken,
  limits: V3MigrationAuthorityInventoryLimitsV1,
) -> V3MigrationAuthorityInventoryRequestV1<'a> {
  V3MigrationAuthorityInventoryRequestV1 {
    source,
    database_id: id(0x10),
    source_physical_instance_id: id(0x30),
    cancellation,
    acquisition_timeout: Duration::from_secs(2),
    limits,
  }
}

fn assert_invalid_request(
  source: &Arc<StorageEngine>,
  cancellation: &CancellationToken,
  configure: impl FnOnce(&mut V3MigrationAuthorityInventoryRequestV1<'_>),
) {
  let mut request = collect(source, cancellation, limits());
  configure(&mut request);
  assert!(collect_v3_migration_authority_inventory_v1(request).is_err());
}

fn populated_source() -> (tempfile::TempDir, std::path::PathBuf, Arc<StorageEngine>) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("source-v3.aeordb");
  let engine = Arc::new(StorageEngine::create(path.to_str().unwrap()).unwrap());
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.ensure_root_directory(&context).unwrap();
  operations.store_file_buffered(&context, "/docs/a.txt", b"first", Some("text/plain")).unwrap();
  operations.store_symlink(&context, "/docs/latest", "/docs/a.txt").unwrap();

  let versions = VersionManager::new(&engine);
  versions.create_snapshot(&context, "snap-z", HashMap::new()).unwrap();
  operations.store_file_buffered(&context, "/docs/a.txt", b"second", Some("text/plain")).unwrap();
  versions.create_snapshot(&context, "snap-a", HashMap::new()).unwrap();
  versions.create_fork(&context, "fork-z", Some("snap-z")).unwrap();
  versions.create_fork(&context, "fork-a", Some("HEAD")).unwrap();

  let peers = vec![
    PeerConfig {
      node_id: 3,
      address: "https://peer-three.invalid".to_string(),
      label: None,
      sync_paths: None,
      last_clock_offset_ms: None,
      last_wire_time_ms: None,
      last_jitter_ms: None,
      clock_state_at: None,
    },
    PeerConfig {
      node_id: 9,
      address: "https://peer-nine.invalid".to_string(),
      label: None,
      sync_paths: None,
      last_clock_offset_ms: None,
      last_wire_time_ms: None,
      last_jitter_ms: None,
      clock_state_at: None,
    },
  ];
  PeerConfigStore::new(&engine).replace_all(&context, peers).unwrap();
  let local_root = hex::encode(engine.head_hash().unwrap());
  store_peer_sync_state(
    &engine,
    &context,
    3,
    &PeerSyncState { last_synced_root_hash: None, last_local_root_hash: Some(local_root), last_sync_at: Some(1) },
  )
  .unwrap();
  store_peer_sync_state(
    &engine,
    &context,
    9,
    &PeerSyncState { last_synced_root_hash: None, last_local_root_hash: None, last_sync_at: Some(2) },
  )
  .unwrap();
  TaskQueue::new(engine.clone()).enqueue("rehearsal", serde_json::json!({"bounded": true})).unwrap();
  (directory, path, engine)
}

#[test]
fn real_v3_inventory_is_canonical_complete_and_read_only() {
  let (_directory, path, source) = populated_source();
  let cancellation = CancellationToken::new();
  let before = fs::read(&path).unwrap();
  let inventory = collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, limits())).unwrap();
  let evidence = inventory.preflight_evidence();

  assert!(evidence.complete);
  assert_eq!(evidence.unresolved_family_count, 0);
  assert_eq!(evidence.counts.snapshots, 2);
  assert_eq!(evidence.counts.forks, 2);
  assert_eq!(evidence.counts.history_roots, 4);
  assert_eq!(evidence.counts.peers, 2);
  assert_eq!(evidence.counts.sync_states, 2);
  assert_eq!(evidence.counts.tasks, 1);
  assert_eq!(evidence.counts.plugins, 0);
  assert_eq!(evidence.counts.modules, 0);
  assert_eq!(evidence.counts.symlinks, 1);
  assert_eq!(evidence.counts.roots, 6);
  assert_ne!(evidence.authority_digest, [0; 32]);

  let mut stream = inventory.into_final_authority_stream();
  let mut kinds_and_identities = Vec::new();
  while let Some(row) = stream.next_seed().unwrap() {
    kinds_and_identities.push((row.seed.kind, row.authority_identity));
  }
  assert_eq!(
    kinds_and_identities,
    vec![
      (MigrationBaseCloneSeedKindV1::CurrentHead, Vec::new()),
      (MigrationBaseCloneSeedKindV1::Snapshot, b"snap-a".to_vec()),
      (MigrationBaseCloneSeedKindV1::Snapshot, b"snap-z".to_vec()),
      (MigrationBaseCloneSeedKindV1::Fork, b"fork-a".to_vec()),
      (MigrationBaseCloneSeedKindV1::Fork, b"fork-z".to_vec()),
      (MigrationBaseCloneSeedKindV1::SyncPin, 3u64.to_be_bytes().to_vec()),
    ]
  );
  let final_closure = stream.finish().unwrap();
  assert_eq!(final_closure.authority_digest, evidence.authority_digest);
  assert_eq!(final_closure.source_authority_counts, evidence.counts);
  assert_eq!(fs::read(&path).unwrap(), before);

  let inventory = collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, limits())).unwrap();
  let mut base = inventory.into_base_clone_stream();
  let mut base_count = 0;
  while base.next_seed().unwrap().is_some() {
    base_count += 1;
  }
  assert_eq!(base_count, 6);
  let base_closure = base.finish().unwrap();
  assert_eq!(base_closure.source_authority_digest, evidence.authority_digest);
  assert_eq!(base_closure.source_authority_counts, evidence.counts);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn cancellation_limits_and_malformed_sync_pins_fail_closed() {
  let (_directory, path, source) = populated_source();
  let canceled = CancellationToken::new();
  canceled.cancel();
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &canceled, limits())).is_err());

  let active = CancellationToken::new();
  let mut root_limited = limits();
  root_limited.maximum_roots = 5;
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &active, root_limited)).is_err());

  let context = RequestContext::system();
  store_peer_sync_state(
    &source,
    &context,
    3,
    &PeerSyncState { last_synced_root_hash: None, last_local_root_hash: Some("not-hex".to_string()), last_sync_at: Some(3) },
  )
  .unwrap();
  let before = fs::read(&path).unwrap();
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &active, limits())).is_err());
  assert_eq!(fs::read(&path).unwrap(), before);

  for malformed in [hex::encode([0u8; 32]), hex::encode([0x11u8; 31]), hex::encode([0x22u8; 32])] {
    store_peer_sync_state(
      &source,
      &context,
      3,
      &PeerSyncState { last_synced_root_hash: None, last_local_root_hash: Some(malformed), last_sync_at: Some(4) },
    )
    .unwrap();
    let before = fs::read(&path).unwrap();
    assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &active, limits())).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn authority_streams_require_complete_single_consumption() {
  let (_directory, _path, source) = populated_source();
  let cancellation = CancellationToken::new();

  let inventory = collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, limits())).unwrap();
  let mut base = inventory.into_base_clone_stream();
  assert!(base.finish().is_err(), "base-clone closure must not be available before every seed is consumed");
  while base.next_seed().unwrap().is_some() {}
  base.finish().unwrap();
  assert!(base.finish().is_err(), "base-clone closure must be single-use");
  assert!(base.next_seed().is_err(), "a finished base-clone stream must stay closed");

  let inventory = collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, limits())).unwrap();
  let mut final_authority = inventory.into_final_authority_stream();
  assert!(final_authority.finish().is_err(), "final-authority closure must not be available before every seed is consumed");
  while final_authority.next_seed().unwrap().is_some() {}
  final_authority.finish().unwrap();
  assert!(final_authority.finish().is_err(), "final-authority closure must be single-use");
  assert!(final_authority.next_seed().is_err(), "a finished final-authority stream must stay closed");
}

#[test]
fn every_inventory_identity_time_and_upper_bound_is_validated() {
  let (_directory, _path, source) = populated_source();
  let cancellation = CancellationToken::new();

  assert_invalid_request(&source, &cancellation, |request| request.database_id = [0; 16]);
  assert_invalid_request(&source, &cancellation, |request| request.source_physical_instance_id = [0; 16]);
  assert_invalid_request(&source, &cancellation, |request| request.acquisition_timeout = Duration::ZERO);
  assert_invalid_request(&source, &cancellation, |request| request.acquisition_timeout = Duration::from_secs(86_401));

  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_roots = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_roots = 1_000_001);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_peers = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_peers = 1_000_001);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_tasks = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_tasks = 1_000_001);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_plugins = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_plugins = 1_000_001);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_namespace_memory_bytes = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_namespace_memory_bytes = 1024 * 1024 * 1024 + 1);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_namespace_work_items = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_namespace_work_items = (1 << 40) + 1);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_directory_depth = 0);
  assert_invalid_request(&source, &cancellation, |request| request.limits.maximum_directory_depth = 1_001);

  let mut peer_limited = limits();
  peer_limited.maximum_peers = 1;
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, peer_limited)).is_err());

  TaskQueue::new(source.clone()).enqueue("second-rehearsal", serde_json::json!({"bounded": true})).unwrap();
  let mut task_limited = limits();
  task_limited.maximum_tasks = 1;
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, task_limited)).is_err());

  let mut memory_limited = limits();
  memory_limited.maximum_namespace_memory_bytes = 1;
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, memory_limited)).is_err());

  let mut work_limited = limits();
  work_limited.maximum_namespace_work_items = 1;
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, work_limited)).is_err());

  let mut depth_limited = limits();
  depth_limited.maximum_directory_depth = 1;
  assert!(collect_v3_migration_authority_inventory_v1(collect(&source, &cancellation, depth_limited)).is_err());
}
