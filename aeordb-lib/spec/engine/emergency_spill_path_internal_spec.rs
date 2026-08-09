use std::io;
use std::path::PathBuf;

use super::*;

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
  let path = PathBuf::from("/var/lib/aeordb/example.aeordb");
  let resolved =
    absolute_path_with_current_dir(&path, || Err(io::Error::new(io::ErrorKind::NotFound, "must not be called for absolute paths")))
      .unwrap();

  assert_eq!(resolved, path);
}
