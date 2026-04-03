# AeorDB — TODO

## Current: Users, Groups, and Permissions (crudlify)

### Batch 1: Core Identity (Tasks 1-7)
- [ ] Task 1: User entity (CRUD + SystemTables + registry)
- [ ] Task 2: Group entity (query-based, CRUD + SystemTables)
- [ ] Task 3: Update ApiKeyRecord (add user_id, drop roles)
- [ ] Task 4: Update JWT (sub = user_id)
- [ ] Task 5: Root as nil UUID (engine bypass + validation)
- [ ] Task 6: Bootstrap (zero keys → root API key with nil UUID)
- [ ] Task 7: Per-user auto-groups

### Batch 2: Permission System (Tasks 8-12)
- [ ] Task 8: .permissions files (per-directory, deny-all default)
- [ ] Task 9: Permission resolution (path walk + crudlify)
- [ ] Task 10: Group cache (user_id → groups, LRU + TTL)
- [ ] Task 11: Permissions cache (.permissions files)
- [ ] Task 12: Permission middleware (check every HTTP request)

### Batch 3: Admin + CLI (Tasks 13-14)
- [ ] Task 13: Admin endpoints (user CRUD, group CRUD)
- [ ] Task 14: Emergency reset CLI command

### Batch 4: Tests (Task 15)
- [ ] Task 15: 7 real-world scenarios + 20 security tests
