use std::collections::HashSet;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;

/// Maximum number of symlink hops before we bail out.
pub const MAX_SYMLINK_DEPTH: usize = 32;

/// The resolved target of a symlink chain.
#[derive(Debug)]
pub enum ResolvedTarget {
    /// The target resolved to a file.
    File(FileRecord),
    /// The target resolved to a directory (the resolved path is returned).
    Directory(String),
}

/// Resolve a symlink by following the chain until we reach a file or directory.
///
/// Uses a visited set for cycle detection and MAX_SYMLINK_DEPTH as a safety valve.
/// Returns an error if:
/// - A cycle is detected (CyclicSymlink)
/// - The chain exceeds MAX_SYMLINK_DEPTH (SymlinkDepthExceeded)
/// - The final target does not exist (NotFound -- "dangling symlink")
///
/// This function resolves ANY path -- not just symlinks. If the path is already
/// a regular file, it returns File directly. This makes it safe to call on any path.
pub fn resolve_symlink(
    engine: &StorageEngine,
    path: &str,
) -> EngineResult<ResolvedTarget> {
    let ops = DirectoryOps::new(engine);
    let mut visited: HashSet<String> = HashSet::new();
    let mut current_path = normalize_path(path);
    let mut depth: usize = 0;
    let mut chain: Vec<String> = Vec::new();

    loop {
        // Cycle detection
        if visited.contains(&current_path) {
            chain.push(current_path.clone());
            let chain_display = chain.join(" -> ");
            return Err(EngineError::CyclicSymlink(chain_display));
        }

        // Max depth check
        if depth >= MAX_SYMLINK_DEPTH {
            return Err(EngineError::SymlinkDepthExceeded(format!(
                "Exceeded maximum symlink depth of {} following '{}'",
                MAX_SYMLINK_DEPTH, path
            )));
        }

        visited.insert(current_path.clone());
        chain.push(current_path.clone());

        // Check if it's a symlink
        if let Some(record) = ops.get_symlink(&current_path)? {
            current_path = normalize_path(&record.target);
            depth += 1;
            continue;
        }

        // Check if it's a file
        if let Some(file_record) = ops.get_metadata(&current_path)? {
            return Ok(ResolvedTarget::File(file_record));
        }

        // Check if it's a directory
        match ops.list_directory(&current_path) {
            Ok(_) => return Ok(ResolvedTarget::Directory(current_path)),
            Err(EngineError::NotFound(_)) => {}
            Err(other) => return Err(other),
        }

        // Nothing found -- dangling symlink
        Err(EngineError::NotFound(format!(
            "Dangling symlink: target '{}' does not exist",
            current_path
        )))?
    }
}
