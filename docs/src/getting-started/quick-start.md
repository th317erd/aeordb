# Quick Start

This tutorial walks you through the core operations: storing files, querying, creating snapshots, and cleaning up. All examples use `curl` and assume auth is disabled for simplicity.

## 1. Start the Server

```bash
aeordb start --database mydb.aeordb --port 3000 --auth false
```

You should see log output indicating the server is listening on port 3000. The `mydb.aeordb` file is created automatically if it does not exist.

## 2. Store a File

Store a JSON file at the path `/users/alice.json`:

```bash
curl -X PUT http://localhost:3000/files/users/alice.json \
  -H "Content-Type: application/json" \
  -d '{"name":"Alice","age":30,"city":"Portland"}'
```

Expected response:

```json
{
  "status": "created",
  "path": "/users/alice.json",
  "hash": "a1b2c3d4..."
}
```

Store a few more files to have data to query:

```bash
curl -X PUT http://localhost:3000/files/users/bob.json \
  -H "Content-Type: application/json" \
  -d '{"name":"Bob","age":25,"city":"Seattle"}'

curl -X PUT http://localhost:3000/files/users/carol.json \
  -H "Content-Type: application/json" \
  -d '{"name":"Carol","age":35,"city":"Portland"}'
```

## 3. Read a File

```bash
curl http://localhost:3000/files/users/alice.json
```

Expected response:

```json
{"name":"Alice","age":30,"city":"Portland"}
```

## 4. List a Directory

Append a trailing slash to list directory contents:

```bash
curl http://localhost:3000/files/users/
```

Expected response:

```json
{
  "path": "/users/",
  "entries": [
    {"name": "alice.json", "type": "file"},
    {"name": "bob.json", "type": "file"},
    {"name": "carol.json", "type": "file"}
  ]
}
```

## 5. Add an Index

To query fields, you need to tell AeorDB which fields to index. Store an index configuration at `.config/indexes.json` inside the directory:

```bash
curl -X PUT http://localhost:3000/files/users/.config/indexes.json \
  -H "Content-Type: application/json" \
  -d '{
    "indexes": [
      {"name": "age", "type": "u64"},
      {"name": "city", "type": "string"},
      {"name": "name", "type": ["string", "trigram"]}
    ]
  }'
```

This tells the engine to index the `age` field as a 64-bit unsigned integer, `city` as an exact string, and `name` as both an exact string and a trigram (for fuzzy matching). Existing files in the directory are automatically reindexed in the background.

## 6. Query

Query for users older than 28 in Portland:

```bash
curl -X POST http://localhost:3000/files/query \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/users/",
    "where": {
      "and": [
        {"field": "age", "op": "gt", "value": 28},
        {"field": "city", "op": "eq", "value": "Portland"}
      ]
    }
  }'
```

Expected response:

```json
{
  "results": [
    {"name": "Alice", "age": 30, "city": "Portland"},
    {"name": "Carol", "age": 35, "city": "Portland"}
  ],
  "total_count": 2
}
```

### Query Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `eq` | Equals | `{"field": "city", "op": "eq", "value": "Portland"}` |
| `gt` | Greater than | `{"field": "age", "op": "gt", "value": 25}` |
| `gte` | Greater than or equal | `{"field": "age", "op": "gte", "value": 25}` |
| `lt` | Less than | `{"field": "age", "op": "lt", "value": 30}` |
| `lte` | Less than or equal | `{"field": "age", "op": "lte", "value": 30}` |
| `between` | Range (inclusive) | `{"field": "age", "op": "between", "value": [25, 35]}` |
| `fuzzy` | Trigram fuzzy match | `{"field": "name", "op": "fuzzy", "value": "Alce"}` |
| `phonetic` | Phonetic match | `{"field": "name", "op": "phonetic", "value": "Karrol"}` |

## 7. Create a Snapshot

Save the current state as a named snapshot:

```bash
curl -X POST http://localhost:3000/versions/snapshots \
  -H "Content-Type: application/json" \
  -d '{"name": "v1"}'
```

Expected response:

```json
{
  "status": "created",
  "name": "v1",
  "root_hash": "e5f6a7b8..."
}
```

You can list all snapshots:

```bash
curl http://localhost:3000/versions/snapshots
```

## 8. Delete a File

```bash
curl -X DELETE http://localhost:3000/files/users/alice.json
```

Expected response:

```json
{
  "status": "deleted",
  "path": "/users/alice.json"
}
```

The file is removed from the current state (HEAD), but the snapshot `v1` still contains it. You can restore the snapshot to get it back.

## 9. Run Garbage Collection

Over time, deleted and overwritten data accumulates in the database file. Run GC to reclaim unreachable entries:

```bash
curl -X POST http://localhost:3000/system/gc
```

Expected response:

```json
{
  "versions_scanned": 2,
  "live_entries": 15,
  "garbage_entries": 3,
  "reclaimed_bytes": 1024,
  "duration_ms": 12,
  "dry_run": false
}
```

To preview what would be collected without actually deleting:

```bash
curl -X POST "http://localhost:3000/system/gc?dry_run=true"
```

## Next Steps

- [Configuration](./configuration.md) -- CLI flags, auth modes, CORS, index config
- [Architecture](../concepts/architecture.md) -- understand the storage engine internals
- [Versioning](../concepts/versioning.md) -- snapshots, forks, diff/patch, export/import
- [Indexing](../concepts/indexing.md) -- index types, multi-strategy fields, WASM parsers
