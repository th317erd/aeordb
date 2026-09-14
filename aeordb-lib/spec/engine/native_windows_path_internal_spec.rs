//! Native Windows path-boundary and mutation-refusal regressions.
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use super::*;
use crate::engine::v4::private_workspace::{
  create_private_directory_synced, create_private_regular_file, secure_platform_private_directory, secure_platform_private_regular_file,
  validate_private_directory_readonly, validate_private_regular_file, PrivateWorkspaceErrorV1,
};

fn extended_fixture_parent(temporary: &tempfile::TempDir) -> PathBuf {
  let text = temporary.path().to_str().unwrap();
  let mut path = PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text));
  assert!(!path.as_os_str().encode_wide().collect::<Vec<_>>().starts_with(&[92, 92, 63, 92]));
  for index in 0..4 {
    path.push(format!("native-path-{index}-{}-λ", "a".repeat(80)));
  }
  assert!(path.as_os_str().encode_wide().count() > 320);
  std::fs::create_dir_all(&path).unwrap();
  path
}

#[test]
fn windows_private_directory_creation_supports_non_verbatim_long_paths() {
  let temporary = tempfile::tempdir().unwrap();
  let parent = extended_fixture_parent(&temporary);
  let directory = parent.join("private-directory");
  create_private_directory_synced(&directory, &parent).unwrap();
  validate_private_directory_readonly(&directory, "long directory").unwrap();
  assert!(create_private_directory_synced(&directory, &parent).is_err());
  assert!(directory.is_dir());
}

#[test]
fn windows_existing_private_directory_permissions_support_non_verbatim_long_paths() {
  let temporary = tempfile::tempdir().unwrap();
  let parent = extended_fixture_parent(&temporary);
  secure_platform_private_directory(&parent).unwrap();
  validate_private_directory_readonly(&parent, "long secured directory").unwrap();
}

#[test]
fn windows_private_regular_file_creation_and_revalidation_support_long_paths() {
  let temporary = tempfile::tempdir().unwrap();
  let parent = extended_fixture_parent(&temporary);
  let path = parent.join("private-file-λ.bin");
  let mut file = create_private_regular_file(&path, "long private file").unwrap();
  file.write_all(b"private payload").unwrap();
  file.sync_all().unwrap();
  validate_private_regular_file(&path, &file, "long private file").unwrap();
  secure_platform_private_regular_file(&path).unwrap();
  assert!(create_private_regular_file(&path, "existing file").is_err());
  assert_eq!(std::fs::read(&path).unwrap(), b"private payload");
}

#[test]
fn windows_durable_replace_existing_file_supports_long_paths_and_keeps_source_identity() {
  let temporary = tempfile::tempdir().unwrap();
  let parent = extended_fixture_parent(&temporary);
  let source = parent.join("replacement-λ.bin");
  let destination = parent.join("selected-λ.bin");
  std::fs::write(&source, b"new payload").unwrap();
  std::fs::write(&destination, b"old payload").unwrap();
  let identity = platform_file_identity(&source).unwrap();
  let old_identity = platform_file_identity(&destination).unwrap();
  durable_replace_native(&source, &destination).unwrap();
  assert_eq!(std::fs::read(&destination).unwrap(), b"new payload");
  assert!(!source.exists());
  let selected = platform_file_identity(&destination).unwrap();
  assert!(identity.represents_same_physical_file_as(selected));
  assert!(!old_identity.represents_same_physical_file_as(selected));
}

#[test]
fn windows_durable_move_to_absent_file_supports_long_paths_across_parents() {
  let temporary = tempfile::tempdir().unwrap();
  let parent = extended_fixture_parent(&temporary);
  let other_parent = parent.join("other-parent");
  std::fs::create_dir(&other_parent).unwrap();
  let source = parent.join("source.bin");
  let destination = other_parent.join("selected.bin");
  std::fs::write(&source, b"new payload").unwrap();
  let identity = platform_file_identity(&source).unwrap();
  durable_replace_native(&source, &destination).unwrap();
  assert_eq!(std::fs::read(&destination).unwrap(), b"new payload");
  assert!(!source.exists());
  assert!(identity.represents_same_physical_file_as(platform_file_identity(&destination).unwrap()));
}

fn append_embedded_nul(path: &Path) -> PathBuf {
  let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
  units.push(0);
  units.extend("not-a-path-suffix".encode_utf16());
  OsString::from_wide(&units).into()
}

#[test]
fn windows_private_file_permissions_reject_empty_and_nul_names_before_operating() {
  let temporary = tempfile::tempdir().unwrap();
  let file_path = temporary.path().join("unchanged.bin");
  std::fs::write(&file_path, b"unchanged payload").unwrap();
  for path in [PathBuf::new(), append_embedded_nul(&file_path)] {
    assert!(matches!(secure_platform_private_regular_file(&path), Err(PrivateWorkspaceErrorV1::Path(_))));
    assert_eq!(std::fs::read(&file_path).unwrap(), b"unchanged payload");
  }
}

#[test]
fn windows_durable_replace_rejects_nul_source_without_mutating_either_real_path() {
  let temporary = tempfile::tempdir().unwrap();
  let source = temporary.path().join("source.bin");
  let destination = temporary.path().join("selected.bin");
  std::fs::write(&source, b"new payload").unwrap();
  std::fs::write(&destination, b"old payload").unwrap();
  let result = durable_replace_native(append_embedded_nul(&source), &destination);
  assert_eq!(std::fs::read(&source).unwrap(), b"new payload");
  assert_eq!(std::fs::read(&destination).unwrap(), b"old payload");
  assert_eq!(result.unwrap_err().class(), NativeDurabilityErrorClass::InvalidInput);
}

#[test]
fn windows_durable_replace_rejects_nul_destination_without_mutating_either_real_path() {
  let temporary = tempfile::tempdir().unwrap();
  let source = temporary.path().join("source.bin");
  let destination = temporary.path().join("selected.bin");
  std::fs::write(&source, b"new payload").unwrap();
  std::fs::write(&destination, b"old payload").unwrap();
  let result = durable_replace_native(&source, append_embedded_nul(&destination));
  assert_eq!(std::fs::read(&source).unwrap(), b"new payload");
  assert_eq!(std::fs::read(&destination).unwrap(), b"old payload");
  assert_eq!(result.unwrap_err().class(), NativeDurabilityErrorClass::InvalidInput);
}

#[test]
fn windows_long_path_replace_failures_preserve_existing_files() {
  let temporary = tempfile::tempdir().unwrap();
  let parent = extended_fixture_parent(&temporary);
  let source = parent.join("source.bin");
  let destination = parent.join("selected.bin");
  std::fs::write(&source, b"new payload").unwrap();
  std::fs::write(&destination, b"old payload").unwrap();
  assert!(durable_replace_native(parent.join("missing.bin"), &destination).is_err());
  assert_eq!(std::fs::read(&destination).unwrap(), b"old payload");
  assert!(durable_replace_native(&source, parent.join("absent-parent/selected.bin")).is_err());
  assert_eq!(std::fs::read(&source).unwrap(), b"new payload");
  assert!(durable_replace_native(&source, &parent).is_err());
  assert_eq!(std::fs::read(&source).unwrap(), b"new payload");
  assert_eq!(std::fs::read(&destination).unwrap(), b"old payload");
}
