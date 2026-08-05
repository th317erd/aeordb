#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=../lib/deployment-safety.sh
source "$repo_root/scripts/lib/deployment-safety.sh"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
log="$scratch/calls.log"
database="$scratch/test.aeordb"
: > "$database"

make_fake() {
  local path="$1"
  local capability="$2"
  local check_status="$3"
  cat > "$path" <<EOF
#!/usr/bin/env bash
echo "\$(basename "$path") \$*" >> "$log"
if [ "\${1:-}" = deployment-capabilities ]; then
  [ "$capability" = yes ] && exit 0
  [ "$capability" = hang ] && sleep 5
  exit 2
fi
if [ "\${1:-}" = deployment-check ]; then
  echo '{"decision":{"allowed":$([ "$check_status" = 0 ] && echo true || echo false)}}'
  exit "$check_status"
fi
if [ "\${1:-}" = --version ]; then
  echo 'aeordb 0.0-test'
  exit 0
fi
exit 2
EOF
  chmod +x "$path"
}

old="$scratch/old-aeordb"
new="$scratch/new-aeordb"
active="$scratch/active-aeordb"
inactive="$scratch/inactive-aeordb"
broken="$scratch/broken-aeordb"
hung="$scratch/hung-aeordb"
make_fake "$old" no 2
make_fake "$new" yes 0
make_fake "$active" yes 3
make_fake "$inactive" yes 0
make_fake "$broken" yes 1
make_fake "$hung" hang 0

AEORDB_DEPLOYMENT_PROBE_TIMEOUT_SECONDS=1
export AEORDB_DEPLOYMENT_PROBE_TIMEOUT_SECONDS

# First upgrade: the installed old binary cannot inspect, so the compatible
# candidate performs the read-only check.
aeordb_checked_replacement "$old" "$new" "$database"
grep -q 'new-aeordb deployment-check' "$log"

# A compatible candidate is allowed even while the current inspector reports
# active transition state.
aeordb_checked_replacement "$active" "$new" "$database"

# An old candidate is refused while active, but allowed after the current
# compatible binary proves the database inactive.
if aeordb_checked_replacement "$active" "$old" "$database"; then
  echo "expected active downgrade refusal" >&2
  exit 1
else
  [ "$?" -eq 3 ]
fi
aeordb_checked_replacement "$inactive" "$old" "$database"

# No compatible inspector means safety cannot be proven.
if aeordb_checked_replacement "$old" "$old" "$database"; then
  echo "expected missing-inspector refusal" >&2
  exit 1
fi

# Inspector failures and hung capability probes fail closed.
if aeordb_checked_replacement "$broken" "$old" "$database"; then
  echo "expected inspector failure" >&2
  exit 1
fi
if aeordb_checked_replacement "$inactive" "$hung" "$database"; then
  echo "expected hung candidate probe failure" >&2
  exit 1
fi

echo "deployment safety shell specs passed"
