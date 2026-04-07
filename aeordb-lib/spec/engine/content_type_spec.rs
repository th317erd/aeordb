use aeordb::engine::content_type::detect_content_type;

// ---------------------------------------------------------------------------
// Unit tests: detection logic
// ---------------------------------------------------------------------------

#[test]
fn test_provided_content_type_used() {
    let data = b"not actually json";
    assert_eq!(detect_content_type(data, Some("application/json")), "application/json");
}

#[test]
fn test_octet_stream_triggers_detection() {
    // PNG magic bytes
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
    // This could be text/plain since JSON has no magic bytes.
    // The caller should provide "application/json" explicitly.
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
    // Even if data looks like PNG, explicit type wins
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

#[test]
fn test_gif_detection() {
    let gif_header = b"GIF89a";
    let result = detect_content_type(gif_header, None);
    assert_eq!(result, "image/gif");
}

#[test]
fn test_binary_with_null_bytes_not_text() {
    let data = b"looks like text\x00but has nulls";
    let result = detect_content_type(data, None);
    assert_eq!(result, "application/octet-stream");
}

#[test]
fn test_text_with_tabs_and_carriage_returns() {
    let data = b"col1\tcol2\tcol3\r\nval1\tval2\tval3\r\n";
    let result = detect_content_type(data, None);
    assert_eq!(result, "text/plain");
}

#[test]
fn test_non_utf8_not_text() {
    let data = vec![0x80, 0x81, 0x82, 0x83];
    let result = detect_content_type(&data, None);
    assert_eq!(result, "application/octet-stream");
}

// ---------------------------------------------------------------------------
// Integration tests: stored files get detected content type
// ---------------------------------------------------------------------------

#[test]
fn test_stored_file_gets_detected_content_type() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store PNG data without content type
    let png_header = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
    let record = ops.store_file(&ctx, "/test.png", &png_header, None).unwrap();
    assert_eq!(record.content_type, Some("image/png".to_string()));
}

#[test]
fn test_stored_text_file_detected() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let record = ops.store_file(&ctx, "/readme.txt", b"Hello world\nThis is text", None).unwrap();
    assert_eq!(record.content_type, Some("text/plain".to_string()));
}

#[test]
fn test_explicit_content_type_preserved() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let record = ops.store_file(&ctx, "/data.json", b"{}", Some("application/json")).unwrap();
    assert_eq!(record.content_type, Some("application/json".to_string()));
}

#[test]
fn test_octet_stream_overridden_in_storage() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Client sends application/octet-stream but data is PNG
    let png_header = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
    let record = ops.store_file(&ctx, "/image.bin", &png_header, Some("application/octet-stream")).unwrap();
    assert_eq!(record.content_type, Some("image/png".to_string()));
}

#[test]
fn test_empty_file_stored_with_octet_stream() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let record = ops.store_file(&ctx, "/empty", b"", None).unwrap();
    assert_eq!(record.content_type, Some("application/octet-stream".to_string()));
}

#[test]
fn test_metadata_reflects_detected_type() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store PDF without content type
    let pdf_data = b"%PDF-1.4 some pdf content here";
    ops.store_file(&ctx, "/doc.pdf", pdf_data, None).unwrap();

    // Read back metadata and verify detected type is persisted
    let metadata = ops.get_metadata("/doc.pdf").unwrap().unwrap();
    assert_eq!(metadata.content_type, Some("application/pdf".to_string()));
}

#[test]
fn test_directory_listing_shows_detected_type() {
    use aeordb::engine::{DirectoryOps, RequestContext};
    use aeordb::server::create_temp_engine_for_tests;

    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let png_header = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
    ops.store_file(&ctx, "/images/photo.png", &png_header, None).unwrap();

    let children = ops.list_directory("/images").unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].content_type, Some("image/png".to_string()));
}
