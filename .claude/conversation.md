# AeorDB — Implementation Plan & Conversation

This document is the async collaboration point between Wyatt and Claude. Wyatt will respond with inline comments. Claude will update based on feedback.

---

## Current State

- Rust workspace initialized: `aeordb-lib` (library) + `aeordb-cli` (binary)
- Rust 1.94.0, cargo 1.94.0, clippy 0.1.94
- `bot-docs/` contains research and architectural plans
- No implementation code yet. No tests. No dependencies beyond the scaffolding.
- redb cloned to `/tmp/claude/aeordb-research/redb` for reference

---

## Phase 1: Walking Skeleton

**Goal:** An HTTP server that can store and retrieve data from a redb-backed single file. JWT auth. Basic CRUD. Tests at every step.

### 1.1 — Project Dependencies & Structure

Add to `aeordb-lib/Cargo.toml`:
```toml
[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["cors", "trace"] }
hyper = "1"
redb = "3.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
jsonwebtoken = "9"
argon2 = "0.5"
ed25519-dalek = { version = "2", features = ["rand_core"] }
rand = "0.8"
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
tracing = "0.1"
tracing-subscriber = "0.3"
thiserror = "2"
```

Add to `aeordb-cli/Cargo.toml`:
```toml
[dependencies]
aeordb = { path = "../aeordb-lib" }
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
```

Dev dependencies (workspace-level):
```toml
[dev-dependencies]
reqwest = { version = "0.12", features = ["json"] }
tempfile = "3"
```

<!-- WYATT: Review these dependency choices. Anything you want swapped out or removed? Any concerns about specific versions? Note: I went with `ed25519-dalek` for JWT signing — it's the most established Ed25519 crate in Rust. `jsonwebtoken` supports EdDSA natively. -->

<!-- 
Looks good to me!
 -->

### 1.2 — Storage Layer (redb wrapper)

Build a thin wrapper around redb that:

1. Opens/creates a `.aeor` database file
2. Manages a redb `Database` instance
3. Provides table-level CRUD operations:
   - `create_document(table, document) -> document_id`
   - `get_document(table, document_id) -> Option<Document>`
   - `update_document(table, document_id, fields) -> Result`
   - `delete_document(table, document_id) -> Result` (soft-delete via `is_deleted`)
   - `list_documents(table, filters) -> Vec<Document>`
4. Auto-injects mandatory fields on every document:
   - `document_id`: UUID v4 (auto-generated if not provided)
   - `created_at`: timestamp (auto-set on create)
   - `updated_at`: timestamp (auto-set on create and update)
   - `is_deleted`: boolean (defaults to `false`)
5. Documents are stored as JSON-encoded bytes in redb key-value tables
6. Table names map to redb table definitions

<!-- WYATT: I'm suggesting JSON encoding for document storage initially because it's simple and debuggable. We could switch to a more efficient binary format (MessagePack, bincode, CBOR) later. The chunk store will eventually replace this whole layer, so I don't want to over-invest here. Thoughts? -->

<!-- 
My thoughts are we always store in the user's format, and the indexer adjusts as needed. User could choose JSON, and json indexers, or XML and XML indexers, or whatever...
 -->

#### 1.2 Tests

```
spec/storage/
  redb_wrapper_spec.rs
    - test_create_document_generates_uuid
    - test_create_document_sets_timestamps
    - test_create_document_sets_is_deleted_false
    - test_create_document_user_provided_id_preserved
    - test_create_document_user_provided_timestamps_preserved
    - test_get_document_returns_none_for_missing
    - test_get_document_returns_document
    - test_get_document_excludes_soft_deleted
    - test_update_document_changes_updated_at
    - test_update_document_preserves_created_at
    - test_update_document_returns_error_for_missing
    - test_delete_document_sets_is_deleted_true
    - test_delete_document_preserves_data
    - test_delete_document_returns_error_for_missing
    - test_list_documents_excludes_soft_deleted
    - test_list_documents_empty_table
    - test_list_documents_returns_all_non_deleted
    - test_create_opens_new_database_file
    - test_create_reopens_existing_database_file
    - test_concurrent_reads_dont_block
    - test_document_survives_close_and_reopen (persistence)
    - test_invalid_table_name_rejected
    - test_empty_document_gets_mandatory_fields
    - test_large_document_storage (push toward redb limits to understand them)
```

<!-- WYATT: This is the storage test suite for Phase 1. I've tried to cover happy paths AND failure paths per your testing requirements. The large_document test is important — we need to understand redb's practical limits firsthand, not just from docs. Anything missing? -->

<!-- 
Concurrency tests are missing.
~~Size tests are missing.~~ Oops! Nope. Now I see it.
Stress tests are missing.
If we have a "delete", we should have an "undelete" (could be `update({ is_deleted: false })` as one option), and a way for "list" to show deleted documents.
(let's please all do this in-memory, so you don't kill my HDD)
 -->

### 1.3 — HTTP Server (axum)

Build the axum server with:

1. Server startup: bind to configurable host:port, initialize redb, load signing keys
2. Health check endpoint: `GET /admin/health` → `200 { "status": "ok" }`
3. Document CRUD endpoints (unauthenticated initially, auth added in 1.4):
   - `POST /:database/:table` → create document
   - `GET /:database/:table/:id` → get document
   - `PATCH /:database/:table/:id` → update document
   - `DELETE /:database/:table/:id` → soft-delete document
   - `GET /:database/:table` → list documents
4. Proper HTTP status codes:
   - `200` — success with body
   - `201` — created
   - `404` — document/table not found
   - `400` — invalid request body
   - `500` — internal error
5. JSON request/response bodies with `Content-Type: application/json`
6. Request tracing via `tower-http`
7. Graceful shutdown on SIGINT/SIGTERM

<!-- WYATT: I'm starting WITHOUT auth on the CRUD endpoints so we can test the storage + HTTP integration in isolation. Auth gets layered on in 1.4. This follows the "test one thing at a time" principle. Agree? Also — should the path be `/:database/:table/:id` or something different? The master plan has `/:database/:schema/:table/:function` for function invocation, but for basic CRUD we don't have functions yet. -->

<!-- 
I am for it! 👍
 -->

#### 1.3 Tests

```
spec/http/
  server_spec.rs
    - test_health_check_returns_200
    - test_create_document_returns_201_with_id
    - test_create_document_returns_body_with_mandatory_fields
    - test_get_document_returns_200_with_document
    - test_get_document_returns_404_for_missing
    - test_get_deleted_document_returns_404
    - test_update_document_returns_200
    - test_update_document_returns_404_for_missing
    - test_delete_document_returns_200
    - test_delete_document_returns_404_for_already_deleted
    - test_list_documents_returns_200_with_array
    - test_list_documents_empty_table_returns_empty_array
    - test_invalid_json_body_returns_400
    - test_missing_content_type_returns_400
    - test_nonexistent_route_returns_404
    - test_server_starts_and_stops_gracefully
    - test_concurrent_requests_handled
    - test_large_request_body_handled (what's our limit?)
    <!-- 
    No limit! But, limit for testing... 🤔 probably something that won't kill anyone's ram. Okay, thinking about this: Our stress tests (or, at least, not all of them) should be unit tests. We should probably make these as "tools", where we can stress test and optimize in sessions. We don't want to mess with GB of data (streamed or otherwise) inside a unit test.
     -->
    - test_empty_table_name_rejected
    - test_special_characters_in_table_name
```

<!-- WYATT: These are integration tests — they spin up a real axum server (on a random port), hit it with reqwest, and verify responses. Each test gets its own tempfile-backed redb instance for isolation. Sound right? -->

<!-- 
Sounds good!
 -->

### 1.4 — Authentication

Layer JWT auth onto the HTTP server:

1. **Key generation on first startup:**
   - Generate Ed25519 keypair
   - Store private key encrypted in the redb database metadata
   - This key signs all JWTs

2. **Bootstrap API key:**
   - On first startup (empty database), generate a root API key
   - Print it to stdout once: `Root API key: aeor_k_...`
   - Store it hashed (argon2id) in the database
   - This is the only way to initially authenticate

   <!-- 
   Let's make "npm scripts" to do this, to set an initial key, to update the server key, etc...
    -->

3. **API key → JWT endpoint:**
   ```
   POST /auth/token
   { "api_key": "aeor_k_..." }
   → 200 { "token": "eyJ...", "expires_in": 3600 }
   ```

4. **Auth middleware (tower layer):**
   - Extract `Authorization: Bearer <jwt>` header
   - Validate JWT signature (Ed25519), expiry, issuer
   - Inject user claims into request extensions
   - Return `401` for missing/invalid/expired tokens
   - Exempt endpoints: `/admin/health`, `/auth/token`

5. **API key management endpoints (authenticated, admin-only):**
   - `POST /admin/api-keys` → create new API key
   - `GET /admin/api-keys` → list API keys (hashed, not plaintext)
   - `DELETE /admin/api-keys/:key_id` → revoke API key

<!-- WYATT: The bootstrap flow is important — first startup prints the root key, and that's the only way in. No default passwords, no insecure defaults. The key is shown once and never again (we only store the hash). If you lose it, you'd need to either have another admin key or reset the database. Is that acceptable, or do you want a recovery mechanism? -->

<!-- 
This is acceptable. This is how I would like it. Let's make sure we have npm scripts to help us create and update he root key.
 -->

#### 1.4 Tests

```
spec/auth/
  jwt_spec.rs
    - test_generate_ed25519_keypair
    - test_sign_and_verify_jwt
    - test_expired_jwt_rejected
    - test_tampered_jwt_rejected
    - test_wrong_issuer_rejected
    - test_missing_claims_rejected
    - test_jwt_contains_correct_claims

  api_key_spec.rs
    - test_create_api_key_returns_prefixed_key
    - test_api_key_stored_as_argon2_hash
    - test_verify_valid_api_key
    - test_verify_invalid_api_key_rejected
    - test_verify_revoked_api_key_rejected
    - test_list_api_keys_returns_metadata_not_secrets
    - test_delete_api_key_succeeds
    - test_delete_nonexistent_api_key_returns_404

  auth_middleware_spec.rs
    - test_unauthenticated_request_returns_401
    - test_valid_bearer_token_passes
    - test_expired_bearer_token_returns_401
    - test_malformed_bearer_token_returns_401
    - test_missing_authorization_header_returns_401
    - test_health_endpoint_exempt_from_auth
    - test_auth_token_endpoint_exempt_from_auth
    - test_claims_injected_into_request_context

  auth_flow_spec.rs (integration / e2e)
    - test_full_flow_api_key_to_jwt_to_crud
    - test_bootstrap_prints_root_api_key
    - test_root_api_key_can_create_other_keys
    - test_non_admin_key_cannot_create_keys
    - test_revoked_key_cannot_get_new_tokens
    - test_concurrent_auth_requests
```

<!-- WYATT: The auth_flow_spec tests are full end-to-end: start server, get root key, exchange for JWT, use JWT to create documents, create a second API key, use that key, revoke it, verify it stops working. These are the tests that prove the whole auth system works as a unit. -->

<!-- 
Looks good!
 -->

---

## Phase 2: Plugin Interface

**Goal:** WASM runtime integrated. Users can deploy and invoke compiled plugins via HTTP. Plugins can access database primitives through an SDK.

### 2.1 — WASM Runtime Integration

1. Add `wasmi` (or chosen runtime) as a dependency
2. Build a `PluginRuntime` that can:
   - Load a `.wasm` binary
   - Instantiate it in a sandbox
   - Call exported functions with arguments
   - Receive return values
   - Enforce memory limits and execution timeouts
3. Define host functions that the WASM guest can call:
   - `db_read(table, key) -> value` — read from storage
   - `db_write(table, key, value) -> result` — write to storage
   - `db_delete(table, key) -> result` — delete from storage
   - `db_query(table, filter) -> results` — query with filter
   - `response_write(status, body)` — write HTTP response
4. Handle errors gracefully: WASM traps, OOM, timeouts all return clean HTTP errors

<!-- WYATT: The host function API is the critical design surface. This is what plugin authors interact with. I've kept it minimal for Phase 2 — just enough to prove the architecture. The full SDK with iterators, maps, filters, etc. comes later. Is this the right starting granularity, or do you want more/fewer host functions initially? -->

<!-- 
This is fine... I am not entirely sure about "tables" just yet... but even so, this is a good starting point. We will more fully plan this later, especially since we will hopefully then know more of what we need to build.
 -->

### 2.2 — Plugin Deployment & Invocation

1. Deploy endpoint: `PUT /:database/:schema/:table/_deploy`
   - Accepts multipart or base64-encoded `.wasm` binary
   - Validates the WASM module (exports required functions)
   - Stores the plugin in redb (as a blob in a system table)
   - Returns the function name/path

2. Invoke endpoint: `POST /:database/:schema/:table/:function_name`
   - Loads the deployed WASM module
   - Instantiates sandbox with user context (from JWT claims)
   - Passes request body as arguments
   - Executes the plugin
   - Returns the plugin's response

3. Plugin SDK crate: `aeordb-plugin-sdk`
   - A new workspace member: `aeordb-plugin-sdk/`
   - Provides Rust types and functions that compile to WASM
   - Wraps the host function FFI in an ergonomic API
   - Users add this as a dependency in their plugin projects

<!-- WYATT: The SDK crate is a separate workspace member that plugin authors depend on. It compiles to WASM and provides the nice API on top of the raw host function FFI. Example usage from a plugin:

```rust
use aeordb_plugin_sdk::prelude::*;

#[aeordb_function]
fn active_users(args: Args) -> Result<Response> {
    let region = args.get::<String>("region")?;
    let rows = db::query("users")
        .filter("region", Eq(region))
        .filter("is_active", Eq(true))
        .collect()?;
    Response::json(200, &rows)
}
```

Is this the kind of ergonomics you're envisioning? -->

<!-- 
Yes, absolutely! This is exactly what I am envisioning!
 -->

### 2.3 — Native Plugin Support (dlopen)

1. Define a C ABI-compatible plugin trait
2. Load `.so` / `.dylib` / `.dll` via `libloading` crate
3. Same host function interface as WASM — just without the sandbox
4. Trust tier configuration: mark plugins as trusted (native) or untrusted (WASM)

<!-- WYATT: Native plugins come after WASM because WASM is the safer default. Native is the performance escape hatch for trusted code. The interface is identical — only the loading mechanism differs. Agree with this ordering? -->

<!-- 
Yes, I think this is sound reasoning. We will need to configure a load path. Let's have a default of `{defaultConfigPath}/plugins/`.

We need to test our WASM and native plugin interfaces completely with unit tests.
 -->

#### Phase 2 Tests

```
spec/plugins/
  wasm_runtime_spec.rs
    - test_load_valid_wasm_module
    - test_load_invalid_wasm_rejected
    - test_call_exported_function
    - test_pass_arguments_to_function
    - test_receive_return_value
    - test_memory_limit_enforced
    - test_execution_timeout_enforced
    - test_wasm_trap_returns_clean_error
    - test_host_function_db_read
    - test_host_function_db_write
    - test_host_function_db_delete
    - test_host_function_response_write
    - test_plugin_cannot_access_unauthorized_tables
    - test_multiple_concurrent_plugin_invocations

  plugin_deploy_spec.rs
    - test_deploy_wasm_plugin_returns_200
    - test_deploy_invalid_wasm_returns_400
    - test_deploy_plugin_missing_required_exports_returns_400
    - test_invoke_deployed_plugin_returns_result
    - test_invoke_nonexistent_plugin_returns_404
    - test_redeploy_overwrites_existing_plugin
    - test_delete_deployed_plugin
    - test_list_deployed_plugins

  plugin_sdk_spec.rs (compile-to-wasm tests)
    - test_sdk_query_function_compiles_to_wasm
    - test_sdk_filter_function_compiles_to_wasm
    - test_sdk_response_write_works
    - test_sdk_error_handling

  native_plugin_spec.rs
    - test_load_native_shared_library
    - test_call_native_plugin_function
    - test_native_plugin_same_interface_as_wasm
    - test_invalid_shared_library_rejected
```

<!-- WYATT: For the WASM tests, we'll need to compile small test plugins as part of the test suite. I'm thinking a `spec/fixtures/` directory with minimal Rust plugin source files that get compiled to .wasm during `cargo test`. This adds a build step but ensures we're testing real WASM, not mocks. -->


<!-- 
Yes, I want this to happen. I want our plugins to be proven with unit tests.
 -->
---

## Phase 3: Foundation Hardening

**Goal:** Magic link auth, hierarchical function scoping, permission rules, and the mandatory field schema enforcement.

### 3.1 — Magic Link Authentication

1. `POST /auth/magic-link` → generate code, store hash, trigger email delivery
2. `GET /auth/magic-link/verify?code=...` → validate code, return JWT
<!-- 
We should have in-memory rate-limiting for these endpoints. We should watch out for massive flood attempts.
 -->
3. Email delivery is a **webhook/plugin** — the database generates the code and calls a configured URL with the email payload. Not our job to run an SMTP server.
<!-- 
We should have the default "email" plugin simply write to the logs, so we can easily verify it works, and get login links.

NOTE: WE WILL NOT enable this in production.
 -->
4. Code properties: cryptographic random, 32+ bytes, hashed in storage, 10-minute expiry, single-use

<!-- WYATT: The webhook approach for email delivery keeps the database engine clean. You configure a URL like `https://your-api.com/send-email` and aeordb POSTs the magic link payload there. Your infrastructure handles actual email sending. This could also be a plugin. Thoughts? -->

<!-- 
For now let's just have the internal mechanism, and log out the magic link. We will add the email guts and webhook posting (which I like) later.
 -->

### 3.2 — Hierarchical Function Scoping

1. Functions deployed at a path level are visible to that level and below
2. Scope resolution: when a plugin calls `db::function("validate")`, the runtime searches upward through the hierarchy
3. Cross-scope calls go through the internal query interface (permission-checked)
4. Same-scope or parent-scope calls can use the fast path (direct invocation)

### 3.3 — Permission Rules as Plugins

1. Rule deployment: `PUT /:database/:table/_deploy` with `"type": "rule"`
2. Rules are WASM plugins that receive a `RuleContext` and return `Allow/Deny/Redact`
3. The SDK intercepts data access and evaluates applicable rules before returning data
4. Rules inherit downward through the hierarchy

### 3.4 — JWT Refresh Flow

1. `POST /auth/refresh` with refresh token
2. Validate refresh token, issue new JWT + new refresh token
3. Old refresh token invalidated (rotation)

#### Phase 3 Tests

```
spec/auth/
  magic_link_spec.rs
    - test_request_magic_link_returns_200_always (no email enumeration)
    - test_magic_link_code_stored_hashed
    - test_verify_valid_code_returns_jwt
    - test_verify_expired_code_returns_401
    - test_verify_used_code_returns_401 (single-use)
    - test_verify_invalid_code_returns_401
    - test_webhook_called_with_correct_payload
    - test_webhook_failure_doesnt_crash_server

  refresh_spec.rs
    - test_refresh_returns_new_jwt
    - test_refresh_rotates_refresh_token
    - test_old_refresh_token_rejected_after_rotation
    - test_expired_refresh_token_rejected
    - test_invalid_refresh_token_rejected

spec/plugins/
  scoping_spec.rs
    - test_function_visible_at_own_level
    - test_function_visible_to_children
    - test_function_not_visible_to_siblings
    - test_function_not_visible_to_parents
    - test_scope_resolution_searches_upward
    - test_cross_scope_call_goes_through_query_interface
    - test_same_scope_call_uses_fast_path

  rules_spec.rs
    - test_deploy_rule_plugin
    - test_rule_receives_correct_context
    - test_rule_allow_passes_data_through
    - test_rule_deny_blocks_access
    - test_rule_redact_removes_cell_value
    - test_multiple_rules_most_restrictive_wins
    - test_rule_inherits_to_child_scopes
    - test_rule_does_not_apply_to_sibling_scopes
    - test_admin_bypass_rules (if we decide this)
    - test_rule_with_invalid_return_treated_as_deny
```

<!-- WYATT: The scoping tests are critical — they verify the hierarchical inheritance model that underpins both function access and permission rules. If scoping is wrong, everything built on top of it is wrong. -->

<!-- 
Correct. Make sure you focus when writing these tests.
 -->

---

## Phase 4: Hard Decisions & Core Engine

**Goal:** Start building the real engine — chunk store, custom indexing, replication. This is where aeordb becomes aeordb.

### 4.1 — Content-Addressed Chunk Store

1. Design the chunk format (header, data, hash)
2. Implement chunk creation (split data → hash → store)
3. Implement chunk retrieval (hash → data)
4. Implement hash maps (ordered list of chunk hashes = a file)
5. Implement map versioning (old maps preserved = snapshots)
6. Wire the chunk store behind the storage API so existing CRUD still works
7. Configurable chunk size (power-of-two, runtime adjustable)

### 4.2 — Scalar Ratio Indexing (Prototype)

1. Implement the `f(x) → [0.0, 1.0]` mapping for basic types
2. Build the offset table structure
3. Implement self-correcting read-back
4. Benchmark against simple hash lookup and B-tree alternatives
5. User-requested index creation: `POST /:database/:table/_index`

### 4.3 — openraft Integration

1. Implement `RaftLogStorage` (custom append-only file)
2. Implement `RaftStateMachine` (chunk store operations)
3. Implement `RaftNetwork` (HTTP-based)
4. Single-node bootstrap
5. Multi-node join and replication

### 4.4 — Versioning

1. Root hash map = database snapshot
2. Named versions / tags
3. Restore to any previous version
4. Diff between versions (which chunks changed)
5. Garbage collection of unreferenced chunks

<!-- WYATT: Phase 4 is where it gets REAL. Each of these sub-phases is substantial. 4.1 (chunk store) is probably the single biggest piece of engineering in the whole project. I expect we'll need to break it down further before implementation. 4.2 (indexing) is your baby — I expect you'll have strong opinions on the implementation details once we get there.

Do you want to prioritize any of these sub-phases over others? My instinct is 4.1 → 4.4 → 4.2 → 4.3 (chunk store enables versioning naturally, indexing and replication can be parallelized after). -->

<!-- 
I think you have thought this out well. I like the order you have here.
 -->

#### Phase 4 Tests

```
spec/chunks/
  chunk_store_spec.rs
    - test_store_and_retrieve_chunk
    - test_chunk_hash_is_deterministic
    - test_duplicate_chunk_not_stored_twice (dedup)
    - test_chunk_integrity_verified_on_read
    - test_corrupt_chunk_detected
    - test_configurable_chunk_size
    - test_runtime_chunk_size_change
    - test_large_file_split_into_chunks
    - test_file_reconstruction_from_chunks
    - test_partial_file_update_creates_minimal_new_chunks
    - test_garbage_collection_removes_unreferenced_chunks
    - test_garbage_collection_preserves_referenced_chunks
    - test_concurrent_chunk_reads
    - test_concurrent_chunk_writes

  hash_map_spec.rs
    - test_create_hash_map
    - test_hash_map_resolves_to_chunks
    - test_hash_map_versioning
    - test_old_version_still_resolvable
    - test_diff_between_versions
    - test_map_of_maps (nested)
    - test_root_map_represents_full_database_state

spec/indexing/
  scalar_ratio_spec.rs
    - test_u8_maps_to_unit_range
    - test_u16_maps_to_unit_range
    - test_u64_maps_to_unit_range
    - test_negative_values_separate_branch
    - test_string_multistage_decomposition
    - test_equality_lookup
    - test_range_query_gt
    - test_range_query_lt
    - test_range_query_between
    - test_offset_table_self_corrects_on_read
    - test_offset_table_resize_degrades_gracefully
    - test_offset_table_heals_after_resize
    - test_user_defined_mapping_function
    - test_dimensional_growth_on_precision_overflow

spec/replication/
  raft_log_spec.rs
    - test_append_entries
    - test_read_entries_by_index
    - test_truncate_after
    - test_purge_old_entries
    - test_crash_recovery (write entries, kill, reopen, verify)
    - test_concurrent_append_and_read

  raft_integration_spec.rs
    - test_single_node_bootstrap
    - test_single_node_write_and_read
    - test_three_node_cluster_formation
    - test_leader_replicates_to_followers
    - test_leader_failure_triggers_election
    - test_follower_catches_up_after_rejoin
    - test_snapshot_transfer_to_new_node
    - test_chunk_dedup_across_replication (only new chunks sent)
```

<!-- WYATT: Phase 4 tests are the most important tests in the whole project. The chunk store tests verify data integrity — if these fail, the database is broken. The replication tests verify distributed correctness — if these fail, nodes diverge. I've included crash recovery and corruption detection tests because those are the scenarios that separate a toy from a real database. -->

<!-- 
Yes, we need to test from every angle here.
 -->

---

## Implementation Notes

### Test Infrastructure
- All tests use `tempfile` for isolated database instances
- HTTP integration tests use `axum::test` or a real server on random ports
- WASM plugin tests compile fixture plugins from `spec/fixtures/` source
- Tests follow the `spec/` directory structure mirroring `src/`
- Test file naming: `*_spec.rs`

### Code Organization (proposed)
```
aeordb-lib/src/
  lib.rs                    — public API
  server/
    mod.rs                  — axum server setup
    routes.rs               — route definitions
    middleware.rs            — auth middleware, tracing
  auth/
    mod.rs
    jwt.rs                  — JWT signing, verification
    api_key.rs              — API key creation, hashing, verification
    magic_link.rs           — magic link generation, verification
  storage/
    mod.rs
    redb_backend.rs         — redb wrapper (Phase 1)
    chunk_store.rs          — content-addressed chunks (Phase 4)
  plugins/
    mod.rs
    wasm_runtime.rs         — WASM loading, sandboxing, execution
    native_runtime.rs       — dlopen native plugin loading
    host_functions.rs       — functions exposed to plugins
    sdk_types.rs            — shared types between host and guest
  indexing/
    mod.rs
    scalar_ratio.rs         — unit scalar indexing
    traits.rs               — indexing plugin interface
  replication/
    mod.rs
    raft_log.rs             — custom append-only log
    state_machine.rs        — openraft state machine impl
    network.rs              — HTTP-based raft transport

aeordb-cli/src/
  main.rs                   — CLI entry point
  commands/
    start.rs                — start server
    api_key.rs              — manage API keys
    migrate.rs              — database migrations
    info.rs                 — database info/stats

aeordb-plugin-sdk/src/
  lib.rs                    — plugin SDK public API
  prelude.rs                — common imports
  db.rs                     — database access functions
  response.rs               — response building
  types.rs                  — shared types

spec/
  storage/
    redb_wrapper_spec.rs
  http/
    server_spec.rs
  auth/
    jwt_spec.rs
    api_key_spec.rs
    auth_middleware_spec.rs
    auth_flow_spec.rs
    magic_link_spec.rs
    refresh_spec.rs
  plugins/
    wasm_runtime_spec.rs
    plugin_deploy_spec.rs
    plugin_sdk_spec.rs
    native_plugin_spec.rs
    scoping_spec.rs
    rules_spec.rs
  chunks/
    chunk_store_spec.rs
    hash_map_spec.rs
  indexing/
    scalar_ratio_spec.rs
  replication/
    raft_log_spec.rs
    raft_integration_spec.rs
  fixtures/
    simple_query.rs         — minimal WASM plugin source
    echo_plugin.rs          — returns its arguments
    failing_plugin.rs       — intentionally traps/panics
```

<!-- WYATT: This is the proposed directory structure. I've separated concerns cleanly but I'm not married to any of it. The key principle is: each module is testable in isolation, and the spec/ directory mirrors src/. Let me know what you'd change. -->

<!-- 
Love it! Just as I would have done it.
 -->

### What's NOT in these phases
- Content-defined chunking (rolling hash) — future optimization
- Multi-database support — one database per instance initially
- Schema system formalization — documents are schemaless JSON initially
- Compression — deferred to chunk store phase
- Encryption — deferred
- Observability dashboard — deferred
- The full indexing plugin interface — deferred to Phase 4

<!-- 
Yeah, essentially we are just making a really cool indexed distributed filesystem right now... which actually sounds fucking epic!
 -->

---

## Questions for Wyatt

1. **Dependency review** — see inline comment at 1.1
2. **Document encoding** — JSON initially, or something else? (see 1.2 comment)
3. **Auth bootstrap** — is "print root key once, never again" acceptable? (see 1.4 comment)
4. **Email delivery** — webhook approach for magic links? (see 3.1 comment)
5. **Plugin SDK ergonomics** — does the example API look right? (see 2.2 comment)
6. **Phase 4 ordering** — chunk store → versioning → indexing → replication? (see 4 comment)
7. **CRUD path structure** — `/:database/:table/:id` for now, or immediately `/:database/:schema/:table/:id`?
8. **Anything missing?** — What did I forget?

<!-- 
I think I answered everything. If I haven't, let me know. I don't think you missed anything here. Good work! I like your plan.
 -->
---

*Waiting for inline comments...*
