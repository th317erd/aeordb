use aeordb_cli::soak_checkpoint::{SOAK_CHECKPOINT_RECORD_MAXIMUM_BYTES, SoakCheckpointRecord, visit_soak_checkpoint_records};

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
