# Media Parser Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fill textual metadata gaps in image (JPEG/TIFF), video (MP4/MOV), and audio (WAV) native parsers so creative professionals can search and index by author, description, copyright, and other document properties.

**Architecture:** Extract shared EXIF/IFD parsing from `image.rs` into a reusable `exif.rs` module consumed by both JPEG and TIFF. Add iTunes-style metadata atom parsing to the MP4 video parser. Add RIFF INFO chunk parsing to the WAV audio parser. No new external dependencies.

**Tech Stack:** Rust, hand-crafted byte parsing (zero external crates), serde_json for output

**Spec:** `docs/superpowers/specs/2026-04-21-media-parser-metadata-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-lib/src/engine/native_parsers/exif.rs` | Create | Shared EXIF/IFD parsing: byte helpers, IFD traversal, GPS, all textual tags |
| `aeordb-lib/src/engine/native_parsers/mod.rs` | Modify (L7) | Add `pub mod exif;` registration |
| `aeordb-lib/src/engine/native_parsers/image.rs` | Modify | Remove inline EXIF code (~220 lines), call `exif::parse_exif()`, wire TIFF EXIF |
| `aeordb-lib/src/engine/native_parsers/video.rs` | Modify | Add `parse_udta()` for MP4 iTunes metadata atoms |
| `aeordb-lib/src/engine/native_parsers/audio.rs` | Modify | Add LIST/INFO chunk parsing in `parse_wav()` |
| `aeordb-lib/spec/engine/native_parsers_spec.rs` | Modify | Add tests for all new metadata extraction |

---

### Task 1: Create Shared EXIF Module with Byte Helpers

**Files:**
- Create: `aeordb-lib/src/engine/native_parsers/exif.rs`
- Modify: `aeordb-lib/src/engine/native_parsers/mod.rs:7`

This task extracts the byte-reading helpers and EXIF/IFD parsing from `image.rs` into a shared module, adds the 5 new EXIF tags, and exposes a structured `ExifData` return type instead of a raw JSON map.

- [ ] **Step 1: Register the new module**

In `aeordb-lib/src/engine/native_parsers/mod.rs`, add after line 7 (`mod text;`):

```rust
pub mod exif;
```

- [ ] **Step 2: Create `exif.rs` with byte helpers and ExifData struct**

Create `aeordb-lib/src/engine/native_parsers/exif.rs` with:

```rust
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
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Expected: Compiles (the module is registered but not yet consumed)

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/native_parsers/exif.rs aeordb-lib/src/engine/native_parsers/mod.rs
git commit -m "Add shared EXIF module with 5 new textual metadata tags"
```

---

### Task 2: Write EXIF Module Tests

**Files:**
- Modify: `aeordb-lib/spec/engine/native_parsers_spec.rs`

Tests use hand-crafted TIFF/IFD byte buffers. A helper function builds a minimal valid TIFF structure with configurable IFD entries.

- [ ] **Step 1: Add EXIF test helpers and tests**

Append to `aeordb-lib/spec/engine/native_parsers_spec.rs`:

```rust
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
```

- [ ] **Step 2: Run EXIF tests to verify they pass**

Run: `cargo test --test native_parsers_spec exif_ 2>&1 | tail -15`
Expected: All `exif_*` tests pass

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/spec/engine/native_parsers_spec.rs
git commit -m "Add tests for shared EXIF module"
```

---

### Task 3: Rewire Image Parser to Use Shared EXIF Module

**Files:**
- Modify: `aeordb-lib/src/engine/native_parsers/image.rs`

Remove the inline `parse_exif()`, `parse_ifd()`, `parse_gps_ifd()`, `read_ifd_string()`, `read_gps_rational_triple()`, and all byte-reading helpers. Replace with calls to the shared `exif` module. Wire TIFF to also extract EXIF metadata.

- [ ] **Step 1: Replace inline EXIF code in `image.rs`**

In `image.rs`:

1. Add at the top: `use super::exif;`

2. Replace the `parse_exif` call in `parse_jpeg` (line ~204-209) — the APP1 EXIF handling block:

```rust
        // APP1 (EXIF) marker
        if marker == 0xE1 && segment_start + segment_length <= data.len() {
            let app1_data = &data[segment_start + 2..segment_end.min(data.len())];
            if app1_data.len() >= 6 && app1_data[0..6] == *b"Exif\x00\x00" {
                if let Some(exif_data) = exif::parse_exif(&app1_data[6..]) {
                    result.exif = Some(exif_data.to_json());
                }
            }
        }
```

3. Wire TIFF EXIF extraction — at the end of `parse_tiff()`, after the structural IFD scan:

```rust
    // Extract textual EXIF metadata (reuses the same TIFF/IFD structure)
    if let Some(exif_data) = exif::parse_exif(data) {
        result.exif = Some(exif_data.to_json());
    }
```

4. Delete the following functions from `image.rs` (they now live in `exif.rs`):
   - `parse_exif` (lines ~225-268)
   - `parse_ifd` (lines ~270-343)
   - `read_ifd_string` (lines ~345-376)
   - `parse_gps_ifd` (lines ~378-459)
   - `read_gps_rational_triple` (lines ~461-482)

5. Replace the byte-reading helper functions (lines ~983-1031) with imports from `exif`:

```rust
use super::exif::{
    read_u16_big_endian, read_u16_little_endian,
    read_u32_big_endian, read_u32_little_endian,
    read_u16, read_u32,
};
```

Delete the local `read_u16_big_endian`, `read_u16_little_endian`, `read_u32_big_endian`, `read_u32_little_endian`, `read_u16`, and `read_u32` functions.

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Expected: Compiles clean

- [ ] **Step 3: Run ALL existing parser tests to verify no regressions**

Run: `cargo test --test native_parsers_spec 2>&1 | tail -5`
Expected: All tests pass (existing JPEG/TIFF tests still work)

- [ ] **Step 4: Add test for TIFF EXIF extraction**

Append to `aeordb-lib/spec/engine/native_parsers_spec.rs`:

```rust
#[test]
fn tiff_extracts_exif_metadata() {
    // Build a minimal TIFF with ImageDescription and Artist tags
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

    // EXIF textual metadata should be present
    assert_eq!(json["metadata"]["exif"]["image_description"], "Mountain landscape");
    assert_eq!(json["metadata"]["exif"]["artist"], "Photo Pro");
}

#[test]
fn jpeg_exif_includes_new_fields() {
    // Build a JPEG with an APP1 EXIF segment containing new tags
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

    // Wrap in JPEG APP1 segment: SOI + APP1 marker + length + "Exif\0\0" + TIFF data
    let mut jpeg = vec![0xFF, 0xD8]; // SOI
    jpeg.push(0xFF); jpeg.push(0xE1); // APP1 marker
    let app1_length = (2 + 6 + tiff_data.len()) as u16; // length includes itself + Exif header + TIFF
    jpeg.extend_from_slice(&app1_length.to_be_bytes());
    jpeg.extend_from_slice(b"Exif\x00\x00");
    jpeg.extend_from_slice(&tiff_data);
    // SOF0 for dimensions (so JPEG parser finds width/height)
    jpeg.extend_from_slice(&[0xFF, 0xC0]); // SOF0 marker
    jpeg.extend_from_slice(&11u16.to_be_bytes()); // segment length
    jpeg.push(8); // bit depth
    jpeg.extend_from_slice(&100u16.to_be_bytes()); // height
    jpeg.extend_from_slice(&200u16.to_be_bytes()); // width
    jpeg.push(3); // num components (RGB)
    // EOI
    jpeg.extend_from_slice(&[0xFF, 0xD9]);

    let result = parse_native(&jpeg, "image/jpeg", "sunset.jpg", "/sunset.jpg", jpeg.len() as u64);
    assert!(result.is_some());
    let json = result.unwrap().expect("JPEG should parse");
    assert_eq!(json["metadata"]["format"], "jpeg");
    assert_eq!(json["metadata"]["exif"]["image_description"], "Beach sunset");
    assert_eq!(json["metadata"]["exif"]["artist"], "Jane Doe");
}
```

- [ ] **Step 5: Run all parser tests**

Run: `cargo test --test native_parsers_spec 2>&1 | tail -5`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/engine/native_parsers/image.rs aeordb-lib/spec/engine/native_parsers_spec.rs
git commit -m "Rewire image parser to shared EXIF module, add TIFF EXIF extraction"
```

---

### Task 4: Add MP4/MOV iTunes Metadata Parsing

**Files:**
- Modify: `aeordb-lib/src/engine/native_parsers/video.rs`

Add `parse_udta()` to extract iTunes-style metadata atoms from `moov/udta/meta/ilst`. The `meta` box is a "full box" with a 4-byte version/flags header before its children. Each `ilst` child contains a `data` sub-atom with 8 bytes of type/locale header before the text.

- [ ] **Step 1: Add `parse_udta` and `parse_ilst` functions to `video.rs`**

Add after the existing `parse_moov` function:

```rust
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
```

- [ ] **Step 2: Add `tags` field to `VideoMetadata` struct and output**

Add to the `VideoMetadata` struct:

```rust
    tags: Option<serde_json::Value>,
```

Wire it into `parse_moov`:

```rust
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
```

Initialize in `parse()`:

```rust
    let mut result = VideoMetadata {
        // ... existing fields ...
        tags: None,
    };
```

Add to the output JSON (in the `parse()` function's `Ok(...)` return):

```rust
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
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Expected: Compiles clean

- [ ] **Step 4: Add tests for MP4 metadata**

Append to `aeordb-lib/spec/engine/native_parsers_spec.rs`:

```rust
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
```

- [ ] **Step 5: Run MP4 metadata tests**

Run: `cargo test --test native_parsers_spec mp4_ 2>&1 | tail -10`
Expected: All `mp4_*` tests pass

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/engine/native_parsers/video.rs aeordb-lib/spec/engine/native_parsers_spec.rs
git commit -m "Add MP4/MOV iTunes metadata extraction (title, artist, description, copyright)"
```

---

### Task 5: Add WAV RIFF INFO Chunk Parsing

**Files:**
- Modify: `aeordb-lib/src/engine/native_parsers/audio.rs`

Extend `parse_wav()` to detect `LIST` chunks with type `INFO` and extract standard INFO sub-chunk tags.

- [ ] **Step 1: Add LIST/INFO parsing to `parse_wav`**

In `audio.rs`, add inside the `parse_wav` `while` loop (after the `data` chunk handler, before the advance-to-next-chunk code):

```rust
        if chunk_id == b"LIST" && chunk_size >= 4 && chunk_data_start + 4 <= data.len() {
            let list_type = &data[chunk_data_start..chunk_data_start + 4];
            if list_type == b"INFO" {
                parse_info_chunks(&data[chunk_data_start + 4..chunk_data_start + chunk_size.min(data.len() - chunk_data_start)], metadata);
            }
        }
```

Add the `parse_info_chunks` function after `parse_wav`:

```rust
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
```

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Expected: Compiles clean

- [ ] **Step 3: Add tests for WAV INFO chunks**

Append to `aeordb-lib/spec/engine/native_parsers_spec.rs`:

```rust
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
```

- [ ] **Step 4: Run WAV tests**

Run: `cargo test --test native_parsers_spec wav_ 2>&1 | tail -10`
Expected: All `wav_*` tests pass

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/native_parsers/audio.rs aeordb-lib/spec/engine/native_parsers_spec.rs
git commit -m "Add WAV RIFF INFO chunk metadata extraction"
```

---

### Task 6: Full Test Suite Verification and Final Commit

**Files:** None (verification only)

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test 2>&1 | grep -E "test result:" | sort | uniq -c | sort -rn`
Expected: All test binaries show 0 failures

- [ ] **Step 2: Run specifically the parser tests to verify no regressions**

Run: `cargo test --test native_parsers_spec 2>&1 | tail -5`
Run: `cargo test --test e2e_parser_spec 2>&1 | tail -5`
Expected: Both pass with 0 failures

- [ ] **Step 3: Verify real-file tests work (if files exist)**

Run: `cargo test --test native_parsers_spec real_ -- --nocapture 2>&1 | grep "Real\|FAILED"`
Expected: Real file tests pass (or silently skip if files don't exist)

- [ ] **Step 4: Count total tests**

Run: `cargo test 2>&1 | grep "test result:" | awk '{sum += $4} END {print "Total:", sum, "tests"}'`
Expected: Total count increased from 3,526 by the number of new tests added

- [ ] **Step 5: Update TODO.md**

Add under the completed features:

```markdown
- [x] Media parser metadata gaps (EXIF textual tags, MP4 iTunes atoms, WAV INFO chunks) — N tests
```

- [ ] **Step 6: Final commit**

```bash
git add .claude/TODO.md
git commit -m "Update TODO with media parser metadata completion"
```
