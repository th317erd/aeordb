use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::merge::MergeOp;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::SystemFamilyPolicyResolver;
use crate::engine::v4::system_family::SystemFamilyTransferOperationV1;

/// Apply merge operations to the local engine.
///
/// NOTE: This is NOT atomic in the database sense. Each operation is applied
/// individually through the normal durable mutation path. If operation N fails, operations
/// 1..N-1 are already committed. The caller should NOT save sync state if this
/// returns an error -- the next sync cycle will re-attempt from the last
/// successfully saved base hash.
///
/// Pre-flight: verifies all required chunks exist before applying any operations.
/// This prevents the most common failure mode (missing chunks) but does not
/// protect against I/O errors or disk-full conditions during the apply phase.
///
/// Steps:
/// 1. Verify all required chunks exist locally (pre-flight check)
/// 2. Apply operations in the caller-provided order
/// 3. HEAD is published by each successful DirectoryOps mutation
///
/// If any step fails, return an error and do not advance the peer checkpoint.
/// Earlier successful operations may already have published a new HEAD.
pub fn apply_merge_operations(engine: &StorageEngine, context: &RequestContext, operations: &[MergeOp]) -> EngineResult<()> {
  validate_peer_transfer_paths(engine, operations)?;

  // Pre-flight: verify all chunks exist for AddFile operations
  verify_chunks_exist(engine, operations)?;

  let directory_ops = DirectoryOps::new(engine);

  for operation in operations {
    match operation {
      MergeOp::AddFile { path, file_hash: _, file_record } => {
        // Reconstruct file data from chunks
        let data = reassemble_file_data(engine, &file_record.chunk_hashes)?;
        directory_ops.store_file_buffered(context, path, &data, file_record.content_type.as_deref())?;
      }
      MergeOp::DeleteFile { path } => {
        // Ignore NotFound errors -- file might already be deleted
        let _ = directory_ops.delete_file(context, path);
      }
      MergeOp::AddSymlink { path, symlink_hash: _, symlink_record } => {
        directory_ops.store_symlink(context, path, &symlink_record.target)?;
      }
      MergeOp::DeleteSymlink { path } => {
        let _ = directory_ops.delete_symlink(context, path);
      }
    }
  }

  Ok(())
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

/// Pre-flight check: verify that all chunks referenced by AddFile operations
/// exist in the engine. Fails fast with a clear error if any chunk is missing,
/// rather than partially applying operations before discovering the gap.
fn verify_chunks_exist(engine: &StorageEngine, operations: &[MergeOp]) -> EngineResult<()> {
  for operation in operations {
    if let MergeOp::AddFile { path, file_record, .. } = operation {
      for chunk_hash in &file_record.chunk_hashes {
        if engine.get_entry(chunk_hash)?.is_none() {
          return Err(EngineError::NotFound(format!("Missing chunk during merge for {}: {}", path, hex::encode(chunk_hash),)));
        }
      }
    }
  }
  Ok(())
}

/// Reassemble file data by reading and decompressing each chunk in order.
fn reassemble_file_data(engine: &StorageEngine, chunk_hashes: &[Vec<u8>]) -> EngineResult<Vec<u8>> {
  let mut data = Vec::new();
  for chunk_hash in chunk_hashes {
    let chunk_data = engine
      .read_chunk(chunk_hash)?
      .ok_or_else(|| EngineError::NotFound(format!("Missing chunk during reassembly: {}", hex::encode(chunk_hash))))?;
    data.extend_from_slice(&chunk_data);
  }
  Ok(data)
}
