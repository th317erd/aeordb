/// Tests for the native parser dispatch module.
///
/// Verifies:
/// 1. Each parser is dispatched correctly by content type
/// 2. Extension fallback works for application/octet-stream
/// 3. Unknown content types return None (falls through to WASM)
/// 4. Basic parsing works for each format

use aeordb::engine::native_parsers::parse_native;

// ==========================================================================
// Helper
// ==========================================================================

fn call(data: &[u8], content_type: &str, filename: &str) -> Option<Result<serde_json::Value, String>> {
    parse_native(data, content_type, filename, &format!("/files/{}", filename), data.len() as u64)
}

// ==========================================================================
// 1. Content-type dispatch — each parser is reached
// ==========================================================================

#[test]
fn text_dispatched_for_text_plain() {
    let result = call(b"hello", "text/plain", "hello.txt");
    assert!(result.is_some(), "text/plain should be handled");
    let json = result.unwrap().unwrap();
    assert_eq!(json["text"], "hello");
}

#[test]
fn text_dispatched_for_text_markdown() {
    let result = call(b"# Title", "text/markdown", "readme.md");
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "markdown");
}

#[test]
fn text_dispatched_for_application_json() {
    let result = call(b"{}", "application/json", "data.json");
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "json");
}

#[test]
fn text_dispatched_for_application_yaml() {
    let result = call(b"key: value", "application/yaml", "config.yaml");
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "yaml");
}

#[test]
fn text_dispatched_for_text_css() {
    let result = call(b"body { color: red; }", "text/css", "style.css");
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "css");
}

#[test]
fn text_dispatched_for_text_csv() {
    let result = call(b"a,b,c", "text/csv", "data.csv");
    assert!(result.is_some());
    result.unwrap().unwrap();
}

#[test]
fn text_dispatched_for_application_xml() {
    let result = call(b"<root/>", "application/xml", "data.xml");
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "xml");
}

#[test]
fn text_dispatched_for_application_javascript() {
    let result = call(b"console.log(1)", "application/javascript", "app.js");
    assert!(result.is_some());
}

#[test]
fn text_dispatched_for_text_javascript() {
    let result = call(b"const x = 1;", "text/javascript", "app.js");
    assert!(result.is_some());
}

#[test]
fn text_dispatched_for_text_x_prefix() {
    let result = call(b"fn main() {}", "text/x-rust", "main.rs");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "rust");
}

#[test]
fn html_dispatched_for_text_html() {
    let html = b"<!DOCTYPE html><html><head><title>Test</title></head><body>Hello</body></html>";
    let result = call(html, "text/html", "page.html");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["title"], "Test");
    assert_eq!(json["metadata"]["format"], "html");
}

#[test]
fn html_dispatched_for_text_xml() {
    let xml = b"<?xml version=\"1.0\"?><root><child>text</child></root>";
    let result = call(xml, "text/xml", "data.xml");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "xml");
}

#[test]
fn html_dispatched_for_xhtml() {
    let result = call(b"<html><body>hi</body></html>", "application/xhtml+xml", "page.xhtml");
    assert!(result.is_some());
}

#[test]
fn image_dispatched_for_image_jpeg() {
    // Minimal JPEG: SOI marker
    let data = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x02, 0x00, 0x00, 0xFF, 0xD9];
    let result = call(&data, "image/jpeg", "photo.jpg");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "jpeg");
}

#[test]
fn image_dispatched_for_image_png() {
    // Minimal PNG header with IHDR
    let mut data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]; // PNG signature
    // IHDR chunk: length(4) + "IHDR"(4) + data(13) + CRC(4)
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]); // length = 13
    data.extend_from_slice(b"IHDR");
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x64]); // width = 100
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0xC8]); // height = 200
    data.push(8); // bit depth
    data.push(2); // color type (rgb)
    data.extend_from_slice(&[0x00, 0x00, 0x00]); // compression, filter, interlace
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC (dummy)

    let result = call(&data, "image/png", "image.png");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "png");
    assert_eq!(json["metadata"]["width"], 100);
    assert_eq!(json["metadata"]["height"], 200);
}

#[test]
fn image_dispatched_for_image_gif() {
    let mut data = Vec::new();
    data.extend_from_slice(b"GIF89a");
    // Logical screen descriptor: width(2) + height(2) + packed(1) + bg(1) + aspect(1)
    data.extend_from_slice(&50u16.to_le_bytes()); // width
    data.extend_from_slice(&50u16.to_le_bytes()); // height
    data.extend_from_slice(&[0x00, 0x00, 0x00]); // packed, bg, aspect
    data.push(0x3B); // trailer

    let result = call(&data, "image/gif", "anim.gif");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "gif");
}

#[test]
fn image_dispatched_for_image_bmp() {
    let result = call(&[0x42, 0x4D].iter().chain(&[0x00u8; 28]).copied().collect::<Vec<u8>>(), "image/bmp", "image.bmp");
    assert!(result.is_some());
}

#[test]
fn image_dispatched_for_image_webp() {
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // size
    data.extend_from_slice(b"WEBP");
    data.extend_from_slice(b"VP8X");
    // Pad to 30 bytes minimum
    data.extend(vec![0u8; 20]);

    let result = call(&data, "image/webp", "image.webp");
    assert!(result.is_some());
}

#[test]
fn image_dispatched_for_image_tiff() {
    let mut data = vec![0x49, 0x49, 0x2A, 0x00]; // II + magic 42
    data.extend_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD offset
    data.extend_from_slice(&[0x00, 0x00]); // 0 entries
    let result = call(&data, "image/tiff", "image.tiff");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "tiff");
}

#[test]
fn image_dispatched_for_image_svg() {
    let svg = b"<svg width=\"100\" height=\"200\" viewBox=\"0 0 100 200\"></svg>";
    let result = call(svg, "image/svg+xml", "icon.svg");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "svg");
}

#[test]
fn audio_dispatched_for_audio_mpeg() {
    // ID3v2 header
    let mut data = Vec::new();
    data.extend_from_slice(b"ID3");
    data.extend_from_slice(&[3, 0, 0]); // version, flags
    data.extend_from_slice(&[0, 0, 0, 0]); // size (synchsafe)
    data.extend(vec![0u8; 20]);

    let result = call(&data, "audio/mpeg", "song.mp3");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "mp3");
}

#[test]
fn audio_dispatched_for_audio_wav() {
    let wav = build_minimal_wav();
    let result = call(&wav, "audio/wav", "sound.wav");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "wav");
}

#[test]
fn audio_dispatched_for_audio_ogg() {
    let mut data = Vec::new();
    data.extend_from_slice(b"OggS");
    data.extend(vec![0u8; 30]);
    let result = call(&data, "audio/ogg", "music.ogg");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "ogg");
}

#[test]
fn audio_dispatched_for_audio_mp3() {
    let result = call(b"ID3\x03\x00\x00\x00\x00\x00\x00", "audio/mp3", "song.mp3");
    assert!(result.is_some());
}

#[test]
fn audio_dispatched_for_audio_x_wav() {
    let wav = build_minimal_wav();
    let result = call(&wav, "audio/x-wav", "sound.wav");
    assert!(result.is_some());
}

#[test]
fn audio_dispatched_for_audio_vorbis() {
    let result = call(b"OggS\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00", "audio/vorbis", "music.ogg");
    assert!(result.is_some());
}

#[test]
fn video_dispatched_for_video_mp4() {
    let mut data = vec![0u8; 12];
    data[4] = b'f'; data[5] = b't'; data[6] = b'y'; data[7] = b'p';
    data.extend(vec![0u8; 20]);
    let result = call(&data, "video/mp4", "clip.mp4");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "mp4");
}

#[test]
fn video_dispatched_for_video_quicktime() {
    let mut data = vec![0u8; 12];
    data[4] = b'f'; data[5] = b't'; data[6] = b'y'; data[7] = b'p';
    let result = call(&data, "video/quicktime", "movie.mov");
    assert!(result.is_some());
}

#[test]
fn video_dispatched_for_video_avi() {
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    data.extend_from_slice(b"AVI ");
    data.extend(vec![0u8; 20]);

    let result = call(&data, "video/x-msvideo", "video.avi");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "avi");
}

#[test]
fn video_dispatched_for_video_webm() {
    // EBML header
    let data = vec![0x1A, 0x45, 0xDF, 0xA3, 0x84, 0x00, 0x00, 0x00, 0x00];
    let result = call(&data, "video/webm", "video.webm");
    assert!(result.is_some());
}

#[test]
fn video_dispatched_for_video_flv() {
    let mut data = Vec::new();
    data.extend_from_slice(b"FLV");
    data.push(1); // version
    data.push(0x05); // flags: audio + video
    data.extend_from_slice(&[0, 0, 0, 9]); // header size
    let result = call(&data, "video/x-flv", "video.flv");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "flv");
}

#[test]
fn video_dispatched_for_video_matroska() {
    let result = call(&[0x1A, 0x45, 0xDF, 0xA3, 0x84, 0, 0, 0, 0], "video/x-matroska", "video.mkv");
    assert!(result.is_some());
}

#[test]
fn pdf_dispatched_for_application_pdf() {
    let data = b"%PDF-1.7\n1 0 obj\n<< /Title (Test) >>\nendobj\ntrailer\n<< /Info 1 0 R >>\n%%EOF\n";
    let result = call(data, "application/pdf", "doc.pdf");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "pdf");
    assert_eq!(json["metadata"]["version"], "1.7");
    assert_eq!(json["title"], "Test");
}

#[test]
fn msoffice_dispatched_for_docx() {
    let zip_data = build_docx_zip("Hello World", None);
    let result = call(&zip_data, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "doc.docx");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "docx");
    assert!(json["text"].as_str().unwrap().contains("Hello World"));
}

#[test]
fn msoffice_dispatched_for_xlsx() {
    let zip_data = build_xlsx_zip("Sheet Data", None);
    let result = call(&zip_data, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "data.xlsx");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "xlsx");
}

#[test]
fn msoffice_dispatched_for_msword() {
    // application/msword maps to msoffice parser, but ZIP validation will fail for non-zip data
    let result = call(b"not a zip", "application/msword", "old.doc");
    assert!(result.is_some());
    assert!(result.unwrap().is_err()); // not a ZIP
}

#[test]
fn msoffice_dispatched_for_ms_excel() {
    let result = call(b"not a zip", "application/vnd.ms-excel", "old.xls");
    assert!(result.is_some());
    assert!(result.unwrap().is_err());
}

#[test]
fn odf_dispatched_for_odt() {
    let zip_data = build_odt_zip("<text>Hello ODF</text>", None);
    let result = call(&zip_data, "application/vnd.oasis.opendocument.text", "doc.odt");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "odt");
}

#[test]
fn odf_dispatched_for_ods() {
    let zip_data = build_ods_zip("<table>Data</table>", None);
    let result = call(&zip_data, "application/vnd.oasis.opendocument.spreadsheet", "sheet.ods");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "ods");
}

// ==========================================================================
// 2. Extension fallback for application/octet-stream
// ==========================================================================

#[test]
fn extension_fallback_txt() {
    let result = call(b"hello world", "application/octet-stream", "readme.txt");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["text"], "hello world");
}

#[test]
fn extension_fallback_md() {
    let result = call(b"# Title", "application/octet-stream", "README.md");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "markdown");
}

#[test]
fn extension_fallback_rs() {
    let result = call(b"fn main() {}", "application/octet-stream", "main.rs");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["language"], "rust");
}

#[test]
fn extension_fallback_py() {
    let result = call(b"print('hi')", "application/octet-stream", "script.py");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_html() {
    let result = call(b"<html><body>hi</body></html>", "application/octet-stream", "page.html");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "html");
}

#[test]
fn extension_fallback_htm() {
    let result = call(b"<html></html>", "application/octet-stream", "page.htm");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_jpg() {
    let result = call(&[0xFF, 0xD8, 0xFF, 0xD9], "application/octet-stream", "photo.jpg");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "jpeg");
}

#[test]
fn extension_fallback_png() {
    let result = call(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0], "application/octet-stream", "img.png");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_mp3() {
    let result = call(b"ID3\x03\x00\x00\x00\x00\x00\x00", "application/octet-stream", "song.mp3");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_wav() {
    let wav = build_minimal_wav();
    let result = call(&wav, "application/octet-stream", "sound.wav");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_mp4() {
    let mut data = vec![0u8; 12];
    data[4] = b'f'; data[5] = b't'; data[6] = b'y'; data[7] = b'p';
    let result = call(&data, "application/octet-stream", "video.mp4");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_pdf() {
    let result = call(b"%PDF-1.4\n%%EOF\n", "application/octet-stream", "doc.pdf");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "pdf");
}

#[test]
fn extension_fallback_docx() {
    let zip_data = build_docx_zip("test", None);
    let result = call(&zip_data, "application/octet-stream", "doc.docx");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "docx");
}

#[test]
fn extension_fallback_xlsx() {
    let zip_data = build_xlsx_zip("data", None);
    let result = call(&zip_data, "application/octet-stream", "data.xlsx");
    assert!(result.is_some());
    let json = result.unwrap().unwrap();
    assert_eq!(json["metadata"]["format"], "xlsx");
}

#[test]
fn extension_fallback_odt() {
    let zip_data = build_odt_zip("<text/>", None);
    let result = call(&zip_data, "application/octet-stream", "doc.odt");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_ods() {
    let zip_data = build_ods_zip("<table/>", None);
    let result = call(&zip_data, "application/octet-stream", "sheet.ods");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_json() {
    let result = call(b"{\"key\": \"value\"}", "application/octet-stream", "data.json");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_yaml() {
    let result = call(b"key: value", "application/octet-stream", "config.yaml");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_yml() {
    let result = call(b"key: value", "application/octet-stream", "config.yml");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_toml() {
    let result = call(b"[section]", "application/octet-stream", "config.toml");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_xml() {
    let result = call(b"<root/>", "application/octet-stream", "data.xml");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_sql() {
    let result = call(b"SELECT 1", "application/octet-stream", "query.sql");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_svg() {
    let result = call(b"<svg></svg>", "application/octet-stream", "icon.svg");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_mov() {
    let mut data = vec![0u8; 12];
    data[4] = b'f'; data[5] = b't'; data[6] = b'y'; data[7] = b'p';
    let result = call(&data, "application/octet-stream", "clip.mov");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_avi() {
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&[0, 0, 0, 0]);
    data.extend_from_slice(b"AVI ");
    data.extend(vec![0u8; 20]);
    let result = call(&data, "application/octet-stream", "movie.avi");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_webm() {
    let result = call(&[0x1A, 0x45, 0xDF, 0xA3, 0x84, 0, 0, 0, 0], "application/octet-stream", "video.webm");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_mkv() {
    let result = call(&[0x1A, 0x45, 0xDF, 0xA3, 0x84, 0, 0, 0, 0], "application/octet-stream", "video.mkv");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_flv() {
    let mut data = Vec::new();
    data.extend_from_slice(b"FLV");
    data.push(1);
    data.push(0x05);
    data.extend_from_slice(&[0, 0, 0, 9]);
    let result = call(&data, "application/octet-stream", "video.flv");
    assert!(result.is_some());
}

#[test]
fn extension_fallback_ogg() {
    let result = call(b"OggS\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00", "application/octet-stream", "audio.ogg");
    assert!(result.is_some());
}

// Also test extension from path (not just filename)
#[test]
fn extension_fallback_uses_path_when_filename_has_no_extension() {
    let result = parse_native(b"hello", "application/octet-stream", "no_ext", "/files/document.txt", 5);
    assert!(result.is_some());
}

// Extension fallback only triggers for octet-stream or empty
#[test]
fn extension_fallback_not_triggered_for_known_content_type() {
    // video/mp4 content type should use content type dispatch, not extension
    let mut data = vec![0u8; 12];
    data[4] = b'f'; data[5] = b't'; data[6] = b'y'; data[7] = b'p';
    let result = call(&data, "video/mp4", "misleading.txt");
    assert!(result.is_some()); // dispatched by content type, not extension
}

// ==========================================================================
// 3. Unknown content types return None (fall through to WASM)
// ==========================================================================

#[test]
fn unknown_content_type_returns_none() {
    let result = call(b"whatever", "application/x-custom-format", "file.xyz");
    assert!(result.is_none(), "unknown type should return None for WASM fallback");
}

#[test]
fn unknown_extension_with_octet_stream_returns_none() {
    let result = call(b"data", "application/octet-stream", "file.custom");
    assert!(result.is_none());
}

#[test]
fn no_extension_with_octet_stream_returns_none() {
    // The path also has no extension
    let result = parse_native(b"data", "application/octet-stream", "Makefile", "/files/Makefile", 4);
    assert!(result.is_none());
}

#[test]
fn empty_content_type_with_unknown_extension_returns_none() {
    let result = parse_native(b"data", "", "file.xyz", "/files/file.xyz", 4);
    assert!(result.is_none());
}

#[test]
fn empty_content_type_with_known_extension_dispatches() {
    let result = parse_native(b"hello", "", "readme.txt", "/files/readme.txt", 5);
    assert!(result.is_some());
}

// ==========================================================================
// 4. Basic parsing works for each format
// ==========================================================================

#[test]
fn text_parser_extracts_metadata() {
    let data = b"First line\nSecond line\nThird line";
    let result = call(data, "text/plain", "doc.txt").unwrap().unwrap();
    assert_eq!(result["title"], "First line");
    assert_eq!(result["metadata"]["line_count"], 3);
    assert_eq!(result["metadata"]["word_count"], 6);
    assert_eq!(result["metadata"]["encoding"], "utf-8");
}

#[test]
fn text_parser_detects_bom() {
    let mut data = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
    data.extend_from_slice(b"BOM content");
    let result = call(&data, "text/plain", "bom.txt").unwrap().unwrap();
    assert_eq!(result["metadata"]["has_bom"], true);
    assert_eq!(result["text"], "BOM content");
}

#[test]
fn text_parser_rejects_invalid_utf8() {
    let result = call(&[0xFF, 0xFE, 0x80], "text/plain", "binary.dat");
    assert!(result.unwrap().is_err());
}

#[test]
fn text_parser_handles_empty_file() {
    let result = call(b"", "text/plain", "empty.txt").unwrap().unwrap();
    assert_eq!(result["text"], "");
    assert_eq!(result["metadata"]["is_empty"], true);
    assert_eq!(result["metadata"]["line_count"], 0);
}

#[test]
fn html_parser_extracts_metadata() {
    let html = b"<!DOCTYPE html><html><head><title>My Page</title>\
    <meta name=\"description\" content=\"A test page\">\
    </head><body><h1>Welcome</h1><p>Content</p>\
    <a href=\"/\">Link1</a><a href=\"/about\">Link2</a></body></html>";
    let result = call(html, "text/html", "page.html").unwrap().unwrap();
    assert_eq!(result["title"], "My Page");
    assert_eq!(result["metadata"]["description"], "A test page");
    assert_eq!(result["metadata"]["link_count"], 2);
    let headings = result["metadata"]["headings"].as_array().unwrap();
    assert_eq!(headings.len(), 1);
    assert_eq!(headings[0], "Welcome");
}

#[test]
fn html_parser_strips_script_and_style() {
    let html = b"<html><body><script>var x = 1;</script><p>visible</p><style>.a{}</style></body></html>";
    let result = call(html, "text/html", "page.html").unwrap().unwrap();
    let text = result["text"].as_str().unwrap();
    assert!(!text.contains("var x"));
    assert!(!text.contains(".a{}"));
    assert!(text.contains("visible"));
}

#[test]
fn xml_parser_extracts_root_and_namespaces() {
    let xml = b"<?xml version=\"1.0\"?><root xmlns=\"http://example.com\" xmlns:ns=\"http://ns.example.com\"><child/></root>";
    let result = call(xml, "text/xml", "data.xml").unwrap().unwrap();
    assert_eq!(result["metadata"]["format"], "xml");
    assert_eq!(result["metadata"]["root_element"], "root");
    let namespaces = result["metadata"]["namespaces"].as_array().unwrap();
    assert!(namespaces.len() >= 1);
}

#[test]
fn image_parser_handles_empty_data() {
    let result = call(b"", "image/jpeg", "empty.jpg").unwrap().unwrap();
    assert_eq!(result["metadata"]["format"], "unknown");
}

#[test]
fn audio_parser_wav_with_metadata() {
    let wav = build_minimal_wav();
    let result = call(&wav, "audio/wav", "sound.wav").unwrap().unwrap();
    assert_eq!(result["metadata"]["format"], "wav");
    assert_eq!(result["metadata"]["channels"], 2);
    assert_eq!(result["metadata"]["sample_rate"], 44100);
}

#[test]
fn audio_parser_unknown_format() {
    // Use bytes that don't match any audio magic: no ID3, no sync word, no RIFF, no OggS
    let data = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B];
    let result = call(&data, "audio/mpeg", "unknown.mp3").unwrap().unwrap();
    // Extension fallback will detect mp3 from filename
    assert!(result["metadata"]["format"].as_str().is_some());
}

#[test]
fn pdf_parser_rejects_non_pdf() {
    let result = call(b"not a pdf", "application/pdf", "fake.pdf");
    assert!(result.unwrap().is_err());
}

#[test]
fn pdf_parser_rejects_too_small() {
    let result = call(b"%PD", "application/pdf", "tiny.pdf");
    assert!(result.unwrap().is_err());
}

#[test]
fn pdf_parser_counts_pages() {
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Page >>\nendobj\n");
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Page >>\nendobj\n");
    pdf.extend_from_slice(b"3 0 obj\n<< /Type /Pages >>\nendobj\n"); // Pages, not Page
    pdf.extend_from_slice(b"%%EOF\n");

    let result = call(&pdf, "application/pdf", "multi.pdf").unwrap().unwrap();
    assert_eq!(result["metadata"]["page_count"], 2);
}

#[test]
fn video_parser_detects_flv_streams() {
    let mut data = Vec::new();
    data.extend_from_slice(b"FLV");
    data.push(1);
    data.push(0x05); // has audio + video
    data.extend_from_slice(&[0, 0, 0, 9]);
    let result = call(&data, "video/x-flv", "video.flv").unwrap().unwrap();
    assert_eq!(result["metadata"]["has_audio"], true);
    assert_eq!(result["metadata"]["has_video"], true);
}

#[test]
fn msoffice_rejects_non_zip() {
    let result = call(b"not a zip file", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "fake.docx");
    assert!(result.unwrap().is_err());
}

#[test]
fn msoffice_rejects_zip_without_office_content() {
    let zip_data = build_zip(&[("random.txt", b"hello")]);
    let result = call(&zip_data, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "fake.docx");
    assert!(result.unwrap().is_err());
}

#[test]
fn odf_rejects_non_zip() {
    let result = call(b"not a zip", "application/vnd.oasis.opendocument.text", "fake.odt");
    assert!(result.unwrap().is_err());
}

#[test]
fn odf_rejects_zip_without_mimetype() {
    let zip_data = build_zip(&[("content.xml", b"<root/>")]);
    let result = call(&zip_data, "application/vnd.oasis.opendocument.text", "no-mime.odt");
    assert!(result.unwrap().is_err());
}

#[test]
fn odf_rejects_wrong_mimetype() {
    let zip_data = build_zip(&[
        ("mimetype", b"application/pdf"),
        ("content.xml", b"<root/>"),
    ]);
    let result = call(&zip_data, "application/vnd.oasis.opendocument.text", "wrong.odt");
    assert!(result.unwrap().is_err());
}

#[test]
fn docx_extracts_core_properties() {
    let core_xml = r#"<cp:coreProperties>
        <dc:title>My Document</dc:title>
        <dc:creator>Test Author</dc:creator>
    </cp:coreProperties>"#;
    let zip_data = build_docx_zip("Hello", Some(core_xml));
    let result = call(&zip_data, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "doc.docx").unwrap().unwrap();
    assert_eq!(result["title"], "My Document");
    assert_eq!(result["metadata"]["author"], "Test Author");
}

#[test]
fn odt_extracts_metadata() {
    let meta_xml = r#"<office:document-meta>
        <office:meta>
            <dc:title>ODT Title</dc:title>
            <dc:creator>ODT Author</dc:creator>
            <meta:keyword>keyword1</meta:keyword>
            <meta:keyword>keyword2</meta:keyword>
        </office:meta>
    </office:document-meta>"#;
    let zip_data = build_odt_zip("<text:p>Hello ODF</text:p>", Some(meta_xml));
    let result = call(&zip_data, "application/vnd.oasis.opendocument.text", "doc.odt").unwrap().unwrap();
    assert_eq!(result["title"], "ODT Title");
    assert_eq!(result["metadata"]["author"], "ODT Author");
    let keywords = result["metadata"]["keywords"].as_array().unwrap();
    assert_eq!(keywords.len(), 2);
}

// ==========================================================================
// ZIP helper for building test archives
// ==========================================================================

fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::{Cursor, Write};
    let buffer = Vec::new();
    let cursor = Cursor::new(buffer);
    let mut writer = zip::ZipWriter::new(cursor);

    for (name, data) in entries {
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file(*name, options).unwrap();
        writer.write_all(data).unwrap();
    }

    writer.finish().unwrap().into_inner()
}

fn build_docx_zip(body_text: &str, core_xml: Option<&str>) -> Vec<u8> {
    let document_xml = format!(
        r#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>{}</w:t></w:r></w:p></w:body></w:document>"#,
        body_text
    );
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("word/document.xml", document_xml.into_bytes()),
    ];
    let core_owned;
    if let Some(core) = core_xml {
        core_owned = core.as_bytes().to_vec();
        entries.push(("docProps/core.xml", core_owned));
    }

    // Convert to slices for build_zip
    let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_ref(), d.as_slice())).collect();
    build_zip(&entry_refs)
}

fn build_xlsx_zip(shared_string: &str, core_xml: Option<&str>) -> Vec<u8> {
    let workbook_xml = r#"<?xml version="1.0"?><workbook><sheets><sheet name="Sheet1"/></sheets></workbook>"#;
    let shared_strings_xml = format!(
        r#"<?xml version="1.0"?><sst><si><t>{}</t></si></sst>"#,
        shared_string
    );
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("xl/workbook.xml", workbook_xml.as_bytes().to_vec()),
        ("xl/sharedStrings.xml", shared_strings_xml.into_bytes()),
    ];
    let core_owned;
    if let Some(core) = core_xml {
        core_owned = core.as_bytes().to_vec();
        entries.push(("docProps/core.xml", core_owned));
    }

    let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_ref(), d.as_slice())).collect();
    build_zip(&entry_refs)
}

fn build_odt_zip(content_xml: &str, meta_xml: Option<&str>) -> Vec<u8> {
    let mimetype = "application/vnd.oasis.opendocument.text";
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", mimetype.as_bytes().to_vec()),
        ("content.xml", content_xml.as_bytes().to_vec()),
    ];
    let meta_owned;
    if let Some(meta) = meta_xml {
        meta_owned = meta.as_bytes().to_vec();
        entries.push(("meta.xml", meta_owned));
    }

    let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_ref(), d.as_slice())).collect();
    build_zip(&entry_refs)
}

fn build_ods_zip(content_xml: &str, meta_xml: Option<&str>) -> Vec<u8> {
    let mimetype = "application/vnd.oasis.opendocument.spreadsheet";
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", mimetype.as_bytes().to_vec()),
        ("content.xml", content_xml.as_bytes().to_vec()),
    ];
    let meta_owned;
    if let Some(meta) = meta_xml {
        meta_owned = meta.as_bytes().to_vec();
        entries.push(("meta.xml", meta_owned));
    }

    let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_ref(), d.as_slice())).collect();
    build_zip(&entry_refs)
}

fn build_minimal_wav() -> Vec<u8> {
    let channels: u16 = 2;
    let sample_rate: u32 = 44100;
    let bits_per_sample: u16 = 16;
    let data_size: u32 = 1000;
    let byte_rate = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align = channels * bits_per_sample / 8;
    let fmt_chunk_size: u32 = 16;
    let riff_size = 4 + (8 + fmt_chunk_size) + 8 + data_size;

    let mut buffer = Vec::new();
    buffer.extend_from_slice(b"RIFF");
    buffer.extend_from_slice(&riff_size.to_le_bytes());
    buffer.extend_from_slice(b"WAVE");
    buffer.extend_from_slice(b"fmt ");
    buffer.extend_from_slice(&fmt_chunk_size.to_le_bytes());
    buffer.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buffer.extend_from_slice(&channels.to_le_bytes());
    buffer.extend_from_slice(&sample_rate.to_le_bytes());
    buffer.extend_from_slice(&byte_rate.to_le_bytes());
    buffer.extend_from_slice(&block_align.to_le_bytes());
    buffer.extend_from_slice(&bits_per_sample.to_le_bytes());
    buffer.extend_from_slice(b"data");
    buffer.extend_from_slice(&data_size.to_le_bytes());
    buffer.extend(vec![0u8; data_size as usize]);
    buffer
}

// ===========================================================================
// Real-file tests — use actual files from disk to validate parsers
// ===========================================================================

#[test]
fn real_jpeg_from_disk() {
    if let Ok(data) = std::fs::read("/home/wyatt/Pictures/me2.jpg") {
        let result = parse_native(&data, "image/jpeg", "me2.jpg", "/me2.jpg", data.len() as u64);
        assert!(result.is_some(), "JPEG should be handled");
        let json = result.unwrap().expect("real JPEG should parse without error");
        assert_eq!(json["metadata"]["format"].as_str().unwrap(), "jpeg");
        let w = json["metadata"]["width"].as_u64().unwrap_or(0);
        let h = json["metadata"]["height"].as_u64().unwrap_or(0);
        assert!(w > 0 && h > 0, "should extract dimensions from real JPEG: {}x{}", w, h);
        eprintln!("  Real JPEG: {}x{}", w, h);
    }
}

#[test]
fn real_png_from_disk() {
    if let Ok(data) = std::fs::read("/home/wyatt/Pictures/After.png") {
        let result = parse_native(&data, "image/png", "After.png", "/After.png", data.len() as u64);
        assert!(result.is_some());
        let json = result.unwrap().expect("real PNG should parse");
        assert_eq!(json["metadata"]["format"].as_str().unwrap(), "png");
        let w = json["metadata"]["width"].as_u64().unwrap_or(0);
        let h = json["metadata"]["height"].as_u64().unwrap_or(0);
        assert!(w > 0 && h > 0, "should extract dimensions: {}x{}", w, h);
        eprintln!("  Real PNG: {}x{}", w, h);
    }
}

#[test]
fn real_mp4_from_disk() {
    if let Ok(data) = std::fs::read("/home/wyatt/Videos/Kazam_screencast_00022.mp4") {
        let result = parse_native(&data, "video/mp4", "screencast.mp4", "/screencast.mp4", data.len() as u64);
        assert!(result.is_some());
        let json = result.unwrap().expect("real MP4 should parse");
        let fmt = json["metadata"]["format"].as_str().unwrap_or("unknown");
        assert!(fmt == "mp4" || fmt == "mov" || fmt == "m4v", "format should be mp4-family, got: {}", fmt);
        eprintln!("  Real MP4: format={}, duration={:?}s, {}x{}",
            fmt,
            json["metadata"]["duration_seconds"].as_f64(),
            json["metadata"]["width"].as_u64().unwrap_or(0),
            json["metadata"]["height"].as_u64().unwrap_or(0),
        );
    }
}

#[test]
fn real_3gpp_from_disk() {
    if let Ok(data) = std::fs::read("/home/wyatt/Videos/messages_0.3gpp") {
        let result = parse_native(&data, "video/3gpp", "messages.3gpp", "/messages.3gpp", data.len() as u64);
        // 3gpp may or may not be handled — it's video/* so it should dispatch to video parser
        if let Some(r) = result {
            let json = r.expect("3gpp should parse");
            eprintln!("  Real 3GPP: format={}", json["metadata"]["format"].as_str().unwrap_or("unknown"));
        }
    }
}

#[test]
fn real_html_portal() {
    if let Ok(data) = std::fs::read("/home/wyatt/Projects/aeordb-workspace/aeordb/aeordb-lib/src/portal/index.html") {
        let result = parse_native(&data, "text/html", "index.html", "/portal/index.html", data.len() as u64);
        assert!(result.is_some());
        let json = result.unwrap().expect("real HTML should parse");
        assert_eq!(json["metadata"]["format"].as_str().unwrap(), "html");
        let title = json["title"].as_str().unwrap_or("");
        assert_eq!(title, "AeorDB Portal");
        eprintln!("  Real HTML: title='{}', headings={:?}",
            title, json["metadata"]["headings"]);
    }
}

#[test]
fn real_rust_source() {
    if let Ok(data) = std::fs::read("/home/wyatt/Projects/aeordb-workspace/aeordb/aeordb-lib/src/engine/native_parsers/mod.rs") {
        let result = parse_native(&data, "text/plain", "mod.rs", "/engine/native_parsers/mod.rs", data.len() as u64);
        assert!(result.is_some());
        let json = result.unwrap().expect("Rust source should parse");
        assert_eq!(json["metadata"]["language"].as_str().unwrap(), "rust");
        let lines = json["metadata"]["line_count"].as_u64().unwrap_or(0);
        assert!(lines > 10, "should have many lines: {}", lines);
        eprintln!("  Real Rust: {} lines, {} words",
            lines, json["metadata"]["word_count"].as_u64().unwrap_or(0));
    }
}

// ===========================================================================
// Shared EXIF module tests
// ===========================================================================

use aeordb::engine::native_parsers::exif::{parse_exif, ExifData};

/// Build a minimal TIFF/IFD buffer with the given IFD entries.
/// Each entry is (tag: u16, data_type: u16, count: u32, value_or_offset: u32).
/// For ASCII strings longer than 4 bytes, `extra_data` is appended after the
/// IFD and `value_or_offset` should point to it (offset from start of buffer).
fn build_tiff_ifd(entries: &[(u16, u16, u32, u32)], extra_data: &[u8], little_endian: bool) -> Vec<u8> {
    let mut buf = Vec::new();

    // TIFF header: byte order (2) + magic 42 (2) + IFD offset (4)
    if little_endian {
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes()); // IFD starts at byte 8
    } else {
        buf.extend_from_slice(b"MM");
        buf.extend_from_slice(&42u16.to_be_bytes());
        buf.extend_from_slice(&8u32.to_be_bytes());
    }

    // IFD: entry count (2) + entries (12 each) + next IFD offset (4)
    let count = entries.len() as u16;
    if little_endian {
        buf.extend_from_slice(&count.to_le_bytes());
    } else {
        buf.extend_from_slice(&count.to_be_bytes());
    }

    for &(tag, dtype, count, value) in entries {
        if little_endian {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&dtype.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            buf.extend_from_slice(&value.to_le_bytes());
        } else {
            buf.extend_from_slice(&tag.to_be_bytes());
            buf.extend_from_slice(&dtype.to_be_bytes());
            buf.extend_from_slice(&count.to_be_bytes());
            buf.extend_from_slice(&value.to_be_bytes());
        }
    }

    // Next IFD offset = 0 (no more IFDs)
    buf.extend_from_slice(&[0u8; 4]);

    // Extra data (strings, GPS rationals, etc.)
    buf.extend_from_slice(extra_data);

    buf
}

#[test]
fn exif_parses_camera_make_and_model() {
    // "Canon\0" at offset after IFD, "EOS R5\0" right after
    let ifd_end = 8 + 2 + (2 * 12) + 4; // header + count + 2 entries + next_ifd
    let make = b"Canon\0";
    let model = b"EOS R5\0";
    let make_offset = ifd_end as u32;
    let model_offset = (ifd_end + make.len()) as u32;

    let entries = vec![
        (0x010F, 2, make.len() as u32, make_offset),   // Make
        (0x0110, 2, model.len() as u32, model_offset),  // Model
    ];
    let mut extra = Vec::new();
    extra.extend_from_slice(make);
    extra.extend_from_slice(model);

    let buf = build_tiff_ifd(&entries, &extra, true);
    let exif = parse_exif(&buf).expect("should parse EXIF");
    assert_eq!(exif.camera_make.as_deref(), Some("Canon"));
    assert_eq!(exif.camera_model.as_deref(), Some("EOS R5"));
}

#[test]
fn exif_parses_new_textual_tags() {
    let ifd_end = 8 + 2 + (5 * 12) + 4; // 5 entries
    let desc = b"Sunset photo\0";
    let artist = b"Jane Doe\0";
    let copyright = b"2026 Jane Doe\0";
    let software = b"Lightroom\0";

    let mut offset = ifd_end as u32;
    let desc_off = offset; offset += desc.len() as u32;
    let artist_off = offset; offset += artist.len() as u32;
    let copyright_off = offset; offset += copyright.len() as u32;
    let software_off = offset;

    let entries = vec![
        (0x010E, 2, desc.len() as u32, desc_off),         // ImageDescription
        (0x013B, 2, artist.len() as u32, artist_off),      // Artist
        (0x8298, 2, copyright.len() as u32, copyright_off), // Copyright
        (0x0131, 2, software.len() as u32, software_off),  // Software
        (0x0112, 3, 1, 6),                                  // Orientation = 6 (SHORT inline)
    ];
    let mut extra = Vec::new();
    extra.extend_from_slice(desc);
    extra.extend_from_slice(artist);
    extra.extend_from_slice(copyright);
    extra.extend_from_slice(software);

    let buf = build_tiff_ifd(&entries, &extra, true);
    let exif = parse_exif(&buf).expect("should parse EXIF");
    assert_eq!(exif.image_description.as_deref(), Some("Sunset photo"));
    assert_eq!(exif.artist.as_deref(), Some("Jane Doe"));
    assert_eq!(exif.copyright.as_deref(), Some("2026 Jane Doe"));
    assert_eq!(exif.software.as_deref(), Some("Lightroom"));
    assert_eq!(exif.orientation, Some(6));
}

#[test]
fn exif_parses_big_endian() {
    let ifd_end = 8 + 2 + (1 * 12) + 4;
    let make = b"Nikon\0";
    let make_offset = ifd_end as u32;

    let entries = vec![
        (0x010F, 2, make.len() as u32, make_offset),
    ];
    let buf = build_tiff_ifd(&entries, make, false); // big-endian
    let exif = parse_exif(&buf).expect("should parse big-endian EXIF");
    assert_eq!(exif.camera_make.as_deref(), Some("Nikon"));
}

#[test]
fn exif_returns_none_for_empty_data() {
    assert!(parse_exif(&[]).is_none());
    assert!(parse_exif(&[0; 4]).is_none());
}

#[test]
fn exif_returns_none_for_invalid_byte_order() {
    let mut buf = vec![0x00, 0x00]; // invalid byte order
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());
    assert!(parse_exif(&buf).is_none());
}

#[test]
fn exif_returns_none_for_wrong_magic() {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&99u16.to_le_bytes()); // wrong magic (not 42)
    buf.extend_from_slice(&8u32.to_le_bytes());
    assert!(parse_exif(&buf).is_none());
}

#[test]
fn exif_returns_none_when_no_tags_present() {
    // Valid TIFF header but zero IFD entries
    let buf = build_tiff_ifd(&[], &[], true);
    assert!(parse_exif(&buf).is_none());
}

#[test]
fn exif_handles_truncated_ifd_gracefully() {
    // Valid header pointing to IFD at offset 8, but buffer ends before IFD data
    let mut buf = Vec::new();
    buf.extend_from_slice(b"II");
    buf.extend_from_slice(&42u16.to_le_bytes());
    buf.extend_from_slice(&8u32.to_le_bytes());
    // Only 8 bytes — IFD count would need 2 more bytes
    assert!(parse_exif(&buf).is_none());
}

#[test]
fn exif_to_json_omits_none_fields() {
    let data = ExifData {
        camera_make: Some("Canon".into()),
        artist: Some("Jane".into()),
        ..Default::default()
    };
    let json = data.to_json();
    assert_eq!(json["camera_make"], "Canon");
    assert_eq!(json["artist"], "Jane");
    assert!(json.get("camera_model").is_none());
    assert!(json.get("copyright").is_none());
    assert!(json.get("gps_latitude").is_none());
}

// ===========================================================================
// MP4 iTunes metadata tests
// ===========================================================================

/// Build a minimal MP4 with ftyp + moov containing udta/meta/ilst atoms.
fn build_mp4_with_tags(tags: &[(&[u8; 4], &str)]) -> Vec<u8> {
    let mut buf = Vec::new();

    // ftyp box
    let ftyp_data = b"isom\x00\x00\x00\x00isom";
    let ftyp_size = (8 + ftyp_data.len()) as u32;
    buf.extend_from_slice(&ftyp_size.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(ftyp_data);

    // Build ilst content
    let mut ilst_content = Vec::new();
    for (atom_type, text) in tags {
        // data sub-atom: size(4) + "data"(4) + type_indicator(4) + locale(4) + text
        let data_payload_size = (8 + 8 + text.len()) as u32;
        let mut data_atom = Vec::new();
        data_atom.extend_from_slice(&data_payload_size.to_be_bytes());
        data_atom.extend_from_slice(b"data");
        data_atom.extend_from_slice(&1u32.to_be_bytes()); // type = UTF-8 text
        data_atom.extend_from_slice(&0u32.to_be_bytes()); // locale
        data_atom.extend_from_slice(text.as_bytes());

        // ilst child atom: size(4) + type(4) + data_atom
        let child_size = (8 + data_atom.len()) as u32;
        ilst_content.extend_from_slice(&child_size.to_be_bytes());
        ilst_content.extend_from_slice(*atom_type);
        ilst_content.extend_from_slice(&data_atom);
    }

    // ilst box
    let ilst_size = (8 + ilst_content.len()) as u32;
    let mut ilst_box = Vec::new();
    ilst_box.extend_from_slice(&ilst_size.to_be_bytes());
    ilst_box.extend_from_slice(b"ilst");
    ilst_box.extend_from_slice(&ilst_content);

    // meta box (full box: 4-byte version/flags before children)
    let meta_size = (8 + 4 + ilst_box.len()) as u32;
    let mut meta_box = Vec::new();
    meta_box.extend_from_slice(&meta_size.to_be_bytes());
    meta_box.extend_from_slice(b"meta");
    meta_box.extend_from_slice(&[0u8; 4]); // version + flags
    meta_box.extend_from_slice(&ilst_box);

    // udta box
    let udta_size = (8 + meta_box.len()) as u32;
    let mut udta_box = Vec::new();
    udta_box.extend_from_slice(&udta_size.to_be_bytes());
    udta_box.extend_from_slice(b"udta");
    udta_box.extend_from_slice(&meta_box);

    // moov box with just udta
    let moov_size = (8 + udta_box.len()) as u32;
    buf.extend_from_slice(&moov_size.to_be_bytes());
    buf.extend_from_slice(b"moov");
    buf.extend_from_slice(&udta_box);

    buf
}

#[test]
fn mp4_extracts_itunes_metadata() {
    let mp4 = build_mp4_with_tags(&[
        (b"\xa9nam", "Mountain Timelapse"),
        (b"\xa9ART", "Jane Doe"),
        (b"desc", "4K timelapse of sunset"),
        (b"cprt", "2026 Jane Doe"),
        (b"\xa9cmt", "Shot with Canon R5"),
        (b"\xa9day", "2026"),
        (b"\xa9too", "HandBrake 1.8.0"),
    ]);

    let result = parse_native(&mp4, "video/mp4", "timelapse.mp4", "/timelapse.mp4", mp4.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("MP4 should parse");
    assert_eq!(json["metadata"]["format"], "mp4");
    assert_eq!(json["metadata"]["tags"]["title"], "Mountain Timelapse");
    assert_eq!(json["metadata"]["tags"]["artist"], "Jane Doe");
    assert_eq!(json["metadata"]["tags"]["description"], "4K timelapse of sunset");
    assert_eq!(json["metadata"]["tags"]["copyright"], "2026 Jane Doe");
    assert_eq!(json["metadata"]["tags"]["comment"], "Shot with Canon R5");
    assert_eq!(json["metadata"]["tags"]["year"], "2026");
    assert_eq!(json["metadata"]["tags"]["encoder"], "HandBrake 1.8.0");
}

#[test]
fn mp4_without_udta_has_no_tags() {
    // Minimal MP4 with just ftyp + moov (no udta)
    let mut buf = Vec::new();

    // ftyp
    let ftyp_data = b"isom\x00\x00\x00\x00isom";
    let ftyp_size = (8 + ftyp_data.len()) as u32;
    buf.extend_from_slice(&ftyp_size.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(ftyp_data);

    // Empty moov
    buf.extend_from_slice(&8u32.to_be_bytes());
    buf.extend_from_slice(b"moov");

    let result = parse_native(&buf, "video/mp4", "video.mp4", "/video.mp4", buf.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("MP4 should parse");
    assert_eq!(json["metadata"]["format"], "mp4");
    assert!(json["metadata"].get("tags").is_none(), "no tags when no udta present");
}

#[test]
fn mp4_with_empty_ilst_has_no_tags() {
    let mp4 = build_mp4_with_tags(&[]);
    let result = parse_native(&mp4, "video/mp4", "empty.mp4", "/empty.mp4", mp4.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("MP4 should parse");
    assert!(json["metadata"].get("tags").is_none(), "no tags when ilst is empty");
}

// ===========================================================================
// WAV RIFF INFO chunk tests
// ===========================================================================

/// Build a WAV file with fmt + data + LIST/INFO chunks.
fn build_wav_with_info(info_chunks: &[(&[u8; 4], &str)]) -> Vec<u8> {
    // Build the INFO sub-chunks
    let mut info_content = Vec::new();
    info_content.extend_from_slice(b"INFO");
    for (id, text) in info_chunks {
        let text_bytes = text.as_bytes();
        let chunk_size = text_bytes.len() as u32 + 1; // +1 for null terminator
        info_content.extend_from_slice(*id);
        info_content.extend_from_slice(&chunk_size.to_le_bytes());
        info_content.extend_from_slice(text_bytes);
        info_content.push(0); // null terminator
        // Pad to even boundary
        if (text_bytes.len() + 1) % 2 == 1 {
            info_content.push(0);
        }
    }

    // Build fmt chunk (PCM, stereo, 44100Hz, 16-bit)
    let mut fmt_data = Vec::new();
    fmt_data.extend_from_slice(&1u16.to_le_bytes());     // PCM
    fmt_data.extend_from_slice(&2u16.to_le_bytes());     // 2 channels
    fmt_data.extend_from_slice(&44100u32.to_le_bytes()); // sample rate
    fmt_data.extend_from_slice(&176400u32.to_le_bytes()); // byte rate
    fmt_data.extend_from_slice(&4u16.to_le_bytes());     // block align
    fmt_data.extend_from_slice(&16u16.to_le_bytes());    // bits per sample

    // Build data chunk (empty audio data)
    let data_size: u32 = 0;

    // Total RIFF size
    let riff_content_size = 4  // "WAVE"
        + 8 + fmt_data.len()   // fmt chunk
        + 8 + data_size as usize // data chunk
        + 8 + info_content.len(); // LIST chunk

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(riff_content_size as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&(fmt_data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&fmt_data);

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    // LIST chunk
    wav.extend_from_slice(b"LIST");
    wav.extend_from_slice(&(info_content.len() as u32).to_le_bytes());
    wav.extend_from_slice(&info_content);

    wav
}

#[test]
fn wav_extracts_info_metadata() {
    let wav = build_wav_with_info(&[
        (b"INAM", "Forest Ambience"),
        (b"IART", "Sound Studio"),
        (b"ICMT", "Field recording"),
        (b"ICOP", "2026 Sound Studio"),
        (b"ISFT", "Audacity 3.5"),
        (b"IGNR", "Ambient"),
        (b"ICRD", "2026-04-15"),
    ]);

    let result = parse_native(&wav, "audio/wav", "forest.wav", "/forest.wav", wav.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("WAV should parse");
    assert_eq!(json["metadata"]["format"], "wav");
    assert_eq!(json["metadata"]["tags"]["title"], "Forest Ambience");
    assert_eq!(json["metadata"]["tags"]["artist"], "Sound Studio");
    assert_eq!(json["metadata"]["tags"]["comment"], "Field recording");
    assert_eq!(json["metadata"]["tags"]["copyright"], "2026 Sound Studio");
    assert_eq!(json["metadata"]["tags"]["software"], "Audacity 3.5");
    assert_eq!(json["metadata"]["tags"]["genre"], "Ambient");
    assert_eq!(json["metadata"]["tags"]["year"], "2026-04-15");
}

#[test]
fn wav_without_info_has_no_tags() {
    let wav = build_minimal_wav(); // existing helper from this file
    let result = parse_native(&wav, "audio/wav", "audio.wav", "/audio.wav", wav.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("WAV should parse");
    assert_eq!(json["metadata"]["format"], "wav");
    assert!(json["metadata"].get("tags").is_none(), "no tags without INFO chunk");
}

#[test]
fn tiff_extracts_exif_metadata() {
    let ifd_end = 8 + 2 + (2 * 12) + 4;
    let desc = b"Mountain landscape\0";
    let artist = b"Photo Pro\0";
    let desc_off = ifd_end as u32;
    let artist_off = (ifd_end + desc.len()) as u32;

    let entries = vec![
        (0x010E_u16, 2_u16, desc.len() as u32, desc_off),
        (0x013B, 2, artist.len() as u32, artist_off),
    ];
    let mut extra = Vec::new();
    extra.extend_from_slice(desc);
    extra.extend_from_slice(artist);
    let tiff_data = build_tiff_ifd(&entries, &extra, true);

    let result = parse_native(&tiff_data, "image/tiff", "photo.tiff", "/photo.tiff", tiff_data.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("TIFF should parse");
    assert_eq!(json["metadata"]["format"], "tiff");
    assert_eq!(json["metadata"]["exif"]["image_description"], "Mountain landscape");
    assert_eq!(json["metadata"]["exif"]["artist"], "Photo Pro");
}

#[test]
fn jpeg_exif_includes_new_fields() {
    let ifd_end = 8 + 2 + (2 * 12) + 4;
    let desc = b"Beach sunset\0";
    let artist = b"Jane Doe\0";
    let desc_off = ifd_end as u32;
    let artist_off = (ifd_end + desc.len()) as u32;

    let entries = vec![
        (0x010E_u16, 2_u16, desc.len() as u32, desc_off),
        (0x013B, 2, artist.len() as u32, artist_off),
    ];
    let mut extra = Vec::new();
    extra.extend_from_slice(desc);
    extra.extend_from_slice(artist);
    let tiff_data = build_tiff_ifd(&entries, &extra, true);

    let mut jpeg = vec![0xFF, 0xD8];
    jpeg.push(0xFF); jpeg.push(0xE1);
    let app1_length = (2 + 6 + tiff_data.len()) as u16;
    jpeg.extend_from_slice(&app1_length.to_be_bytes());
    jpeg.extend_from_slice(b"Exif\x00\x00");
    jpeg.extend_from_slice(&tiff_data);
    jpeg.extend_from_slice(&[0xFF, 0xC0]);
    jpeg.extend_from_slice(&11u16.to_be_bytes());
    jpeg.push(8);
    jpeg.extend_from_slice(&100u16.to_be_bytes());
    jpeg.extend_from_slice(&200u16.to_be_bytes());
    jpeg.push(3);
    jpeg.extend_from_slice(&[0xFF, 0xD9]);

    let result = parse_native(&jpeg, "image/jpeg", "sunset.jpg", "/sunset.jpg", jpeg.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("JPEG should parse");
    assert_eq!(json["metadata"]["format"], "jpeg");
    assert_eq!(json["metadata"]["exif"]["image_description"], "Beach sunset");
    assert_eq!(json["metadata"]["exif"]["artist"], "Jane Doe");
}
