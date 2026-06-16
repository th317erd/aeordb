use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::{DirectoryOps, EngineFileStream};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::storage_engine::StorageEngine;

pub const DEFAULT_RANGE_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const ABSOLUTE_RANGE_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RangeMode {
  Lines,
  Chars,
  Bytes,
  JsonPointer,
}

impl RangeMode {
  pub fn as_str(&self) -> &'static str {
    match self {
      RangeMode::Lines => "lines",
      RangeMode::Chars => "chars",
      RangeMode::Bytes => "bytes",
      RangeMode::JsonPointer => "json_pointer",
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeExtractionRequest {
  pub mode: RangeMode,
  pub start: Option<u64>,
  pub end: Option<u64>,
  pub pointer: Option<String>,
  pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractedRange {
  pub content: String,
  pub content_type: String,
  pub source_size: u64,
  pub mode: RangeMode,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub start: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub end: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub pointer: Option<String>,
  pub truncated: bool,
}

struct ExtractedText {
  text: String,
  truncated: bool,
}

pub fn extract_range_by_path(engine: &StorageEngine, path: &str, request: &RangeExtractionRequest) -> EngineResult<ExtractedRange> {
  let directory_ops = DirectoryOps::new(engine);
  let file_record = directory_ops.get_metadata(path)?.ok_or_else(|| EngineError::NotFound(path.to_string()))?;
  extract_range_from_record(engine, &file_record, request)
}

pub fn extract_range_from_record(
  engine: &StorageEngine,
  file_record: &FileRecord,
  request: &RangeExtractionRequest,
) -> EngineResult<ExtractedRange> {
  let max_bytes = effective_max_bytes(request.max_bytes)?;
  let content_type = file_record.content_type.clone().unwrap_or_else(|| "application/octet-stream".to_string());
  let source_size = file_record.total_size;

  let extracted = match request.mode {
    RangeMode::Lines => {
      let start = request.start.unwrap_or(1);
      let end = request.end;
      if start == 0 {
        return Err(EngineError::InvalidInput("Line ranges are 1-based; start must be at least 1".to_string()));
      }
      if let Some(end) = end {
        if end < start {
          return Err(EngineError::InvalidInput("Range end must be greater than or equal to start".to_string()));
        }
      }
      let stream = EngineFileStream::from_chunk_hashes(file_record.chunk_hashes.clone(), engine)?;
      let extracted = extract_lines_from_stream(stream, start, end, max_bytes)?;
      ExtractedRange {
        content: extracted.text,
        content_type,
        source_size,
        mode: request.mode.clone(),
        start: Some(start),
        end,
        pointer: None,
        truncated: extracted.truncated,
      }
    }
    RangeMode::Chars => {
      let start = request.start.unwrap_or(0);
      let end = request.end;
      if let Some(end) = end {
        if end < start {
          return Err(EngineError::InvalidInput("Range end must be greater than or equal to start".to_string()));
        }
      }
      let stream = EngineFileStream::from_chunk_hashes(file_record.chunk_hashes.clone(), engine)?;
      let extracted = extract_chars_from_stream(stream, start, end, max_bytes)?;
      ExtractedRange {
        content: extracted.text,
        content_type,
        source_size,
        mode: request.mode.clone(),
        start: Some(start),
        end,
        pointer: None,
        truncated: extracted.truncated,
      }
    }
    RangeMode::Bytes => {
      let start = request.start.unwrap_or(0);
      let end = request.end;
      if let Some(end) = end {
        if end < start {
          return Err(EngineError::InvalidInput("Range end must be greater than or equal to start".to_string()));
        }
      }
      let stream = EngineFileStream::from_chunk_hashes(file_record.chunk_hashes.clone(), engine)?;
      let extracted = extract_bytes_from_stream(stream, start, end, max_bytes)?;
      ExtractedRange {
        content: extracted.text,
        content_type,
        source_size,
        mode: request.mode.clone(),
        start: Some(start),
        end,
        pointer: None,
        truncated: extracted.truncated,
      }
    }
    RangeMode::JsonPointer => {
      let pointer =
        request.pointer.as_deref().ok_or_else(|| EngineError::InvalidInput("json_pointer range requires 'pointer'".to_string()))?;
      let directory_ops = DirectoryOps::new(engine);
      let data = directory_ops.read_file_buffered(&file_record.path)?;
      let value: serde_json::Value =
        serde_json::from_slice(&data).map_err(|error| EngineError::JsonParseError(format!("Stored file is not valid JSON: {}", error)))?;
      let selected = value.pointer(pointer).ok_or_else(|| EngineError::InvalidInput(format!("JSON pointer not found: {}", pointer)))?;
      let raw = match selected {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string(other).map_err(|error| EngineError::JsonParseError(error.to_string()))?,
      };
      let (content, truncated) = truncate_utf8_string(&raw, max_bytes);
      ExtractedRange {
        content,
        content_type,
        source_size,
        mode: request.mode.clone(),
        start: None,
        end: None,
        pointer: Some(pointer.to_string()),
        truncated,
      }
    }
  };

  engine.counters().record_read(extracted.content.len() as u64);
  Ok(extracted)
}

fn effective_max_bytes(max_bytes: Option<usize>) -> EngineResult<usize> {
  let max_bytes = max_bytes.unwrap_or(DEFAULT_RANGE_MAX_BYTES);
  if max_bytes == 0 || max_bytes > ABSOLUTE_RANGE_MAX_BYTES {
    return Err(EngineError::InvalidInput(format!("Invalid max_bytes: must be between 1 and {}", ABSOLUTE_RANGE_MAX_BYTES)));
  }
  Ok(max_bytes)
}

fn extract_lines_from_stream(stream: EngineFileStream<'_>, start: u64, end: Option<u64>, max_bytes: usize) -> EngineResult<ExtractedText> {
  let mut text = String::new();
  let mut truncated = false;
  let mut current_line = 1u64;
  let mut pending_cr = false;
  let mut pending_cr_selected = false;

  for_each_utf8_char(stream, |character| {
    if pending_cr {
      if character == '\n' {
        if pending_cr_selected && !push_limited(&mut text, character, max_bytes) {
          truncated = true;
          return Ok(false);
        }
        current_line += 1;
        pending_cr = false;
        return Ok(!end.map(|end| current_line > end).unwrap_or(false));
      }

      current_line += 1;
      pending_cr = false;
      if end.map(|end| current_line > end).unwrap_or(false) {
        return Ok(false);
      }
    }

    let selected = current_line >= start && end.map(|end| current_line <= end).unwrap_or(true);
    if selected && !push_limited(&mut text, character, max_bytes) {
      truncated = true;
      return Ok(false);
    }

    if character == '\r' {
      pending_cr = true;
      pending_cr_selected = selected;
    } else if character == '\n' {
      current_line += 1;
      if end.map(|end| current_line > end).unwrap_or(false) {
        return Ok(false);
      }
    }

    Ok(true)
  })?;

  Ok(ExtractedText { text, truncated })
}

fn extract_chars_from_stream(stream: EngineFileStream<'_>, start: u64, end: Option<u64>, max_bytes: usize) -> EngineResult<ExtractedText> {
  let mut text = String::new();
  let mut truncated = false;
  let mut current_char = 0u64;

  for_each_utf8_char(stream, |character| {
    if end.map(|end| current_char >= end).unwrap_or(false) {
      return Ok(false);
    }
    if current_char >= start {
      if !push_limited(&mut text, character, max_bytes) {
        truncated = true;
        return Ok(false);
      }
    }
    current_char += 1;
    Ok(true)
  })?;

  Ok(ExtractedText { text, truncated })
}

fn extract_bytes_from_stream(stream: EngineFileStream<'_>, start: u64, end: Option<u64>, max_bytes: usize) -> EngineResult<ExtractedText> {
  let mut bytes = Vec::new();
  let mut current_offset = 0u64;
  let mut truncated = false;

  for chunk in stream {
    let chunk = chunk?;
    let chunk_start = current_offset;
    let chunk_end = chunk_start.saturating_add(chunk.len() as u64);
    current_offset = chunk_end;

    if chunk_end <= start {
      continue;
    }
    if end.map(|end| chunk_start >= end).unwrap_or(false) {
      break;
    }

    let overlap_start = start.max(chunk_start);
    let overlap_end = end.unwrap_or(chunk_end).min(chunk_end);
    if overlap_end <= overlap_start {
      continue;
    }

    let relative_start = (overlap_start - chunk_start) as usize;
    let relative_end = (overlap_end - chunk_start) as usize;
    let slice = &chunk[relative_start..relative_end];
    let remaining = max_bytes.saturating_sub(bytes.len());
    if remaining == 0 {
      truncated = true;
      break;
    }

    if slice.len() > remaining {
      bytes.extend_from_slice(&slice[..remaining]);
      truncated = true;
      break;
    }

    bytes.extend_from_slice(slice);
  }

  Ok(ExtractedText { text: String::from_utf8_lossy(&bytes).into_owned(), truncated })
}

fn push_limited(text: &mut String, character: char, max_bytes: usize) -> bool {
  if text.len() + character.len_utf8() > max_bytes {
    return false;
  }
  text.push(character);
  true
}

fn for_each_utf8_char<F>(stream: EngineFileStream<'_>, mut handle: F) -> EngineResult<()>
where
  F: FnMut(char) -> EngineResult<bool>,
{
  let mut pending = Vec::new();

  for chunk in stream {
    let chunk = chunk?;
    pending.extend_from_slice(&chunk);

    loop {
      match std::str::from_utf8(&pending) {
        Ok(valid) => {
          for character in valid.chars() {
            if !handle(character)? {
              return Ok(());
            }
          }
          pending.clear();
          break;
        }
        Err(error) if error.error_len().is_none() => {
          let valid_up_to = error.valid_up_to();
          if valid_up_to > 0 {
            let valid = std::str::from_utf8(&pending[..valid_up_to])
              .map_err(|error| EngineError::InvalidInput(format!("Invalid UTF-8: {}", error)))?;
            for character in valid.chars() {
              if !handle(character)? {
                return Ok(());
              }
            }
          }
          pending = pending[valid_up_to..].to_vec();
          break;
        }
        Err(error) => {
          return Err(EngineError::InvalidInput(format!("Invalid UTF-8: {}", error)));
        }
      }
    }
  }

  if !pending.is_empty() {
    let valid = std::str::from_utf8(&pending).map_err(|error| EngineError::InvalidInput(format!("Invalid UTF-8: {}", error)))?;
    for character in valid.chars() {
      if !handle(character)? {
        break;
      }
    }
  }

  Ok(())
}

fn truncate_utf8_string(text: &str, max_bytes: usize) -> (String, bool) {
  if text.len() <= max_bytes {
    return (text.to_string(), false);
  }

  let mut end = max_bytes;
  while end > 0 && !text.is_char_boundary(end) {
    end -= 1;
  }
  (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::directory_ops::DirectoryOps;
  use crate::engine::request_context::RequestContext;
  use crate::server::create_temp_engine_for_tests;

  #[test]
  fn line_ranges_treat_crlf_as_one_break() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/mixed.txt", b"one\r\ntwo\nthree\rfour", Some("text/plain")).unwrap();

    let request = RangeExtractionRequest { mode: RangeMode::Lines, start: Some(2), end: Some(3), pointer: None, max_bytes: Some(1024) };
    let extracted = extract_range_by_path(&engine, "/mixed.txt", &request).unwrap();
    assert_eq!(extracted.content, "two\nthree\r");
    assert!(!extracted.truncated);
  }

  #[test]
  fn char_ranges_handle_unicode_scalars() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/unicode.txt", "aé日b".as_bytes(), Some("text/plain")).unwrap();

    let request = RangeExtractionRequest { mode: RangeMode::Chars, start: Some(1), end: Some(3), pointer: None, max_bytes: Some(1024) };
    let extracted = extract_range_by_path(&engine, "/unicode.txt", &request).unwrap();
    assert_eq!(extracted.content, "é日");
  }

  #[test]
  fn byte_ranges_allow_lossy_binary_strings() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file_buffered(&ctx, "/bin.dat", &[0x61, 0x62, 0xff, 0x63], Some("application/octet-stream")).unwrap();

    let request = RangeExtractionRequest { mode: RangeMode::Bytes, start: Some(1), end: Some(4), pointer: None, max_bytes: Some(1024) };
    let extracted = extract_range_by_path(&engine, "/bin.dat", &request).unwrap();
    assert_eq!(extracted.content, "b\u{FFFD}c");
  }

  #[test]
  fn json_pointer_returns_selected_value() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops
      .store_file_buffered(
        &ctx,
        "/doc.json",
        br#"{"messages":[{"content":"hello"},{"content":{"nested":true}}]}"#,
        Some("application/json"),
      )
      .unwrap();

    let request = RangeExtractionRequest {
      mode: RangeMode::JsonPointer,
      start: None,
      end: None,
      pointer: Some("/messages/1/content".to_string()),
      max_bytes: Some(1024),
    };
    let extracted = extract_range_by_path(&engine, "/doc.json", &request).unwrap();
    assert_eq!(extracted.content, r#"{"nested":true}"#);
  }
}
