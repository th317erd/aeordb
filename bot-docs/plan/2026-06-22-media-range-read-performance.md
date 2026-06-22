# Media Range Read Performance

## Context

4K playback through the web UI can buffer every few seconds on FS-Server1. Live testing showed AeorDB can serve cached ranges quickly, but cold mid-file byte ranges from multi-GB videos were only about 2 MB/s on the HDD/ZFS pool. The current range streamer reads file chunks one at a time from the append-only database file.

## Goal

Make browser media byte-range reads cheaper and friendlier to cold storage without weakening integrity checks or changing the public HTTP API.

## Plan

1. Add range-read diagnostics so slow media reads report range size, first-byte latency, total latency, chunk count, and throughput.
2. Build a chunk read plan for live file byte ranges:
   - resolve requested byte range into file-order chunks,
   - read each chunk's live WAL offset and entry length from the KV snapshot,
   - group adjacent or near-adjacent file-order chunks into bounded WAL spans,
   - read each span with one `pread`,
   - parse and verify every chunk entry before returning bytes.
3. Keep the current per-chunk path for version/snapshot reads until deleted-entry offset planning is implemented.
4. Add bounded range read-ahead/cache for media once coalesced reads are in place.
5. Longer term, evaluate a materialized large-file/media cache or extent-oriented storage path for cold playback on HDD-backed deployments.

## Initial Coalescing Policy

- Coalesce only chunks that are contiguous in file order and non-decreasing in WAL offset.
- Always coalesce exact adjacency.
- Allow small WAL gaps only while the total span stays bounded.
- Default maximum span: 32 MiB.
- Default maximum gap: 256 KiB.
- Preserve per-entry hash verification and chunk type validation.
- If cheap live chunk metadata does not add up to the file's logical size,
  fall back to the per-chunk range reader with metadata skipping disabled.
  This preserves correctness for compressed chunks and other edge cases where
  stored chunk length is not the decoded file length.

## Non-Goals

- Do not bypass BLAKE3 entry verification for user-facing reads.
- Do not change browser-visible range semantics.
- Do not make snapshot/version range reads depend on live-only KV entries.
