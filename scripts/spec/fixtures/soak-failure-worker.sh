#!/usr/bin/env bash
set -euo pipefail
printf 'started\n' >> "$AEORDB_SOAK_FAILURE_STARTS"
printf 'disposable simulated database\n' > "$AEORDB_SOAK_DB"
printf '# worker up mode=stress\n' >> "$AEORDB_SOAK_DB.crash.checkpoint.tsv"
case "$AEORDB_SOAK_FAILURE_SCENARIO" in *-early) exit 7 ;; esac
exec sleep 30
