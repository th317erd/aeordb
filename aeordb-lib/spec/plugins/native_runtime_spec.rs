use std::path::{Path, PathBuf};
use std::process::Command;

use aeordb::plugins::native_runtime::{NativePluginRuntime, NativeRuntimeError};

/// Compiles the test fixture plugin into a cdylib `.so` inside `output_dir`.
/// Returns the path to the compiled shared library.
fn compile_test_plugin(output_dir: &Path) -> PathBuf {
  let fixture_source = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("spec")
    .join("fixtures")
    .join("test_native_plugin.rs");

  let library_path = output_dir.join("libtest_native_plugin.so");

  let compile_result = Command::new("rustc")
    .arg("--crate-type")
    .arg("cdylib")
    .arg("--edition")
    .arg("2021")
    .arg("-o")
    .arg(&library_path)
    .arg(&fixture_source)
    .output()
    .expect("failed to invoke rustc");

  assert!(
    compile_result.status.success(),
    "rustc failed to compile test plugin: {}",
    String::from_utf8_lossy(&compile_result.stderr)
  );

  assert!(
    library_path.exists(),
    "compiled library not found at: {}",
    library_path.display()
  );

  library_path
}

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
fn test_load_real_native_plugin() {
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let library_path = compile_test_plugin(temp_dir.path());

  let runtime = NativePluginRuntime::load(&library_path);
  assert!(
    runtime.is_ok(),
    "should successfully load a valid native plugin, got: {:?}",
    runtime.err()
  );
}

#[test]
fn test_call_native_plugin() {
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let library_path = compile_test_plugin(temp_dir.path());
  let runtime = NativePluginRuntime::load(&library_path).expect("load plugin");

  let request_bytes = b"hello aeordb";
  let response = runtime
    .call_handle(request_bytes)
    .expect("call_handle should succeed");

  assert_eq!(
    response.as_slice(),
    request_bytes,
    "echo plugin should return the same bytes as input"
  );
}

#[test]
fn test_call_native_plugin_empty_input() {
  let temp_dir = tempfile::tempdir().expect("create temp dir");
  let library_path = compile_test_plugin(temp_dir.path());
  let runtime = NativePluginRuntime::load(&library_path).expect("load plugin");

  let response = runtime
    .call_handle(b"")
    .expect("call_handle with empty input should succeed");

  assert_eq!(
    response.as_slice(),
    b"empty",
    "echo plugin should return 'empty' for zero-length input"
  );
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
