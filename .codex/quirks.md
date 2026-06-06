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
