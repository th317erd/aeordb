//! Native parser dispatch module.
//!
//! Provides built-in parsers for common file formats so they work
//! out of the box without deploying WASM plugins. Unknown content
//! types return `None`, falling through to the WASM plugin system.

mod audio;
pub mod exif;
mod html;
mod image;
mod msoffice;
mod odf;
mod pdf;
mod text;
mod video;

use std::io::{Cursor, Read};

#[derive(Debug)]
pub(crate) enum CorrectedNativeParserErrorV1 {
  Malformed(String),
  PolicyLimit { observed: u64 },
  Host(String),
}

#[derive(Clone, Copy)]
pub(crate) struct CorrectedNativeParserLimitsV1 {
  maximum_expanded_bytes: u64,
  maximum_response_bytes: u64,
  maximum_structure_nodes: u64,
  maximum_scalar_bytes: u64,
  maximum_container_members: u32,
}

impl CorrectedNativeParserLimitsV1 {
  pub(crate) const fn new(
    maximum_expanded_bytes: u64,
    maximum_response_bytes: u64,
    maximum_structure_nodes: u64,
    maximum_scalar_bytes: u64,
    maximum_container_members: u32,
  ) -> Self {
    Self { maximum_expanded_bytes, maximum_response_bytes, maximum_structure_nodes, maximum_scalar_bytes, maximum_container_members }
  }

  pub(super) const fn maximum_expanded_bytes(self) -> u64 {
    self.maximum_expanded_bytes
  }

  pub(super) const fn maximum_response_bytes(self) -> u64 {
    self.maximum_response_bytes
  }

  pub(super) const fn maximum_structure_nodes(self) -> u64 {
    self.maximum_structure_nodes
  }

  pub(super) const fn maximum_scalar_bytes(self) -> u64 {
    self.maximum_scalar_bytes
  }

  pub(super) const fn maximum_container_members(self) -> u32 {
    self.maximum_container_members
  }
}

pub(super) struct ExpandedArchiveBudgetV1 {
  maximum: u64,
  consumed: u64,
}

impl ExpandedArchiveBudgetV1 {
  pub(super) const fn new(maximum: u64) -> Self {
    Self { maximum, consumed: 0 }
  }
}

/// Attempt to parse data using a native parser matched by content type.
///
/// Returns:
/// - `Some(Ok(json))` if a native parser handled it successfully
/// - `Some(Err(msg))` if a native parser claimed it but failed
/// - `None` if no native parser handles this content type (fall through to WASM)
pub fn parse_native(data: &[u8], content_type: &str, filename: &str, path: &str, size: u64) -> Option<Result<serde_json::Value, String>> {
  legacy_parser(content_type, filename, path).map(|parser| parser(data, filename, content_type, size))
}

pub(crate) fn native_parser_claims_legacy(content_type: &str, filename: &str, path: &str) -> bool {
  legacy_parser(content_type, filename, path).is_some()
}

/// Parse through the corrected v1 native routing contract.
///
/// Routing uses a prevalidated MIME essence and ASCII-lowercased extension,
/// while parser-visible metadata retains the exact stored content type.
pub(crate) fn parse_native_corrected(
  data: &[u8],
  mime_essence: Option<&str>,
  extension: Option<&str>,
  filename: &str,
  stored_content_type: &str,
  size: u64,
  limits: CorrectedNativeParserLimitsV1,
) -> Option<Result<serde_json::Value, CorrectedNativeParserErrorV1>> {
  corrected_parser(mime_essence, extension).map(|parser| match parser {
    CorrectedParserV1::Generic(parser) => {
      parser(data, filename, stored_content_type, size).map_err(CorrectedNativeParserErrorV1::Malformed)
    }
    CorrectedParserV1::MsOffice => msoffice::parse_corrected(data, filename, size, limits),
    CorrectedParserV1::Odf => odf::parse_corrected(data, filename, size, limits),
  })
}

pub(crate) fn native_parser_claims_corrected(mime_essence: Option<&str>, extension: Option<&str>) -> bool {
  corrected_parser(mime_essence, extension).is_some()
}

pub(crate) fn native_parser_expands_archive_corrected(mime_essence: Option<&str>, extension: Option<&str>) -> bool {
  matches!(corrected_parser(mime_essence, extension), Some(CorrectedParserV1::MsOffice | CorrectedParserV1::Odf))
}

#[derive(Clone, Copy)]
enum CorrectedParserV1 {
  Generic(ParserFn),
  MsOffice,
  Odf,
}

fn corrected_parser(mime_essence: Option<&str>, extension: Option<&str>) -> Option<CorrectedParserV1> {
  if let Some(essence) = mime_essence {
    if let Some(parser) = corrected_parser_for_content_type(essence) {
      return Some(parser);
    }
    if essence != "application/octet-stream" {
      return None;
    }
  }
  match extension {
    Some(extension) => corrected_parser_for_extension(extension),
    None => None,
  }
}

fn corrected_parser_for_content_type(content_type: &str) -> Option<CorrectedParserV1> {
  match content_type {
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    | "application/msword"
    | "application/vnd.ms-excel" => Some(CorrectedParserV1::MsOffice),
    "application/vnd.oasis.opendocument.text" | "application/vnd.oasis.opendocument.spreadsheet" => Some(CorrectedParserV1::Odf),
    _ => parser_for_content_type(content_type).map(CorrectedParserV1::Generic),
  }
}

fn corrected_parser_for_extension(extension: &str) -> Option<CorrectedParserV1> {
  match extension {
    "docx" | "xlsx" => Some(CorrectedParserV1::MsOffice),
    "odt" | "ods" => Some(CorrectedParserV1::Odf),
    _ => parser_for_extension(extension).map(CorrectedParserV1::Generic),
  }
}

fn legacy_parser(content_type: &str, filename: &str, path: &str) -> Option<ParserFn> {
  if let Some(parser) = parser_for_content_type(content_type) {
    return Some(parser);
  }
  if content_type != "application/octet-stream" && !content_type.is_empty() {
    return None;
  }
  if let Some(extension) = extract_extension(filename) {
    return parser_for_extension(extension);
  }
  match extract_extension(path) {
    Some(extension) => parser_for_extension(extension),
    None => None,
  }
}

type ParserFn = fn(&[u8], &str, &str, u64) -> Result<serde_json::Value, String>;

pub(super) fn read_zip_entry_bounded(
  archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
  name: &str,
  required: bool,
  budget: &mut ExpandedArchiveBudgetV1,
) -> Result<Option<String>, CorrectedNativeParserErrorV1> {
  let mut entry = match archive.by_name(name) {
    Ok(entry) => entry,
    Err(zip::result::ZipError::FileNotFound) if !required => return Ok(None),
    Err(error) => {
      return Err(CorrectedNativeParserErrorV1::Malformed(format!("cannot open ZIP entry {name}: {error}")));
    }
  };
  let remaining = budget.maximum.saturating_sub(budget.consumed);
  let declared = entry.size();
  if declared > remaining {
    return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: budget.consumed.saturating_add(declared) });
  }
  // The policy ceiling is 16 MiB, so every supported target can represent the
  // prechecked entry length as usize.
  let declared = declared as usize;
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(declared)
    .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot reserve bounded ZIP entry {name}: {error}")))?;
  let mut block = [0u8; 16 * 1_024];
  loop {
    let retained = bytes.len() as u64;
    let available = remaining.saturating_sub(retained);
    let read_limit = available.saturating_add(1).min(block.len() as u64) as usize;
    let read = entry
      .read(&mut block[..read_limit])
      .map_err(|error| CorrectedNativeParserErrorV1::Malformed(format!("cannot read ZIP entry {name}: {error}")))?;
    if read == 0 {
      break;
    }
    if read as u64 > available {
      return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: budget.maximum.saturating_add(1) });
    }
    bytes
      .try_reserve(read)
      .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot grow bounded ZIP entry {name}: {error}")))?;
    bytes.extend_from_slice(&block[..read]);
  }
  budget.consumed =
    budget.consumed.checked_add(bytes.len() as u64).ok_or(CorrectedNativeParserErrorV1::PolicyLimit { observed: u64::MAX })?;
  String::from_utf8(bytes)
    .map(Some)
    .map_err(|error| CorrectedNativeParserErrorV1::Malformed(format!("ZIP entry {name} is not UTF-8: {error}")))
}

fn parser_for_content_type(content_type: &str) -> Option<ParserFn> {
  match content_type {
    // Text / code / structured text
    "text/plain"
    | "text/markdown"
    | "text/css"
    | "text/csv"
    | "application/json"
    | "application/xml"
    | "application/yaml"
    | "application/javascript"
    | "text/javascript" => Some(text::parse),
    ct if ct.starts_with("text/x-") => Some(text::parse),

    // HTML / XML
    "text/html" | "text/xml" | "application/xhtml+xml" => Some(html::parse),

    // Images
    "image/jpeg" | "image/png" | "image/gif" | "image/bmp" | "image/webp" | "image/tiff" | "image/svg+xml" => Some(image::parse),

    // Audio
    "audio/mpeg" | "audio/mp3" | "audio/wav" | "audio/x-wav" | "audio/ogg" | "audio/vorbis" => Some(audio::parse),

    // Video
    "video/mp4" | "video/quicktime" | "video/x-msvideo" | "video/avi" | "video/webm" | "video/x-matroska" | "video/x-flv" => {
      Some(video::parse)
    }

    // PDF
    "application/pdf" => Some(pdf::parse),

    // MS Office
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    | "application/msword"
    | "application/vnd.ms-excel" => Some(msoffice::parse),

    // ODF
    "application/vnd.oasis.opendocument.text" | "application/vnd.oasis.opendocument.spreadsheet" => Some(odf::parse),

    _ => None,
  }
}

fn parser_for_extension(ext: &str) -> Option<ParserFn> {
  match ext {
    // Text / code
    "txt" | "md" | "rs" | "js" | "py" | "ts" | "c" | "h" | "cpp" | "java" | "go" | "sh" | "css" | "json" | "yaml" | "yml" | "toml"
    | "xml" | "sql" => Some(text::parse),

    // HTML
    "html" | "htm" | "xhtml" => Some(html::parse),

    // Images
    "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "svg" => Some(image::parse),

    // Audio
    "mp3" | "wav" | "ogg" => Some(audio::parse),

    // Video
    "mp4" | "mov" | "avi" | "webm" | "mkv" | "flv" => Some(video::parse),

    // PDF
    "pdf" => Some(pdf::parse),

    // MS Office
    "docx" | "xlsx" => Some(msoffice::parse),

    // ODF
    "odt" | "ods" => Some(odf::parse),

    _ => None,
  }
}

fn extract_extension(name: &str) -> Option<&str> {
  // Get the filename portion (after last /)
  let filename = match name.rsplit('/').next() {
    Some(filename) => filename,
    None => name,
  };
  let dot_position = filename.rfind('.')?;
  let ext = &filename[dot_position + 1..];
  if ext.is_empty() {
    None
  } else {
    Some(ext)
  }
}

/// Build the shared metadata envelope every native parser starts with.
/// Returns a `serde_json::Value` so callers can mutate it via `["key"] =`.
pub(crate) fn base_metadata(filename: &str, content_type: &str, size: u64) -> serde_json::Value {
  serde_json::json!({
      "filename": filename,
      "content_type": content_type,
      "size": size,
  })
}
