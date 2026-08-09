use super::*;

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
