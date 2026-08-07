use super::try_create_engine_with_hot_dir_progress_and_configuration_overrides;

#[test]
fn fallible_engine_constructor_reports_corrupt_database_without_unwinding() {
  let temporary_directory = tempfile::tempdir().expect("create temporary directory");
  let database = temporary_directory.path().join("invalid.aeordb");
  std::fs::write(&database, b"not an AeorDB database").expect("write invalid database");

  let outcome = std::panic::catch_unwind(|| {
    try_create_engine_with_hot_dir_progress_and_configuration_overrides(
      database.to_str().expect("database path is UTF-8"),
      Some(temporary_directory.path()),
      None,
      Default::default(),
    )
  });

  let result = outcome.expect("fallible constructor must not unwind");
  assert!(result.is_err(), "corrupt database must be reported as an error");
}
