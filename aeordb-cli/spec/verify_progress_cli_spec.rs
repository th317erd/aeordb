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
    let arguments = if repair { &["--repair", "--force-fix-in-place"][..] } else { &[] };
    self.run_command("verify", arguments, filter)
  }

  fn run_command(&self, subcommand: &str, arguments: &[&str], filter: Option<&str>) -> Output {
    // File-backed output avoids deadlocking a verbose child on a full pipe.
    let stdout_path = self.directory.path().join("stdout.log");
    let stderr_path = self.directory.path().join("stderr.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aeordb"));
    command.args([subcommand, "-D", self.database.to_str().unwrap()]).args(arguments).env_remove("AEORDB_LOG").env("NO_COLOR", "1");
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
        panic!("{subcommand} CLI exceeded 30 seconds; output: {}", std::fs::read_to_string(stdout_path).unwrap());
      }
      std::thread::sleep(Duration::from_millis(20));
    }
  }
}

#[test]
fn verify_cli_preserves_clean_database_bytes_and_modification_time() {
  let fixture = Fixture::new(false);
  let before = std::fs::read(&fixture.database).unwrap();
  let modified_before = std::fs::metadata(&fixture.database).unwrap().modified().unwrap();
  let output = fixture.run(false, Some("off"));
  assert!(output.status.success(), "{output:?}");
  assert!(before == std::fs::read(&fixture.database).unwrap(), "non-repair verification changed clean database bytes");
  assert_eq!(std::fs::metadata(&fixture.database).unwrap().modified().unwrap(), modified_before);
}

#[test]
fn verify_cli_reports_stale_locators_without_changing_source_bytes() {
  let fixture = Fixture::new(true);
  let before = std::fs::read(&fixture.database).unwrap();
  let output = fixture.run(false, Some("off"));
  assert_eq!(output.status.code(), Some(2), "{output:?}");
  assert!(String::from_utf8_lossy(&output.stdout).contains("Stale dir_key entries"), "{output:?}");
  assert!(before == std::fs::read(&fixture.database).unwrap(), "verification changed the stale-locator evidence");
}

#[test]
fn verify_cli_accepts_a_read_only_clean_database_without_rewriting_it() {
  let fixture = Fixture::new(false);
  let before = std::fs::read(&fixture.database).unwrap();
  let original_permissions = std::fs::metadata(&fixture.database).unwrap().permissions();
  let mut read_only = original_permissions.clone();
  read_only.set_readonly(true);
  std::fs::set_permissions(&fixture.database, read_only).unwrap();
  let output = fixture.run(false, Some("off"));
  std::fs::set_permissions(&fixture.database, original_permissions).unwrap();
  assert!(output.status.success(), "{output:?}");
  assert!(before == std::fs::read(&fixture.database).unwrap(), "verification changed an OS-read-only source");
}

#[test]
fn verify_cli_refuses_recovery_needing_state_without_mutating_it() {
  let fixture = Fixture::new(false);
  let length = std::fs::metadata(&fixture.database).unwrap().len();
  std::fs::OpenOptions::new().write(true).open(&fixture.database).unwrap().set_len(length - 7).unwrap();
  let before = std::fs::read(&fixture.database).unwrap();
  let output = fixture.run(false, Some("off"));
  assert!(!output.status.success(), "read-only verification silently recovered a truncated source: {output:?}");
  assert!(before == std::fs::read(&fixture.database).unwrap(), "verification changed a source requiring explicit recovery");
}

#[test]
fn normal_startup_recovery_remains_explicit_and_subsequent_verification_is_read_only() {
  let fixture = Fixture::new(false);
  let length = std::fs::metadata(&fixture.database).unwrap().len();
  std::fs::OpenOptions::new().write(true).open(&fixture.database).unwrap().set_len(length - 7).unwrap();
  let damaged = std::fs::read(&fixture.database).unwrap();
  let reopen = fixture.run_command("probe", &["--growth-stats"], Some("off"));
  assert!(reopen.status.success(), "{reopen:?}");
  let recovered = std::fs::read(&fixture.database).unwrap();
  assert!(recovered != damaged, "explicit normal startup did not publish its recovery");
  let verify = fixture.run(false, Some("off"));
  assert!(verify.status.success(), "{verify:?}");
  assert!(recovered == std::fs::read(&fixture.database).unwrap());
  let engine = StorageEngine::open(fixture.database.to_str().unwrap()).unwrap();
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered("/docs/file.txt").unwrap(), b"retained repair proof");
  engine.shutdown().unwrap();
}

#[test]
fn verify_cli_refuses_malformed_sources_without_rewriting_them() {
  let fixture = Fixture::new(false);
  std::fs::write(&fixture.database, b"not a database").unwrap();
  let output = fixture.run(false, Some("off"));
  assert!(!output.status.success(), "{output:?}");
  assert_eq!(std::fs::read(&fixture.database).unwrap(), b"not a database");
}

#[test]
fn verify_cli_does_not_create_a_missing_database() {
  let fixture = Fixture::new(false);
  std::fs::remove_file(&fixture.database).unwrap();
  let output = fixture.run(false, Some("off"));
  assert!(!output.status.success(), "{output:?}");
  assert!(!fixture.database.exists());
}

#[test]
fn verify_cli_respects_the_exclusive_database_lock_without_changing_source_bytes() {
  let fixture = Fixture::new(false);
  let owner = StorageEngine::open(fixture.database.to_str().unwrap()).unwrap();
  let before = std::fs::read(&fixture.database).unwrap();
  let output = fixture.run(false, Some("off"));
  assert!(!output.status.success(), "{output:?}");
  assert!(String::from_utf8_lossy(&output.stderr).contains("locked by another process"), "{output:?}");
  assert!(before == std::fs::read(&fixture.database).unwrap(), "a rejected lock acquisition changed the source");
  owner.shutdown().unwrap();
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
