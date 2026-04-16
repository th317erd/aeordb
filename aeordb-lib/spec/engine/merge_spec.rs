use std::collections::{HashMap, HashSet};

use aeordb::engine::file_record::FileRecord;
use aeordb::engine::merge::{three_way_merge, ConflictType, MergeOp};
use aeordb::engine::symlink_record::SymlinkRecord;
use aeordb::engine::tree_walker::TreeDiff;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an empty TreeDiff.
fn empty_diff() -> TreeDiff {
    TreeDiff {
        added: HashMap::new(),
        modified: HashMap::new(),
        deleted: Vec::new(),
        new_chunks: HashSet::new(),
        changed_directories: HashMap::new(),
        symlinks_added: HashMap::new(),
        symlinks_modified: HashMap::new(),
        symlinks_deleted: Vec::new(),
    }
}

/// Create a FileRecord with a specific updated_at timestamp and chunk hashes.
fn make_file_record(path: &str, updated_at: i64, chunk_hashes: Vec<Vec<u8>>) -> FileRecord {
    let total_size: u64 = chunk_hashes.iter().map(|c| c.len() as u64).sum();
    FileRecord {
        path: path.to_string(),
        content_type: Some("text/plain".to_string()),
        total_size,
        created_at: updated_at,
        updated_at,
        metadata: Vec::new(),
        chunk_hashes,
    }
}

/// Create a SymlinkRecord with a specific target and updated_at.
fn make_symlink_record(path: &str, target: &str, updated_at: i64) -> SymlinkRecord {
    SymlinkRecord {
        path: path.to_string(),
        target: target.to_string(),
        created_at: updated_at,
        updated_at,
    }
}

/// Count operations of each type.
fn count_ops(ops: &[MergeOp]) -> (usize, usize, usize, usize) {
    let mut add_files = 0;
    let mut delete_files = 0;
    let mut add_symlinks = 0;
    let mut delete_symlinks = 0;
    for op in ops {
        match op {
            MergeOp::AddFile { .. } => add_files += 1,
            MergeOp::DeleteFile { .. } => delete_files += 1,
            MergeOp::AddSymlink { .. } => add_symlinks += 1,
            MergeOp::DeleteSymlink { .. } => delete_symlinks += 1,
        }
    }
    (add_files, delete_files, add_symlinks, delete_symlinks)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_no_conflict_different_paths() {
    // Local adds /a.txt, remote adds /b.txt -> both applied, no conflicts
    let mut local_diff = empty_diff();
    local_diff.added.insert(
        "/a.txt".to_string(),
        (vec![1, 2, 3], make_file_record("/a.txt", 100, vec![vec![10]])),
    );

    let mut remote_diff = empty_diff();
    remote_diff.added.insert(
        "/b.txt".to_string(),
        (vec![4, 5, 6], make_file_record("/b.txt", 100, vec![vec![20]])),
    );

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 0, "should have no conflicts");

    // Remote's /b.txt should be applied; local's /a.txt is already applied
    let (add_files, delete_files, _, _) = count_ops(&result.operations);
    assert_eq!(add_files, 1, "should add remote's file");
    assert_eq!(delete_files, 0, "no deletes");

    // The added file should be /b.txt (from remote)
    match &result.operations[0] {
        MergeOp::AddFile { path, .. } => assert_eq!(path, "/b.txt"),
        other => panic!("expected AddFile, got {:?}", other),
    }
}

#[test]
fn test_no_conflict_same_hash() {
    // Both modify same path with same hash -> no conflict
    let hash = vec![1, 2, 3, 4];
    let record = make_file_record("/shared.txt", 100, vec![vec![42]]);

    let mut local_diff = empty_diff();
    local_diff.modified.insert("/shared.txt".to_string(), (hash.clone(), record.clone()));

    let mut remote_diff = empty_diff();
    remote_diff.modified.insert("/shared.txt".to_string(), (hash.clone(), record.clone()));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 0, "identical changes = no conflict");
    assert!(result.operations.is_empty(), "nothing to apply since changes are identical");
}

#[test]
fn test_conflict_concurrent_modify() {
    // Same path, different hash -> LWW winner, conflict recorded
    let local_record = make_file_record("/doc.txt", 200, vec![vec![1]]);
    let remote_record = make_file_record("/doc.txt", 300, vec![vec![2]]);

    let mut local_diff = empty_diff();
    local_diff.modified.insert("/doc.txt".to_string(), (vec![10], local_record.clone()));

    let mut remote_diff = empty_diff();
    remote_diff.modified.insert("/doc.txt".to_string(), (vec![20], remote_record.clone()));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1, "should have one conflict");
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ConcurrentModify);
    assert_eq!(result.conflicts[0].path, "/doc.txt");

    // Remote has higher timestamp (300 > 200), so remote wins
    assert_eq!(result.conflicts[0].winner.virtual_time, 300);
    assert_eq!(result.conflicts[0].loser.virtual_time, 200);

    // Since remote wins, we need an AddFile op to apply remote's version
    let (add_files, _, _, _) = count_ops(&result.operations);
    assert_eq!(add_files, 1, "should apply remote winner");
}

#[test]
fn test_conflict_concurrent_modify_local_wins() {
    // Local has higher timestamp, so local wins -> no AddFile op needed
    let local_record = make_file_record("/doc.txt", 500, vec![vec![1]]);
    let remote_record = make_file_record("/doc.txt", 100, vec![vec![2]]);

    let mut local_diff = empty_diff();
    local_diff.modified.insert("/doc.txt".to_string(), (vec![10], local_record.clone()));

    let mut remote_diff = empty_diff();
    remote_diff.modified.insert("/doc.txt".to_string(), (vec![20], remote_record.clone()));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].winner.virtual_time, 500);
    assert_eq!(result.conflicts[0].loser.virtual_time, 100);

    // Local wins, so no operations needed (local version already applied)
    assert!(result.operations.is_empty(), "local wins = no ops needed");
}

#[test]
fn test_modify_beats_delete_remote_modifies() {
    // Remote modifies, local deletes -> modify wins
    let remote_record = make_file_record("/important.txt", 200, vec![vec![99]]);

    let mut local_diff = empty_diff();
    local_diff.deleted.push("/important.txt".to_string());

    let mut remote_diff = empty_diff();
    remote_diff.modified.insert(
        "/important.txt".to_string(),
        (vec![50], remote_record.clone()),
    );

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ModifyDelete);
    assert_eq!(result.conflicts[0].path, "/important.txt");

    // Modify wins: should apply the file (undo our delete)
    let (add_files, delete_files, _, _) = count_ops(&result.operations);
    assert_eq!(add_files, 1, "modify wins: should re-add file");
    assert_eq!(delete_files, 0, "should not delete");
}

#[test]
fn test_modify_beats_delete_local_modifies() {
    // Local modifies, remote deletes -> modify wins (keep local)
    let local_record = make_file_record("/important.txt", 200, vec![vec![99]]);

    let mut local_diff = empty_diff();
    local_diff.modified.insert(
        "/important.txt".to_string(),
        (vec![50], local_record.clone()),
    );

    let mut remote_diff = empty_diff();
    remote_diff.deleted.push("/important.txt".to_string());

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ModifyDelete);

    // Local modify stays, no operations needed
    assert!(result.operations.is_empty(), "local modify stays = no ops");
}

#[test]
fn test_delete_only_remote() {
    // Only remote deletes -> delete applied
    let mut remote_diff = empty_diff();
    remote_diff.deleted.push("/old.txt".to_string());

    let result = three_way_merge(&empty_diff(), &remote_diff);

    assert_eq!(result.conflicts.len(), 0);
    let (_, delete_files, _, _) = count_ops(&result.operations);
    assert_eq!(delete_files, 1, "should delete remote-deleted file");
}

#[test]
fn test_delete_only_local() {
    // Only local deletes -> already deleted, no ops
    let mut local_diff = empty_diff();
    local_diff.deleted.push("/old.txt".to_string());

    let result = three_way_merge(&local_diff, &empty_diff());

    assert_eq!(result.conflicts.len(), 0);
    assert!(result.operations.is_empty(), "local delete already applied");
}

#[test]
fn test_both_delete_same_path() {
    // Both delete same path -> no conflict
    let mut local_diff = empty_diff();
    local_diff.deleted.push("/gone.txt".to_string());

    let mut remote_diff = empty_diff();
    remote_diff.deleted.push("/gone.txt".to_string());

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 0, "both delete = no conflict");
    assert!(result.operations.is_empty(), "nothing to do");
}

#[test]
fn test_adds_before_deletes() {
    // Operations should be sorted: adds/modifies first, deletes last
    let remote_record = make_file_record("/new.txt", 100, vec![vec![1]]);

    let mut remote_diff = empty_diff();
    remote_diff.added.insert("/new.txt".to_string(), (vec![1], remote_record));
    remote_diff.deleted.push("/old.txt".to_string());
    remote_diff.deleted.push("/ancient.txt".to_string());

    let result = three_way_merge(&empty_diff(), &remote_diff);

    assert!(result.operations.len() >= 3, "should have 3 operations");

    // First op should be an AddFile, last ops should be DeleteFile
    assert!(
        matches!(&result.operations[0], MergeOp::AddFile { .. }),
        "first op should be AddFile"
    );

    for op in &result.operations[1..] {
        assert!(
            matches!(op, MergeOp::DeleteFile { .. }),
            "remaining ops should be deletes"
        );
    }
}

#[test]
fn test_commutativity() {
    // merge(A, B) must produce same conflicts with same winners as merge(B, A)
    let record_a = make_file_record("/conflict.txt", 100, vec![vec![1, 1]]);
    let record_b = make_file_record("/conflict.txt", 200, vec![vec![2, 2]]);

    let mut diff_a = empty_diff();
    diff_a.modified.insert("/conflict.txt".to_string(), (vec![10], record_a));
    diff_a.added.insert(
        "/only_a.txt".to_string(),
        (vec![30], make_file_record("/only_a.txt", 50, vec![vec![3]])),
    );

    let mut diff_b = empty_diff();
    diff_b.modified.insert("/conflict.txt".to_string(), (vec![20], record_b));
    diff_b.deleted.push("/removed.txt".to_string());

    let result_ab = three_way_merge(&diff_a, &diff_b);
    let result_ba = three_way_merge(&diff_b, &diff_a);

    // Same number of conflicts
    assert_eq!(
        result_ab.conflicts.len(),
        result_ba.conflicts.len(),
        "same number of conflicts"
    );

    // Same conflict paths
    let paths_ab: Vec<&str> = result_ab.conflicts.iter().map(|c| c.path.as_str()).collect();
    let paths_ba: Vec<&str> = result_ba.conflicts.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths_ab, paths_ba, "same conflict paths");

    // Same winner in each conflict (deterministic LWW)
    for (conflict_ab, conflict_ba) in result_ab.conflicts.iter().zip(result_ba.conflicts.iter()) {
        assert_eq!(
            conflict_ab.winner.virtual_time,
            conflict_ba.winner.virtual_time,
            "same winner virtual_time for {}",
            conflict_ab.path,
        );
        assert_eq!(
            conflict_ab.winner.hash,
            conflict_ba.winner.hash,
            "same winner hash for {}",
            conflict_ab.path,
        );
        assert_eq!(
            conflict_ab.loser.virtual_time,
            conflict_ba.loser.virtual_time,
            "same loser virtual_time for {}",
            conflict_ab.path,
        );
    }
}

#[test]
fn test_commutativity_with_equal_timestamps() {
    // When timestamps are equal, hash-based tiebreak must still be deterministic
    let record_a = make_file_record("/tie.txt", 100, vec![vec![1]]);
    let record_b = make_file_record("/tie.txt", 100, vec![vec![2]]);

    let mut diff_a = empty_diff();
    diff_a.modified.insert("/tie.txt".to_string(), (vec![10], record_a));

    let mut diff_b = empty_diff();
    diff_b.modified.insert("/tie.txt".to_string(), (vec![20], record_b));

    let result_ab = three_way_merge(&diff_a, &diff_b);
    let result_ba = three_way_merge(&diff_b, &diff_a);

    assert_eq!(result_ab.conflicts.len(), 1);
    assert_eq!(result_ba.conflicts.len(), 1);

    // Same winner regardless of argument order
    assert_eq!(
        result_ab.conflicts[0].winner.hash,
        result_ba.conflicts[0].winner.hash,
        "tiebreak must be deterministic"
    );
}

#[test]
fn test_concurrent_create() {
    // Both add same path (new file) with different content -> conflict
    let record_a = make_file_record("/new.txt", 100, vec![vec![1]]);
    let record_b = make_file_record("/new.txt", 200, vec![vec![2]]);

    let mut diff_a = empty_diff();
    diff_a.added.insert("/new.txt".to_string(), (vec![10], record_a));

    let mut diff_b = empty_diff();
    diff_b.added.insert("/new.txt".to_string(), (vec![20], record_b));

    let result = three_way_merge(&diff_a, &diff_b);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ConcurrentCreate);
    assert_eq!(result.conflicts[0].path, "/new.txt");
}

#[test]
fn test_symlink_only_remote_add() {
    let symlink = make_symlink_record("/link", "/target/file", 100);

    let mut remote_diff = empty_diff();
    remote_diff.symlinks_added.insert(
        "/link".to_string(),
        (vec![1, 2], symlink),
    );

    let result = three_way_merge(&empty_diff(), &remote_diff);

    assert_eq!(result.conflicts.len(), 0);
    let (_, _, add_symlinks, _) = count_ops(&result.operations);
    assert_eq!(add_symlinks, 1);
}

#[test]
fn test_symlink_only_remote_delete() {
    let mut remote_diff = empty_diff();
    remote_diff.symlinks_deleted.push("/link".to_string());

    let result = three_way_merge(&empty_diff(), &remote_diff);

    assert_eq!(result.conflicts.len(), 0);
    let (_, _, _, delete_symlinks) = count_ops(&result.operations);
    assert_eq!(delete_symlinks, 1);
}

#[test]
fn test_symlink_same_target_no_conflict() {
    let symlink = make_symlink_record("/link", "/same/target", 100);

    let mut local_diff = empty_diff();
    local_diff.symlinks_modified.insert("/link".to_string(), (vec![1], symlink.clone()));

    let mut remote_diff = empty_diff();
    remote_diff.symlinks_modified.insert("/link".to_string(), (vec![2], symlink.clone()));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 0, "same target = no conflict");
}

#[test]
fn test_symlink_different_target_conflict() {
    let local_symlink = make_symlink_record("/link", "/target/a", 100);
    let remote_symlink = make_symlink_record("/link", "/target/b", 200);

    let mut local_diff = empty_diff();
    local_diff.symlinks_modified.insert("/link".to_string(), (vec![1], local_symlink));

    let mut remote_diff = empty_diff();
    remote_diff.symlinks_modified.insert("/link".to_string(), (vec![2], remote_symlink));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ConcurrentModify);
}

#[test]
fn test_symlink_modify_beats_delete() {
    let remote_symlink = make_symlink_record("/link", "/new/target", 200);

    let mut local_diff = empty_diff();
    local_diff.symlinks_deleted.push("/link".to_string());

    let mut remote_diff = empty_diff();
    remote_diff.symlinks_modified.insert("/link".to_string(), (vec![5], remote_symlink));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ModifyDelete);

    // Should re-add the symlink (modify wins)
    let (_, _, add_symlinks, delete_symlinks) = count_ops(&result.operations);
    assert_eq!(add_symlinks, 1, "modify should win, re-adding symlink");
    assert_eq!(delete_symlinks, 0, "should not delete");
}

#[test]
fn test_symlink_both_delete_no_conflict() {
    let mut local_diff = empty_diff();
    local_diff.symlinks_deleted.push("/link".to_string());

    let mut remote_diff = empty_diff();
    remote_diff.symlinks_deleted.push("/link".to_string());

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 0);
    assert!(result.operations.is_empty());
}

#[test]
fn test_empty_diffs_produce_empty_result() {
    let result = three_way_merge(&empty_diff(), &empty_diff());

    assert!(result.operations.is_empty());
    assert!(result.conflicts.is_empty());
}

#[test]
fn test_mixed_files_and_symlinks() {
    // Verify that file and symlink merges don't interfere with each other
    let file_record = make_file_record("/data.txt", 100, vec![vec![1]]);
    let symlink_record = make_symlink_record("/link", "/data.txt", 100);

    let mut remote_diff = empty_diff();
    remote_diff.added.insert("/data.txt".to_string(), (vec![1], file_record));
    remote_diff.symlinks_added.insert("/link".to_string(), (vec![2], symlink_record));
    remote_diff.deleted.push("/old.txt".to_string());

    let result = three_way_merge(&empty_diff(), &remote_diff);

    let (add_files, delete_files, add_symlinks, delete_symlinks) = count_ops(&result.operations);
    assert_eq!(add_files, 1);
    assert_eq!(delete_files, 1);
    assert_eq!(add_symlinks, 1);
    assert_eq!(delete_symlinks, 0);
    assert_eq!(result.conflicts.len(), 0);

    // Verify ordering: adds before deletes
    let first_delete_index = result.operations.iter().position(|op| {
        matches!(op, MergeOp::DeleteFile { .. } | MergeOp::DeleteSymlink { .. })
    });
    let last_add_index = result.operations.iter().rposition(|op| {
        matches!(op, MergeOp::AddFile { .. } | MergeOp::AddSymlink { .. })
    });
    if let (Some(first_del), Some(last_add)) = (first_delete_index, last_add_index) {
        assert!(
            last_add < first_del,
            "all adds should come before all deletes"
        );
    }
}

#[test]
fn test_many_paths_no_conflicts() {
    // Stress test: many non-overlapping changes
    let mut local_diff = empty_diff();
    let mut remote_diff = empty_diff();

    for i in 0..100 {
        let local_path = format!("/local/file_{}.txt", i);
        let remote_path = format!("/remote/file_{}.txt", i);

        local_diff.added.insert(
            local_path.clone(),
            (vec![i as u8], make_file_record(&local_path, 100, vec![vec![i as u8]])),
        );
        remote_diff.added.insert(
            remote_path.clone(),
            (vec![i as u8 + 100], make_file_record(&remote_path, 100, vec![vec![i as u8 + 100]])),
        );
    }

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 0, "disjoint paths = no conflicts");
    // Should apply all 100 remote adds
    let (add_files, _, _, _) = count_ops(&result.operations);
    assert_eq!(add_files, 100, "should apply all 100 remote adds");
}

#[test]
fn test_commutativity_comprehensive() {
    // Comprehensive commutativity test with files, symlinks, deletes, and conflicts
    let mut diff_a = empty_diff();
    diff_a.modified.insert(
        "/conflict.txt".to_string(),
        (vec![1], make_file_record("/conflict.txt", 100, vec![vec![1]])),
    );
    diff_a.added.insert(
        "/only_a.txt".to_string(),
        (vec![2], make_file_record("/only_a.txt", 50, vec![vec![2]])),
    );
    diff_a.deleted.push("/both_delete.txt".to_string());
    diff_a.deleted.push("/a_deletes_b_modifies.txt".to_string());

    let mut diff_b = empty_diff();
    diff_b.modified.insert(
        "/conflict.txt".to_string(),
        (vec![3], make_file_record("/conflict.txt", 200, vec![vec![3]])),
    );
    diff_b.added.insert(
        "/only_b.txt".to_string(),
        (vec![4], make_file_record("/only_b.txt", 75, vec![vec![4]])),
    );
    diff_b.deleted.push("/both_delete.txt".to_string());
    diff_b.modified.insert(
        "/a_deletes_b_modifies.txt".to_string(),
        (vec![5], make_file_record("/a_deletes_b_modifies.txt", 300, vec![vec![5]])),
    );

    let result_ab = three_way_merge(&diff_a, &diff_b);
    let result_ba = three_way_merge(&diff_b, &diff_a);

    // Same conflicts
    assert_eq!(result_ab.conflicts.len(), result_ba.conflicts.len());

    // Collect conflict info into comparable form
    let mut conflicts_ab: Vec<(String, String, u64)> = result_ab.conflicts.iter()
        .map(|c| (c.path.clone(), format!("{:?}", c.conflict_type), c.winner.virtual_time))
        .collect();
    conflicts_ab.sort();

    let mut conflicts_ba: Vec<(String, String, u64)> = result_ba.conflicts.iter()
        .map(|c| (c.path.clone(), format!("{:?}", c.conflict_type), c.winner.virtual_time))
        .collect();
    conflicts_ba.sort();

    assert_eq!(conflicts_ab, conflicts_ba, "conflict details must be identical");
}

#[test]
fn test_add_vs_modify_same_path_different_sides() {
    // Local adds a file (new), remote modifies same path (already existed)
    // This hits the "local_added + remote_modified" combination
    let local_record = make_file_record("/file.txt", 100, vec![vec![1]]);
    let remote_record = make_file_record("/file.txt", 200, vec![vec![2]]);

    let mut local_diff = empty_diff();
    local_diff.added.insert("/file.txt".to_string(), (vec![10], local_record));

    let mut remote_diff = empty_diff();
    remote_diff.modified.insert("/file.txt".to_string(), (vec![20], remote_record));

    let result = three_way_merge(&local_diff, &remote_diff);

    // Should be a conflict since hashes differ
    assert_eq!(result.conflicts.len(), 1);
    // ConcurrentCreate because local side is an add
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ConcurrentCreate);
}

#[test]
fn test_symlink_concurrent_create() {
    // Both add same symlink path with different targets
    let local_symlink = make_symlink_record("/link", "/target/a", 100);
    let remote_symlink = make_symlink_record("/link", "/target/b", 200);

    let mut local_diff = empty_diff();
    local_diff.symlinks_added.insert("/link".to_string(), (vec![1], local_symlink));

    let mut remote_diff = empty_diff();
    remote_diff.symlinks_added.insert("/link".to_string(), (vec![2], remote_symlink));

    let result = three_way_merge(&local_diff, &remote_diff);

    assert_eq!(result.conflicts.len(), 1);
    assert_eq!(result.conflicts[0].conflict_type, ConflictType::ConcurrentCreate);
}
