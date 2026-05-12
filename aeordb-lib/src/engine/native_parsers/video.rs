/// Native video metadata parser.
///
/// Ported from `aeordb-plugin-parser-video`.



pub fn parse(data: &[u8], filename: &str, content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    
    

    let mut result = VideoMetadata {
        filename: filename.to_string(),
        content_type: content_type.to_string(),
        size,
        format: None,
        brand: None,
        duration_seconds: None,
        width: None,
        height: None,
        frame_rate: None,
        has_audio: None,
        has_video: None,
        video_codec: None,
        audio_codec: None,
        tags: None,
    };

    if data.len() >= 8 && (&data[4..8] == b"ftyp" || is_mp4_box_type(&data[4..8])) {
        parse_mp4(data, &mut result);
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"AVI " {
        parse_avi(data, &mut result);
    } else if data.len() >= 4 && data[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        parse_ebml(data, &mut result);
    } else if data.len() >= 3 && &data[0..3] == b"FLV" {
        parse_flv(data, &mut result);
    }

    let mut metadata = serde_json::json!({
        "filename": result.filename,
        "content_type": result.content_type,
        "size": result.size,
        "format": result.format,
        "brand": result.brand,
        "duration_seconds": result.duration_seconds,
        "width": result.width,
        "height": result.height,
        "frame_rate": result.frame_rate,
        "has_audio": result.has_audio,
        "has_video": result.has_video,
        "video_codec": result.video_codec,
        "audio_codec": result.audio_codec,
    });

    if let Some(tags) = result.tags {
        metadata["tags"] = tags;
    }

    Ok(serde_json::json!({
        "text": "",
        "metadata": metadata,
    }))
}

struct VideoMetadata {
    filename: String,
    content_type: String,
    size: u64,
    format: Option<String>,
    brand: Option<String>,
    duration_seconds: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<f64>,
    has_audio: Option<bool>,
    has_video: Option<bool>,
    video_codec: Option<String>,
    audio_codec: Option<String>,
    tags: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_u32_be(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > data.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

#[allow(dead_code)]
fn read_u16_be(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(u16::from_be_bytes([data[offset], data[offset + 1]]))
}

fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > data.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

fn read_u64_be(data: &[u8], offset: usize) -> Option<u64> {
    if offset + 8 > data.len() {
        return None;
    }
    Some(u64::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]))
}

/// Check if a 4-byte slice is a known top-level MP4 box type.
fn is_mp4_box_type(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot"
    )
}

// ---------------------------------------------------------------------------
// MP4 / MOV (ISO BMFF)
// ---------------------------------------------------------------------------

/// Iterate top-level ISO BMFF boxes. Callback receives (box_type, box_data, offset).
fn iter_boxes(data: &[u8], mut callback: impl FnMut(&[u8; 4], &[u8], usize) -> bool) {
    let mut offset: usize = 0;
    // Safety limit to prevent infinite loops on malformed data
    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 10_000;

    while offset + 8 <= data.len() && iterations < MAX_ITERATIONS {
        iterations += 1;

        let size = match read_u32_be(data, offset) {
            Some(s) => s as u64,
            None => break,
        };

        let box_type: [u8; 4] = match data.get(offset + 4..offset + 8) {
            Some(slice) => [slice[0], slice[1], slice[2], slice[3]],
            None => break,
        };

        let (header_size, box_size) = if size == 1 {
            // 64-bit extended size
            match read_u64_be(data, offset + 8) {
                Some(extended) => (16_usize, extended),
                None => break,
            }
        } else if size == 0 {
            // Box extends to end of data
            (8_usize, (data.len() - offset) as u64)
        } else {
            (8_usize, size)
        };

        if box_size < header_size as u64 {
            break;
        }

        let box_end = offset.saturating_add(box_size as usize).min(data.len());
        let box_data_start = offset + header_size;

        if box_data_start > box_end {
            break;
        }

        let box_data = &data[box_data_start..box_end];

        if !callback(&box_type, box_data, offset) {
            break;
        }

        let next_offset = offset.saturating_add(box_size as usize);
        if next_offset <= offset {
            break;
        }
        offset = next_offset;
    }
}

fn parse_mp4(data: &[u8], result: &mut VideoMetadata) {
    result.format = Some("mp4".to_string());
    result.has_video = Some(true);

    // Parse ftyp for brand
    iter_boxes(data, |box_type, box_data, _| {
        if box_type == b"ftyp" && box_data.len() >= 4 {
            let brand = String::from_utf8_lossy(&box_data[0..4])
                .trim()
                .to_string();
            result.brand = Some(brand.clone());

            // Refine format based on brand (already trimmed)
            if brand == "qt" || brand.starts_with("qt") {
                result.format = Some("mov".to_string());
            } else if brand == "M4V" || brand == "M4VP" {
                result.format = Some("m4v".to_string());
            } else if brand == "M4A" {
                result.format = Some("m4a".to_string());
            }
        }
        true
    });

    // Parse moov for duration, dimensions, tracks
    iter_boxes(data, |box_type, box_data, _| {
        if box_type == b"moov" {
            parse_moov(box_data, result);
        }
        true
    });
}

fn parse_moov(data: &[u8], result: &mut VideoMetadata) {
    iter_boxes(data, |box_type, box_data, _| {
        match box_type {
            b"mvhd" => parse_mvhd(box_data, result),
            b"trak" => parse_trak(box_data, result),
            b"udta" => parse_udta(box_data, result),
            _ => {}
        }
        true
    });
}

fn parse_udta(data: &[u8], result: &mut VideoMetadata) {
    iter_boxes(data, |box_type, box_data, _| {
        if box_type == b"meta" {
            // `meta` is a "full box" — first 4 bytes are version (1) + flags (3)
            if box_data.len() > 4 {
                parse_meta_ilst(&box_data[4..], result);
            }
        }
        true
    });
}

fn parse_meta_ilst(data: &[u8], result: &mut VideoMetadata) {
    iter_boxes(data, |box_type, box_data, _| {
        if box_type == b"ilst" {
            parse_ilst(box_data, result);
        }
        true
    });
}

fn parse_ilst(data: &[u8], result: &mut VideoMetadata) {
    let mut tags = serde_json::Map::new();

    iter_boxes(data, |box_type, box_data, _| {
        // iTunes atoms use © prefix encoded as two bytes: 0xA9 + ASCII char
        let key = match box_type {
            [0xA9, b'n', b'a', b'm'] => Some("title"),
            [0xA9, b'A', b'R', b'T'] => Some("artist"),
            [0xA9, b'a', b'l', b'b'] => Some("album"),
            [0xA9, b'c', b'm', b't'] => Some("comment"),
            [0xA9, b'd', b'a', b'y'] => Some("year"),
            [0xA9, b'g', b'e', b'n'] => Some("genre"),
            [0xA9, b't', b'o', b'o'] => Some("encoder"),
            b"desc" => Some("description"),
            b"cprt" => Some("copyright"),
            _ => None,
        };

        if let Some(key) = key {
            if let Some(text) = extract_ilst_text(box_data) {
                tags.insert(key.to_string(), serde_json::json!(text));
            }
        }
        true
    });

    if !tags.is_empty() {
        result.tags = Some(serde_json::Value::Object(tags));
    }
}

/// Extract text from an ilst atom's `data` sub-atom.
///
/// Each ilst child contains a `data` atom with:
///   - 4 bytes: type indicator (1 = UTF-8 text)
///   - 4 bytes: locale (usually 0)
///   - N bytes: the actual text
fn extract_ilst_text(atom_data: &[u8]) -> Option<String> {
    let mut result = None;
    iter_boxes(atom_data, |box_type, box_data, _| {
        if box_type == b"data" && box_data.len() > 8 {
            let text_bytes = &box_data[8..];
            // Strip trailing nulls
            let trimmed = text_bytes.iter()
                .rposition(|&b| b != 0)
                .map(|last| &text_bytes[..=last])
                .unwrap_or(text_bytes);
            if !trimmed.is_empty() {
                result = Some(String::from_utf8_lossy(trimmed).into_owned());
            }
        }
        true
    });
    result
}

fn parse_mvhd(data: &[u8], result: &mut VideoMetadata) {
    if data.is_empty() {
        return;
    }

    let version = data[0];

    if version == 0 {
        // Version 0: 4-byte fields
        // [version: 1][flags: 3][creation_time: 4][modification_time: 4][timescale: 4][duration: 4]
        if data.len() < 20 {
            return;
        }
        let timescale = match read_u32_be(data, 12) {
            Some(t) if t > 0 => t,
            _ => return,
        };
        let duration = match read_u32_be(data, 16) {
            Some(d) => d as u64,
            None => return,
        };
        result.duration_seconds = Some(duration as f64 / timescale as f64);
    } else if version == 1 {
        // Version 1: 8-byte fields
        // [version: 1][flags: 3][creation_time: 8][modification_time: 8][timescale: 4][duration: 8]
        if data.len() < 32 {
            return;
        }
        let timescale = match read_u32_be(data, 20) {
            Some(t) if t > 0 => t,
            _ => return,
        };
        let duration = match read_u64_be(data, 24) {
            Some(d) => d,
            None => return,
        };
        result.duration_seconds = Some(duration as f64 / timescale as f64);
    }
}

fn parse_trak(data: &[u8], result: &mut VideoMetadata) {
    let mut track_is_video = false;
    let mut track_is_audio = false;

    // First pass: check handler type in mdia->hdlr
    iter_boxes(data, |box_type, box_data, _| {
        if box_type == b"mdia" {
            iter_boxes(box_data, |inner_type, inner_data, _| {
                if inner_type == b"hdlr"
                    && inner_data.len() >= 12 {
                        // [version: 1][flags: 3][pre_defined: 4][handler_type: 4]
                        let handler = &inner_data[8..12];
                        if handler == b"vide" {
                            track_is_video = true;
                        } else if handler == b"soun" {
                            track_is_audio = true;
                        }
                    }
                true
            });
        }
        true
    });

    if track_is_video {
        result.has_video = Some(true);
    }
    if track_is_audio {
        result.has_audio = Some(true);
    }

    // Second pass: extract tkhd dimensions (only for video tracks)
    iter_boxes(data, |box_type, box_data, _| {
        if box_type == b"tkhd" {
            parse_tkhd(box_data, result, track_is_video);
        }
        true
    });
}

fn parse_tkhd(data: &[u8], result: &mut VideoMetadata, is_video: bool) {
    if data.is_empty() {
        return;
    }

    let version = data[0];

    // Width and height are stored as 16.16 fixed-point at the end of tkhd
    let (width_offset, height_offset) = if version == 0 {
        // Version 0: tkhd is 80 bytes of data after version+flags
        // width at offset 76, height at offset 80 (from start of box data)
        (76_usize, 80_usize)
    } else {
        // Version 1: tkhd is 92 bytes of data after version+flags
        // width at offset 88, height at offset 92
        (88_usize, 92_usize)
    };

    if !is_video {
        return;
    }

    if let Some(width_fixed) = read_u32_be(data, width_offset) {
        let width = width_fixed >> 16;
        if width > 0 {
            result.width = Some(width);
        }
    }

    if let Some(height_fixed) = read_u32_be(data, height_offset) {
        let height = height_fixed >> 16;
        if height > 0 {
            result.height = Some(height);
        }
    }
}

// ---------------------------------------------------------------------------
// AVI (RIFF)
// ---------------------------------------------------------------------------

fn parse_avi(data: &[u8], result: &mut VideoMetadata) {
    result.format = Some("avi".to_string());
    result.has_video = Some(true);
    result.has_audio = Some(true);

    // Find hdrl list, then avih chunk inside it
    // RIFF structure: [RIFF: 4][size: 4][AVI : 4][chunks...]
    // Each chunk: [type: 4][size: 4][data...]
    // LIST chunks: [LIST: 4][size: 4][list_type: 4][sub-chunks...]
    let mut offset: usize = 12; // Skip RIFF header + AVI type

    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 10_000;

    while offset + 8 <= data.len() && iterations < MAX_ITERATIONS {
        iterations += 1;

        let chunk_type = match data.get(offset..offset + 4) {
            Some(slice) => [slice[0], slice[1], slice[2], slice[3]],
            None => break,
        };

        let chunk_size = match read_u32_le(data, offset + 4) {
            Some(s) => s as usize,
            None => break,
        };

        if &chunk_type == b"LIST"
            && offset + 12 <= data.len() {
                let list_type = &data[offset + 8..offset + 12];
                if list_type == b"hdrl" {
                    parse_avi_hdrl(&data[offset + 12..], chunk_size.saturating_sub(4), result);
                    return;
                }
            }

        // Advance: 8 (header) + chunk_size, padded to even boundary
        let padded_size = (chunk_size + 1) & !1;
        let next_offset = offset + 8 + padded_size;
        if next_offset <= offset {
            break;
        }
        offset = next_offset;
    }
}

fn parse_avi_hdrl(data: &[u8], max_size: usize, result: &mut VideoMetadata) {
    let limit = max_size.min(data.len());
    let mut offset: usize = 0;

    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 10_000;

    while offset + 8 <= limit && iterations < MAX_ITERATIONS {
        iterations += 1;

        let chunk_type = match data.get(offset..offset + 4) {
            Some(slice) => [slice[0], slice[1], slice[2], slice[3]],
            None => break,
        };

        let chunk_size = match read_u32_le(data, offset + 4) {
            Some(s) => s as usize,
            None => break,
        };

        if &chunk_type == b"avih" && offset + 8 + chunk_size <= limit {
            parse_avih(&data[offset + 8..offset + 8 + chunk_size], result);
            return;
        }

        let padded_size = (chunk_size + 1) & !1;
        let next_offset = offset + 8 + padded_size;
        if next_offset <= offset {
            break;
        }
        offset = next_offset;
    }
}

fn parse_avih(data: &[u8], result: &mut VideoMetadata) {
    // avih structure (all u32 LE):
    // [0]  microseconds_per_frame
    // [4]  max_bytes_per_second
    // [8]  padding_granularity
    // [12] flags
    // [16] total_frames
    // [20] initial_frames
    // [24] streams
    // [28] suggested_buffer_size
    // [32] width
    // [36] height
    if data.len() < 40 {
        return;
    }

    let microseconds_per_frame = match read_u32_le(data, 0) {
        Some(v) if v > 0 => v,
        _ => return,
    };

    let total_frames = match read_u32_le(data, 16) {
        Some(v) => v,
        None => return,
    };

    let width = match read_u32_le(data, 32) {
        Some(v) => v,
        None => return,
    };

    let height = match read_u32_le(data, 36) {
        Some(v) => v,
        None => return,
    };

    result.frame_rate = Some(1_000_000.0 / microseconds_per_frame as f64);
    result.duration_seconds =
        Some(total_frames as f64 * microseconds_per_frame as f64 / 1_000_000.0);
    result.width = Some(width);
    result.height = Some(height);
}

// ---------------------------------------------------------------------------
// WebM / MKV (EBML)
// ---------------------------------------------------------------------------

/// Read an EBML variable-length integer (VINT). Returns (value, bytes_consumed).
fn read_vint(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }

    let first = data[0];
    if first == 0 {
        return None;
    }

    let length = first.leading_zeros() as usize + 1;
    if length > 8 || length > data.len() {
        return None;
    }

    let mut value = (first as u64) & ((1 << (8 - length)) - 1);
    for i in 1..length {
        value = (value << 8) | (data[i] as u64);
    }

    Some((value, length))
}

/// Read an EBML element ID. Returns (element_id_bytes, bytes_consumed).
fn read_ebml_element_id(data: &[u8]) -> Option<(u64, usize)> {
    if data.is_empty() {
        return None;
    }

    let first = data[0];
    if first == 0 {
        return None;
    }

    let length = first.leading_zeros() as usize + 1;
    if length > 4 || length > data.len() {
        return None;
    }

    // For IDs we keep the leading bits (VINT marker included)
    let mut value = first as u64;
    for i in 1..length {
        value = (value << 8) | (data[i] as u64);
    }

    Some((value, length))
}

fn parse_ebml(data: &[u8], result: &mut VideoMetadata) {
    // The first element should be the EBML header (ID 0x1A45DFA3)
    // Parse it to find the DocType which tells us webm vs matroska

    // Read the EBML header element
    let (element_id, id_len) = match read_ebml_element_id(data) {
        Some(v) => v,
        None => return,
    };

    if element_id != 0x1A45DFA3 {
        return;
    }

    let remaining = &data[id_len..];
    let (header_size, size_len) = match read_vint(remaining) {
        Some(v) => v,
        None => {
            // Truncated, but we confirmed the EBML ID
            result.format = Some("matroska".to_string());
            result.has_video = Some(true);
            return;
        }
    };

    let header_data_start = id_len + size_len;
    let header_data_end = header_data_start + header_size as usize;

    if header_data_end > data.len() {
        // Truncated, but we detected the format
        result.format = Some("matroska".to_string());
        return;
    }

    let header_data = &data[header_data_start..header_data_end];

    // Search for DocType element (ID 0x4282) inside the EBML header
    let mut offset = 0;
    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 1000;

    while offset < header_data.len() && iterations < MAX_ITERATIONS {
        iterations += 1;

        let (child_id, child_id_len) = match read_ebml_element_id(&header_data[offset..]) {
            Some(v) => v,
            None => break,
        };

        let (child_size, child_size_len) = match read_vint(&header_data[offset + child_id_len..]) {
            Some(v) => v,
            None => break,
        };

        let child_data_start = offset + child_id_len + child_size_len;
        let child_data_end = child_data_start + child_size as usize;

        if child_data_end > header_data.len() {
            break;
        }

        // DocType element ID = 0x4282
        if child_id == 0x4282 {
            let doc_type =
                String::from_utf8_lossy(&header_data[child_data_start..child_data_end])
                    .trim_end_matches('\0')
                    .to_string();

            if doc_type == "webm" {
                result.format = Some("webm".to_string());
            } else if doc_type == "matroska" {
                result.format = Some("matroska".to_string());
            } else {
                result.format = Some(doc_type);
            }

            result.has_video = Some(true);
            return;
        }

        offset = child_data_end;
    }

    // If we got here we parsed a valid EBML header but didn't find DocType
    result.format = Some("matroska".to_string());
    result.has_video = Some(true);
}

// ---------------------------------------------------------------------------
// FLV
// ---------------------------------------------------------------------------

fn parse_flv(data: &[u8], result: &mut VideoMetadata) {
    result.format = Some("flv".to_string());

    // FLV header: [F L V][version: 1][flags: 1][header_size: 4 (BE)]
    if data.len() < 9 {
        return;
    }

    let flags = data[4];
    result.has_audio = Some(flags & 0x04 != 0);
    result.has_video = Some(flags & 0x01 != 0);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

