use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};

const SOURCE_COMMIT: &str = "2121212121212121212121212121212121212121";

fn aeordb() -> Command {
  Command::new(env!("CARGO_BIN_EXE_aeordb"))
}

fn output_with_timeout(mut command: Command) -> Output {
  let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn aeordb migration command");
  let deadline = Instant::now() + Duration::from_secs(20);
  loop {
    if child.try_wait().expect("poll aeordb migration command").is_some() {
      return child.wait_with_output().expect("collect aeordb migration output");
    }
    if Instant::now() >= deadline {
      child.kill().expect("terminate hung aeordb migration command");
      let output = child.wait_with_output().expect("collect timed-out aeordb migration output");
      panic!(
        "aeordb migration command exceeded 20 seconds\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
      );
    }
    std::thread::sleep(Duration::from_millis(20));
  }
}

struct Fixture {
  _temporary: tempfile::TempDir,
  source: std::path::PathBuf,
  destination: std::path::PathBuf,
  workspace: std::path::PathBuf,
  source_before: Vec<u8>,
}

impl Fixture {
  fn new() -> Self {
    let temporary = tempfile::tempdir().expect("create migration CLI fixture");
    let root = temporary.path().canonicalize().expect("canonicalize migration CLI fixture");
    let source = root.join("source-v3.aeordb");
    let engine = StorageEngine::create(source.to_str().expect("source path is UTF-8")).expect("create v3 source");
    let operations = DirectoryOps::new(&engine);
    let context = RequestContext::system();
    operations.ensure_root_directory(&context).expect("create root directory");
    operations
      .store_file_buffered(&context, "/docs/readme.txt", b"operator CLI migration proof", Some("text/plain"))
      .expect("store source file");
    engine.shutdown().expect("close v3 source");
    let source_before = std::fs::read(&source).expect("read source fixture");
    Self {
      _temporary: temporary,
      source,
      destination: root.join("destination-v4.aeordb"),
      workspace: root.join("migration-workspace"),
      source_before,
    }
  }

  fn base_command(&self) -> Command {
    let mut command = aeordb();
    command.args([
      "migrate-v4",
      "--source",
      self.source.to_str().expect("source path is UTF-8"),
      "--destination",
      self.destination.to_str().expect("destination path is UTF-8"),
      "--workspace",
      self.workspace.to_str().expect("workspace path is UTF-8"),
      "--migration-capture-max-bytes",
      "1GiB",
      "--migration-capture-free-reserve-bytes",
      "1GiB",
      "--migration-checkpoint-after-seconds",
      "30",
      "--json",
    ]);
    command
  }

  fn fresh_command(&self) -> Command {
    let mut command = self.base_command();
    command.args(["--root-map-maximum-stored-bytes", "32MiB", "--root-map-minimum-free-bytes", "0", "--source-commit", SOURCE_COMMIT]);
    command
  }
}

#[test]
fn migrate_v4_help_exposes_the_versioned_non_cutover_operator_surface() {
  let output = aeordb().args(["migrate-v4", "--help"]).output().expect("run migration help");
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let help = String::from_utf8(output.stdout).expect("migration help is UTF-8");
  for required in ["--source", "--destination", "--workspace", "--source-commit", "--resume", "--json"] {
    assert!(help.contains(required), "migration help omitted {required}:\n{help}");
  }
  for forbidden in ["cutover", "accept", "first-v4-write", "activate"] {
    assert!(!help.to_ascii_lowercase().contains(forbidden), "shadow-only command exposed forbidden {forbidden}:\n{help}");
  }
}

#[test]
fn migrate_v4_fresh_and_resume_emit_bound_receipts_without_changing_the_source() {
  let fixture = Fixture::new();
  let fresh = fixture.fresh_command();

  let fresh = output_with_timeout(fresh);

  assert!(fresh.status.success(), "{}", String::from_utf8_lossy(&fresh.stderr));
  let fresh_receipt: serde_json::Value = serde_json::from_slice(&fresh.stdout).expect("decode fresh migration receipt");
  assert_eq!(fresh_receipt["protocol"], "aeordb.offline-migration.receipt.v1");
  assert_eq!(fresh_receipt["resumed"], false);
  assert_eq!(fresh_receipt["phase"], "destination_verify");
  assert_eq!(fresh_receipt["state"], "complete");
  assert_eq!(fresh_receipt["destination_full_verified"], true);
  assert_eq!(fresh_receipt["binary"]["source_commit"], SOURCE_COMMIT);
  let progress = String::from_utf8(fresh.stderr).expect("fresh migration progress is UTF-8");
  let milestones =
    progress.lines().map(|line| serde_json::from_str::<serde_json::Value>(line).expect("every progress line is JSON")).collect::<Vec<_>>();
  assert!(milestones.iter().all(|event| event["protocol"] == "aeordb.offline-migration.progress.v1"));
  assert!(milestones.iter().any(|event| event["milestone"] == "copy_running"));
  assert!(milestones.iter().any(|event| event["milestone"] == "destination_verification_complete"));
  for identity in ["database_id", "migration_id", "source_physical_instance_id", "destination_physical_instance_id", "holder_boot_id"] {
    let value = fresh_receipt["identity"][identity].as_str().expect("identity is a string");
    assert_eq!(value.len(), 32, "{identity}");
    assert_ne!(value, "00000000000000000000000000000000", "{identity}");
  }
  assert_ne!(fresh_receipt["identity"]["source_physical_instance_id"], fresh_receipt["identity"]["destination_physical_instance_id"]);
  assert_eq!(std::fs::read(&fixture.source).expect("read source after migration"), fixture.source_before);
  assert!(fixture.destination.is_file());
  assert!(fixture.workspace.join("migration-run-v1.json").is_file());
  let destination_before_resume = std::fs::read(&fixture.destination).expect("read completed destination");

  let mut resume = fixture.base_command();
  resume.arg("--resume");
  let resume = output_with_timeout(resume);

  assert!(resume.status.success(), "{}", String::from_utf8_lossy(&resume.stderr));
  let resume_receipt: serde_json::Value = serde_json::from_slice(&resume.stdout).expect("decode resumed migration receipt");
  assert_eq!(resume_receipt["resumed"], true);
  assert_eq!(resume_receipt["identity"], fresh_receipt["identity"]);
  assert_eq!(resume_receipt["binary"], fresh_receipt["binary"]);
  assert_eq!(std::fs::read(&fixture.source).expect("read source after resume"), fixture.source_before);
  assert_eq!(std::fs::read(&fixture.destination).expect("read destination after resume"), destination_before_resume);

  let mut conflicting_resume = fixture.base_command();
  conflicting_resume.args(["--resume", "--maximum-memory-bytes", "128MiB"]);
  let conflicting_resume = output_with_timeout(conflicting_resume);
  assert_eq!(conflicting_resume.status.code(), Some(1));
  let error: serde_json::Value = serde_json::from_slice(&conflicting_resume.stderr).expect("decode immutable-bound error");
  assert_eq!(error["code"], "migration_cli_resume_bounds");
  assert_eq!(std::fs::read(&fixture.destination).expect("read destination after refused bound override"), destination_before_resume);
}

#[test]
fn migrate_v4_rejects_malformed_provenance_and_missing_resume_state_without_creating_artifacts() {
  let fixture = Fixture::new();
  let mut malformed = fixture.base_command();
  malformed.args(["--source-commit", "not-a-commit"]);
  let malformed = output_with_timeout(malformed);
  assert_eq!(malformed.status.code(), Some(1));
  let malformed_error: serde_json::Value = serde_json::from_slice(&malformed.stderr).expect("decode malformed-provenance error");
  assert_eq!(malformed_error["protocol"], "aeordb.offline-migration.error.v1");
  assert_eq!(malformed_error["code"], "migration_cli_source_commit");
  assert!(!fixture.destination.exists());
  assert!(!fixture.workspace.exists());
  assert_eq!(std::fs::read(&fixture.source).expect("read source after malformed command"), fixture.source_before);

  let mut resume = fixture.base_command();
  resume.arg("--resume");
  let resume = output_with_timeout(resume);
  assert_eq!(resume.status.code(), Some(1));
  let resume_error: serde_json::Value = serde_json::from_slice(&resume.stderr).expect("decode missing-resume error");
  assert_eq!(resume_error["protocol"], "aeordb.offline-migration.error.v1");
  assert!(resume_error["code"].as_str().expect("error code").starts_with("migration_run_manifest_"));
  assert!(!fixture.destination.exists());
  assert_eq!(std::fs::read(&fixture.source).expect("read source after refused resume"), fixture.source_before);
}
