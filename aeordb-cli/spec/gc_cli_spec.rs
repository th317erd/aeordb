use std::process::Command;

use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};

fn seeded_database() -> (tempfile::TempDir, std::path::PathBuf) {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join("gc-cli.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/live.txt", b"live", Some("text/plain")).unwrap();
  engine.shutdown().unwrap();
  (temporary, database)
}

#[test]
fn cli_dry_run_executes_the_real_gc_command_path() {
  let (_temporary, database) = seeded_database();

  let output =
    Command::new(env!("CARGO_BIN_EXE_aeordb")).args(["gc", "-D", database.to_str().unwrap(), "--dry-run"]).output().expect("run aeordb gc");

  assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.contains("AeorDB Garbage Collection [DRY RUN]"));
  assert!(stdout.contains("[DRY RUN] Would collect"));
}

#[test]
fn cli_surfaces_database_open_failure_without_starting_gc() {
  let (_temporary, database) = seeded_database();
  let _locked = StorageEngine::open(database.to_str().unwrap()).unwrap();

  let output = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args(["gc", "-D", database.to_str().unwrap(), "--dry-run"])
    .output()
    .expect("run locked aeordb gc");

  assert!(!output.status.success());
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.contains("Error opening database"), "unexpected stderr: {stderr}");
}
