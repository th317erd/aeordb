use super::enforce_backup_retention;

#[cfg(unix)]
#[test]
fn retention_reports_old_backup_removal_failures() {
  use std::os::unix::fs::PermissionsExt;

  let directory = tempfile::tempdir().unwrap();
  let oldest = directory.path().join("backup-oldest.aeordb");
  let newest = directory.path().join("backup-newest.aeordb");
  std::fs::write(&oldest, b"oldest").unwrap();
  std::thread::sleep(std::time::Duration::from_millis(10));
  std::fs::write(&newest, b"newest").unwrap();

  std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
  let result = enforce_backup_retention(directory.path().to_str().unwrap(), 1);
  std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

  let error = result.expect_err("configured retention must report a failed removal");
  assert!(error.contains("backup-oldest.aeordb"), "the failed artifact must be identified: {error}");
  assert!(oldest.exists(), "a failed removal must leave the old backup intact");
  assert!(newest.exists(), "retention must not remove the newest retained backup");
}
