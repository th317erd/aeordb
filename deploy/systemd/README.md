# systemd unit

`aeordb.service` is the systemd unit used to run AeorDB as a long-lived
service.

## Install

Assumes the binary lives at `/opt/aeordb/bin/aeordb` and a dedicated
service user `aeordb` exists with home at `/opt/aeordb/home`. Adjust the
unit if your layout differs.

```bash
# 1. Create the service user (no login shell, dedicated home).
sudo useradd --system --home-dir /opt/aeordb/home --shell /usr/sbin/nologin aeordb
sudo install -d -o aeordb -g aeordb -m 0750 /opt/aeordb/home

# 2. Drop the binary into place.
sudo install -d -o root -g root -m 0755 /opt/aeordb/bin
sudo install -o root -g root -m 0755 ./target/release/aeordb /opt/aeordb/bin/aeordb

# 3. Make sure the database directory is writable by the service user.
#    (Path is whatever you set in ExecStart's --database flag.)
sudo install -d -o aeordb -g aeordb -m 0750 /mnt/storage/aeordb

# 4. Install the unit and enable it.
sudo install -o root -g root -m 0644 deploy/systemd/aeordb.service /etc/systemd/system/aeordb.service
sudo systemctl daemon-reload
sudo systemctl enable --now aeordb
sudo systemctl status aeordb
```

## Deploying FS-Server1

Use the checked-in deploy helper instead of hand-running the individual
`scp`/`systemctl` commands:

```bash
scripts/deploy-fs-server1.sh
```

The script:

- builds `aeordb` with `cargo build --release -p aeordb-cli --bin aeordb -j 6`
- installs that release binary locally to `~/.local/bin/aeordb` and verifies the hash
- copies the binary and this systemd unit to `FS-Server1`
- installs the unit and reloads systemd before stopping the service
- stops AeorDB with the unit's long shutdown timeout
- backs up the previous remote binary as `/opt/aeordb/bin/aeordb.bak.<timestamp>`
- installs the new binary, starts the service, and polls `/system/health`
- writes a local deploy log under `deploy/logs/`

The log directory is intentionally ignored by git. It may include operational
hostnames, health output, and journal excerpts.

## Startup Health

The CLI binds HTTP before opening the storage engine. During a clean open,
dirty startup, or WAL/KV recovery, `GET /system/health` returns a JSON body
with `status: "starting"` instead of leaving callers with a connection
refusal or proxy `502`. Other routes return `503 Service Unavailable` until
the engine has opened and the full application router is ready.

Startup health includes:

- `progress` — overall startup progress as a fraction from `0.0` to `1.0`
- `eta` — `null` when unknown, or `{ "seconds": N, "at": "<rfc3339>" }`
- `phase` — the active startup phase, such as `opening_engine` or `rebuild_kv_scan`
- `message` — human-readable phase detail

Example:

```json
{
  "status": "starting",
  "phase": "rebuild_kv_scan",
  "message": "Scanning WAL entries for dirty startup recovery",
  "version": "0.9.5",
  "progress": 0.42,
  "eta": { "seconds": 480, "at": "2026-06-12T03:05:00Z" }
}
```

During dirty startup recovery, AeorDB rebuilds the KV/index view from WAL
headers and metadata records. Large chunk payloads are not reread during this
startup scan; they remain verified on normal content reads and by explicit
verification tooling. This keeps production recovery bounded by entry metadata
instead of forcing every large media payload through startup.

## Shutdown Behavior

On SIGTERM, AeorDB stops accepting new top-level storage operations, lets
existing reads/writes drain, then flushes indexes, KV buffers, hot buffers, and
the WAL. The default engine drain wait is 600 seconds and can be overridden
with:

```bash
AEORDB_SHUTDOWN_OPERATION_WAIT_SECS=1800
```

If operations remain active after the wait, AeorDB logs the operation names and
counts before stopping. That stop is intentionally not reported as graceful.
The bundled unit uses a long `TimeoutStopSec` so systemd does not escalate to
SIGKILL while AeorDB is still draining active work.

## Customizing the unit

The unit shipped here points at `/mnt/storage/aeordb/files.taraani.org.aeordb`
(the production database on the FS-Server1 host). Before deploying
elsewhere, edit:

- `ExecStart` — `--database` path, `--host` / `--port`, any extra flags
- `ReadWritePaths` — the directory holding the DB file and any hot-dir
- `Description` — a friendly name for your deployment

## Control

```bash
sudo systemctl start aeordb
sudo systemctl stop aeordb
sudo systemctl restart aeordb
sudo journalctl -u aeordb -f      # follow logs
```
