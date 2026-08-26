use aeordb::engine::v4::locator_range::{
  ExactSourceRangeLimitsV1, ExactSourceRangeSelectorV1, LocatorMatchSemanticsV1, LocatorRangeErrorClassV1, LocatorScanLimitsV1,
  LocatorScanStopReasonV1, locate_source_matches_v1, read_exact_source_range_v1,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OracleLinePoint {
  line: u64,
  column: u64,
}

fn oracle_line_point(text: &str, target_byte: usize) -> Option<OracleLinePoint> {
  if !text.is_char_boundary(target_byte)
    || (target_byte > 0 && target_byte < text.len() && text.as_bytes()[target_byte - 1] == b'\r' && text.as_bytes()[target_byte] == b'\n')
  {
    return None;
  }
  let mut line = 1u64;
  let mut column = 0u64;
  let mut characters = text[..target_byte].chars().peekable();
  while let Some(character) = characters.next() {
    match character {
      '\r' => {
        if characters.peek() == Some(&'\n') {
          characters.next();
        }
        line += 1;
        column = 0;
      }
      '\n' => {
        line += 1;
        column = 0;
      }
      _ => column += 1,
    }
  }
  Some(OracleLinePoint { line, column })
}

fn oracle_nonoverlapping_matches(source: &[u8], needle: &[u8], ascii_insensitive: bool) -> Vec<(u64, u64)> {
  let mut matches = Vec::new();
  let mut start = 0usize;
  while start.saturating_add(needle.len()) <= source.len() {
    let candidate = &source[start..start + needle.len()];
    let matched = if ascii_insensitive { candidate.eq_ignore_ascii_case(needle) } else { candidate == needle };
    if matched {
      matches.push((start as u64, (start + needle.len()) as u64));
      start += needle.len();
    } else {
      start += 1;
    }
  }
  matches
}

fn byte_ranges(scan: &aeordb::engine::v4::locator_range::LocatorScanV1) -> Vec<(u64, u64)> {
  scan.matches().iter().map(|located| (located.byte_range().start(), located.byte_range().end())).collect()
}

#[test]
fn locator_scan_reports_exact_byte_scalar_and_crlf_line_coordinates() {
  let source = "first\r\nCAFÉ needle\nlast";
  let limits = LocatorScanLimitsV1::new(8, 1024, 64).unwrap();
  let scan = locate_source_matches_v1(source.as_bytes(), b"needle", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap();

  assert_eq!(scan.stop_reason(), LocatorScanStopReasonV1::Complete);
  assert!(scan.continuation().is_none());
  assert_eq!(byte_ranges(&scan), vec![(13, 19)]);
  let located = &scan.matches()[0];
  assert_eq!(located.matching_semantics(), LocatorMatchSemanticsV1::ExactBytes);
  let scalars = located.unicode_scalar_range().unwrap();
  assert_eq!((scalars.start(), scalars.end()), (12, 18));
  let lines = located.line_column_range().unwrap();
  assert_eq!((lines.start().line(), lines.start().column()), (2, 5));
  assert_eq!((lines.end().line(), lines.end().column()), (2, 11));
}

#[test]
fn matching_semantics_binary_text_metadata_and_crlf_split_are_explicit() {
  let limits = LocatorScanLimitsV1::new(8, 1024, 64).unwrap();
  let source = b"Needle nEEdLe";
  let exact = locate_source_matches_v1(source, b"needle", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap();
  assert!(exact.matches().is_empty());
  let insensitive = locate_source_matches_v1(source, b"needle", LocatorMatchSemanticsV1::AsciiCaseInsensitiveBytes, 0, limits).unwrap();
  assert_eq!(byte_ranges(&insensitive), vec![(0, 6), (7, 13)]);

  let invalid = locate_source_matches_v1(b"a\xffneedle", b"needle", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap();
  assert_eq!(byte_ranges(&invalid), vec![(2, 8)]);
  assert!(invalid.matches()[0].unicode_scalar_range().is_none());
  assert!(invalid.matches()[0].line_column_range().is_none());

  let newline = locate_source_matches_v1(b"a\r\nb", b"\r\n", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap();
  let newline_match = &newline.matches()[0];
  assert_eq!((newline_match.unicode_scalar_range().unwrap().start(), newline_match.unicode_scalar_range().unwrap().end()), (1, 3),);
  let line_range = newline_match.line_column_range().unwrap();
  assert_eq!((line_range.start().line(), line_range.start().column()), (1, 1));
  assert_eq!((line_range.end().line(), line_range.end().column()), (2, 0));

  let split = locate_source_matches_v1(b"a\r\nb", b"\n", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap();
  assert_eq!(
    (split.matches()[0].unicode_scalar_range().unwrap().start(), split.matches()[0].unicode_scalar_range().unwrap().end()),
    (2, 3)
  );
  assert!(split.matches()[0].line_column_range().is_none(), "a boundary inside CRLF has no honest logical line point");
}

#[test]
fn locator_continuations_equal_the_independent_unbounded_oracle_without_duplicates() {
  let cases: [(&[u8], &[u8], bool); 3] =
    [(b"12345needle--needle--NEEDLE--tail", b"needle", true), (b"aaaaa", b"aa", false), (b"abababab", b"abab", false)];
  for (source, needle, ascii_insensitive) in cases {
    let matching_semantics =
      if ascii_insensitive { LocatorMatchSemanticsV1::AsciiCaseInsensitiveBytes } else { LocatorMatchSemanticsV1::ExactBytes };
    let expected = oracle_nonoverlapping_matches(source, needle, ascii_insensitive);
    for maximum_scanned_bytes in needle.len()..=needle.len() + 3 {
      for maximum_matches in 1..=3 {
        let mut actual = Vec::new();
        let mut start = 0u64;
        let mut page_count = 0usize;
        loop {
          page_count += 1;
          assert!(page_count <= source.len() + 1, "continuations did not converge for {source:?} / {needle:?}");
          let scan = locate_source_matches_v1(
            source,
            needle,
            matching_semantics,
            start,
            LocatorScanLimitsV1::new(maximum_matches, maximum_scanned_bytes as u64, 64).unwrap(),
          )
          .unwrap();
          actual.extend(byte_ranges(&scan));
          match scan.continuation() {
            Some(continuation) => {
              assert!(continuation.next_candidate_byte() > start, "continuation failed to make forward progress");
              start = continuation.next_candidate_byte();
            }
            None => {
              assert_eq!(scan.stop_reason(), LocatorScanStopReasonV1::Complete);
              break;
            }
          }
        }
        assert_eq!(actual, expected, "paged scan diverged for scan={maximum_scanned_bytes}, matches={maximum_matches}");
      }
    }
  }

  let needle = b"needle";
  let first = locate_source_matches_v1(
    b"needle needle",
    needle,
    LocatorMatchSemanticsV1::ExactBytes,
    0,
    LocatorScanLimitsV1::new(1, 64, 64).unwrap(),
  )
  .unwrap();
  assert_eq!(first.stop_reason(), LocatorScanStopReasonV1::MatchLimit);
  assert_eq!(byte_ranges(&first), vec![(0, 6)]);
  assert_eq!(first.continuation().unwrap().next_candidate_byte(), 6);
  let second = locate_source_matches_v1(
    b"needle needle",
    needle,
    LocatorMatchSemanticsV1::ExactBytes,
    first.continuation().unwrap().next_candidate_byte(),
    LocatorScanLimitsV1::new(1, 64, 64).unwrap(),
  )
  .unwrap();
  assert_eq!(byte_ranges(&second), vec![(7, 13)]);
  assert_eq!(second.stop_reason(), LocatorScanStopReasonV1::Complete);
}

#[test]
fn locator_limits_and_malformed_requests_fail_before_partial_success() {
  for error in [
    LocatorScanLimitsV1::new(0, 1, 1).unwrap_err(),
    LocatorScanLimitsV1::new(1, 0, 1).unwrap_err(),
    LocatorScanLimitsV1::new(1, 1, 0).unwrap_err(),
    LocatorScanLimitsV1::new(usize::MAX, u64::MAX, usize::MAX).unwrap_err(),
  ] {
    assert_eq!(error.class(), LocatorRangeErrorClassV1::InvalidRequest);
  }

  let limits = LocatorScanLimitsV1::new(4, 5, 64).unwrap();
  let error = locate_source_matches_v1(b"a needle", b"needle", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap_err();
  assert_eq!(error.class(), LocatorRangeErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "locator_scan_forward_progress");

  let limits = LocatorScanLimitsV1::new(4, 64, 5).unwrap();
  let error = locate_source_matches_v1(b"needle", b"needle", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap_err();
  assert_eq!(error.class(), LocatorRangeErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "locator_literal_bytes");

  let limits = LocatorScanLimitsV1::new(4, 64, 64).unwrap();
  assert_eq!(
    locate_source_matches_v1(b"needle", b"", LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap_err().class(),
    LocatorRangeErrorClassV1::InvalidRequest,
  );
  assert_eq!(
    locate_source_matches_v1(b"needle", b"needle", LocatorMatchSemanticsV1::ExactBytes, 7, limits).unwrap_err().class(),
    LocatorRangeErrorClassV1::InvalidRequest,
  );
}

#[test]
fn locator_short_remaining_source_completes_without_a_nonprogress_error() {
  let scan =
    locate_source_matches_v1(b"prefixneed", b"needle", LocatorMatchSemanticsV1::ExactBytes, 6, LocatorScanLimitsV1::new(1, 2, 64).unwrap())
      .unwrap();
  assert!(scan.matches().is_empty());
  assert_eq!(scan.stop_reason(), LocatorScanStopReasonV1::Complete);
  assert!(scan.continuation().is_none());
  assert!(scan.scanned_byte_range().is_empty());
  assert_eq!((scan.scanned_byte_range().start(), scan.scanned_byte_range().end()), (6, 6));
}

#[test]
fn locator_and_range_reject_sources_above_the_absolute_protocol_bound_before_scanning() {
  const MAXIMUM_SOURCE_BYTES_V1: usize = 256 * 1_024 * 1_024;
  let mut source = Vec::new();
  source.try_reserve_exact(MAXIMUM_SOURCE_BYTES_V1 + 1).unwrap();
  source.resize(MAXIMUM_SOURCE_BYTES_V1 + 1, 0xff);
  let exact_bound_scan = locate_source_matches_v1(
    &source[..MAXIMUM_SOURCE_BYTES_V1],
    b"x",
    LocatorMatchSemanticsV1::ExactBytes,
    0,
    LocatorScanLimitsV1::new(1, 1, 1).unwrap(),
  )
  .unwrap();
  assert_eq!(exact_bound_scan.stop_reason(), LocatorScanStopReasonV1::ScanByteLimit);
  let exact_bound_range = read_exact_source_range_v1(
    &source[..MAXIMUM_SOURCE_BYTES_V1],
    ExactSourceRangeSelectorV1::Bytes { start: 0, end: Some(1) },
    ExactSourceRangeLimitsV1::new(1).unwrap(),
  )
  .unwrap();
  assert_eq!(exact_bound_range.bytes(), &[0xff]);

  let error = locate_source_matches_v1(&source, b"x", LocatorMatchSemanticsV1::ExactBytes, 0, LocatorScanLimitsV1::new(1, 1, 1).unwrap())
    .unwrap_err();
  assert_eq!(error.class(), LocatorRangeErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "locator_source_bytes");

  let error = read_exact_source_range_v1(
    &source,
    ExactSourceRangeSelectorV1::Bytes { start: 0, end: Some(1) },
    ExactSourceRangeLimitsV1::new(1).unwrap(),
  )
  .unwrap_err();
  assert_eq!(error.class(), LocatorRangeErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "exact_range_source_bytes");
}

#[test]
fn byte_ranges_preserve_binary_bytes_and_return_exact_remaining_continuations() {
  let source = [0x00, 0xff, 0x01, 0x02];
  let first = read_exact_source_range_v1(
    &source,
    ExactSourceRangeSelectorV1::Bytes { start: 1, end: Some(4) },
    ExactSourceRangeLimitsV1::new(2).unwrap(),
  )
  .unwrap();
  assert_eq!(first.bytes(), &[0xff, 0x01]);
  assert_eq!((first.source_byte_range().start(), first.source_byte_range().end()), (1, 3));
  assert!(first.unicode_scalar_range().is_none());
  assert!(first.line_column_range().is_none());
  assert!(first.truncated());
  let remaining = first.continuation().unwrap().remaining_byte_range();
  assert_eq!((remaining.start(), remaining.end()), (3, 4));

  let second = read_exact_source_range_v1(
    &source,
    ExactSourceRangeSelectorV1::Bytes { start: remaining.start(), end: Some(remaining.end()) },
    ExactSourceRangeLimitsV1::new(2).unwrap(),
  )
  .unwrap();
  let mut combined = first.bytes().to_vec();
  combined.extend_from_slice(second.bytes());
  assert_eq!(combined, source[1..4]);
  assert!(!second.truncated());
}

#[test]
fn unicode_scalar_ranges_never_split_utf8_and_continue_as_exact_bytes() {
  let source = "aé日b".as_bytes();
  let complete = read_exact_source_range_v1(
    source,
    ExactSourceRangeSelectorV1::UnicodeScalars { start: 1, end: Some(3) },
    ExactSourceRangeLimitsV1::new(32).unwrap(),
  )
  .unwrap();
  assert_eq!(complete.bytes(), "é日".as_bytes());
  assert_eq!((complete.source_byte_range().start(), complete.source_byte_range().end()), (1, 6));
  assert_eq!((complete.unicode_scalar_range().unwrap().start(), complete.unicode_scalar_range().unwrap().end()), (1, 3));
  let lines = complete.line_column_range().unwrap();
  assert_eq!((lines.start().line(), lines.start().column(), lines.end().line(), lines.end().column()), (1, 1, 1, 3));

  let truncated = read_exact_source_range_v1(
    source,
    ExactSourceRangeSelectorV1::UnicodeScalars { start: 1, end: Some(3) },
    ExactSourceRangeLimitsV1::new(3).unwrap(),
  )
  .unwrap();
  assert_eq!(truncated.bytes(), "é".as_bytes());
  assert_eq!((truncated.source_byte_range().start(), truncated.source_byte_range().end()), (1, 3));
  assert_eq!(
    (truncated.continuation().unwrap().remaining_byte_range().start(), truncated.continuation().unwrap().remaining_byte_range().end()),
    (3, 6)
  );

  let error = read_exact_source_range_v1(
    source,
    ExactSourceRangeSelectorV1::UnicodeScalars { start: 1, end: Some(2) },
    ExactSourceRangeLimitsV1::new(1).unwrap(),
  )
  .unwrap_err();
  assert_eq!(error.class(), LocatorRangeErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "exact_range_forward_progress");
}

#[test]
fn inclusive_line_ranges_preserve_mixed_endings_and_never_split_crlf() {
  let source = b"one\r\ntwo\nthree\rfour";
  let selected = read_exact_source_range_v1(
    source,
    ExactSourceRangeSelectorV1::LinesInclusive { start: 2, end: Some(3) },
    ExactSourceRangeLimitsV1::new(1024).unwrap(),
  )
  .unwrap();
  assert_eq!(selected.bytes(), b"two\nthree\r");
  assert_eq!((selected.source_byte_range().start(), selected.source_byte_range().end()), (5, 15));
  let coordinates = selected.line_column_range().unwrap();
  assert_eq!((coordinates.start().line(), coordinates.start().column()), (2, 0));
  assert_eq!((coordinates.end().line(), coordinates.end().column()), (4, 0));

  let crlf = read_exact_source_range_v1(
    source,
    ExactSourceRangeSelectorV1::LinesInclusive { start: 1, end: Some(1) },
    ExactSourceRangeLimitsV1::new(4).unwrap(),
  )
  .unwrap();
  assert_eq!(crlf.bytes(), b"one");
  assert_eq!(
    (crlf.continuation().unwrap().remaining_byte_range().start(), crlf.continuation().unwrap().remaining_byte_range().end()),
    (3, 5)
  );

  let complete_crlf = read_exact_source_range_v1(
    source,
    ExactSourceRangeSelectorV1::LinesInclusive { start: 1, end: Some(1) },
    ExactSourceRangeLimitsV1::new(5).unwrap(),
  )
  .unwrap();
  assert_eq!(complete_crlf.bytes(), b"one\r\n");
  assert!(!complete_crlf.truncated());

  let final_empty_line = read_exact_source_range_v1(
    b"a\n",
    ExactSourceRangeSelectorV1::LinesInclusive { start: 2, end: Some(2) },
    ExactSourceRangeLimitsV1::new(8).unwrap(),
  )
  .unwrap();
  assert!(final_empty_line.bytes().is_empty());
  assert_eq!((final_empty_line.source_byte_range().start(), final_empty_line.source_byte_range().end()), (2, 2));

  let mid_line = read_exact_source_range_v1(
    b"abcdefgh\nsecond",
    ExactSourceRangeSelectorV1::LinesInclusive { start: 1, end: Some(1) },
    ExactSourceRangeLimitsV1::new(4).unwrap(),
  )
  .unwrap();
  assert_eq!(mid_line.bytes(), b"abcd");
  let remaining = mid_line.continuation().unwrap().remaining_byte_range();
  assert_eq!((remaining.start(), remaining.end()), (4, 9));
  let rest = read_exact_source_range_v1(
    b"abcdefgh\nsecond",
    ExactSourceRangeSelectorV1::Bytes { start: remaining.start(), end: Some(remaining.end()) },
    ExactSourceRangeLimitsV1::new(16).unwrap(),
  )
  .unwrap();
  let mut reconstructed = mid_line.bytes().to_vec();
  reconstructed.extend_from_slice(rest.bytes());
  assert_eq!(reconstructed, b"abcdefgh\n");
}

#[test]
fn range_validation_distinguishes_binary_reads_from_invalid_utf8_text_requests() {
  let limits = ExactSourceRangeLimitsV1::new(64).unwrap();
  assert_eq!(ExactSourceRangeLimitsV1::new(0).unwrap_err().class(), LocatorRangeErrorClassV1::InvalidRequest);
  assert_eq!(ExactSourceRangeLimitsV1::new(u64::MAX).unwrap_err().class(), LocatorRangeErrorClassV1::InvalidRequest);

  for selector in
    [ExactSourceRangeSelectorV1::UnicodeScalars { start: 0, end: None }, ExactSourceRangeSelectorV1::LinesInclusive { start: 1, end: None }]
  {
    let error = read_exact_source_range_v1(b"a\xffb", selector, limits).unwrap_err();
    assert_eq!(error.class(), LocatorRangeErrorClassV1::InvalidUtf8);
  }

  for selector in [
    ExactSourceRangeSelectorV1::Bytes { start: 2, end: Some(1) },
    ExactSourceRangeSelectorV1::Bytes { start: 4, end: None },
    ExactSourceRangeSelectorV1::UnicodeScalars { start: 4, end: None },
    ExactSourceRangeSelectorV1::UnicodeScalars { start: 2, end: Some(1) },
    ExactSourceRangeSelectorV1::LinesInclusive { start: 0, end: None },
    ExactSourceRangeSelectorV1::LinesInclusive { start: 3, end: None },
    ExactSourceRangeSelectorV1::LinesInclusive { start: 2, end: Some(1) },
  ] {
    let error = read_exact_source_range_v1(b"abc", selector, limits).unwrap_err();
    assert_eq!(error.class(), LocatorRangeErrorClassV1::InvalidRequest, "unexpected error for {selector:?}: {error}");
  }

  for selector in [
    ExactSourceRangeSelectorV1::Bytes { start: 0, end: None },
    ExactSourceRangeSelectorV1::UnicodeScalars { start: 0, end: None },
    ExactSourceRangeSelectorV1::LinesInclusive { start: 1, end: Some(1) },
  ] {
    let empty = read_exact_source_range_v1(b"", selector, limits).unwrap();
    assert!(empty.bytes().is_empty());
    assert!(!empty.truncated());
  }

  let split_scalar =
    read_exact_source_range_v1("aéb".as_bytes(), ExactSourceRangeSelectorV1::Bytes { start: 2, end: Some(3) }, limits).unwrap();
  assert_eq!(split_scalar.bytes(), &[0xa9]);
  assert!(split_scalar.unicode_scalar_range().is_none());
  assert!(split_scalar.line_column_range().is_none());
}

#[test]
fn independent_utf8_crlf_oracle_agrees_for_every_valid_match_boundary() {
  let sources = ["", "plain needle", "é\r\nneedle\n日needle\rend", "a\r\nb", "needle needle"];
  let needles: [&[u8]; 4] = [b"needle", b"\r\n", b"\n", "日".as_bytes()];
  let limits = LocatorScanLimitsV1::new(64, 4096, 64).unwrap();
  for source in sources {
    for needle in needles {
      let scan = locate_source_matches_v1(source.as_bytes(), needle, LocatorMatchSemanticsV1::ExactBytes, 0, limits).unwrap();
      assert_eq!(byte_ranges(&scan), oracle_nonoverlapping_matches(source.as_bytes(), needle, false));
      for located in scan.matches() {
        let start = located.byte_range().start() as usize;
        let end = located.byte_range().end() as usize;
        let expected_scalars = (source[..start].chars().count() as u64, source[..end].chars().count() as u64);
        let actual_scalars = located.unicode_scalar_range().unwrap();
        assert_eq!((actual_scalars.start(), actual_scalars.end()), expected_scalars);
        match (oracle_line_point(source, start), oracle_line_point(source, end)) {
          (Some(expected_start), Some(expected_end)) => {
            let actual = located.line_column_range().unwrap();
            assert_eq!((actual.start().line(), actual.start().column()), (expected_start.line, expected_start.column));
            assert_eq!((actual.end().line(), actual.end().column()), (expected_end.line, expected_end.column));
          }
          _ => assert!(located.line_column_range().is_none()),
        }
      }
    }
  }
}

#[test]
fn v4_locator_range_contract_is_storage_neutral_and_unique() {
  let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/engine/v4/locator_range.rs")).unwrap();
  assert_eq!(source.matches("pub fn locate_source_matches_v1(").count(), 1);
  assert_eq!(source.matches("pub fn read_exact_source_range_v1<'source>(").count(), 1);
  assert!(source.contains("bytes: &'source [u8]"), "exact range output must borrow source bytes");
  assert!(source.contains("source: &'source [u8]"), "exact range input must bind the returned slice lifetime");
  assert!(source.contains("Result<ExactSourceRangeV1<'source>, LocatorRangeErrorV1>"));
  assert!(!source.contains("bytes: Vec<u8>"), "exact range output must not allocate or copy selected bytes");
  for forbidden in
    ["StorageEngine", "DirectoryOps", "range_extract", "search_locators", "serde", "wasm", "plugin", "head_hash", "FileRecord"]
  {
    assert!(!source.contains(forbidden), "storage-neutral locator/range contract imported forbidden authority: {forbidden}");
  }
}
