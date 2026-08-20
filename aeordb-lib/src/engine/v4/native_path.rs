//! Lexical validation for native paths persisted by cross-platform formats.
//!
//! These bytes describe the native path on the writer's host. Validation must
//! recognize canonical POSIX and Windows drive forms regardless of the current
//! build host. Actual I/O still uses host-specific `Path` validation.

pub(super) fn canonical_persisted_native_path(path: &str) -> bool {
  if path.is_empty() || path.contains('\\') || path.as_bytes().contains(&0) || path.len() > 1 && path.ends_with('/') {
    return false;
  }
  let remainder = if let Some(rest) = path.strip_prefix('/') {
    rest
  } else if path.len() >= 3 && path.as_bytes()[0].is_ascii_uppercase() && path.as_bytes()[1] == b':' && path.as_bytes()[2] == b'/' {
    &path[3..]
  } else {
    return false;
  };
  !remainder.is_empty() && remainder.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests {
  use super::canonical_persisted_native_path;

  #[test]
  fn accepts_host_independent_canonical_native_paths() {
    assert!(canonical_persisted_native_path("/var/lib/aeordb/workspace"));
    assert!(canonical_persisted_native_path("C:/Users/wyatt/AppData/Local/AeorDB/workspace"));
    for path in ["", "/", "C:/", "relative/path", "c:/path", "C:\\path", "/a//b", "/a/./b", "/a/../b", "/a/b/"] {
      assert!(!canonical_persisted_native_path(path), "accepted {path:?}");
    }
  }
}
