use std::fs::File;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use aeordb::engine::directory_ops::directory_path_hash;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};

struct Fixture {
  directory: tempfile::TempDir,
  database: std::path::PathBuf,
}

impl Fixture {
  fn new(stale_locator: bool) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("verify.aeordb");
    let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
    let operations = DirectoryOps::new(&engine);
    let context = RequestContext::system();
    operations.ensure_root_directory(&context).unwrap();
    operations.store_file_buffered(&context, "/docs/file.txt", b"retained repair proof", Some("text/plain")).unwrap();
    if stale_locator {
      let root_key = directory_path_hash("/", &engine.hash_algo()).unwrap();
      engine.store_entry(EntryType::DirectoryIndex, &root_key, &vec![0x5a; engine.hash_algo().hash_length()]).unwrap();
    }
    engine.shutdown().unwrap();
    Self { directory, database }
  }

  fn run(&self, repair: bool, filter: Option<&str>) -> Output {
    // File-backed output avoids deadlocking a verbose child on a full pipe.
    let stdout_path = self.directory.path().join("stdout.log");
    let stderr_path = self.directory.path().join("stderr.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aeordb"));
    command.args(["verify", "-D", self.database.to_str().unwrap()]).env_remove("AEORDB_LOG").env("NO_COLOR", "1");
    if repair {
      command.args(["--repair", "--force-fix-in-place"]);
    }
    if let Some(filter) = filter {
      command.env("AEORDB_LOG", filter);
    }
    let mut child = command
      .stdout(Stdio::from(File::create(&stdout_path).unwrap()))
      .stderr(Stdio::from(File::create(&stderr_path).unwrap()))
      .spawn()
      .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
      if let Some(status) = child.try_wait().unwrap() {
        return Output { status, stdout: std::fs::read(stdout_path).unwrap(), stderr: std::fs::read(stderr_path).unwrap() };
      }
      if Instant::now() >= deadline {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("verify CLI exceeded 30 seconds; output: {}", std::fs::read_to_string(stdout_path).unwrap());
      }
      std::thread::sleep(Duration::from_millis(20));
    }
  }
}

#[test]
fn verify_cli_emits_progress_by_default_and_preserves_explicit_filter_overrides() {
  let fixture = Fixture::new(false);
  let output = fixture.run(false, None);
  assert!(output.status.success(), "{output:?}");
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.contains("Status: OK"), "{stdout}");
  assert!(stdout.contains("AeorDB maintenance progress"), "default logging hid maintenance progress: {stdout}");
  for field in ["phase:", "units:", "elapsed_ms:", "cache_hits:", "cache_disk_reads:", "cache_eviction_candidates:"] {
    assert!(stdout.contains(field), "missing {field}: {stdout}");
  }

  let quiet = fixture.run(false, Some("off"));
  assert!(quiet.status.success(), "{quiet:?}");
  let quiet = String::from_utf8(quiet.stdout).unwrap();
  assert!(quiet.contains("Status: OK"));
  assert!(!quiet.contains("AeorDB maintenance progress"), "explicit filter must take precedence");
}

#[test]
fn repair_cli_publishes_a_stale_locator_then_verifies_and_preserves_payloads() {
  let fixture = Fixture::new(true);
  let corrupt_before = std::fs::read(&fixture.database).unwrap();
  let output = fixture.run(true, None);
  assert!(output.status.success(), "{output:?}");
  let stdout = String::from_utf8(output.stdout).unwrap();
  for expected in ["Status: OK", "Stale dir_keys rewritten: 1 fixed", "Repairs durably published.", "final_verification"] {
    assert!(stdout.contains(expected), "missing {expected}: {stdout}");
  }
  assert!(!fixture.database.with_extension("aeordb.repaired").exists());
  assert_ne!(std::fs::read(&fixture.database).unwrap(), corrupt_before);
  let strict = fixture.run(false, None);
  assert!(strict.status.success(), "{strict:?}");
  assert!(String::from_utf8(strict.stdout).unwrap().contains("Status: OK"));
  let engine = StorageEngine::open(fixture.database.to_str().unwrap()).unwrap();
  let record = DirectoryOps::new(&engine).read_file_buffered("/docs/file.txt").unwrap();
  assert_eq!(record, b"retained repair proof");
  engine.shutdown().unwrap();
}

#[test]
fn invalid_logging_override_fails_before_opening_or_repairing_the_database() {
  let fixture = Fixture::new(true);
  let before = std::fs::read(&fixture.database).unwrap();
  let output = fixture.run(true, Some("aeordb=not_a_level"));
  assert_eq!(output.status.code(), Some(1));
  assert!(String::from_utf8(output.stderr).unwrap().contains("invalid AEORDB_LOG"));
  assert_eq!(std::fs::read(&fixture.database).unwrap(), before);
}
