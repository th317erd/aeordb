# Bug Report: Session Subtree Deletes Leave/Reanimate Recursive Listing Entries

Date captured: 2026-06-05 UTC
Reporter context: Kikx dev database cleanup
AeorDB version: 0.9.5
Database path in Kikx project: `/home/wyatt/Projects/kikx-workspace/kikx/.aeordb/kikx.aeordb`
HTTP base URL used during investigation: `http://127.0.0.1:6830`

## Summary

While deleting the Kikx test session/frame data from AeorDB, recursive listing and direct file operations became inconsistent under `/kikx/sessions`.

The requested application operation was:

- Delete all current Kikx sessions and frames.
- Preserve `/kikx/agents`.
- Do not destroy corruption evidence.

The session manifests and normal frame glob queries now report zero:

- `GET /files/kikx/sessions?depth=-1&glob=*/session.json&limit=500` -> `total: 0`
- `GET /files/kikx/sessions?depth=-1&glob=**/frames/*.json&limit=500` -> `total: 0`

However, recursive listing still reports stale/reanimated entries:

- `GET /files/kikx/sessions?depth=-1&glob=**/*&limit=1000` -> `total: 7`

During the operation, some of those listed paths returned `404` when directly deleted or fetched. After a graceful AeorDB restart, AeorDB performed dirty-startup KV rebuild and the listed paths became directly readable again (`GET 200`). Deleting them again returned `200` once, then repeated deletes returned `404`, while recursive listing continued to show the same paths. A later fresh observation again showed direct `GET 200` for the seven listed paths.

This looks like a storage/KV/index/void handling bug, not an application cache issue.

## Artifact Warning

The DB artifacts are full Kikx development database files. They intentionally preserve evidence and may include the `/kikx/agents` subtree, including development agent configuration/secrets. Do not publish these artifacts publicly.

## Included Artifacts

All artifacts are under this directory.

- `evidence/kikx-before-session-delete.aeordb`
  - Pre-delete database snapshot.
  - SHA-256: `cfbfabfd443d0cae9d35a17f9f694ffd8e703d4ac2e656fa67a0b8f08011595b`

- `evidence/kikx-post-session-delete-pre-restart.aeordb`
  - Database snapshot taken after the first delete/cleanup passes and after graceful AeorDB shutdown, before the restart test.
  - SHA-256: `4447a18a3bd725c0dca5a351b4dc3fd795cc849d601dcd83340a7bfeeaf0fd42`

- `evidence/kikx-post-session-delete-pre-restart.aeordb.lock`
  - Empty lock sidecar copied with the preserved DB.

- `evidence/aeordb-pre-restart.log`
  - AeorDB log from before the restart test.
  - Contains repeated `Corrupt KV page ... Resetting` warnings and failed root session directory deletes.

- `evidence/aeordb-restart-rebuild.log`
  - AeorDB log from the restart test.
  - Contains dirty-startup KV rebuild, many corrupt-entry scan warnings, void recovery, and successful listener startup.

- `evidence/pre-restart-fetch-details.txt`
  - Preservation notes generated at the time of the post-delete/pre-restart snapshot.

- `current-http-observations.json`
  - Fresh HTTP observations captured after the restart test and second delete pass.

- `evidence/*.sha256`
  - Checksums for the copied database and log artifacts.

## Timeline

1. Kikx was stopped so its in-memory runtime could not write stale sessions during the purge.
2. AeorDB remained running.
3. A pre-delete DB copy already existed at `/tmp/codex/kikx-db-preserve/kikx-before-session-delete-20260605T061946Z.aeordb`.
4. The first delete pass listed `/kikx/sessions` recursively and deleted only paths with the `/kikx/sessions/` prefix.
5. First pass result:
   - `matchedItems: 285`
   - `deletedFiles: 279`
   - `missingFiles: 6`
   - `remainingItems: 7`
   - Deleting `/files/kikx/sessions` failed with `Directory '/kikx/sessions' is not empty (10 children)`.
6. Cleanup pass reconstructed known directories from the AeorDB log and deleted deepest-first.
7. Cleanup pass result:
   - `deletedFiles: 1`
   - `knownDirectoryPaths: 146`
   - `deletedDirs: 126`
   - `remainingItems: 6`
   - Deleting `/files/kikx/sessions` failed with `Directory '/kikx/sessions' is not empty (5 children)`.
8. Listing showed six remaining paths. Direct explicit deletes for those paths returned `404`.
9. Verification before restart:
   - `*/session.json` glob returned `total: 0`.
   - `**/frames/*.json` glob returned `total: 0`.
   - `/kikx/agents` still returned `total: 9`.
10. AeorDB was gracefully stopped with `SIGTERM` and waited until the process exited.
11. The post-delete/pre-restart evidence DB and log were copied into `/tmp/codex/aeordb-evidence/kikx-post-session-delete-20260605T062831Z/`, then copied into this report directory.
12. AeorDB was restarted against the same database.
13. On restart, AeorDB logged dirty-startup recovery:
    - KV bucket page CRC failures on open.
    - `Rebuilding KV index from append log...`
    - many `Skipping corrupt entry during KV rebuild` warnings.
    - `KV rebuild complete: 39613 entries indexed in 0.25s`.
    - `Recovered voids via gap-scan after dirty startup, void_count: 16277, total_void_bytes: 4511187`.
14. After restart:
    - `*/session.json` remained `total: 0`.
    - `**/frames/*.json` remained `total: 0`.
    - recursive `**/*` returned `total: 7`.
    - the seven listed paths were directly readable (`GET 200`).
15. The seven paths were explicitly deleted.
16. Immediate repeated deletes returned `404`, but recursive listing still returned the same seven paths.
17. A later fresh observation again showed recursive listing `total: 7` and direct `GET 200` for those paths.

## Current Observed Paths

As captured in `current-http-observations.json`, recursive listing reports:

```text
/kikx/sessions/6e3e46d3-73de-4968-a5ee-997411e983f1/commits/0000000000000002-b5f97bdf-5a90-4be6-ac2f-5feb2dc1f874.json
/kikx/sessions/7dfae1af-3528-46ba-ac6d-ecd436ef8b71/interactions/126e9b8e-135b-48ec-ba8b-118fa60852d8/frames/0000000000000002-UserMessage-c1d92357-6926-4c43-aec3-f116be54e2b2.json
/kikx/sessions/6e3e46d3-73de-4968-a5ee-997411e983f1/commits/0000000000000018-8676167e-2773-497f-bee3-fd4cb471479f.json
/kikx/sessions/bbdcfe45-b376-4286-be68-783b6a31fb13/interactions/9f0cf722-45bb-4c22-b4d2-49a84e8e3da3/frames/0000000000000259-AgentMessage-196e3a18-d98c-4930-b27d-8e9799a1c85e.json
/kikx/sessions/71916237-9891-4a6a-a31d-7e429e811661/values/.aeordb-config/indexes.json
/kikx/sessions/7dfae1af-3528-46ba-ac6d-ecd436ef8b71/tool-log/.aeordb-config/indexes.json
/kikx/sessions/86892975-ef3c-403c-aa8f-c263b8ffd588/tool-log/.aeordb-config/indexes.json
```

## Important Log Signals

From `evidence/aeordb-pre-restart.log`:

```text
WARN aeordb::engine::disk_kv_store: Corrupt KV page at bucket ... page CRC mismatch ... Resetting.
ERROR aeordb::server::engine_routes: Engine: failed to delete 'kikx/sessions': Invalid input: Directory '/kikx/sessions' is not empty (10 children)
ERROR aeordb::server::engine_routes: Engine: failed to delete 'kikx/sessions': Invalid input: Directory '/kikx/sessions' is not empty (5 children)
```

From `evidence/aeordb-restart-rebuild.log`:

```text
WARN aeordb::engine::disk_kv_store: KV bucket page failed CRC on open - triggering dirty startup
INFO aeordb::engine::storage_engine: Rebuilding KV index from append log...
WARN aeordb::engine::storage_engine: Skipping corrupt entry during KV rebuild: Corrupt entry at offset ... Corrupt header: Invalid magic bytes
INFO aeordb::engine::storage_engine: KV rebuild complete: 39613 entries indexed in 0.25s
INFO aeordb::engine::storage_engine: Recovered voids via gap-scan after dirty startup, void_count: 16277, total_void_bytes: 4511187, wal_start: 4194816
```

## Reproduction/Inspection Procedure

Use the preserved post-delete/pre-restart DB artifact:

```bash
cd /home/wyatt/Projects/aeordb-workspace/aeordb
mkdir -p /tmp/codex/aeordb-repro
cp bot-docs/bug-reports/2026-06-05-kikx-session-delete-ghost-listing/evidence/kikx-post-session-delete-pre-restart.aeordb /tmp/codex/aeordb-repro/kikx.aeordb
target/release/aeordb start -D /tmp/codex/aeordb-repro/kikx.aeordb --host 127.0.0.1 --port 6831 --auth self
```

Then query with an authorized token:

```text
GET /files/kikx/sessions?depth=-1&glob=*/session.json&limit=500
GET /files/kikx/sessions?depth=-1&glob=**/frames/*.json&limit=500
GET /files/kikx/sessions?depth=-1&glob=**/*&limit=1000
GET /files/<each path returned by recursive listing>
DELETE /files/<each path returned by recursive listing>
GET /files/kikx/sessions?depth=-1&glob=**/*&limit=1000
```

Expected suspicious behavior from this incident:

- targeted session/frame globs may report zero,
- recursive `**/*` reports old entries,
- direct `GET`/`DELETE` behavior may vary across restart/rebuild and repeated operations,
- root directory delete refuses because AeorDB still believes child entries exist.

## Why This Is Not Kikx Runtime Cache

- Kikx was stopped before the delete pass.
- AeorDB was gracefully stopped and restarted.
- Restart triggered AeorDB storage recovery/rebuild, then the paths reappeared as direct `GET 200`.
- The inconsistency is observable with raw AeorDB HTTP calls without Kikx involvement.

## Open Questions for AeorDB Investigation

- Are delete voids being recovered correctly during dirty-startup KV rebuild?
- Can recursive listing read directory/index state that direct file lookup/delete does not agree with?
- Can recursive glob `**/*` include stale entries that more specific globs exclude?
- Do `.aeordb-config` and `.aeordb-indexes` entries interact incorrectly with directory child counts after deletion?
- Why did the same path return `DELETE 200`, later `DELETE 404`, and later fresh `GET 200` again?
- Why does root `/kikx/sessions` child count stay non-zero after all visible paths have been deleted or report missing?

## Service State at Report Creation

At the time this report was written:

- AeorDB was running on `127.0.0.1:6830`.
- Kikx was running on `127.0.0.1:3001`.
- `/kikx/agents` remained present.
- Kikx session manifests were cleared from the normal `*/session.json` query path.

