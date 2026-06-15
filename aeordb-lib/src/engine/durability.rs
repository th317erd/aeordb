use std::path::Path;

use crate::engine::errors::{EngineError, EngineResult};

/// Sync the directory entry that contains `path`.
///
/// On Unix, fsyncing a file does not guarantee that a newly-created or renamed
/// directory entry survives a crash; the containing directory must be synced too.
pub fn sync_parent_dir(path: impl AsRef<Path>) -> EngineResult<()> {
  let path = path.as_ref();
  let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
  sync_dir(parent)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> EngineResult<()> {
  let dir = std::fs::File::open(path)?;
  dir.sync_all()?;
  Ok(())
}

#[cfg(windows)]
fn sync_dir(_path: &Path) -> EngineResult<()> {
  // Opening directories for FlushFileBuffers on Windows requires platform
  // handles with FILE_FLAG_BACKUP_SEMANTICS. Keep this helper as the single
  // place to add that implementation without changing call sites.
  Ok(())
}

/// Atomically publish `from` at `to`, then sync the parent directory for crash
/// durability of the namespace update.
pub fn rename_durable(from: impl AsRef<Path>, to: impl AsRef<Path>) -> EngineResult<()> {
  let to = to.as_ref();
  std::fs::rename(from.as_ref(), to).map_err(EngineError::from)?;
  sync_parent_dir(to)
}
