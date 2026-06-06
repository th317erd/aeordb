#!/usr/bin/env bash
# Build and install the AeorDB CLI into the current user's local bin dir.
#
# Default:
#   ./scripts/install-local.sh
#
# Install an already-built binary:
#   ./scripts/install-local.sh --from /tmp/codex/aeordb-release-20260605/aeordb
#
# Environment:
#   AEORDB_INSTALL_BIN_DIR   install directory (default: $HOME/.local/bin)

set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/install-local.sh [--from PATH] [--bin-dir DIR]

Builds target/release/aeordb and installs it to ~/.local/bin/aeordb by
default. If --from is supplied, installs that binary instead of building.

Options:
  --from PATH       Install an already-built aeordb binary.
  --bin-dir DIR     Install directory. Defaults to AEORDB_INSTALL_BIN_DIR or ~/.local/bin.
  -h, --help        Show this help.
EOF
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin_dir="${AEORDB_INSTALL_BIN_DIR:-$HOME/.local/bin}"
source_binary=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --from)
      if [ "$#" -lt 2 ]; then
        echo "error: --from requires a path" >&2
        exit 2
      fi
      source_binary="$2"
      shift 2
      ;;
    --bin-dir)
      if [ "$#" -lt 2 ]; then
        echo "error: --bin-dir requires a directory" >&2
        exit 2
      fi
      bin_dir="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$source_binary" ]; then
  cd "$repo_root"
  echo "Building AeorDB release binary..."
  cargo build --release -p aeordb-cli --bin aeordb
  source_binary="$repo_root/target/release/aeordb"
fi

if [ ! -f "$source_binary" ]; then
  echo "error: binary not found: $source_binary" >&2
  exit 1
fi

if [ ! -x "$source_binary" ]; then
  echo "error: binary is not executable: $source_binary" >&2
  exit 1
fi

mkdir -p "$bin_dir"
install -m 0755 "$source_binary" "$bin_dir/aeordb"

installed="$bin_dir/aeordb"
echo "Installed: $installed"
"$installed" --version

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *)
    echo "warning: $bin_dir is not currently on PATH" >&2
    echo "         add it to your shell profile or run: export PATH=\"$bin_dir:\$PATH\"" >&2
    ;;
esac
