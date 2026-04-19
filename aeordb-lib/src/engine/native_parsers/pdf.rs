/// Native PDF parser.
///
/// Ported from `aeordb-plugin-parser-pdf`.



/// PDF magic bytes that must appear at the start of every valid PDF.
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Parse a PDF file, extracting text (best-effort) and metadata from the
/// Info dictionary.
///
/// PDF text extraction is inherently complex (fonts, CMap tables, glyph
/// encodings, etc.). This parser handles simple/uncompressed PDFs and
/// always falls back to returning metadata when text extraction fails or
/// produces garbage. The Info dictionary metadata (title, author, dates,
/// etc.) is the primary value here.
pub fn parse(data: &[u8], filename: &str, content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    if data.len() < PDF_MAGIC.len() {
        return Err("file too small to be a valid PDF".to_string());
    }

    if !data.starts_with(PDF_MAGIC) {
        return Err("not a PDF file (missing %PDF- magic)".to_string());
    }

    let version = extract_version(&data);
    let page_count = count_pages(&data);

    // Parse the Info dictionary for structured metadata
    let info = extract_info_dictionary(&data);

    // Best-effort text extraction from stream objects
    let extracted_text = extract_text(&data);

    // Use /Title from Info dict if available, otherwise use filename
    let title = info.title.clone().unwrap_or_default();

    Ok(serde_json::json!({
        "text": extracted_text,
        "title": title,
        "metadata": {
            "filename": filename,
            "content_type": content_type,
            "size": size,
            "format": "pdf",
            "version": version,
            "page_count": page_count,
            "author": info.author.unwrap_or_default(),
            "subject": info.subject.unwrap_or_default(),
            "keywords": info.keywords.unwrap_or_default(),
            "creator": info.creator.unwrap_or_default(),
            "producer": info.producer.unwrap_or_default(),
            "creation_date": info.creation_date.unwrap_or_default(),
            "mod_date": info.mod_date.unwrap_or_default(),
        }
    }))
}

/// Metadata extracted from a PDF's Info dictionary.
#[derive(Debug, Default)]
struct PdfInfo {
    title: Option<String>,
    author: Option<String>,
    subject: Option<String>,
    keywords: Option<String>,
    creator: Option<String>,
    producer: Option<String>,
    creation_date: Option<String>,
    mod_date: Option<String>,
}

// ---------------------------------------------------------------------------
// Version extraction
// ---------------------------------------------------------------------------

/// Extract the PDF version from the header line (e.g. "%PDF-1.7" -> "1.7").
fn extract_version(data: &[u8]) -> String {
    // The version sits right after "%PDF-" on the first line.
    // Find end of first line.
    let header_end = data.iter()
        .position(|&byte| byte == b'\n' || byte == b'\r')
        .unwrap_or(data.len().min(20));

    let header_slice = &data[PDF_MAGIC.len()..header_end];
    let version_string = String::from_utf8_lossy(header_slice).trim().to_string();

    if version_string.is_empty() {
        "unknown".to_string()
    } else {
        version_string
    }
}

// ---------------------------------------------------------------------------
// Page counting
// ---------------------------------------------------------------------------

/// Count pages by finding `/Type /Page` entries (but NOT `/Type /Pages`).
fn count_pages(data: &[u8]) -> usize {
    let needle = b"/Type /Page";
    let mut count = 0;
    let mut position = 0;

    while position + needle.len() <= data.len() {
        if let Some(found) = find_bytes(&data[position..], needle) {
            let absolute_position = position + found;
            let after_match = absolute_position + needle.len();

            // Make sure this is `/Type /Page` and NOT `/Type /Pages`
            let is_pages = after_match < data.len() && data[after_match] == b's';
            if !is_pages {
                count += 1;
            }

            position = absolute_position + 1;
        } else {
            break;
        }
    }

    count
}

// ---------------------------------------------------------------------------
// Info dictionary extraction
// ---------------------------------------------------------------------------

/// Extract the Info dictionary by locating it through the trailer.
///
/// Strategy:
/// 1. Find `trailer` near the end of the file.
/// 2. Within the trailer dict, find `/Info N 0 R` to get the object number.
/// 3. Find the object `N 0 obj` and parse its dictionary entries.
///
/// Falls back to scanning the entire file for an Info-like dictionary if
/// the trailer approach fails.
fn extract_info_dictionary(data: &[u8]) -> PdfInfo {
    // Try to find the Info object number from the trailer
    if let Some(info_object_number) = find_info_object_number(data) {
        if let Some(info) = parse_info_object(data, info_object_number) {
            return info;
        }
    }

    // Fallback: scan for a dictionary that contains typical Info keys
    scan_for_info_dictionary(data)
}

/// Search for the trailer and extract the `/Info N 0 R` object number.
fn find_info_object_number(data: &[u8]) -> Option<u32> {
    // Trailers are near the end of the file. Search the last 4096 bytes
    // (or the whole file if it's small).
    let search_region_start = data.len().saturating_sub(4096);
    let search_region = &data[search_region_start..];

    let trailer_position = find_bytes(search_region, b"trailer")?;
    let after_trailer = &search_region[trailer_position..];

    // Find /Info within the trailer dict
    let info_position = find_bytes(after_trailer, b"/Info")?;
    let after_info = &after_trailer[info_position + 5..]; // skip "/Info"

    // Parse the indirect reference: skip whitespace, read number, expect "0 R"
    parse_indirect_reference(after_info)
}

/// Parse an indirect reference like " 5 0 R" from the start of the slice.
/// Returns the object number.
fn parse_indirect_reference(data: &[u8]) -> Option<u32> {
    let text = String::from_utf8_lossy(data);
    let trimmed = text.trim_start();

    // Expect: number whitespace number whitespace "R"
    let mut parts = trimmed.splitn(4, char::is_whitespace);
    let object_number_string = parts.next()?;
    let generation_string = parts.next()?;

    let object_number: u32 = object_number_string.parse().ok()?;
    let _generation: u32 = generation_string.parse().ok()?;

    // The next non-whitespace char should be 'R'
    let remaining = parts.next().unwrap_or("");
    let remaining_trimmed = remaining.trim_start();
    if remaining_trimmed.starts_with('R') {
        Some(object_number)
    } else {
        // Try the next part
        if let Some(next) = parts.next() {
            if next.trim_start().starts_with('R') {
                return Some(object_number);
            }
        }
        None
    }
}

/// Find and parse the object with the given number, extracting Info fields.
fn parse_info_object(data: &[u8], object_number: u32) -> Option<PdfInfo> {
    let pattern = format!("{} 0 obj", object_number);
    let pattern_bytes = pattern.as_bytes();

    let position = find_bytes(data, pattern_bytes)?;
    let after_obj = &data[position..];

    // Find the dictionary start
    let dict_start = find_bytes(after_obj, b"<<")?;
    let dict_region = &after_obj[dict_start..];

    // Find the matching ">>" end
    let dict_end = find_dict_end(dict_region)?;
    let dict_content = &dict_region[2..dict_end]; // skip the opening "<<"

    Some(parse_info_fields(dict_content))
}

/// Scan the entire file for a dictionary containing typical Info keys.
/// This is the fallback when the trailer approach fails.
fn scan_for_info_dictionary(data: &[u8]) -> PdfInfo {
    // Look for dictionaries that contain at least /Title or /Author
    let mut position = 0;
    while position + 2 < data.len() {
        if let Some(found) = find_bytes(&data[position..], b"<<") {
            let absolute_position = position + found;
            let dict_region = &data[absolute_position..];

            if let Some(dict_end) = find_dict_end(dict_region) {
                let dict_content = &dict_region[2..dict_end];
                let content_bytes = dict_content;

                // Check if this dictionary looks like an Info dictionary
                let has_title = find_bytes(content_bytes, b"/Title").is_some();
                let has_author = find_bytes(content_bytes, b"/Author").is_some();
                let has_creator = find_bytes(content_bytes, b"/Creator").is_some();
                let has_producer = find_bytes(content_bytes, b"/Producer").is_some();

                if has_title || has_author || has_creator || has_producer {
                    return parse_info_fields(dict_content);
                }
            }

            position = absolute_position + 2;
        } else {
            break;
        }
    }

    PdfInfo::default()
}

/// Parse individual fields from an Info dictionary's content bytes.
fn parse_info_fields(dict_content: &[u8]) -> PdfInfo {
    PdfInfo {
        title: extract_dict_string(dict_content, b"/Title"),
        author: extract_dict_string(dict_content, b"/Author"),
        subject: extract_dict_string(dict_content, b"/Subject"),
        keywords: extract_dict_string(dict_content, b"/Keywords"),
        creator: extract_dict_string(dict_content, b"/Creator"),
        producer: extract_dict_string(dict_content, b"/Producer"),
        creation_date: extract_dict_string(dict_content, b"/CreationDate")
            .map(|date_string| parse_pdf_date(&date_string)),
        mod_date: extract_dict_string(dict_content, b"/ModDate")
            .map(|date_string| parse_pdf_date(&date_string)),
    }
}

/// Find the end of a dictionary that starts with "<<".
/// Handles nested dictionaries.
fn find_dict_end(data: &[u8]) -> Option<usize> {
    let mut depth = 0;
    let mut index = 0;

    while index + 1 < data.len() {
        if data[index] == b'<' && data[index + 1] == b'<' {
            depth += 1;
            index += 2;
        } else if data[index] == b'>' && data[index + 1] == b'>' {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
            index += 2;
        } else {
            index += 1;
        }
    }

    None
}

/// Extract a string value for a given key from a dictionary's raw bytes.
///
/// Handles both `(parenthesized strings)` and `<hex strings>`.
fn extract_dict_string(dict_content: &[u8], key: &[u8]) -> Option<String> {
    let key_position = find_bytes(dict_content, key)?;
    let after_key = &dict_content[key_position + key.len()..];

    // Skip whitespace to find the value start
    let value_start = after_key.iter()
        .position(|&byte| byte != b' ' && byte != b'\t' && byte != b'\n' && byte != b'\r')?;

    let value_region = &after_key[value_start..];

    if value_region.starts_with(b"(") {
        // Parenthesized string
        extract_parenthesized_string(value_region)
    } else if value_region.starts_with(b"<") && !value_region.starts_with(b"<<") {
        // Hex string
        extract_hex_string(value_region)
    } else {
        None
    }
}

/// Extract a parenthesized string like `(Hello World)`.
/// Handles:
/// - Escaped parens: `\(` and `\)`
/// - Escaped backslash: `\\`
/// - Nested balanced parens
/// - Common escape sequences: `\n`, `\r`, `\t`
fn extract_parenthesized_string(data: &[u8]) -> Option<String> {
    if data.is_empty() || data[0] != b'(' {
        return None;
    }

    let mut result = Vec::new();
    let mut depth = 0;
    let mut index = 0;

    while index < data.len() {
        let byte = data[index];

        if byte == b'(' {
            depth += 1;
            if depth > 1 {
                result.push(b'(');
            }
            index += 1;
        } else if byte == b')' {
            depth -= 1;
            if depth == 0 {
                break;
            }
            result.push(b')');
            index += 1;
        } else if byte == b'\\' && index + 1 < data.len() {
            let next = data[index + 1];
            match next {
                b'n' => result.push(b'\n'),
                b'r' => result.push(b'\r'),
                b't' => result.push(b'\t'),
                b'(' => result.push(b'('),
                b')' => result.push(b')'),
                b'\\' => result.push(b'\\'),
                _ => {
                    // Octal escape or unknown — pass through
                    result.push(next);
                }
            }
            index += 2;
        } else {
            result.push(byte);
            index += 1;
        }
    }

    Some(String::from_utf8_lossy(&result).to_string())
}

/// Extract a hex-encoded string like `<48656C6C6F>`.
fn extract_hex_string(data: &[u8]) -> Option<String> {
    if data.is_empty() || data[0] != b'<' {
        return None;
    }

    let end = data.iter().position(|&byte| byte == b'>')?;
    let hex_content = &data[1..end];

    // Strip whitespace from hex content
    let hex_chars: Vec<u8> = hex_content.iter()
        .copied()
        .filter(|&byte| !byte.is_ascii_whitespace())
        .collect();

    // Decode pairs of hex digits
    let mut result = Vec::new();
    let mut index = 0;
    while index + 1 < hex_chars.len() {
        let high = hex_digit_value(hex_chars[index])?;
        let low = hex_digit_value(hex_chars[index + 1])?;
        result.push((high << 4) | low);
        index += 2;
    }

    // If there's an odd trailing nibble, pad with 0
    if index < hex_chars.len() {
        let high = hex_digit_value(hex_chars[index])?;
        result.push(high << 4);
    }

    Some(String::from_utf8_lossy(&result).to_string())
}

/// Convert a hex ASCII digit to its numeric value.
fn hex_digit_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// PDF date parsing
// ---------------------------------------------------------------------------

/// Parse a PDF date string into an ISO 8601 string.
///
/// PDF dates have the format: `D:YYYYMMDDHHmmSS[+HH'mm']`
/// Examples:
///   - `D:20260115103000` -> `2026-01-15T10:30:00`
///   - `D:20260401142200+05'00'` -> `2026-04-01T14:22:00+05:00`
///   - `D:2026` -> `2026-01-01T00:00:00`
///
/// Returns the original string if parsing fails.
fn parse_pdf_date(raw: &str) -> String {
    let date_string = raw.trim();

    // Strip the "D:" prefix if present
    let stripped = if date_string.starts_with("D:") {
        &date_string[2..]
    } else {
        date_string
    };

    if stripped.len() < 4 {
        return date_string.to_string();
    }

    // Extract components with defaults
    let year = &stripped[0..4.min(stripped.len())];
    let month = if stripped.len() >= 6 { &stripped[4..6] } else { "01" };
    let day = if stripped.len() >= 8 { &stripped[6..8] } else { "01" };
    let hour = if stripped.len() >= 10 { &stripped[8..10] } else { "00" };
    let minute = if stripped.len() >= 12 { &stripped[10..12] } else { "00" };
    let second = if stripped.len() >= 14 { &stripped[12..14] } else { "00" };

    // Validate that all components are numeric
    if !year.chars().all(|character| character.is_ascii_digit())
        || !month.chars().all(|character| character.is_ascii_digit())
        || !day.chars().all(|character| character.is_ascii_digit())
        || !hour.chars().all(|character| character.is_ascii_digit())
        || !minute.chars().all(|character| character.is_ascii_digit())
        || !second.chars().all(|character| character.is_ascii_digit())
    {
        return date_string.to_string();
    }

    // Parse timezone if present (after the 14-character date portion)
    let timezone_portion = if stripped.len() > 14 { &stripped[14..] } else { "" };
    let timezone_string = parse_pdf_timezone(timezone_portion);

    if timezone_string.is_empty() {
        format!("{}-{}-{}T{}:{}:{}", year, month, day, hour, minute, second)
    } else {
        format!("{}-{}-{}T{}:{}:{}{}", year, month, day, hour, minute, second, timezone_string)
    }
}

/// Parse the timezone portion of a PDF date string.
///
/// Formats: `+HH'mm'`, `-HH'mm'`, `Z`, or empty.
fn parse_pdf_timezone(timezone_portion: &str) -> String {
    let trimmed = timezone_portion.trim();

    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed == "Z" {
        return "Z".to_string();
    }

    let sign = match trimmed.as_bytes().first() {
        Some(b'+') => "+",
        Some(b'-') => "-",
        _ => return String::new(),
    };

    let rest = &trimmed[1..];

    // Strip trailing/internal quotes: +05'00' -> 05 00
    let cleaned: String = rest.chars()
        .filter(|&character| character.is_ascii_digit())
        .collect();

    if cleaned.len() >= 4 {
        format!("{}{}:{}", sign, &cleaned[0..2], &cleaned[2..4])
    } else if cleaned.len() >= 2 {
        format!("{}{}:00", sign, &cleaned[0..2])
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Text extraction (best-effort)
// ---------------------------------------------------------------------------

/// Extract text content from PDF stream objects.
///
/// This is best-effort only. It finds BT...ET blocks within stream objects
/// and extracts text from Tj, TJ, ', and " operators. Works well for
/// simple/uncompressed PDFs; will produce incomplete or empty results for
/// compressed or complex PDFs.
fn extract_text(data: &[u8]) -> String {
    let mut text_fragments: Vec<String> = Vec::new();

    // Find all stream...endstream regions
    let mut position = 0;
    while position < data.len() {
        if let Some(stream_start) = find_stream_start(&data[position..]) {
            let absolute_start = position + stream_start;

            if let Some(stream_end) = find_bytes(&data[absolute_start..], b"endstream") {
                let stream_content = &data[absolute_start..absolute_start + stream_end];

                // Extract text from BT...ET blocks within the stream
                extract_text_from_stream(stream_content, &mut text_fragments);

                position = absolute_start + stream_end + 9; // skip "endstream"
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // Join fragments, collapsing excessive whitespace
    let joined = text_fragments.join(" ");
    collapse_whitespace(&joined)
}

/// Find the start of a stream's content (right after "stream\n" or "stream\r\n").
fn find_stream_start(data: &[u8]) -> Option<usize> {
    let marker_position = find_bytes(data, b"stream")?;
    let after_marker = marker_position + 6; // "stream".len()

    if after_marker >= data.len() {
        return None;
    }

    // The stream keyword is followed by a single EOL (CR, LF, or CRLF)
    if data[after_marker] == b'\r' {
        if after_marker + 1 < data.len() && data[after_marker + 1] == b'\n' {
            Some(after_marker + 2) // CRLF
        } else {
            Some(after_marker + 1) // CR only
        }
    } else if data[after_marker] == b'\n' {
        Some(after_marker + 1) // LF only
    } else {
        // Not a valid stream start — might be "endstream" or similar
        // Try to find the next one
        if after_marker + 1 < data.len() {
            find_stream_start(&data[after_marker..]).map(|position| after_marker + position)
        } else {
            None
        }
    }
}

/// Extract text from BT...ET blocks within a stream.
fn extract_text_from_stream(stream: &[u8], fragments: &mut Vec<String>) {
    let mut position = 0;

    while position < stream.len() {
        // Find next BT (begin text object)
        if let Some(bt_position) = find_bytes(&stream[position..], b"BT") {
            let absolute_bt = position + bt_position;
            let after_bt = absolute_bt + 2;

            // Find matching ET (end text object)
            if let Some(et_position) = find_bytes(&stream[after_bt..], b"ET") {
                let text_block = &stream[after_bt..after_bt + et_position];
                extract_text_from_block(text_block, fragments);
                position = after_bt + et_position + 2;
            } else {
                break;
            }
        } else {
            break;
        }
    }
}

/// Extract text strings from a single BT...ET text block.
///
/// Looks for text-showing operators:
/// - `(string) Tj` — show string
/// - `[(string) offset (string)] TJ` — show strings with kerning
/// - `(string) '` — move to next line and show string
/// - `(string) "` — set spacing, move to next line, and show string
fn extract_text_from_block(block: &[u8], fragments: &mut Vec<String>) {
    let mut index = 0;

    while index < block.len() {
        if block[index] == b'(' {
            // Parenthesized string
            if let Some(extracted) = extract_parenthesized_string(&block[index..]) {
                if !extracted.trim().is_empty() {
                    fragments.push(extracted);
                }
                // Advance past the closing paren
                if let Some(end) = find_matching_paren(&block[index..]) {
                    index += end + 1;
                } else {
                    index += 1;
                }
            } else {
                index += 1;
            }
        } else if block[index] == b'<' && (index + 1 >= block.len() || block[index + 1] != b'<') {
            // Hex string
            if let Some(extracted) = extract_hex_string(&block[index..]) {
                if !extracted.trim().is_empty() {
                    fragments.push(extracted);
                }
                // Advance past the closing >
                if let Some(end) = block[index..].iter().position(|&byte| byte == b'>') {
                    index += end + 1;
                } else {
                    index += 1;
                }
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
}

/// Find the position of the matching closing parenthesis, respecting nesting
/// and escapes.
fn find_matching_paren(data: &[u8]) -> Option<usize> {
    if data.is_empty() || data[0] != b'(' {
        return None;
    }

    let mut depth = 0;
    let mut index = 0;

    while index < data.len() {
        match data[index] {
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            b'\\' => {
                index += 2; // skip escaped character
            }
            _ => {
                index += 1;
            }
        }
    }

    None
}

/// Collapse runs of whitespace into single spaces and trim.
fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut last_was_whitespace = true; // start true to trim leading

    for character in text.chars() {
        if character.is_whitespace() {
            if !last_was_whitespace {
                result.push(' ');
                last_was_whitespace = true;
            }
        } else {
            result.push(character);
            last_was_whitespace = false;
        }
    }

    // Trim trailing space
    if result.ends_with(' ') {
        result.pop();
    }

    result
}

// ---------------------------------------------------------------------------
// Byte search utilities
// ---------------------------------------------------------------------------

/// Find the first occurrence of `needle` in `haystack`.
/// Returns the byte offset or None.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }

    haystack.windows(needle.len())
        .position(|window| window == needle)
}

// ===========================================================================
// Tests
// ===========================================================================

