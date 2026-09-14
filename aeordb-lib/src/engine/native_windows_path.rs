//! Native Windows API path arguments; never persisted path identities.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, Prefix};

// The Windows native path ceiling includes the terminating UTF-16 NUL. An API
// may impose a smaller limit after expanding a prefix or a filesystem component.
const MAXIMUM_NATIVE_PATH_UNITS: usize = 32_767;
const VERBATIM_PREFIX: &[u16] = &[92, 92, 63, 92];
const VERBATIM_UNC_PREFIX: &[u16] = &[92, 92, 63, 92, 85, 78, 67, 92];

/// Normalize ordinary paths before adding an extended-length prefix. This does
/// not access the filesystem, resolve symlinks, or require an existing target.
/// Existing verbatim/device names keep their native interpretation. Callers
/// retain responsibility for path admission, no-follow checks and durability.
pub(crate) fn encode_native_windows_path(path: &Path) -> io::Result<Vec<u16>> {
  let mut input_length = 0;
  for unit in path.as_os_str().encode_wide() {
    if unit == 0 {
      return Err(io::Error::new(io::ErrorKind::InvalidInput, "Windows path contains NUL"));
    }
    input_length += 1;
    if input_length >= MAXIMUM_NATIVE_PATH_UNITS {
      return Err(io::Error::new(io::ErrorKind::InvalidInput, "Windows path exceeds the native UTF-16 limit"));
    }
  }
  if input_length == 0 {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, "Windows path is empty"));
  }

  // The NT spelling is already absolute but is not a std::path verbatim prefix.
  let absolute = if path.as_os_str().as_encoded_bytes().starts_with(br"\??\") { path.to_path_buf() } else { std::path::absolute(path)? };
  let (prefix, skipped_units) = match absolute.components().next() {
    Some(Component::Prefix(component)) => match component.kind() {
      Prefix::Disk(_) => (VERBATIM_PREFIX, 0),
      Prefix::UNC(_, _) => (VERBATIM_UNC_PREFIX, 2),
      _ => (&[][..], 0),
    },
    _ => (&[][..], 0),
  };
  let output_length = absolute
    .as_os_str()
    .encode_wide()
    .count()
    .checked_sub(skipped_units)
    .and_then(|length| length.checked_add(prefix.len() + 1))
    .filter(|length| *length <= MAXIMUM_NATIVE_PATH_UNITS)
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "normalized Windows path exceeds the native UTF-16 limit"))?;
  let mut encoded = Vec::with_capacity(output_length);
  encoded.extend_from_slice(prefix);
  encoded.extend(absolute.as_os_str().encode_wide().skip(skipped_units));
  encoded.push(0);
  Ok(encoded)
}

#[cfg(test)]
#[path = "../../spec/engine/native_windows_path_encoding_internal_spec.rs"]
mod native_windows_path_encoding_internal_spec;
