//! Regression tests for the 2026-05-11 xenocept corruption:
//! header.hot_tail_offset > file_size after a kill-mid-write.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use aeordb::engine::{
    inspect_header, repair_header_in_place, DirectoryOps, RequestContext, StorageEngine,
};

fn make_temp_db() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.aeordb").to_string_lossy().to_string();
    (dir, path)
}

/// Simulate the xenocept failure mode: corrupt the header so hot_tail_offset
/// points beyond the file's actual EOF.
fn poison_hot_tail_offset_past_eof(path: &str) {
    let file_size = std::fs::metadata(path).unwrap().len();
    let phantom_offset = file_size + 57_064; // arbitrary, just must exceed EOF
    let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
    // hot_tail_offset position depends on header version. v3 inserts a u64
    // sequence field at byte 7, shifting hot_tail_offset from 114 → 122.
    let mut version_byte = [0u8; 1];
    file.seek(SeekFrom::Start(4)).unwrap();
    file.read_exact(&mut version_byte).unwrap();
    let hot_tail_pos: u64 = if version_byte[0] >= 3 { 122 } else { 114 };

    file.seek(SeekFrom::Start(hot_tail_pos)).unwrap();
    file.write_all(&phantom_offset.to_le_bytes()).unwrap();
    // Recompute CRC so the corruption looks like a real fsync-ordering
    // bug, not a CRC failure.
    let mut bytes = [0u8; aeordb::engine::FILE_HEADER_SIZE];
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_exact(&mut bytes).unwrap();
    let new_crc = crc32fast::hash(&bytes[..aeordb::engine::FILE_HEADER_SIZE - 4]);
    file.seek(SeekFrom::Start((aeordb::engine::FILE_HEADER_SIZE - 4) as u64))
        .unwrap();
    file.write_all(&new_crc.to_le_bytes()).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn inspect_detects_hot_tail_past_eof() {
    let (_dir, path) = make_temp_db();
    {
        let engine = StorageEngine::create(&path).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain"))
            .unwrap();
        engine.shutdown().unwrap();
    }

    poison_hot_tail_offset_past_eof(&path);

    let report = inspect_header(&path).unwrap();
    assert!(report.hot_tail_past_eof.is_some());
    let mismatch = report.hot_tail_past_eof.unwrap();
    assert!(mismatch.bytes_past_eof > 0);
    assert_eq!(mismatch.bytes_past_eof, 57_064);
}

#[test]
fn repair_recovers_data_after_hot_tail_past_eof() {
    let (_dir, path) = make_temp_db();
    {
        let engine = StorageEngine::create(&path).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file_buffered(&ctx, "/test.txt", b"hello world", Some("text/plain"))
            .unwrap();
        ops.store_file_buffered(&ctx, "/dir/nested.txt", b"nested", Some("text/plain"))
            .unwrap();
        engine.shutdown().unwrap();
    }

    poison_hot_tail_offset_past_eof(&path);

    // Open via low-level repair, which should reset hot_tail_offset to 0
    // and let dirty startup rebuild from WAL
    let report = repair_header_in_place(&path).unwrap();
    assert!(report.repaired);
    assert!(report.hot_tail_past_eof.is_some());

    // Now StorageEngine::open should succeed and recover the files
    let engine = StorageEngine::open(&path).unwrap();
    let ops = DirectoryOps::new(&engine);
    let recovered = ops.read_file_buffered("/test.txt").unwrap();
    assert_eq!(recovered, b"hello world");
    let recovered_nested = ops.read_file_buffered("/dir/nested.txt").unwrap();
    assert_eq!(recovered_nested, b"nested");
}

#[test]
fn header_crc_catches_single_byte_corruption_in_one_slot() {
    // A/B double-buffer: corrupting ONE slot is recoverable — the engine
    // reads the other slot. inspect_header (which reads slot A only) reports
    // the CRC fail; open succeeds via slot B (which holds the most recent
    // header after any update).
    let (_dir, path) = make_temp_db();
    {
        let engine = StorageEngine::create(&path).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain"))
            .unwrap();
        engine.shutdown().unwrap();
    }

    // Flip a byte in slot A only (offset 50). Slot B is at 256-511 and untouched.
    {
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(50)).unwrap();
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 0xFF;
        file.seek(SeekFrom::Start(50)).unwrap();
        file.write_all(&byte).unwrap();
        file.sync_all().unwrap();
    }

    let report = inspect_header(&path).unwrap();
    assert!(report.crc_failed, "byte flip in slot A should fail its CRC");

    // Open should SUCCEED because slot B is still valid — that's the entire
    // point of A/B double-buffering.
    let result = StorageEngine::open(&path);
    assert!(result.is_ok(), "open should fall back to slot B");
    let engine = result.unwrap();
    let ops = DirectoryOps::new(&engine);
    assert_eq!(ops.read_file_buffered("/test.txt").unwrap(), b"hello");
}

#[test]
fn corrupting_both_slots_refuses_open() {
    // The A/B fallback only protects against single-slot torn writes.
    // Corrupting BOTH slots leaves the engine with no valid header to read.
    let (_dir, path) = make_temp_db();
    {
        let engine = StorageEngine::create(&path).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file_buffered(&ctx, "/test.txt", b"hello", Some("text/plain"))
            .unwrap();
        engine.shutdown().unwrap();
    }

    // Flip a byte in slot A AND in slot B.
    {
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        for offset in [50u64, 256 + 50] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            let mut byte = [0u8; 1];
            file.read_exact(&mut byte).unwrap();
            byte[0] ^= 0xFF;
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&byte).unwrap();
        }
        file.sync_all().unwrap();
    }

    let result = StorageEngine::open(&path);
    assert!(result.is_err(), "open should refuse when both slots fail CRC");
}

#[test]
fn repair_writes_v2_header_with_valid_crc() {
    let (_dir, path) = make_temp_db();
    {
        let engine = StorageEngine::create(&path).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file_buffered(&ctx, "/x.txt", b"x", Some("text/plain"))
            .unwrap();
        engine.shutdown().unwrap();
    }
    poison_hot_tail_offset_past_eof(&path);

    let repaired = repair_header_in_place(&path).unwrap();
    assert!(repaired.repaired);

    // Re-inspect — should now be already_ok
    let post = inspect_header(&path).unwrap();
    assert!(post.already_ok, "post-repair header should be clean, got {:?}", post);
}

#[test]
fn inspect_reports_already_ok_on_clean_db() {
    let (_dir, path) = make_temp_db();
    {
        let engine = StorageEngine::create(&path).unwrap();
        engine.shutdown().unwrap();
    }

    let report = inspect_header(&path).unwrap();
    assert!(report.already_ok);
    assert!(report.hot_tail_past_eof.is_none());
    assert!(report.upgraded_version.is_none());
    assert!(!report.crc_failed);
}
