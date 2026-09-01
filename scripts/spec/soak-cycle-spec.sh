#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
helper="$repo_root/scripts/lib/soak-cycle.sh"

fail() {
  printf 'soak-cycle spec failed: %s\n' "$*" >&2
  exit 1
}

[[ -r "$helper" ]] || fail "helper is missing or unreadable: $helper"
# The helper path is resolved from the repository root.
# shellcheck disable=SC1090,SC1091
source "$helper"

fixture=$(mktemp -d /tmp/codex/soak-cycle-spec.XXXXXX)
worker_pid=""
cleanup() {
  if [[ -n "$worker_pid" ]] && kill -0 "$worker_pid" 2>/dev/null; then
    command kill -TERM "$worker_pid" 2>/dev/null || true
    wait "$worker_pid" 2>/dev/null || true
  fi
  rm -rf "$fixture"
}
trap cleanup EXIT

status=0
wait_for_worker_window "" 1 "invalid worker" >"$fixture/invalid-pid.out" 2>&1 || status=$?
[[ "$status" = "2" ]] || fail "an invalid process ID did not return status 2"
rg -q --fixed-strings 'requires a positive numeric process ID' "$fixture/invalid-pid.out" \
  || fail "an invalid process ID did not produce a useful diagnostic"

status=0
wait_for_worker_window 1 0 "invalid window" >"$fixture/invalid-window.out" 2>&1 || status=$?
[[ "$status" = "2" ]] || fail "an invalid scheduled window did not return status 2"
rg -q --fixed-strings 'requires a positive scheduled window' "$fixture/invalid-window.out" \
  || fail "an invalid scheduled window did not produce a useful diagnostic"

( exit 7 ) &
worker_pid=$!
started=$SECONDS
if wait_for_worker_window "$worker_pid" 5 "test worker" >"$fixture/early.out" 2>&1; then
  fail "an early worker exit unexpectedly satisfied the scheduled window"
fi
elapsed=$((SECONDS - started))
[[ "$elapsed" -lt 3 ]] || fail "early worker exit was not detected promptly (${elapsed}s)"
rg -q --fixed-strings 'test worker exited before its scheduled window completed (status=7)' "$fixture/early.out" \
  || fail "early worker status was not preserved in the diagnostic"
worker_pid=""

( exit 0 ) &
worker_pid=$!
if wait_for_worker_window "$worker_pid" 5 "successful early worker" >"$fixture/early-success.out" 2>&1; then
  fail "a successful but early worker exit unexpectedly satisfied the scheduled window"
fi
rg -q --fixed-strings 'successful early worker exited before its scheduled window completed (status=0)' "$fixture/early-success.out" \
  || fail "a successful early worker status was not preserved in the diagnostic"
worker_pid=""

sleep 3 &
worker_pid=$!
if ! wait_for_worker_window "$worker_pid" 1 "surviving worker" >"$fixture/surviving.out" 2>&1; then
  fail "a live worker did not satisfy the scheduled window"
fi
kill -0 "$worker_pid" 2>/dev/null || fail "the helper reaped or stopped a worker that survived its window"
command kill -TERM "$worker_pid" 2>/dev/null || true
wait "$worker_pid" 2>/dev/null || true
worker_pid=""

printf 'soak-cycle spec: PASS\n'
