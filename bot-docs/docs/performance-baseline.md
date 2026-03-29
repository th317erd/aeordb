# Performance Baseline — March 2026

First stress test results. These numbers are pre-optimization baselines.

## Test Environment

- **CPU:** (host machine)
- **RAM:** 32 GB (tmpfs used for ramdisk tests)
- **Storage:** USB external drive (Elements) + tmpfs ramdisk
- **OS:** Linux 6.17.0-19-generic
- **Rust:** 1.94.0, release build
- **redb:** 3.1.1
- **Chunk size:** 256 KB (default)
- **HTTP client:** curl (sequential, one connection per request — no concurrency)

## Small File Performance (Images)

500 images from ~/Pictures, average ~350 KB each.

| Metric | USB External | Ramdisk (tmpfs) | Speedup |
|---|---|---|---|
| Upload speed | 9.1 files/sec, 3.5 MB/sec | 15.9 files/sec, 5.5 MB/sec | 1.75x |
| Read speed | 8ms/file, 37.8 MB/sec | 7ms/file, 40.0 MB/sec | 1.14x |
| Directory list (462 entries) | 33ms | 24ms | 1.38x |
| Delete speed | 18ms/file | 7ms/file | 2.57x |

### Key Observation

Writes are CPU/protocol-bound, not disk-bound. Ramdisk is only 1.75x faster than USB — if I/O were the bottleneck, we'd see 10-100x improvement. The bottleneck is curl overhead (new TCP connection per request), HTTP parsing, JSON serialization, redb transaction commits, and BLAKE3 chunk hashing.

Reads are barely faster on ramdisk (7ms vs 8ms) — dominated by HTTP/protocol overhead.

## Large File Performance (Video)

952 MB MP4 video file.

| Operation | Speed | Time | Notes |
|---|---|---|---|
| Upload (USB) | 19.4 MB/sec | 49 seconds | Single PUT request |
| Read (USB) | 934 MB/sec | 1.02 seconds | Streaming response, data in page cache |
| Size verification | Exact match | — | 952 MB in = 952 MB out |

### Key Observation

Read speed of 934 MB/sec is from redb's page cache (data was just written and still in memory). Cold-read performance would be lower and bounded by disk speed.

Write speed of 19.4 MB/sec on USB is reasonable — limited by USB 3.0 sustained write speed and redb's transactional commit overhead.

## Storage Overhead

| Test | Data Size | DB File Size | Ratio |
|---|---|---|---|
| 30 images (13 MB) | 13 MB | 43 MB | 334% |
| 462 images (179 MB) | 179 MB | 403 MB | 224% |
| 447 images (154 MB) ramdisk | 154 MB | 307 MB | 199% |
| 1 video (952 MB) | 952 MB | 2,200 MB | 231% |

### Key Observation

Storage ratio is consistently 2x-3.3x. Sources of overhead:
- redb COW B-tree page allocation (buddy allocator, power-of-two pages)
- redb transaction metadata and internal tables
- Chunk headers (33 bytes per chunk — negligible at 256KB chunk size)
- Directory table metadata (index entries serialized as JSON)
- Dead pages from COW that haven't been compacted
- redb doesn't shrink the file after deletes

Smaller files have worse ratios because the fixed overhead per entry is a larger percentage of the data.

### Improvement Opportunities
- redb compaction (if available)
- Binary serialization for index entries instead of JSON
- Larger chunk sizes for large files (reduce per-chunk overhead)
- Adaptive chunk sizing based on file size
- Eventually: replace redb with purpose-built chunk store (eliminate double B-tree)

## Deduplication

| Test | Chunks Stored | Chunks Deduplicated | Dedup Rate |
|---|---|---|---|
| 462 images + churn | 678 | 480 | 41% |
| 447 images + churn (ramdisk) | 643 | 384 | 37% |

Deduplication is working — re-uploaded files reuse existing chunks. The dedup rate reflects the churn test (delete half, re-upload).

## Concurrency (Not Yet Tested)

All tests used sequential curl requests (one connection at a time). Concurrent performance is expected to be significantly higher due to:
- Eliminated connection-per-request overhead
- redb's MVCC allowing concurrent reads
- Tokio's async I/O handling multiple requests simultaneously

Concurrent stress testing is a TODO.

## Identified Issues

1. **HTTP 413 on large uploads** — Fixed by setting DefaultBodyLimit to 10GB
2. **Filename collisions** — `shuf` can produce duplicate basenames from different directories. Mitigated with md5sum suffix in test scripts.
3. **No streaming upload** — Files are loaded entirely into memory on the server side before chunking. For multi-GB files, this is a memory concern. Streaming ingestion is a TODO.
4. **Storage ratio 2-3x** — redb overhead. Acceptable for prototype, needs optimization.

## Baseline Summary

| Category | Baseline |
|---|---|
| Small file write | 10-16 files/sec (sequential curl) |
| Small file read | 7-8ms/file, ~40 MB/sec |
| Large file write | ~20 MB/sec (USB) |
| Large file read | 934 MB/sec (hot cache) |
| Directory listing | 24-33ms for ~450 entries |
| Storage overhead | 2x-3.3x |
| Dedup effectiveness | 37-41% on churn workloads |
