#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
fake_bin="$scratch/fake-bin"
mkdir -p "$fake_bin"
candidate="$scratch/aeordb-candidate"
calls="$scratch/calls.log"

cat > "$candidate" <<'EOF'
#!/usr/bin/env bash
echo 'aeordb test candidate'
EOF
chmod +x "$candidate"
candidate_sha="$(sha256sum "$candidate" | awk '{print $1}')"

cat > "$fake_bin/cargo" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$fake_bin/readelf" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
cat > "$fake_bin/scp" <<EOF
#!/usr/bin/env bash
echo "scp \$*" >> "$calls"
exit 0
EOF
cat > "$fake_bin/ssh" <<EOF
#!/usr/bin/env bash
command="\${!#}"
echo "ssh \$command" >> "$calls"
case "\$command" in
  *"if systemctl is-active --quiet"*) echo 1; exit 0 ;;
  *"aeordb_checked_replacement"*)
    if [ "\${FAKE_GATE_FAIL:-0}" = 1 ]; then exit 3; fi
    echo '{"decision":{"allowed":true}}'
    exit 0
    ;;
  *"sudo install -o root -g root -m 0755"*)
    if [ "\${FAKE_INSTALL_FAIL:-0}" = 1 ]; then exit 1; fi
    exit 0
    ;;
  *"curl -sS -m 5"*) printf '{"status":"healthy"}\nHTTP=200\n'; exit 0 ;;
  *"sha256sum '/opt/aeordb/bin/aeordb'"*) echo "$candidate_sha"; exit 0 ;;
esac
exit 0
EOF
chmod +x "$fake_bin/cargo" "$fake_bin/readelf" "$fake_bin/scp" "$fake_bin/ssh"

run_deploy() {
  PATH="$fake_bin:$PATH" \
  HOST=test-host \
  LOCAL_BIN="$candidate" \
  INSTALL_LOCAL=0 \
  DEBUGGABLE_RELEASE=0 \
  STARTUP_WAIT_SECONDS=2 \
  STOP_WAIT_SECONDS=2 \
  "$repo_root/scripts/deploy-fs-server1.sh"
}

cd "$repo_root"
if ! run_deploy > "$scratch/success.out" 2>&1; then
  cat "$scratch/success.out" >&2
  cat "$calls" >&2
  exit 1
fi
grep -q 'Deploy complete' "$scratch/success.out"
grep -q 'aeordb_checked_replacement' "$calls"

: > "$calls"
if FAKE_GATE_FAIL=1 run_deploy > "$scratch/refused.out" 2>&1; then
  echo "expected deployment gate refusal" >&2
  exit 1
fi
grep -q 'existing binary and unit were not replaced' "$scratch/refused.out"
grep -q "systemctl start 'aeordb'" "$calls"
if grep -q "install -o root -g root -m 0755" "$calls"; then
  echo "refused deploy reached binary installation" >&2
  exit 1
fi

: > "$calls"
if FAKE_INSTALL_FAIL=1 run_deploy > "$scratch/install-failed.out" 2>&1; then
  echo "expected remote installation failure" >&2
  exit 1
fi
grep -q 'restoring the previous binary/unit before restarting' "$scratch/install-failed.out"
grep -q "aeordb.bak." "$calls"
grep -q "aeordb.service.bak." "$calls"
grep -q "systemctl start 'aeordb'" "$calls"

echo "FS-Server1 deploy safety specs passed"
