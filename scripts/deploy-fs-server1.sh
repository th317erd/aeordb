#!/usr/bin/env bash
set -euo pipefail

HOST="${HOST:-FS-Server1}"
SERVICE="${SERVICE:-aeordb}"
REMOTE_BIN="${REMOTE_BIN:-/opt/aeordb/bin/aeordb}"
REMOTE_TMP="${REMOTE_TMP:-/tmp/aeordb-new}"
REMOTE_UNIT="${REMOTE_UNIT:-/etc/systemd/system/aeordb.service}"
LOCAL_BIN="${LOCAL_BIN:-target/release/aeordb}"
LOCAL_UNIT="${LOCAL_UNIT:-deploy/systemd/aeordb.service}"
HEALTH_URL="${HEALTH_URL:-http://127.0.0.1:6830/system/health}"
CARGO_JOBS="${CARGO_JOBS:-6}"
STARTUP_WAIT_SECONDS="${STARTUP_WAIT_SECONDS:-1800}"
INSTALL_LOCAL="${INSTALL_LOCAL:-1}"
LOCAL_INSTALL_BIN="${LOCAL_INSTALL_BIN:-$HOME/.local/bin/aeordb}"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
log_dir="deploy/logs"
mkdir -p "$log_dir"
log_file="$log_dir/fs-server1-deploy-$timestamp.log"
exec > >(tee -a "$log_file") 2>&1

echo "== AeorDB deploy to $HOST =="
echo "timestamp=$timestamp"
echo "service=$SERVICE"
echo "remote_bin=$REMOTE_BIN"
echo "health_url=$HEALTH_URL"
echo "install_local=$INSTALL_LOCAL"
echo "local_install_bin=$LOCAL_INSTALL_BIN"
echo "log_file=$log_file"
echo

echo "== Build release binary =="
cargo build --release -p aeordb-cli --bin aeordb -j "$CARGO_JOBS"
local_sha="$(sha256sum "$LOCAL_BIN" | awk '{print $1}')"
echo "local_sha256=$local_sha"
echo

case "$INSTALL_LOCAL" in
  1|true|yes)
    echo "== Install local binary =="
    install -d -m 0755 "$(dirname "$LOCAL_INSTALL_BIN")"
    install -m 0755 "$LOCAL_BIN" "$LOCAL_INSTALL_BIN"
    "$LOCAL_INSTALL_BIN" --version
    local_install_sha="$(sha256sum "$LOCAL_INSTALL_BIN" | awk '{print $1}')"
    echo "local_install_sha256=$local_install_sha"
    if [ "$local_install_sha" != "$local_sha" ]; then
      echo "Local install SHA mismatch: built=$local_sha installed=$local_install_sha"
      exit 1
    fi
    echo
    ;;
  0|false|no)
    echo "== Install local binary =="
    echo "skipped"
    echo
    ;;
  *)
    echo "Invalid INSTALL_LOCAL value: $INSTALL_LOCAL"
    exit 2
    ;;
esac

echo "== Remote preflight =="
ssh "$HOST" "set -euo pipefail
  echo host=\$(hostname)
  echo time=\$(date -Is)
  systemctl is-active '$SERVICE' || true
  systemctl show -p MainPID -p ActiveState -p SubState '$SERVICE' || true
  curl -sS -m 3 -w '\nHTTP=%{http_code}\n' '$HEALTH_URL' || true
"
echo

remote_tmp_bin="$REMOTE_TMP.$timestamp"
remote_tmp_unit="/tmp/aeordb.service.$timestamp"

echo "== Copy artifacts =="
scp -q "$LOCAL_BIN" "$HOST:$remote_tmp_bin"
scp -q "$LOCAL_UNIT" "$HOST:$remote_tmp_unit"
echo "copied_binary=$remote_tmp_bin"
echo "copied_unit=$remote_tmp_unit"
echo

echo "== Install unit before stop =="
ssh "$HOST" "set -euo pipefail
  sudo install -o root -g root -m 0644 '$remote_tmp_unit' '$REMOTE_UNIT'
  sudo rm -f '$remote_tmp_unit'
  sudo systemctl daemon-reload
  systemctl show -p TimeoutStopUSec '$SERVICE'
"
echo

echo "== Stop service cleanly =="
ssh "$HOST" "set -euo pipefail
  if systemctl is-active --quiet '$SERVICE'; then
    sudo systemctl stop '$SERVICE'
  fi
  systemctl is-active '$SERVICE' || true
  systemctl show -p ActiveState -p SubState -p Result '$SERVICE' || true
"
echo

echo "== Install binary =="
ssh "$HOST" "set -euo pipefail
  sudo install -d -o root -g root -m 0755 \"\$(dirname '$REMOTE_BIN')\"
  if [ -x '$REMOTE_BIN' ]; then
    sudo cp -a '$REMOTE_BIN' '$REMOTE_BIN.bak.$timestamp'
    echo backup='$REMOTE_BIN.bak.$timestamp'
  fi
  sudo install -o root -g root -m 0755 '$remote_tmp_bin' '$REMOTE_BIN'
  sudo rm -f '$remote_tmp_bin'
  sha256sum '$REMOTE_BIN'
"
echo

echo "== Start service =="
ssh "$HOST" "set -euo pipefail
  sudo systemctl start '$SERVICE'
  systemctl show -p MainPID -p ActiveState -p SubState '$SERVICE'
"
echo

echo "== Wait for health =="
deadline=$((SECONDS + STARTUP_WAIT_SECONDS))
ready=0
while [ "$SECONDS" -lt "$deadline" ]; do
  output="$(ssh "$HOST" "curl -sS -m 5 -w '\nHTTP=%{http_code}\n' '$HEALTH_URL' 2>&1" || true)"
  echo "$output"
  http_code="$(printf '%s\n' "$output" | awk -F= '/^HTTP=/{print $2}' | tail -1)"
  if printf '%s\n' "$output" | grep -q '"status":"healthy"'; then
    ready=1
    break
  fi
  if [ "$http_code" = "200" ] && printf '%s\n' "$output" | grep -q '"status":"starting"'; then
    sleep 5
    continue
  fi
  sleep 5
done

echo
echo "== Remote status =="
ssh "$HOST" "set -euo pipefail
  systemctl status '$SERVICE' --no-pager || true
  echo
  journalctl -u '$SERVICE' --since '15 minutes ago' --no-pager | tail -n 120 || true
"

if [ "$ready" -ne 1 ]; then
  echo "Deploy completed binary install/start, but health did not become healthy within ${STARTUP_WAIT_SECONDS}s."
  echo "Check $log_file for details."
  exit 1
fi

remote_sha="$(ssh "$HOST" "sha256sum '$REMOTE_BIN' | awk '{print \$1}'")"
if [ "$remote_sha" != "$local_sha" ]; then
  echo "Remote SHA mismatch: local=$local_sha remote=$remote_sha"
  exit 1
fi

echo
echo "Deploy complete."
echo "sha256=$remote_sha"
echo "log_file=$log_file"
