#!/usr/bin/env bash
# 12-hour soak runner for AeorDB.
#
# Usage:
#   ./scripts/soak.sh s1                      # steady-state, no chaos
#   ./scripts/soak.sh s2                      # crash injection during sustained load
#   ./scripts/soak.sh s3                      # aggressive tiny JSON + merge crash stress
#   ./scripts/soak.sh s3 /tmp/aeordb-soak     # use /tmp/aeordb-soak/soak.aeordb
#   ./scripts/soak.sh s3 /tmp/soak.aeordb     # use an explicit DB path
#   ./scripts/soak.sh summarize <metrics.tsv>
#
# Environment:
#   AEORDB_SOAK_DB         (default: /media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/soak.aeordb)
#   AEORDB_SOAK_SOURCE     (default: /media/Data/Remote/Seafile/wyatt-desktop/)
#   AEORDB_SOAK_HOURS      (default: 12)
#   AEORDB_SOAK_DURATION_SECS (optional; overrides HOURS for s2/s3 loop duration)
#   AEORDB_SOAK_KILL_MIN   (default: 5)   only used by s2; minutes between SIGKILLs (random N..M)
#   AEORDB_SOAK_KILL_MAX   (default: 15)
#   AEORDB_SOAK_S2_KILL_MIN_SECS (optional) only used by s2; overrides minute window
#   AEORDB_SOAK_S2_KILL_MAX_SECS (optional)
#   AEORDB_SOAK_S3_KILL_MIN_SECS (default: 5)  only used by s3; seconds between SIGKILLs
#   AEORDB_SOAK_S3_KILL_MAX_SECS (default: 30)
#   AEORDB_SOAK_S3_STARTUP_TIMEOUT_SECS (default: 120) initial durable-ready wait
#   AEORDB_SOAK_SCRATCH    (default: ~/.cache/codex/aeordb-tests/soak-scratch)
#   CARGO_BUILD_JOBS       (default: 4)
#
# Outputs land beside the DB file: <db>.checkpoint.tsv, <db>.metrics.tsv.

set -uo pipefail

MODE="${1:-}"
DEFAULT_DB="/media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/soak.aeordb"
DB="${AEORDB_SOAK_DB:-$DEFAULT_DB}"
if [ "$MODE" != "summarize" ] && [ -n "${2:-}" ]; then
  if [[ "$2" == *.aeordb ]]; then
    DB="$2"
  else
    DB="$2/soak.aeordb"
  fi
fi
SOURCE="${AEORDB_SOAK_SOURCE:-/media/Data/Remote/Seafile/wyatt-desktop/}"
HOURS="${AEORDB_SOAK_HOURS:-12}"
LOOP_DURATION_SECS="${AEORDB_SOAK_DURATION_SECS:-$(( HOURS * 3600 ))}"
KILL_MIN="${AEORDB_SOAK_KILL_MIN:-5}"
KILL_MAX="${AEORDB_SOAK_KILL_MAX:-15}"
S2_KILL_MIN_SECS="${AEORDB_SOAK_S2_KILL_MIN_SECS:-}"
S2_KILL_MAX_SECS="${AEORDB_SOAK_S2_KILL_MAX_SECS:-}"
S3_KILL_MIN_SECS="${AEORDB_SOAK_S3_KILL_MIN_SECS:-5}"
S3_KILL_MAX_SECS="${AEORDB_SOAK_S3_KILL_MAX_SECS:-30}"
S3_STARTUP_TIMEOUT_SECS="${AEORDB_SOAK_S3_STARTUP_TIMEOUT_SECS:-120}"
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
SCRATCH_ROOT="${AEORDB_SOAK_SCRATCH:-${XDG_CACHE_HOME:-$HOME/.cache}/codex/aeordb-tests/soak-scratch}"
SOAK_FAILURES=0
mkdir -p "$SCRATCH_ROOT"

cd "$(dirname "$0")/.."

if [ "$MODE" != "summarize" ]; then
  case "$MODE" in
    s1|s2)
      echo "Building release soak worker and CLI..."
      cargo build -j "$CARGO_BUILD_JOBS" --release --bin soak-worker --bin aeordb >/dev/null || { echo "build failed"; exit 1; }
      ;;
    s3)
      echo "Building release crash worker and CLI..."
      cargo build -j "$CARGO_BUILD_JOBS" --release --bin crash-soak-worker --bin aeordb >/dev/null || { echo "build failed"; exit 1; }
      ;;
  esac
fi

WORKER="$(pwd)/target/release/soak-worker"
CRASH_WORKER="$(pwd)/target/release/crash-soak-worker"
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

copy_db_for_diagnostic() {
  local src="$1"
  local dest="$2"
  if cp -a --reflink=auto "$src" "$dest" 2>/dev/null; then
    return 0
  fi
  cp -a "$src" "$dest"
}

finish_chaos_soak() {
  local mode="$1"
  local iterations="$2"
  echo
  if [ "$SOAK_FAILURES" -gt 0 ]; then
    echo "$mode FAILED. $SOAK_FAILURES of $iterations kill cycles reported retained diagnostic failures."
    return 1
  fi
  echo "$mode complete. $iterations kill cycles executed."
}

count_report_lines() {
  local prefix="$1"
  awk -v prefix="$prefix" 'index($0, prefix) == 1 { count++ } END { print count + 0 }' "$verify_log"
}

verify_report_is_acceptable() {
  [ -n "$corrupt_hash" ] && [ -n "$corrupt_header" ] && [ -n "$stale" ] \
    && [ -n "$missing_kv" ] && [ -n "$missing_children" ] && [ -n "$dangling_records" ] \
    && [ -n "$btree_issues" ] && [ -n "$unlisted_files" ] && [ -n "$broken_snapshots" ] \
    && [ -n "$invalid_offsets" ] && [ -n "$invalid_voids" ] \
    && [ "$corrupt_hash" = "0" ] && [ "$stale" = "0" ] && [ "$missing_kv" = "0" ] \
    && [ "$missing_children" = "0" ] && [ "$dangling_records" = "0" ] && [ "$btree_issues" = "0" ] \
    && [ "$unlisted_files" = "0" ] && [ "$broken_snapshots" = "0" ] \
    && [ "$invalid_offsets" = "0" ] && [ "$invalid_voids" = "0" ] \
    && [ "$verification_errors" = "0" ] && [ "$stale_dir_keys" = "0" ] || return 1

  if [ "$verify_status" = "0" ]; then
    return 0
  fi
  [ "$corrupt_header" -gt 0 ]
}

wait_for_s3_startup_checkpoint() {
  local target_pid="$1"
  local prior_markers="$2"
  local deadline=$(( $(date +%s) + S3_STARTUP_TIMEOUT_SECS ))
  local current_markers

  while true; do
    current_markers=$(grep -c '^# worker up mode=stress$' "$CHECKPOINT" 2>/dev/null || true)
    current_markers=${current_markers:-0}
    if [ "$current_markers" -gt "$prior_markers" ]; then
      echo "[$(date +%T)] initial worker published its durable startup checkpoint"
      return 0
    fi
    if ! kill -0 "$target_pid" 2>/dev/null; then
      wait "$target_pid" 2>/dev/null
      local status=$?
      echo "S3 worker exited with status $status before publishing its initial durable startup checkpoint" >&2
      return 1
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "S3 worker did not publish its initial durable startup checkpoint within ${S3_STARTUP_TIMEOUT_SECS}s" >&2
      return 1
    fi
    sleep 1
  done
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
    echo "  duration:    ${LOOP_DURATION_SECS}s loop window (${HOURS}h worker duration)"
    if [ -n "$S2_KILL_MIN_SECS" ]; then
      if [ -z "$S2_KILL_MAX_SECS" ] || [ "$S2_KILL_MAX_SECS" -lt "$S2_KILL_MIN_SECS" ]; then
        S2_KILL_MAX_SECS="$S2_KILL_MIN_SECS"
      fi
      echo "  kill window: random ${S2_KILL_MIN_SECS}..${S2_KILL_MAX_SECS} sec between SIGKILLs"
    else
      echo "  kill window: random ${KILL_MIN}..${KILL_MAX} min between SIGKILLs"
    fi
    echo "  worker log:  $WORKER_LOG"
    echo "  pmap log:    $PMAP_LOG  (every ${PMAP_INTERVAL_SECS}s)"
    echo

    end_epoch=$(( $(date +%s) + LOOP_DURATION_SECS ))
    iteration=0

    while [ "$(date +%s)" -lt "$end_epoch" ]; do
      iteration=$((iteration + 1))

      if [ -n "$S2_KILL_MIN_SECS" ]; then
        kill_after_secs=$(( ( RANDOM % (S2_KILL_MAX_SECS - S2_KILL_MIN_SECS + 1) ) + S2_KILL_MIN_SECS ))
      else
        # Random sleep in [KILL_MIN, KILL_MAX] minutes, in seconds.
        kill_after_secs=$(( ( RANDOM % ((KILL_MAX - KILL_MIN + 1) * 60) ) + KILL_MIN * 60 ))
      fi
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
      verify_log="$(mktemp -p "$SCRATCH_ROOT" verify.XXXXXX)"
      verify_status=0
      ./target/release/aeordb verify -D "$DB" > "$verify_log" 2>&1 || verify_status=$?
      # Parse the report. `awk` prints the numeric value in each line.
      get_field() { awk -v label="$1" '$0 ~ "^  " label ":" { print $NF; exit }' "$verify_log"; }
      corrupt_hash=$(get_field "Corrupt hash")
      corrupt_header=$(get_field "Corrupt header")
      stale=$(get_field "Stale entries")
      missing_kv=$(get_field "Missing entries")
      missing_children=$(get_field "Missing children")
      dangling_records=$(get_field "Dangling records")
      btree_issues=$(get_field "B-tree issues")
      unlisted_files=$(get_field "Unlisted files")
      broken_snapshots=$(get_field "Broken snapshots")
      invalid_offsets=$(get_field "Invalid offsets")
      invalid_voids=$(get_field "Invalid voids")
      verification_errors=$(count_report_lines "  Verification error:")
      stale_dir_keys=$(count_report_lines "Stale dir_key entries (")
      if verify_report_is_acceptable; then
        echo "[$(date +%T)] iteration $iteration: verify OK (status=$verify_status, corrupt_header=$corrupt_header — expected SIGKILL tail)"
        rm -f "$verify_log"
      else
        echo "[$(date +%T)] iteration $iteration: verify reported real issues — see $verify_log"
        echo "  status=$verify_status corrupt_hash=${corrupt_hash:-?} corrupt_header=${corrupt_header:-?} stale=${stale:-?} \
missing_kv=${missing_kv:-?} missing_children=${missing_children:-?} dangling=${dangling_records:-?} btree=${btree_issues:-?} \
unlisted=${unlisted_files:-?} broken_snapshots=${broken_snapshots:-?} invalid_offsets=${invalid_offsets:-?} \
invalid_voids=${invalid_voids:-?} verification_errors=${verification_errors:-?} stale_dir_keys=${stale_dir_keys:-?}"
        echo "  (continuing soak; collect failures at the end)"
        SOAK_FAILURES=$((SOAK_FAILURES + 1))
      fi
    done

    if ! finish_chaos_soak "S2" "$iteration"; then
      exit 1
    fi
    echo "  Run: $0 summarize ${DB}.metrics.tsv"
    ;;

  s3)
    mkdir -p "$LOG_DIR"
    : > "$PMAP_LOG"
    CHECKPOINT="${AEORDB_SOAK_CHECKPOINT:-${DB}.crash.checkpoint.tsv}"
    if ! [[ "$S3_STARTUP_TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]]; then
      echo "AEORDB_SOAK_S3_STARTUP_TIMEOUT_SECS must be a positive integer" >&2
      exit 2
    fi
    if [ "$S3_KILL_MAX_SECS" -lt "$S3_KILL_MIN_SECS" ]; then
      S3_KILL_MAX_SECS="$S3_KILL_MIN_SECS"
    fi
    echo "== S3 aggressive crash stress =="
    echo "  database:    $DB"
    echo "  duration:    ${LOOP_DURATION_SECS}s"
    echo "  workload:    crash-soak-worker --mode stress"
    echo "  kill window: random ${S3_KILL_MIN_SECS}..${S3_KILL_MAX_SECS} sec between SIGKILLs"
    echo "  checkpoint:  $CHECKPOINT"
    echo "  worker log:  $WORKER_LOG"
    echo "  pmap log:    $PMAP_LOG  (every ${PMAP_INTERVAL_SECS}s)"
    echo

    end_epoch=$(( $(date +%s) + LOOP_DURATION_SECS ))
    iteration=0

    while [ "$(date +%s)" -lt "$end_epoch" ]; do
      iteration=$((iteration + 1))

      kill_after_secs=$(( ( RANDOM % (S3_KILL_MAX_SECS - S3_KILL_MIN_SECS + 1) ) + S3_KILL_MIN_SECS ))
      remaining=$(( end_epoch - $(date +%s) ))
      slot=$(( kill_after_secs < remaining ? kill_after_secs : remaining ))
      [ "$slot" -le 0 ] && break

      echo "[$(date +%T)] iteration $iteration: spawning stress worker for ${slot}s"
      startup_markers_before=0
      if [ "$iteration" -eq 1 ]; then
        startup_markers_before=$(grep -c '^# worker up mode=stress$' "$CHECKPOINT" 2>/dev/null || true)
        startup_markers_before=${startup_markers_before:-0}
      fi
      "$CRASH_WORKER" \
        --database "$DB" \
        --checkpoint "$CHECKPOINT" \
        --mode stress >> "$WORKER_LOG" 2>&1 &
      worker_pid=$!
      if [ "$iteration" -eq 1 ] && ! wait_for_s3_startup_checkpoint "$worker_pid" "$startup_markers_before"; then
        if kill -0 "$worker_pid" 2>/dev/null; then
          kill -TERM "$worker_pid" 2>/dev/null
        fi
        wait "$worker_pid" 2>/dev/null
        exit 1
      fi
      sleep 1
      pmap_pid=$(start_pmap_recorder "$worker_pid")

      remaining_slot=$(( slot - 1 ))
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

      diag_dir="$(mktemp -d -p "$SCRATCH_ROOT" diagnostics.XXXXXX)"
      verify_db="$diag_dir/verify.aeordb"
      probe_db="$diag_dir/probe.aeordb"
      checkpoint_copy="$diag_dir/checkpoint.tsv"
      copy_db_for_diagnostic "$DB" "$verify_db"
      copy_db_for_diagnostic "$DB" "$probe_db"
      cp -a "$CHECKPOINT" "$checkpoint_copy"
      diag_ok=1

      verify_log="$diag_dir/verify.log"
      verify_status=0
      ./target/release/aeordb verify -D "$verify_db" > "$verify_log" 2>&1 || verify_status=$?
      get_field() { awk -v label="$1" '$0 ~ "^  " label ":" { print $NF; exit }' "$verify_log"; }
      corrupt_hash=$(get_field "Corrupt hash")
      corrupt_header=$(get_field "Corrupt header")
      stale=$(get_field "Stale entries")
      missing_kv=$(get_field "Missing entries")
      missing_children=$(get_field "Missing children")
      dangling_records=$(get_field "Dangling records")
      btree_issues=$(get_field "B-tree issues")
      unlisted_files=$(get_field "Unlisted files")
      broken_snapshots=$(get_field "Broken snapshots")
      invalid_offsets=$(get_field "Invalid offsets")
      invalid_voids=$(get_field "Invalid voids")
      verification_errors=$(count_report_lines "  Verification error:")
      stale_dir_keys=$(count_report_lines "Stale dir_key entries (")
      if verify_report_is_acceptable; then
        echo "[$(date +%T)] iteration $iteration: verify OK (status=$verify_status, corrupt_header=$corrupt_header — expected SIGKILL tail)"
      else
        echo "[$(date +%T)] iteration $iteration: verify reported real issues — see $verify_log"
        echo "  status=$verify_status corrupt_hash=${corrupt_hash:-?} corrupt_header=${corrupt_header:-?} stale=${stale:-?} \
missing_kv=${missing_kv:-?} missing_children=${missing_children:-?} dangling=${dangling_records:-?} btree=${btree_issues:-?} \
unlisted=${unlisted_files:-?} broken_snapshots=${broken_snapshots:-?} invalid_offsets=${invalid_offsets:-?} \
invalid_voids=${invalid_voids:-?} verification_errors=${verification_errors:-?} stale_dir_keys=${stale_dir_keys:-?}"
        diag_ok=0
      fi

      probe_log="$diag_dir/probe.log"
      if ./target/release/aeordb probe -D "$probe_db" --diff-checkpoint "$checkpoint_copy" > "$probe_log" 2>&1; then
        echo "[$(date +%T)] iteration $iteration: checkpoint diff OK"
      else
        echo "[$(date +%T)] iteration $iteration: checkpoint diff reported loss — see $probe_log"
        diag_ok=0
      fi

      if [ "$diag_ok" = "1" ]; then
        rm -rf "$diag_dir"
      else
        echo "[$(date +%T)] iteration $iteration: preserved diagnostic copies in $diag_dir"
        SOAK_FAILURES=$((SOAK_FAILURES + 1))
      fi
    done

    if ! finish_chaos_soak "S3" "$iteration"; then
      exit 1
    fi
    echo "  Checkpoint: $CHECKPOINT"
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
    echo "Usage: $0 {s1|s2|s3|summarize [metrics.tsv]}"
    exit 2
    ;;
esac
