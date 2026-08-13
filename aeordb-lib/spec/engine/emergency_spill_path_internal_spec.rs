use std::io;
use std::path::PathBuf;

use super::*;

#[cfg(unix)]
fn native_absolute_database_path() -> PathBuf {
  "/var/lib/aeordb/example.aeordb".into()
}

#[cfg(windows)]
fn native_absolute_database_path() -> PathBuf {
  r"C:\var\lib\aeordb\example.aeordb".into()
}

#[test]
fn relative_spill_path_resolution_propagates_current_directory_failure() {
  let error = absolute_path_with_current_dir(Path::new("relative.aeordb"), || {
    Err(io::Error::new(io::ErrorKind::NotFound, "working directory was removed"))
  })
  .expect_err("unresolved relative spill paths must not fall back to dot");

  assert!(matches!(error, EngineError::IoError(_)));
  assert!(error.to_string().contains("working directory was removed"));
}

#[test]
fn absolute_spill_path_resolution_does_not_require_current_directory() {
  let path = native_absolute_database_path();
  let resolved =
    absolute_path_with_current_dir(&path, || Err(io::Error::new(io::ErrorKind::NotFound, "must not be called for absolute paths")))
      .unwrap();

  assert_eq!(resolved, path);
}

#[test]
fn spill_path_resolution_propagates_canonicalization_permission_failures() {
  let path = PathBuf::from("/restricted/database.aeordb");
  let error = absolute_path_with_resolvers(
    &path,
    || Ok(PathBuf::from("/unused")),
    |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "canonicalization denied")),
  )
  .unwrap_err();

  assert!(matches!(error, EngineError::IoError(ref source) if source.kind() == io::ErrorKind::PermissionDenied));
}

#[test]
fn spill_path_resolution_falls_back_only_when_the_path_does_not_exist() {
  let path = PathBuf::from("relative/database.aeordb");
  let resolved = absolute_path_with_resolvers(
    &path,
    || Ok(PathBuf::from("/var/lib/aeordb")),
    |_| Err(io::Error::new(io::ErrorKind::NotFound, "not created yet")),
  )
  .unwrap();

  assert_eq!(resolved, PathBuf::from("/var/lib/aeordb/relative/database.aeordb"));
}
