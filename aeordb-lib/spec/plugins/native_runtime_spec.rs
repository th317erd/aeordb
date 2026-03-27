use std::path::Path;

use aeordb::plugins::native_runtime::{NativePluginRuntime, NativeRuntimeError};

#[test]
fn test_load_nonexistent_library_returns_error() {
  let path = Path::new("/tmp/aeordb_test_nonexistent_library_12345.so");
  let result = NativePluginRuntime::load(path);

  assert!(result.is_err(), "should fail for nonexistent library");
  match result.unwrap_err() {
    NativeRuntimeError::LoadFailed(message) => {
      assert!(
        message.contains("not found"),
        "error should mention 'not found', got: {}",
        message
      );
    }
    other => panic!("expected LoadFailed, got: {:?}", other),
  }
}

#[test]
fn test_invalid_library_returns_error() {
  // Create a temporary file that is NOT a valid shared library.
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let fake_library_path = temp_dir.path().join("fake.so");
  std::fs::write(&fake_library_path, b"this is not a shared library").expect("write fake file");

  let result = NativePluginRuntime::load(&fake_library_path);
  assert!(
    result.is_err(),
    "should fail for a file that is not a valid shared library"
  );

  match result.unwrap_err() {
    NativeRuntimeError::LoadFailed(_) => {}
    other => panic!("expected LoadFailed, got: {:?}", other),
  }
}

#[test]
fn test_load_empty_file_returns_error() {
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let empty_path = temp_dir.path().join("empty.so");
  std::fs::write(&empty_path, b"").expect("write empty file");

  let result = NativePluginRuntime::load(&empty_path);
  assert!(result.is_err(), "should fail for empty file");
}

#[test]
fn test_load_directory_path_returns_error() {
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let result = NativePluginRuntime::load(temp_dir.path());
  assert!(
    result.is_err(),
    "should fail when given a directory instead of a file"
  );
}
