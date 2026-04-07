/// Detect or refine the content type of file data.
///
/// Priority:
/// 1. If the provided content_type is specific (not None, not "application/octet-stream"),
///    use it as-is -- the caller knows best.
/// 2. Otherwise, sniff the file's magic bytes via file-format crate.
/// 3. If sniffing fails, fall back to "application/octet-stream".
pub fn detect_content_type(data: &[u8], provided: Option<&str>) -> String {
    // If caller provided a specific content type, trust it
    if let Some(ct) = provided {
        if ct != "application/octet-stream" && !ct.is_empty() {
            return ct.to_string();
        }
    }

    // Sniff magic bytes
    let format = file_format::FileFormat::from_bytes(data);
    let media_type = format.media_type();

    // file-format returns "application/octet-stream" for unknown formats
    // and "application/x-empty" for empty data. Normalize both to octet-stream
    // unless the UTF-8 text heuristic matches.
    if media_type == "application/octet-stream" || media_type == "application/x-empty" {
        if is_likely_text(data) {
            return "text/plain".to_string();
        }
        return "application/octet-stream".to_string();
    }

    media_type.to_string()
}

/// Simple heuristic: if data is valid UTF-8 and has no control characters
/// (except \n, \r, \t), it's probably text.
fn is_likely_text(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    // Check a sample (first 8KB max)
    let sample = &data[..data.len().min(8192)];
    if std::str::from_utf8(sample).is_err() {
        return false;
    }
    // Check for non-text control characters
    !sample.iter().any(|&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provided_content_type_used() {
        let data = b"not actually json";
        assert_eq!(detect_content_type(data, Some("application/json")), "application/json");
    }

    #[test]
    fn test_octet_stream_triggers_detection() {
        let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = detect_content_type(&png_header, Some("application/octet-stream"));
        assert_eq!(result, "image/png");
    }

    #[test]
    fn test_none_content_type_triggers_detection() {
        let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = detect_content_type(&png_header, None);
        assert_eq!(result, "image/png");
    }

    #[test]
    fn test_jpeg_detection() {
        let jpeg_header = [0xFF, 0xD8, 0xFF, 0xE0];
        let result = detect_content_type(&jpeg_header, None);
        assert!(result.starts_with("image/jpeg"), "got: {}", result);
    }

    #[test]
    fn test_pdf_detection() {
        let pdf_header = b"%PDF-1.4";
        let result = detect_content_type(pdf_header, None);
        assert_eq!(result, "application/pdf");
    }

    #[test]
    fn test_zip_detection() {
        let zip_header = [0x50, 0x4B, 0x03, 0x04];
        let result = detect_content_type(&zip_header, None);
        assert!(result.contains("zip"), "got: {}", result);
    }

    #[test]
    fn test_plain_text_detection() {
        let text = b"Hello, this is plain text content.\nWith multiple lines.\n";
        let result = detect_content_type(text, None);
        assert_eq!(result, "text/plain");
    }

    #[test]
    fn test_json_text_detected_as_text() {
        // JSON has no magic bytes -- detected as text/plain
        let json = b"{\"name\": \"alice\"}";
        let result = detect_content_type(json, None);
        assert!(result == "text/plain" || result == "application/json", "got: {}", result);
    }

    #[test]
    fn test_binary_data_unknown() {
        let binary = vec![0x00, 0x01, 0x02, 0xFF, 0xFE, 0xFD];
        let result = detect_content_type(&binary, None);
        assert_eq!(result, "application/octet-stream");
    }

    #[test]
    fn test_empty_data() {
        let result = detect_content_type(b"", None);
        assert_eq!(result, "application/octet-stream");
    }

    #[test]
    fn test_explicit_type_not_overridden() {
        let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = detect_content_type(&png_header, Some("image/custom"));
        assert_eq!(result, "image/custom");
    }

    #[test]
    fn test_empty_string_triggers_detection() {
        let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = detect_content_type(&png_header, Some(""));
        assert_eq!(result, "image/png");
    }

    // Edge cases for is_likely_text
    #[test]
    fn test_text_with_tabs_and_carriage_returns() {
        let data = b"col1\tcol2\tcol3\r\nval1\tval2\tval3\r\n";
        let result = detect_content_type(data, None);
        assert_eq!(result, "text/plain");
    }

    #[test]
    fn test_binary_with_null_bytes_not_text() {
        let data = b"looks like text\x00but has nulls";
        let result = detect_content_type(data, None);
        // Should NOT be text/plain due to null byte
        assert_eq!(result, "application/octet-stream");
    }

    #[test]
    fn test_non_utf8_binary_not_text() {
        // Invalid UTF-8 sequence
        let data = vec![0x80, 0x81, 0x82, 0x83];
        let result = detect_content_type(&data, None);
        assert_eq!(result, "application/octet-stream");
    }

    #[test]
    fn test_gif_detection() {
        let gif_header = b"GIF89a";
        let result = detect_content_type(gif_header, None);
        assert_eq!(result, "image/gif");
    }

    #[test]
    fn test_whitespace_only_content_type_triggers_detection() {
        // Whitespace-only is not empty but also not a valid MIME type;
        // however our check is != "" and != "application/octet-stream",
        // so whitespace would be trusted. This documents the behavior.
        let png_header = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let result = detect_content_type(&png_header, Some(" "));
        // Whitespace is treated as a provided type (caller's choice)
        assert_eq!(result, " ");
    }
}
