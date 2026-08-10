use aeordb::auth::AuthMode;
use aeordb::engine::StorageEngine;
use aeordb::engine::config_resolver::CommandLineConfigOverrides;
use aeordb::metrics::try_initialize_metrics;
use aeordb::server::try_create_app_with_auth_mode_cancel_progress_and_configuration_overrides;

#[test]
fn foreign_global_recorder_conflict_is_a_stable_initialization_error() {
  metrics::set_global_recorder(metrics::NoopRecorder).unwrap();

  let first = match try_initialize_metrics() {
    Ok(_) => panic!("AeorDB must not claim ownership of a foreign metrics recorder"),
    Err(error) => error,
  };
  let second = match try_initialize_metrics() {
    Ok(_) => panic!("a cached metrics conflict must remain an error"),
    Err(error) => error,
  };

  assert!(first.contains("metrics recorder"), "{first}");
  assert_eq!(second, first);

  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("foreign-recorder.aeordb");
  let error = match try_create_app_with_auth_mode_cancel_progress_and_configuration_overrides(
    database.to_str().unwrap(),
    &AuthMode::Disabled,
    None,
    None,
    None,
    None,
    CommandLineConfigOverrides::default(),
  ) {
    Ok(_) => panic!("fallible server startup must not hide a metrics-recorder conflict"),
    Err(error) => error,
  };
  assert!(error.contains("failed to initialize server metrics"), "{error}");

  let engine = StorageEngine::open(database.to_str().unwrap()).unwrap();
  engine.shutdown().unwrap();
}
