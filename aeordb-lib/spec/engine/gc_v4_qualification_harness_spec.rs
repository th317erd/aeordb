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
