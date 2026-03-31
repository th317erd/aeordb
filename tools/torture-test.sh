#!/bin/bash
#
# AeorDB Torture Test — Try to break it.
#
# Tests: long paths, deep nesting, large files, deletes, versions,
# snapshots, forks, data integrity, edge cases.
#
set -uo pipefail

SERVER="http://localhost:3360"
RESULTS_FILE="/home/wyatt/Projects/aeordb/bot-docs/docs/torture-test-results.md"
DB_PATH="${1:-/media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/torture.aeor}"
BINARY="/home/wyatt/Projects/aeordb/target/release/aeordb-cli"
VIDEOS_DIR="/media/Data/Remote/Seafile/wyatt-desktop/Videos/Porn"
PICTURES_DIR="$HOME/Pictures"

# Tracking
TOTAL_TESTS=0
PASSED=0
FAILED=0
RESULTS=""

log() { echo "$1"; RESULTS+="$1"$'\n'; }
pass() { TOTAL_TESTS=$((TOTAL_TESTS+1)); PASSED=$((PASSED+1)); log "  ✅ $1"; }
fail() { TOTAL_TESTS=$((TOTAL_TESTS+1)); FAILED=$((FAILED+1)); log "  ❌ $1"; }

log "# AeorDB Torture Test Results"
log ""
log "Date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
log "Database: $DB_PATH"
log ""

# ─── SETUP ─────────────────────────────────────────
log "## Setup"
rm -f "$DB_PATH"
rm -f /tmp/aeordb-engine-*.aeordb
mkdir -p "$(dirname "$DB_PATH")"

$BINARY start --database "$DB_PATH" --port 3360 --log-format json > /tmp/torture-server.log 2>&1 &
SERVER_PID=$!
trap "kill $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null" EXIT
sleep 3

if ! curl -sf "$SERVER/admin/health" > /dev/null; then
  log "SERVER FAILED TO START"
  cat /tmp/torture-server.log
  exit 1
fi

API_KEY=$(grep "aeor_k_" /tmp/torture-server.log | tr -d ' ')
JWT=$(curl -sf -X POST "$SERVER/auth/token" \
  -H "Content-Type: application/json" \
  -d "{\"api_key\": \"$API_KEY\"}" | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")
log "Server running, authenticated."
log ""

ENGINE_FILE=$(find /tmp -name "aeordb-engine-*.aeordb" -newer /tmp/torture-server.log 2>/dev/null | head -1)
log "Engine file: $ENGINE_FILE"
log ""

auth() { echo "Authorization: Bearer $JWT"; }

# ─── TEST 1: LONG FILE PATHS ──────────────────────
log "## Test 1: Long File Paths"
log ""

# 1a: Moderately long path (200 chars)
LONG_PATH="engine/$(python3 -c "print('/'.join(['segment_' + str(i) for i in range(20)]))")/file.txt"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/$LONG_PATH" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "long path test" 2>/dev/null)
[ "$CODE" = "201" ] && pass "200-char path: stored" || fail "200-char path: HTTP $CODE"

# 1b: Read it back
BODY=$(curl -sf "$SERVER/$LONG_PATH" -H "$(auth)" 2>/dev/null)
[ "$BODY" = "long path test" ] && pass "200-char path: read back matches" || fail "200-char path: read mismatch"

# 1c: Very long path (1000+ chars)
VERY_LONG="engine/$(python3 -c "print('/'.join(['very_long_segment_name_' + str(i).zfill(4) for i in range(40)]))")/deep_file.dat"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/$VERY_LONG" \
  -H "$(auth)" -H "Content-Type: application/octet-stream" -d "deep nesting test" 2>/dev/null)
[ "$CODE" = "201" ] && pass "1000-char path (40 levels deep): stored" || fail "1000-char path: HTTP $CODE"

BODY=$(curl -sf "$SERVER/$VERY_LONG" -H "$(auth)" 2>/dev/null)
[ "$BODY" = "deep nesting test" ] && pass "1000-char path: read back matches" || fail "1000-char path: read mismatch"

log ""

# ─── TEST 2: UNIQUE PATHS PER FILE ────────────────
log "## Test 2: 200 Files, Each at a Unique Deep Path"
log ""

UP=0; FAIL_COUNT=0
mapfile -t IMGS < <(find "$PICTURES_DIR" -type f -name "*.jpg" | shuf | head -200)
START=$(date +%s%N)
for i in "${!IMGS[@]}"; do
  F="${IMGS[$i]}"
  B=$(basename "$F")
  # Each file gets its own unique deep path
  UNIQUE_PATH="engine/torture/unique/category_$((i % 10))/subcategory_$((i % 5))/batch_$((i / 50))/item_${i}/$B"
  CT=$(file --mime-type -b "$F")
  CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/$UNIQUE_PATH" \
    -H "$(auth)" -H "Content-Type: $CT" --data-binary "@$F" 2>/dev/null)
  [ "$CODE" = "201" ] && UP=$((UP+1)) || FAIL_COUNT=$((FAIL_COUNT+1))
done
END=$(date +%s%N); DUR=$(( (END-START)/1000000 ))
[ $UP -ge 195 ] && pass "200 unique deep paths: $UP stored in ${DUR}ms" || fail "Only $UP/200 stored ($FAIL_COUNT failed)"
log ""

# ─── TEST 3: LARGE FILES ──────────────────────────
log "## Test 3: Large Files"
log ""

if [ -d "$VIDEOS_DIR" ]; then
  mapfile -t VIDS < <(find "$VIDEOS_DIR" -type f -name "*.mp4" -size +100M -size -2G | shuf | head -2)
  for V in "${VIDS[@]}"; do
    VB=$(basename "$V")
    VSZ=$(stat -c%s "$V")
    VMB=$((VSZ/1024/1024))
    log "  Uploading: $VB ($VMB MB)..."
    START=$(date +%s%N)
    CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 600 \
      -X PUT "$SERVER/engine/torture/videos/$VB" \
      -H "$(auth)" -H "Content-Type: video/mp4" --data-binary "@$V" 2>/dev/null)
    END=$(date +%s%N); VDUR=$(( (END-START)/1000000 ))

    if [ "$CODE" = "201" ]; then
      SPEED=$(echo "scale=1; $VSZ*1000/$VDUR/1048576" | bc)
      pass "Large file $VB ($VMB MB): stored in ${VDUR}ms ($SPEED MB/sec)"

      # Verify integrity
      ORIG_HASH=$(sha256sum "$V" | awk '{print $1}')
      curl -sf --max-time 600 -o /tmp/torture-dl-$$ \
        "$SERVER/engine/torture/videos/$VB" -H "$(auth)" 2>/dev/null
      DL_HASH=$(sha256sum /tmp/torture-dl-$$ 2>/dev/null | awk '{print $1}')
      rm -f /tmp/torture-dl-$$
      [ "$ORIG_HASH" = "$DL_HASH" ] && pass "Large file $VB: integrity verified" || fail "Large file $VB: HASH MISMATCH"
    else
      fail "Large file $VB ($VMB MB): HTTP $CODE after ${VDUR}ms"
    fi
  done
else
  log "  (Videos directory not available — skipping)"
fi
log ""

# ─── TEST 4: SNAPSHOTS ────────────────────────────
log "## Test 4: Snapshots"
log ""

# Create a snapshot
SNAP_RESP=$(curl -sf -X POST "$SERVER/version/snapshot" \
  -H "$(auth)" -H "Content-Type: application/json" \
  -d '{"name": "torture-v1", "metadata": {"phase": "after-uploads"}}' 2>/dev/null)
echo "$SNAP_RESP" | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null && \
  pass "Snapshot 'torture-v1' created" || fail "Snapshot creation failed: $SNAP_RESP"

# List snapshots
SNAP_LIST=$(curl -sf "$SERVER/version/snapshots" -H "$(auth)" 2>/dev/null)
SNAP_COUNT=$(echo "$SNAP_LIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")
[ "$SNAP_COUNT" -ge 1 ] && pass "Snapshot listed ($SNAP_COUNT total)" || fail "Snapshot list failed"

log ""

# ─── TEST 5: DELETE AND VERIFY ─────────────────────
log "## Test 5: Delete Files and Verify Gone"
log ""

# Delete 50 files from the unique paths
DELETED=0
for i in $(seq 0 49); do
  F="${IMGS[$i]}"
  B=$(basename "$F")
  UNIQUE_PATH="engine/torture/unique/category_$((i % 10))/subcategory_$((i % 5))/batch_$((i / 50))/item_${i}/$B"
  CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$SERVER/$UNIQUE_PATH" -H "$(auth)" 2>/dev/null)
  [ "$CODE" = "200" ] && DELETED=$((DELETED+1))
done
[ $DELETED -ge 45 ] && pass "Deleted $DELETED/50 files" || fail "Only deleted $DELETED/50"

# Verify they're actually gone (should 404)
GONE=0
for i in $(seq 0 9); do
  F="${IMGS[$i]}"
  B=$(basename "$F")
  UNIQUE_PATH="engine/torture/unique/category_$((i % 10))/subcategory_$((i % 5))/batch_$((i / 50))/item_${i}/$B"
  CODE=$(curl -s -o /dev/null -w "%{http_code}" "$SERVER/$UNIQUE_PATH" -H "$(auth)" 2>/dev/null)
  [ "$CODE" = "404" ] && GONE=$((GONE+1))
done
[ $GONE -eq 10 ] && pass "All 10 checked deleted files return 404" || fail "Only $GONE/10 return 404"

log ""

# ─── TEST 6: SNAPSHOT AFTER DELETE ─────────────────
log "## Test 6: Snapshot After Deletes"
log ""

SNAP2=$(curl -sf -X POST "$SERVER/version/snapshot" \
  -H "$(auth)" -H "Content-Type: application/json" \
  -d '{"name": "torture-v2-after-deletes", "metadata": {"phase": "after-deletes"}}' 2>/dev/null)
echo "$SNAP2" | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null && \
  pass "Snapshot 'torture-v2-after-deletes' created" || fail "Post-delete snapshot failed"

log ""

# ─── TEST 7: FORKS ────────────────────────────────
log "## Test 7: Forks"
log ""

# Create a fork
FORK_RESP=$(curl -sf -X POST "$SERVER/version/fork" \
  -H "$(auth)" -H "Content-Type: application/json" \
  -d '{"name": "torture-branch", "base": "HEAD"}' 2>/dev/null)
echo "$FORK_RESP" | python3 -c "import sys,json; json.load(sys.stdin)" 2>/dev/null && \
  pass "Fork 'torture-branch' created" || fail "Fork creation failed: $FORK_RESP"

# List forks
FORK_LIST=$(curl -sf "$SERVER/version/forks" -H "$(auth)" 2>/dev/null)
FORK_COUNT=$(echo "$FORK_LIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")
[ "$FORK_COUNT" -ge 1 ] && pass "Fork listed ($FORK_COUNT total)" || fail "Fork list failed"

# Promote fork
PROMOTE_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
  -X POST "$SERVER/version/fork/torture-branch/promote" -H "$(auth)" 2>/dev/null)
[ "$PROMOTE_CODE" = "200" ] && pass "Fork promoted to HEAD" || fail "Fork promotion: HTTP $PROMOTE_CODE"

log ""

# ─── TEST 8: SPECIAL CHARACTERS ────────────────────
log "## Test 8: Special Characters in Paths"
log ""

# Spaces (URL-encoded)
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/special/file%20with%20spaces.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "spaces test" 2>/dev/null)
[ "$CODE" = "201" ] && pass "Filename with spaces (URL-encoded)" || fail "Spaces: HTTP $CODE"

# Dots in path
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/special/.hidden/file.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "hidden dir test" 2>/dev/null)
[ "$CODE" = "201" ] && pass "Dot-prefixed directory (.hidden)" || fail "Dot-prefix: HTTP $CODE"

# Unicode in filename
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/special/caf%C3%A9.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "unicode test" 2>/dev/null)
[ "$CODE" = "201" ] && pass "Unicode filename (café)" || fail "Unicode: HTTP $CODE"

# Hyphens and underscores
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/special/my-file_v2.0.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "hyphens and underscores" 2>/dev/null)
[ "$CODE" = "201" ] && pass "Hyphens and underscores in filename" || fail "Hyphens: HTTP $CODE"

log ""

# ─── TEST 9: EMPTY AND TINY FILES ──────────────────
log "## Test 9: Edge Case File Sizes"
log ""

# Empty file (0 bytes)
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/edge/empty.dat" \
  -H "$(auth)" -H "Content-Type: application/octet-stream" -d "" 2>/dev/null)
[ "$CODE" = "201" ] && pass "Empty file (0 bytes)" || fail "Empty file: HTTP $CODE"

# 1 byte file
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/edge/one_byte.dat" \
  -H "$(auth)" -H "Content-Type: application/octet-stream" -d "X" 2>/dev/null)
[ "$CODE" = "201" ] && pass "1-byte file" || fail "1-byte: HTTP $CODE"

BODY=$(curl -sf "$SERVER/engine/torture/edge/one_byte.dat" -H "$(auth)" 2>/dev/null)
[ "$BODY" = "X" ] && pass "1-byte file: read back matches" || fail "1-byte: read mismatch ('$BODY')"

# Exactly 256KB (one chunk boundary)
dd if=/dev/urandom bs=262144 count=1 2>/dev/null > /tmp/torture-256k
ORIG_HASH=$(sha256sum /tmp/torture-256k | awk '{print $1}')
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/edge/exact_256k.dat" \
  -H "$(auth)" -H "Content-Type: application/octet-stream" --data-binary "@/tmp/torture-256k" 2>/dev/null)
[ "$CODE" = "201" ] && pass "Exactly 256KB file" || fail "256KB: HTTP $CODE"

curl -sf -o /tmp/torture-256k-dl "$SERVER/engine/torture/edge/exact_256k.dat" -H "$(auth)" 2>/dev/null
DL_HASH=$(sha256sum /tmp/torture-256k-dl 2>/dev/null | awk '{print $1}')
[ "$ORIG_HASH" = "$DL_HASH" ] && pass "256KB boundary: integrity verified" || fail "256KB: HASH MISMATCH"
rm -f /tmp/torture-256k /tmp/torture-256k-dl

# 256KB + 1 byte (just over chunk boundary)
dd if=/dev/urandom bs=262145 count=1 2>/dev/null > /tmp/torture-256k1
ORIG_HASH=$(sha256sum /tmp/torture-256k1 | awk '{print $1}')
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$SERVER/engine/torture/edge/over_256k.dat" \
  -H "$(auth)" -H "Content-Type: application/octet-stream" --data-binary "@/tmp/torture-256k1" 2>/dev/null)
[ "$CODE" = "201" ] && pass "256KB+1 byte file (just over boundary)" || fail "256KB+1: HTTP $CODE"

curl -sf -o /tmp/torture-256k1-dl "$SERVER/engine/torture/edge/over_256k.dat" -H "$(auth)" 2>/dev/null
DL_HASH=$(sha256sum /tmp/torture-256k1-dl 2>/dev/null | awk '{print $1}')
[ "$ORIG_HASH" = "$DL_HASH" ] && pass "256KB+1 boundary: integrity verified" || fail "256KB+1: HASH MISMATCH"
rm -f /tmp/torture-256k1 /tmp/torture-256k1-dl

log ""

# ─── TEST 10: OVERWRITE AND MUTATE ─────────────────
log "## Test 10: File Overwrite and Mutation"
log ""

# Write version 1
echo "version 1 content" | curl -s -o /dev/null -w "%{http_code}" \
  -X PUT "$SERVER/engine/torture/mutate/evolving.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d @- 2>/dev/null
V1=$(curl -sf "$SERVER/engine/torture/mutate/evolving.txt" -H "$(auth)" 2>/dev/null)
[ "$V1" = "version 1 content" ] && pass "Version 1 written and read" || fail "Version 1 mismatch: '$V1'"

# Overwrite with version 2
CODE=$(curl -s -o /dev/null -w "%{http_code}" \
  -X PUT "$SERVER/engine/torture/mutate/evolving.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "version 2 content - now longer" 2>/dev/null)
V2=$(curl -sf "$SERVER/engine/torture/mutate/evolving.txt" -H "$(auth)" 2>/dev/null)
[ "$V2" = "version 2 content - now longer" ] && pass "Version 2 overwrites version 1" || fail "Version 2 mismatch: '$V2'"

# Overwrite with version 3 (shorter)
CODE=$(curl -s -o /dev/null -w "%{http_code}" \
  -X PUT "$SERVER/engine/torture/mutate/evolving.txt" \
  -H "$(auth)" -H "Content-Type: text/plain" -d "v3" 2>/dev/null)
V3=$(curl -sf "$SERVER/engine/torture/mutate/evolving.txt" -H "$(auth)" 2>/dev/null)
[ "$V3" = "v3" ] && pass "Version 3 (shorter) overwrites version 2" || fail "Version 3 mismatch: '$V3'"

log ""

# ─── TEST 11: DIRECTORY LISTINGS ───────────────────
log "## Test 11: Directory Listings"
log ""

# List a directory we know has files
LIST=$(curl -sf "$SERVER/engine/torture/special/" -H "$(auth)" 2>/dev/null)
COUNT=$(echo "$LIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "err")
[ "$COUNT" != "err" ] && [ "$COUNT" -ge 3 ] && pass "Directory listing: $COUNT entries in /torture/special/" || fail "Directory listing failed: count=$COUNT"

# List root torture directory
LIST=$(curl -sf "$SERVER/engine/torture/" -H "$(auth)" 2>/dev/null)
COUNT=$(echo "$LIST" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "err")
[ "$COUNT" != "err" ] && [ "$COUNT" -ge 3 ] && pass "Root torture directory: $COUNT entries" || fail "Root listing: count=$COUNT"

log ""

# ─── TEST 12: BINARY DATA INTEGRITY ───────────────
log "## Test 12: Binary Data Integrity (random bytes)"
log ""

# Generate random binary data of various sizes and verify roundtrip
for SIZE in 1 100 1000 10000 100000 1000000; do
  dd if=/dev/urandom bs=$SIZE count=1 2>/dev/null > /tmp/torture-bin-$SIZE
  ORIG=$(sha256sum /tmp/torture-bin-$SIZE | awk '{print $1}')

  curl -sf -o /dev/null -X PUT "$SERVER/engine/torture/binary/random_${SIZE}.bin" \
    -H "$(auth)" -H "Content-Type: application/octet-stream" \
    --data-binary "@/tmp/torture-bin-$SIZE" 2>/dev/null

  curl -sf -o /tmp/torture-bin-dl-$SIZE "$SERVER/engine/torture/binary/random_${SIZE}.bin" -H "$(auth)" 2>/dev/null
  DL=$(sha256sum /tmp/torture-bin-dl-$SIZE 2>/dev/null | awk '{print $1}')

  [ "$ORIG" = "$DL" ] && pass "Binary integrity ${SIZE} bytes" || fail "Binary ${SIZE} bytes: HASH MISMATCH"
  rm -f /tmp/torture-bin-$SIZE /tmp/torture-bin-dl-$SIZE
done

log ""

# ─── FINAL STATS ──────────────────────────────────
ENGINE_SIZE=$(stat -c%s "$ENGINE_FILE" 2>/dev/null || echo "0")

log "## Final Results"
log ""
log "| Metric | Value |"
log "|---|---|"
log "| Total tests | $TOTAL_TESTS |"
log "| Passed | $PASSED |"
log "| Failed | $FAILED |"
log "| Engine file | $((ENGINE_SIZE/1024/1024)) MB |"
log ""

if [ $FAILED -eq 0 ]; then
  log "**ALL TESTS PASSED** 🎉"
else
  log "**$FAILED TESTS FAILED** ❌"
fi

# Write results to file
echo "$RESULTS" > "$RESULTS_FILE"
echo ""
echo "Results written to: $RESULTS_FILE"
echo ""
echo "═══════════════════════════════════════"
echo "  $PASSED / $TOTAL_TESTS passed ($FAILED failed)"
echo "═══════════════════════════════════════"
