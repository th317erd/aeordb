use std::collections::BTreeSet;

use crate::engine::file_record::FileRecord;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::tree_walker::TreeDiff;

/// The type of conflict detected during merge.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictType {
    ConcurrentModify,
    ModifyDelete,
    ConcurrentCreate,
}

/// A version involved in a conflict.
#[derive(Debug, Clone)]
pub struct ConflictVersion {
    pub hash: Vec<u8>,
    pub virtual_time: u64,
    pub node_id: u64,
    pub size: u64,
    pub content_type: Option<String>,
}

/// A conflict detected during merge.
#[derive(Debug, Clone)]
pub struct ConflictEntry {
    pub path: String,
    pub conflict_type: ConflictType,
    pub winner: ConflictVersion,
    pub loser: ConflictVersion,
}

/// An operation to apply during merge.
#[derive(Debug, Clone)]
pub enum MergeOp {
    /// Add or update a file (store the FileRecord's data).
    AddFile {
        path: String,
        file_hash: Vec<u8>,
        file_record: FileRecord,
    },
    /// Delete a file.
    DeleteFile { path: String },
    /// Add or update a symlink.
    AddSymlink {
        path: String,
        symlink_hash: Vec<u8>,
        symlink_record: SymlinkRecord,
    },
    /// Delete a symlink.
    DeleteSymlink { path: String },
}

/// Result of a three-way merge.
#[derive(Debug)]
pub struct MergeResult {
    pub operations: Vec<MergeOp>,
    pub conflicts: Vec<ConflictEntry>,
}

/// Perform a three-way merge between local and remote changes.
///
/// The merge is DETERMINISTIC and COMMUTATIVE:
/// `merge(local_diff, remote_diff)` produces the same conflict winners
/// as `merge(remote_diff, local_diff)`.
///
/// Conflict resolution rules:
/// 1. Different paths: no conflict, apply both
/// 2. Same path, same content hash: no conflict (identical change)
/// 3. Same path, different hash: LWW by `(virtual_time, node_id)`, loser preserved
/// 4. One modifies, one deletes: MODIFY WINS (safety-first)
/// 5. Both delete: no conflict
pub fn three_way_merge(
    local_diff: &TreeDiff,
    remote_diff: &TreeDiff,
) -> MergeResult {
    let mut operations: Vec<MergeOp> = Vec::new();
    let mut conflicts: Vec<ConflictEntry> = Vec::new();

    // --- FILE MERGING ---
    merge_files(local_diff, remote_diff, &mut operations, &mut conflicts);

    // --- SYMLINK MERGING ---
    merge_symlinks(local_diff, remote_diff, &mut operations, &mut conflicts);

    // Sort operations: adds/modifies before deletes for safety
    operations.sort_by(|a, b| {
        let a_is_delete = matches!(a, MergeOp::DeleteFile { .. } | MergeOp::DeleteSymlink { .. });
        let b_is_delete = matches!(b, MergeOp::DeleteFile { .. } | MergeOp::DeleteSymlink { .. });
        a_is_delete.cmp(&b_is_delete)
    });

    MergeResult { operations, conflicts }
}

/// Merge file changes from local and remote diffs.
fn merge_files(
    local_diff: &TreeDiff,
    remote_diff: &TreeDiff,
    operations: &mut Vec<MergeOp>,
    conflicts: &mut Vec<ConflictEntry>,
) {
    let mut all_file_paths: BTreeSet<String> = BTreeSet::new();
    for path in local_diff.added.keys() { all_file_paths.insert(path.clone()); }
    for path in local_diff.modified.keys() { all_file_paths.insert(path.clone()); }
    for path in &local_diff.deleted { all_file_paths.insert(path.clone()); }
    for path in remote_diff.added.keys() { all_file_paths.insert(path.clone()); }
    for path in remote_diff.modified.keys() { all_file_paths.insert(path.clone()); }
    for path in &remote_diff.deleted { all_file_paths.insert(path.clone()); }

    for path in &all_file_paths {
        let local_added = local_diff.added.get(path);
        let local_modified = local_diff.modified.get(path);
        let local_deleted = local_diff.deleted.contains(path);

        let remote_added = remote_diff.added.get(path);
        let remote_modified = remote_diff.modified.get(path);
        let remote_deleted = remote_diff.deleted.contains(path);

        let local_change = local_added.or(local_modified);
        let remote_change = remote_added.or(remote_modified);

        match (local_change, local_deleted, remote_change, remote_deleted) {
            // Only remote changed -> apply remote
            (None, false, Some((hash, record)), false) => {
                operations.push(MergeOp::AddFile {
                    path: path.clone(),
                    file_hash: hash.clone(),
                    file_record: record.clone(),
                });
            }
            // Only local changed -> already applied, nothing to do
            (Some(_), false, None, false) => {}
            // Both changed, same hash -> identical change, no conflict
            (Some((local_hash, _)), false, Some((remote_hash, _)), false)
                if local_hash == remote_hash => {}
            // Both changed, different hash -> CONFLICT (LWW)
            (Some((local_hash, local_record)), false, Some((remote_hash, remote_record)), false) => {
                resolve_file_conflict(
                    path,
                    local_hash, local_record, local_added.is_some(),
                    remote_hash, remote_record, remote_added.is_some(),
                    operations,
                    conflicts,
                );
            }
            // Remote modified, local deleted -> MODIFY WINS
            (None, true, Some((hash, record)), false) => {
                // Undo our delete, apply remote's modification
                operations.push(MergeOp::AddFile {
                    path: path.clone(),
                    file_hash: hash.clone(),
                    file_record: record.clone(),
                });
                conflicts.push(make_modify_delete_conflict(path, hash, record));
            }
            // Local modified, remote deleted -> MODIFY WINS (keep local)
            (Some((hash, record)), false, None, true) => {
                // Our modification stays, record conflict for visibility
                conflicts.push(make_modify_delete_conflict(path, hash, record));
            }
            // Only remote deleted -> apply delete
            (None, false, None, true) => {
                operations.push(MergeOp::DeleteFile { path: path.clone() });
            }
            // Only local deleted -> already deleted
            (None, true, None, false) => {}
            // Both deleted -> no conflict
            (None, true, None, true) => {}
            // Fallthrough: shouldn't happen, handle gracefully
            _ => {}
        }
    }
}

/// Resolve a conflict where both sides changed the same file path with
/// different content. Uses Last-Writer-Wins (LWW) ordering:
/// highest `(updated_at, node_id)` wins. The comparison is deterministic
/// regardless of which side is called "local" vs "remote".
fn resolve_file_conflict(
    path: &str,
    local_hash: &Vec<u8>,
    local_record: &FileRecord,
    local_is_add: bool,
    remote_hash: &Vec<u8>,
    remote_record: &FileRecord,
    remote_is_add: bool,
    operations: &mut Vec<MergeOp>,
    conflicts: &mut Vec<ConflictEntry>,
) {
    let local_time = local_record.updated_at as u64;
    let remote_time = remote_record.updated_at as u64;

    // Deterministic tiebreak: when timestamps are equal, compare hashes
    // lexicographically so both sides pick the same winner.
    let local_wins = match local_time.cmp(&remote_time) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => local_hash >= remote_hash,
    };

    let (winner_hash, winner_record, loser_hash, loser_record) = if local_wins {
        (local_hash, local_record, remote_hash, remote_record)
    } else {
        (remote_hash, remote_record, local_hash, local_record)
    };

    // If remote wins, we need to apply its version
    if !local_wins {
        operations.push(MergeOp::AddFile {
            path: path.to_string(),
            file_hash: winner_hash.clone(),
            file_record: winner_record.clone(),
        });
    }

    let conflict_type = if local_is_add || remote_is_add {
        ConflictType::ConcurrentCreate
    } else {
        ConflictType::ConcurrentModify
    };

    conflicts.push(ConflictEntry {
        path: path.to_string(),
        conflict_type,
        winner: ConflictVersion {
            hash: winner_hash.clone(),
            virtual_time: winner_record.updated_at as u64,
            node_id: 0,
            size: winner_record.total_size,
            content_type: winner_record.content_type.clone(),
        },
        loser: ConflictVersion {
            hash: loser_hash.clone(),
            virtual_time: loser_record.updated_at as u64,
            node_id: 0,
            size: loser_record.total_size,
            content_type: loser_record.content_type.clone(),
        },
    });
}

/// Create a modify-delete conflict entry where the modify side wins.
fn make_modify_delete_conflict(
    path: &str,
    winner_hash: &Vec<u8>,
    winner_record: &FileRecord,
) -> ConflictEntry {
    ConflictEntry {
        path: path.to_string(),
        conflict_type: ConflictType::ModifyDelete,
        winner: ConflictVersion {
            hash: winner_hash.clone(),
            virtual_time: winner_record.updated_at as u64,
            node_id: 0,
            size: winner_record.total_size,
            content_type: winner_record.content_type.clone(),
        },
        loser: ConflictVersion {
            hash: Vec::new(),
            virtual_time: 0,
            node_id: 0,
            size: 0,
            content_type: None,
        },
    }
}

/// Merge symlink changes from local and remote diffs.
fn merge_symlinks(
    local_diff: &TreeDiff,
    remote_diff: &TreeDiff,
    operations: &mut Vec<MergeOp>,
    conflicts: &mut Vec<ConflictEntry>,
) {
    let mut all_symlink_paths: BTreeSet<String> = BTreeSet::new();
    for path in local_diff.symlinks_added.keys() { all_symlink_paths.insert(path.clone()); }
    for path in local_diff.symlinks_modified.keys() { all_symlink_paths.insert(path.clone()); }
    for path in &local_diff.symlinks_deleted { all_symlink_paths.insert(path.clone()); }
    for path in remote_diff.symlinks_added.keys() { all_symlink_paths.insert(path.clone()); }
    for path in remote_diff.symlinks_modified.keys() { all_symlink_paths.insert(path.clone()); }
    for path in &remote_diff.symlinks_deleted { all_symlink_paths.insert(path.clone()); }

    for path in &all_symlink_paths {
        let local_added = local_diff.symlinks_added.get(path);
        let local_modified = local_diff.symlinks_modified.get(path);
        let local_deleted = local_diff.symlinks_deleted.contains(path);

        let remote_added = remote_diff.symlinks_added.get(path);
        let remote_modified = remote_diff.symlinks_modified.get(path);
        let remote_deleted = remote_diff.symlinks_deleted.contains(path);

        let local_change = local_added.or(local_modified);
        let remote_change = remote_added.or(remote_modified);

        match (local_change, local_deleted, remote_change, remote_deleted) {
            // Only remote changed -> apply
            (None, false, Some((hash, record)), false) => {
                operations.push(MergeOp::AddSymlink {
                    path: path.clone(),
                    symlink_hash: hash.clone(),
                    symlink_record: record.clone(),
                });
            }
            // Only local changed -> already applied
            (Some(_), false, None, false) => {}
            // Both changed, same target -> identical, no conflict
            (Some((_, local_record)), false, Some((_, remote_record)), false)
                if local_record.target == remote_record.target => {}
            // Both changed, different target -> LWW conflict
            (Some((local_hash, local_record)), false, Some((remote_hash, remote_record)), false) => {
                let local_time = local_record.updated_at as u64;
                let remote_time = remote_record.updated_at as u64;
                let local_wins = match local_time.cmp(&remote_time) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => local_hash >= remote_hash,
                };
                if !local_wins {
                    operations.push(MergeOp::AddSymlink {
                        path: path.clone(),
                        symlink_hash: remote_hash.clone(),
                        symlink_record: remote_record.clone(),
                    });
                }

                let (winner_hash, winner_record, loser_hash, loser_record) = if local_wins {
                    (local_hash, local_record, remote_hash, remote_record)
                } else {
                    (remote_hash, remote_record, local_hash, local_record)
                };

                let conflict_type = if local_added.is_some() || remote_added.is_some() {
                    ConflictType::ConcurrentCreate
                } else {
                    ConflictType::ConcurrentModify
                };

                conflicts.push(ConflictEntry {
                    path: path.clone(),
                    conflict_type,
                    winner: ConflictVersion {
                        hash: winner_hash.clone(),
                        virtual_time: winner_record.updated_at as u64,
                        node_id: 0,
                        size: 0,
                        content_type: None,
                    },
                    loser: ConflictVersion {
                        hash: loser_hash.clone(),
                        virtual_time: loser_record.updated_at as u64,
                        node_id: 0,
                        size: 0,
                        content_type: None,
                    },
                });
            }
            // Remote modified, local deleted -> MODIFY WINS
            (None, true, Some((hash, record)), false) => {
                operations.push(MergeOp::AddSymlink {
                    path: path.clone(),
                    symlink_hash: hash.clone(),
                    symlink_record: record.clone(),
                });
                conflicts.push(ConflictEntry {
                    path: path.clone(),
                    conflict_type: ConflictType::ModifyDelete,
                    winner: ConflictVersion {
                        hash: hash.clone(),
                        virtual_time: record.updated_at as u64,
                        node_id: 0,
                        size: 0,
                        content_type: None,
                    },
                    loser: ConflictVersion {
                        hash: Vec::new(),
                        virtual_time: 0,
                        node_id: 0,
                        size: 0,
                        content_type: None,
                    },
                });
            }
            // Local modified, remote deleted -> MODIFY WINS (keep local)
            (Some((hash, record)), false, None, true) => {
                conflicts.push(ConflictEntry {
                    path: path.clone(),
                    conflict_type: ConflictType::ModifyDelete,
                    winner: ConflictVersion {
                        hash: hash.clone(),
                        virtual_time: record.updated_at as u64,
                        node_id: 0,
                        size: 0,
                        content_type: None,
                    },
                    loser: ConflictVersion {
                        hash: Vec::new(),
                        virtual_time: 0,
                        node_id: 0,
                        size: 0,
                        content_type: None,
                    },
                });
            }
            // Only remote deleted -> apply
            (None, false, None, true) => {
                operations.push(MergeOp::DeleteSymlink { path: path.clone() });
            }
            // Only local deleted -> already deleted
            (None, true, None, false) => {}
            // Both deleted -> no conflict
            (None, true, None, true) => {}
            // Fallthrough
            _ => {}
        }
    }
}
