/// Native parser dispatch module.
///
/// Provides built-in parsers for common file formats so they work
/// out of the box without deploying WASM plugins. Unknown content
/// types return `None`, falling through to the WASM plugin system.

mod text;
mod html;
mod image;
mod audio;
mod video;
mod pdf;
mod msoffice;
mod odf;

/// Attempt to parse data using a native parser matched by content type.
///
/// Returns:
/// - `Some(Ok(json))` if a native parser handled it successfully
/// - `Some(Err(msg))` if a native parser claimed it but failed
/// - `None` if no native parser handles this content type (fall through to WASM)
pub fn parse_native(
    data: &[u8],
    content_type: &str,
    filename: &str,
    path: &str,
    size: u64,
) -> Option<Result<serde_json::Value, String>> {
    // Try content-type-based dispatch first
    if let Some(parser) = parser_for_content_type(content_type) {
        return Some(parser(data, filename, content_type, size));
    }

    // Fall back to extension-based dispatch when content type is generic
    if content_type == "application/octet-stream" || content_type.is_empty() {
        let extension = extract_extension(filename)
            .or_else(|| extract_extension(path));

        if let Some(ext) = extension {
            if let Some(parser) = parser_for_extension(ext) {
                return Some(parser(data, filename, content_type, size));
            }
        }
    }

    None
}

type ParserFn = fn(&[u8], &str, &str, u64) -> Result<serde_json::Value, String>;

fn parser_for_content_type(content_type: &str) -> Option<ParserFn> {
    match content_type {
        // Text / code / structured text
        "text/plain" | "text/markdown" | "text/css" | "text/csv"
        | "application/json" | "application/xml" | "application/yaml"
        | "application/javascript" | "text/javascript" => {
            Some(text::parse)
        }
        ct if ct.starts_with("text/x-") => Some(text::parse),

        // HTML / XML
        "text/html" | "text/xml" | "application/xhtml+xml" => Some(html::parse),

        // Images
        "image/jpeg" | "image/png" | "image/gif" | "image/bmp" | "image/webp"
        | "image/tiff" | "image/svg+xml" => {
            Some(image::parse)
        }

        // Audio
        "audio/mpeg" | "audio/mp3" | "audio/wav" | "audio/x-wav"
        | "audio/ogg" | "audio/vorbis" => {
            Some(audio::parse)
        }

        // Video
        "video/mp4" | "video/quicktime" | "video/x-msvideo" | "video/avi"
        | "video/webm" | "video/x-matroska" | "video/x-flv" => {
            Some(video::parse)
        }

        // PDF
        "application/pdf" => Some(pdf::parse),

        // MS Office
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/msword" | "application/vnd.ms-excel" => {
            Some(msoffice::parse)
        }

        // ODF
        "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.spreadsheet" => {
            Some(odf::parse)
        }

        _ => None,
    }
}

fn parser_for_extension(ext: &str) -> Option<ParserFn> {
    match ext {
        // Text / code
        "txt" | "md" | "rs" | "js" | "py" | "ts" | "c" | "h" | "cpp" | "java"
        | "go" | "sh" | "css" | "json" | "yaml" | "yml" | "toml" | "xml" | "sql" => {
            Some(text::parse)
        }

        // HTML
        "html" | "htm" | "xhtml" => Some(html::parse),

        // Images
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tiff" | "tif" | "svg" => {
            Some(image::parse)
        }

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
    let filename = name.rsplit('/').next().unwrap_or(name);
    let dot_position = filename.rfind('.')?;
    let ext = &filename[dot_position + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}
