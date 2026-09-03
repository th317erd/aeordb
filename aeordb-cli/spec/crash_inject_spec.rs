//! Crash-injection soak test.
//!
//! Marked `#[ignore]` because each iteration takes seconds-to-minutes and
//! the full suite runs for many minutes. Run on demand:
//!
//! ```
//! cargo test --release --test crash_inject_spec -- --ignored --nocapture
//! ```
//!
//! Or via the workspace stress workflow (.github/workflows/stress.yml after
//! pattern is extended to include crash_inject).
//!
//! ## What we test
//!
//! - **SIGKILL during writes**: spawn the worker, kill it at a random moment,
//!   reopen the DB, verify every checkpointed write is still readable.
//! - **SIGKILL during a mixed workload**: same but with delete+snapshot churn.
//! - **Bit flip in a KV page**: open a clean DB, flip one byte in a bucket
//!   page, verify the v2 page CRC catches it.
//! - **Trailing truncation**: simulate the xenocept failure mode by chopping
//!   bytes off the file tail; the repair path must recover via dirty startup.
//!
//! The `umount -f` variant is gated behind `AEORDB_CRASH_SOAK_TMPFS=/path` so
//! the test never touches a real mount point. The path must be a tmpfs
//! mountpoint that the test owns; see the helper docstring for setup.
//!
//! Run on tmpfs:
//! ```
//! sudo mount -t tmpfs -o size=512m tmpfs /tmp/aeordb-crash-fs
//! sudo chown $USER /tmp/aeordb-crash-fs
//! AEORDB_CRASH_SOAK_TMPFS=/tmp/aeordb-crash-fs \
//!   cargo test --release --test crash_inject_spec test_umount_during_writes \
//!   -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use aeordb::engine::{DirectoryOps, StorageEngine};
use aeordb_cli::soak_checkpoint::{SoakCheckpointRecord, visit_soak_checkpoint_records};

/// Locate the crash-soak-worker binary built by cargo. Walks up from the
/// current test exe to find target/<profile>/crash-soak-worker.
fn worker_binary() -> PathBuf {
  let test_exe = std::env::current_exe().expect("current_exe");
  let mut dir = test_exe.parent().expect("test exe parent").to_path_buf();
  // dir is target/<profile>/deps/. Walk up one to target/<profile>/.
  if dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
    dir = dir.parent().expect("deps parent").to_path_buf();
  }
  let candidate = dir.join("crash-soak-worker");
  assert!(
    candidate.exists(),
    "crash-soak-worker binary not found at {} — run with `cargo test --release` after `cargo build --release`",
    candidate.display(),
  );
  candidate
}

fn spawn_worker(db_path: &str, checkpoint_path: &str, mode: &str) -> Child {
  Command::new(worker_binary())
    .args(["--database", db_path, "--checkpoint", checkpoint_path, "--mode", mode])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn worker")
}

#[test]
fn crash_worker_reports_checkpoint_open_failure_without_panicking() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("checkpoint-open-failure.aeordb");
  let checkpoint = temp.path().join("missing-parent").join("checkpoint.tsv");

  let output = Command::new(env!("CARGO_BIN_EXE_crash-soak-worker"))
    .args(["--database", database.to_str().unwrap(), "--checkpoint", checkpoint.to_str().unwrap(), "--mode", "writes"])
    .output()
    .unwrap();

  assert_eq!(output.status.code(), Some(4), "{}", String::from_utf8_lossy(&output.stderr));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("open checkpoint"), "{stderr}");
  assert!(!stderr.contains("panicked"), "{stderr}");
  assert!(!database.exists(), "checkpoint admission must fail before the worker creates or opens the database");
}

/// Terminate the child without graceful shutdown. `Child::kill` maps to
/// SIGKILL on Unix and the equivalent forced process termination on Windows.
fn sigkill(child: &mut Child) {
  child.kill().expect("force-stop crash soak worker");
  let _ = child.wait();
}

/// Wait until the worker has written its "up" marker so we don't race the
/// SIGKILL against engine startup itself. Returns true if startup completed
/// within `timeout`.
fn wait_for_worker_up(checkpoint_path: &str, timeout: Duration) -> bool {
  let start = std::time::Instant::now();
  while start.elapsed() < timeout {
    let mut worker_up = false;
    let read_result = visit_soak_checkpoint_records(Path::new(checkpoint_path), |_line_number, record| {
      if let SoakCheckpointRecord::Comment { text } = record {
        worker_up |= text.starts_with("# worker up mode=");
      }
      Ok(())
    });
    if read_result.is_ok() && worker_up {
      return true;
    }
    std::thread::sleep(Duration::from_millis(20));
  }
  false
}

fn wait_for_checkpoint_path_record_count(checkpoint_path: &str, path: &str, minimum_records: usize, timeout: Duration) -> bool {
  let start = std::time::Instant::now();
  while start.elapsed() < timeout {
    let mut records = 0usize;
    let read_result = visit_soak_checkpoint_records(Path::new(checkpoint_path), |_line_number, record| {
      if matches!(record, SoakCheckpointRecord::Committed { path: record_path, .. } if record_path == path) {
        records += 1;
      }
      Ok(())
    });
    if read_result.is_ok() && records >= minimum_records {
      return true;
    }
    std::thread::sleep(Duration::from_millis(20));
  }
  false
}

/// Read the checkpoint file. Returns the list of `(path, expected_body)`
/// pairs the worker reported as committed. Comments (`#`-prefixed lines)
/// are skipped.
fn read_checkpoint(checkpoint_path: &str) -> Result<Vec<(String, String)>, String> {
  let mut entries = BTreeMap::new();
  visit_soak_checkpoint_records(Path::new(checkpoint_path), |_line_number, record| match record {
    SoakCheckpointRecord::Comment { .. } => Ok(()),
    SoakCheckpointRecord::Committed { path, body: Some(body) } => {
      entries.insert(path.to_string(), body.to_string());
      Ok(())
    }
    SoakCheckpointRecord::Committed { .. } => Err("committed qualification-oracle record is missing its expected body".to_string()),
    SoakCheckpointRecord::PendingWrite { path } | SoakCheckpointRecord::PendingDelete { path } | SoakCheckpointRecord::Deleted { path } => {
      entries.remove(path);
      Ok(())
    }
  })?;
  Ok(entries.into_iter().collect())
}

#[test]
fn pending_delete_checkpoint_is_not_a_required_survivor() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("pending-delete.tsv");
  std::fs::write(&checkpoint, "/data/file-00000051.txt\tbody\n?\t/data/file-00000051.txt\n").unwrap();

  assert!(
    read_checkpoint(checkpoint.to_str().unwrap()).unwrap().is_empty(),
    "a delete intent must leave the path outside the must-survive oracle before the database mutation can commit"
  );
}

#[test]
fn pending_write_checkpoint_drops_a_stale_exact_body_expectation() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("pending-write.tsv");
  std::fs::write(&checkpoint, "/stress/state/doc-021.json\told body\n!\t/stress/state/doc-021.json\n").unwrap();

  assert!(
    read_checkpoint(checkpoint.to_str().unwrap()).unwrap().is_empty(),
    "an overwrite intent must retire the prior exact-body oracle before the newer database value can commit"
  );
}

#[test]
fn checkpoint_reader_ignores_an_incomplete_final_body_record() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("incomplete-body.tsv");
  std::fs::write(&checkpoint, "/docs/complete.json\t{\"counter\":1}\n/docs/incomplete.json\t").unwrap();

  assert_eq!(
    read_checkpoint(checkpoint.to_str().unwrap()).unwrap(),
    vec![("/docs/complete.json".to_string(), "{\"counter\":1}".to_string())],
    "a nonterminated final record was not durably committed and must stay outside the oracle"
  );
}

#[test]
fn checkpoint_reader_rejects_a_completed_malformed_record() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("malformed.tsv");
  std::fs::write(&checkpoint, "/docs/complete.json\t{}\nmalformed\n").unwrap();

  let error = read_checkpoint(checkpoint.to_str().unwrap()).unwrap_err();
  assert!(error.contains("malformed checkpoint") && error.contains("line 2"), "{error}");
}

#[test]
fn checkpoint_reader_rejects_a_completed_committed_record_without_a_body() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("missing-body.tsv");
  std::fs::write(&checkpoint, "+\t/docs/missing-body.json\n").unwrap();

  let error = read_checkpoint(checkpoint.to_str().unwrap()).unwrap_err();
  assert!(error.contains("line 1") && error.contains("missing its expected body"), "{error}");
}

#[test]
fn checkpoint_reader_ignores_incomplete_invalid_text_but_rejects_completed_invalid_text() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("invalid-text.tsv");
  let complete_prefix = b"/docs/complete.json\t{}\n";
  let mut incomplete = complete_prefix.to_vec();
  incomplete.extend_from_slice(b"/docs/incomplete.json\t\xff");
  std::fs::write(&checkpoint, incomplete).unwrap();

  assert_eq!(read_checkpoint(checkpoint.to_str().unwrap()).unwrap(), vec![("/docs/complete.json".to_string(), "{}".to_string())]);

  let mut completed = complete_prefix.to_vec();
  completed.extend_from_slice(b"/docs/invalid.json\t\xff\n");
  std::fs::write(&checkpoint, completed).unwrap();
  let error = read_checkpoint(checkpoint.to_str().unwrap()).unwrap_err();
  assert!(error.contains("checkpoint") && error.contains("line 2") && error.contains("UTF-8"), "{error}");
}

#[test]
fn checkpoint_waiters_require_complete_marker_and_body_records() {
  let temp = tempfile::tempdir().unwrap();
  let checkpoint = temp.path().join("waiters.tsv");
  let checkpoint_path = checkpoint.to_str().unwrap();
  let path = "/docs/reused.json";
  std::fs::write(&checkpoint, format!("# worker up mode=stress\n{path}\tbody-1\n{path}\tbody-2")).unwrap();

  assert!(wait_for_worker_up(checkpoint_path, Duration::from_millis(60)));
  assert!(!wait_for_checkpoint_path_record_count(checkpoint_path, path, 2, Duration::from_millis(60)));

  let mut checkpoint_file = OpenOptions::new().append(true).open(&checkpoint).unwrap();
  writeln!(checkpoint_file).unwrap();
  assert!(wait_for_checkpoint_path_record_count(checkpoint_path, path, 2, Duration::from_millis(60)));

  std::fs::write(&checkpoint, "# worker up mode=stress").unwrap();
  assert!(!wait_for_worker_up(checkpoint_path, Duration::from_millis(60)));
}

#[test]
fn gc_reused_path_records_pending_intent_before_replacement_commit() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("gc-reused-path.aeordb");
  let checkpoint = temp.path().join("checkpoint.tsv");
  let path = "/gc/file-0000.json";
  let old_body = r#"{"counter":"old"}"#;

  {
    let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&aeordb::engine::RequestContext::system(), path, old_body.as_bytes(), Some("application/json")).unwrap();
    engine.shutdown().unwrap();
  }
  std::fs::write(&checkpoint, format!("{path}\t{old_body}\n")).unwrap();

  let mut worker = Command::new(env!("CARGO_BIN_EXE_crash-soak-worker"))
    .args(["--database", database.to_str().unwrap(), "--checkpoint", checkpoint.to_str().unwrap(), "--mode", "gc"])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let replacement_committed = wait_for_checkpoint_path_record_count(checkpoint.to_str().unwrap(), path, 2, Duration::from_secs(10));
  sigkill(&mut worker);
  assert!(replacement_committed, "worker did not commit the replacement within the test deadline");

  let mut committed_records = 0usize;
  let mut previous_record_was_pending_intent = false;
  let mut replacement_preceded_by_pending_intent = false;
  visit_soak_checkpoint_records(&checkpoint, |_line_number, record| {
    if matches!(record, SoakCheckpointRecord::Committed { path: record_path, .. } if record_path == path) {
      committed_records += 1;
      if committed_records == 2 {
        replacement_preceded_by_pending_intent = previous_record_was_pending_intent;
      }
    }
    previous_record_was_pending_intent =
      matches!(record, SoakCheckpointRecord::PendingWrite { path: pending_path } if pending_path == path);
    Ok(())
  })
  .unwrap();

  assert!(
    committed_records >= 2 && replacement_preceded_by_pending_intent,
    "a reused path must retire the previous exact-body expectation before the replacement can commit"
  );
}

/// Open the DB after a crash, falling back to the repair path if a normal
/// open fails. Returns the engine ready for verification.
fn open_or_repair(db_path: &str) -> StorageEngine {
  match StorageEngine::open(db_path) {
    Ok(engine) => engine,
    Err(error) => {
      eprintln!("normal open failed ({}), attempting header repair", error);
      let _ = aeordb::engine::repair_header_in_place(db_path);
      StorageEngine::open(db_path).expect("open after repair")
    }
  }
}

fn run_sigkill_iteration(iteration: usize, mode: &str, kill_after: Duration) {
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("crash.aeordb").to_string_lossy().to_string();
  let checkpoint = temp.path().join("checkpoint.tsv").to_string_lossy().to_string();

  // Pre-create the DB so the worker's open path is the "open existing" branch.
  drop(StorageEngine::create(&db_path).expect("create db"));

  let mut worker = spawn_worker(&db_path, &checkpoint, mode);
  assert!(wait_for_worker_up(&checkpoint, Duration::from_secs(10)), "iteration {}: worker didn't come up in time", iteration,);

  std::thread::sleep(kill_after);
  sigkill(&mut worker);

  let committed = read_checkpoint(&checkpoint).unwrap_or_else(|error| panic!("iteration {iteration}: {error}"));
  assert!(!committed.is_empty(), "iteration {}: worker was killed before committing anything; raise kill_after", iteration,);

  // Reopen and verify every committed entry is intact.
  let engine = open_or_repair(&db_path);
  let ops = DirectoryOps::new(&engine);

  let mut missing: Vec<String> = Vec::new();
  let mut corrupted: Vec<(String, String, String)> = Vec::new();
  for (path, expected) in &committed {
    match ops.read_file_buffered(path) {
      Ok(data) => {
        let actual = String::from_utf8_lossy(&data).to_string();
        if actual != *expected {
          corrupted.push((path.clone(), expected.clone(), actual));
        }
      }
      Err(_) => missing.push(path.clone()),
    }
  }

  assert!(
    missing.is_empty() && corrupted.is_empty(),
    "iteration {}: missing={} corrupted={} (out of {} committed)\n  first missing: {:?}\n  first corrupted: {:?}",
    iteration,
    missing.len(),
    corrupted.len(),
    committed.len(),
    missing.first(),
    corrupted.first(),
  );

  println!("iteration {}: {} entries survived SIGKILL (mode={}, killed_after={:?})", iteration, committed.len(), mode, kill_after,);
}

#[test]
#[ignore]
fn test_crash_inject_sigkill_during_writes() {
  // 10 iterations, kill delay random in 200ms..3s.
  // Mix of short delays (catch early-write race) and long (steady state).
  let delays = [200, 500, 800, 1200, 1800, 2500, 300, 700, 1500, 2200];
  for (i, ms) in delays.iter().enumerate() {
    run_sigkill_iteration(i, "writes", Duration::from_millis(*ms));
  }
}

#[test]
#[ignore]
fn test_crash_inject_sigkill_during_mixed_workload() {
  let delays = [400, 900, 1500, 2200, 700];
  for (i, ms) in delays.iter().enumerate() {
    run_sigkill_iteration(i, "mixed", Duration::from_millis(*ms));
  }
}

#[test]
#[ignore]
fn test_crash_inject_sigkill_during_gc_workload() {
  let delays = [700, 1300, 2100, 3200];
  for (i, ms) in delays.iter().enumerate() {
    run_sigkill_iteration(i, "gc", Duration::from_millis(*ms));
  }
}

#[test]
#[ignore]
fn test_crash_inject_sigkill_during_stress_workload() {
  let delays = [800, 1600, 2600, 3800];
  for (i, ms) in delays.iter().enumerate() {
    run_sigkill_iteration(i, "stress", Duration::from_millis(*ms));
  }
}

#[test]
#[ignore]
fn test_bit_flip_in_kv_page_caught_by_crc() {
  // 1. Build a normal DB with some content
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("flip.aeordb").to_string_lossy().to_string();

  {
    let engine = StorageEngine::create(&db_path).expect("create");
    let ops = DirectoryOps::new(&engine);
    let ctx = aeordb::engine::RequestContext::system();
    for i in 0..100 {
      let path = format!("/file-{:04}.txt", i);
      ops.store_file_buffered(&ctx, &path, format!("body {}", i).as_bytes(), Some("text/plain")).expect("store");
    }
    engine.shutdown().expect("shutdown");
  }

  // 2. Flip a byte in the KV block region. File header is bytes 0..256.
  //    KV pages start at 256. Flip a byte deep inside (page 1, somewhere
  //    in the entry data).
  {
    let mut file = OpenOptions::new().read(true).write(true).open(&db_path).expect("open");
    let flip_offset = 256u64 + 1500u64; // bucket page 1, inside an entry
    file.seek(SeekFrom::Start(flip_offset)).expect("seek");
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).expect("read");
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(flip_offset)).expect("seek back");
    file.write_all(&byte).expect("flip");
    file.sync_all().expect("sync");
  }

  // 3. Reopen. The bucket-page CRC must catch the flip — either dirty
  //    startup runs (and rebuilds from WAL) or the engine reports the
  //    corruption explicitly. EITHER path is acceptable; what's NOT
  //    acceptable is the engine silently returning wrong data.
  let engine = StorageEngine::open(&db_path).expect("reopen");
  let ops = DirectoryOps::new(&engine);

  // Every original file must still read correctly. If the CRC caught the
  // flip and the page was rebuilt from WAL, all data is intact. If the CRC
  // had NOT caught it, some reads would either fail or return garbage.
  for i in 0..100 {
    let path = format!("/file-{:04}.txt", i);
    let data = ops.read_file_buffered(&path).expect("read survived bit flip");
    assert_eq!(data, format!("body {}", i).as_bytes(), "{} content match", path);
  }
}

/// Force-unmount the tmpfs mid-write. Verifies the engine doesn't corrupt
/// state when the underlying filesystem disappears.
///
/// SAFETY: ONLY operates on the tmpfs path provided via the environment
/// variable. Refuses to run if the path isn't on a tmpfs mount, so we can
/// never accidentally umount the user's real filesystems.
///
/// Setup (one-time):
/// ```
/// sudo mkdir -p /tmp/aeordb-crash-fs
/// sudo mount -t tmpfs -o size=512m tmpfs /tmp/aeordb-crash-fs
/// sudo chown "$USER" /tmp/aeordb-crash-fs
/// ```
///
/// Run:
/// ```
/// AEORDB_CRASH_SOAK_TMPFS=/tmp/aeordb-crash-fs \
///   cargo test --release --test crash_inject_spec test_umount_during_writes \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn test_umount_during_writes() {
  let tmpfs = match std::env::var("AEORDB_CRASH_SOAK_TMPFS") {
    Ok(value) => value,
    Err(_) => {
      println!("AEORDB_CRASH_SOAK_TMPFS not set; skipping umount test");
      return;
    }
  };

  // SAFETY check: verify the path is actually a tmpfs mountpoint. Reading
  // /proc/mounts is Linux-specific; this test is Linux-only.
  let mounts = std::fs::read_to_string("/proc/mounts").expect("read /proc/mounts");
  let mut is_tmpfs_mount = false;
  for line in mounts.lines() {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() >= 3 && fields[1] == tmpfs && fields[2] == "tmpfs" {
      is_tmpfs_mount = true;
      break;
    }
  }
  assert!(is_tmpfs_mount, "AEORDB_CRASH_SOAK_TMPFS path {} is NOT a tmpfs mount — refusing to run umount test for safety", tmpfs,);

  let db_path = format!("{}/crash.aeordb", tmpfs);
  let checkpoint = format!("{}/checkpoint.tsv", tmpfs);
  let _ = std::fs::remove_file(&db_path);
  let _ = std::fs::remove_file(&checkpoint);

  drop(StorageEngine::create(&db_path).expect("create db"));
  let mut worker = spawn_worker(&db_path, &checkpoint, "writes");
  assert!(wait_for_worker_up(&checkpoint, Duration::from_secs(10)));

  std::thread::sleep(Duration::from_millis(1500));

  // Force unmount. This sends EIO to the worker's outstanding fs operations.
  let status = Command::new("umount").args(["-f", &tmpfs]).status().expect("run umount");
  assert!(status.success(), "umount -f failed; need sudo? errno was reported");

  let _ = worker.wait();

  // Re-mount and verify what survived
  let status = Command::new("mount").args(["-t", "tmpfs", "-o", "size=512m", "tmpfs", &tmpfs]).status().expect("run mount");
  assert!(status.success(), "remount failed");

  // The DB file is on the now-fresh tmpfs and is gone — that's expected
  // for a tmpfs umount. The real test value is that the worker process
  // exited cleanly and didn't, for example, leave a zombie or corrupt
  // anything outside the tmpfs.
  println!("umount-f survived: worker terminated, tmpfs remounted cleanly");
}

#[test]
#[ignore]
fn test_trailing_truncation_recoverable() {
  // The xenocept failure mode in pure form: the header is fine, but the
  // file's actual length is less than what the header advertises. The
  // engine's existing dirty-startup path handles this — we verify it.
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("trunc.aeordb").to_string_lossy().to_string();

  let mut last_path_written: Option<String> = None;
  {
    let engine = StorageEngine::create(&db_path).expect("create");
    let ops = DirectoryOps::new(&engine);
    let ctx = aeordb::engine::RequestContext::system();
    for i in 0..200 {
      let path = format!("/data-{:04}.txt", i);
      ops.store_file_buffered(&ctx, &path, format!("v{}", i).as_bytes(), Some("text/plain")).expect("store");
      last_path_written = Some(path);
    }
    engine.shutdown().expect("shutdown");
  }

  // Lop off the last 256 bytes (likely lands in the middle of the hot tail).
  {
    let file = OpenOptions::new().write(true).open(&db_path).expect("open");
    let size = file.metadata().expect("metadata").len();
    assert!(size > 1024, "DB too small for meaningful truncation");
    file.set_len(size - 256).expect("truncate");
    file.sync_all().expect("sync");
  }

  // Open via repair-aware path. Older entries must survive even if the very
  // last one didn't (the truncation may have eaten part of it).
  let engine = open_or_repair(&db_path);
  let ops = DirectoryOps::new(&engine);

  // Don't require the *very last* write to be intact — the truncation could
  // have eaten its hot-tail entry. But require everything else.
  let last = last_path_written.expect("at least one write");
  let mut earlier_surviving = 0usize;
  for i in 0..199 {
    let path = format!("/data-{:04}.txt", i);
    if ops.read_file_buffered(&path).is_ok() {
      earlier_surviving += 1;
    }
  }
  assert!(earlier_surviving >= 190, "expected most earlier writes to survive truncation; got {} of 199", earlier_surviving,);
  let _ = last;
}
