#!/bin/bash
#
# AeorDB Directory Stress Test
#
# Generates random files and measures performance at scale:
# - Write throughput (files/sec) as directory grows
# - List directory latency at various sizes
# - Read latency for random files
# - HEAD latency for random files
#
# Usage:
#   ./tools/directory-stress-test.sh [--db-path /tmp/stress.aeordb] [--port 3040] [--count 10000] [--batch 500]
#
# Output: CSV-style metrics to stdout, human-readable progress to stderr
#

set -euo pipefail

# Defaults
DB_PATH="${DB_PATH:-/tmp/aeordb-dir-stress.aeordb}"
PORT="${PORT:-3040}"
FILE_COUNT="${FILE_COUNT:-10000}"
BATCH_SIZE="${BATCH_SIZE:-500}"
BINARY="$(dirname "$0")/../target/release/aeordb-cli"
SERVER="http://localhost:$PORT"
RESULTS_FILE=""

# Parse args
while [[ $# -gt 0 ]]; do
  case $1 in
    --db-path) DB_PATH="$2"; shift 2 ;;
    --port) PORT="$2"; SERVER="http://localhost:$PORT"; shift 2 ;;
    --count) FILE_COUNT="$2"; shift 2 ;;
    --batch) BATCH_SIZE="$2"; shift 2 ;;
    --output) RESULTS_FILE="$2"; shift 2 ;;
    *) echo "Unknown arg: $1" >&2; exit 1 ;;
  esac
done

# Ensure release binary exists
if [ ! -f "$BINARY" ]; then
  echo "Building release binary..." >&2
  (cd "$(dirname "$0")/.." && cargo build --release 2>&1) >&2
fi

# Cleanup function
cleanup() {
  if [ -n "${SERVER_PID:-}" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Start server
rm -f "$DB_PATH"
echo "Starting server: db=$DB_PATH port=$PORT" >&2
"$BINARY" start --auth=false --database "$DB_PATH" --port "$PORT" &>/dev/null &
SERVER_PID=$!
sleep 3

# Verify server is up
if ! curl -s -o /dev/null -w "" "$SERVER/admin/health"; then
  echo "ERROR: Server failed to start" >&2
  exit 1
fi
echo "Server running (PID $SERVER_PID)" >&2

# Helper: high-precision timestamp in milliseconds
now_ms() {
  python3 -c "import time; print(int(time.time() * 1000))"
}

# Helper: measure a curl request, return time in ms
measure_curl() {
  curl -s -o /dev/null -w "%{time_total}" "$@" | python3 -c "import sys; print(int(float(sys.stdin.read().strip()) * 1000))"
}

# CSV header
HEADER="operation,file_count,batch_num,duration_ms,rate_per_sec,db_size_bytes,detail"
echo "$HEADER"
if [ -n "$RESULTS_FILE" ]; then
  echo "$HEADER" > "$RESULTS_FILE"
fi

log_metric() {
  local line="$1"
  echo "$line"
  if [ -n "$RESULTS_FILE" ]; then
    echo "$line" >> "$RESULTS_FILE"
  fi
}

# ============================================================
# Phase 1: Write files in batches, measuring throughput
# ============================================================
echo "" >&2
echo "=== PHASE 1: Write throughput test ($FILE_COUNT files in batches of $BATCH_SIZE) ===" >&2

TOTAL_WRITTEN=0
BATCH_NUM=0

while [ "$TOTAL_WRITTEN" -lt "$FILE_COUNT" ]; do
  BATCH_NUM=$((BATCH_NUM + 1))
  BATCH_START=$(now_ms)
  BATCH_COUNT=0

  while [ "$BATCH_COUNT" -lt "$BATCH_SIZE" ] && [ "$TOTAL_WRITTEN" -lt "$FILE_COUNT" ]; do
    # Generate random file content (small JSON docs, ~200-500 bytes)
    IDX=$TOTAL_WRITTEN
    HASH=$(echo "${IDX}-$(date +%s%N)" | sha256sum | cut -c1-16)
    NAME="file_${HASH}"
    AGE=$((RANDOM % 80 + 18))
    DEPT=$([ $((RANDOM % 3)) -eq 0 ] && echo "engineering" || ([ $((RANDOM % 2)) -eq 0 ] && echo "sales" || echo "marketing"))

    curl -s -X PUT "$SERVER/engine/data/${NAME}.json" \
      -H "Content-Type: application/json" \
      -d "{\"name\":\"${NAME}\",\"age\":${AGE},\"department\":\"${DEPT}\",\"index\":${IDX},\"hash\":\"${HASH}\"}" \
      -o /dev/null &

    BATCH_COUNT=$((BATCH_COUNT + 1))
    TOTAL_WRITTEN=$((TOTAL_WRITTEN + 1))

    # Limit concurrent curls
    if [ $((BATCH_COUNT % 50)) -eq 0 ]; then
      wait
    fi
  done

  wait # Wait for all curls in batch to finish
  BATCH_END=$(now_ms)
  BATCH_DURATION=$((BATCH_END - BATCH_START))

  # Get DB size
  DB_SIZE=$(stat --printf="%s" "$DB_PATH" 2>/dev/null || echo 0)

  # Calculate rate
  if [ "$BATCH_DURATION" -gt 0 ]; then
    RATE=$(python3 -c "print(round($BATCH_COUNT / ($BATCH_DURATION / 1000.0), 1))")
  else
    RATE="inf"
  fi

  log_metric "write,$TOTAL_WRITTEN,$BATCH_NUM,${BATCH_DURATION},${RATE},${DB_SIZE},batch_of_${BATCH_COUNT}"
  echo "  Batch $BATCH_NUM: $BATCH_COUNT files in ${BATCH_DURATION}ms (${RATE}/s) | total=$TOTAL_WRITTEN | db=$(python3 -c "print(f'{$DB_SIZE/1024/1024:.1f}MB')")" >&2
done

echo "" >&2
echo "Write phase complete: $TOTAL_WRITTEN files written" >&2

# ============================================================
# Phase 2: List directory at various depths
# ============================================================
echo "" >&2
echo "=== PHASE 2: List directory latency ===" >&2

# List root
LIST_MS=$(measure_curl "$SERVER/engine/data/")
log_metric "list_directory,$TOTAL_WRITTEN,1,${LIST_MS},,$(stat --printf='%s' "$DB_PATH" 2>/dev/null || echo 0),/data/"
echo "  List /data/ ($TOTAL_WRITTEN files): ${LIST_MS}ms" >&2

# List root multiple times for consistency
for i in $(seq 1 5); do
  LIST_MS=$(measure_curl "$SERVER/engine/data/")
  log_metric "list_directory,$TOTAL_WRITTEN,$((i+1)),${LIST_MS},,$(stat --printf='%s' "$DB_PATH" 2>/dev/null || echo 0),/data/_run_${i}"
done
echo "  5 more list measurements recorded" >&2

# ============================================================
# Phase 3: Random read latency
# ============================================================
echo "" >&2
echo "=== PHASE 3: Random read latency (50 reads) ===" >&2

# Get a sample of file paths from the list
SAMPLE_PATHS=$(curl -s "$SERVER/engine/data/" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    children = data if isinstance(data, list) else data.get('children', [])
    import random
    random.shuffle(children)
    for c in children[:50]:
        print('/data/' + c.get('name', ''))
except:
    pass
" 2>/dev/null)

READ_TOTAL=0
READ_COUNT=0
while IFS= read -r path; do
  if [ -z "$path" ]; then continue; fi
  READ_MS=$(measure_curl "$SERVER/engine${path}")
  READ_TOTAL=$((READ_TOTAL + READ_MS))
  READ_COUNT=$((READ_COUNT + 1))
done <<< "$SAMPLE_PATHS"

if [ "$READ_COUNT" -gt 0 ]; then
  READ_AVG=$(python3 -c "print(round($READ_TOTAL / $READ_COUNT, 1))")
  log_metric "read,$TOTAL_WRITTEN,$READ_COUNT,${READ_TOTAL},,$(stat --printf='%s' "$DB_PATH" 2>/dev/null || echo 0),avg_${READ_AVG}ms"
  echo "  $READ_COUNT reads, total ${READ_TOTAL}ms, avg ${READ_AVG}ms" >&2
fi

# ============================================================
# Phase 4: HEAD latency
# ============================================================
echo "" >&2
echo "=== PHASE 4: HEAD latency (50 requests) ===" >&2

HEAD_TOTAL=0
HEAD_COUNT=0
while IFS= read -r path; do
  if [ -z "$path" ]; then continue; fi
  HEAD_MS=$(measure_curl -I "$SERVER/engine${path}")
  HEAD_TOTAL=$((HEAD_TOTAL + HEAD_MS))
  HEAD_COUNT=$((HEAD_COUNT + 1))
done <<< "$SAMPLE_PATHS"

if [ "$HEAD_COUNT" -gt 0 ]; then
  HEAD_AVG=$(python3 -c "print(round($HEAD_TOTAL / $HEAD_COUNT, 1))")
  log_metric "head,$TOTAL_WRITTEN,$HEAD_COUNT,${HEAD_TOTAL},,$(stat --printf='%s' "$DB_PATH" 2>/dev/null || echo 0),avg_${HEAD_AVG}ms"
  echo "  $HEAD_COUNT HEAD requests, total ${HEAD_TOTAL}ms, avg ${HEAD_AVG}ms" >&2
fi

# ============================================================
# Phase 5: Write more files and re-measure list
# ============================================================
echo "" >&2
echo "=== PHASE 5: Incremental growth + re-measure ===" >&2

CHECKPOINTS=(1000 2000 5000 10000 20000 50000)
for CHECKPOINT in "${CHECKPOINTS[@]}"; do
  if [ "$TOTAL_WRITTEN" -ge "$CHECKPOINT" ]; then
    continue
  fi
  if [ "$CHECKPOINT" -gt "$FILE_COUNT" ]; then
    break
  fi

  GROW_COUNT=$((CHECKPOINT - TOTAL_WRITTEN))
  echo "  Growing to $CHECKPOINT (adding $GROW_COUNT)..." >&2

  GROW_START=$(now_ms)
  for i in $(seq 1 "$GROW_COUNT"); do
    HASH=$(echo "${TOTAL_WRITTEN}-$(date +%s%N)" | sha256sum | cut -c1-16)
    curl -s -X PUT "$SERVER/engine/data/file_${HASH}.json" \
      -H "Content-Type: application/json" \
      -d "{\"name\":\"file_${HASH}\",\"index\":${TOTAL_WRITTEN}}" \
      -o /dev/null &
    TOTAL_WRITTEN=$((TOTAL_WRITTEN + 1))
    if [ $((i % 50)) -eq 0 ]; then wait; fi
  done
  wait
  GROW_END=$(now_ms)
  GROW_DURATION=$((GROW_END - GROW_START))

  # Measure list at this checkpoint
  LIST_MS=$(measure_curl "$SERVER/engine/data/")
  DB_SIZE=$(stat --printf="%s" "$DB_PATH" 2>/dev/null || echo 0)

  RATE=$(python3 -c "print(round($GROW_COUNT / ($GROW_DURATION / 1000.0), 1))" 2>/dev/null || echo "?")
  log_metric "checkpoint_write,$CHECKPOINT,1,${GROW_DURATION},${RATE},${DB_SIZE},grew_${GROW_COUNT}"
  log_metric "checkpoint_list,$CHECKPOINT,1,${LIST_MS},,${DB_SIZE},list_at_${CHECKPOINT}"

  echo "    Write: ${GROW_COUNT} files in ${GROW_DURATION}ms (${RATE}/s) | List: ${LIST_MS}ms | DB: $(python3 -c "print(f'{$DB_SIZE/1024/1024:.1f}MB')")" >&2
done

# ============================================================
# Summary
# ============================================================
echo "" >&2
echo "=== SUMMARY ===" >&2
DB_SIZE=$(stat --printf="%s" "$DB_PATH" 2>/dev/null || echo 0)
echo "  Total files: $TOTAL_WRITTEN" >&2
echo "  Database size: $(python3 -c "print(f'{$DB_SIZE/1024/1024:.1f}MB')")" >&2
echo "  Database path: $DB_PATH" >&2
if [ -n "$RESULTS_FILE" ]; then
  echo "  Results CSV: $RESULTS_FILE" >&2
fi
echo "" >&2
echo "Done!" >&2
