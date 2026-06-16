use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::file_name;
use crate::engine::query_engine::{QueryNode, QueryOp};
use crate::engine::storage_engine::StorageEngine;

const DEFAULT_MAX_MATCHES_PER_RESULT: usize = 5;
const HARD_MAX_MATCHES_PER_RESULT: usize = 50;
const DEFAULT_SNIPPET_CHARS: usize = 160;
const HARD_MAX_SNIPPET_CHARS: usize = 4096;
const DEFAULT_MATCH_CONTEXT_LINES: u64 = 2;
const DEFAULT_MAX_LOCATOR_SCAN_BYTES: u64 = 256 * 1024 * 1024;
const HARD_MAX_LOCATOR_SCAN_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LocatorOptionsRequest {
  pub include_matches: Option<bool>,
  pub max_matches_per_result: Option<usize>,
  pub snippet_chars: Option<usize>,
  pub match_context_lines: Option<u64>,
  pub max_locator_scan_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct LocatorOptions {
  pub include_matches: bool,
  pub max_matches_per_result: usize,
  pub snippet_chars: usize,
  pub match_context_lines: u64,
  pub max_locator_scan_bytes: u64,
}

impl LocatorOptions {
  pub fn from_request(request: &LocatorOptionsRequest) -> Self {
    Self {
      include_matches: request.include_matches.unwrap_or(false),
      max_matches_per_result: request
        .max_matches_per_result
        .unwrap_or(DEFAULT_MAX_MATCHES_PER_RESULT)
        .clamp(1, HARD_MAX_MATCHES_PER_RESULT),
      snippet_chars: request.snippet_chars.unwrap_or(DEFAULT_SNIPPET_CHARS).clamp(16, HARD_MAX_SNIPPET_CHARS),
      match_context_lines: request.match_context_lines.unwrap_or(DEFAULT_MATCH_CONTEXT_LINES),
      max_locator_scan_bytes: request
        .max_locator_scan_bytes
        .unwrap_or(DEFAULT_MAX_LOCATOR_SCAN_BYTES)
        .clamp(1, HARD_MAX_LOCATOR_SCAN_BYTES),
    }
  }
}

#[derive(Debug, Clone)]
pub struct LocatorTerm {
  pub field: String,
  pub operator: String,
  pub literal: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHitLocator {
  pub id: String,
  pub query: String,
  pub matched_text: String,
  pub score: f64,
  pub field: String,
  pub operator: String,
  pub source: LocatorSource,
  pub range: LocatorRangeSet,
  pub fetch: LocatorFetchHints,
  pub snippet: LocatorSnippet,
  pub confidence: &'static str,
  pub scan_status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LocatorSource {
  StoredFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<String>,
    encoding: &'static str,
  },
  FieldValue {
    field: String,
    json_pointer: String,
    value_type: &'static str,
  },
  Metadata {
    field: String,
  },
}

#[derive(Debug, Clone, Serialize)]
pub struct LocatorRangeSet {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub byte: Option<ByteRange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub char: Option<CharRange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub line: Option<LineRange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub column: Option<ColumnRange>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ByteRange {
  pub start: u64,
  pub end: u64,
  pub unit: &'static str,
  pub basis: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CharRange {
  pub start: u64,
  pub end: u64,
  pub unit: &'static str,
  pub basis: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct LineRange {
  pub start: u64,
  pub end: u64,
  pub unit: &'static str,
  pub basis: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnRange {
  pub start: u64,
  pub end: u64,
  pub unit: &'static str,
  pub basis: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocatorFetchHints {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub byte_range: Option<SimpleRange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub line_range: Option<SimpleRange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub json_pointer: Option<String>,
  pub preferred: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimpleRange {
  pub start: u64,
  pub end: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocatorSnippet {
  pub text: String,
  pub highlight: Vec<SnippetHighlight>,
  pub truncated_before: bool,
  pub truncated_after: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnippetHighlight {
  pub start: u64,
  pub end: u64,
  pub unit: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocatorGeneration {
  pub matches: Vec<SearchHitLocator>,
  pub matches_truncated: bool,
  pub locator_status: &'static str,
}

pub fn terms_from_query_node(node: &QueryNode) -> Vec<LocatorTerm> {
  let mut terms = Vec::new();
  collect_query_terms(node, &mut terms);
  dedupe_terms(&mut terms);
  terms
}

pub fn broad_query_terms(query: &str, fields: &[String]) -> Vec<LocatorTerm> {
  let mut terms = Vec::new();
  for field in fields {
    terms.push(LocatorTerm { field: canonical_field_name(field).to_string(), operator: "match".to_string(), literal: query.to_string() });
  }
  if terms.is_empty() {
    terms.push(LocatorTerm { field: "@content".to_string(), operator: "match".to_string(), literal: query.to_string() });
  }
  dedupe_terms(&mut terms);
  terms
}

pub fn generate_locators(
  engine: &StorageEngine,
  file_record: &FileRecord,
  terms: &[LocatorTerm],
  options: &LocatorOptions,
) -> LocatorGeneration {
  if !options.include_matches || terms.is_empty() {
    return LocatorGeneration { matches: Vec::new(), matches_truncated: false, locator_status: "unsupported" };
  }

  let mut matches = Vec::new();
  let mut saw_unsupported = false;
  let mut matches_truncated = false;

  for (term_index, term) in terms.iter().enumerate() {
    if matches.len() >= options.max_matches_per_result {
      if term_index < terms.len() {
        matches_truncated = true;
      }
      break;
    }

    if term.literal.is_empty() {
      continue;
    }

    if term.field.starts_with('@') {
      let before = matches.len();
      matches_truncated |= generate_metadata_locators(file_record, term, options, &mut matches);
      if matches.len() == before {
        saw_unsupported = true;
      }
      continue;
    }

    if file_record.total_size > options.max_locator_scan_bytes {
      saw_unsupported = true;
      continue;
    }

    let before = matches.len();
    matches_truncated |= generate_stored_file_locators(engine, file_record, term, options, &mut matches);
    if matches.len() == before {
      saw_unsupported = true;
    }
  }

  for (index, locator) in matches.iter_mut().enumerate() {
    locator.id = format!("m_{:04}", index + 1);
  }

  let locator_status = if matches.is_empty() && saw_unsupported {
    "unsupported"
  } else if saw_unsupported || matches_truncated {
    "partial"
  } else {
    "complete"
  };

  LocatorGeneration { matches, matches_truncated, locator_status }
}

fn collect_query_terms(node: &QueryNode, out: &mut Vec<LocatorTerm>) {
  match node {
    QueryNode::Field(field_query) => {
      if let Some((operator, literal)) = literal_from_query_op(&field_query.operation) {
        out.push(LocatorTerm { field: canonical_field_name(&field_query.field_name).to_string(), operator, literal });
      }
    }
    QueryNode::And(children) | QueryNode::Or(children) => {
      for child in children {
        collect_query_terms(child, out);
      }
    }
    QueryNode::Not(_) => {}
  }
}

fn literal_from_query_op(operation: &QueryOp) -> Option<(String, String)> {
  match operation {
    QueryOp::Eq(bytes) => std::str::from_utf8(bytes).ok().map(|text| ("eq".to_string(), text.to_string())),
    QueryOp::Contains(value) => Some(("contains".to_string(), value.clone())),
    QueryOp::Similar(value, _) => Some(("similar".to_string(), value.clone())),
    QueryOp::Phonetic(value) => Some(("phonetic".to_string(), value.clone())),
    QueryOp::Fuzzy(value, _) => Some(("fuzzy".to_string(), value.clone())),
    QueryOp::Match(value) => Some(("match".to_string(), value.clone())),
    QueryOp::In(values) => values.iter().find_map(|value| std::str::from_utf8(value).ok().map(|text| ("in".to_string(), text.to_string()))),
    QueryOp::Gt(_) | QueryOp::Lt(_) | QueryOp::Between(_, _) => None,
  }
}

fn dedupe_terms(terms: &mut Vec<LocatorTerm>) {
  let mut seen = std::collections::HashSet::new();
  terms.retain(|term| seen.insert((term.field.clone(), term.operator.clone(), term.literal.clone())));
}

fn canonical_field_name(field: &str) -> &str {
  match field {
    "@file_name" => "@filename",
    other => other,
  }
}

fn generate_metadata_locators(
  file_record: &FileRecord,
  term: &LocatorTerm,
  options: &LocatorOptions,
  out: &mut Vec<SearchHitLocator>,
) -> bool {
  let Some(value) = metadata_value(file_record, &term.field) else {
    return false;
  };
  let Some((start, end)) = find_literal_case_insensitive(&value, &term.literal, 0) else {
    return false;
  };

  let snippet = build_snippet(&value, start, end, options.snippet_chars);
  let char_start = value[..start].chars().count() as u64;
  let char_end = char_start + value[start..end].chars().count() as u64;

  out.push(SearchHitLocator {
    id: String::new(),
    query: term.literal.clone(),
    matched_text: value[start..end].to_string(),
    score: 1.0,
    field: term.field.clone(),
    operator: term.operator.clone(),
    source: LocatorSource::Metadata { field: term.field.clone() },
    range: LocatorRangeSet {
      byte: None,
      char: Some(CharRange { start: char_start, end: char_end, unit: "unicode-scalar", basis: "field-value" }),
      line: None,
      column: None,
    },
    fetch: LocatorFetchHints { byte_range: None, line_range: None, json_pointer: None, preferred: "metadata" },
    snippet: snippet.snippet,
    confidence: "exact",
    scan_status: "complete",
  });
  false
}

fn generate_stored_file_locators(
  engine: &StorageEngine,
  file_record: &FileRecord,
  term: &LocatorTerm,
  options: &LocatorOptions,
  out: &mut Vec<SearchHitLocator>,
) -> bool {
  let ops = DirectoryOps::new(engine);
  let Ok(data) = ops.read_file_buffered(&file_record.path) else {
    return false;
  };
  let Ok(text) = std::str::from_utf8(&data) else {
    return false;
  };

  let before_json = out.len();
  let json_truncated = generate_json_field_locators(file_record, term, text, options, out);
  if out.len() > before_json {
    return json_truncated;
  }

  let mut search_from = 0usize;
  while out.len() < options.max_matches_per_result {
    let Some((start, end)) = find_literal_case_insensitive(text, &term.literal, search_from) else {
      return false;
    };
    search_from = end;

    let line_info = line_info_for_byte(text, start);
    let match_char_len = text[start..end].chars().count() as u64;
    let snippet = build_snippet(text, start, end, options.snippet_chars);
    let fetch_line_start = line_info.line.saturating_sub(options.match_context_lines).max(1);
    let fetch_line_end = line_info.line.saturating_add(options.match_context_lines);
    let fetch_byte = snippet.byte_range.clone();

    out.push(SearchHitLocator {
      id: String::new(),
      query: term.literal.clone(),
      matched_text: text[start..end].to_string(),
      score: 1.0,
      field: term.field.clone(),
      operator: term.operator.clone(),
      source: LocatorSource::StoredFile { mime_type: file_record.content_type.clone(), encoding: "utf-8" },
      range: LocatorRangeSet {
        byte: Some(ByteRange { start: start as u64, end: end as u64, unit: "utf8-byte", basis: "stored-file" }),
        char: Some(CharRange {
          start: line_info.global_char,
          end: line_info.global_char + match_char_len,
          unit: "unicode-scalar",
          basis: "stored-file-text",
        }),
        line: Some(LineRange { start: line_info.line, end: line_info.line, unit: "line", basis: "stored-file-text" }),
        column: Some(ColumnRange {
          start: line_info.column,
          end: line_info.column + match_char_len,
          unit: "unicode-scalar",
          basis: "line",
        }),
      },
      fetch: LocatorFetchHints {
        byte_range: Some(SimpleRange { start: fetch_byte.start, end: fetch_byte.end }),
        line_range: Some(SimpleRange { start: fetch_line_start, end: fetch_line_end }),
        json_pointer: None,
        preferred: "line_range",
      },
      snippet: snippet.snippet,
      confidence: "exact",
      scan_status: "complete",
    });

    if out.len() >= options.max_matches_per_result {
      return find_literal_case_insensitive(text, &term.literal, search_from).is_some();
    }
  }
  false
}

fn generate_json_field_locators(
  file_record: &FileRecord,
  term: &LocatorTerm,
  text: &str,
  options: &LocatorOptions,
  out: &mut Vec<SearchHitLocator>,
) -> bool {
  if !is_json_content_type(file_record.content_type.as_deref()) {
    return false;
  }

  let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
    return false;
  };
  let Some(pointer) = json_pointer_for_field(&json, &term.field) else {
    return false;
  };
  let Some(value) = json.pointer(&pointer) else {
    return false;
  };

  let value_text = match value {
    serde_json::Value::String(text) => text.clone(),
    other => match serde_json::to_string(other) {
      Ok(serialized) => serialized,
      Err(_) => return false,
    },
  };

  let mut search_from = 0usize;
  while out.len() < options.max_matches_per_result {
    let Some((start, end)) = find_literal_case_insensitive(&value_text, &term.literal, search_from) else {
      return false;
    };
    search_from = end;

    let snippet = build_snippet(&value_text, start, end, options.snippet_chars);
    let char_start = value_text[..start].chars().count() as u64;
    let char_end = char_start + value_text[start..end].chars().count() as u64;

    out.push(SearchHitLocator {
      id: String::new(),
      query: term.literal.clone(),
      matched_text: value_text[start..end].to_string(),
      score: 1.0,
      field: term.field.clone(),
      operator: term.operator.clone(),
      source: LocatorSource::FieldValue { field: term.field.clone(), json_pointer: pointer.clone(), value_type: json_value_type(value) },
      range: LocatorRangeSet {
        byte: None,
        char: Some(CharRange { start: char_start, end: char_end, unit: "unicode-scalar", basis: "field-value" }),
        line: None,
        column: None,
      },
      fetch: LocatorFetchHints { byte_range: None, line_range: None, json_pointer: Some(pointer.clone()), preferred: "json_pointer" },
      snippet: snippet.snippet,
      confidence: "exact",
      scan_status: "complete",
    });

    if out.len() >= options.max_matches_per_result {
      return find_literal_case_insensitive(&value_text, &term.literal, search_from).is_some();
    }
  }
  false
}

fn is_json_content_type(content_type: Option<&str>) -> bool {
  let Some(content_type) = content_type else {
    return false;
  };
  let mime = content_type.split(';').next().unwrap_or(content_type).trim();
  mime == "application/json" || mime.ends_with("+json")
}

fn json_pointer_for_field(root: &serde_json::Value, field: &str) -> Option<String> {
  if field.starts_with('/') && root.pointer(field).is_some() {
    return Some(field.to_string());
  }

  if root.get(field).is_some() {
    return Some(format!("/{}", escape_json_pointer_segment(field)));
  }

  let pointer = format!("/{}", field.split('.').map(escape_json_pointer_segment).collect::<Vec<String>>().join("/"));
  if root.pointer(&pointer).is_some() {
    Some(pointer)
  } else {
    None
  }
}

fn escape_json_pointer_segment(segment: &str) -> String {
  segment.replace('~', "~0").replace('/', "~1")
}

fn json_value_type(value: &serde_json::Value) -> &'static str {
  match value {
    serde_json::Value::Null => "null",
    serde_json::Value::Bool(_) => "boolean",
    serde_json::Value::Number(_) => "number",
    serde_json::Value::String(_) => "string",
    serde_json::Value::Array(_) => "array",
    serde_json::Value::Object(_) => "object",
  }
}

fn metadata_value(file_record: &FileRecord, field: &str) -> Option<String> {
  match field {
    "@path" => Some(file_record.path.clone()),
    "@filename" => Some(file_name(&file_record.path).unwrap_or("").to_string()),
    "@extension" => {
      let filename = file_name(&file_record.path).unwrap_or("");
      let extension = filename.rsplit('.').next().unwrap_or("");
      Some(if extension == filename { "" } else { extension }.to_string())
    }
    "@content_type" => Some(file_record.content_type.as_deref().unwrap_or("").to_string()),
    "@hash" => Some(file_record.content_hash_hex()),
    "@size" => Some(file_record.total_size.to_string()),
    "@created_at" => Some(file_record.created_at.to_string()),
    "@updated_at" => Some(file_record.updated_at.to_string()),
    _ => None,
  }
}

fn find_literal_case_insensitive(text: &str, literal: &str, from: usize) -> Option<(usize, usize)> {
  if literal.is_empty() || from >= text.len() {
    return None;
  }

  let haystack = text[from..].to_ascii_lowercase();
  let needle = literal.to_ascii_lowercase();
  let relative = haystack.find(&needle)?;
  let start = from + relative;
  let end = start + literal.len();
  if text.is_char_boundary(start) && text.is_char_boundary(end) {
    Some((start, end))
  } else {
    None
  }
}

#[derive(Clone)]
struct SnippetBuild {
  snippet: LocatorSnippet,
  byte_range: SimpleRange,
}

fn build_snippet(text: &str, match_start: usize, match_end: usize, snippet_chars: usize) -> SnippetBuild {
  let before_chars = snippet_chars / 2;
  let after_chars = snippet_chars.saturating_sub(before_chars);
  let snippet_start = move_by_chars(text, match_start, -(before_chars as isize));
  let snippet_end = move_by_chars(text, match_end, after_chars as isize);
  let snippet_text = text[snippet_start..snippet_end].to_string();
  let highlight_start = text[snippet_start..match_start].chars().count() as u64;
  let highlight_end = highlight_start + text[match_start..match_end].chars().count() as u64;

  SnippetBuild {
    snippet: LocatorSnippet {
      text: snippet_text,
      highlight: vec![SnippetHighlight { start: highlight_start, end: highlight_end, unit: "unicode-scalar" }],
      truncated_before: snippet_start > 0,
      truncated_after: snippet_end < text.len(),
    },
    byte_range: SimpleRange { start: snippet_start as u64, end: snippet_end as u64 },
  }
}

fn move_by_chars(text: &str, byte_index: usize, amount: isize) -> usize {
  if amount == 0 {
    return byte_index;
  }

  if amount > 0 {
    let mut index = byte_index;
    for _ in 0..amount {
      if index >= text.len() {
        return text.len();
      }
      let Some(character) = text[index..].chars().next() else {
        return text.len();
      };
      index += character.len_utf8();
    }
    index
  } else {
    let mut index = byte_index;
    for _ in 0..(-amount) {
      if index == 0 {
        return 0;
      }
      let Some((previous_index, _)) = text[..index].char_indices().last() else {
        return 0;
      };
      index = previous_index;
    }
    index
  }
}

struct LineInfo {
  line: u64,
  column: u64,
  global_char: u64,
}

fn line_info_for_byte(text: &str, target: usize) -> LineInfo {
  let mut line = 1u64;
  let mut column = 0u64;
  let mut global_char = 0u64;
  let mut iter = text.char_indices().peekable();

  while let Some((idx, character)) = iter.next() {
    if idx >= target {
      break;
    }
    global_char += 1;
    if character == '\r' {
      if let Some((_, '\n')) = iter.peek().copied() {
        iter.next();
      }
      line += 1;
      column = 0;
    } else if character == '\n' {
      line += 1;
      column = 0;
    } else {
      column += 1;
    }
  }

  LineInfo { line, column, global_char }
}
