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
