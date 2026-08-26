use std::error::Error;
use std::fmt;

const MAXIMUM_LOCATOR_MATCHES_V1: usize = 1_024;
const MAXIMUM_SOURCE_BYTES_V1: u64 = 256 * 1_024 * 1_024;
const MAXIMUM_LOCATOR_SCAN_BYTES_V1: u64 = MAXIMUM_SOURCE_BYTES_V1;
const MAXIMUM_LOCATOR_LITERAL_BYTES_V1: usize = 1_024 * 1_024;
const MAXIMUM_EXACT_RANGE_BYTES_V1: u64 = 16 * 1_024 * 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocatorRangeErrorClassV1 {
  InvalidRequest,
  InvalidUtf8,
  ResourceLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocatorRangeErrorV1 {
  class: LocatorRangeErrorClassV1,
  code: &'static str,
  context: String,
}

impl LocatorRangeErrorV1 {
  pub const fn class(&self) -> LocatorRangeErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for LocatorRangeErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for LocatorRangeErrorV1 {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactByteRangeV1 {
  start: u64,
  end: u64,
}

impl ExactByteRangeV1 {
  pub const fn start(&self) -> u64 {
    self.start
  }

  pub const fn end(&self) -> u64 {
    self.end
  }

  pub const fn len(&self) -> u64 {
    self.end - self.start
  }

  pub const fn is_empty(&self) -> bool {
    self.start == self.end
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnicodeScalarRangeV1 {
  start: u64,
  end: u64,
}

impl UnicodeScalarRangeV1 {
  pub const fn start(&self) -> u64 {
    self.start
  }

  pub const fn end(&self) -> u64 {
    self.end
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineColumnPointV1 {
  line: u64,
  column: u64,
}

impl LineColumnPointV1 {
  pub const fn line(&self) -> u64 {
    self.line
  }

  pub const fn column(&self) -> u64 {
    self.column
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineColumnRangeV1 {
  start: LineColumnPointV1,
  end: LineColumnPointV1,
}

impl LineColumnRangeV1 {
  pub const fn start(&self) -> LineColumnPointV1 {
    self.start
  }

  pub const fn end(&self) -> LineColumnPointV1 {
    self.end
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocatorMatchSemanticsV1 {
  ExactBytes,
  AsciiCaseInsensitiveBytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocatorScanLimitsV1 {
  maximum_matches: usize,
  maximum_scanned_bytes: u64,
  maximum_literal_bytes: usize,
}

impl LocatorScanLimitsV1 {
  pub fn new(maximum_matches: usize, maximum_scanned_bytes: u64, maximum_literal_bytes: usize) -> Result<Self, LocatorRangeErrorV1> {
    if maximum_matches == 0
      || maximum_matches > MAXIMUM_LOCATOR_MATCHES_V1
      || maximum_scanned_bytes == 0
      || maximum_scanned_bytes > MAXIMUM_LOCATOR_SCAN_BYTES_V1
      || maximum_literal_bytes == 0
      || maximum_literal_bytes > MAXIMUM_LOCATOR_LITERAL_BYTES_V1
    {
      return Err(invalid("locator_scan_limits", "locator scan limits must be nonzero and remain within protocol maxima"));
    }
    Ok(Self { maximum_matches, maximum_scanned_bytes, maximum_literal_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocatedSourceMatchV1 {
  byte_range: ExactByteRangeV1,
  unicode_scalar_range: Option<UnicodeScalarRangeV1>,
  line_column_range: Option<LineColumnRangeV1>,
  matching_semantics: LocatorMatchSemanticsV1,
}

impl LocatedSourceMatchV1 {
  pub const fn byte_range(&self) -> ExactByteRangeV1 {
    self.byte_range
  }

  pub const fn unicode_scalar_range(&self) -> Option<UnicodeScalarRangeV1> {
    self.unicode_scalar_range
  }

  pub const fn line_column_range(&self) -> Option<LineColumnRangeV1> {
    self.line_column_range
  }

  pub const fn matching_semantics(&self) -> LocatorMatchSemanticsV1 {
    self.matching_semantics
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocatorScanStopReasonV1 {
  Complete,
  MatchLimit,
  ScanByteLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocatorScanContinuationV1 {
  next_candidate_byte: u64,
}

impl LocatorScanContinuationV1 {
  pub const fn next_candidate_byte(&self) -> u64 {
    self.next_candidate_byte
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocatorScanV1 {
  matches: Vec<LocatedSourceMatchV1>,
  scanned_byte_range: ExactByteRangeV1,
  stop_reason: LocatorScanStopReasonV1,
  continuation: Option<LocatorScanContinuationV1>,
}

impl LocatorScanV1 {
  pub fn matches(&self) -> &[LocatedSourceMatchV1] {
    &self.matches
  }

  pub const fn scanned_byte_range(&self) -> ExactByteRangeV1 {
    self.scanned_byte_range
  }

  pub const fn stop_reason(&self) -> LocatorScanStopReasonV1 {
    self.stop_reason
  }

  pub const fn continuation(&self) -> Option<LocatorScanContinuationV1> {
    self.continuation
  }
}

pub fn locate_source_matches_v1(
  source: &[u8],
  literal: &[u8],
  matching_semantics: LocatorMatchSemanticsV1,
  start_byte: u64,
  limits: LocatorScanLimitsV1,
) -> Result<LocatorScanV1, LocatorRangeErrorV1> {
  if literal.is_empty() {
    return Err(invalid("locator_literal_empty", "locator literal must not be empty"));
  }
  if literal.len() > limits.maximum_literal_bytes {
    return Err(resource("locator_literal_bytes", "locator literal exceeds the admitted byte bound"));
  }
  let source_length = u64::try_from(source.len()).map_err(|error| {
    resource("locator_source_platform_size", format!("locator source length does not fit the protocol coordinate: {error}"))
  })?;
  if source_length > MAXIMUM_SOURCE_BYTES_V1 {
    return Err(resource("locator_source_bytes", "locator source exceeds the absolute protocol byte bound"));
  }
  if start_byte > source_length {
    return Err(invalid("locator_scan_start", "locator scan start exceeds the source length"));
  }
  let start = usize::try_from(start_byte)
    .map_err(|error| resource("locator_scan_platform_start", format!("locator scan start does not fit this platform: {error}")))?;
  if source.len().saturating_sub(start) < literal.len() {
    return Ok(LocatorScanV1 {
      matches: Vec::new(),
      scanned_byte_range: exact_byte_range(start, start)?,
      stop_reason: LocatorScanStopReasonV1::Complete,
      continuation: None,
    });
  }
  let maximum_scanned_bytes = usize::try_from(limits.maximum_scanned_bytes)
    .map_err(|error| resource("locator_scan_platform_limit", format!("locator scan byte limit does not fit this platform: {error}")))?;
  let window_end = start.saturating_add(maximum_scanned_bytes).min(source.len());
  if source.len().saturating_sub(start) >= literal.len() && window_end.saturating_sub(start) < literal.len() {
    return Err(resource("locator_scan_forward_progress", "locator scan byte limit cannot examine one complete literal candidate"));
  }

  let prefix = locator_prefix_table(literal, matching_semantics)?;
  let mut matches = Vec::new();
  matches
    .try_reserve_exact(limits.maximum_matches)
    .map_err(|error| resource("locator_match_allocation", format!("cannot reserve locator matches: {error}")))?;
  let mut matched_prefix = 0usize;
  let mut scanned_end = start;

  for (relative_index, source_byte) in source[start..window_end].iter().copied().enumerate() {
    let absolute_end = start + relative_index + 1;
    scanned_end = absolute_end;
    while matched_prefix > 0 && !locator_bytes_equal(source_byte, literal[matched_prefix], matching_semantics) {
      matched_prefix = prefix[matched_prefix - 1];
    }
    if locator_bytes_equal(source_byte, literal[matched_prefix], matching_semantics) {
      matched_prefix += 1;
      if matched_prefix == literal.len() {
        let match_start = absolute_end - literal.len();
        matches.push(LocatedSourceMatchV1 {
          byte_range: exact_byte_range(match_start, absolute_end)?,
          unicode_scalar_range: None,
          line_column_range: None,
          matching_semantics,
        });
        matched_prefix = 0;
        if matches.len() == limits.maximum_matches {
          let has_another_candidate = source.len().saturating_sub(absolute_end) >= literal.len();
          let (stop_reason, continuation) = if has_another_candidate {
            let next_candidate_byte = u64::try_from(absolute_end).map_err(|error| {
              resource("locator_match_continuation", format!("locator match continuation does not fit the protocol coordinate: {error}"))
            })?;
            (LocatorScanStopReasonV1::MatchLimit, Some(LocatorScanContinuationV1 { next_candidate_byte }))
          } else {
            (LocatorScanStopReasonV1::Complete, None)
          };
          attach_text_coordinates(source, &mut matches)?;
          return Ok(LocatorScanV1 { matches, scanned_byte_range: exact_byte_range(start, scanned_end)?, stop_reason, continuation });
        }
      }
    }
  }

  let (stop_reason, continuation) = if window_end == source.len() {
    (LocatorScanStopReasonV1::Complete, None)
  } else {
    let next_candidate = window_end
      .checked_sub(matched_prefix)
      .ok_or_else(|| resource("locator_scan_continuation", "locator scan continuation underflowed the examined byte window"))?;
    if next_candidate <= start {
      return Err(resource("locator_scan_forward_progress", "locator scan continuation did not advance"));
    }
    let next_candidate_byte = u64::try_from(next_candidate).map_err(|error| {
      resource("locator_scan_continuation", format!("locator scan continuation does not fit the protocol coordinate: {error}"))
    })?;
    (LocatorScanStopReasonV1::ScanByteLimit, Some(LocatorScanContinuationV1 { next_candidate_byte }))
  };
  attach_text_coordinates(source, &mut matches)?;
  Ok(LocatorScanV1 { matches, scanned_byte_range: exact_byte_range(start, scanned_end)?, stop_reason, continuation })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactSourceRangeLimitsV1 {
  maximum_output_bytes: u64,
}

impl ExactSourceRangeLimitsV1 {
  pub fn new(maximum_output_bytes: u64) -> Result<Self, LocatorRangeErrorV1> {
    if maximum_output_bytes == 0 || maximum_output_bytes > MAXIMUM_EXACT_RANGE_BYTES_V1 {
      return Err(invalid("exact_range_limits", "exact range output limit must be nonzero and remain within the protocol maximum"));
    }
    Ok(Self { maximum_output_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactSourceRangeSelectorV1 {
  Bytes { start: u64, end: Option<u64> },
  UnicodeScalars { start: u64, end: Option<u64> },
  LinesInclusive { start: u64, end: Option<u64> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactSourceRangeContinuationV1 {
  remaining_byte_range: ExactByteRangeV1,
}

impl ExactSourceRangeContinuationV1 {
  pub const fn remaining_byte_range(&self) -> ExactByteRangeV1 {
    self.remaining_byte_range
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactSourceRangeV1<'source> {
  bytes: &'source [u8],
  source_byte_range: ExactByteRangeV1,
  unicode_scalar_range: Option<UnicodeScalarRangeV1>,
  line_column_range: Option<LineColumnRangeV1>,
  continuation: Option<ExactSourceRangeContinuationV1>,
}

impl ExactSourceRangeV1<'_> {
  pub fn bytes(&self) -> &[u8] {
    self.bytes
  }

  pub const fn source_byte_range(&self) -> ExactByteRangeV1 {
    self.source_byte_range
  }

  pub const fn unicode_scalar_range(&self) -> Option<UnicodeScalarRangeV1> {
    self.unicode_scalar_range
  }

  pub const fn line_column_range(&self) -> Option<LineColumnRangeV1> {
    self.line_column_range
  }

  pub const fn truncated(&self) -> bool {
    self.continuation.is_some()
  }

  pub const fn continuation(&self) -> Option<ExactSourceRangeContinuationV1> {
    self.continuation
  }
}

pub fn read_exact_source_range_v1<'source>(
  source: &'source [u8],
  selector: ExactSourceRangeSelectorV1,
  limits: ExactSourceRangeLimitsV1,
) -> Result<ExactSourceRangeV1<'source>, LocatorRangeErrorV1> {
  let source_length = u64::try_from(source.len()).map_err(|error| {
    resource("exact_range_source_platform_size", format!("exact range source length does not fit the protocol coordinate: {error}"))
  })?;
  if source_length > MAXIMUM_SOURCE_BYTES_V1 {
    return Err(resource("exact_range_source_bytes", "exact range source exceeds the absolute protocol byte bound"));
  }
  let (selected, text_mode, preserve_crlf) = selected_source_byte_range(source, source_length, selector)?;
  let maximum_output_bytes = usize::try_from(limits.maximum_output_bytes)
    .map_err(|error| resource("exact_range_platform_limit", format!("exact range output limit does not fit this platform: {error}")))?;
  let selected_start = usize::try_from(selected.start)
    .map_err(|error| resource("exact_range_platform_start", format!("exact range start does not fit this platform: {error}")))?;
  let selected_end = usize::try_from(selected.end)
    .map_err(|error| resource("exact_range_platform_end", format!("exact range end does not fit this platform: {error}")))?;
  let mut output_end = selected_start.saturating_add(maximum_output_bytes).min(selected_end);
  if text_mode && output_end < selected_end {
    let text = std::str::from_utf8(source).map_err(invalid_utf8)?;
    while output_end > selected_start && !text.is_char_boundary(output_end) {
      output_end -= 1;
    }
    if preserve_crlf && splits_crlf(source, output_end) {
      output_end -= 1;
    }
    if output_end == selected_start && selected_start < selected_end {
      return Err(resource("exact_range_forward_progress", "exact text range output limit cannot retain one complete source character"));
    }
  }

  let bytes = &source[selected_start..output_end];
  let output_range = exact_byte_range(selected_start, output_end)?;
  let (unicode_scalar_range, line_column_range) = text_coordinates(source, output_range)?;
  let continuation = if output_end < selected_end {
    Some(ExactSourceRangeContinuationV1 { remaining_byte_range: exact_byte_range(output_end, selected_end)? })
  } else {
    None
  };
  Ok(ExactSourceRangeV1 { bytes, source_byte_range: output_range, unicode_scalar_range, line_column_range, continuation })
}

fn locator_prefix_table(literal: &[u8], matching_semantics: LocatorMatchSemanticsV1) -> Result<Vec<usize>, LocatorRangeErrorV1> {
  let mut prefix = Vec::new();
  prefix
    .try_reserve_exact(literal.len())
    .map_err(|error| resource("locator_prefix_allocation", format!("cannot reserve locator prefix table: {error}")))?;
  prefix.resize(literal.len(), 0);
  let mut matched = 0usize;
  for index in 1..literal.len() {
    while matched > 0 && !locator_bytes_equal(literal[index], literal[matched], matching_semantics) {
      matched = prefix[matched - 1];
    }
    if locator_bytes_equal(literal[index], literal[matched], matching_semantics) {
      matched += 1;
      prefix[index] = matched;
    }
  }
  Ok(prefix)
}

fn locator_bytes_equal(left: u8, right: u8, matching_semantics: LocatorMatchSemanticsV1) -> bool {
  match matching_semantics {
    LocatorMatchSemanticsV1::ExactBytes => left == right,
    LocatorMatchSemanticsV1::AsciiCaseInsensitiveBytes => left.eq_ignore_ascii_case(&right),
  }
}

fn attach_text_coordinates(source: &[u8], matches: &mut [LocatedSourceMatchV1]) -> Result<(), LocatorRangeErrorV1> {
  let text = match std::str::from_utf8(source) {
    Ok(text) => text,
    Err(error) => {
      if error.valid_up_to() > source.len() {
        return Err(resource("locator_invalid_utf8_boundary", format!("invalid UTF-8 boundary exceeded its source: {error}")));
      }
      return Ok(());
    }
  };
  let mut cursor = TextCoordinateCursorV1::start();
  for located in matches {
    let start = usize::try_from(located.byte_range.start)
      .map_err(|error| resource("locator_coordinate_platform_start", format!("locator byte start does not fit this platform: {error}")))?;
    let end = usize::try_from(located.byte_range.end)
      .map_err(|error| resource("locator_coordinate_platform_end", format!("locator byte end does not fit this platform: {error}")))?;
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
      continue;
    }
    cursor.advance_to(text, start)?;
    let start_scalar = cursor.unicode_scalars;
    let start_point = cursor.line_column_point();
    cursor.advance_to(text, end)?;
    located.unicode_scalar_range = Some(UnicodeScalarRangeV1 { start: start_scalar, end: cursor.unicode_scalars });
    if !splits_crlf(source, start) && !splits_crlf(source, end) {
      located.line_column_range = Some(LineColumnRangeV1 { start: start_point, end: cursor.line_column_point() });
    }
  }
  Ok(())
}

fn selected_source_byte_range(
  source: &[u8],
  source_length: u64,
  selector: ExactSourceRangeSelectorV1,
) -> Result<(ExactByteRangeV1, bool, bool), LocatorRangeErrorV1> {
  match selector {
    ExactSourceRangeSelectorV1::Bytes { start, end } => {
      if start > source_length {
        return Err(invalid("exact_byte_range_start", "exact byte range start exceeds the source length"));
      }
      if end.is_some_and(|end| end < start) {
        return Err(invalid("exact_byte_range_order", "exact byte range end precedes its start"));
      }
      let requested_end = match end {
        Some(end) => end,
        None => source_length,
      };
      Ok((ExactByteRangeV1 { start, end: requested_end.min(source_length) }, false, false))
    }
    ExactSourceRangeSelectorV1::UnicodeScalars { start, end } => {
      if end.is_some_and(|end| end < start) {
        return Err(invalid("exact_unicode_scalar_range_order", "Unicode scalar range end precedes its start"));
      }
      let text = std::str::from_utf8(source).map_err(invalid_utf8)?;
      let scalar_count = u64::try_from(text.chars().count())
        .map_err(|error| resource("exact_unicode_scalar_count", format!("Unicode scalar count overflowed: {error}")))?;
      if start > scalar_count {
        return Err(invalid("exact_unicode_scalar_range_start", "Unicode scalar range start exceeds the source length"));
      }
      let requested_end = match end {
        Some(end) => end,
        None => scalar_count,
      };
      let end = requested_end.min(scalar_count);
      let start_byte = byte_offset_for_unicode_scalar(text, start)?;
      let end_byte = byte_offset_for_unicode_scalar(text, end)?;
      Ok((exact_byte_range(start_byte, end_byte)?, true, false))
    }
    ExactSourceRangeSelectorV1::LinesInclusive { start, end } => {
      if start == 0 {
        return Err(invalid("exact_line_range_start", "line ranges are one-based"));
      }
      if end.is_some_and(|end| end < start) {
        return Err(invalid("exact_line_range_order", "inclusive line range end precedes its start"));
      }
      let text = std::str::from_utf8(source).map_err(invalid_utf8)?;
      let line_count = logical_line_count(text)?;
      if start > line_count {
        return Err(invalid("exact_line_range_start", "line range start exceeds the source line count"));
      }
      let requested_end = match end {
        Some(end) => end,
        None => line_count,
      };
      let end = requested_end.min(line_count);
      let start_byte = logical_line_start_byte(text, start)?;
      let end_byte = if end < line_count { logical_line_start_byte(text, end + 1)? } else { source.len() };
      Ok((exact_byte_range(start_byte, end_byte)?, true, true))
    }
  }
}

fn byte_offset_for_unicode_scalar(text: &str, target: u64) -> Result<usize, LocatorRangeErrorV1> {
  if target == 0 {
    return Ok(0);
  }
  let target = usize::try_from(target).map_err(|error| {
    resource("exact_unicode_scalar_platform_index", format!("Unicode scalar index does not fit this platform: {error}"))
  })?;
  Ok(text.char_indices().nth(target).map_or(text.len(), |(offset, _)| offset))
}

fn logical_line_count(text: &str) -> Result<u64, LocatorRangeErrorV1> {
  let mut count = 1u64;
  let mut characters = text.chars().peekable();
  while let Some(character) = characters.next() {
    if character == '\r' {
      if characters.peek() == Some(&'\n') {
        characters.next();
      }
      count = count.checked_add(1).ok_or_else(|| resource("exact_line_count", "logical line count overflowed"))?;
    } else if character == '\n' {
      count = count.checked_add(1).ok_or_else(|| resource("exact_line_count", "logical line count overflowed"))?;
    }
  }
  Ok(count)
}

fn logical_line_start_byte(text: &str, target_line: u64) -> Result<usize, LocatorRangeErrorV1> {
  if target_line == 1 {
    return Ok(0);
  }
  let mut current_line = 1u64;
  let mut characters = text.char_indices().peekable();
  while let Some((offset, character)) = characters.next() {
    let next_line_start = if character == '\r' {
      if characters.peek().is_some_and(|(_, next)| *next == '\n') {
        let (line_feed_offset, _) = characters.next().ok_or_else(|| resource("exact_line_cursor", "CRLF cursor lost its line feed"))?;
        line_feed_offset + 1
      } else {
        offset + 1
      }
    } else if character == '\n' {
      offset + 1
    } else {
      continue;
    };
    current_line = current_line.checked_add(1).ok_or_else(|| resource("exact_line_count", "logical line count overflowed"))?;
    if current_line == target_line {
      return Ok(next_line_start);
    }
  }
  if current_line == target_line {
    Ok(text.len())
  } else {
    Err(invalid("exact_line_range_start", "line range start exceeds the source line count"))
  }
}

fn text_coordinates(
  source: &[u8],
  byte_range: ExactByteRangeV1,
) -> Result<(Option<UnicodeScalarRangeV1>, Option<LineColumnRangeV1>), LocatorRangeErrorV1> {
  let text = match std::str::from_utf8(source) {
    Ok(text) => text,
    Err(error) => {
      if error.valid_up_to() > source.len() {
        return Err(resource("exact_range_invalid_utf8_boundary", format!("invalid UTF-8 boundary exceeded its source: {error}")));
      }
      return Ok((None, None));
    }
  };
  let start = usize::try_from(byte_range.start)
    .map_err(|error| resource("exact_range_coordinate_start", format!("byte start does not fit this platform: {error}")))?;
  let end = usize::try_from(byte_range.end)
    .map_err(|error| resource("exact_range_coordinate_end", format!("byte end does not fit this platform: {error}")))?;
  if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
    return Ok((None, None));
  }
  let mut cursor = TextCoordinateCursorV1::start();
  cursor.advance_to(text, start)?;
  let start_scalar = cursor.unicode_scalars;
  let start_point = cursor.line_column_point();
  cursor.advance_to(text, end)?;
  let scalar_range = Some(UnicodeScalarRangeV1 { start: start_scalar, end: cursor.unicode_scalars });
  let line_range = if splits_crlf(source, start) || splits_crlf(source, end) {
    None
  } else {
    Some(LineColumnRangeV1 { start: start_point, end: cursor.line_column_point() })
  };
  Ok((scalar_range, line_range))
}

#[derive(Clone, Copy, Debug)]
struct TextCoordinateCursorV1 {
  byte_offset: usize,
  unicode_scalars: u64,
  line: u64,
  column: u64,
  pending_carriage_return: bool,
}

impl TextCoordinateCursorV1 {
  const fn start() -> Self {
    Self { byte_offset: 0, unicode_scalars: 0, line: 1, column: 0, pending_carriage_return: false }
  }

  fn advance_to(&mut self, text: &str, target: usize) -> Result<(), LocatorRangeErrorV1> {
    if target < self.byte_offset || !text.is_char_boundary(target) {
      return Err(resource("text_coordinate_cursor", "text coordinate cursor received a non-monotonic or non-scalar boundary"));
    }
    for character in text[self.byte_offset..target].chars() {
      self.unicode_scalars =
        self.unicode_scalars.checked_add(1).ok_or_else(|| resource("text_coordinate_scalar", "Unicode scalar coordinate overflowed"))?;
      if self.pending_carriage_return {
        self.pending_carriage_return = false;
        if character == '\n' {
          continue;
        }
      }
      if character == '\r' {
        self.line = self.line.checked_add(1).ok_or_else(|| resource("text_coordinate_line", "line coordinate overflowed"))?;
        self.column = 0;
        self.pending_carriage_return = true;
      } else if character == '\n' {
        self.line = self.line.checked_add(1).ok_or_else(|| resource("text_coordinate_line", "line coordinate overflowed"))?;
        self.column = 0;
      } else {
        self.column = self.column.checked_add(1).ok_or_else(|| resource("text_coordinate_column", "column coordinate overflowed"))?;
      }
    }
    self.byte_offset = target;
    Ok(())
  }

  const fn line_column_point(&self) -> LineColumnPointV1 {
    LineColumnPointV1 { line: self.line, column: self.column }
  }
}

fn splits_crlf(source: &[u8], offset: usize) -> bool {
  offset > 0 && offset < source.len() && source[offset - 1] == b'\r' && source[offset] == b'\n'
}

fn exact_byte_range(start: usize, end: usize) -> Result<ExactByteRangeV1, LocatorRangeErrorV1> {
  let start = u64::try_from(start)
    .map_err(|error| resource("byte_range_start", format!("byte range start does not fit the protocol coordinate: {error}")))?;
  let end = u64::try_from(end)
    .map_err(|error| resource("byte_range_end", format!("byte range end does not fit the protocol coordinate: {error}")))?;
  Ok(ExactByteRangeV1 { start, end })
}

fn invalid_utf8(error: std::str::Utf8Error) -> LocatorRangeErrorV1 {
  LocatorRangeErrorV1 { class: LocatorRangeErrorClassV1::InvalidUtf8, code: "exact_range_invalid_utf8", context: error.to_string() }
}

fn invalid(code: &'static str, context: impl Into<String>) -> LocatorRangeErrorV1 {
  LocatorRangeErrorV1 { class: LocatorRangeErrorClassV1::InvalidRequest, code, context: context.into() }
}

fn resource(code: &'static str, context: impl Into<String>) -> LocatorRangeErrorV1 {
  LocatorRangeErrorV1 { class: LocatorRangeErrorClassV1::ResourceLimit, code, context: context.into() }
}
