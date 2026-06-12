# Bug Report: Chunked Sync Uploads Become Extremely Slow, Then FS-Server Returns 502

Date: 2026-06-11

## Summary

During a controlled one-off `aeordb-client` run against `files.taraani.org`, the client no longer showed the earlier memory blow-up, but the remote AeorDB server became progressively sluggish during chunk upload and commit work, then started returning HTTP 502 responses through the proxy.

The most important observed measurements:

- The client scanned 94,249 local entries and queued 53,537 files for hashing/checking.
- The client used 4 local file-preparation workers.
- The first material upload batch checked 3,163 chunks in 81 ms.
- That same batch uploaded 348 missing chunks, totaling 91,226,112 bytes, in 80,821 ms.
- That same batch then committed 1 file, 3,163 chunks, and 828,984,758 logical file bytes in 54,782 ms.
- Individual chunk upload requests were repeatedly slow: 2.1s, 3.3s, 4.5s, 5.4s, and later 11.0s for single chunk PUTs.
- The next batch began uploading 3,622 needed chunks and reached only 400/3,622 before `/blobs/check` started returning HTTP 502.
- After the first 502, the client fallback behavior amplified the failure by falling back from batch push to per-file isolation, producing hundreds of additional `/blobs/check` failures before I stopped the one-off client.

My read from the client side: `/blobs/check` itself can be fast when the server is healthy, but chunk PUT and/or `/blobs/commit` are still far too slow under this workload. The server then becomes unhealthy enough that even `/blobs/check` returns NGINX 502. I only have client-side evidence, so the DB team should treat this as a report of observed external behavior plus hypotheses, not as proof of the internal cause.

## Controlled Process Used

I intentionally did not run `dev-watch.sh` for this test. The goal was to run exactly one client process so that, if the server started failing again, the client would not automatically restart and continue applying pressure.

Process:

1. Started the current debug client as a one-off foreground/background process:

   ```sh
   ./target/debug/aeordb-client start --start-minimized
   ```

2. Captured stdout/stderr to:

   ```text
   /tmp/aeordb-client-oneoff.log
   ```

3. Confirmed the client was using the current startup behavior:

   - startup sync delay: 60 seconds
   - startup scan mode: Lite scan
   - no `dev-watch.sh`
   - no restart loop

4. Monitored:

   - client process memory/CPU with `ps`
   - connection health through the local client API:

     ```sh
     curl http://127.0.0.1:9400/api/v1/health/connections
     ```

   - sync runner status through:

     ```sh
     curl http://127.0.0.1:9400/api/v1/sync/runner/status
     ```

   - exact upload/check/commit timings from `/tmp/aeordb-client-oneoff.log`

5. When the local health API reported the connection as down with `server returned HTTP 502`, I stopped the one-off client:

   ```sh
   ./scripts/stop-client.sh
   ```

6. Verified afterward that no `dev-watch.sh` or `aeordb-client start` process was still running.

This was not a release binary test. It was a controlled debug build from the client repo at:

```text
a8d7fcba8b143d63762aaa14236a947c6ddf199f
```

The working tree had uncommitted client changes from the ongoing sync work. Relevant to this report: the client commit payload already includes the trusted sync fast-path fields requested by the DB team, namely file `content_hash` and `size` along with chunk references.

## Timeline

Client log timestamps below are UTC. Local time was America/Phoenix, which was UTC-07:00 for this run.

### Startup and scan

```text
2026-06-12T03:13:44.636313Z sync auto-start is enabled; waiting 60s before starting enabled sync relationships
2026-06-12T03:14:44.637330Z starting sync for 'Taraani (Pictures)' (PushOnly)
2026-06-12T03:14:45.143154Z push Lite scan: applying path migration local /home/wyatt/Pictures -> /media/Data/Remote/Seafile/wyatt-desktop, remote /workspaces/wyatt/Pictures/ -> /workspaces/wyatt/
2026-06-12T03:14:48.715891Z push Lite scan: found 94249 local entries under /media/Data/Remote/Seafile/wyatt-desktop
2026-06-12T03:15:23.694098Z push Lite scan: preparing 53537 files with 4 workers
```

### First material upload batch

```text
2026-06-12T03:15:42.371553Z push batch chunk progress: uploaded 1/348 needed chunks
2026-06-12T03:16:05.309362Z push batch chunk progress: uploaded 100/348 needed chunks
2026-06-12T03:16:29.139680Z push batch chunk progress: uploaded 200/348 needed chunks
2026-06-12T03:16:53.796043Z push batch chunk progress: uploaded 300/348 needed chunks
2026-06-12T03:17:03.088248Z push batch chunk progress: uploaded 348/348 needed chunks
2026-06-12T03:17:57.870999Z slow push blob_commit_batch for batch: files=1, chunks=3163, file_bytes=828984758, duration_ms=54782
2026-06-12T03:17:57.871049Z push batch committed: files=1, checked_chunks=3163, uploaded_chunks=348, uploaded_bytes=91226112, check_ms=81, upload_ms=80821, commit_ms=54782
```

That is the strongest single data point in the run:

- `check_ms=81` means asking the server which chunks it already had was fast for 3,163 chunk hashes.
- `upload_ms=80821` means uploading 91.2 MB of missing chunks took 80.8s, roughly 1.13 MB/s.
- `commit_ms=54782` means committing one already-hashed logical file took 54.8s.

The commit duration is especially surprising after the new trusted sync fast path. The client sent the file content hash and size. If all chunks are raw chunks and the fast path applies, I would expect the server to validate chunk existence/metadata and update file metadata without re-reading 829 MB of chunk bodies.

### Slow individual chunk PUTs

The client logs any chunk upload that crosses its slow threshold. The first upload batch repeatedly logged groups like this:

```text
2026-06-12T03:15:44.523722Z slow push upload_chunk_batch ... duration_ms=2150
2026-06-12T03:15:48.696838Z slow push upload_chunk_batch ... duration_ms=3210
2026-06-12T03:16:04.891802Z slow push upload_chunk_batch ... duration_ms=4485
2026-06-12T03:16:53.071947Z slow push upload_chunk_batch ... duration_ms=3924
```

The second batch got worse:

```text
2026-06-12T03:18:04.898364Z slow push upload_chunk_batch ... duration_ms=4215
2026-06-12T03:18:43.819602Z slow push upload_chunk_batch ... duration_ms=4583
2026-06-12T03:19:22.037609Z slow push upload_chunk_batch ... duration_ms=11034
2026-06-12T03:19:44.618515Z slow push upload_chunk_batch ... duration_ms=5121
2026-06-12T03:20:18.195945Z slow push upload_chunk_batch ... duration_ms=5442
```

These are 256 KiB-class chunks in the current client. Even allowing for HTTPS and persistence overhead, 2s to 11s per chunk is not an acceptable steady-state path for multi-TB sync.

### Server starts returning 502

After the first commit and while the second batch was still uploading needed chunks:

```text
2026-06-12T03:18:00.544465Z push batch chunk progress: uploaded 1/3622 needed chunks
2026-06-12T03:18:34.511709Z push batch chunk progress: uploaded 100/3622 needed chunks
2026-06-12T03:18:59.649967Z push batch chunk progress: uploaded 200/3622 needed chunks
2026-06-12T03:19:39.495536Z push batch chunk progress: uploaded 300/3622 needed chunks
2026-06-12T03:20:18.717344Z push batch chunk progress: uploaded 400/3622 needed chunks
2026-06-12T03:21:03.725502Z failed to upload [redacted path]: server error: blob_check returned HTTP 502 Bad Gateway
2026-06-12T03:21:07.339640Z batched push failed for 1 files; falling back to per-file isolation: server error: blob_check returned HTTP 502 Bad Gateway
```

By the time I stopped the client, the log contained 786 lines matching the 502/fallback/failure patterns. The client had reached:

```text
processed=23700, pushed=1, skipped=23297, failed=402, deleted=0
```

The local client health endpoint then reported:

```json
[
  {
    "connection_id": "0eabc208-12ec-45bd-a369-4036d9441ca1",
    "status": "down",
    "latency_ms": 15,
    "message": "server returned HTTP 502",
    "checked_at": 1781234634594
  }
]
```

The sync runner status simultaneously reported the relationship as executing but with unhealthy connection state:

```json
[
  {
    "relationship_id": "0f08f58f-4188-451c-a1f0-c649841d4e6a",
    "relationship_name": "Taraani (Pictures)",
    "running": true,
    "executing": true,
    "syncing": false,
    "connection_health": "down",
    "connection_healthy": false,
    "connection_message": "server returned HTTP 502"
  }
]
```

## Why This Looks Like A Server-Side Bottleneck

From the client side, the scan and hashing stages were not the limiting issue in this run. The client reached remote work quickly:

- 94,249 local entries found in about 4 seconds.
- 53,537 files queued for preparation.
- File preparation ran with 4 workers.
- `/blobs/check` completed for 3,163 chunks in 81 ms before the server became unhealthy.

The slow parts were remote operations:

- repeated slow `PUT /blobs/chunks/{hash}` calls
- one very slow `/blobs/commit`
- eventual HTTP 502 from `/blobs/check`

The 54.8s commit is the most suspicious. Given the new server fast path, please verify whether this request actually used it. If the fast path did not apply, the client may still be sending the wrong shape or the server may not be recognizing the alias it documented. If the fast path did apply, then the fast path still has a major internal bottleneck.

Possible server-side causes to rule in or out:

- `/blobs/commit` is still reading full chunk bodies instead of header/metadata only.
- The `chunks` alias does not activate the new `chunk_hashes` fast path.
- Commit is blocked behind a global write lock held by chunk uploads or indexing.
- Commit performs expensive directory/index/metadata work synchronously for the full file.
- WAL append, fsync, merge, or index update work is serial and too expensive per file.
- Chunk PUTs are doing more than raw append plus metadata registration.
- NGINX/proxy buffering or upstream timeouts are interacting badly with slow upstream responses.
- The async runtime or blocking thread pool is starved by storage/index work, causing unrelated `/blobs/check` calls to fail.
- The server process is crashing/restarting or becoming unavailable under chunk upload pressure.

## Client Behavior That May Be Making The Outage Worse

This report is for the DB team, but the client is not blameless here.

When a batch operation failed with HTTP 502, the current client treated it like a possibly file-specific batch failure and fell back to per-file isolation. That is useful for identifying one bad file in a healthy system, but it is the wrong behavior when the upstream is returning proxy/server-wide failures.

Observed result:

- one `/blobs/check` 502 became a batch failure
- the batch fallback tried individual files
- those individual attempts also hit `/blobs/check`
- the log quickly filled with hundreds of 502 failures

Client-side mitigation I recommend separately:

1. Treat 502, 503, 504, connection refused, connection reset, and timeout as connection-level failures, not per-file failures.
2. Do not enter per-file isolation for connection-level/server-level failures.
3. Trip a circuit breaker after a small number of server-level failures.
4. Mark the relationship as waiting for connection recovery.
5. Back off exponentially and let health checks decide when to resume.

That will not fix slow DB writes, but it will stop the client from kicking a server while it is already falling over.

## Requested DB-Team Investigation

Please add or inspect server-side structured timings for these endpoints:

- `POST /blobs/check`
- `PUT /blobs/chunks/{hash}`
- `POST /blobs/commit`

For `/blobs/commit`, the most useful fields would be:

- request id
- file count
- total chunk references
- total logical file bytes
- supplied `content_hash` present: yes/no
- supplied `size` present: yes/no
- fast path selected: yes/no
- reason fast path was not selected, if no
- chunk metadata/header lookup time
- chunk body read time, if any
- file record build time
- directory update time
- metadata index update time
- WAL append time
- fsync time
- merge/compaction time
- lock wait time
- total handler duration

For chunk PUTs:

- bytes received
- request body read time
- hash verification time
- compression status
- WAL append time
- fsync time
- metadata/index update time
- lock wait time
- total handler duration

For the 502 collapse:

- whether the AeorDB process crashed/restarted
- whether NGINX timed out waiting on upstream
- whether upstream accepted the connection and then closed it
- whether runtime/blocking thread pools were saturated
- whether storage locks caused health/check requests to stall behind writes

## Acceptance Criteria

The DB-side behavior should satisfy these practical expectations before the sync client can safely handle multi-TB datasets:

1. A trusted sync commit with `content_hash`, `size`, and raw chunk references should not re-read full chunk bodies.
2. Server logs should make it explicit whether the trusted fast path was selected.
3. Chunk PUTs for 256 KiB chunks should not routinely take multiple seconds on a healthy LAN/server.
4. Commit of a file with thousands of already-known chunks should be bounded and mostly metadata-oriented.
5. A slow write path should not make `/blobs/check` return proxy-level 502.
6. Under overload, the server should fail in a bounded way that gives clients a clear retry/backoff signal.

## Notes And Caveats

- This report is based on client-side logs and local client health endpoints only.
- I did not have server CPU, memory, WAL, lock, merge, or NGINX logs while writing this.
- The raw client log contains private local/remote file paths. It should be redacted before being copied into public issue trackers.
- The one-off client was stopped as soon as the server was observed returning 502.
- No `dev-watch.sh` process was left running after the test.

