use std::path::Path;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::native_durability::{durable_replace_native, sync_directory_native};

/// Sync the directory entry that contains `path`.
///
/// On Unix, fsyncing a file does not guarantee that a newly-created or renamed
/// directory entry survives a crash; the containing directory must be synced too.
pub fn sync_parent_dir(path: impl AsRef<Path>) -> EngineResult<()> {
  let path = path.as_ref();
  let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
  sync_directory_native(parent).map_err(|error| EngineError::DurabilityFailure(error.to_string()))
}

/// Atomically publish `from` at `to`, then sync the parent directory for crash
/// durability of the namespace update.
pub fn rename_durable(from: impl AsRef<Path>, to: impl AsRef<Path>) -> EngineResult<()> {
  durable_replace_native(from, to).map_err(|error| EngineError::DurabilityFailure(error.to_string()))
}
