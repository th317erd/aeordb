#!/usr/bin/env bash
set -euo pipefail
if test "$1" = probe; then
  case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-checkpoint) exit 1 ;; *) exit 0 ;; esac
fi
test "$1" = verify
case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-malformed) printf 'invalid report\n'; exit 1 ;; esac
for label in 'Corrupt hash' 'Corrupt header' 'Stale entries' 'Missing entries' \
  'Missing children' 'Dangling records' 'B-tree issues' 'Unlisted files' \
  'Broken snapshots' 'Invalid offsets' 'Invalid voids'; do
  value=0
  if [[ "$AEORDB_SOAK_FAILURE_SCENARIO" = *-verify ]] && test "$label" = 'Corrupt hash'; then value=1; fi
  printf '  %s: %s\n' "$label" "$value"
done
case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-verify) exit 1 ;; *) exit 0 ;; esac
