use super::*;

#[test]
fn absent_v4_runtime_is_a_noop_for_the_shared_index_timer_tick() {
  let directory = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(directory.path().join("no-v4-cadence.aeordb").to_str().unwrap()).unwrap();

  assert_eq!(engine.flush_index_runtime_if_due_v1().unwrap(), None);
  assert!(engine.index_runtime_snapshot_v1().is_none());
}
