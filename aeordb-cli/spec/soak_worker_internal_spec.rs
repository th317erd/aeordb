use super::*;
use std::io;
use std::sync::Mutex;

#[test]
fn diagnostic_worker_result_preserves_each_failure_combination() {
  assert_eq!(combine_diagnostic_worker_results(Ok(()), Ok(())), Ok(()));
  assert_eq!(combine_diagnostic_worker_results(Err("metrics failed".to_string()), Ok(())), Err("metrics failed".to_string()));
  assert_eq!(combine_diagnostic_worker_results(Ok(()), Err("RSS failed".to_string())), Err("RSS failed".to_string()));
  assert_eq!(
    combine_diagnostic_worker_results(Err("metrics failed".to_string()), Err("RSS failed".to_string())),
    Err("diagnostic workers failed: metrics: metrics failed; wide RSS: RSS failed".to_string())
  );
}

#[test]
fn shutdown_result_preserves_diagnostics_and_engine_failures() {
  assert_eq!(combine_soak_shutdown_results(Ok(()), Ok(())), Ok(()));
  assert_eq!(combine_soak_shutdown_results(Err("diagnostics failed".to_string()), Ok(())), Err("diagnostics failed".to_string()));
  assert_eq!(combine_soak_shutdown_results(Ok(()), Err("shutdown failed".to_string())), Err("shutdown failed".to_string()));
  assert_eq!(
    combine_soak_shutdown_results(Err("diagnostics failed".to_string()), Err("shutdown failed".to_string())),
    Err("soak shutdown failed: diagnostics: diagnostics failed; engine: shutdown failed".to_string())
  );
}

#[test]
fn workload_result_preserves_workload_and_cleanup_failures() {
  assert_eq!(combine_workload_cleanup_results(Ok(()), Ok(())), Ok(()));
  assert_eq!(combine_workload_cleanup_results(Err("workload failed".to_string()), Ok(())), Err("workload failed".to_string()));
  assert_eq!(combine_workload_cleanup_results(Ok(()), Err("cleanup failed".to_string())), Err("cleanup failed".to_string()));
  assert_eq!(
    combine_workload_cleanup_results(Err("workload failed".to_string()), Err("cleanup failed".to_string())),
    Err("soak workload and cleanup failed: workload: workload failed; cleanup: cleanup failed".to_string())
  );
}

struct FailingWriter {
  fail_write: bool,
  fail_flush: bool,
  bytes: Vec<u8>,
}

impl io::Write for FailingWriter {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    if self.fail_write {
      return Err(io::Error::other("injected write failure"));
    }
    self.bytes.extend_from_slice(buffer);
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    if self.fail_flush {
      return Err(io::Error::other("injected flush failure"));
    }
    Ok(())
  }
}

#[test]
fn checkpoint_append_requires_both_write_and_flush() {
  let mut writer = FailingWriter { fail_write: false, fail_flush: false, bytes: Vec::new() };
  append_checkpoint_line(&mut writer, '+', "/docs/a.txt").unwrap();
  assert_eq!(writer.bytes, b"+\t/docs/a.txt\n");

  let mut write_failure = FailingWriter { fail_write: true, fail_flush: false, bytes: Vec::new() };
  let error = append_checkpoint_line(&mut write_failure, '+', "/docs/a.txt").unwrap_err();
  assert!(error.contains("checkpoint write failed") && error.contains("/docs/a.txt"));

  let mut flush_failure = FailingWriter { fail_write: false, fail_flush: true, bytes: Vec::new() };
  let error = append_checkpoint_line(&mut flush_failure, '-', "/docs/a.txt").unwrap_err();
  assert!(error.contains("checkpoint flush failed") && error.contains("/docs/a.txt"));
}

#[test]
fn checkpoint_load_distinguishes_missing_from_unreadable_or_malformed_state() {
  let temporary = tempfile::tempdir().unwrap();
  let missing = temporary.path().join("missing.tsv");
  assert!(load_checkpoint(&missing).unwrap().is_empty());

  let unreadable = temporary.path().join("checkpoint-directory");
  std::fs::create_dir(&unreadable).unwrap();
  let error = load_checkpoint(&unreadable).unwrap_err();
  // Windows rejects the directory at open; Unix opens it and fails on read.
  assert!(error.starts_with("open checkpoint ") || error.starts_with("read checkpoint "), "{error}");
  assert!(error.contains(unreadable.to_string_lossy().as_ref()), "{error}");

  let malformed = temporary.path().join("malformed.tsv");
  std::fs::write(&malformed, "+\t/docs/a.txt\nnot-an-operation\n").unwrap();
  let error = load_checkpoint(&malformed).unwrap_err();
  assert!(error.contains("malformed checkpoint") && error.contains("line 2"), "{error}");
}

#[test]
fn checkpoint_load_excludes_a_pending_delete_from_restart_work() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("pending-delete.tsv");
  std::fs::write(&checkpoint, "+\t/docs/a.txt\n?\t/docs/a.txt\n+\t/docs/b.txt\n!\t/docs/b.txt\n").unwrap();

  assert!(load_checkpoint(&checkpoint).unwrap().is_empty());
}

#[test]
fn checkpoint_load_accepts_completed_crlf_records() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("crlf.tsv");
  std::fs::write(&checkpoint, "+\t/docs/a.txt\r\n?\t/docs/a.txt\r\n+\t/docs/b.txt\r\n").unwrap();

  assert_eq!(load_checkpoint(&checkpoint).unwrap(), HashSet::from(["/docs/b.txt".to_string()]));
}

#[test]
fn checkpoint_load_truncates_a_one_byte_final_fragment() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("one-byte-tail.tsv");
  let complete_prefix = b"+\t/docs/a.txt\n";
  let mut checkpoint_bytes = complete_prefix.to_vec();
  checkpoint_bytes.push(b'+');
  std::fs::write(&checkpoint, checkpoint_bytes).unwrap();

  assert_eq!(load_checkpoint(&checkpoint).unwrap(), HashSet::from(["/docs/a.txt".to_string()]));
  assert_eq!(std::fs::read(&checkpoint).unwrap(), complete_prefix);
}

#[test]
fn checkpoint_load_truncates_only_a_nonterminated_final_record_before_append_resumes() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("incomplete-tail.tsv");
  let complete_prefix = b"+\t/docs/a.txt\n+\t/docs/b.txt\n?\t/docs/a.txt\n";
  let mut checkpoint_bytes = complete_prefix.to_vec();
  checkpoint_bytes.extend_from_slice(b"+\t/docs/incomplete.txt");
  std::fs::write(&checkpoint, checkpoint_bytes).unwrap();

  let committed = load_checkpoint(&checkpoint).unwrap();
  assert_eq!(committed, HashSet::from(["/docs/b.txt".to_string()]));
  assert_eq!(std::fs::read(&checkpoint).unwrap(), complete_prefix);

  let mut writer = OpenOptions::new().append(true).open(&checkpoint).unwrap();
  append_checkpoint(&mut writer, '+', "/docs/c.txt").unwrap();
  drop(writer);

  let committed = load_checkpoint(&checkpoint).unwrap();
  assert_eq!(committed, HashSet::from(["/docs/b.txt".to_string(), "/docs/c.txt".to_string()]));
  assert_eq!(std::fs::read(&checkpoint).unwrap(), b"+\t/docs/a.txt\n+\t/docs/b.txt\n?\t/docs/a.txt\n+\t/docs/c.txt\n");
}

#[test]
fn checkpoint_load_ignores_an_invalid_utf8_final_fragment_but_rejects_a_completed_one() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("invalid-utf8-tail.tsv");
  let complete_prefix = b"+\t/docs/a.txt\n";
  let mut incomplete = complete_prefix.to_vec();
  incomplete.extend_from_slice(b"+\t/docs/");
  incomplete.push(0xff);
  std::fs::write(&checkpoint, incomplete).unwrap();

  let committed = load_checkpoint(&checkpoint).unwrap();
  assert_eq!(committed, HashSet::from(["/docs/a.txt".to_string()]));
  assert_eq!(std::fs::read(&checkpoint).unwrap(), complete_prefix);

  let mut completed = complete_prefix.to_vec();
  completed.extend_from_slice(b"+\t/docs/");
  completed.push(0xff);
  completed.push(b'\n');
  std::fs::write(&checkpoint, &completed).unwrap();

  let error = load_checkpoint(&checkpoint).unwrap_err();
  assert!(error.contains("checkpoint") && error.contains("line 2") && error.contains("UTF-8"), "{error}");
  assert_eq!(std::fs::read(&checkpoint).unwrap(), completed);
}

#[test]
fn checkpoint_load_enforces_the_file_record_path_boundary() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("path-boundary.tsv");
  let maximum_path = format!("/{}", "a".repeat(u16::MAX as usize - 1));
  std::fs::write(&checkpoint, format!("+\t{maximum_path}\r\n")).unwrap();
  assert_eq!(load_checkpoint(&checkpoint).unwrap(), HashSet::from([maximum_path]));

  let excessive_path = format!("/{}", "b".repeat(u16::MAX as usize));
  let completed = format!("+\t{excessive_path}\n").into_bytes();
  std::fs::write(&checkpoint, &completed).unwrap();

  let error = load_checkpoint(&checkpoint).unwrap_err();
  assert!(error.contains("checkpoint") && error.contains("line 1") && error.contains("payload limit"), "{error}");
  assert_eq!(std::fs::read(&checkpoint).unwrap(), completed);
}

#[test]
fn database_size_failure_is_not_reported_as_zero() {
  let temporary = tempfile::tempdir().unwrap();
  let missing = temporary.path().join("missing.aeordb");
  let error = database_size_bytes(&missing).unwrap_err();
  assert!(error.contains("database metadata"), "{error}");
}

#[test]
fn poisoned_last_action_state_invalidates_diagnostic_evidence() {
  let action = Mutex::new("startup".to_string());
  let _ = std::panic::catch_unwind(|| {
    let _guard = action.lock().unwrap();
    panic!("inject last-action poison");
  });

  let write_error = set_last_action(&action, "write").unwrap_err();
  assert!(write_error.contains("last-action diagnostics lock is poisoned"), "{write_error}");
  let read_error = read_last_action(&action).unwrap_err();
  assert!(read_error.contains("last-action diagnostics lock is poisoned"), "{read_error}");
}

#[test]
fn workload_failure_preserves_both_engine_and_diagnostic_failures() {
  let action = Mutex::new("startup".to_string());
  let error = record_workload_failure(&action, "read /docs/a.txt failed: corrupt chunk".to_string());
  assert_eq!(error, "read /docs/a.txt failed: corrupt chunk");
  assert_eq!(read_last_action(&action).unwrap(), "FAIL read /docs/a.txt failed: corrupt chunk");

  let poisoned = Mutex::new("startup".to_string());
  let _ = std::panic::catch_unwind(|| {
    let _guard = poisoned.lock().unwrap();
    panic!("inject last-action poison");
  });
  let error = record_workload_failure(&poisoned, "GC failed: injected".to_string());
  assert!(error.contains("GC failed: injected"), "{error}");
  assert!(error.contains("last-action diagnostics lock is poisoned"), "{error}");
}

#[test]
fn summary_rejects_a_malformed_row_instead_of_skipping_it() {
  let temporary = tempfile::tempdir().unwrap();
  let metrics = temporary.path().join("metrics.tsv");
  let valid = "2026-01-01T00:00:00Z\t0\t1\t1\t0\t1024\t512\t2048\t1024\t8\t4096\t0\t0\t4\t0\t0\t0\twrite";
  std::fs::write(&metrics, format!("{METRICS_HEADER}\n{valid}\nmalformed\n")).unwrap();

  let error = summarize(metrics.to_str().unwrap()).unwrap_err();
  assert!(error.contains("malformed metrics row 3"), "{error}");
}
