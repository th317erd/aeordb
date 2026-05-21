use crate::engine::compression::CompressionAlgorithm;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::merge::MergeOp;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// Apply merge operations to the local engine.
///
/// NOTE: This is NOT atomic in the database sense. Each operation is applied
/// individually and durable (per-entry fsync). If operation N fails, operations
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
/// 2. Apply all add/modify operations
/// 3. Apply all delete operations
/// 4. HEAD is updated by the final directory_ops operations
///
/// If any step fails, return error. The caller should NOT have modified
/// HEAD before calling this -- the previous HEAD remains valid on failure.
pub fn apply_merge_operations(
    engine: &StorageEngine,
    context: &RequestContext,
    operations: &[MergeOp],
) -> EngineResult<()> {
    // Pre-flight: verify all chunks exist for AddFile operations
    verify_chunks_exist(engine, operations)?;

    let directory_ops = DirectoryOps::new(engine);

    for operation in operations {
        match operation {
            MergeOp::AddFile { path, file_hash: _, file_record } => {
                // Reconstruct file data from chunks
                let data = reassemble_file_data(engine, &file_record.chunk_hashes)?;
                directory_ops.store_file_buffered(
                    context,
                    path,
                    &data,
                    file_record.content_type.as_deref(),
                )?;
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

/// Pre-flight check: verify that all chunks referenced by AddFile operations
/// exist in the engine. Fails fast with a clear error if any chunk is missing,
/// rather than partially applying operations before discovering the gap.
fn verify_chunks_exist(engine: &StorageEngine, operations: &[MergeOp]) -> EngineResult<()> {
    for operation in operations {
        if let MergeOp::AddFile { path, file_record, .. } = operation {
            for chunk_hash in &file_record.chunk_hashes {
                if engine.get_entry(chunk_hash)?.is_none() {
                    return Err(EngineError::NotFound(
                        format!(
                            "Missing chunk during merge for {}: {}",
                            path,
                            hex::encode(chunk_hash),
                        ),
                    ));
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
        let (header, _key, value) = engine.get_entry(chunk_hash)?
            .ok_or_else(|| EngineError::NotFound(
                format!("Missing chunk during reassembly: {}", hex::encode(chunk_hash)),
            ))?;
        let chunk_data = if header.compression_algo != CompressionAlgorithm::None {
            crate::engine::decompress(&value, header.compression_algo)?
        } else {
            value
        };
        data.extend_from_slice(&chunk_data);
    }
    Ok(data)
}
