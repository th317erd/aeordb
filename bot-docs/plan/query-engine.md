# Query Engine — Function-Based Compute-at-Data

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## Core Concept: No Query Language. Functions.

AeorDB does not use SQL or any declarative query language. Instead, queries are **compiled functions** (plugins) that are deployed to the database and invoked by name with arguments over HTTP(S).

The function executes at the data layer — next to the storage and indexes — and only the result travels back to the caller. This eliminates:
- Query parsing overhead (there's nothing to parse)
- Optimizer black-box behavior (you wrote the logic, you control it)
- Network waste (no shipping massive query strings; just arguments)
- SQL injection (there is no SQL to inject into)

---

## How It Works

### 1. Deploy a Function

Write a function using the AeorDB SDK (Rust crate), compile it (native .so or WASM), and deploy it to a path in the database hierarchy:

```bash
# Write your function
cargo new my_query --lib
# Use the aeordb SDK crate to write query logic
# Compile
cargo build --release  # native .so
# or
cargo build --release --target wasm32-unknown-unknown  # WASM

# Deploy to a path
aeordb deploy ./target/release/libmy_query.so --path /mydb/public/users/active_by_region
```

### 2. Invoke a Function

Call it over HTTP(S) with arguments:

```
POST /mydb/public/users/active_by_region
Content-Type: application/json

{"region": "northeast", "since": "2026-01-01"}
```

The function is already loaded, already compiled, already sitting next to the data. The database routes the request, passes the arguments, executes the function, and returns the result.

### 3. Run-Once (Ad-Hoc Queries)

For development, debugging, or one-off queries: deploy and execute in a single call using the standard deploy endpoint with a run-immediately directive:

```
POST /mydb/public/users/_deploy
Content-Type: application/json

{
  "plugin": "<base64-encoded .so or .wasm>",
  "onSuccess": {
    "runWith": {
      "arguments": {"region": "northeast"}
    }
  }
}
```

The function is compiled, loaded, executed with the provided arguments, the result is returned, and the function is discarded. Same machinery as a permanent deploy — no special-case code path.

---

## HTTP(S) Interface

The external interface to AeorDB is HTTP(S). Period.

**Why HTTP(S):**
- Every language, platform, and tool speaks HTTP
- No custom wire protocol, no special driver, no ORM needed
- `curl` is a valid database client — zero-ceremony philosophy
- TLS gives encryption in transit for free
- Load balancers, proxies, and observability tools all work out of the box
- REST semantics are universally understood

**Endpoint structure mirrors the database hierarchy:**

```
POST   /database/schema/table/function_name     → invoke function
PUT    /database/schema/table/_deploy            → deploy function
DELETE /database/schema/table/function_name      → remove function
GET    /database/schema/table/_functions          → list deployed functions
```

---

## Hierarchical Function Scoping

Functions can be defined at any level in the database hierarchy. They are **visible downward** — a function at a higher level can be called by any function at the same level or below.

### Scope Hierarchy

```
/database
  └── function: audit_log()        ← available everywhere in this database
  └── /schema/public
        └── function: validate()   ← available to all tables in public schema
        └── /table/users
              └── function: active_users()    ← can call validate() and audit_log()
              └── /column/email
                    └── function: normalize()  ← can call everything above it
```

### Scoping Rules

1. A function can access any function at its own level or above in the hierarchy
2. A function CANNOT reach sideways to call functions in sibling scopes (e.g., a function on /table/users cannot directly call a function on /table/orders)
3. Cross-scope access goes through the database query interface, which enforces permissions
4. The location of a function defines its default access scope — the hierarchy IS the permissions model

### Triple-Duty Path Structure

The path hierarchy serves three purposes simultaneously:
- **REST route** — how clients invoke functions over HTTP
- **Scope/permissions boundary** — what data and functions a plugin can access
- **Function namespace** — how functions are organized and discovered

This is load-bearing architecture. The path design must be deliberate and well-defined.

---

## Function Composition

Functions can call other functions through two paths:

### Fast Path: Direct Plugin-to-Plugin Calls

When functions are in the same scope or explicitly linked, they can call each other directly through the WASM/native plugin interface. No serialization, no HTTP overhead, no routing — just a function call.

- Lowest possible latency
- Used when composition is known at deploy time
- Functions must be in compatible scope (same level or parent-child)

### Universal Path: Internal Query Interface

Functions can invoke other functions through the database's internal query interface — a local loopback that skips the network stack but goes through the database's routing and permission layer.

- Works across any scope boundary
- Permissions enforced on every call
- Slightly more overhead than direct calls
- Always available as a fallback

The fast path is an optimization on top of the universal path, not a replacement.

---

## Plugin Execution Model

### Trust Tiers

| Trust Level | Runtime | Overhead | Use Case |
|---|---|---|---|
| **Trusted** | Native `.so` via `dlopen` | Zero (direct function call) | First-party functions, core operations |
| **Untrusted** | WASM sandbox (wasmi or similar) | ~5x interpreter overhead | Community plugins, user-submitted functions |

Both tiers implement the same plugin trait. The database loads them through the same interface. Deployment configuration determines trust level.

### SDK Primitives

The AeorDB SDK crate exposes database primitives that functions use to access data:

```rust
// Pseudo-code — SDK ergonomics TBD
let rows = table.rows()
    .filter(|row| row.get("region") == region)
    .unique()
    .map(|row| capitalize(row.get("first_name")))
    .collect();

response.write(200, json::encode(&rows));
```

The SDK provides:
- Table/column access (subject to scope permissions)
- Index lookups (leveraging the scalar ratio indexing engine)
- Iterators with filter, map, reduce, unique, sort, group_by, etc.
- Access to parent-scope functions
- Response writing (status codes, encoding)

### Permissions

Permissions are enforced at the plugin interface boundary:
- Row-level access control
- Column-level access control
- Cell-level access control
- Function invocation permissions
- Scope-based defaults from the hierarchy, with optional further restrictions

The plugin cannot bypass the interface — it has no direct access to storage or indexes. It sees only what the permission layer allows through the SDK.

---

## What This Solves

From [Why Databases Suck](../docs/why-databases-suck.md):
- **#2 Query optimizer is a black box of lies** — No optimizer. You write the logic.
- **#4 Schema rigidity vs. schema chaos** — Functions define their own view of the data.
- **#8 Indexing is manual and static** — SDK leverages adaptive scalar indexing transparently.
- **#9 Observability is awful** — Functions are code. Profile them like code.
- **#10 Data types are anemic** — Functions can transform data however they want.
- **#12 Testing and local dev is painful** — Functions are testable Rust code. Unit test locally, deploy to database.

---

## Open Questions

- [ ] Exact SDK API design and ergonomics
- [ ] Function versioning strategy (blue-green? canary? rollback?)
- [ ] Streaming responses for large result sets
- [ ] Function hot-reloading (update without downtime)
- [ ] Rate limiting and resource quotas per function
- [ ] Function dependency tracking and lifecycle management
- [ ] WASM runtime selection (wasmi vs alternatives — see [WASM Runtime Research](./wasm-runtime-research.md))
- [ ] Schema for function metadata (arguments, return types, documentation)
- [ ] How do functions declare their expected argument schema for validation?
