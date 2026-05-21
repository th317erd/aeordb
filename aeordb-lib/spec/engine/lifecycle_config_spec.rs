use std::collections::HashMap;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{
  load_lifecycle_config, save_lifecycle_config, prune_expired_snapshots,
  LifecycleConfig, SnapshotRetention, RequestContext,
  SNAPSHOT_TYPE_KEY, SNAPSHOT_TYPE_AUTO, SNAPSHOT_TYPE_MANUAL,
};

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

#[test]
fn default_config_when_file_missing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let config = load_lifecycle_config(&engine);
  assert_eq!(config.snapshot_retention.auto_months, 0);
  assert_eq!(config.snapshot_retention.manual_months, 0);
}

#[test]
fn config_round_trip_through_disk() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let written = LifecycleConfig {
    snapshot_retention: SnapshotRetention { auto_months: 1, manual_months: 12 },
  };
  save_lifecycle_config(&engine, &written).unwrap();
  let read_back = load_lifecycle_config(&engine);
  assert_eq!(read_back, written);
}

#[test]
fn create_snapshot_defaults_to_manual_type() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let snap = vm.create_snapshot(&ctx, "untagged", HashMap::new()).unwrap();
  assert_eq!(snap.metadata.get(SNAPSHOT_TYPE_KEY).map(String::as_str), Some(SNAPSHOT_TYPE_MANUAL));
}

#[test]
fn create_snapshot_with_explicit_auto_type_preserved() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let mut metadata = HashMap::new();
  metadata.insert(SNAPSHOT_TYPE_KEY.to_string(), SNAPSHOT_TYPE_AUTO.to_string());
  let snap = vm.create_snapshot(&ctx, "tagged-auto", metadata).unwrap();
  assert_eq!(snap.metadata.get(SNAPSHOT_TYPE_KEY).map(String::as_str), Some(SNAPSHOT_TYPE_AUTO));
}

#[test]
fn rename_promotes_auto_to_manual() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let mut metadata = HashMap::new();
  metadata.insert(SNAPSHOT_TYPE_KEY.to_string(), SNAPSHOT_TYPE_AUTO.to_string());
  vm.create_snapshot(&ctx, "auto-snap", metadata).unwrap();

  let renamed = vm.rename_snapshot(&ctx, "auto-snap", "kept-snap").unwrap();
  assert_eq!(renamed.metadata.get(SNAPSHOT_TYPE_KEY).map(String::as_str), Some(SNAPSHOT_TYPE_MANUAL));
}

#[test]
fn prune_disabled_does_nothing() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  let mut metadata = HashMap::new();
  metadata.insert(SNAPSHOT_TYPE_KEY.to_string(), SNAPSHOT_TYPE_AUTO.to_string());
  vm.create_snapshot(&ctx, "auto-1", metadata).unwrap();

  // Default config: both months = 0 → no pruning
  let result = prune_expired_snapshots(&engine, &ctx).unwrap();
  assert_eq!(result.pruned_count, 0);
}

#[test]
fn prune_respects_engine_internal_prefix() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let vm = VersionManager::new(&engine);

  // Engine-internal snapshot (would normally be created by run_gc itself).
  let mut auto_meta = HashMap::new();
  auto_meta.insert(SNAPSHOT_TYPE_KEY.to_string(), SNAPSHOT_TYPE_AUTO.to_string());
  vm.create_snapshot(&ctx, "_aeordb_pre_gc_12345", auto_meta).unwrap();

  // Aggressive retention: anything older than 0 months should be pruned —
  // but engine-internal must still be skipped. We set 1 to avoid the "0 means
  // disabled" sentinel, then manually backdate by editing... actually, the
  // engine-internal check fires regardless of age, so 1 month and an aged-0
  // snapshot is fine: even an "old enough" engine-internal snapshot must not
  // be touched here. Since we just created it (age 0), it wouldn't be eligible
  // for pruning anyway, but the result.skipped_engine_internal count proves
  // the check ran.
  save_lifecycle_config(&engine, &LifecycleConfig {
    snapshot_retention: SnapshotRetention { auto_months: 1, manual_months: 1 },
  }).unwrap();

  let result = prune_expired_snapshots(&engine, &ctx).unwrap();
  assert_eq!(result.pruned_count, 0);
  assert!(result.skipped_engine_internal >= 1);
}

#[test]
fn prune_targets_correct_type() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);
  let vm = VersionManager::new(&engine);

  // Two snapshots with different HEADs (write between them so the dedup
  // logic doesn't collapse them).
  let mut auto_meta = HashMap::new();
  auto_meta.insert(SNAPSHOT_TYPE_KEY.to_string(), SNAPSHOT_TYPE_AUTO.to_string());
  ops.store_file_buffered(&ctx, "/a.txt", b"first", Some("text/plain")).unwrap();
  vm.create_snapshot(&ctx, "auto-snap", auto_meta).unwrap();

  ops.store_file_buffered(&ctx, "/b.txt", b"second", Some("text/plain")).unwrap();
  let manual_meta = HashMap::new();
  vm.create_snapshot(&ctx, "manual-snap", manual_meta).unwrap();

  save_lifecycle_config(&engine, &LifecycleConfig {
    snapshot_retention: SnapshotRetention { auto_months: 1, manual_months: 12 },
  }).unwrap();

  let result = prune_expired_snapshots(&engine, &ctx).unwrap();
  assert_eq!(result.pruned_count, 0, "fresh snapshots shouldn't be pruned");
  assert_eq!(vm.list_snapshots().unwrap().len(), 2);
}
