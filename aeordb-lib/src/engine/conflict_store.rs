use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::merge::ConflictEntry;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// Store a conflict as a regular database entry under `/.aeordb-conflicts/`.
///
/// Structure:
///   `/.aeordb-conflicts/{path}/.meta` — JSON metadata with winner/loser details
///
/// Since conflicts are stored as normal files in the directory tree,
/// they automatically sync to peers and are covered by GC (the walk
/// from HEAD traverses all directories including `/.aeordb-conflicts/`).
pub fn store_conflict(
    engine: &StorageEngine,
    ctx: &RequestContext,
    conflict: &ConflictEntry,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let base_path = format!("/.conflicts{}", conflict.path);

    let meta = serde_json::json!({
        "path": conflict.path,
        "conflict_type": format!("{:?}", conflict.conflict_type),
        "auto_winner": "winner",
        "created_at": chrono::Utc::now().timestamp_millis(),
        "winner": {
            "hash": hex::encode(&conflict.winner.hash),
            "virtual_time": conflict.winner.virtual_time,
            "node_id": conflict.winner.node_id,
            "size": conflict.winner.size,
            "content_type": conflict.winner.content_type,
        },
        "loser": {
            "hash": hex::encode(&conflict.loser.hash),
            "virtual_time": conflict.loser.virtual_time,
            "node_id": conflict.loser.node_id,
            "size": conflict.loser.size,
            "content_type": conflict.loser.content_type,
        },
    });

    let meta_json = serde_json::to_vec_pretty(&meta).unwrap_or_default();
    ops.store_file_buffered(
        ctx,
        &format!("{}/.meta", base_path),
        &meta_json,
        Some("application/json"),
    )?;

    Ok(())
}

/// List all unresolved conflicts.
///
/// Walks the `/.aeordb-conflicts/` directory tree recursively, collecting
/// every `.meta` file it finds.
pub fn list_conflicts(engine: &StorageEngine) -> EngineResult<Vec<serde_json::Value>> {
    let ops = DirectoryOps::new(engine);
    let mut conflicts = Vec::new();

    // Use recursive listing to find all .meta files under /.conflicts
    let entries = match crate::engine::directory_listing::list_directory_recursive(
        engine,
        "/.conflicts",
        -1,    // unlimited depth
        Some("*.meta"),  // glob for .meta files only
        None,
    ) {
        Ok(e) => e,
        Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    for entry in &entries {
        if entry.name == ".meta" {
            if let Ok(data) = ops.read_file_buffered(&entry.path) {
                if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&data) {
                    conflicts.push(meta);
                }
            }
        }
    }

    Ok(conflicts)
}

/// Get a specific conflict's metadata.
pub fn get_conflict(
    engine: &StorageEngine,
    path: &str,
) -> EngineResult<Option<serde_json::Value>> {
    let ops = DirectoryOps::new(engine);
    let meta_path = format!("/.conflicts{}/.meta", path);

    match ops.read_file_buffered(&meta_path) {
        Ok(data) => {
            let meta = serde_json::from_slice(&data)
                .map_err(|e| EngineError::JsonParseError(e.to_string()))?;
            Ok(Some(meta))
        }
        Err(EngineError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Resolve a conflict by picking a version ("winner" or "loser").
///
/// The chosen version's file data is read from the engine by its
/// identity hash, reconstructed from chunks, and written to the real
/// path. The conflict entry is then cleaned up.
pub fn resolve_conflict(
    engine: &StorageEngine,
    ctx: &RequestContext,
    path: &str,
    pick: &str,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);

    // Read the conflict metadata
    let meta_path = format!("/.conflicts{}/.meta", path);
    let meta_data = ops.read_file_buffered(&meta_path)?;
    let meta: serde_json::Value = serde_json::from_slice(&meta_data)
        .map_err(|e| EngineError::JsonParseError(e.to_string()))?;

    // Validate the pick value
    if pick != "winner" && pick != "loser" {
        return Err(EngineError::InvalidInput(format!(
            "Invalid pick '{}': must be 'winner' or 'loser'",
            pick
        )));
    }

    // Get the chosen version's hash
    let chosen = &meta[pick];
    let chosen_hash_hex = chosen["hash"]
        .as_str()
        .ok_or_else(|| {
            EngineError::InvalidInput(format!("Invalid pick '{}': no hash found", pick))
        })?;
    let chosen_hash = hex::decode(chosen_hash_hex)
        .map_err(|_| EngineError::InvalidInput("Invalid hash hex".to_string()))?;

    // Read the chosen version's FileRecord from the engine by identity hash
    let hash_length = engine.hash_algo().hash_length();
    if let Some((header, _key, value)) = engine.get_entry(&chosen_hash)? {
        let file_record =
            crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version)?;

        // Read chunks and reconstruct file data
        let mut data = Vec::new();
        for chunk_hash in &file_record.chunk_hashes {
            if let Some((chunk_header, _key, chunk_value)) = engine.get_entry(chunk_hash)? {
                let chunk_data =
                    if chunk_header.compression_algo != crate::engine::CompressionAlgorithm::None {
                        crate::engine::decompress(&chunk_value, chunk_header.compression_algo)?
                    } else {
                        chunk_value
                    };
                data.extend_from_slice(&chunk_data);
            }
        }

        // Write the chosen version to the real path
        ops.store_file_buffered(ctx, path, &data, file_record.content_type.as_deref())?;
    }

    // Clean up conflict entry
    let _ = ops.delete_file(ctx, &meta_path);

    Ok(())
}

/// Dismiss a conflict (accept the auto-winner, just clean up the conflict entry).
///
/// The auto-winner is already at the real path from the merge, so we only
/// need to remove the conflict metadata.
pub fn dismiss_conflict(
    engine: &StorageEngine,
    ctx: &RequestContext,
    path: &str,
) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let meta_path = format!("/.conflicts{}/.meta", path);

    // Verify the conflict exists before dismissing
    match ops.read_file_buffered(&meta_path) {
        Ok(_) => {}
        Err(EngineError::NotFound(_)) => {
            return Err(EngineError::NotFound(format!(
                "No conflict found for path: {}",
                path
            )));
        }
        Err(e) => return Err(e),
    }

    let _ = ops.delete_file(ctx, &meta_path);
    Ok(())
}
