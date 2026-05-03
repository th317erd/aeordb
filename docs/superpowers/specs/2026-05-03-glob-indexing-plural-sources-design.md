# Glob-Based Subdirectory Indexing + Plural Source Resolver

## Goal

Enable a single index config to index files across subdirectories using glob patterns, and support extracting multiple values from arrays/objects in JSON documents using empty-string and regex source path segments.

## 1. Glob-Based Subdirectory Indexing

### Config Change

Add optional `glob` field to `PathIndexConfig`:

```json
{
  "glob": "*/session.json",
  "indexes": [
    {"name": "patient_name", "type": ["string", "trigram"]},
    {"name": "comments", "type": "trigram", "source": ["comments", "", "text"]}
  ]
}
```

- `glob` absent: behavior unchanged — index direct children only.
- `glob` present: index files matching the glob across subdirectories.
- Standard glob patterns: `*` (one directory level), `**` (any depth), `?` (single char).
- Use the `glob` crate's pattern matching (already a dependency).

### File Structure Example

```
/sessions/
  .config/indexes.json       ← config with glob: "*/session.json"
  .indexes/                  ← indexes stored HERE (config owner)
    patient_name.string.idx
    patient_name.trigram.idx
    comments.trigram.idx
  session-2024-01-01/
    session.json             ← indexed (matches glob)
  session-2024-01-02/
    session.json             ← indexed (matches glob)
  session-2024-01-02/
    notes.txt                ← NOT indexed (doesn't match glob)
```

### Three Code Paths

**1. Inline indexing on file store (`indexing_pipeline.rs`)**

Current behavior: check immediate parent for `.config/indexes.json`.

New behavior:
1. Check immediate parent for config (existing — handles non-glob configs).
2. If no config in parent, walk up the ancestor chain.
3. At each ancestor, check for a config with a `glob` field.
4. If found, test the file's path relative to the config directory against the glob pattern.
5. First matching ancestor wins (nearest ancestor priority).
6. Index files stored at the **config owner's directory** `.indexes/`, not the file's parent.

**2. Reindex task (`task_worker.rs`)**

Current behavior: `list_directory(path)` — direct children only.

New behavior when `glob` is present:
- Use `list_directory_recursive` with the glob pattern and `max_results: None`.
- Filter results to only `FileRecord` entries whose relative path matches the glob.
- Store all index entries at the config directory's `.indexes/`.

**3. Deletion cleanup (`indexing_pipeline.rs` or `directory_ops.rs`)**

When a file is deleted:
- Check ancestor directories for configs with matching globs.
- Remove the file's index entries from the config owner's `.indexes/` directory using the file_hash (already deterministic from full path).

### Query Behavior

No changes needed. Queries are already scoped to a directory path and load indexes from `{path}/.indexes/`. Since glob-indexed files store their indexes at the config owner's `.indexes/`, querying `/sessions/` finds all results. The query response already returns full file paths via `FileRecord.path`.

## 2. Plural Source Resolver

### API Change

Rename `resolve_source` to `resolve_sources`. Return type changes from `Option<Vec<u8>>` to `Vec<Vec<u8>>`.

### Source Path Segment Types

| Segment | Current Value | Behavior |
|---------|--------------|----------|
| `"name"` (non-empty, no `/` delimiters) | Object | Key lookup → single result |
| `0` (integer) | Array | Index lookup → single result |
| `""` (empty string) | Array | Fan out: iterate ALL elements, continue path on each |
| `""` (empty string) | Object | Fan out: iterate ALL values, continue path on each |
| `"/pattern/"` or `"/pattern/flags"` | Array | Fan out: iterate elements whose JSON stringification matches regex |
| `"/pattern/"` or `"/pattern/flags"` | Object | Fan out: iterate values whose keys match regex |

Supported regex flags: `i` (case-insensitive).

Regex segments are detected by: starts with `"/"`, contains at least one more `"/"`. The pattern is between the first and last `/`, flags are after the last `/`.

### Fan-Out Behavior

Each fan-out step produces multiple "current values." Remaining path segments are applied to each independently. Results accumulate across all branches.

**Example:** `{"comments": [{"text": "hello"}, {"text": "world"}]}`

`source: ["comments", "", "text"]` produces:
- Branch 1: `comments[0].text` → `"hello"`
- Branch 2: `comments[1].text` → `"world"`
- Result: `["hello", "world"]` → 2 index entries for the same file

**Example:** `{"tag_color": "red", "tag_size": "large", "name": "foo"}`

`source: ["/^tag_/", ""]` produces:
- Branch 1: key `tag_color` matches → value `"red"`
- Branch 2: key `tag_size` matches → value `"large"`
- Result: `["red", "large"]` → 2 index entries

### Indexing Pipeline Change

For each value in the `Vec<Vec<u8>>` returned by `resolve_sources`, insert a separate `IndexEntry` with the same `file_hash`. One file can produce N index entries for a single field.

On reindex/update: remove ALL existing entries for the file_hash first, then insert the new set. This handles cases where an array element was added or removed.

### Backward Compatibility

Existing configs with simple string sources like `"source": ["metadata", "author"]` return a `Vec` of one element. No behavioral change for existing users.

The old `resolve_source` function is removed and replaced by `resolve_sources`. All callers updated.

## Files Affected

| File | Change |
|------|--------|
| `aeordb-lib/src/engine/index_config.rs` | Add `glob: Option<String>` to `PathIndexConfig`, parse from JSON |
| `aeordb-lib/src/engine/source_resolver.rs` | Rewrite to `resolve_sources` returning `Vec<Vec<u8>>`, add empty-string and regex segment handling |
| `aeordb-lib/src/engine/indexing_pipeline.rs` | Use `resolve_sources`, insert multiple entries per field, ancestor config discovery with glob matching, store indexes at config owner directory |
| `aeordb-lib/src/engine/task_worker.rs` | Reindex with `list_directory_recursive` + glob when config has glob field |
| `aeordb-lib/src/engine/index_store.rs` | `insert` method may need batch variant for multiple entries; `remove_file` already removes by file_hash (no change needed) |
| `aeordb-lib/src/engine/directory_ops.rs` | Deletion path: check ancestor configs for glob-matched cleanup |
