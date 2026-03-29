# AeorDB — TODO

## Current: Sprint 2 — redb-Native Filesystem

### Task 1: Soft-Delete Cleanup — DONE
### Task 2: Chunk Headers — DONE
### Task 3: Custom B-tree — DONE (backed up, pivoted to redb-native)

### Task 3R: Directory Entry Types + redb Directory Layer
- [ ] DirectoryEntry struct (serializable to redb values)
- [ ] EntryType enum (File, Directory, HardLink)
- [ ] RedbDirectory: insert, get, remove, list entries in a redb table
- [ ] Table-per-directory pattern: "dir:{path}" naming
- [ ] Tests

### Task 4R: Path Resolver (redb-native)
- [ ] Open "dir:{path}" tables segment by segment
- [ ] Auto-create intermediate directories (mkdir -p)
- [ ] store_file, read_file (streaming), delete_file, list_directory
- [ ] Tests

### Task 5: HTTP Wiring
- [ ] Replace document CRUD with path-based filesystem operations
- [ ] System tables stay in redb (API keys, config)
- [ ] Update HTTP tests
- [ ] Tests

### Task 6R: Version Management (redb savepoints)
- [ ] Thin wrapper around persistent savepoints
- [ ] Named versions table mapping names → savepoint IDs
- [ ] Create, restore, list, delete versions
- [ ] Tests
