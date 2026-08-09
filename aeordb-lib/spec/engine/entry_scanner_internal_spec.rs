use super::*;
use crate::engine::append_writer::AppendWriter;

#[test]
fn rebuild_scan_classifies_cancellation_as_fatal() {
  let temp = tempfile::tempdir().unwrap();
  let path = temp.path().join("cancelled-rebuild-scan.aeordb");
  let mut writer = AppendWriter::create(&path).unwrap();
  writer.append_entry(EntryType::Chunk, &[0xA7; 32], b"payload", 0).unwrap();
  let cancellation = Arc::new(AtomicBool::new(true));
  let mut scanner = EntryScanner::new_reporting_to(File::open(path).unwrap(), writer.current_offset(), Some(cancellation)).unwrap();

  assert!(matches!(scanner.next_rebuild_entry(), Some(Err(RebuildScanError::Fatal(EngineError::ShuttingDown)))));
}
