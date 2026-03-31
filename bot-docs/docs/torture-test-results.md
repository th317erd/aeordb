# AeorDB Torture Test Results

Date: 2026-03-31T06:41:56Z
Database: /media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/torture-final.aeordb

## Setup
Server running, authenticated.

Engine file: 

## Test 1: Long File Paths

  ✅ 200-char path: stored
  ✅ 200-char path: read back matches
  ✅ 1000-char path (40 levels deep): stored
  ✅ 1000-char path: read back matches

## Test 2: 200 Files, Each at a Unique Deep Path

  ✅ 200 unique deep paths: 196 stored in 14545ms

## Test 3: Large Files

  Uploading: LucyH-Sylvan_MyLovelyHousewife_1920x1080_mp4.mp4 (1018 MB)...
  ✅ Large file LucyH-Sylvan_MyLovelyHousewife_1920x1080_mp4.mp4 (1018 MB): stored in 17635ms (57.7 MB/sec)
  ✅ Large file LucyH-Sylvan_MyLovelyHousewife_1920x1080_mp4.mp4: integrity verified
  Uploading: f591141728.mp4 (713 MB)...
  ✅ Large file f591141728.mp4 (713 MB): stored in 12208ms (58.4 MB/sec)
  ✅ Large file f591141728.mp4: integrity verified

## Test 4: Snapshots

  ✅ Snapshot 'torture-v1' created
  ✅ Snapshot listed (1 total)

## Test 5: Delete Files and Verify Gone

  ✅ Deleted 48/50 files
  ✅ All 10 checked deleted files return 404

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
| Passed | 39 |
| Failed | 0 |
| Engine file | 0 MB |

**ALL TESTS PASSED** 🎉

