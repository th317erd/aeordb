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
) -> Option<Result<serde_json::Value, String>> {
  corrected_parser(mime_essence, extension).map(|parser| parser(data, filename, stored_content_type, size))
}

pub(crate) fn native_parser_claims_corrected(mime_essence: Option<&str>, extension: Option<&str>) -> bool {
  corrected_parser(mime_essence, extension).is_some()
}

fn corrected_parser(mime_essence: Option<&str>, extension: Option<&str>) -> Option<ParserFn> {
  if let Some(essence) = mime_essence {
    if let Some(parser) = parser_for_content_type(essence) {
      return Some(parser);
    }
    if essence != "application/octet-stream" {
      return None;
    }
  }
  match extension {
    Some(extension) => parser_for_extension(extension),
    None => None,
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
