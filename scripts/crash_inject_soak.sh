#!/usr/bin/env bash
# Crash-injection soak runner.
#
# Wraps the cargo test invocation in a loop so you can run many cycles
# overnight without writing your own watchdog. Each pass runs the full
# crash_inject_spec suite (SIGKILL during writes, SIGKILL during mixed
# workload, bit flip detection, trailing truncation).
#
# Usage:
#   ./scripts/crash_inject_soak.sh [iterations] [sleep_secs_between]
#
# Examples:
#   ./scripts/crash_inject_soak.sh              # 100 passes, no sleep
#   ./scripts/crash_inject_soak.sh 1000 10      # 1000 passes, 10s between
#
# Optional tmpfs setup for the umount-f variant (requires sudo, untested):
#   sudo mount -t tmpfs -o size=512m tmpfs /tmp/aeordb-crash-fs
#   sudo chown $USER /tmp/aeordb-crash-fs
#   AEORDB_CRASH_SOAK_TMPFS=/tmp/aeordb-crash-fs ./scripts/crash_inject_soak.sh

set -euo pipefail

iterations="${1:-100}"
sleep_between="${2:-0}"

cd "$(dirname "$0")/.."

echo "Building release binaries..."
cargo build --release --bin crash-soak-worker >/dev/null
cargo test --release --test crash_inject_spec --no-run >/dev/null

start_time=$(date +%s)
echo "Soak start: $(date)"
echo "Iterations: ${iterations}, sleep between: ${sleep_between}s"
echo

for ((i=1; i<=iterations; i++)); do
  echo "--- pass ${i}/${iterations} (elapsed $((($(date +%s) - start_time)))s) ---"
  if ! cargo test --release --test crash_inject_spec -- --ignored --nocapture; then
    echo "FAIL on pass ${i}. Halting."
    exit 1
  fi
  if [ "${sleep_between}" -gt 0 ] && [ "${i}" -lt "${iterations}" ]; then
    sleep "${sleep_between}"
  fi
done

echo
echo "Soak complete: $(date)"
echo "Total elapsed: $((($(date +%s) - start_time)))s across ${iterations} passes."
