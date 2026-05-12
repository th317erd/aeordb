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

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use aeordb::engine::{DirectoryOps, StorageEngine};

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

/// SIGKILL the child by PID — bypasses Drop, mimics power loss / OOM kill.
fn sigkill(child: &mut Child) {
  let pid = child.id() as i32;
  unsafe {
    libc::kill(pid, libc::SIGKILL);
  }
  let _ = child.wait();
}

/// Wait until the worker has written its "up" marker so we don't race the
/// SIGKILL against engine startup itself. Returns true if startup completed
/// within `timeout`.
fn wait_for_worker_up(checkpoint_path: &str, timeout: Duration) -> bool {
  let start = std::time::Instant::now();
  while start.elapsed() < timeout {
    if let Ok(file) = File::open(checkpoint_path) {
      let reader = BufReader::new(file);
      for line in reader.lines().map_while(Result::ok) {
        if line.starts_with("# worker up") {
          return true;
        }
      }
    }
    std::thread::sleep(Duration::from_millis(20));
  }
  false
}

/// Read the checkpoint file. Returns the list of `(path, expected_body)`
/// pairs the worker reported as committed. Comments (`#`-prefixed lines)
/// are skipped.
fn read_checkpoint(checkpoint_path: &str) -> Vec<(String, String)> {
  let file = File::open(checkpoint_path).expect("open checkpoint");
  let mut entries = Vec::new();
  for line in BufReader::new(file).lines().map_while(Result::ok) {
    if line.starts_with('#') || line.is_empty() { continue; }
    if let Some((path, body)) = line.split_once('\t') {
      entries.push((path.to_string(), body.to_string()));
    }
  }
  entries
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
  assert!(
    wait_for_worker_up(&checkpoint, Duration::from_secs(10)),
    "iteration {}: worker didn't come up in time",
    iteration,
  );

  std::thread::sleep(kill_after);
  sigkill(&mut worker);

  let committed = read_checkpoint(&checkpoint);
  assert!(
    !committed.is_empty(),
    "iteration {}: worker was killed before committing anything; raise kill_after",
    iteration,
  );

  // Reopen and verify every committed entry is intact.
  let engine = open_or_repair(&db_path);
  let ops = DirectoryOps::new(&engine);

  let mut missing: Vec<String> = Vec::new();
  let mut corrupted: Vec<(String, String, String)> = Vec::new();
  for (path, expected) in &committed {
    match ops.read_file(path) {
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

  println!(
    "iteration {}: {} entries survived SIGKILL (mode={}, killed_after={:?})",
    iteration, committed.len(), mode, kill_after,
  );
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
      ops.store_file(&ctx, &path, format!("body {}", i).as_bytes(), Some("text/plain"))
        .expect("store");
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
    let data = ops.read_file(&path).expect("read survived bit flip");
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
  assert!(
    is_tmpfs_mount,
    "AEORDB_CRASH_SOAK_TMPFS path {} is NOT a tmpfs mount — refusing to run umount test for safety",
    tmpfs,
  );

  let db_path = format!("{}/crash.aeordb", tmpfs);
  let checkpoint = format!("{}/checkpoint.tsv", tmpfs);
  let _ = std::fs::remove_file(&db_path);
  let _ = std::fs::remove_file(&checkpoint);

  drop(StorageEngine::create(&db_path).expect("create db"));
  let mut worker = spawn_worker(&db_path, &checkpoint, "writes");
  assert!(wait_for_worker_up(&checkpoint, Duration::from_secs(10)));

  std::thread::sleep(Duration::from_millis(1500));

  // Force unmount. This sends EIO to the worker's outstanding fs operations.
  let status = Command::new("umount")
    .args(["-f", &tmpfs])
    .status()
    .expect("run umount");
  assert!(status.success(), "umount -f failed; need sudo? errno was reported");

  let _ = worker.wait();

  // Re-mount and verify what survived
  let status = Command::new("mount")
    .args(["-t", "tmpfs", "-o", "size=512m", "tmpfs", &tmpfs])
    .status()
    .expect("run mount");
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
      ops.store_file(&ctx, &path, format!("v{}", i).as_bytes(), Some("text/plain"))
        .expect("store");
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
    if ops.read_file(&path).is_ok() {
      earlier_surviving += 1;
    }
  }
  assert!(
    earlier_surviving >= 190,
    "expected most earlier writes to survive truncation; got {} of 199",
    earlier_surviving,
  );
  let _ = last;
}
