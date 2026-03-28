# Data Model — Paths, Parsers, and Indexes

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## Core Concept: Everything Is a Path

AeorDB has no tables, no schemas, no collections in the traditional sense. It has **paths**. A path is a configured location where data lives.

```
/{root}/{...segments...}/{document_id}
```

Path segments are user-defined. The database does not impose meaning on them. The user gives them meaning through configuration. A path could represent:
- `database/schema/table` (relational-style)
- `project/year/month` (temporal organization)
- `media/type/resolution` (content-based)
- `tenant/service/entity` (multi-tenant)

How many segments mean what is either convention or a future configuration option.

---

## Path Configuration

Configuration lives at any path level and **inherits downward**. A document stored at `/myapp/users/abc123` is governed by the configuration at `/myapp/users/`, which may also inherit from `/myapp/`.

### Configuration Properties

```
{
  parsers: [...],        // plugins that extract fields from raw bytes
  validators: [...],     // plugins that accept/reject writes
  permissions: [...],    // rule plugins (allow/deny/redact)
  // inherited from parent if not specified
}
```

### Inheritance Rules

- Child paths inherit all configuration from parent paths
- Child paths can override or extend parent configuration
- Deepest configuration wins for conflicts
- Configuration set at `/` applies to the entire database

---

## Document Storage

A document is:
1. **Raw bytes** — whatever the user sent, in whatever format, untouched
2. **Mandatory metadata** — document_id, created_at, updated_at, content_type
3. **Stored as content-addressed chunks** — the raw bytes are split into chunks, keyed by hash

The database does NOT interpret the raw bytes. It stores them as-is.

---

## Parsers

Parsers are **plugins** (WASM or native) that extract named fields from raw bytes. They are the bridge between format-agnostic storage and structured indexing.

### How Parsers Work

```
User writes raw bytes to a path
       ↓
Validator plugin(s): "is this valid?" → reject or accept
       ↓
Parser plugin(s): "extract fields from these bytes"
       ↓
Extracted fields → handed to configured indexes
       ↓
Raw bytes → chunked and stored (format untouched)
```

### Multiple Parsers Per Path

A single document can be processed by **multiple parsers** simultaneously. Each parser extracts a different dimension of meaning from the same raw bytes.

Example — a photo stored at `/myapp/photos/`:

```
parsers: [
  {
    name: "image_metadata",
    plugin: "image_parser",
    indexes: [
      { field: "width", index_type: "u32" },
      { field: "height", index_type: "u32" },
      { field: "bits_per_pixel", index_type: "u8" },
      { field: "format", index_type: "string" },
    ]
  },
  {
    name: "ai_vision",
    plugin: "ai_classifier",
    indexes: [
      { field: "objects", index_type: "string" },
      { field: "scene", index_type: "string" },
      { field: "sentiment", index_type: "f64" },
      { field: "nsfw_score", index_type: "f64" },
    ]
  },
  {
    name: "exif",
    plugin: "exif_parser",
    indexes: [
      { field: "gps_latitude", index_type: "f64" },
      { field: "gps_longitude", index_type: "f64" },
      { field: "camera_model", index_type: "string" },
      { field: "taken_at", index_type: "u64" },
    ]
  }
]
```

One photo. Three parsers. Twelve indexes. Each parser extracts what it knows about. The parsers don't know about each other. The indexes don't know which parser produced the field.

### Parsers Are Paths

Plugins are stored at paths — they are documents in the database like everything else. Parser references in configuration are **path references** to deployed plugins.

```
/myapp/
  parsers/
    json_parser     → deployed WASM plugin
    image_parser    → deployed WASM plugin
    ai_classifier   → deployed WASM plugin
  users/
    _config: {
      parsers: [
        { name: "user_data", plugin: "../parsers/json_parser", indexes: [...] }
      ]
    }
  photos/
    _config: {
      parsers: [
        { name: "metadata", plugin: "../parsers/image_parser", indexes: [...] },
        { name: "ai", plugin: "../parsers/ai_classifier", indexes: [...] },
      ]
    }
```

This means:
- Parser plugins are versioned for free (they're chunks like everything else)
- Parser plugins are replicated for free (Raft replicates chunks)
- Parser plugins can be shared across paths (same path reference)
- Parsers are inspectable with the same tools as any other data
- Everything is a file at a path: documents, plugins, parsers, validators, query functions, permission rules

### Parser Plugin Interface

A parser plugin implements:
```
fn parse(data: &[u8], content_type: Option<&str>) -> Vec<ParsedField>
```

Where `ParsedField` is:
```
struct ParsedField {
  name: String,       // field name (e.g., "email", "width")
  value: Vec<u8>,     // raw field value (will be passed to the indexer)
}
```

The parser is given the raw bytes and content type. It returns named fields with their raw values. The engine then routes each field to the appropriate index based on the path configuration.

### Default Parsers (Ship With AeorDB)

Deployed as built-in plugins at a well-known path (e.g., `/_system/parsers/`):
- **JSON parser** — extracts fields by JSON path (e.g., `"$.email"`, `"$.address.city"`)
- **XML parser** — extracts fields by XPath
- Other format parsers added over time

### Custom Parsers

Users write their own parser plugin for any format:
1. Write a parser (Rust → WASM) that extracts fields from bytes
2. Deploy it to a path in the database (e.g., `/myapp/parsers/my_format`)
3. Configure a data path to reference it: `plugin: "../parsers/my_format"`
4. Start storing files — fields are automatically extracted and indexed

### Retroactive Parsing

Parsers can be added to a path at any time. When a new parser is added, existing documents can be re-indexed without modifying the raw data. The bytes never change — you just add a new lens to look at them through.

---

## Indexes

Indexes are **engine-level** (not plugins). The database owns the indexing algorithms. Parsers produce fields; the engine indexes them.

### Index Configuration

Each parser defines which of its extracted fields should be indexed and how:

```
{
  field: "email",           // field name from the parser
  index_type: "string",     // which indexing algorithm to use
}
```

### Available Index Types

- **Scalar ratio** — the [0.0, 1.0] self-correcting index (see [Indexing Engine](./indexing-engine.md))
- **String** — optimized for string equality and prefix matching
- **Fuzzy** — approximate/phonetic matching
- **Numeric types** — u8, u16, u32, u64, i64, f64 via scalar ratio mapping
- **Full-text** — text search (future)
- **Geospatial** — location-based queries (future)
- **Custom** — user-provided indexing algorithm via plugin (future)

### No Default Indexes

Nothing is indexed unless the user configures it at the path level. The only things that are always present are the mandatory metadata fields (document_id, created_at, updated_at), which are indexed at the engine level.

### Multiple Indexes Per Field

A single field can have multiple index types. For example, a field containing `"56"` could be indexed as both a string AND a u64, allowing both string matching and numeric range queries on the same data.

---

## Validators

Validators are **plugins** (WASM or native) that run before a write is accepted. They receive the raw bytes and return accept/reject.

```
fn validate(data: &[u8], content_type: Option<&str>) -> Result<(), String>
```

Validators are optional. If not configured at a path, any data is accepted. Multiple validators can be configured; all must pass.

---

## Permissions (Rules)

Permission rules are plugins configured at paths, as described in [HTTP Server & Auth](./http-server-and-auth.md). They inherit downward through the path hierarchy and control read/write/delete access at the document, field, or cell level.

---

## Write Flow

When a user writes a document to a path:

```
1. Resolve path configuration (local + inherited)
2. Run validator plugins → reject with 400 if any fail
3. Store raw bytes as content-addressed chunks
4. Create document metadata (document_id, timestamps, content_type)
5. Run parser plugins → extract named fields
6. Index extracted fields per configuration
7. Return document metadata to caller
```

## Read Flow

When a user reads a document by ID:

```
1. Look up document metadata by ID at the path
2. Check permission rules → reject with 403 if denied
3. Retrieve chunks → reconstruct raw bytes
4. Return raw bytes with original content-type
```

## Query Flow

When a user queries via a deployed function plugin:

```
1. Function plugin calls SDK to query indexes
2. SDK checks which indexes exist at the target path
3. Uses appropriate index for the query (range, exact, fuzzy, etc.)
4. Returns matching document IDs/locations
5. Function plugin retrieves documents as needed
6. Permission rules filter results per-document or per-field
```

---

## Data Organization Summary

| Concept | Traditional DB | AeorDB |
|---|---|---|
| Database | Named database | First path segment (configurable) |
| Schema | Named schema | Second path segment (configurable) |
| Table | Fixed-column table | Configured path with parsers + indexes |
| Row | Typed column values | Raw bytes (any format) |
| Column | Typed field | Parser-extracted field |
| Index | Per-column, DBA-created | Per-field, user-configured at path |
| Constraint | Schema enforcement | Validator plugin |
| View | Saved query | Deployed function plugin |

---

## Dot-Prefix Convention

Dot-prefixed names (`.config`, `.indexes`, `.system`) follow Unix "hidden file" convention. They are **meaningful by convention, not by enforcement**.

### Engine-Recognized Dot-Paths

| Path | Purpose | Default Permissions |
|---|---|---|
| `.config` | Path configuration (parsers, validators, permissions) | admin: read/write, users: read |
| `.indexes` | Index data (offset tables, scalar mappings) | engine: read/write, admin: read/write, users: read |
| `.system` | System-level data (API keys, signing keys, etc.) | admin: read/write, users: none |

### Design Principles

- **No special-casing in the engine.** Dot-prefixed paths are not "magical" — the engine reads `.config` by convention, but the storage layer treats them like any other path.
- **Everything is allowed if you have permission.** An admin can grant write access to `.indexes` for an external indexer. A service account can write to `.config` for automated path setup.
- **Custom dot-paths are fine.** Users can create `.hidden`, `.drafts`, `.backups` — the engine assigns no special meaning. Only the recognized names (`.config`, `.indexes`, `.system`) have engine-level semantics.
- **Defaults make it hard to shoot yourself in the foot.** But the waiver is implicit: if you have permission and you corrupt your indexes by writing garbage, that's on you.
- **Custom indexers aren't restricted to `.indexes`.** A plugin could write to `.custom_indexes` or anywhere else it has permission.

This is the Unix philosophy applied to a database: the filesystem doesn't care what you put where. Permissions are the safety net. Tools give meaning to paths by convention.

### Index Storage

Indexes live at `.indexes/` under the path they index:

```
/myapp/users/
  .config              → path configuration
  .indexes/
    email_string/      → index data (offset tables, scalar mappings)
    age_u8/            → index data
    name_fuzzy/        → index data
  abc123               → user document
  def456               → user document
```

Index data is stored as chunks like everything else, which means:
- **Indexes are versioned** — roll back to an old version, the indexes are still there
- **Indexes are replicated** — Raft syncs them like any other chunk
- **Indexes are integrity-checked** — BLAKE3 hashes, corruption detected on read
- **Indexes self-heal via replication** — corrupt chunk? Pull it from another node
- **Rebuilding** is just deleting the index data and re-running parsers over the documents

---

## Open Questions

- [ ] Path configuration storage format (how is `.config` data structured?)
- [ ] Re-indexing workflow when a new parser is added to an existing path
- [ ] Index naming convention when multiple parsers extract same-named fields
- [ ] Configuration API endpoints for managing path configs
- [ ] How deep can paths go? Any limit on nesting?
- [ ] Hot-reloading parser plugins without downtime
- [ ] How does path configuration interact with versioning?
- [ ] **Permission system design** — becoming a critical architectural piece (see [Permissions](./permissions.md))
