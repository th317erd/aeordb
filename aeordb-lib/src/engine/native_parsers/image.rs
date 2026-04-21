/// Native image metadata parser.
///
/// Ported from `aeordb-plugin-parser-image`.

use serde_json::json;
use super::exif;
use super::exif::{
    read_u16_big_endian, read_u16_little_endian,
    read_u32_big_endian, read_u32_little_endian,
    read_u16, read_u32,
};

pub fn parse(data: &[u8], filename: &str, content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    // data is already passed as parameter
    

    let mut metadata = json!({
        "filename": filename,
        "content_type": content_type,
        "size": size,
    });

    if data.is_empty() {
        metadata["format"] = json!("unknown");
        return Ok(json!({ "text": "", "metadata": metadata }));
    }

    let format_result = detect_and_parse(data);

    metadata["format"] = json!(format_result.format);
    if let Some(width) = format_result.width {
        metadata["width"] = json!(width);
    }
    if let Some(height) = format_result.height {
        metadata["height"] = json!(height);
    }
    if let Some(bit_depth) = format_result.bit_depth {
        metadata["bit_depth"] = json!(bit_depth);
    }
    if let Some(ref color_type) = format_result.color_type {
        metadata["color_type"] = json!(color_type);
    }
    if let Some(has_alpha) = format_result.has_alpha {
        metadata["has_alpha"] = json!(has_alpha);
    }
    if let Some(is_animated) = format_result.is_animated {
        metadata["is_animated"] = json!(is_animated);
    }
    if let Some(ref byte_order) = format_result.byte_order {
        metadata["byte_order"] = json!(byte_order);
    }
    if let Some(ref viewbox) = format_result.viewbox {
        metadata["viewbox"] = json!(viewbox);
    }
    if let Some(ref exif) = format_result.exif {
        metadata["exif"] = exif.clone();
    }
    if let Some(ref text_metadata) = format_result.text_metadata {
        metadata["text_metadata"] = text_metadata.clone();
    }

    Ok(json!({ "text": "", "metadata": metadata }))
}

struct FormatResult {
    format: String,
    width: Option<u32>,
    height: Option<u32>,
    bit_depth: Option<u32>,
    color_type: Option<String>,
    has_alpha: Option<bool>,
    is_animated: Option<bool>,
    byte_order: Option<String>,
    viewbox: Option<String>,
    exif: Option<serde_json::Value>,
    text_metadata: Option<serde_json::Value>,
}

impl FormatResult {
    fn unknown() -> Self {
        Self {
            format: "unknown".to_string(),
            width: None,
            height: None,
            bit_depth: None,
            color_type: None,
            has_alpha: None,
            is_animated: None,
            byte_order: None,
            viewbox: None,
            exif: None,
            text_metadata: None,
        }
    }
}

fn detect_and_parse(data: &[u8]) -> FormatResult {
    // JPEG: 0xFF 0xD8
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return parse_jpeg(data);
    }

    // PNG: 0x89 0x50 0x4E 0x47 0x0D 0x0A 0x1A 0x0A
    if data.len() >= 8 && data[0..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return parse_png(data);
    }

    // GIF: GIF87a or GIF89a
    if data.len() >= 6 && (data[0..6] == *b"GIF87a" || data[0..6] == *b"GIF89a") {
        return parse_gif(data);
    }

    // BMP: 0x42 0x4D
    if data.len() >= 2 && data[0] == 0x42 && data[1] == 0x4D {
        return parse_bmp(data);
    }

    // WebP: RIFF....WEBP
    if data.len() >= 12 && data[0..4] == *b"RIFF" && data[8..12] == *b"WEBP" {
        return parse_webp(data);
    }

    // TIFF: II (little-endian) or MM (big-endian)
    if data.len() >= 4
        && ((data[0] == 0x49 && data[1] == 0x49 && data[2] == 0x2A && data[3] == 0x00)
            || (data[0] == 0x4D && data[1] == 0x4D && data[2] == 0x00 && data[3] == 0x2A))
    {
        return parse_tiff(data);
    }

    // SVG: text-based, starts with <svg or <?xml
    if is_svg(data) {
        return parse_svg(data);
    }

    FormatResult::unknown()
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

fn parse_jpeg(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "jpeg".to_string();
    result.has_alpha = Some(false);
    result.is_animated = Some(false);

    let mut offset = 2; // skip SOI marker

    while offset + 1 < data.len() {
        if data[offset] != 0xFF {
            break;
        }

        let marker = data[offset + 1];

        // Skip padding 0xFF bytes
        if marker == 0xFF {
            offset += 1;
            continue;
        }

        // Markers without length (standalone)
        if marker == 0x00 || marker == 0xD8 || (0xD0..=0xD7).contains(&marker) {
            offset += 2;
            continue;
        }

        // End of image
        if marker == 0xD9 {
            break;
        }

        // Need at least 2 bytes for segment length
        if offset + 3 >= data.len() {
            break;
        }

        let segment_length = read_u16_big_endian(data, offset + 2) as usize;
        if segment_length < 2 {
            break;
        }

        let segment_start = offset + 2;
        let segment_end = segment_start + segment_length;

        // SOF0 (baseline) or SOF2 (progressive) -- dimensions
        if marker == 0xC0 || marker == 0xC2 {
            if segment_start + segment_length <= data.len() && segment_length >= 7 {
                result.bit_depth = Some(data[segment_start + 2] as u32);
                result.height = Some(read_u16_big_endian(data, segment_start + 3) as u32);
                result.width = Some(read_u16_big_endian(data, segment_start + 5) as u32);

                if segment_length >= 8 {
                    let num_components = data[segment_start + 7];
                    result.color_type = Some(match num_components {
                        1 => "grayscale".to_string(),
                        3 => "rgb".to_string(),
                        4 => "cmyk".to_string(),
                        _ => format!("components_{}", num_components),
                    });
                }
            }
        }

        // APP1 (EXIF) marker
        if marker == 0xE1 && segment_start + segment_length <= data.len() {
            let app1_data = &data[segment_start + 2..segment_end.min(data.len())];
            if app1_data.len() >= 6 && app1_data[0..6] == *b"Exif\x00\x00" {
                if let Some(exif_data) = exif::parse_exif(&app1_data[6..]) {
                    result.exif = Some(exif_data.to_json());
                }
            }
        }

        // Advance past this segment
        if segment_end > data.len() {
            break;
        }
        offset = segment_end;
    }

    result
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

fn parse_png(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "png".to_string();
    result.is_animated = Some(false);

    // IHDR must be the first chunk, starting at byte 8
    // Chunk layout: 4-byte length, 4-byte type, N-byte data, 4-byte CRC
    if data.len() < 29 {
        // 8 (sig) + 4 (len) + 4 (type) + 13 (IHDR data) = 29 minimum
        return result;
    }

    let chunk_type = &data[12..16];
    if chunk_type != b"IHDR" {
        return result;
    }

    // IHDR data starts at byte 16
    result.width = Some(read_u32_big_endian(data, 16));
    result.height = Some(read_u32_big_endian(data, 20));
    result.bit_depth = Some(data[24] as u32);

    let color_type_byte = data[25];
    let (color_type_name, has_alpha) = match color_type_byte {
        0 => ("grayscale", false),
        2 => ("rgb", false),
        3 => ("indexed", false),
        4 => ("grayscale_alpha", true),
        6 => ("rgba", true),
        _ => ("unknown", false),
    };
    result.color_type = Some(color_type_name.to_string());
    result.has_alpha = Some(has_alpha);

    // Scan remaining chunks for tEXt, iTXt, and acTL (animated PNG)
    let mut text_entries = serde_json::Map::new();
    let mut offset = 8; // skip PNG signature

    while offset + 12 <= data.len() {
        let chunk_length = read_u32_big_endian(data, offset) as usize;
        let chunk_name = &data[offset + 4..offset + 8];
        let chunk_data_start = offset + 8;
        let chunk_data_end = chunk_data_start + chunk_length;

        if chunk_data_end + 4 > data.len() {
            break;
        }

        if chunk_name == b"tEXt" && chunk_length > 0 {
            // tEXt: keyword\0text
            let chunk_bytes = &data[chunk_data_start..chunk_data_end];
            if let Some(null_position) = chunk_bytes.iter().position(|&b| b == 0) {
                let keyword = String::from_utf8_lossy(&chunk_bytes[..null_position]).to_string();
                let value = String::from_utf8_lossy(&chunk_bytes[null_position + 1..]).to_string();
                text_entries.insert(keyword, json!(value));
            }
        }

        if chunk_name == b"iTXt" && chunk_length > 0 {
            // iTXt: keyword\0compression_flag\0compression_method\0language\0translated_keyword\0text
            let chunk_bytes = &data[chunk_data_start..chunk_data_end];
            if let Some(null_position) = chunk_bytes.iter().position(|&b| b == 0) {
                let keyword = String::from_utf8_lossy(&chunk_bytes[..null_position]).to_string();
                // Skip compression_flag, compression_method, then find text after remaining nulls
                let remaining = &chunk_bytes[null_position + 1..];
                // Skip to the text content (after 3 more null-terminated fields)
                let mut null_count = 0;
                let mut text_start = 0;
                for (index, &byte) in remaining.iter().enumerate() {
                    if byte == 0 {
                        null_count += 1;
                        if null_count == 3 {
                            text_start = index + 1;
                            break;
                        }
                    }
                }
                if text_start < remaining.len() {
                    let value = String::from_utf8_lossy(&remaining[text_start..]).to_string();
                    text_entries.insert(keyword, json!(value));
                }
            }
        }

        if chunk_name == b"acTL" {
            result.is_animated = Some(true);
        }

        // Move to next chunk: length + type(4) + data(chunk_length) + CRC(4)
        offset = chunk_data_end + 4;
    }

    if !text_entries.is_empty() {
        result.text_metadata = Some(serde_json::Value::Object(text_entries));
    }

    result
}

// ---------------------------------------------------------------------------
// GIF
// ---------------------------------------------------------------------------

fn parse_gif(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "gif".to_string();
    result.bit_depth = Some(8);
    result.color_type = Some("indexed".to_string());
    result.has_alpha = Some(true); // GIF supports transparency

    if data.len() < 10 {
        return result;
    }

    // Width and height at bytes 6-9 (little-endian 16-bit)
    result.width = Some(read_u16_little_endian(data, 6) as u32);
    result.height = Some(read_u16_little_endian(data, 8) as u32);

    // Count image descriptors (0x2C) to detect animation
    // Skip the logical screen descriptor and global color table first
    let packed = data[10];
    let has_global_color_table = (packed & 0x80) != 0;
    let global_color_table_size = if has_global_color_table {
        3 * (1 << ((packed & 0x07) + 1))
    } else {
        0
    };

    let mut offset = 13 + global_color_table_size as usize;
    let mut frame_count: u32 = 0;

    while offset < data.len() {
        match data[offset] {
            // Extension introducer
            0x21 => {
                if offset + 1 >= data.len() {
                    break;
                }
                offset += 2; // skip extension label
                // Skip sub-blocks
                while offset < data.len() {
                    let block_size = data[offset] as usize;
                    if block_size == 0 {
                        offset += 1;
                        break;
                    }
                    offset += 1 + block_size;
                }
            }
            // Image descriptor
            0x2C => {
                frame_count += 1;
                if offset + 10 >= data.len() {
                    break;
                }
                let image_packed = data[offset + 9];
                let has_local_color_table = (image_packed & 0x80) != 0;
                let local_color_table_size = if has_local_color_table {
                    3 * (1 << ((image_packed & 0x07) + 1))
                } else {
                    0
                };
                offset += 10 + local_color_table_size as usize;
                // Skip LZW minimum code size
                if offset >= data.len() {
                    break;
                }
                offset += 1;
                // Skip sub-blocks
                while offset < data.len() {
                    let block_size = data[offset] as usize;
                    if block_size == 0 {
                        offset += 1;
                        break;
                    }
                    offset += 1 + block_size;
                }
            }
            // Trailer
            0x3B => break,
            _ => break,
        }
    }

    result.is_animated = Some(frame_count > 1);

    result
}

// ---------------------------------------------------------------------------
// BMP
// ---------------------------------------------------------------------------

fn parse_bmp(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "bmp".to_string();
    result.is_animated = Some(false);

    // BMP header: 14-byte file header + DIB header
    // DIB header starts at offset 14
    if data.len() < 26 {
        return result;
    }

    let dib_header_size = read_u32_little_endian(data, 14);

    // BITMAPCOREHEADER (12 bytes) uses 16-bit width/height
    if dib_header_size == 12 {
        if data.len() >= 26 {
            result.width = Some(read_u16_little_endian(data, 18) as u32);
            result.height = Some(read_u16_little_endian(data, 20) as u32);
            result.bit_depth = Some(read_u16_little_endian(data, 24) as u32);
        }
    } else if dib_header_size >= 40 {
        // BITMAPINFOHEADER and larger: 32-bit width/height (signed)
        if data.len() >= 30 {
            let width = read_u32_little_endian(data, 18);
            let height_raw = read_u32_little_endian(data, 22);
            // Height can be negative (top-down bitmap), take absolute value
            let height = if height_raw > 0x7FFFFFFF {
                (height_raw as i32).unsigned_abs()
            } else {
                height_raw
            };
            result.width = Some(width);
            result.height = Some(height);
            result.bit_depth = Some(read_u16_little_endian(data, 28) as u32);
        }
    }

    // Determine color type from bit depth
    if let Some(bit_depth) = result.bit_depth {
        let (color_type, has_alpha) = match bit_depth {
            1 | 4 | 8 => ("indexed", false),
            16 => ("rgb", false),
            24 => ("rgb", false),
            32 => ("rgba", true),
            _ => ("unknown", false),
        };
        result.color_type = Some(color_type.to_string());
        result.has_alpha = Some(has_alpha);
    }

    result
}

// ---------------------------------------------------------------------------
// WebP
// ---------------------------------------------------------------------------

fn parse_webp(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "webp".to_string();
    result.is_animated = Some(false);
    result.bit_depth = Some(8);

    if data.len() < 16 {
        return result;
    }

    // After RIFF header (12 bytes), look at the first chunk
    let chunk_fourcc = &data[12..16];

    // VP8 (lossy)
    if chunk_fourcc == b"VP8 " && data.len() >= 30 {
        // VP8 bitstream header starts after chunk header (8 bytes from chunk start)
        let vp8_start = 20;
        // Check for VP8 frame tag (3 bytes: 0x9D 0x01 0x2A)
        if data.len() > vp8_start + 9
            && data[vp8_start + 3] == 0x9D
            && data[vp8_start + 4] == 0x01
            && data[vp8_start + 5] == 0x2A
        {
            let width = read_u16_little_endian(data, vp8_start + 6) & 0x3FFF;
            let height = read_u16_little_endian(data, vp8_start + 8) & 0x3FFF;
            result.width = Some(width as u32);
            result.height = Some(height as u32);
            result.color_type = Some("rgb".to_string());
            result.has_alpha = Some(false);
        }
    }

    // VP8L (lossless)
    if chunk_fourcc == b"VP8L" && data.len() >= 25 {
        let vp8l_start = 20;
        // VP8L signature byte: 0x2F
        if data[vp8l_start] == 0x2F {
            let b1 = data[vp8l_start + 1] as u32;
            let b2 = data[vp8l_start + 2] as u32;
            let b3 = data[vp8l_start + 3] as u32;
            let b4 = data[vp8l_start + 4] as u32;

            // Width is 14 bits starting at bit 0 of the 32-bit value
            // Height is 14 bits starting at bit 14
            let bits = b1 | (b2 << 8) | (b3 << 16) | (b4 << 24);
            let width = (bits & 0x3FFF) + 1;
            let height = ((bits >> 14) & 0x3FFF) + 1;

            result.width = Some(width);
            result.height = Some(height);
            result.has_alpha = Some((bits >> 28) & 1 == 1);
            result.color_type = Some(if (bits >> 28) & 1 == 1 { "rgba" } else { "rgb" }.to_string());
        }
    }

    // VP8X (extended format)
    if chunk_fourcc == b"VP8X" && data.len() >= 30 {
        let flags = data[20];
        let has_alpha = (flags & 0x10) != 0;
        let is_animated = (flags & 0x02) != 0;

        // Canvas size is at bytes 24-29 (3 bytes width, 3 bytes height, each +1)
        let width = (data[24] as u32) | ((data[25] as u32) << 8) | ((data[26] as u32) << 16);
        let height = (data[27] as u32) | ((data[28] as u32) << 8) | ((data[29] as u32) << 16);

        result.width = Some(width + 1);
        result.height = Some(height + 1);
        result.has_alpha = Some(has_alpha);
        result.is_animated = Some(is_animated);
        result.color_type = Some(if has_alpha { "rgba" } else { "rgb" }.to_string());
    }

    result
}

// ---------------------------------------------------------------------------
// TIFF
// ---------------------------------------------------------------------------

fn parse_tiff(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "tiff".to_string();
    result.is_animated = Some(false);

    let little_endian = data[0] == 0x49; // II = little endian
    result.byte_order = Some(if little_endian { "little_endian" } else { "big_endian" }.to_string());

    if data.len() < 8 {
        return result;
    }

    let ifd_offset = read_u32(data, 4, little_endian) as usize;
    if ifd_offset + 2 > data.len() {
        return result;
    }

    let entry_count = read_u16(data, ifd_offset, little_endian) as usize;
    let mut position = ifd_offset + 2;

    for _ in 0..entry_count {
        if position + 12 > data.len() {
            break;
        }

        let tag = read_u16(data, position, little_endian);
        let data_type = read_u16(data, position + 2, little_endian);
        let value = match data_type {
            3 => read_u16(data, position + 8, little_endian) as u32, // SHORT
            _ => read_u32(data, position + 8, little_endian),        // LONG or other
        };

        match tag {
            // ImageWidth (0x0100)
            0x0100 => result.width = Some(value),
            // ImageLength (0x0101)
            0x0101 => result.height = Some(value),
            // BitsPerSample (0x0102)
            0x0102 => result.bit_depth = Some(value),
            // PhotometricInterpretation (0x0106)
            0x0106 => {
                result.color_type = Some(match value {
                    0 => "grayscale_inverted".to_string(),
                    1 => "grayscale".to_string(),
                    2 => "rgb".to_string(),
                    3 => "indexed".to_string(),
                    _ => format!("photometric_{}", value),
                });
            }
            // SamplesPerPixel (0x0115) -- helps determine alpha
            0x0115 => {
                // If samples > channels expected by color type, there's alpha
                result.has_alpha = Some(value > 3);
            }
            _ => {}
        }

        position += 12;
    }

    // Default has_alpha if not set
    if result.has_alpha.is_none() {
        result.has_alpha = Some(false);
    }

    // Extract textual EXIF metadata (reuses the same TIFF/IFD structure)
    if let Some(exif_data) = exif::parse_exif(data) {
        result.exif = Some(exif_data.to_json());
    }

    result
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

fn is_svg(data: &[u8]) -> bool {
    let text = match std::str::from_utf8(data.get(..512.min(data.len())).unwrap_or(data)) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let trimmed = text.trim_start();
    trimmed.starts_with("<svg") || trimmed.starts_with("<?xml")
}

fn parse_svg(data: &[u8]) -> FormatResult {
    let mut result = FormatResult::unknown();
    result.format = "svg".to_string();
    result.is_animated = Some(false);
    result.has_alpha = Some(true); // SVG supports transparency inherently
    result.color_type = Some("vector".to_string());

    let text = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(_) => return result,
    };

    // Find the <svg ...> tag
    let svg_tag_start = match text.find("<svg") {
        Some(position) => position,
        None => return result,
    };

    let svg_tag_end = match text[svg_tag_start..].find('>') {
        Some(position) => svg_tag_start + position,
        None => return result,
    };

    let svg_tag = &text[svg_tag_start..=svg_tag_end];

    // Extract width attribute
    if let Some(width) = extract_svg_attribute(svg_tag, "width") {
        if let Some(numeric_value) = parse_svg_dimension(&width) {
            result.width = Some(numeric_value);
        }
    }

    // Extract height attribute
    if let Some(height) = extract_svg_attribute(svg_tag, "height") {
        if let Some(numeric_value) = parse_svg_dimension(&height) {
            result.height = Some(numeric_value);
        }
    }

    // Extract viewBox attribute
    if let Some(viewbox) = extract_svg_attribute(svg_tag, "viewBox") {
        result.viewbox = Some(viewbox);
    }

    result
}

fn extract_svg_attribute(tag: &str, attribute_name: &str) -> Option<String> {
    // Match attribute_name="value" or attribute_name='value'
    let patterns = [
        format!("{}=\"", attribute_name),
        format!("{}='", attribute_name),
    ];

    for pattern in &patterns {
        if let Some(start) = tag.find(pattern.as_str()) {
            let value_start = start + pattern.len();
            let delimiter = if pattern.ends_with('"') { '"' } else { '\'' };
            if let Some(end) = tag[value_start..].find(delimiter) {
                return Some(tag[value_start..value_start + end].to_string());
            }
        }
    }
    None
}

fn parse_svg_dimension(value: &str) -> Option<u32> {
    // Strip known CSS units, then parse the numeric part
    let stripped = value
        .trim()
        .trim_end_matches("px")
        .trim_end_matches("pt")
        .trim_end_matches("em")
        .trim_end_matches("rem")
        .trim_end_matches("cm")
        .trim_end_matches("mm")
        .trim_end_matches("in")
        .trim_end_matches('%');
    stripped.parse::<f64>().ok().map(|v| v as u32)
}

// ===========================================================================
// Tests
// ===========================================================================

