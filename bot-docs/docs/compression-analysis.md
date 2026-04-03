# Compression Analysis — April 2026

Real-world compression test on aeordb data.

## Test Setup

- 500 indexed JSON user files with rich metadata (name, age, city, email, bio, tags, preferences)
- 4 field indexes (age, name, city, email, bio)
- Algorithms tested: gzip (levels 1/6/9), zstd (levels 1/3/9), lz4, brotli, xz/lzma

## Results by Data Type

### Directory Listing (500 entries, 70 KB)

| Algorithm | Size | Ratio |
|---|---|---|
| Original | 72,643 B | 100% |
| **zstd -1 (fast)** | **4,274 B** | **5.8%** |
| zstd -9 | 4,283 B | 5.8% |
| gzip -1 | 6,533 B | 8.9% |
| gzip -9 | 5,283 B | 7.2% |
| xz/lzma | 3,768 B | 5.1% |

**Winner: zstd -1.** 17x compression. Even at fastest level, matches zstd -9.

### Index Files (31 KB each)

| Algorithm | Ratio |
|---|---|
| zstd -9 | 50.9% |
| gzip -6 | 53.0% |
| xz | 52.3% |

All roughly 2x. zstd slightly better.

### KV Block (390 KB simulated — random hashes + offsets)

| Algorithm | Ratio |
|---|---|
| zstd -1 | 86.4% |
| gzip -6 | 91.0% |

Random hashes are incompressible. Not worth compressing.

### Individual JSON Files (~275 bytes average)

| Algorithm | Ratio |
|---|---|
| gzip | 85.2% |

Too small — header overhead eats the savings.

## Decompression Speed

| Algorithm | 100 decompressions of 70 KB | Avg |
|---|---|---|
| gzip | 177ms | 1.8ms |
| zstd | 223ms | 2.2ms |

Both sub-3ms. Negligible overhead.

## Recommendation

**zstd** (via `zstd` Rust crate, BSD-3-Clause):
- Best ratio on structured data (5.8% on directory listings)
- Fast decompression (2ms for 70 KB)
- Streaming-friendly
- Well-supported in Rust ecosystem

Compress: directory entries, index files, large metadata-rich FileRecords.
Don't compress: KV block, NVT, small files, already-compressed content (JPEG, MP4).
