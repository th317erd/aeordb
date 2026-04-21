/// Shared EXIF/IFD parser for JPEG and TIFF images.
///
/// Extracts structured metadata from TIFF-format IFD entries:
/// camera info, dates, GPS coordinates, and textual fields
/// (description, artist, copyright, software, user comment).

use serde_json::json;

/// Structured EXIF metadata extracted from IFD entries.
#[derive(Debug, Default)]
pub struct ExifData {
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub orientation: Option<u32>,
    pub date_taken: Option<String>,
    pub image_description: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub software: Option<String>,
    pub user_comment: Option<String>,
    pub gps_latitude: Option<f64>,
    pub gps_longitude: Option<f64>,
}

impl ExifData {
    /// Convert to a serde_json::Value, omitting None fields.
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(ref v) = self.camera_make { map.insert("camera_make".into(), json!(v)); }
        if let Some(ref v) = self.camera_model { map.insert("camera_model".into(), json!(v)); }
        if let Some(v) = self.orientation { map.insert("orientation".into(), json!(v)); }
        if let Some(ref v) = self.date_taken { map.insert("date_taken".into(), json!(v)); }
        if let Some(ref v) = self.image_description { map.insert("image_description".into(), json!(v)); }
        if let Some(ref v) = self.artist { map.insert("artist".into(), json!(v)); }
        if let Some(ref v) = self.copyright { map.insert("copyright".into(), json!(v)); }
        if let Some(ref v) = self.software { map.insert("software".into(), json!(v)); }
        if let Some(ref v) = self.user_comment { map.insert("user_comment".into(), json!(v)); }
        if let Some(v) = self.gps_latitude { map.insert("gps_latitude".into(), json!(v)); }
        if let Some(v) = self.gps_longitude { map.insert("gps_longitude".into(), json!(v)); }
        serde_json::Value::Object(map)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse EXIF data from a TIFF-structured byte buffer.
///
/// For JPEG: pass the APP1 segment data AFTER stripping the `Exif\0\0` prefix.
/// For TIFF: pass the entire file data (TIFF files are natively TIFF/IFD).
///
/// Returns `None` if the data is too small, has invalid byte order, or
/// contains no recognized EXIF tags.
pub fn parse_exif(tiff_data: &[u8]) -> Option<ExifData> {
    if tiff_data.len() < 8 {
        return None;
    }

    let little_endian = match (tiff_data[0], tiff_data[1]) {
        (0x49, 0x49) => true,
        (0x4D, 0x4D) => false,
        _ => return None,
    };

    let magic = read_u16(tiff_data, 2, little_endian);
    if magic != 42 {
        return None;
    }

    let ifd_offset = read_u32(tiff_data, 4, little_endian) as usize;
    if ifd_offset >= tiff_data.len() {
        return None;
    }

    let mut data = ExifData::default();
    let mut gps_ifd_offset: Option<usize> = None;
    let mut exif_ifd_offset: Option<usize> = None;

    // Parse IFD0 (main image tags)
    parse_ifd(tiff_data, ifd_offset, little_endian, &mut data, &mut gps_ifd_offset, &mut exif_ifd_offset);

    // Parse Exif sub-IFD (DateTimeOriginal, UserComment, etc.)
    if let Some(offset) = exif_ifd_offset {
        parse_ifd(tiff_data, offset, little_endian, &mut data, &mut None, &mut None);
    }

    // Parse GPS IFD
    if let Some(offset) = gps_ifd_offset {
        parse_gps_ifd(tiff_data, offset, little_endian, &mut data);
    }

    let is_empty = data.camera_make.is_none()
        && data.camera_model.is_none()
        && data.orientation.is_none()
        && data.date_taken.is_none()
        && data.image_description.is_none()
        && data.artist.is_none()
        && data.copyright.is_none()
        && data.software.is_none()
        && data.user_comment.is_none()
        && data.gps_latitude.is_none()
        && data.gps_longitude.is_none();

    if is_empty { None } else { Some(data) }
}

// ---------------------------------------------------------------------------
// IFD parsing
// ---------------------------------------------------------------------------

fn parse_ifd(
    data: &[u8],
    offset: usize,
    little_endian: bool,
    exif: &mut ExifData,
    gps_ifd_offset: &mut Option<usize>,
    exif_ifd_offset: &mut Option<usize>,
) {
    if offset + 2 > data.len() {
        return;
    }

    let entry_count = read_u16(data, offset, little_endian) as usize;
    let mut position = offset + 2;

    for _ in 0..entry_count {
        if position + 12 > data.len() {
            break;
        }

        let tag = read_u16(data, position, little_endian);
        let data_type = read_u16(data, position + 2, little_endian);
        let count = read_u32(data, position + 4, little_endian) as usize;
        let value_offset_raw = read_u32(data, position + 8, little_endian);

        match tag {
            // ImageDescription (0x010E) — NEW
            0x010E => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.image_description = Some(value);
                }
            }
            // Make (0x010F)
            0x010F => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.camera_make = Some(value);
                }
            }
            // Model (0x0110)
            0x0110 => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.camera_model = Some(value);
                }
            }
            // Orientation (0x0112)
            0x0112 => {
                let orientation = if data_type == 3 {
                    read_u16(data, position + 8, little_endian) as u32
                } else {
                    value_offset_raw
                };
                exif.orientation = Some(orientation);
            }
            // Software (0x0131) — NEW
            0x0131 => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.software = Some(value);
                }
            }
            // DateTime (0x0132)
            0x0132 => {
                if exif.date_taken.is_none() {
                    if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                        exif.date_taken = Some(value);
                    }
                }
            }
            // Artist (0x013B) — NEW
            0x013B => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.artist = Some(value);
                }
            }
            // Copyright (0x8298) — NEW
            0x8298 => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.copyright = Some(value);
                }
            }
            // Exif IFD pointer (0x8769)
            0x8769 => {
                *exif_ifd_offset = Some(value_offset_raw as usize);
            }
            // GPS IFD pointer (0x8825)
            0x8825 => {
                *gps_ifd_offset = Some(value_offset_raw as usize);
            }
            // DateTimeOriginal (0x9003) — Exif sub-IFD, preferred over 0x0132
            0x9003 => {
                if let Some(value) = read_ifd_string(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.date_taken = Some(value);
                }
            }
            // UserComment (0x9286) — Exif sub-IFD, NEW
            0x9286 => {
                if let Some(value) = read_user_comment(data, data_type, count, value_offset_raw as usize, position + 8) {
                    exif.user_comment = Some(value);
                }
            }
            _ => {}
        }

        position += 12;
    }
}

// ---------------------------------------------------------------------------
// GPS IFD parsing
// ---------------------------------------------------------------------------

fn parse_gps_ifd(
    data: &[u8],
    offset: usize,
    little_endian: bool,
    exif: &mut ExifData,
) {
    if offset + 2 > data.len() {
        return;
    }

    let entry_count = read_u16(data, offset, little_endian) as usize;
    let mut position = offset + 2;

    let mut latitude_ref: Option<char> = None;
    let mut longitude_ref: Option<char> = None;
    let mut latitude_values: Option<(f64, f64, f64)> = None;
    let mut longitude_values: Option<(f64, f64, f64)> = None;

    for _ in 0..entry_count {
        if position + 12 > data.len() {
            break;
        }

        let tag = read_u16(data, position, little_endian);
        let data_type = read_u16(data, position + 2, little_endian);
        let count = read_u32(data, position + 4, little_endian) as usize;
        let value_offset = read_u32(data, position + 8, little_endian) as usize;

        match tag {
            // GPSLatitudeRef (1)
            1 => {
                if data_type == 2 && count >= 1 {
                    let char_offset = if count <= 4 { position + 8 } else { value_offset };
                    if char_offset < data.len() {
                        latitude_ref = Some(data[char_offset] as char);
                    }
                }
            }
            // GPSLatitude (2)
            2 => {
                if data_type == 5 && count == 3 {
                    latitude_values = read_gps_rational_triple(data, value_offset, little_endian);
                }
            }
            // GPSLongitudeRef (3)
            3 => {
                if data_type == 2 && count >= 1 {
                    let char_offset = if count <= 4 { position + 8 } else { value_offset };
                    if char_offset < data.len() {
                        longitude_ref = Some(data[char_offset] as char);
                    }
                }
            }
            // GPSLongitude (4)
            4 => {
                if data_type == 5 && count == 3 {
                    longitude_values = read_gps_rational_triple(data, value_offset, little_endian);
                }
            }
            _ => {}
        }

        position += 12;
    }

    // Convert DMS to decimal degrees
    if let Some((degrees, minutes, seconds)) = latitude_values {
        let mut decimal = degrees + minutes / 60.0 + seconds / 3600.0;
        if latitude_ref == Some('S') {
            decimal = -decimal;
        }
        exif.gps_latitude = Some(decimal);
    }

    if let Some((degrees, minutes, seconds)) = longitude_values {
        let mut decimal = degrees + minutes / 60.0 + seconds / 3600.0;
        if longitude_ref == Some('W') {
            decimal = -decimal;
        }
        exif.gps_longitude = Some(decimal);
    }
}

// ---------------------------------------------------------------------------
// IFD value readers
// ---------------------------------------------------------------------------

fn read_ifd_string(
    data: &[u8],
    data_type: u16,
    count: usize,
    value_offset: usize,
    inline_offset: usize,
) -> Option<String> {
    // data_type 2 = ASCII
    if data_type != 2 || count == 0 {
        return None;
    }

    let string_offset = if count <= 4 {
        inline_offset
    } else {
        value_offset
    };

    if string_offset + count > data.len() {
        return None;
    }

    let bytes = &data[string_offset..string_offset + count];
    // Strip trailing null
    let trimmed = if bytes.last() == Some(&0) {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    String::from_utf8(trimmed.to_vec()).ok()
}

/// Read a UserComment tag (EXIF type UNDEFINED, tag 0x9286).
///
/// Format: 8-byte character code prefix + text.
/// Known prefixes: "ASCII\0\0\0", "UNICODE\0", "JIS\0\0\0\0\0", "\0\0\0\0\0\0\0\0" (undefined).
fn read_user_comment(
    data: &[u8],
    data_type: u16,
    count: usize,
    value_offset: usize,
    inline_offset: usize,
) -> Option<String> {
    // data_type 7 = UNDEFINED
    if data_type != 7 || count <= 8 {
        return None;
    }

    let offset = if count <= 4 { inline_offset } else { value_offset };
    if offset + count > data.len() {
        return None;
    }

    let comment_data = &data[offset..offset + count];
    let charset_prefix = &comment_data[..8];
    let text_bytes = &comment_data[8..];

    // Strip trailing nulls/whitespace
    let trimmed = text_bytes.iter()
        .rposition(|&b| b != 0 && b != b' ')
        .map(|last| &text_bytes[..=last])
        .unwrap_or(&[]);

    if trimmed.is_empty() {
        return None;
    }

    if charset_prefix.starts_with(b"ASCII") || charset_prefix == b"\0\0\0\0\0\0\0\0" {
        String::from_utf8(trimmed.to_vec()).ok()
    } else if charset_prefix.starts_with(b"UNICODE") {
        // UTF-16, try both endianness
        if trimmed.len() >= 2 {
            let code_units: Vec<u16> = trimmed.chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let text = String::from_utf16_lossy(&code_units);
            if text.is_empty() { None } else { Some(text) }
        } else {
            None
        }
    } else {
        // JIS or unknown — try as UTF-8
        String::from_utf8(trimmed.to_vec()).ok()
    }
}

fn read_gps_rational_triple(data: &[u8], offset: usize, little_endian: bool) -> Option<(f64, f64, f64)> {
    if offset + 24 > data.len() {
        return None;
    }

    let read_rational = |position: usize| -> f64 {
        let numerator = read_u32(data, position, little_endian) as f64;
        let denominator = read_u32(data, position + 4, little_endian) as f64;
        if denominator == 0.0 { 0.0 } else { numerator / denominator }
    };

    Some((
        read_rational(offset),
        read_rational(offset + 8),
        read_rational(offset + 16),
    ))
}

// ---------------------------------------------------------------------------
// Byte reading helpers (used by image.rs too via pub)
// ---------------------------------------------------------------------------

pub fn read_u16_big_endian(data: &[u8], offset: usize) -> u16 {
    if offset + 1 >= data.len() { return 0; }
    ((data[offset] as u16) << 8) | (data[offset + 1] as u16)
}

pub fn read_u16_little_endian(data: &[u8], offset: usize) -> u16 {
    if offset + 1 >= data.len() { return 0; }
    (data[offset] as u16) | ((data[offset + 1] as u16) << 8)
}

pub fn read_u32_big_endian(data: &[u8], offset: usize) -> u32 {
    if offset + 3 >= data.len() { return 0; }
    ((data[offset] as u32) << 24)
        | ((data[offset + 1] as u32) << 16)
        | ((data[offset + 2] as u32) << 8)
        | (data[offset + 3] as u32)
}

pub fn read_u32_little_endian(data: &[u8], offset: usize) -> u32 {
    if offset + 3 >= data.len() { return 0; }
    (data[offset] as u32)
        | ((data[offset + 1] as u32) << 8)
        | ((data[offset + 2] as u32) << 16)
        | ((data[offset + 3] as u32) << 24)
}

pub fn read_u16(data: &[u8], offset: usize, little_endian: bool) -> u16 {
    if little_endian { read_u16_little_endian(data, offset) } else { read_u16_big_endian(data, offset) }
}

pub fn read_u32(data: &[u8], offset: usize, little_endian: bool) -> u32 {
    if little_endian { read_u32_little_endian(data, offset) } else { read_u32_big_endian(data, offset) }
}
