#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
database="$scratch/test.aeordb"
: > "$database"

make_fake() {
  local path="$1"
  local capability="$2"
  local check_status="$3"
  local version="$4"
  cat > "$path" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  deployment-capabilities) [ "$capability" = yes ] && exit 0; exit 2 ;;
  deployment-check) echo '{"decision":{"allowed":$([ "$check_status" = 0 ] && echo true || echo false)}}'; exit "$check_status" ;;
  --version) echo 'aeordb $version'; exit 0 ;;
esac
exit 2
EOF
  chmod +x "$path"
}

bin_dir="$scratch/bin"
mkdir -p "$bin_dir"
compatible="$scratch/compatible"
old="$scratch/old"
active_current="$bin_dir/aeordb"
make_fake "$compatible" yes 0 compatible

AEORDB_INSTALL_BIN_DIR="$bin_dir" "$repo_root/scripts/install-local.sh" --from "$compatible" --database "$database"
"$bin_dir/aeordb" --version | grep -q compatible

make_fake "$active_current" yes 3 active-current
make_fake "$old" no 2 old
before="$(sha256sum "$active_current" | awk '{print $1}')"
if AEORDB_INSTALL_BIN_DIR="$bin_dir" "$repo_root/scripts/install-local.sh" --from "$old" --database "$database"; then
  echo "expected active downgrade refusal" >&2
  exit 1
fi
after="$(sha256sum "$active_current" | awk '{print $1}')"
[ "$before" = "$after" ]
"$active_current" --version | grep -q active-current

echo "install-local checked replacement specs passed"
