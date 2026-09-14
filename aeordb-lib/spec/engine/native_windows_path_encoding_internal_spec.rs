//! Native Windows path normalization without filesystem access.
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::encode_native_windows_path;

fn decoded_units(path: &Path) -> Vec<u16> {
  let mut encoded = encode_native_windows_path(path).unwrap();
  assert_eq!(encoded.pop(), Some(0));
  assert!(!encoded.contains(&0));
  encoded
}

#[test]
fn windows_native_path_normalizes_drive_paths_before_prefixing() {
  for (input, expected) in [
    (r"C:\uncreated\child\..\file.bin", r"\\?\C:\uncreated\file.bin"),
    (r"C:/uncreated/./file.bin", r"\\?\C:\uncreated\file.bin"),
    (r"C:\uncreated\λ-file.bin", r"\\?\C:\uncreated\λ-file.bin"),
  ] {
    assert_eq!(decoded_units(Path::new(input)), expected.encode_utf16().collect::<Vec<_>>());
  }
}

#[test]
fn windows_native_path_normalizes_unc_without_accessing_the_server() {
  let input = Path::new(r"\\nonexistent-aeordb-test-host\share\child\..\file.bin");
  let expected = r"\\?\UNC\nonexistent-aeordb-test-host\share\file.bin";
  assert_eq!(decoded_units(input), expected.encode_utf16().collect::<Vec<_>>());
}

#[test]
fn windows_native_path_preserves_existing_verbatim_names_exactly() {
  for input in
    [r"\\?\C:\uncreated\child\..\file.bin", r"\\?\UNC\server\share\file.bin", r"\\?\C:\literal-name. ", r"\??\C:\uncreated\file.bin"]
  {
    assert_eq!(decoded_units(Path::new(input)), input.encode_utf16().collect::<Vec<_>>());
  }
}

#[test]
fn windows_native_path_resolves_relative_nonexistent_names_without_changing_current_directory() {
  let current_directory = std::env::current_dir().unwrap();
  let input = Path::new(r"uncreated-aeordb-path-test\..\uncreated-aeordb-file.bin");
  let absolute = std::path::absolute(input).unwrap();
  assert_eq!(decoded_units(input), decoded_units(&absolute));
  assert_eq!(std::env::current_dir().unwrap(), current_directory);
  assert!(!absolute.exists());
}

#[test]
fn windows_native_path_preserves_native_unpaired_utf16_units() {
  for prefix in [r"C:\uncreated\", r"\\?\C:\uncreated\"] {
    let mut units: Vec<u16> = prefix.encode_utf16().collect();
    units.push(0xd800);
    units.extend(".bin".encode_utf16());
    let path = PathBuf::from(OsString::from_wide(&units));
    let mut expected = Vec::new();
    if !prefix.starts_with(r"\\?\") {
      expected.extend(r"\\?\".encode_utf16());
    }
    expected.extend_from_slice(&units);
    assert_eq!(decoded_units(&path), expected);
  }
}

#[test]
fn windows_native_path_does_not_reinterpret_device_or_volume_names_as_unc() {
  for input in [r"\\.\pipe\aeordb-unopened-test-name", r"\\?\Volume{00000000-0000-0000-0000-000000000000}\file.bin"] {
    assert_eq!(decoded_units(Path::new(input)), input.encode_utf16().collect::<Vec<_>>());
  }
}

#[test]
fn windows_native_path_rejects_empty_and_embedded_nul_inputs() {
  for input in ["", "C:\\file.bin\0ignored", "\\\\?\\C:\\file.bin\0ignored"] {
    let error = encode_native_windows_path(Path::new(input)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
  }
}

#[test]
fn windows_native_path_measures_long_names_in_utf16_not_utf8() {
  let path = PathBuf::from(format!(r"C:\uncreated\{}\file.bin", "λ".repeat(180)));
  let expected: Vec<u16> = format!(r"\\?\{}", path.display()).encode_utf16().collect();
  assert_eq!(decoded_units(&path), expected);
  assert!(path.as_os_str().encode_wide().count() < path.to_str().unwrap().len());
}

#[test]
fn windows_native_path_enforces_the_native_limit_before_and_after_prefix_expansion() {
  let prefix = r"\\?\C:\";
  let accepted = format!("{prefix}{}", "a".repeat(32_766 - prefix.len()));
  assert_eq!(encode_native_windows_path(Path::new(&accepted)).unwrap().len(), 32_767);
  let too_long = format!("{accepted}a");
  assert_eq!(encode_native_windows_path(Path::new(&too_long)).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
  let needs_prefix = format!(r"C:\{}", "a".repeat(32_763));
  assert_eq!(encode_native_windows_path(Path::new(&needs_prefix)).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
}
