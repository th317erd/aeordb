use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, TryLockError};
use std::time::Duration;

use crate::engine::config_resolver::{
  ConfigResolutionContext, ConfigResolutionInputs, ConfigResolver, ConfigShadowReport, ConfigValue, ConfigurationFamily,
  StartupConfigurationState,
};
use crate::engine::v4::configuration_controls::{ConfigurationControlCapability, ConfigurationControlFamilyStatus};

use super::{ConfigurationAuthority, ConfigurationConvergenceResult};

#[cfg(unix)]
fn native_absolute_paths() -> (PathBuf, PathBuf, PathBuf) {
  ("/test.aeordb".into(), "/gc".into(), "/spill".into())
}

#[cfg(windows)]
fn native_absolute_paths() -> (PathBuf, PathBuf, PathBuf) {
  (r"C:\test.aeordb".into(), r"C:\gc".into(), r"C:\spill".into())
}

fn status() -> ConfigurationControlFamilyStatus {
  ConfigurationControlFamilyStatus {
    capability: ConfigurationControlCapability::UnavailableNoDatabaseIdentity,
    database_id: None,
    lkg_sequence: None,
    lkg_activated_at_ms: None,
    diagnostics_sequence: None,
    redundancy_degraded: false,
    errors: Vec::new(),
  }
}

fn authority() -> Arc<ConfigurationAuthority> {
  let (database_path, default_gc_workspace_root, default_emergency_spill_dir) = native_absolute_paths();
  let context = ConfigResolutionContext {
    physical_memory_bytes: 16 * 1024 * 1024 * 1024,
    logical_cpu_count: 8,
    filesystem_capacity_bytes: 4 * 1024 * 1024 * 1024 * 1024,
    chunk_size_bytes: 256 * 1024,
    database_path,
    default_gc_workspace_root: Some(default_gc_workspace_root),
    default_emergency_spill_dir: Some(default_emergency_spill_dir),
  };
  let inputs = ConfigResolutionInputs::default();
  let resolution = ConfigResolver::new(context.clone()).resolve(inputs.clone());
  assert!(resolution.complete(), "test authority must start from a complete policy: {:?}", resolution.issues);
  let report = ConfigShadowReport { context: Some(context), resolution: Some(resolution), context_error: None };
  let statuses = ConfigurationFamily::ALL.into_iter().map(|family| (family, status())).collect::<BTreeMap<_, _>>();
  Arc::new(ConfigurationAuthority::new(StartupConfigurationState { report, inputs }, statuses))
}

#[test]
fn pending_snapshot_precedes_owner_activation_and_serializes_replacements() {
  let authority = authority();
  let worker_authority = Arc::clone(&authority);
  let (entered_tx, entered_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let worker = std::thread::spawn(move || {
    worker_authority
      .replace_document(
        ConfigurationFamily::Runtime,
        br#"{"schema_version":1,"index":{"flush_after_seconds":45}}"#,
        |_bytes, _schema, _prospective| Ok(status()),
        move |_prospective, changed| {
          entered_tx.send(()).unwrap();
          release_rx.recv_timeout(Duration::from_secs(2)).expect("test must release owner convergence");
          let mut result = ConfigurationConvergenceResult::default();
          result.activate(changed.iter().cloned());
          result
        },
      )
      .unwrap()
  });

  entered_rx.recv_timeout(Duration::from_secs(2)).expect("owner convergence must start");
  let pending = authority.snapshot();
  assert_eq!(pending.generation, 2);
  assert_eq!(pending.resolved_unsigned("index.flush_after_seconds"), Some(30));
  assert_eq!(
    pending.desired.resolution.as_ref().unwrap().property("index.flush_after_seconds").unwrap().value,
    Some(ConfigValue::Unsigned(45))
  );
  assert!(pending.pending_convergence.contains("index.flush_after_seconds"));
  assert!(matches!(authority.inputs.try_lock(), Err(TryLockError::WouldBlock)), "the update lock must cover owner convergence");

  release_tx.send(()).unwrap();
  let converged = worker.join().unwrap();
  assert_eq!(converged.resolved_unsigned("index.flush_after_seconds"), Some(45));
  assert!(converged.pending_convergence.is_empty());
  assert!(converged.convergence_errors.is_empty());
}

#[test]
fn failed_owner_keeps_old_active_value_visible_and_same_desired_document_can_retry() {
  let authority = authority();
  let document = br#"{"schema_version":1,"index":{"flush_after_seconds":45}}"#;
  let failed = authority
    .replace_document(
      ConfigurationFamily::Runtime,
      document,
      |_bytes, _schema, _prospective| Ok(status()),
      |_prospective, changed| {
        let mut result = ConfigurationConvergenceResult::default();
        result.fail(changed.iter().cloned(), "deterministic owner failure");
        result
      },
    )
    .unwrap();

  assert_eq!(failed.resolved_unsigned("index.flush_after_seconds"), Some(30));
  assert_eq!(
    failed.desired.resolution.as_ref().unwrap().property("index.flush_after_seconds").unwrap().value,
    Some(ConfigValue::Unsigned(45))
  );
  assert!(failed.pending_convergence.contains("index.flush_after_seconds"));
  assert_eq!(failed.convergence_errors["index.flush_after_seconds"], "deterministic owner failure");

  let retried = authority
    .replace_document(
      ConfigurationFamily::Runtime,
      document,
      |_bytes, _schema, _prospective| Ok(status()),
      |_prospective, changed| {
        let mut result = ConfigurationConvergenceResult::default();
        result.activate(changed.iter().cloned());
        result
      },
    )
    .unwrap();
  assert_eq!(retried.generation, 3);
  assert_eq!(retried.resolved_unsigned("index.flush_after_seconds"), Some(45));
  assert!(retried.pending_convergence.is_empty());
  assert!(retried.convergence_errors.is_empty());
}
