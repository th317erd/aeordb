#!/usr/bin/env bash
# 12-hour soak runner for AeorDB.
#
# Usage:
#   ./scripts/soak.sh s1                      # steady-state, no chaos
#   ./scripts/soak.sh s2                      # crash injection during sustained load
#   ./scripts/soak.sh summarize <metrics.tsv>
#
# Environment:
#   AEORDB_SOAK_DB         (default: /media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/soak.aeordb)
#   AEORDB_SOAK_SOURCE     (default: /media/Data/Remote/Seafile/wyatt-desktop/)
#   AEORDB_SOAK_HOURS      (default: 12)
#   AEORDB_SOAK_KILL_MIN   (default: 5)   only used by s2; minutes between SIGKILLs (random N..M)
#   AEORDB_SOAK_KILL_MAX   (default: 15)
#
# Outputs land beside the DB file: <db>.checkpoint.tsv, <db>.metrics.tsv.

set -uo pipefail

MODE="${1:-}"
DB="${AEORDB_SOAK_DB:-/media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/soak.aeordb}"
SOURCE="${AEORDB_SOAK_SOURCE:-/media/Data/Remote/Seafile/wyatt-desktop/}"
HOURS="${AEORDB_SOAK_HOURS:-12}"
KILL_MIN="${AEORDB_SOAK_KILL_MIN:-5}"
KILL_MAX="${AEORDB_SOAK_KILL_MAX:-15}"

cd "$(dirname "$0")/.."

if [ "$MODE" != "summarize" ]; then
  echo "Building release worker..."
  cargo build --release --bin soak-worker >/dev/null || { echo "build failed"; exit 1; }
fi

WORKER="$(pwd)/target/release/soak-worker"
LOG_DIR="$(dirname "$DB")"
WORKER_LOG="$LOG_DIR/soak.worker.log"
PMAP_LOG="$LOG_DIR/soak.pmap.log"
PMAP_INTERVAL_SECS=1800   # every 30 minutes

# Pick the right address-space dump tool for the host OS.
#   Linux:  pmap -x       (pages + permissions + RSS columns)
#   macOS:  vmmap         (regions + sizes; no -x flag exists)
# Both write a roughly-equivalent VMA listing to stdout.
case "$(uname -s)" in
  Darwin) PMAP_BIN="vmmap"; PMAP_ARGS=() ;;
  *)      PMAP_BIN="pmap";  PMAP_ARGS=("-x") ;;
esac

# Spawn a background loop that takes an address-space snapshot of $1
# every $PMAP_INTERVAL_SECS, writing to $PMAP_LOG with a timestamp header.
# Returns the loop's PID so the caller can kill it on shutdown.
start_pmap_recorder() {
  local target_pid="$1"
  # The bg subshell must NOT inherit stdout — if it does, the surrounding
  # `pmap_pid=$(start_pmap_recorder ...)` command substitution will block
  # forever waiting for the pipe to EOF, because the bg subshell keeps
  # the pipe's write end open for the duration of its sleep loop. Redirect
  # the subshell's stdout to /dev/null and stderr to the pmap log itself.
  (
    local slept=0
    while kill -0 "$target_pid" 2>/dev/null; do
      if [ "$slept" -ge "$PMAP_INTERVAL_SECS" ] || [ "$slept" -eq 0 ]; then
        {
          echo "===== $(date -Iseconds) $PMAP_BIN pid=$target_pid ====="
          "$PMAP_BIN" "${PMAP_ARGS[@]}" "$target_pid" 2>/dev/null || echo "($PMAP_BIN failed)"
          echo
        } >> "$PMAP_LOG"
        slept=0
      fi
      sleep 2  # short slices so we notice worker exit quickly
      slept=$((slept + 2))
    done
  ) </dev/null >/dev/null 2>&1 &
  echo $!
}

case "$MODE" in
  s1)
    mkdir -p "$LOG_DIR"
    : > "$PMAP_LOG"
    echo "== S1 steady-state soak =="
    echo "  database:    $DB"
    echo "  source:      $SOURCE"
    echo "  duration:    ${HOURS}h"
    echo "  worker log:  $WORKER_LOG"
    echo "  pmap log:    $PMAP_LOG  (every ${PMAP_INTERVAL_SECS}s)"
    echo
    # Spawn the worker as a background process so $! is its actual PID
    # (piping through tee would give us tee's PID, not the worker's). For a
    # 12-hour soak you'd `tail -f $WORKER_LOG` from another terminal anyway.
    "$WORKER" \
      --database "$DB" \
      --source-dir "$SOURCE" \
      --duration-hours "$HOURS" > "$WORKER_LOG" 2>&1 &
    worker_pid=$!
    echo "  worker pid:  $worker_pid"
    echo "  tail with:   tail -f $WORKER_LOG"
    sleep 2  # let the worker start before snapshotting its address space
    pmap_pid=$(start_pmap_recorder "$worker_pid")
    trap "kill $worker_pid $pmap_pid 2>/dev/null" EXIT INT TERM
    wait "$worker_pid"
    kill "$pmap_pid" 2>/dev/null
    wait "$pmap_pid" 2>/dev/null
    trap - EXIT INT TERM
    echo
    echo "S1 complete."
    echo "  Run: $0 summarize ${DB}.metrics.tsv"
    ;;

  s2)
    mkdir -p "$LOG_DIR"
    : > "$PMAP_LOG"
    echo "== S2 crash-injection soak =="
    echo "  database:    $DB"
    echo "  source:      $SOURCE"
    echo "  duration:    ${HOURS}h"
    echo "  kill window: random ${KILL_MIN}..${KILL_MAX} min between SIGKILLs"
    echo "  worker log:  $WORKER_LOG"
    echo "  pmap log:    $PMAP_LOG  (every ${PMAP_INTERVAL_SECS}s)"
    echo

    end_epoch=$(( $(date +%s) + HOURS * 3600 ))
    iteration=0

    while [ "$(date +%s)" -lt "$end_epoch" ]; do
      iteration=$((iteration + 1))

      # Random sleep in [KILL_MIN, KILL_MAX] minutes, in seconds.
      kill_after_secs=$(( ( RANDOM % ((KILL_MAX - KILL_MIN + 1) * 60) ) + KILL_MIN * 60 ))
      remaining=$(( end_epoch - $(date +%s) ))
      slot=$(( kill_after_secs < remaining ? kill_after_secs : remaining ))
      [ "$slot" -le 0 ] && break

      slot_hours=$(awk -v s="$slot" 'BEGIN { printf "%.4f", s/3600 }')
      echo "[$(date +%T)] iteration $iteration: spawning worker for ${slot}s (${slot_hours}h)"

      # Spawn worker with a deliberately too-large duration; we'll SIGKILL it.
      "$WORKER" \
        --database "$DB" \
        --source-dir "$SOURCE" \
        --duration-hours "$HOURS" >> "$WORKER_LOG" 2>&1 &
      worker_pid=$!
      sleep 2  # let the worker initialize before snapshotting
      pmap_pid=$(start_pmap_recorder "$worker_pid")

      # Sleep, then SIGKILL.
      remaining_slot=$(( slot - 2 ))
      [ "$remaining_slot" -gt 0 ] && sleep "$remaining_slot"
      if kill -0 "$worker_pid" 2>/dev/null; then
        echo "[$(date +%T)] iteration $iteration: SIGKILL pid=$worker_pid"
        kill -KILL "$worker_pid" 2>/dev/null
        wait "$worker_pid" 2>/dev/null
      else
        echo "[$(date +%T)] iteration $iteration: worker already exited"
      fi
      kill "$pmap_pid" 2>/dev/null
      wait "$pmap_pid" 2>/dev/null

      # Quick verify: try to open the database and read N random committed
      # paths. The repair-aware open path inside aeordb handles the dirty
      # startup; we just need a smoke test that it works.
      #
      # `aeordb verify` exits non-zero on any has_issues() — including
      # `corrupt_header > 0`. For S2's expected behavior, a SIGKILL that
      # lands mid-write produces exactly that: one (or rarely two) partial
      # entry headers at the tail, which the entry scanner skip-and-resumes
      # past. So we don't treat corrupt_header / skipped_region as a soak
      # failure here — they're the textbook crash artifact. The signals we
      # DO treat as failure (the ones the engine should be preventing):
      #   - corrupt_hash > 0       (hash mismatch on a recovered entry)
      #   - stale_kv_entries > 0   (KV pointing past dirty-rebuild)
      #   - missing_kv_entries > 0 (entries lost from the KV)
      #   - missing_children > 0   (directory tree forgot a child)
      #   - unlisted_files > 0     (file exists but parent doesn't list it)
      #   - broken_snapshots > 0   (snapshot root unreachable)
      verify_log="$(mktemp)"
      ./target/release/aeordb verify -D "$DB" > "$verify_log" 2>&1
      # Parse the report. `awk` prints the numeric value in each line.
      get_field() { awk -v label="$1" '$0 ~ "^  " label ":" { print $NF; exit }' "$verify_log"; }
      corrupt_hash=$(get_field "Corrupt hash")
      corrupt_header=$(get_field "Corrupt header")
      stale=$(get_field "Stale entries")
      missing_kv=$(get_field "Missing entries")
      missing_children=$(get_field "Missing children")
      unlisted_files=$(get_field "Unlisted files")
      broken_snapshots=$(get_field "Broken snapshots")
      if [ "${corrupt_hash:-0}" = "0" ] && [ "${stale:-0}" = "0" ] \
        && [ "${missing_kv:-0}" = "0" ] && [ "${missing_children:-0}" = "0" ] \
        && [ "${unlisted_files:-0}" = "0" ] && [ "${broken_snapshots:-0}" = "0" ]; then
        echo "[$(date +%T)] iteration $iteration: verify OK (corrupt_header=${corrupt_header:-0} — expected SIGKILL tail)"
        rm -f "$verify_log"
      else
        echo "[$(date +%T)] iteration $iteration: verify reported real issues — see $verify_log"
        echo "  corrupt_hash=${corrupt_hash:-?} stale=${stale:-?} missing_kv=${missing_kv:-?} \
missing_children=${missing_children:-?} unlisted=${unlisted_files:-?} broken_snapshots=${broken_snapshots:-?}"
        echo "  (continuing soak; collect failures at the end)"
      fi
    done

    echo
    echo "S2 complete. $iteration kill cycles executed."
    echo "  Run: $0 summarize ${DB}.metrics.tsv"
    ;;

  summarize)
    METRICS="${2:-${DB}.metrics.tsv}"
    if [ ! -f "$METRICS" ]; then
      echo "metrics file not found: $METRICS"
      exit 1
    fi
    "$WORKER" --summarize "$METRICS"
    ;;

  *)
    echo "Usage: $0 {s1|s2|summarize [metrics.tsv]}"
    exit 2
    ;;
esac
