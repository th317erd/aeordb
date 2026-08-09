use super::*;
use std::io;

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
  append_checkpoint(&mut writer, '+', "/docs/a.txt").unwrap();
  assert_eq!(writer.bytes, b"+\t/docs/a.txt\n");

  let mut write_failure = FailingWriter { fail_write: true, fail_flush: false, bytes: Vec::new() };
  let error = append_checkpoint(&mut write_failure, '+', "/docs/a.txt").unwrap_err();
  assert!(error.contains("checkpoint write failed") && error.contains("/docs/a.txt"));

  let mut flush_failure = FailingWriter { fail_write: false, fail_flush: true, bytes: Vec::new() };
  let error = append_checkpoint(&mut flush_failure, '-', "/docs/a.txt").unwrap_err();
  assert!(error.contains("checkpoint flush failed") && error.contains("/docs/a.txt"));
}
