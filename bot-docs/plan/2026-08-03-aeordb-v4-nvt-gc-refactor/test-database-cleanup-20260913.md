# Test-database cleanup — September 13, 2026

Cleanup completed at 2026-09-13T07:27:53.353Z on `wyatt-desktop`, after all current
qualification runs had closed successfully. Exactly **20 databases** were removed:
**79,259,487,871 logical bytes / 79,259,533,312 allocated bytes** (about 79.26 GB).
Data free space increased from 277,887,242,240 to **357,146,759,168 bytes**.
Free-space changes can also reflect filesystem bookkeeping and concurrent activity.

The [machine receipt](evidence/p9-test-database-cleanup-20260913.json) records
every exact path, digest, physical identity, timestamp, disposition and retained
duplicate reference. Detailed admission/removal logs remain under
`/media/Data/AeorDB/Tests/p9-release-closeout-20260913/cleanup/`.

## Exact removals

All paths below are relative to `wyatt-desktop:/media/Data/AeorDB/Tests/`.
No recursive directory deletion or wildcard database deletion was performed.

| Relative database path | Logical bytes |
| --- | ---: |
| `p9-exact-9a71d4ce/long/s1/soak.aeordb` | 638,775,378 |
| `p9-exact-9a71d4ce/long/s2/soak.aeordb` | 7,772,085,386 |
| `p9-exact-9a71d4ce/long/s3/soak.aeordb` | 151,156,262 |
| `p8-media-rehearsal-20260903/target/shadow-08025b80-dirty-resume.aeordb` | 11,447,013,668 |
| `p8-media-rehearsal-20260903/migration-copy-v3.aeordb` | 11,654,356,123 |
| `p9-release-33420bad-20260910/media/source-v3.aeordb` | 11,654,356,141 |
| `p9-release-33420bad-20260910/media-capacity-retry/shadow-v4.aeordb` | 11,447,013,668 |
| `p9-release-48baeefe-20260911/media/source-v3.aeordb` | 11,654,356,141 |
| `p9-release-48baeefe-20260911/media/shadow-v4.aeordb` | 11,447,013,668 |
| `p9-release-a8047327-20260911/long/s1-short/soak.aeordb` | 13,332,832 |
| `p9-release-a8047327-20260911/long/s2-short/soak.aeordb` | 17,607,033 |
| `p9-release-a8047327-20260911/long/s3-short/soak.aeordb` | 3,702,318 |
| `p9-release-a8047327-20260911/long/s1-12h/soak.aeordb` | 621,590,063 |
| `p9-release-a8047327-20260911/long/s2-12h/soak.aeordb` | 540,173,616 |
| `p9-release-a8047327-20260911/long/s3-12h/soak.aeordb` | 142,162,844 |
| `p9-release-33420bad-20260910/long/s1-short/soak.aeordb` | 14,547,533 |
| `p9-release-33420bad-20260910/long/s2-short/soak.aeordb` | 14,246,961 |
| `p9-release-33420bad-20260910/long/s3-short/soak.aeordb` | 3,348,415 |
| `p9-release-48baeefe-20260911/long/s1-short/soak.aeordb` | 8,831,304 |
| `p9-release-48baeefe-20260911/long/s2-short/soak.aeordb` | 13,818,517 |

## Preservation and recoverability

Three redundant media input copies remain byte-for-byte recoverable from the
retained P8 `source-v3.aeordb` and `migration-clean-copy-v3.aeordb`; both retained
references were freshly checksummed. Three successful shadows and fourteen
successful soak databases were deliberately retired without raw-byte backups.
Their workloads/content can be regenerated, but new physical bytes need not
match the deleted images. Logs and checkpoint traces are retained.

Useful failed/unknown specimens were excluded, including the plain-verification
mutation copy, failed 33420bad S1, malformed 48baeefe S3 and all earlier closed
followup evidence. Retained source/failure fixture stats match before and after
cleanup. Source media, corpus, old binaries, source worktrees, logs and the
lossless maximum-stage capacity archive remain. FS-Server1 was not accessed.

Every admitted file was a regular, singly linked, current-user-owned file below
the exact authorized root, with no symlink in its path. Fresh SHA-256 and
physical/timestamp identity matched; duplicate references were not deletion
targets. Identity and current-user `fuser` checks were repeated before unlink.
Known owned test processes/services were closed. This is not a claim of
privileged visibility into every system process. The helper has ten passing
local and native-desktop checks, including refusal paths and post-admission
changes. Before/after-unlink events were durably recorded.

## Evidence availability after cleanup

The new closed release seal covers **278 metadata/evidence files**, not retired
raw database payloads. Manifest SHA-256:
`91740b6c41ad46c5a8f497e7f6dda1e9883981d2a427cd655b56b18ddee4287b`.

The [manifest copy](evidence/p9-release-closeout-20260913.sha256) is relative to
`/media/Data/AeorDB/Tests/p9-release-closeout-20260913/`; every listed file was
reverified after sealing, with a second verified mirror in the laptop cache.

Older manifests that name a removed payload remain historical records, but
cannot now reverify that deleted entry. This explicitly includes the 9a71
long-sequence S2/S3 artifact manifests and retired media database hash records.
These tombstones retire availability claims only; no old digest or failed result
was rewritten. The separate 236/209-file failure-followup seals remain intact.

No installation, publication, production change, activation or retained
production-database deletion occurred. The owner-required step-4 discussion
remains the next boundary.
