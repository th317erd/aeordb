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
    // hot_tail_offset lives at bytes 114..122 in both v1 and v2
    file.seek(SeekFrom::Start(114)).unwrap();
    file.write_all(&phantom_offset.to_le_bytes()).unwrap();
    // Recompute CRC for v2 so the corruption looks like a real fsync-ordering
    // bug, not a CRC failure
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
        ops.store_file(&ctx, "/test.txt", b"hello", Some("text/plain"))
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
        ops.store_file(&ctx, "/test.txt", b"hello world", Some("text/plain"))
            .unwrap();
        ops.store_file(&ctx, "/dir/nested.txt", b"nested", Some("text/plain"))
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
    let recovered = ops.read_file("/test.txt").unwrap();
    assert_eq!(recovered, b"hello world");
    let recovered_nested = ops.read_file("/dir/nested.txt").unwrap();
    assert_eq!(recovered_nested, b"nested");
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
