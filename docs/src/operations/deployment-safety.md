# Deployment Safety

AeorDB persists durability-recovery authority inside the database and may also
leave external emergency-spill artifacts. A binary that predates that state
cannot safely decide that a database is writable. Checked replacement prevents
an old binary from silently bypassing a durability latch, pending spill replay,
or incomplete repair.

## Capability Protocol

Compatible binaries advertise this capability:

```text
aeordb.v3-transition-recovery.v1
```

Inspect a binary without opening a database:

```bash
aeordb deployment-capabilities
aeordb deployment-capabilities --json
aeordb deployment-capabilities \
  --require aeordb.v3-transition-recovery.v1
```

`--require` exits with status `0` when supported and `3` when the capability is
known but unsupported. An older CLI normally rejects the unknown command with
status `2`. Probe timeouts or other failures must be treated as inspection
failures, not as evidence that a downgrade is safe.

## Read-Only Database Check

```bash
aeordb deployment-check \
  --database /var/lib/aeordb/data.aeordb \
  --candidate-capability aeordb.v3-transition-recovery.v1 \
  --json
```

The command reads the selected v3 header, bounded hot-tail and KV metadata,
canonical durability controls, and unapplied external spill manifests. It does
not open `StorageEngine`, perform recovery, or modify a database byte.
Malformed, truncated, ambiguous, or unsupported state fails closed.

The JSON response contains:

```json
{
  "state": {
    "database_header_version": 3,
    "persistent_recovery": null,
    "external_spill_count": 0,
    "requires_transition_capability": false,
    "reasons": []
  },
  "decision": {
    "allowed": true,
    "candidate_supports_transition_recovery": true,
    "message": "candidate understands v3 transition recovery authority"
  }
}
```

The check exits with status `0` when replacement is allowed, `3` when policy
refuses it, and `1` when state could not be inspected safely.

When the candidate lacks the capability, the command also takes the database's
exclusive `.lock`. Stop every process using the database before attempting an
old-binary replacement. A compatible candidate can inspect the state while the
current server is running, but deployment scripts should still stop the server
before replacing its executable.

## Replacement Policy

The checked installer follows this matrix:

| Candidate | Database state | Result |
|-----------|----------------|--------|
| Advertises the recovery capability | Valid v3 state | Allowed; the candidate understands the authority |
| Does not advertise the capability | No active latch, unapplied spill, or incomplete repair | Allowed after exclusive-lock inspection |
| Does not advertise the capability | Active latch, unapplied spill, or incomplete repair | Refused |
| Any candidate | Corrupt, ambiguous, unsupported, or unreadable inspection state | Refused |

For a first upgrade from a binary too old to inspect transition state, the
compatible candidate performs the read-only inspection. For a downgrade, the
installed compatible binary performs it on behalf of the old candidate.

## Local Installation

Pass every local database that the installed binary may open:

```bash
./scripts/install-local.sh \
  --from ./target/release/aeordb \
  --database "$HOME/data/primary.aeordb" \
  --database "$HOME/data/archive.aeordb"
```

`AEORDB_INSTALL_DATABASE` supplies one database through the environment. When
no database is supplied, the installer emits a warning because it cannot infer
all databases on the machine. That warning is not a clean-state proof.

## Production Deployment

`scripts/deploy-fs-server1.sh` uses the same checked-replacement helper as the
local installer. It:

1. Builds and copies the candidate without replacing the installed binary.
2. Records whether the service was active, then stops it cleanly.
3. Runs the read-only replacement gate as the service user.
4. On refusal, removes temporary artifacts and restarts the untouched service.
5. On install failure, restores the previous binary and unit before restart.
6. Starts the candidate, waits for healthy status, verifies its hash, and only
   then installs that binary locally.

Once a candidate has been installed and started, an unhealthy startup is not
automatically downgraded. The candidate may already have published newer state;
an operator must inspect the failure before selecting a rollback binary.

The production helper recognizes these overrides:

| Variable | Default |
|----------|---------|
| `REMOTE_DATABASE` | `/mnt/storage/aeordb/files.taraani.org.aeordb` |
| `REMOTE_RUN_USER` | `aeordb` |
| `REMOTE_RUN_HOME` | `/opt/aeordb/home` |
| `REMOTE_EMERGENCY_SPILL_DIR` | unset |
| `AEORDB_DEPLOYMENT_PROBE_TIMEOUT_SECONDS` | `15` |
| `AEORDB_DEPLOYMENT_CHECK_TIMEOUT_SECONDS` | `120` |

The spill-directory override must match the service's configuration so the
gate sees every configured external artifact location.
