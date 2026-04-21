/// Native audio metadata parser.
///
/// Ported from `aeordb-plugin-parser-audio`.

use serde_json::json;



pub fn parse(data: &[u8], filename: &str, content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    
    
    
    

    let format = detect_format(data, filename);

    let mut metadata = json!({
        "filename": filename,
        "content_type": content_type,
        "size": size,
        "format": format,
        "duration_seconds": serde_json::Value::Null,
        "sample_rate": serde_json::Value::Null,
        "channels": serde_json::Value::Null,
        "bitrate": serde_json::Value::Null,
        "bits_per_sample": serde_json::Value::Null,
    });

    match format {
        "mp3" => parse_mp3(data, size, &mut metadata),
        "wav" => parse_wav(data, &mut metadata),
        "ogg" => parse_ogg(data, &mut metadata),
        _ => {}
    }

    Ok(json!({
        "text": "",
        "metadata": metadata,
    }))
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

fn detect_format<'a>(data: &[u8], filename: &str) -> &'a str {
    // Check magic bytes first
    if data.len() >= 3 && &data[0..3] == b"ID3" {
        return "mp3";
    }
    if data.len() >= 2 {
        let sync = u16::from_be_bytes([data[0], data[1]]);
        if sync & 0xFFE0 == 0xFFE0 {
            return "mp3";
        }
    }
    if data.len() >= 12
        && &data[0..4] == b"RIFF"
        && &data[8..12] == b"WAVE"
    {
        return "wav";
    }
    if data.len() >= 4 && &data[0..4] == b"OggS" {
        return "ogg";
    }

    // Fall back to file extension
    let extension = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match extension.as_str() {
        "mp3" => "mp3",
        "wav" => "wav",
        "ogg" | "oga" => "ogg",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// MP3 parsing
// ---------------------------------------------------------------------------

/// MP3 bitrate table for MPEG1 Layer III (kbps), indexed by 4-bit value.
/// Index 0 = free, index 15 = bad. Values are in kbps.
const MP3_BITRATES_MPEG1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];

/// MP3 sample rate table for MPEG1, indexed by 2-bit value.
const MP3_SAMPLE_RATES_MPEG1: [u32; 4] = [44100, 48000, 32000, 0];

fn parse_mp3(data: &[u8], file_size: u64, metadata: &mut serde_json::Value) {
    let mut tags = serde_json::Map::new();

    // Try ID3v2 at the start
    let mut audio_start: usize = 0;
    if data.len() >= 10 && &data[0..3] == b"ID3" {
        let id3v2_size = parse_id3v2_tags(data, &mut tags);
        audio_start = id3v2_size;
    }

    // Try ID3v1 at the end (last 128 bytes)
    if data.len() >= 128 {
        let tag_start = data.len() - 128;
        if &data[tag_start..tag_start + 3] == b"TAG" {
            parse_id3v1_tags(&data[tag_start..], &mut tags);
        }
    }

    // Find first valid MP3 frame header after ID3v2
    if let Some(frame_offset) = find_mp3_frame_header(data, audio_start) {
        parse_mp3_frame_header(&data[frame_offset..], file_size, metadata);
    }

    if !tags.is_empty() {
        metadata["tags"] = serde_json::Value::Object(tags);
    }
}

fn find_mp3_frame_header(data: &[u8], start: usize) -> Option<usize> {
    let mut offset = start;
    while offset + 4 <= data.len() {
        let sync = u16::from_be_bytes([data[offset], data[offset + 1]]);
        if sync & 0xFFE0 == 0xFFE0 {
            return Some(offset);
        }
        offset += 1;
    }
    None
}

fn parse_mp3_frame_header(
    header_bytes: &[u8],
    file_size: u64,
    metadata: &mut serde_json::Value,
) {
    if header_bytes.len() < 4 {
        return;
    }

    let byte1 = header_bytes[1];
    let byte2 = header_bytes[2];

    // MPEG version: bits 4-3 of byte1
    let mpeg_version_bits = (byte1 >> 3) & 0x03;
    // Layer: bits 2-1 of byte1
    let layer_bits = (byte1 >> 1) & 0x03;
    // Bitrate index: bits 7-4 of byte2
    let bitrate_index = ((byte2 >> 4) & 0x0F) as usize;
    // Sample rate index: bits 3-2 of byte2
    let sample_rate_index = ((byte2 >> 2) & 0x03) as usize;
    // Channel mode: bits 7-6 of byte3
    let channel_mode = (header_bytes[3] >> 6) & 0x03;

    // Only handle MPEG1 Layer III for now (most common)
    if mpeg_version_bits == 3 && layer_bits == 1 {
        if bitrate_index > 0 && bitrate_index < 15 {
            let bitrate_kbps = MP3_BITRATES_MPEG1_L3[bitrate_index];
            let bitrate_bps = bitrate_kbps * 1000;
            metadata["bitrate"] = json!(bitrate_bps);

            if bitrate_bps > 0 {
                let duration = file_size as f64 / (bitrate_bps as f64 / 8.0);
                metadata["duration_seconds"] = json!(duration);
            }
        }

        if sample_rate_index < 3 {
            let sample_rate = MP3_SAMPLE_RATES_MPEG1[sample_rate_index];
            metadata["sample_rate"] = json!(sample_rate);
        }
    }

    let channels: u32 = if channel_mode == 3 { 1 } else { 2 };
    metadata["channels"] = json!(channels);
}

// ---------------------------------------------------------------------------
// ID3v2
// ---------------------------------------------------------------------------

/// Parse ID3v2 header and frames. Returns total tag size (header + body)
/// so the caller knows where audio data begins.
fn parse_id3v2_tags(data: &[u8], tags: &mut serde_json::Map<String, serde_json::Value>) -> usize {
    if data.len() < 10 {
        return 0;
    }

    let _version_major = data[3];
    let _version_minor = data[4];
    let _flags = data[5];

    // Tag size is 4 bytes of synchsafe integer (7 bits per byte)
    let tag_body_size = synchsafe_to_u32(&data[6..10]) as usize;
    let total_tag_size = 10 + tag_body_size;

    if data.len() < total_tag_size {
        return total_tag_size;
    }

    // Parse frames within the tag body
    let mut offset: usize = 10;
    while offset + 10 <= total_tag_size && offset + 10 <= data.len() {
        let frame_id = &data[offset..offset + 4];

        // Stop if we hit padding (all zeros)
        if frame_id == b"\0\0\0\0" {
            break;
        }

        let frame_size = u32::from_be_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;

        // Skip the 2-byte flags
        let frame_data_start = offset + 10;
        let frame_data_end = frame_data_start + frame_size;

        if frame_data_end > total_tag_size || frame_data_end > data.len() {
            break;
        }

        let frame_data = &data[frame_data_start..frame_data_end];
        let key = match frame_id {
            b"TIT2" => Some("title"),
            b"TPE1" => Some("artist"),
            b"TALB" => Some("album"),
            b"TYER" => Some("year"),
            b"TDRC" => Some("year"),
            b"TCON" => Some("genre"),
            b"TRCK" => Some("track"),
            b"COMM" => Some("comment"),
            _ => None,
        };

        if let Some(key) = key {
            if frame_id == b"COMM" {
                if let Some(value) = decode_id3v2_comment_frame(frame_data) {
                    tags.insert(key.to_string(), json!(value));
                }
            } else if let Some(value) = decode_id3v2_text_frame(frame_data) {
                tags.insert(key.to_string(), json!(value));
            }
        }

        offset = frame_data_end;
    }

    total_tag_size
}

fn synchsafe_to_u32(bytes: &[u8]) -> u32 {
    ((bytes[0] as u32) << 21)
        | ((bytes[1] as u32) << 14)
        | ((bytes[2] as u32) << 7)
        | (bytes[3] as u32)
}

fn decode_id3v2_text_frame(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }

    let encoding = data[0];
    let text_bytes = &data[1..];

    match encoding {
        // ISO-8859-1 / ASCII
        0 => {
            let text: String = text_bytes
                .iter()
                .take_while(|&&byte| byte != 0)
                .map(|&byte| byte as char)
                .collect();
            if text.is_empty() { None } else { Some(text) }
        }
        // UTF-16 with BOM
        1 => decode_utf16_with_bom(text_bytes),
        // UTF-16BE without BOM
        2 => decode_utf16_be(text_bytes),
        // UTF-8
        3 => {
            let text = String::from_utf8_lossy(text_bytes)
                .trim_end_matches('\0')
                .to_string();
            if text.is_empty() { None } else { Some(text) }
        }
        _ => None,
    }
}

fn decode_utf16_with_bom(data: &[u8]) -> Option<String> {
    if data.len() < 2 {
        return None;
    }

    let (big_endian, text_data) = if data[0] == 0xFE && data[1] == 0xFF {
        (true, &data[2..])
    } else if data[0] == 0xFF && data[1] == 0xFE {
        (false, &data[2..])
    } else {
        // No BOM, assume little-endian
        (false, data)
    };

    let code_units: Vec<u16> = text_data
        .chunks_exact(2)
        .map(|chunk| {
            if big_endian {
                u16::from_be_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_le_bytes([chunk[0], chunk[1]])
            }
        })
        .take_while(|&unit| unit != 0)
        .collect();

    let text = String::from_utf16_lossy(&code_units);
    if text.is_empty() { None } else { Some(text) }
}

fn decode_utf16_be(data: &[u8]) -> Option<String> {
    let code_units: Vec<u16> = data
        .chunks_exact(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .take_while(|&unit| unit != 0)
        .collect();

    let text = String::from_utf16_lossy(&code_units);
    if text.is_empty() { None } else { Some(text) }
}

fn decode_id3v2_comment_frame(data: &[u8]) -> Option<String> {
    // COMM frame: encoding(1) + language(3) + short_description(NUL-terminated) + text
    if data.len() < 5 {
        return None;
    }

    let encoding = data[0];
    // Skip language (3 bytes)
    let content = &data[4..];

    match encoding {
        0 => {
            // Find NUL separator between short description and actual comment
            if let Some(nul_position) = content.iter().position(|&byte| byte == 0) {
                let comment_bytes = &content[nul_position + 1..];
                let text: String = comment_bytes
                    .iter()
                    .take_while(|&&byte| byte != 0)
                    .map(|&byte| byte as char)
                    .collect();
                if text.is_empty() { None } else { Some(text) }
            } else {
                // No separator -- whole thing is the comment
                let text: String = content
                    .iter()
                    .take_while(|&&byte| byte != 0)
                    .map(|&byte| byte as char)
                    .collect();
                if text.is_empty() { None } else { Some(text) }
            }
        }
        3 => {
            // UTF-8
            if let Some(nul_position) = content.iter().position(|&byte| byte == 0) {
                let comment_bytes = &content[nul_position + 1..];
                let text = String::from_utf8_lossy(comment_bytes)
                    .trim_end_matches('\0')
                    .to_string();
                if text.is_empty() { None } else { Some(text) }
            } else {
                let text = String::from_utf8_lossy(content)
                    .trim_end_matches('\0')
                    .to_string();
                if text.is_empty() { None } else { Some(text) }
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ID3v1
// ---------------------------------------------------------------------------

fn parse_id3v1_tags(tag_block: &[u8], tags: &mut serde_json::Map<String, serde_json::Value>) {
    if tag_block.len() < 128 || &tag_block[0..3] != b"TAG" {
        return;
    }

    let read_fixed = |start: usize, length: usize| -> Option<String> {
        let slice = &tag_block[start..start + length];
        let text: String = slice
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| byte as char)
            .collect();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    };

    if let Some(value) = read_fixed(3, 30) {
        tags.entry("title").or_insert(json!(value));
    }
    if let Some(value) = read_fixed(33, 30) {
        tags.entry("artist").or_insert(json!(value));
    }
    if let Some(value) = read_fixed(63, 30) {
        tags.entry("album").or_insert(json!(value));
    }
    if let Some(value) = read_fixed(93, 4) {
        tags.entry("year").or_insert(json!(value));
    }
    if let Some(value) = read_fixed(97, 30) {
        tags.entry("comment").or_insert(json!(value));
    }

    // Genre byte
    let genre_byte = tag_block[127];
    if let Some(genre_name) = id3v1_genre_name(genre_byte) {
        tags.entry("genre").or_insert(json!(genre_name));
    }

    // ID3v1.1: if comment byte 28 is 0 and byte 29 is non-zero, byte 29 is track number
    if tag_block[125] == 0 && tag_block[126] != 0 {
        let track = tag_block[126];
        tags.entry("track").or_insert(json!(track.to_string()));
    }
}

fn id3v1_genre_name(index: u8) -> Option<&'static str> {
    const GENRES: [&str; 80] = [
        "Blues", "Classic Rock", "Country", "Dance", "Disco", "Funk", "Grunge",
        "Hip-Hop", "Jazz", "Metal", "New Age", "Oldies", "Other", "Pop", "R&B",
        "Rap", "Reggae", "Rock", "Techno", "Industrial", "Alternative", "Ska",
        "Death Metal", "Pranks", "Soundtrack", "Euro-Techno", "Ambient",
        "Trip-Hop", "Vocal", "Jazz+Funk", "Fusion", "Trance", "Classical",
        "Instrumental", "Acid", "House", "Game", "Sound Clip", "Gospel",
        "Noise", "AlternRock", "Bass", "Soul", "Punk", "Space", "Meditative",
        "Instrumental Pop", "Instrumental Rock", "Ethnic", "Gothic",
        "Darkwave", "Techno-Industrial", "Electronic", "Pop-Folk", "Eurodance",
        "Dream", "Southern Rock", "Comedy", "Cult", "Gangsta", "Top 40",
        "Christian Rap", "Pop/Funk", "Jungle", "Native American", "Cabaret",
        "New Wave", "Psychadelic", "Rave", "Showtunes", "Trailer", "Lo-Fi",
        "Tribal", "Acid Punk", "Acid Jazz", "Polka", "Retro", "Musical",
        "Rock & Roll", "Hard Rock",
    ];

    if (index as usize) < GENRES.len() {
        Some(GENRES[index as usize])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// WAV parsing
// ---------------------------------------------------------------------------

fn parse_wav(data: &[u8], metadata: &mut serde_json::Value) {
    if data.len() < 12 {
        return;
    }

    // Validate RIFF header
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return;
    }

    let mut offset: usize = 12;

    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;

        let chunk_data_start = offset + 8;

        if chunk_id == b"fmt " && chunk_size >= 16 && chunk_data_start + 16 <= data.len() {
            let chunk_data = &data[chunk_data_start..];

            let _audio_format = u16::from_le_bytes([chunk_data[0], chunk_data[1]]);
            let channels = u16::from_le_bytes([chunk_data[2], chunk_data[3]]);
            let sample_rate = u32::from_le_bytes([
                chunk_data[4], chunk_data[5], chunk_data[6], chunk_data[7],
            ]);
            let byte_rate = u32::from_le_bytes([
                chunk_data[8], chunk_data[9], chunk_data[10], chunk_data[11],
            ]);
            let bits_per_sample = u16::from_le_bytes([chunk_data[14], chunk_data[15]]);

            metadata["channels"] = json!(channels);
            metadata["sample_rate"] = json!(sample_rate);
            metadata["bitrate"] = json!(byte_rate * 8);
            metadata["bits_per_sample"] = json!(bits_per_sample);

            // Store byte_rate for duration calculation when we find the data chunk
            metadata["_byte_rate"] = json!(byte_rate);
        }

        if chunk_id == b"data" {
            if let Some(byte_rate) = metadata.get("_byte_rate").and_then(|value| value.as_u64()) {
                if byte_rate > 0 {
                    let duration = chunk_size as f64 / byte_rate as f64;
                    metadata["duration_seconds"] = json!(duration);
                }
            }
        }

        if chunk_id == b"LIST" && chunk_size >= 4 && chunk_data_start + 4 <= data.len() {
            let list_type = &data[chunk_data_start..chunk_data_start + 4];
            if list_type == b"INFO" {
                parse_info_chunks(&data[chunk_data_start + 4..chunk_data_start + chunk_size.min(data.len() - chunk_data_start)], metadata);
            }
        }

        // Advance to next chunk (chunks are 2-byte aligned)
        let padded_size = if chunk_size % 2 == 1 { chunk_size + 1 } else { chunk_size };
        offset = chunk_data_start + padded_size;
    }

    // Remove internal helper field
    if let Some(object) = metadata.as_object_mut() {
        object.remove("_byte_rate");
    }
}

fn parse_info_chunks(data: &[u8], metadata: &mut serde_json::Value) {
    let mut tags = serde_json::Map::new();
    let mut offset: usize = 0;

    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;

        let chunk_data_start = offset + 8;
        let chunk_data_end = (chunk_data_start + chunk_size).min(data.len());

        if chunk_data_start > data.len() {
            break;
        }

        let key = match chunk_id {
            b"INAM" => Some("title"),
            b"IART" => Some("artist"),
            b"ICMT" => Some("comment"),
            b"ICOP" => Some("copyright"),
            b"IGNR" => Some("genre"),
            b"ICRD" => Some("year"),
            b"ISFT" => Some("software"),
            _ => None,
        };

        if let Some(key) = key {
            let text_bytes = &data[chunk_data_start..chunk_data_end];
            // Strip trailing nulls
            let trimmed = text_bytes.iter()
                .rposition(|&b| b != 0)
                .map(|last| &text_bytes[..=last])
                .unwrap_or(&[]);
            if !trimmed.is_empty() {
                if let Ok(text) = std::str::from_utf8(trimmed) {
                    tags.insert(key.to_string(), json!(text.to_string()));
                }
            }
        }

        // Advance to next sub-chunk (2-byte aligned)
        let padded_size = if chunk_size % 2 == 1 { chunk_size + 1 } else { chunk_size };
        offset = chunk_data_start + padded_size;
    }

    if !tags.is_empty() {
        metadata["tags"] = serde_json::Value::Object(tags);
    }
}

// ---------------------------------------------------------------------------
// OGG / Vorbis parsing
// ---------------------------------------------------------------------------

fn parse_ogg(data: &[u8], metadata: &mut serde_json::Value) {
    if data.len() < 27 {
        return;
    }

    // Parse the first Ogg page to find its segments
    let mut offset: usize = 0;

    // We need to collect packets across pages. Vorbis identification = packet 1,
    // vorbis comment = packet 3. Both should be early in the stream.
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut current_packet = Vec::new();

    for _page_number in 0..8 {
        if offset + 27 > data.len() {
            break;
        }

        if &data[offset..offset + 4] != b"OggS" {
            break;
        }

        let segment_count = data[offset + 26] as usize;
        let segment_table_start = offset + 27;
        let segment_table_end = segment_table_start + segment_count;

        if segment_table_end > data.len() {
            break;
        }

        let segment_sizes = &data[segment_table_start..segment_table_end];
        let mut payload_offset = segment_table_end;

        for &segment_size in segment_sizes {
            let size = segment_size as usize;
            if payload_offset + size > data.len() {
                break;
            }

            current_packet.extend_from_slice(&data[payload_offset..payload_offset + size]);
            payload_offset += size;

            // A segment < 255 terminates the packet
            if segment_size < 255 {
                packets.push(std::mem::take(&mut current_packet));
                if packets.len() >= 3 {
                    break;
                }
            }
        }

        if packets.len() >= 3 {
            break;
        }

        // Move to next page
        let total_payload: usize = segment_sizes.iter().map(|&size| size as usize).sum();
        offset = segment_table_end + total_payload;
    }

    // Packet 0: Vorbis identification header
    if !packets.is_empty() {
        parse_vorbis_identification(&packets[0], metadata);
    }

    // Packet 1: Vorbis comment header
    if packets.len() >= 2 {
        parse_vorbis_comment(&packets[1], metadata);
    }
}

fn parse_vorbis_identification(packet: &[u8], metadata: &mut serde_json::Value) {
    // Minimum: packet_type(1) + "vorbis"(6) + version(4) + channels(1) + sample_rate(4) +
    //          bitrate_max(4) + bitrate_nom(4) + bitrate_min(4) = 28 bytes
    if packet.len() < 28 {
        return;
    }

    // Check packet type (1 = identification) and "vorbis" signature
    if packet[0] != 1 || &packet[1..7] != b"vorbis" {
        return;
    }

    let channels = packet[11] as u32;
    let sample_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
    let _bitrate_maximum = i32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]);
    let bitrate_nominal = i32::from_le_bytes([packet[20], packet[21], packet[22], packet[23]]);
    let _bitrate_minimum = i32::from_le_bytes([packet[24], packet[25], packet[26], packet[27]]);

    metadata["channels"] = json!(channels);
    metadata["sample_rate"] = json!(sample_rate);

    if bitrate_nominal > 0 {
        metadata["bitrate"] = json!(bitrate_nominal);
    }
}

fn parse_vorbis_comment(packet: &[u8], metadata: &mut serde_json::Value) {
    // Check packet type (3 = comment) and "vorbis" signature
    if packet.len() < 7 || packet[0] != 3 || &packet[1..7] != b"vorbis" {
        return;
    }

    let mut offset: usize = 7;

    // Vendor string length (little-endian u32)
    if offset + 4 > packet.len() {
        return;
    }
    let vendor_length = u32::from_le_bytes([
        packet[offset], packet[offset + 1], packet[offset + 2], packet[offset + 3],
    ]) as usize;
    offset += 4;

    // Skip vendor string
    offset += vendor_length;

    // Comment count
    if offset + 4 > packet.len() {
        return;
    }
    let comment_count = u32::from_le_bytes([
        packet[offset], packet[offset + 1], packet[offset + 2], packet[offset + 3],
    ]) as usize;
    offset += 4;

    let mut tags = serde_json::Map::new();

    for _ in 0..comment_count {
        if offset + 4 > packet.len() {
            break;
        }
        let comment_length = u32::from_le_bytes([
            packet[offset], packet[offset + 1], packet[offset + 2], packet[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + comment_length > packet.len() {
            break;
        }

        let comment = String::from_utf8_lossy(&packet[offset..offset + comment_length]);
        offset += comment_length;

        if let Some(equals_position) = comment.find('=') {
            let key = comment[..equals_position].to_lowercase();
            let value = &comment[equals_position + 1..];

            let mapped_key = match key.as_str() {
                "title" => "title",
                "artist" => "artist",
                "album" => "album",
                "date" | "year" => "year",
                "genre" => "genre",
                "tracknumber" | "track" => "track",
                "comment" => "comment",
                _ => continue,
            };

            tags.insert(mapped_key.to_string(), json!(value.to_string()));
        }
    }

    if !tags.is_empty() {
        metadata["tags"] = serde_json::Value::Object(tags);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

