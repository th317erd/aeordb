# AeorDB Project Quirks

This file records project-specific preferences for AeorDB work.

---

## Real World Tests For API And SDK Changes
*Added: 2026-06-05*

**Principle**: Significant AeorDB updates, especially API or SDK interface additions/changes, must be verified with a real running AeorDB instance in addition to unit/integration tests.

**Rationale**:
- Unit tests and harness-level HTTP tests are necessary but do not catch every packaging, CLI, routing, startup, auth-mode, or live HTTP behavior issue.
- API and SDK changes are contract changes; they need proof that a real caller can use them through the deployed surface.
- A throwaway `/tmp/codex` database gives realistic coverage without risking user data or polluting the repository.

**Examples**:

| Avoid | Prefer |
|-------|--------|
| Only running `cargo test` after adding an HTTP endpoint | Start AeorDB against `/tmp/codex/.../test.aeordb` and exercise the endpoint with `curl` or a real SDK client |
| Only testing SDK serialization with mocks | Run the SDK against a live local AeorDB server |
| Reporting an API change as done without a live request | Include the exact live-server test scenario and result in the final answer |

**Required Procedure**:
- For significant API/SDK changes, create a fresh database under `/tmp/codex/<task-name>/`.
- Start AeorDB through the normal CLI/server path unless the task specifically targets embedded usage.
- Exercise the changed behavior through real HTTP requests or the real SDK/client.
- Cover at least one success path and relevant failure/edge paths.
- Shut the server down cleanly and report the commands/scenario tested.

**Exceptions**: Skip the live test only when the change cannot reasonably be exercised outside a specific external environment; in that case, say exactly what prevented the live test and what was tested instead.

---

## Treat Production-Like Migration As Development First
*Added: 2026-09-04*

**Principle**: When a large, damaged, or production-derived AeorDB database is
used to exercise repair or migration, the primary objective is to find and
permanently fix AeorDB defects. Recovering that particular database is desirable
but secondary.

**Rationale**:
- The source contents may already be backed up even when database metadata and
  authority state are damaged.
- Real damaged databases are adversarial development fixtures that expose gaps
  synthetic tests did not anticipate.
- A one-off manual recovery can hide a product defect and provides no protection
  for the next database with the same failure.
- Every discovered failure should become a deterministic failing-first regression,
  a systemic code correction, and broader proof before the large operation is
  retried.

**Examples**:

| Avoid | Prefer |
|-------|--------|
| Hand-edit the large database until one migration happens to finish | Reproduce the failure in a bounded fixture, fix AeorDB, then retry the large database |
| Call the effort successful solely because the files were recovered | Require repair, migration, restart, corruption, and resource regressions to prove the product correction |
| Weaken validation to admit damaged state | Preserve fail-closed behavior and add an explicit, tested recovery rule only when evidence justifies it |

**Exceptions**: Continue to preserve original evidence, protect source bytes, and
avoid unnecessary data loss. A development-first objective does not authorize
unapproved destructive production actions or careless handling of recoverable
data.

---

## Budget Multi-Terabyte Migrations For One Shadow
*Added: 2026-09-04*

**Principle**: For an explicitly authorized, development-focused migration of a
multi-terabyte AeorDB whose file contents are independently backed up, do not
create both a full repaired copy and a full migration destination when the host
cannot safely hold both. Repair the source in place, freeze it after successful
verification, and allocate capacity for only one v4 shadow.

**Rationale**:
- A roughly 5 TB repair copy plus a roughly 5 TB migration shadow would consume
  nearly all of an 11 TB free-space budget.
- Low free space is itself a migration hazard and can corrupt the usefulness of
  the rehearsal.
- The primary objective is to expose and fix AeorDB defects; exact recovery of
  this particular database is desirable but secondary.
- Independent content backups make the explicitly accepted in-place repair risk
  materially different from operating on the sole copy of user data.

**Examples**:

| Avoid | Prefer |
|-------|--------|
| Run default `verify --repair` and create a multi-terabyte `.repaired` file before also creating a v4 shadow | Run the reviewed repair with `--force-fix-in-place`, verify it, freeze the source, then create one v4 shadow |
| Consume nearly all pool capacity to maximize recoverability | Preserve a large free-space floor so repair and migration can fail safely |
| Treat a failed large repair as a bespoke data-recovery emergency | Capture evidence, reproduce the defect, fix AeorDB, and retry while recovery remains practical |

**Exceptions**: Use a full repair copy when exact database recovery is the primary
goal, the source contents are not independently recoverable, or separately
provisioned capacity makes the copy safe. In-place repair still requires explicit
authorization, an offline source, evidence capture, bounded monitoring, and no
unreviewed emergency-spill replay.

---
