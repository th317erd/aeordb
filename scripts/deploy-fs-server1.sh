#!/usr/bin/env bash
set -euo pipefail

HOST="${HOST:-FS-Server1}"
SERVICE="${SERVICE:-aeordb}"
REMOTE_BIN="${REMOTE_BIN:-/opt/aeordb/bin/aeordb}"
REMOTE_TMP="${REMOTE_TMP:-/tmp/aeordb-new}"
REMOTE_UNIT="${REMOTE_UNIT:-/etc/systemd/system/aeordb.service}"
REMOTE_DATABASE="${REMOTE_DATABASE:-/mnt/storage/aeordb/files.taraani.org.aeordb}"
REMOTE_RUN_USER="${REMOTE_RUN_USER:-aeordb}"
REMOTE_RUN_HOME="${REMOTE_RUN_HOME:-/opt/aeordb/home}"
REMOTE_EMERGENCY_SPILL_DIR="${REMOTE_EMERGENCY_SPILL_DIR:-}"
LOCAL_BIN="${LOCAL_BIN:-target/release/aeordb}"
LOCAL_UNIT="${LOCAL_UNIT:-deploy/systemd/aeordb.service}"
LOCAL_SAFETY="${LOCAL_SAFETY:-scripts/lib/deployment-safety.sh}"
HEALTH_URL="${HEALTH_URL:-http://127.0.0.1:6830/system/health}"
CARGO_JOBS="${CARGO_JOBS:-4}"
STARTUP_WAIT_SECONDS="${STARTUP_WAIT_SECONDS:-1800}"
STOP_WAIT_SECONDS="${STOP_WAIT_SECONDS:-2100}"
INSTALL_LOCAL="${INSTALL_LOCAL:-1}"
LOCAL_INSTALL_BIN="${LOCAL_INSTALL_BIN:-$HOME/.local/bin/aeordb}"
DEBUGGABLE_RELEASE="${DEBUGGABLE_RELEASE:-1}"
SSH_CONNECT_TIMEOUT="${SSH_CONNECT_TIMEOUT:-10}"
SSH_SERVER_ALIVE_INTERVAL="${SSH_SERVER_ALIVE_INTERVAL:-15}"
SSH_SERVER_ALIVE_COUNT_MAX="${SSH_SERVER_ALIVE_COUNT_MAX:-4}"
SSH_OPTS=(
  -o BatchMode=yes
  -o ConnectTimeout="$SSH_CONNECT_TIMEOUT"
  -o ServerAliveInterval="$SSH_SERVER_ALIVE_INTERVAL"
  -o ServerAliveCountMax="$SSH_SERVER_ALIVE_COUNT_MAX"
)
SCP_OPTS=("${SSH_OPTS[@]}")

case "$DEBUGGABLE_RELEASE" in
  1|true|yes)
    case " ${RUSTFLAGS:-} " in
      *" -C force-frame-pointers=yes "*) ;;
      *) export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C force-frame-pointers=yes" ;;
    esac
    ;;
  0|false|no) ;;
  *)
    echo "Invalid DEBUGGABLE_RELEASE value: $DEBUGGABLE_RELEASE"
    exit 2
    ;;
esac

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
log_dir="deploy/logs"
mkdir -p "$log_dir"
log_file="$log_dir/fs-server1-deploy-$timestamp.log"
exec > >(tee -a "$log_file") 2>&1

echo "== AeorDB deploy to $HOST =="
echo "timestamp=$timestamp"
echo "service=$SERVICE"
echo "remote_bin=$REMOTE_BIN"
echo "remote_database=$REMOTE_DATABASE"
echo "health_url=$HEALTH_URL"
echo "stop_wait_seconds=$STOP_WAIT_SECONDS"
echo "install_local=$INSTALL_LOCAL"
echo "local_install_bin=$LOCAL_INSTALL_BIN"
echo "debuggable_release=$DEBUGGABLE_RELEASE"
echo "rustflags=${RUSTFLAGS:-}"
echo "ssh_connect_timeout=$SSH_CONNECT_TIMEOUT"
echo "ssh_server_alive_interval=$SSH_SERVER_ALIVE_INTERVAL"
echo "ssh_server_alive_count_max=$SSH_SERVER_ALIVE_COUNT_MAX"
echo "log_file=$log_file"
echo

echo "== Build release binary =="
cargo build --release -p aeordb-cli --bin aeordb -j "$CARGO_JOBS"
local_sha="$(sha256sum "$LOCAL_BIN" | awk '{print $1}')"
echo "local_sha256=$local_sha"
file "$LOCAL_BIN"
if command -v readelf >/dev/null 2>&1; then
  readelf -n "$LOCAL_BIN" | awk '/Build ID/ {print "build_id="$3; found=1; exit} END {if (!found) print "build_id="}'
  debug_sections="$(readelf -S "$LOCAL_BIN" | awk '/\.debug_/ {count++} END {print count + 0}')"
  echo "debug_sections=$debug_sections"
fi
echo

echo "== Remote preflight =="
ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
  echo host=\$(hostname)
  echo time=\$(date -Is)
  systemctl is-active '$SERVICE' || true
  systemctl show -p MainPID -p ActiveState -p SubState '$SERVICE' || true
  curl -sS -m 3 -w '\nHTTP=%{http_code}\n' '$HEALTH_URL' || true
"
echo

service_was_active="$(ssh "${SSH_OPTS[@]}" "$HOST" "if systemctl is-active --quiet '$SERVICE'; then echo 1; else echo 0; fi")"
echo "service_was_active=$service_was_active"

remote_tmp_bin="$REMOTE_TMP.$timestamp"
remote_tmp_unit="/tmp/aeordb.service.$timestamp"
remote_tmp_safety="/tmp/aeordb-deployment-safety.$timestamp.sh"

echo "== Copy artifacts =="
scp -q "${SCP_OPTS[@]}" "$LOCAL_BIN" "$HOST:$remote_tmp_bin"
scp -q "${SCP_OPTS[@]}" "$LOCAL_UNIT" "$HOST:$remote_tmp_unit"
scp -q "${SCP_OPTS[@]}" "$LOCAL_SAFETY" "$HOST:$remote_tmp_safety"
echo "copied_binary=$remote_tmp_bin"
echo "copied_unit=$remote_tmp_unit"
echo "copied_safety_helper=$remote_tmp_safety"
echo

echo "== Stop service cleanly =="
set +e
timeout "$STOP_WAIT_SECONDS" ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
  if systemctl is-active --quiet '$SERVICE'; then
    sudo systemctl stop '$SERVICE'
  fi
  systemctl is-active '$SERVICE' || true
  systemctl show -p ActiveState -p SubState -p Result '$SERVICE' || true
"
stop_status=$?
set -e
if [ "$stop_status" -eq 124 ]; then
  echo "Timed out waiting ${STOP_WAIT_SECONDS}s for '$SERVICE' to stop on $HOST."
  echo "The binary was built locally and copied to $HOST:$remote_tmp_bin, but was not installed."
  echo "Leaving deployment incomplete so an operator can inspect the stuck service."
  exit 1
fi
if [ "$stop_status" -ne 0 ]; then
  echo "Failed to stop '$SERVICE' on $HOST; ssh/systemctl exit status: $stop_status"
  exit "$stop_status"
fi
echo

echo "== Checked deployment compatibility =="
if ! ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
  sudo -u '$REMOTE_RUN_USER' env \
    HOME='$REMOTE_RUN_HOME' \
    AEORDB_EMERGENCY_SPILL_DIR='$REMOTE_EMERGENCY_SPILL_DIR' \
    bash -c 'set -euo pipefail; source \"\$1\"; aeordb_checked_replacement \"\$2\" \"\$3\" \"\$4\"' \
    aeordb-deployment-check '$remote_tmp_safety' '$REMOTE_BIN' '$remote_tmp_bin' '$REMOTE_DATABASE'
"; then
  echo "Deployment compatibility check failed; the existing binary and unit were not replaced."
  ssh "${SSH_OPTS[@]}" "$HOST" "sudo rm -f '$remote_tmp_bin' '$remote_tmp_unit' '$remote_tmp_safety'; if [ '$service_was_active' = 1 ]; then sudo systemctl start '$SERVICE'; fi" || true
  exit 1
fi
echo

echo "== Install unit and binary =="
if ! ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
  if [ -f '$REMOTE_UNIT' ]; then
    sudo cp -a '$REMOTE_UNIT' '$REMOTE_UNIT.bak.$timestamp'
    echo unit_backup='$REMOTE_UNIT.bak.$timestamp'
  fi
  sudo install -o root -g root -m 0644 '$remote_tmp_unit' '$REMOTE_UNIT'
  sudo rm -f '$remote_tmp_unit'
  sudo systemctl daemon-reload
  sudo install -d -o root -g root -m 0755 \"\$(dirname '$REMOTE_BIN')\"
  if [ -x '$REMOTE_BIN' ]; then
    sudo cp -a '$REMOTE_BIN' '$REMOTE_BIN.bak.$timestamp'
    echo backup='$REMOTE_BIN.bak.$timestamp'
  fi
  sudo install -o root -g root -m 0755 '$remote_tmp_bin' '$REMOTE_BIN'
  sudo rm -f '$remote_tmp_bin' '$remote_tmp_safety'
  sha256sum '$REMOTE_BIN'
"; then
  echo "Remote install failed; restoring the previous binary/unit before restarting."
  ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
    if [ -f '$REMOTE_BIN.bak.$timestamp' ]; then sudo cp -a '$REMOTE_BIN.bak.$timestamp' '$REMOTE_BIN'; fi
    if [ -f '$REMOTE_UNIT.bak.$timestamp' ]; then sudo cp -a '$REMOTE_UNIT.bak.$timestamp' '$REMOTE_UNIT'; fi
    sudo rm -f '$remote_tmp_bin' '$remote_tmp_unit' '$remote_tmp_safety'
    sudo systemctl daemon-reload
    if [ '$service_was_active' = 1 ]; then sudo systemctl start '$SERVICE'; fi
  " || true
  exit 1
fi
echo

echo "== Start service =="
ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
  sudo systemctl start '$SERVICE'
  systemctl show -p MainPID -p ActiveState -p SubState '$SERVICE'
"
echo

echo "== Wait for health =="
deadline=$((SECONDS + STARTUP_WAIT_SECONDS))
ready=0
while [ "$SECONDS" -lt "$deadline" ]; do
  output="$(ssh "${SSH_OPTS[@]}" "$HOST" "curl -sS -m 5 -w '\nHTTP=%{http_code}\n' '$HEALTH_URL' 2>&1" || true)"
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
ssh "${SSH_OPTS[@]}" "$HOST" "set -euo pipefail
  systemctl status '$SERVICE' --no-pager || true
  echo
  journalctl -u '$SERVICE' --since '15 minutes ago' --no-pager | tail -n 120 || true
"

if [ "$ready" -ne 1 ]; then
  echo "Deploy completed binary install/start, but health did not become healthy within ${STARTUP_WAIT_SECONDS}s."
  echo "Check $log_file for details."
  exit 1
fi

remote_sha="$(ssh "${SSH_OPTS[@]}" "$HOST" "sha256sum '$REMOTE_BIN' | awk '{print \$1}'")"
if [ "$remote_sha" != "$local_sha" ]; then
  echo "Remote SHA mismatch: local=$local_sha remote=$remote_sha"
  exit 1
fi

case "$INSTALL_LOCAL" in
  1|true|yes)
    echo "== Install local binary =="
    AEORDB_INSTALL_BIN_DIR="$(dirname "$LOCAL_INSTALL_BIN")" scripts/install-local.sh --from "$LOCAL_BIN"
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

echo
echo "Deploy complete."
echo "sha256=$remote_sha"
echo "log_file=$log_file"
