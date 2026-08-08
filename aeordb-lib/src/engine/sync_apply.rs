use std::collections::HashSet;

use crate::engine::directory_ops::{DirectoryOps, SyncImmutableVersion};
use crate::engine::errors::EngineResult;
use crate::engine::merge::{ConflictEntry, MergeOp};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::SystemFamilyPolicyResolver;
use crate::engine::tree_walker::TreeDiff;
use crate::engine::v4::system_family::SystemFamilyTransferOperationV1;

/// Apply merge operations to the local engine.
///
/// The complete operation set is preflighted and published through one
/// `DirectoryOps` namespace receipt. Missing delete targets are idempotent;
/// malformed input, corruption, missing chunks, and every other delete failure
/// abort before HEAD, locators, counters, indexes, or events are published.
pub fn apply_merge_operations(engine: &StorageEngine, context: &RequestContext, operations: &[MergeOp]) -> EngineResult<()> {
  validate_peer_transfer_paths(engine, operations)?;
  DirectoryOps::new(engine).apply_sync_merge(context, operations)
}

pub(crate) fn apply_merge_operations_with_conflicts(
  engine: &StorageEngine,
  context: &RequestContext,
  operations: &[MergeOp],
  conflicts: &[ConflictEntry],
  remote_diff: &TreeDiff,
) -> EngineResult<usize> {
  validate_peer_transfer_paths(engine, operations)?;
  let immutable_versions = remote_conflict_versions(engine, operations, conflicts, remote_diff)?;
  DirectoryOps::new(engine).apply_sync_receipt(context, operations, conflicts, &immutable_versions)
}

fn remote_conflict_versions(
  engine: &StorageEngine,
  operations: &[MergeOp],
  conflicts: &[ConflictEntry],
  remote_diff: &TreeDiff,
) -> EngineResult<Vec<SyncImmutableVersion>> {
  let resolver = SystemFamilyPolicyResolver::new(engine.hash_algo())?;
  let conflict_hashes = conflicts
    .iter()
    .flat_map(|conflict| [&conflict.winner.hash, &conflict.loser.hash])
    .filter(|hash| !hash.is_empty())
    .cloned()
    .collect::<HashSet<_>>();
  let published_hashes = operations
    .iter()
    .filter_map(|operation| match operation {
      MergeOp::AddFile { file_hash, .. } => Some(file_hash.clone()),
      MergeOp::AddSymlink { symlink_hash, .. } => Some(symlink_hash.clone()),
      MergeOp::DeleteFile { .. } | MergeOp::DeleteSymlink { .. } => None,
    })
    .collect::<HashSet<_>>();
  let mut retained_hashes = HashSet::new();
  let mut versions = Vec::new();

  for (path, (identity_hash, record)) in remote_diff.added.iter().chain(remote_diff.modified.iter()) {
    if conflict_hashes.contains(identity_hash) && !published_hashes.contains(identity_hash) && retained_hashes.insert(identity_hash.clone())
    {
      resolver.require_transfer_leaf_path(path, SystemFamilyTransferOperationV1::PeerReplication)?;
      versions.push(SyncImmutableVersion::File { identity_hash: identity_hash.clone(), record: record.clone() });
    }
  }
  for (path, (identity_hash, record)) in remote_diff.symlinks_added.iter().chain(remote_diff.symlinks_modified.iter()) {
    if conflict_hashes.contains(identity_hash) && !published_hashes.contains(identity_hash) && retained_hashes.insert(identity_hash.clone())
    {
      resolver.require_transfer_leaf_path(path, SystemFamilyTransferOperationV1::PeerReplication)?;
      versions.push(SyncImmutableVersion::Symlink { identity_hash: identity_hash.clone(), record: record.clone() });
    }
  }

  Ok(versions)
}

/// Validate the complete mutation set before reading chunks or changing HEAD.
/// Merge operations are currently a peer-replication-only boundary, so an
/// omitted, node-local, unknown, or structural-only path is a malformed peer
/// payload rather than something to skip midway through a batch.
fn validate_peer_transfer_paths(engine: &StorageEngine, operations: &[MergeOp]) -> EngineResult<()> {
  let resolver = SystemFamilyPolicyResolver::new(engine.hash_algo())?;
  for operation in operations {
    let path = match operation {
      MergeOp::AddFile { path, .. } | MergeOp::DeleteFile { path } | MergeOp::AddSymlink { path, .. } | MergeOp::DeleteSymlink { path } => {
        path
      }
    };
    resolver.require_transfer_leaf_path(path, SystemFamilyTransferOperationV1::PeerReplication)?;
  }
  Ok(())
}
