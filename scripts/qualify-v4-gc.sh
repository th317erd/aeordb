#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "$0")/.."

CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
QUALIFICATION_DURATION_SECS="${AEORDB_GC_QUALIFICATION_DURATION_SECS:-30}"
QUALIFICATION_TIMEOUT_SECS="${AEORDB_GC_QUALIFICATION_TIMEOUT_SECS:-300}"
QUALIFICATION_PORT="${AEORDB_GC_QUALIFICATION_PORT:-$((16980 + ($$ % 400)))}"
QUALIFICATION_ROOT="${AEORDB_GC_QUALIFICATION_ROOT:-${XDG_CACHE_HOME:-$HOME/.cache}/codex/aeordb-tests/real-world/p4-8e-$(date -u +%Y%m%dT%H%M%SZ)}"
QUALIFICATION_TMPDIR="$QUALIFICATION_ROOT/temporary"
DATABASE="$QUALIFICATION_ROOT/live.aeordb"
LOAD_REPORT="$QUALIFICATION_ROOT/load-report.json"
RESOURCE_REPORT="$QUALIFICATION_ROOT/resource-report.json"
VERIFY_LOG="$QUALIFICATION_ROOT/verify.log"
SERVER_LOG="$QUALIFICATION_ROOT/server.log"
UNIT="aeordb-p4-8e-$RANDOM-$$.service"
BINARY="$(pwd)/target/debug/aeordb"

mkdir -p "$QUALIFICATION_TMPDIR"

if [ "${AEORDB_GC_QUALIFICATION_SKIP_BUILD:-0}" != "1" ]; then
  timeout "$QUALIFICATION_TIMEOUT_SECS" env \
    TMPDIR="$QUALIFICATION_TMPDIR" \
    CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" \
    cargo build -j "$CARGO_BUILD_JOBS" -p aeordb-cli --bin aeordb
fi

if [ ! -x "$BINARY" ]; then
  echo "qualification binary is missing: $BINARY" >&2
  exit 1
fi

cleanup() {
  systemctl --user kill --signal=SIGTERM "$UNIT" >/dev/null 2>&1 || true
  for _ in $(seq 1 60); do
    state="$(systemctl --user show "$UNIT" --property=ActiveState --value 2>/dev/null || true)"
    [ "$state" = "inactive" ] || [ "$state" = "failed" ] || { sleep 0.25; continue; }
    break
  done
  journalctl --user --unit "$UNIT" --no-pager > "$SERVER_LOG" 2>&1 || true
}
trap cleanup EXIT INT TERM

systemd-run --user \
  --unit "$UNIT" \
  --property=MemoryMax=8G \
  --property=MemorySwapMax=0 \
  --property=TasksMax=512 \
  --setenv=TMPDIR="$QUALIFICATION_TMPDIR" \
  --same-dir \
  "$BINARY" start \
    -D "$DATABASE" \
    --host 127.0.0.1 \
    --port "$QUALIFICATION_PORT" \
    --auth disabled \
    --log-format json >/dev/null

BASE_URL="http://127.0.0.1:$QUALIFICATION_PORT"
ready=0
for _ in $(seq 1 240); do
  if curl --fail --silent --show-error --max-time 1 "$BASE_URL/system/health" > "$QUALIFICATION_ROOT/startup-health.json" 2>/dev/null; then
    status="$(node -e 'let value=JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")); process.stdout.write(value.status || "")' "$QUALIFICATION_ROOT/startup-health.json")"
    if [ "$status" = "healthy" ]; then
      ready=1
      break
    fi
  fi
  sleep 0.25
done
if [ "$ready" != "1" ]; then
  echo "server did not become healthy; evidence retained at $QUALIFICATION_ROOT" >&2
  exit 1
fi

timeout "$QUALIFICATION_TIMEOUT_SECS" node scripts/qualify-v4-gc-load.mjs \
  --base-url "$BASE_URL" \
  --duration-secs "$QUALIFICATION_DURATION_SECS" \
  --report "$LOAD_REPORT"

CONTROL_GROUP="$(systemctl --user show "$UNIT" --property=ControlGroup --value)"
CGROUP_ROOT="/sys/fs/cgroup$CONTROL_GROUP"
MEMORY_PEAK="$(<"$CGROUP_ROOT/memory.peak")"
MEMORY_SWAP_PEAK="$(<"$CGROUP_ROOT/memory.swap.peak")"
MEMORY_MAX="$(<"$CGROUP_ROOT/memory.max")"
MEMORY_SWAP_MAX="$(<"$CGROUP_ROOT/memory.swap.max")"

cat > "$RESOURCE_REPORT" <<EOF
{
  "schema": "aeordb-v4-p4-8e-resource-v1",
  "memory_peak_bytes": $MEMORY_PEAK,
  "memory_swap_peak_bytes": $MEMORY_SWAP_PEAK,
  "memory_max_bytes": $MEMORY_MAX,
  "memory_swap_max_bytes": $MEMORY_SWAP_MAX
}
EOF

if [ "$MEMORY_MAX" != "8589934592" ] || [ "$MEMORY_SWAP_MAX" != "0" ]; then
  echo "cgroup did not apply the required 8 GiB/no-swap limits" >&2
  exit 1
fi
if [ "$MEMORY_PEAK" -gt 8589934592 ] || [ "$MEMORY_SWAP_PEAK" != "0" ]; then
  echo "qualification exceeded its resource contract" >&2
  exit 1
fi

systemctl --user kill --signal=SIGTERM "$UNIT"
for _ in $(seq 1 120); do
  state="$(systemctl --user show "$UNIT" --property=ActiveState --value 2>/dev/null || true)"
  [ "$state" = "inactive" ] || [ "$state" = "failed" ] || { sleep 0.25; continue; }
  break
done
state="$(systemctl --user show "$UNIT" --property=ActiveState --value 2>/dev/null || true)"
if [ "$state" != "inactive" ]; then
  echo "server did not shut down cleanly (state=$state)" >&2
  exit 1
fi

timeout "$QUALIFICATION_TIMEOUT_SECS" env TMPDIR="$QUALIFICATION_TMPDIR" "$BINARY" verify -D "$DATABASE" > "$VERIFY_LOG" 2>&1
grep -q '^Status: OK$' "$VERIFY_LOG"

cleanup
trap - EXIT INT TERM

EXPANSION_REQUESTS="$(awk '/KV layout change requested from StorageEngine/ { count++ } END { print count + 0 }' "$SERVER_LOG")"
EXPANSION_COMPLETIONS="$(awk '/Online KV block expansion complete/ { count++ } END { print count + 0 }' "$SERVER_LOG")"
if [ "$EXPANSION_REQUESTS" -lt 1 ] || [ "$EXPANSION_REQUESTS" != "$EXPANSION_COMPLETIONS" ]; then
  echo "qualification did not complete every requested online KV expansion (requested=$EXPANSION_REQUESTS completed=$EXPANSION_COMPLETIONS)" >&2
  exit 1
fi

if grep -Eq '\"level\":\"ERROR\"|panicked at|thread .* panicked|corrupt' "$SERVER_LOG"; then
  echo "unexpected error-level server log; evidence retained at $SERVER_LOG" >&2
  exit 1
fi

echo "P4-8e live qualification passed"
echo "  evidence: $QUALIFICATION_ROOT"
echo "  memory peak: $MEMORY_PEAK bytes"
echo "  swap peak: $MEMORY_SWAP_PEAK bytes"
echo "  KV expansions: $EXPANSION_COMPLETIONS"
