# Issues from aeordb-client team

## Build Error: `SyncConfig.cluster_secret` removed but still referenced

**Date:** 2026-04-17
**Severity:** Blocker (can't build)

After pulling the latest commit (`f2f123b5`) which fixed the `--auth false` bypass for sync endpoints, the codebase fails to compile:

```
error[E0609]: no field `cluster_secret` on type `SyncConfig`
```

It looks like `cluster_secret` was removed from `SyncConfig` as part of the auth fix, but something still references it. We can't rebuild the aeordb binary until this is resolved.

**To reproduce:**
```bash
cd /home/wyatt/Projects/aeordb-workspace/aeordb
cargo build --release
```

**Impact:** We're blocked on E2E testing of the client's new sync flow. The client code compiles fine (it uses aeordb as a library dependency), but the standalone server binary doesn't build, so we can't start a test server.
