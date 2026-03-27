# AeorDB — TODO

## Current Phase: Planning → Phase 1

### Planning (In Progress)
- [x] Research filesystems for storage engine
- [x] Research why current databases suck
- [x] Design storage engine architecture (content-addressed chunks)
- [x] Design indexing engine (scalar ratio + pluggable)
- [x] Design query engine (function-based, WASM/native plugins)
- [x] Design replication strategy (openraft)
- [x] Design HTTP server and authentication
- [x] Research redb as initial storage backend
- [x] Research openraft for consensus
- [x] Research WASM runtimes for plugin sandboxing
- [x] Write implementation plan (4 phases)
- [ ] **Awaiting Wyatt's review of `.claude/conversation.md`**

### Phase 1: Walking Skeleton (Not Started)
- [ ] Add dependencies to Cargo.toml files
- [ ] Build redb storage wrapper with mandatory fields
- [ ] Build axum HTTP server with CRUD routes
- [ ] Implement JWT auth (Ed25519, API key exchange)
- [ ] Bootstrap flow (root API key on first startup)
- [ ] Auth middleware (tower layer)
- [ ] API key management endpoints
- [ ] Tests for all of the above

### Phase 2: Plugin Interface (Not Started)
- [ ] WASM runtime integration
- [ ] Host function API
- [ ] Plugin deployment and invocation endpoints
- [ ] Plugin SDK crate
- [ ] Native plugin support (dlopen)

### Phase 3: Foundation Hardening (Not Started)
- [ ] Magic link auth flow
- [ ] Hierarchical function scoping
- [ ] Permission rules as WASM plugins
- [ ] JWT refresh flow

### Phase 4: Core Engine (Not Started)
- [ ] Content-addressed chunk store
- [ ] Scalar ratio indexing prototype
- [ ] openraft integration
- [ ] Versioning via hash maps
