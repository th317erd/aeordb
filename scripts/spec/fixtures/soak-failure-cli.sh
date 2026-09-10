#!/usr/bin/env bash
set -euo pipefail
printf '%s %s\n' "$1" "${4:-}" >> "$AEORDB_SOAK_FAILURE_OPERATIONS"
if test "$1" = probe; then
  if test "$4" = --growth-stats; then
    printf 'normal recovery changed the diagnostic copy\n' >> "$3"
    case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-reopen) exit 7 ;; esac
    printf 'normal startup completed\n' > "$3.reopened"
    exit 0
  fi
  case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-checkpoint) exit 1 ;; *) exit 0 ;; esac
fi
test "$1" = verify
if test ! -f "$3.reopened"; then
  printf 'read-only verification refuses dirty startup\n'
  exit 2
fi
case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-malformed) printf 'invalid report\n'; exit 1 ;; esac
for label in 'Corrupt hash' 'Corrupt header' 'Stale entries' 'Missing entries' \
  'Missing children' 'Dangling records' 'B-tree issues' 'Unlisted files' \
  'Broken snapshots' 'Invalid offsets' 'Invalid voids'; do
  value=0
  if [[ "$AEORDB_SOAK_FAILURE_SCENARIO" = *-verify ]] && test "$label" = 'Corrupt hash'; then value=1; fi
  printf '  %s: %s\n' "$label" "$value"
done
case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-verify) exit 2 ;; *) exit 0 ;; esac
