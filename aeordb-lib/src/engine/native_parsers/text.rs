//! Native text/code/markdown parser.
//!
//! Ported from `aeordb-plugin-parser-text`.

//! UTF-8 BOM bytes (0xEF, 0xBB, 0xBF).
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

pub fn parse(data: &[u8], filename: &str, content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    let has_bom = data.starts_with(UTF8_BOM);

    // Strip BOM before decoding so it doesn't pollute the text output
    let raw_bytes = if has_bom {
        &data[UTF8_BOM.len()..]
    } else {
        data
    };

    let text = std::str::from_utf8(raw_bytes)
        .map_err(|e| format!("not valid UTF-8: {}", e))?;

    let is_empty = text.trim().is_empty();
    let title = extract_title(text);
    let line_count = if text.is_empty() { 0 } else { text.lines().count() };
    let word_count = text.split_whitespace().count();
    let char_count = text.chars().count();
    let language = detect_language(filename, content_type);

    Ok(serde_json::json!({
        "text": text,
        "title": title,
        "metadata": {
            "filename": filename,
            "content_type": content_type,
            "size": size,
            "line_count": line_count,
            "word_count": word_count,
            "char_count": char_count,
            "language": language,
            "encoding": "utf-8",
            "is_empty": is_empty,
            "has_bom": has_bom,
        }
    }))
}

/// Extract the first non-empty, trimmed line as the document title.
fn extract_title(text: &str) -> String {
    text.lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Detect the programming/markup language from the file extension or content type.
fn detect_language(filename: &str, content_type: &str) -> &'static str {
    if let Some(language) = language_from_content_type(content_type) {
        return language;
    }
    if let Some(extension) = extension_from_filename(filename) {
        if let Some(language) = language_from_extension(extension) {
            return language;
        }
    }
    "plaintext"
}

fn language_from_content_type(content_type: &str) -> Option<&'static str> {
    match content_type {
        "text/markdown" => Some("markdown"),
        "application/json" => Some("json"),
        "application/xml" => Some("xml"),
        "application/yaml" => Some("yaml"),
        _ => None,
    }
}

fn extension_from_filename(filename: &str) -> Option<&str> {
    let dot_position = filename.rfind('.')?;
    let extension = &filename[dot_position + 1..];
    if extension.is_empty() {
        return None;
    }
    Some(extension)
}

fn language_from_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "md" => Some("markdown"),
        "rs" => Some("rust"),
        "js" | "mjs" => Some("javascript"),
        "py" => Some("python"),
        "ts" => Some("typescript"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "xml" => Some("xml"),
        "sql" => Some("sql"),
        "sh" => Some("shell"),
        "c" | "h" => Some("c"),
        "cpp" | "hpp" => Some("cpp"),
        "java" => Some("java"),
        "go" => Some("go"),
        _ => None,
    }
}
