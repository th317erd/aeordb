use aeordb_cli::soak_checkpoint::{SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES, SoakCheckpointRecord, visit_soak_checkpoint_records};
use std::fs::OpenOptions;
use std::io::Write;

#[test]
fn checkpoint_append_admission_preserves_every_complete_prefix_at_every_byte_boundary() {
  use aeordb_cli::soak_checkpoint::prepare_soak_checkpoint_append;

  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("restart.tsv");
  // This independent byte oracle includes both newline styles, all operations,
  // tabs in a body and a multibyte character that can be interrupted mid-codepoint.
  let records = "# worker up mode=stress\r\n/old.json\t{}\n!\t/old.json\n+\t/new.json\t雪\tdata\n?\t/new.json\n-\t/new.json\n".as_bytes();
  for boundary in 0..=records.len() {
    let interrupted = &records[..boundary];
    let complete_end = interrupted.iter().rposition(|byte| *byte == b'\n').map_or(0, |position| position + 1);
    std::fs::write(&checkpoint, interrupted).unwrap();
    let mut writer = OpenOptions::new().read(true).write(true).open(&checkpoint).unwrap();
    prepare_soak_checkpoint_append(&mut writer, &checkpoint).unwrap();
    assert_eq!(std::fs::read(&checkpoint).unwrap(), &interrupted[..complete_end], "boundary {boundary}");
    // Repeated admission, including after a previous scan reached EOF, must be idempotent.
    prepare_soak_checkpoint_append(&mut writer, &checkpoint).unwrap();
    writer.write_all(b"# worker up mode=stress\n/after.json\t{}\n").unwrap();
    drop(writer);
    let mut expected = interrupted[..complete_end].to_vec();
    expected.extend_from_slice(b"# worker up mode=stress\n/after.json\t{}\n");
    assert_eq!(std::fs::read(&checkpoint).unwrap(), expected, "boundary {boundary}");
    let summary = visit_soak_checkpoint_records(&checkpoint, |_line, _record| Ok(())).unwrap();
    assert!(!summary.ignored_incomplete_tail, "boundary {boundary}");
  }
}

#[test]
fn checkpoint_append_admission_rejects_completed_damage_without_changing_any_bytes() {
  use aeordb_cli::soak_checkpoint::prepare_soak_checkpoint_append;

  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("damage.tsv");
  for damage in [b"malformed\n".as_slice(), b"!\t\n", b"/bad.json\t\xff\n", b"?\t/a\textra\n"] {
    let mut original = b"/prior.json\t{}\n".to_vec();
    original.extend_from_slice(damage);
    original.extend_from_slice(b"/incomplete");
    std::fs::write(&checkpoint, &original).unwrap();
    let mut writer = OpenOptions::new().read(true).write(true).open(&checkpoint).unwrap();
    let error = prepare_soak_checkpoint_append(&mut writer, &checkpoint).unwrap_err();
    assert!(error.contains("line 2"), "{error}");
    assert_eq!(std::fs::read(&checkpoint).unwrap(), original);
  }
}

#[test]
fn checkpoint_append_admission_preserves_oversized_records_and_accepts_the_exact_limit() {
  use aeordb_cli::soak_checkpoint::prepare_soak_checkpoint_append;

  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("limit.tsv");
  for terminated in [false, true] {
    let mut original = vec![b'x'; SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES + 1];
    original[0] = b'#';
    if terminated {
      original.push(b'\n');
    }
    std::fs::write(&checkpoint, &original).unwrap();
    let mut writer = OpenOptions::new().read(true).write(true).open(&checkpoint).unwrap();
    let error = prepare_soak_checkpoint_append(&mut writer, &checkpoint).unwrap_err();
    assert!(error.contains("record limit"), "{error}");
    assert_eq!(std::fs::read(&checkpoint).unwrap(), original);
  }
  let mut original = vec![b'x'; SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES];
  original[0] = b'#';
  original[SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES - 1] = b'\n';
  std::fs::write(&checkpoint, &original).unwrap();
  let mut writer = OpenOptions::new().read(true).write(true).open(&checkpoint).unwrap();
  prepare_soak_checkpoint_append(&mut writer, &checkpoint).unwrap();
  assert_eq!(std::fs::read(&checkpoint).unwrap(), original);
}

#[test]
fn checkpoint_append_admission_propagates_read_and_truncation_failures() {
  use aeordb_cli::soak_checkpoint::prepare_soak_checkpoint_append;

  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("io-failure.tsv");
  let original = b"/prior.json\t{}\n/incomplete";
  std::fs::write(&checkpoint, original).unwrap();
  {
    let mut unreadable = OpenOptions::new().write(true).open(&checkpoint).unwrap();
    let error = prepare_soak_checkpoint_append(&mut unreadable, &checkpoint).unwrap_err();
    assert!(error.contains("read checkpoint"), "{error}");
  }
  {
    let mut readonly = std::fs::File::open(&checkpoint).unwrap();
    let error = prepare_soak_checkpoint_append(&mut readonly, &checkpoint).unwrap_err();
    assert!(error.contains("truncate incomplete checkpoint tail"), "{error}");
  }
  assert_eq!(std::fs::read(&checkpoint).unwrap(), original);
}

#[test]
fn checkpoint_parser_visits_every_complete_record_kind_and_accepts_crlf() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("records.tsv");
  std::fs::write(
    &checkpoint,
    b"\r\n# worker up\r\n+\t/docs/path-only.txt\r\n+\t/docs/body.txt\tbody\twith-tab\r\n!\t/docs/path-only.txt\r\n?\t/docs/body.txt\r\n-\t/docs/deleted.txt\r\n/docs/legacy.txt\t\r\n",
  )
  .unwrap();

  let mut records = Vec::new();
  let summary = visit_soak_checkpoint_records(&checkpoint, |_line_number, record| {
    let description = match record {
      SoakCheckpointRecord::Comment { text } => format!("comment:{text}"),
      SoakCheckpointRecord::Committed { path, body } => format!("committed:{path}:{body:?}"),
      SoakCheckpointRecord::PendingWrite { path } => format!("pending-write:{path}"),
      SoakCheckpointRecord::PendingDelete { path } => format!("pending-delete:{path}"),
      SoakCheckpointRecord::Deleted { path } => format!("deleted:{path}"),
    };
    records.push(description);
    Ok(())
  })
  .unwrap();

  assert_eq!(summary.complete_lines, 8);
  assert!(!summary.ignored_incomplete_tail);
  assert_eq!(
    records,
    vec![
      "comment:",
      "comment:# worker up",
      "committed:/docs/path-only.txt:None",
      "committed:/docs/body.txt:Some(\"body\\twith-tab\")",
      "pending-write:/docs/path-only.txt",
      "pending-delete:/docs/body.txt",
      "deleted:/docs/deleted.txt",
      "committed:/docs/legacy.txt:Some(\"\")",
    ]
  );
}

#[test]
fn checkpoint_parser_ignores_an_incomplete_invalid_text_tail() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("incomplete-invalid.tsv");
  std::fs::write(&checkpoint, b"+\t/docs/complete.txt\n+\t/docs/incomplete.txt\xff").unwrap();

  let mut paths = Vec::new();
  let summary = visit_soak_checkpoint_records(&checkpoint, |_line_number, record| {
    if let SoakCheckpointRecord::Committed { path, .. } = record {
      paths.push(path.to_string());
    }
    Ok(())
  })
  .unwrap();

  assert_eq!(paths, vec!["/docs/complete.txt"]);
  assert_eq!(summary.complete_lines, 1);
  assert!(summary.ignored_incomplete_tail);
}

#[test]
fn checkpoint_parser_rejects_completed_invalid_text() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("completed-invalid.tsv");
  std::fs::write(&checkpoint, b"+\t/docs/complete.txt\n+\t/docs/invalid.txt\xff\n").unwrap();

  let error = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Ok(())).unwrap_err();
  assert!(error.contains("checkpoint") && error.contains("line 2") && error.contains("UTF-8"), "{error}");
}

#[test]
fn checkpoint_parser_rejects_each_completed_malformed_shape() {
  let temporary = tempfile::tempdir().unwrap();
  let malformed_records = ["garbage\n", "+\t\n", "!\t\n", "?\t/docs/a.txt\textra\n", "-\t\n", "\tbody\n"];

  for (index, malformed_record) in malformed_records.iter().enumerate() {
    let checkpoint = temporary.path().join(format!("malformed-{index}.tsv"));
    std::fs::write(&checkpoint, malformed_record).unwrap();
    let error = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Ok(())).unwrap_err();
    assert!(error.contains("malformed checkpoint") && error.contains("line 1"), "record {malformed_record:?}: {error}");
  }
}

#[test]
fn checkpoint_parser_reports_a_missing_file() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("missing.tsv");

  let error = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Ok(())).unwrap_err();
  assert!(error.contains("open checkpoint") && error.contains("missing.tsv"), "{error}");
}

#[test]
fn checkpoint_parser_bounds_completed_and_incomplete_records() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("oversized.tsv");
  let mut maximum = vec![b'x'; SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES];
  maximum[0] = b'#';
  maximum[SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES - 1] = b'\n';
  std::fs::write(&checkpoint, maximum).unwrap();
  let maximum_summary = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Ok(())).unwrap();
  assert_eq!(maximum_summary.complete_lines, 1);

  let mut oversized = vec![b'x'; SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES + 1];

  std::fs::write(&checkpoint, &oversized).unwrap();
  let incomplete_error = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Ok(())).unwrap_err();
  assert!(incomplete_error.contains("record limit"), "{incomplete_error}");

  oversized.push(b'\n');
  std::fs::write(&checkpoint, oversized).unwrap();
  let completed_error = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Ok(())).unwrap_err();
  assert!(completed_error.contains("record limit"), "{completed_error}");
}

#[test]
fn checkpoint_parser_preserves_a_consumer_rejection_with_line_context() {
  let temporary = tempfile::tempdir().unwrap();
  let checkpoint = temporary.path().join("consumer-rejection.tsv");
  std::fs::write(&checkpoint, "+\t/docs/path-only.txt\n").unwrap();

  let error = visit_soak_checkpoint_records(&checkpoint, |_line_number, _record| Err("consumer rejected record".to_string())).unwrap_err();
  assert!(error.contains("line 1") && error.contains("consumer rejected record"), "{error}");
}
