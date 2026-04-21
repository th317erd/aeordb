# Media Parser Metadata Gaps — Design Spec

**Date:** 2026-04-21
**Status:** Approved

## Problem

AeorDB's native file parsers extract structural metadata (dimensions, duration, codecs, etc.) from media files but are missing textual metadata that creative professionals search and filter by — author, description, copyright, comments, and similar fields.

### Current Gap Summary

| Parser | Structural | Textual Metadata | Gap |
|--------|-----------|-----------------|-----|
| Image (JPEG) | width, height, bit depth, color | camera_make, model, date_taken, orientation, GPS | Missing: description, artist, copyright, user_comment, software |
| Image (TIFF) | width, height, bit depth, color | None | Missing: all EXIF textual tags |
| Image (PNG) | width, height, bit depth, color, animated | tEXt/iTXt chunks | Already complete |
| Video (MP4/MOV) | format, brand, duration, resolution, fps, codecs | None | Missing: title, artist, description, copyright, comment |
| Video (AVI/WebM/FLV) | format, duration, resolution | None | Out of scope (rare formats) |
| Audio (MP3/OGG) | duration, sample_rate, channels, bitrate | ID3/Vorbis tags | Already complete |
| Audio (WAV) | duration, sample_rate, channels, bitrate | None | Missing: RIFF INFO chunk tags |

## Approach

**Option B: Extract shared EXIF helpers, extend parsers in-place.**

The JPEG EXIF parser already does full IFD traversal. TIFF files use the same IFD structure. Extracting the shared EXIF/IFD logic into a helper module lets both JPEG and TIFF get full textual metadata without duplication. Video (MP4) and WAV get inline additions. No new external crate dependencies.

## Design

### 1. Shared EXIF Module — `native_parsers/exif.rs`

Extract the existing JPEG EXIF/IFD parsing code into a shared module.

**Public API:**

```rust
pub struct ExifData {
    pub camera_make: Option<String>,       // 0x010F
    pub camera_model: Option<String>,      // 0x0110
    pub orientation: Option<u32>,          // 0x0112
    pub date_taken: Option<String>,        // 0x0132 / 0x9003
    pub image_description: Option<String>, // 0x010E  (NEW)
    pub artist: Option<String>,            // 0x013B  (NEW)
    pub copyright: Option<String>,         // 0x8298  (NEW)
    pub software: Option<String>,          // 0x0131  (NEW)
    pub user_comment: Option<String>,      // 0x9286  (NEW) — UNDEFINED type, 8-byte charset prefix + text
    pub gps_latitude: Option<f64>,
    pub gps_longitude: Option<f64>,
}

pub fn parse_exif(tiff_data: &[u8]) -> Option<ExifData>
```

Moves from `image.rs`: `parse_exif()`, `parse_ifd()`, `parse_gps_ifd()`, `read_ifd_string()`, `read_gps_rational_triple()`, and all byte-reading helpers (`read_u16`, `read_u32`).

JPEG calls `parse_exif()` on APP1 data (after stripping `Exif\0\0` prefix). TIFF calls `parse_exif()` directly on the file data (TIFF files are TIFF/IFD structures natively).

### 2. Image Parser Changes

**JPEG (`image.rs`):**
- Remove inline EXIF/IFD parsing code, replace with `use super::exif`
- `parse_jpeg()` calls `exif::parse_exif()` on APP1 segment data
- Maps `ExifData` struct into `metadata.exif` JSON object (all fields, including 5 new ones)

**TIFF (`image.rs`):**
- After existing structural IFD scan (width/height/bit_depth/color_type), call `exif::parse_exif()` on the full TIFF data
- Adds `result.exif` field with same output shape as JPEG

**PNG:** No changes — tEXt/iTXt chunk parsing already covers textual metadata.

**EXIF output shape (JPEG and TIFF):**
```json
{
  "metadata": {
    "exif": {
      "camera_make": "Canon",
      "camera_model": "EOS R5",
      "orientation": 1,
      "date_taken": "2026:04:15 10:30:00",
      "image_description": "Sunset over the mountains",
      "artist": "Jane Doe",
      "copyright": "© 2026 Jane Doe",
      "software": "Adobe Lightroom",
      "user_comment": "Shot during golden hour",
      "gps_latitude": 34.0522,
      "gps_longitude": -118.2437
    }
  }
}
```

### 3. Video Parser — MP4/MOV Metadata

Parse the iTunes-style metadata atoms stored at `moov/udta/meta/ilst`.

**Atoms to extract:**

| Atom | Field name |
|------|------------|
| `©nam` | `title` |
| `©ART` | `artist` |
| `©alb` | `album` |
| `©cmt` | `comment` |
| `©day` | `year` |
| `©gen` | `genre` |
| `desc` | `description` |
| `cprt` | `copyright` |
| `©too` | `encoder` |

**Implementation:** Add `parse_udta()` that walks `moov/udta` looking for a `meta` box, then walks `ilst` extracting data atoms. Uses the existing `iter_boxes()` helper for box traversal.

**Output shape:**
```json
{
  "metadata": {
    "format": "mp4",
    "duration_seconds": 127.5,
    "width": 1920,
    "height": 1080,
    "tags": {
      "title": "Mountain Sunset Timelapse",
      "artist": "Jane Doe",
      "description": "4K timelapse of sunset over the Rockies",
      "copyright": "© 2026 Jane Doe",
      "comment": "Shot with Canon R5",
      "year": "2026",
      "encoder": "HandBrake 1.8.0"
    }
  }
}
```

**AVI/WebM/FLV:** Out of scope for this round. MP4/MOV covers the overwhelming majority of video content creative professionals work with.

### 4. WAV Audio — RIFF INFO Chunk Tags

WAV files store metadata in RIFF INFO chunks inside a `LIST` chunk with type `INFO`.

**INFO chunk tags to extract:**

| Chunk ID | Field name |
|----------|------------|
| `INAM` | `title` |
| `IART` | `artist` |
| `ICMT` | `comment` |
| `ICOP` | `copyright` |
| `IGNR` | `genre` |
| `ICRD` | `year` |
| `ISFT` | `software` |

**Implementation:** In `parse_wav()`, the existing chunk scanner already iterates RIFF sub-chunks. Add a check for `LIST` chunks — when the list type is `INFO`, scan its sub-chunks and extract tag values.

**Output shape:**
```json
{
  "metadata": {
    "format": "wav",
    "duration_seconds": 42.3,
    "sample_rate": 44100,
    "channels": 2,
    "tags": {
      "title": "Ambient Forest Recording",
      "artist": "Sound Studio Pro",
      "comment": "Field recording",
      "copyright": "© 2026",
      "software": "Audacity 3.5"
    }
  }
}
```

This follows the same `tags` pattern that MP3 and OGG already use.

## Testing Strategy

All tests use hand-crafted byte buffers — same pattern as existing parser tests.

**Shared EXIF module:**
- Parse crafted TIFF/IFD buffer with all 10 tag types — verify each field
- Parse with missing optional tags — verify None for absent fields
- Parse with truncated data — verify graceful None return, no panic
- Parse with invalid byte order markers — verify None return
- GPS parsing edge cases: S/W hemispheres, missing ref bytes

**Image parser (JPEG + TIFF):**
- JPEG with EXIF containing new fields — verify in `metadata.exif`
- JPEG without APP1 segment — verify exif absent
- TIFF with EXIF tags — verify same output shape as JPEG
- TIFF without textual tags — structural metadata still extracted

**Video parser (MP4):**
- Crafted MP4 with `moov/udta/meta/ilst` atoms — verify `metadata.tags`
- MP4 without `udta` — verify tags absent, structural metadata intact
- MP4 with empty `ilst` — verify no crash

**WAV audio:**
- WAV with LIST/INFO chunk — verify `metadata.tags`
- WAV without LIST chunk — verify existing behavior unchanged
- WAV with malformed INFO values — graceful handling

## Out of Scope

- AVI RIFF INFO metadata (rare format for creative work)
- WebM EBML tag metadata (complex nested structure, low priority)
- FLV metadata (no standard container)
- IPTC / XMP embedded metadata (significant complexity, future enhancement)
- Chapter markers, encoding parameters, content ratings
