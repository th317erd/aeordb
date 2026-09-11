use super::*;
use crate::engine::append_writer::AppendWriter;
use crate::engine::hash_algorithm::HashAlgorithm;
use std::io::Write;

#[test]
fn rebuild_scan_classifies_cancellation_as_fatal() {
  let temp = tempfile::tempdir().unwrap();
  let path = temp.path().join("cancelled-rebuild-scan.aeordb");
  let mut writer = AppendWriter::create(&path).unwrap();
  writer.append_entry(EntryType::Chunk, &[0xA7; 32], b"payload", 0).unwrap();
  let cancellation = Arc::new(AtomicBool::new(true));
  let mut scanner = EntryScanner::new_reporting_to(File::open(path).unwrap(), writer.current_offset(), Some(cancellation)).unwrap();

  assert!(matches!(scanner.next_rebuild_entry(None), Some(Err(RebuildScanError::Fatal(EngineError::ShuttingDown)))));
}

#[test]
fn rebuild_hash_checks_void_covered_chunk_keys_and_payloads() {
  for corrupt_key in [false, true] {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("void-covered-chunk.aeordb");
    let mut writer = AppendWriter::create(&path).unwrap();
    let (offset, length) = writer.append_entry(EntryType::Chunk, &[0xA7; 32], &[0x55; 128 * 1024], 0).unwrap();
    writer.sync().unwrap();
    let corruption_offset = if corrupt_key { offset + 63 } else { offset + u64::from(length) - 1 };
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(corruption_offset)).unwrap();
    file.write_all(&[0x31]).unwrap();
    file.sync_all().unwrap();
    let mut voids = VoidManager::new(HashAlgorithm::Blake3_256);
    voids.register_void(offset, length);
    let mut scanner = EntryScanner::new_reporting_to(File::open(&path).unwrap(), writer.current_offset(), None).unwrap();
    assert!(
      matches!(
        scanner.next_rebuild_entry(Some(&voids)),
        Some(Err(RebuildScanError::SkippedHistorical { error: EngineError::CorruptEntry { .. }, .. }))
      ),
      "discarded bytes must not supply unverified key-retirement evidence"
    );
  }
}

#[test]
fn rebuild_keeps_metadata_only_scan_for_chunks_outside_voids() {
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("ordinary-chunk.aeordb");
  let mut writer = AppendWriter::create(&path).unwrap();
  let (offset, length) = writer.append_entry(EntryType::Chunk, &[0xA7; 32], &[0x55; 128 * 1024], 0).unwrap();
  writer.sync().unwrap();
  let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
  file.seek(SeekFrom::Start(offset + u64::from(length) - 1)).unwrap();
  file.write_all(&[0x31]).unwrap();
  file.sync_all().unwrap();
  let mut voids = VoidManager::new(HashAlgorithm::Blake3_256);
  voids.register_void(offset + u64::from(length), 100);
  let mut scanner = EntryScanner::new_reporting_to(File::open(&path).unwrap(), writer.current_offset(), None).unwrap();
  let scanned = scanner.next_rebuild_entry(Some(&voids)).unwrap().unwrap();
  assert_eq!(scanned.offset, offset);
  assert!(!scanned.payload_verified);
  assert!(scanned.value.is_none(), "ordinary rebuild must not materialize large chunk payloads");
}

#[test]
fn rebuild_validates_void_covered_chunks_without_retaining_payloads() {
  let temporary = tempfile::tempdir().unwrap();
  let path = temporary.path().join("valid-void-covered-chunk.aeordb");
  let mut writer = AppendWriter::create(&path).unwrap();
  let (offset, length) = writer.append_entry(EntryType::Chunk, &[0xA7; 32], &[0x55; 128 * 1024], 0).unwrap();
  writer.sync().unwrap();
  for (void_offset, void_length) in [(offset, length), (offset + u64::from(length) - 1, 1)] {
    let mut voids = VoidManager::new(HashAlgorithm::Blake3_256);
    voids.register_void(void_offset, void_length);
    let mut scanner = EntryScanner::new_reporting_to(File::open(&path).unwrap(), writer.current_offset(), None).unwrap();
    let scanned = scanner.next_rebuild_entry(Some(&voids)).unwrap().unwrap();
    assert_eq!(scanned.key, [0xA7; 32]);
    assert!(scanned.payload_verified);
    assert!(scanned.value.is_none());
    assert!(scanned._retained_value_memory.is_none());
  }
}
