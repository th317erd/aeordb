#!/usr/bin/env bash
set -euo pipefail
for argument in "$@"; do
  case "$AEORDB_SOAK_FAILURE_SCENARIO:$argument" in
    s2-copy:*/diagnostics.*/verify.aeordb|s3-copy:*/diagnostics.*/verify.aeordb|\
    s3-copy-probe:*/diagnostics.*/probe.aeordb|s3-copy-checkpoint:*/diagnostics.*/checkpoint.tsv)
      printf 'simulated diagnostic copy failure\n' >&2
      exit 5
      ;;
  esac
done
exec /bin/cp "$@"
