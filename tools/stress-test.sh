#!/bin/bash
#
# AeorDB Stress Test — Real Files
#
# Usage:
#   ./tools/stress-test.sh [--db-path /path/to/test.aeor] [--port 3334] [--images 200] [--videos 3]
#
# Requires: the server binary built at target/release/aeordb-cli
#

set -euo pipefail

# Defaults
DB_PATH="${DB_PATH:-/media/wyatt/Elements/wyatt-desktop/AORDB-TEST/stress.aeor}"
PORT="${PORT:-3334}"
IMAGE_COUNT="${IMAGE_COUNT:-200}"
VIDEO_COUNT="${VIDEO_COUNT:-3}"
PICTURES_DIR="$HOME/Pictures"
VIDEOS_DIR="/media/Data/Remote/Seafile/wyatt-desktop/Videos/Porn/Brazzers 2"
BINARY="$(dirname "$0")/../target/release/aeordb-cli"
SERVER="http://localhost:$PORT"

# Parse args
while [[ $# -gt 0 ]]; do
  case $1 in
    --db-path) DB_PATH="$2"; shift 2 ;;
    --port) PORT="$2"; SERVER="http://localhost:$PORT"; shift 2 ;;
    --images) IMAGE_COUNT="$2"; shift 2 ;;
    --videos) VIDEO_COUNT="$2"; shift 2 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

echo "══════════════════════════════════════════════════════"
echo "  AeorDB Stress Test"
echo "══════════════════════════════════════════════════════"
echo "  Database:  $DB_PATH"
echo "  Port:      $PORT"
echo "  Images:    $IMAGE_COUNT"
echo "  Videos:    $VIDEO_COUNT"
echo "══════════════════════════════════════════════════════"
echo ""

# Clean slate
rm -f "$DB_PATH"
mkdir -p "$(dirname "$DB_PATH")"

# Start server
echo "[SETUP] Starting server..."
$BINARY start --database "$DB_PATH" --port "$PORT" --log-format json > /tmp/aeordb-stress-server.log 2>&1 &
SERVER_PID=$!
trap "echo 'Stopping server...'; kill $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null; echo 'Done.'" EXIT
sleep 3

# Verify
if ! curl -sf "$SERVER/system/health" > /dev/null; then
  echo "Server failed to start! Log:"
  cat /tmp/aeordb-stress-server.log
  exit 1
fi
echo "  Server running (PID: $SERVER_PID)"

# Get API key and JWT
API_KEY=$(grep "aeor_k_" /tmp/aeordb-stress-server.log | tr -d ' ')
JWT=$(curl -sf -X POST "$SERVER/auth/token" \
  -H "Content-Type: application/json" \
  -d "{\"api_key\": \"$API_KEY\"}" | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")
echo "  Authenticated"
echo ""

# Helper function for timing
now_ms() { date +%s%N | cut -b1-13; }

# ─────────────────────────────────────────────────
# PHASE 1: Upload images
# ─────────────────────────────────────────────────
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PHASE 1: Upload $IMAGE_COUNT images"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

mapfile -t IMAGES < <(find "$PICTURES_DIR" -type f \( -name "*.jpg" -o -name "*.png" -o -name "*.jpeg" \) | shuf | head -"$IMAGE_COUNT")
echo "  Found ${#IMAGES[@]} image files"

UPLOADED=0; FAILED=0; BYTES=0
START=$(now_ms)
for FILE in "${IMAGES[@]}"; do
  BASENAME=$(basename "$FILE")
  # Add a unique suffix to avoid collisions from different directories
  UNIQUE_NAME="${BASENAME%.*}_$(echo "$FILE" | md5sum | head -c8).${BASENAME##*.}"
  CONTENT_TYPE=$(file --mime-type -b "$FILE")
  SIZE=$(stat -c%s "$FILE")

  CODE=$(curl -sf -o /dev/null -w "%{http_code}" \
    -X PUT "$SERVER/files/stress/images/$UNIQUE_NAME" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: $CONTENT_TYPE" \
    --data-binary "@$FILE" 2>/dev/null || echo "000")

  if [ "$CODE" = "201" ]; then
    UPLOADED=$((UPLOADED + 1)); BYTES=$((BYTES + SIZE))
  else
    FAILED=$((FAILED + 1))
  fi

  if [ $((UPLOADED % 50)) -eq 0 ] && [ $UPLOADED -gt 0 ]; then
    echo "  Progress: $UPLOADED / ${#IMAGES[@]}"
  fi
done
END=$(now_ms); DUR=$((END - START))

echo "  Uploaded: $UPLOADED files ($FAILED failed)"
echo "  Data: $((BYTES / 1024 / 1024)) MB in ${DUR}ms"
echo "  Throughput: $(echo "scale=1; $UPLOADED * 1000 / $DUR" | bc) files/sec"
echo "  Bandwidth: $(echo "scale=2; $BYTES * 1000 / $DUR / 1048576" | bc) MB/sec"
echo ""

TOTAL_BYTES=$BYTES

# ─────────────────────────────────────────────────
# PHASE 2: Upload large videos
# ─────────────────────────────────────────────────
if [ -d "$VIDEOS_DIR" ] && [ "$VIDEO_COUNT" -gt 0 ]; then
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  echo "  PHASE 2: Upload $VIDEO_COUNT large videos"
  echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

  mapfile -t VIDEOS < <(find "$VIDEOS_DIR" -type f -name "*.mp4" | sort -R | head -"$VIDEO_COUNT")

  for VIDEO in "${VIDEOS[@]}"; do
    BASENAME=$(basename "$VIDEO")
    SIZE=$(stat -c%s "$VIDEO")
    SIZE_MB=$((SIZE / 1024 / 1024))
    echo "  Uploading: $BASENAME ($SIZE_MB MB)..."

    START_V=$(now_ms)
    CODE=$(curl -sf -o /dev/null -w "%{http_code}" \
      -X PUT "$SERVER/files/stress/videos/$BASENAME" \
      -H "Authorization: Bearer $JWT" \
      -H "Content-Type: video/mp4" \
      --data-binary "@$VIDEO" 2>/dev/null || echo "000")
    END_V=$(now_ms); DUR_V=$((END_V - START_V))

    if [ "$CODE" = "201" ]; then
      echo "    ✓ Stored in ${DUR_V}ms ($(echo "scale=1; $SIZE * 1000 / $DUR_V / 1048576" | bc) MB/sec)"
      TOTAL_BYTES=$((TOTAL_BYTES + SIZE))
    else
      echo "    ✗ Failed (HTTP $CODE)"
    fi
  done
  echo ""
fi

# ─────────────────────────────────────────────────
# PHASE 3: Random reads
# ─────────────────────────────────────────────────
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PHASE 3: Read 50 random images"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# Get list of stored files
STORED_FILES=$(curl -sf "$SERVER/files/stress/images/" -H "Authorization: Bearer $JWT" | \
  python3 -c "import sys,json; [print(e['name']) for e in json.load(sys.stdin)]" 2>/dev/null)
mapfile -t STORED < <(echo "$STORED_FILES" | shuf | head -50)

READ_BYTES=0
START=$(now_ms)
for NAME in "${STORED[@]}"; do
  SIZE=$(curl -sf -o /dev/null -w "%{size_download}" \
    "$SERVER/files/stress/images/$NAME" \
    -H "Authorization: Bearer $JWT" 2>/dev/null || echo "0")
  READ_BYTES=$((READ_BYTES + SIZE))
done
END=$(now_ms); DUR=$((END - START))
echo "  Read ${#STORED[@]} files ($((READ_BYTES / 1024 / 1024)) MB) in ${DUR}ms"
echo "  Avg: $((DUR / ${#STORED[@]}))ms/file"
echo "  Bandwidth: $(echo "scale=2; $READ_BYTES * 1000 / $DUR / 1048576" | bc) MB/sec"
echo ""

# ─────────────────────────────────────────────────
# PHASE 4: Directory listing
# ─────────────────────────────────────────────────
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PHASE 4: Directory listings"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

START=$(now_ms)
COUNT=$(curl -sf "$SERVER/files/stress/images/" -H "Authorization: Bearer $JWT" | \
  python3 -c "import sys,json; print(len(json.load(sys.stdin)))")
END=$(now_ms)
echo "  images/: $COUNT entries in $((END - START))ms"

START=$(now_ms)
COUNT=$(curl -sf "$SERVER/files/stress/" -H "Authorization: Bearer $JWT" | \
  python3 -c "import sys,json; print(len(json.load(sys.stdin)))")
END=$(now_ms)
echo "  stress/: $COUNT entries in $((END - START))ms"
echo ""

# ─────────────────────────────────────────────────
# PHASE 5: Delete half the images
# ─────────────────────────────────────────────────
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PHASE 5: Delete half the images"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

DELETE_LIST=$(curl -sf "$SERVER/files/stress/images/" -H "Authorization: Bearer $JWT" | \
  python3 -c "import sys,json; names=[e['name'] for e in json.load(sys.stdin)]; [print(n) for n in names[:len(names)//2]]" 2>/dev/null)
mapfile -t TO_DELETE < <(echo "$DELETE_LIST")

DELETED=0
START=$(now_ms)
for NAME in "${TO_DELETE[@]}"; do
  CODE=$(curl -sf -o /dev/null -w "%{http_code}" \
    -X DELETE "$SERVER/files/stress/images/$NAME" \
    -H "Authorization: Bearer $JWT" 2>/dev/null || echo "000")
  [ "$CODE" = "200" ] && DELETED=$((DELETED + 1))
done
END=$(now_ms); DUR=$((END - START))
echo "  Deleted $DELETED / ${#TO_DELETE[@]} files in ${DUR}ms"
echo ""

# ─────────────────────────────────────────────────
# PHASE 6: Re-upload (churn test)
# ─────────────────────────────────────────────────
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  PHASE 6: Re-upload deleted files (churn)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

REUP=0
START=$(now_ms)
for FILE in "${IMAGES[@]:0:$((UPLOADED/2))}"; do
  BASENAME=$(basename "$FILE")
  UNIQUE_NAME="${BASENAME%.*}_$(echo "$FILE" | md5sum | head -c8).${BASENAME##*.}"
  CODE=$(curl -sf -o /dev/null -w "%{http_code}" \
    -X PUT "$SERVER/files/stress/images/$UNIQUE_NAME" \
    -H "Authorization: Bearer $JWT" \
    -H "Content-Type: image/jpeg" \
    --data-binary "@$FILE" 2>/dev/null || echo "000")
  [ "$CODE" = "201" ] && REUP=$((REUP + 1))
done
END=$(now_ms); DUR=$((END - START))
echo "  Re-uploaded $REUP files in ${DUR}ms"
echo ""

# ─────────────────────────────────────────────────
# RESULTS
# ─────────────────────────────────────────────────
echo "══════════════════════════════════════════════════════"
echo "  FINAL RESULTS"
echo "══════════════════════════════════════════════════════"

DB_SIZE=$(stat -c%s "$DB_PATH")
REMAINING=$(curl -sf "$SERVER/files/stress/images/" -H "Authorization: Bearer $JWT" | \
  python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "?")

echo "  Database file:   $((DB_SIZE / 1024 / 1024)) MB"
echo "  Data uploaded:   $((TOTAL_BYTES / 1024 / 1024)) MB"
echo "  Storage ratio:   $(echo "scale=1; $DB_SIZE * 100 / $TOTAL_BYTES" | bc)%"
echo "  Files remaining: $REMAINING"
echo ""

# Metrics
echo "  Prometheus Metrics:"
METRICS=$(curl -sf "$SERVER/system/metrics" -H "Authorization: Bearer $JWT" 2>/dev/null || echo "unavailable")
if [ "$METRICS" != "unavailable" ]; then
  echo "$METRICS" | grep -E "^aeordb_(files|chunks_stored|chunks_read|chunk_store)" | head -15
fi

echo ""
echo "══════════════════════════════════════════════════════"
echo "  Test complete. Server stopping..."
echo "══════════════════════════════════════════════════════"
