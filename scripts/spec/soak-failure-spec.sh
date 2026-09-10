#!/usr/bin/env bash
set -euo pipefail

repository=$(cd "$(dirname "$0")/../.." && pwd)
deadline_command=$(command -v timeout || command -v gtimeout)
mkdir -p /tmp/codex
fixture=$(mktemp -d /tmp/codex/aeordb-soak-failure.XXXXXX)
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/scripts/lib" "$fixture/target/release" "$fixture/bin"
cp "$repository/scripts/soak.sh" "$fixture/scripts/soak.sh"
cp "$repository/scripts/lib/soak-cycle.sh" "$fixture/scripts/lib/soak-cycle.sh"
cp "$repository/scripts/spec/fixtures/soak-failure-worker.sh" "$fixture/target/release/soak-worker"
cp "$repository/scripts/spec/fixtures/soak-failure-worker.sh" "$fixture/target/release/crash-soak-worker"
cp "$repository/scripts/spec/fixtures/soak-failure-cli.sh" "$fixture/target/release/aeordb"
ln -s /usr/bin/true "$fixture/bin/cargo"
chmod +x "$fixture/target/release/soak-worker" "$fixture/target/release/crash-soak-worker" "$fixture/target/release/aeordb"
export PATH="$fixture/bin:$PATH"
export AEORDB_SOAK_HOURS=1 AEORDB_SOAK_DURATION_SECS=4
export AEORDB_SOAK_S2_KILL_MIN_SECS=1 AEORDB_SOAK_S2_KILL_MAX_SECS=1
export AEORDB_SOAK_S3_KILL_MIN_SECS=1 AEORDB_SOAK_S3_KILL_MAX_SECS=1
export AEORDB_SOAK_S3_STARTUP_TIMEOUT_SECS=5
failures=0
for scenario in s1-failure s1-pass s2-verify s2-malformed s2-early s3-verify s3-checkpoint s3-early s2-pass s3-pass; do
  run_directory="$fixture/$scenario"
  mkdir -p "$run_directory/source" "$run_directory/scratch"
  export AEORDB_SOAK_FAILURE_SCENARIO="$scenario"
  export AEORDB_SOAK_FAILURE_STARTS="$run_directory/starts"
  export AEORDB_SOAK_DB="$run_directory/soak.aeordb"
  export AEORDB_SOAK_SOURCE="$run_directory/source"
  export AEORDB_SOAK_SCRATCH="$run_directory/scratch"
  set +e
  "$deadline_command" --signal=TERM --kill-after=2s 20s bash "$fixture/scripts/soak.sh" "${scenario%%-*}" > "$run_directory/result.log" 2>&1
  result=$?
  set -e
  starts=$(wc -l < "$run_directory/starts")
  case "$scenario" in
    s1-failure)
      if test "$result" -ne 7 || test "$starts" -ne 1; then
        printf 'FAIL %s: exit=%s starts=%s; worker failure was not propagated\n' "$scenario" "$result" "$starts"
        failures=$((failures + 1))
      else
        printf 'PASS %s: worker failure status is preserved\n' "$scenario"
      fi
      ;;
    s1-pass)
      if test "$result" -ne 0 || test "$starts" -ne 1; then
        printf 'FAIL %s: exit=%s starts=%s\n' "$scenario" "$result" "$starts"
        failures=$((failures + 1))
      else
        printf 'PASS %s: completed worker succeeds\n' "$scenario"
      fi
      ;;
    *-pass)
      if test "$result" -ne 0 || test "$starts" -lt 2; then
        printf 'FAIL %s: exit=%s starts=%s\n' "$scenario" "$result" "$starts"
        failures=$((failures + 1))
      else
        printf 'PASS %s: successful cycles continue\n' "$scenario"
      fi
      ;;
    *)
      if test "$result" -ne 1 || test "$starts" -ne 1; then
        printf 'FAIL %s: exit=%s starts=%s; failed database was reopened\n' "$scenario" "$result" "$starts"
        failures=$((failures + 1))
      else
        printf 'PASS %s: failure stops before a second worker\n' "$scenario"
      fi
      ;;
  esac
done
test "$failures" -eq 0
