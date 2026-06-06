use std::process::Command;

use aeordb::engine::{DirectoryOps, RequestContext};
use aeordb::server::create_engine_for_storage;
use aeordb_cli::commands::probe::db_path_from_http_path;

#[test]
fn http_path_mapping_strips_files_prefix_and_query_string() {
  let mapping = db_path_from_http_path("/files/kikx/sessions/example/frames/0001.json?download=true", "/files");

  assert!(mapping.prefix_matched);
  assert_eq!(mapping.request_path, "/files/kikx/sessions/example/frames/0001.json");
  assert_eq!(mapping.route_prefix, "/files");
  assert_eq!(mapping.db_path, "/kikx/sessions/example/frames/0001.json");
}

#[test]
fn http_path_mapping_files_route_root_maps_to_database_root() {
  let mapping = db_path_from_http_path("/files", "/files");

  assert!(mapping.prefix_matched);
  assert_eq!(mapping.db_path, "/");
}

#[test]
fn http_path_mapping_normalizes_unmatched_route_without_hiding_mismatch() {
  let mapping = db_path_from_http_path("/engine/kikx//frames/0001.json", "/files");

  assert!(!mapping.prefix_matched);
  assert_eq!(mapping.request_path, "/engine/kikx//frames/0001.json");
  assert_eq!(mapping.route_prefix, "/files");
  assert_eq!(mapping.db_path, "/engine/kikx/frames/0001.json");
}

#[test]
fn probe_command_reports_metadata_chunks_and_read_timing_for_http_path() {
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("probe-read.aeordb");
  let db_path_str = db_path.to_str().unwrap();

  {
    let engine = create_engine_for_storage(db_path_str);
    let ops = DirectoryOps::new(&engine);
    ops
      .store_file_buffered(&RequestContext::system(), "/docs/readme.txt", b"hello diagnostic probe", Some("text/plain"))
      .expect("store file");
    engine.shutdown().expect("shutdown engine");
  }

  let output = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args(["probe", "-D", db_path_str, "--http-path", "/files/docs/readme.txt", "--read", "--chunks"])
    .output()
    .expect("run aeordb probe");

  assert!(
    output.status.success(),
    "probe failed: status={:?}\nstdout={}\nstderr={}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(stdout.contains("HTTP path mapping"), "stdout was:\n{stdout}");
  assert!(stdout.contains("DB path:       /docs/readme.txt"), "stdout was:\n{stdout}");
  assert!(stdout.contains("FileRecord: PRESENT"), "stdout was:\n{stdout}");
  assert!(stdout.contains("content_type: text/plain"), "stdout was:\n{stdout}");
  assert!(stdout.contains("chunk_count: 1"), "stdout was:\n{stdout}");
  assert!(stdout.contains("chunk[0]: PRESENT"), "stdout was:\n{stdout}");
  assert!(stdout.contains("verified stream read: OK"), "stdout was:\n{stdout}");
}

#[test]
fn probe_command_reports_missing_file_without_failing_the_diagnostic() {
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("probe-missing.aeordb");
  let db_path_str = db_path.to_str().unwrap();

  {
    let engine = create_engine_for_storage(db_path_str);
    engine.shutdown().expect("shutdown engine");
  }

  let output = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args(["probe", "-D", db_path_str, "--path", "/missing.txt", "--read"])
    .output()
    .expect("run aeordb probe");

  assert!(
    output.status.success(),
    "probe failed: status={:?}\nstdout={}\nstderr={}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(stdout.contains("FileRecord: MISSING"), "stdout was:\n{stdout}");
  assert!(stdout.contains("verified stream read: ERROR"), "stdout was:\n{stdout}");
}

#[test]
fn probe_command_supports_explicit_growth_stats_flag() {
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("probe-growth.aeordb");
  let db_path_str = db_path.to_str().unwrap();

  {
    let engine = create_engine_for_storage(db_path_str);
    engine.shutdown().expect("shutdown engine");
  }

  let output =
    Command::new(env!("CARGO_BIN_EXE_aeordb")).args(["probe", "-D", db_path_str, "--growth-stats"]).output().expect("run aeordb probe");

  assert!(
    output.status.success(),
    "probe failed: status={:?}\nstdout={}\nstderr={}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(stdout.contains("=== growth-stats ==="), "stdout was:\n{stdout}");
  assert!(stdout.contains("tail gap:"), "stdout was:\n{stdout}");
}

#[test]
fn probe_command_supports_explicit_wal_tail_bytes_without_valid_database() {
  let temp = tempfile::tempdir().expect("tempdir");
  let db_path = temp.path().join("not-a-real-db.aeordb");
  std::fs::write(&db_path, b"not a database").expect("write temp file");
  let db_path_str = db_path.to_str().unwrap();

  let output = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args(["probe", "-D", db_path_str, "--wal-tail-bytes", "4"])
    .output()
    .expect("run aeordb probe");

  assert!(
    output.status.success(),
    "probe failed: status={:?}\nstdout={}\nstderr={}",
    output.status.code(),
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr),
  );

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(stdout.contains("=== wal-tail-bytes"), "stdout was:\n{stdout}");
  assert!(stdout.contains("|base|"), "stdout was:\n{stdout}");
}
