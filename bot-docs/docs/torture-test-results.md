# AeorDB Torture Test Results

Date: 2026-03-31T06:32:23Z
Database: /media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/torture2.aeordb

## Setup
Server running, authenticated.

Engine file: 

## Test 1: Long File Paths

  ✅ 200-char path: stored
  ✅ 200-char path: read back matches
  ✅ 1000-char path (40 levels deep): stored
  ✅ 1000-char path: read back matches

## Test 2: 200 Files, Each at a Unique Deep Path

  ✅ 200 unique deep paths: 198 stored in 20957ms

## Test 3: Large Files

  Uploading: f1281627912.mp4 (351 MB)...
  ✅ Large file f1281627912.mp4 (351 MB): stored in 5402ms (65.1 MB/sec)
  ✅ Large file f1281627912.mp4: integrity verified
  Uploading: f1433745832.mp4 (359 MB)...
  ✅ Large file f1433745832.mp4 (359 MB): stored in 5569ms (64.4 MB/sec)
  ✅ Large file f1433745832.mp4: integrity verified

## Test 4: Snapshots

  ✅ Snapshot 'torture-v1' created
  ✅ Snapshot listed (1 total)

## Test 5: Delete Files and Verify Gone

  ✅ Deleted 50/50 files
  ❌ Only 0/10 return 404

## Test 6: Snapshot After Deletes

  ✅ Snapshot 'torture-v2-after-deletes' created

## Test 7: Forks

  ✅ Fork 'torture-branch' created
  ✅ Fork listed (1 total)
  ✅ Fork promoted to HEAD

## Test 8: Special Characters in Paths

  ✅ Filename with spaces (URL-encoded)
  ✅ Dot-prefixed directory (.hidden)
  ✅ Unicode filename (café)
  ✅ Hyphens and underscores in filename

## Test 9: Edge Case File Sizes

  ✅ Empty file (0 bytes)
  ✅ 1-byte file
  ✅ 1-byte file: read back matches
  ✅ Exactly 256KB file
  ✅ 256KB boundary: integrity verified
  ✅ 256KB+1 byte file (just over boundary)
  ✅ 256KB+1 boundary: integrity verified

## Test 10: File Overwrite and Mutation

  ✅ Version 1 written and read
  ✅ Version 2 overwrites version 1
  ✅ Version 3 (shorter) overwrites version 2

## Test 11: Directory Listings

  ✅ Directory listing: 4 entries in /torture/special/
  ✅ Root torture directory: 5 entries

## Test 12: Binary Data Integrity (random bytes)

  ✅ Binary integrity 1 bytes
  ✅ Binary integrity 100 bytes
  ✅ Binary integrity 1000 bytes
  ✅ Binary integrity 10000 bytes
  ✅ Binary integrity 100000 bytes
  ✅ Binary integrity 1000000 bytes

## Final Results

| Metric | Value |
|---|---|
| Total tests | 39 |
| Passed | 38 |
| Failed | 1 |
| Engine file | 0 MB |

**1 TESTS FAILED** ❌

