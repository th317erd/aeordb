use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn read_script(relative_path: &str) -> String {
  let path = workspace_root().join(relative_path);
  std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("qualification script {} is missing or unreadable: {error}", path.display()))
}

#[test]
fn qualification_blob_fixture_hash_is_stable() {
  let payload = b"p4-8e-qualification-blob-payload-v1\n";
  let mut hash_input = b"chunk:".to_vec();
  hash_input.extend_from_slice(payload);
  assert_eq!(hex::encode(blake3::hash(&hash_input).as_bytes()), "5b5cb25c9365b67b6648d6a273e0b7d0a80719903fd8af7355a89f6bea335f5c");
}

#[test]
fn live_gc_qualification_harness_freezes_resource_and_non_destructive_contracts() {
  let shell = read_script("scripts/qualify-v4-gc.sh");
  let load = read_script("scripts/qualify-v4-gc-load.mjs");

  for required in [
    "set -euo pipefail",
    "CARGO_BUILD_JOBS",
    "-j \"$CARGO_BUILD_JOBS\"",
    "systemd-run --user",
    "MemoryMax=8G",
    "MemorySwapMax=0",
    "timeout \"$QUALIFICATION_TIMEOUT_SECS\"",
    "--auth disabled",
    "memory.peak",
    "memory.swap.peak",
    "verify -D",
    "Online KV block expansion complete",
    "unexpected error-level server log",
  ] {
    assert!(shell.contains(required), "live qualification shell is missing required contract {required:?}");
  }
  assert!(!shell.contains("/tmp"), "qualification evidence and temporary files must remain under the caller-owned cache root");
  assert!(!shell.contains("dry_run=false"), "P4-8e must not cross the destructive P4-9 gate");

  for required in [
    "/system/health",
    "/files/qualify/",
    "/files/search",
    "/blobs/check",
    "/blobs/chunks/",
    "/blobs/commit",
    "/system/tasks/reindex",
    "/system/gc?dry_run=true",
    "AbortController",
  ] {
    assert!(load.contains(required), "live qualification workload is missing required operation {required:?}");
  }
  assert!(!load.contains("dry_run=false"), "the live workload must never request destructive GC");
  assert!(
    !load.contains("Math.max(..."),
    "qualification inputs are externally configurable, so function-argument spreads must stay bounded"
  );
}

#[test]
fn legacy_soak_builds_are_job_bounded_and_diagnostics_use_owned_scratch() {
  let soak = read_script("scripts/soak.sh");

  assert!(soak.contains("CARGO_BUILD_JOBS=\"${CARGO_BUILD_JOBS:-4}\""));
  assert!(soak.contains("-j \"$CARGO_BUILD_JOBS\""));
  assert!(soak.contains("AEORDB_SOAK_SCRATCH"));
  assert!(soak.contains("mktemp -p \"$SCRATCH_ROOT\""));
  assert!(!soak.lines().any(|line| line.trim() == "verify_log=\"$(mktemp)\""));
  assert!(!soak.lines().any(|line| line.trim() == "diag_dir=\"$(mktemp -d)\""));
}

#[test]
fn legacy_soak_propagates_diagnostic_failures_to_process_status() {
  let soak = read_script("scripts/soak.sh");

  assert_eq!(soak.matches("verify_status=0").count(), 2, "both crash modes must capture a failing verifier exit status");
  assert_eq!(soak.matches("|| verify_status=$?").count(), 2, "both crash modes must retain the verifier exit status");
  assert_eq!(
    soak.matches("SOAK_FAILURES=$((SOAK_FAILURES + 1))").count(),
    2,
    "each crash mode must accumulate failed iterations instead of printing and forgetting them"
  );
  assert!(soak.contains("finish_chaos_soak"), "the soak harness must centralize its final pass/fail exit contract");
  assert!(soak.contains("if [ \"$SOAK_FAILURES\" -gt 0 ]; then"), "retained diagnostic failures must produce a failing process status");
  assert!(soak.contains("if ! finish_chaos_soak \"S2\" \"$iteration\"; then"));
  assert!(soak.contains("if ! finish_chaos_soak \"S3\" \"$iteration\"; then"));
  assert_eq!(soak.matches("dangling_records=$(get_field \"Dangling records\")").count(), 2);
  assert_eq!(soak.matches("btree_issues=$(get_field \"B-tree issues\")").count(), 2);
  assert_eq!(soak.matches("verification_errors=$(count_report_lines \"  Verification error:\")").count(), 2);
  assert_eq!(soak.matches("stale_dir_keys=$(count_report_lines \"Stale dir_key entries (\")").count(), 2);
  assert_eq!(
    soak.matches("if verify_report_is_acceptable; then").count(),
    2,
    "both crash modes must reject every verifier issue except an explicitly counted torn terminal header"
  );
}

#[test]
fn legacy_soak_checkpoint_intents_are_durable_before_database_mutation() {
  let worker = read_script("aeordb-cli/src/bin/soak-worker.rs");
  let append_start = worker.find("fn append_checkpoint(").expect("soak worker must centralize checkpoint publication");
  let append_end =
    worker[append_start..].find("\n#[cfg(test)]").map(|offset| append_start + offset).expect("checkpoint helper boundary changed");
  let append = &worker[append_start..append_end];

  assert!(append.contains("writer.flush()"), "checkpoint publication must flush userspace buffers");
  assert!(
    append.contains("sync_file_data_native"),
    "checkpoint publication must cross a native durability barrier before a pending delete can be followed by a database commit"
  );
}

#[test]
fn aggressive_soak_waits_for_the_initial_durable_startup_checkpoint() {
  let soak = read_script("scripts/soak.sh");
  let spawn = soak.find("\"$CRASH_WORKER\" \\").expect("S3 worker spawn is present");
  let recorder = soak[spawn..].find("start_pmap_recorder").map(|offset| spawn + offset).expect("S3 pmap recorder is present");
  let startup_wait = soak[spawn..]
    .find("wait_for_s3_startup_checkpoint \"$worker_pid\"")
    .map(|offset| spawn + offset)
    .expect("S3 must await the worker's durable startup checkpoint before its first SIGKILL window");

  assert!(startup_wait < recorder, "the readiness gate must run before the first crash timer/recorder starts");
  assert!(soak.contains("^# worker up mode=stress$"), "the harness must consume the exact durable marker emitted after engine open/create");
  assert!(soak.contains("AEORDB_SOAK_S3_STARTUP_TIMEOUT_SECS"), "the startup wait must have an operator-visible finite bound");
  assert!(soak.contains("current_markers=${current_markers:-0}"), "a not-yet-created checkpoint must normalize to zero current markers");
  assert!(
    soak.contains("startup_markers_before=${startup_markers_before:-0}"),
    "a missing checkpoint before the first spawn must normalize to zero prior markers"
  );
}

#[test]
fn runtime_metrics_acquire_writer_before_kv_snapshot_and_share_one_engine_path() {
  let metrics_pulse = read_script("aeordb-lib/src/engine/metrics_pulse.rs");
  let portal_routes = read_script("aeordb-lib/src/server/portal_routes.rs");
  let storage_engine = read_script("aeordb-lib/src/engine/storage_engine.rs");

  assert!(metrics_pulse.contains("engine.kv_observability_metrics()"));
  assert!(!metrics_pulse.contains("engine.kv_snapshot.load()"));
  assert!(portal_routes.contains("state.engine.kv_observability_metrics()"));
  assert!(!portal_routes.contains("state.engine.kv_snapshot.load()"));

  let method_start =
    storage_engine.find("pub fn kv_observability_metrics").expect("StorageEngine must own the combined KV observability path");
  let method_end = storage_engine[method_start..]
    .find("\n  /// Perform online KV block expansion")
    .map(|offset| method_start + offset)
    .expect("KV observability method boundary changed");
  let method = &storage_engine[method_start..method_end];
  let writer_lock = method.find(".writer").expect("KV observability must read the writer header");
  let snapshot_load = method.find("self.kv_snapshot.load()").expect("KV observability must read the immutable KV view");
  assert!(
    writer_lock < snapshot_load,
    "KV observability must acquire and release the writer read lock before taking a snapshot lease; expansion holds the writer while draining leases"
  );
}

#[test]
fn gc_sweep_keeps_kv_offsets_bound_to_one_stable_wal_layout() {
  let gc = read_script("aeordb-lib/src/engine/gc.rs");
  let storage_engine = read_script("aeordb-lib/src/engine/storage_engine.rs");

  let sweep_start = gc.find("fn gc_sweep_internal(").expect("GC sweep owner is present");
  let sweep_end = gc[sweep_start..]
    .find("\n/// Run a complete garbage collection cycle")
    .map(|offset| sweep_start + offset)
    .expect("GC sweep owner boundary changed");
  let sweep = &gc[sweep_start..sweep_end];
  assert!(
    sweep.contains("visit_kv_entries_with_stable_wal"),
    "GC sweep must not retain KV offsets after releasing the WAL layout that makes those offsets meaningful"
  );
  assert!(!sweep.contains("engine.iter_kv_entries()?"), "GC sweep must not copy offsets out of an unpinned WAL layout");

  let visit_start =
    storage_engine.find("fn visit_kv_entries_with_stable_wal").expect("StorageEngine must own the stable KV/WAL visit lock order");
  let visit_end =
    storage_engine[visit_start..].find("\n  ///").map(|offset| visit_start + offset).expect("stable KV/WAL visit boundary changed");
  let visit = &storage_engine[visit_start..visit_end];
  let writer_lock = visit.find("self.writer.read()").expect("stable visit must retain the WAL layout");
  let snapshot_load = visit.find("self.kv_snapshot.load()").expect("stable visit must select one immutable KV view");
  assert!(
    writer_lock < snapshot_load,
    "stable visit must lock the WAL before leasing KV pages so expansion cannot deadlock while draining"
  );
}
