use std::io::ErrorKind;

use super::remove_failed_artifact;

#[test]
fn failed_artifact_cleanup_accepts_missing_files_and_removes_existing_files() {
  let temporary = tempfile::tempdir().unwrap();
  let artifact = temporary.path().join("partial-output");

  remove_failed_artifact(&artifact).unwrap();
  std::fs::write(&artifact, b"partial").unwrap();
  remove_failed_artifact(&artifact).unwrap();

  assert!(!artifact.exists());
}

#[test]
fn failed_artifact_cleanup_preserves_real_filesystem_errors() {
  let temporary = tempfile::tempdir().unwrap();

  let error = remove_failed_artifact(temporary.path()).unwrap_err();

  assert_ne!(error.kind(), ErrorKind::NotFound);
}
