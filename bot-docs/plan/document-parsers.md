# Configurable Document Parsers — Implementation Spec

**Date:** 2026-04-04
**Status:** Approved

---

## 1. Overview

A three-layer pipeline for extracting and indexing structured data from any file format:

```
raw bytes → Parser (format → JSON) → Source Path Resolution (JSON → field values) → Indexing Pipeline
```

- **Parser**: WASM/native plugin. Converts format-specific bytes (PDF, image, XML, etc.) into a JSON object. Community-provided.
- **Source Path Resolution**: Built-in JSON traversal using array-of-segments syntax. Extracts specific values from the parser's JSON output. Configurable per index.
- **Indexing Pipeline**: Existing system (unchanged). Takes `(name, value_bytes)` pairs, runs through converters (string, trigram, phonetic, etc.), stores in NVT indexes.

For JSON files with no explicit parser configured, the parser step is skipped — raw data is already JSON. Users can override this by setting `"parser"` in the config (e.g., for JSONC, JSON5, or custom comment-stripped JSON).

Error handling: file is ALWAYS stored regardless of parse/map/index errors. Failures are logged to `{directory}/.logs/` if logging is enabled.

---

## 2. Configuration Format

Lives at `{directory}/.config/indexes.json` (extended from existing format).

### Full example (parsed file with mapping):

```json
{
  "parser": "pdf-extractor",
  "parser_memory_limit": "256mb",
  "logging": true,
  "indexes": [
    { "name": "title",   "source": ["metadata", "title"],  "type": ["string", "trigram"] },
    { "name": "author",  "source": ["metadata", "author"], "type": ["phonetic", "trigram"] },
    { "name": "content", "source": ["text"],                "type": "trigram" },
    { "name": "pages",   "source": ["metadata", "pages", "length"], "type": "u64" }
  ]
}
```

### Plain JSON file (no parser, no source — existing behavior):

```json
{
  "indexes": [
    { "name": "name", "type": ["string", "trigram", "phonetic"] },
    { "name": "age",  "type": "u64" }
  ]
}
```

### With plugin mapper (escape hatch):

```json
{
  "parser": "pdf-extractor",
  "indexes": [
    { "name": "summary", "source": "plugin:my-mapper", "type": "trigram" }
  ]
}
```

### Configuration fields:

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `parser` | string | No | none (raw data is JSON) | Name of deployed parser plugin |
| `parser_memory_limit` | string | No | `"256mb"` | WASM memory limit for parser (e.g., `"64mb"`, `"512mb"`) |
| `logging` | bool | No | `false` | Enable error logging to `.logs/` |
| `indexes` | array | Yes | — | Index definitions |

### Index definition fields:

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | Yes | — | Index field name (opaque string, used in `.indexes/{name}.{strategy}.idx`) |
| `source` | array or string | No | `["name"]` | Path segments into parser output, or `"plugin:name"` for plugin mapper |
| `type` | string or array | Yes | — | One or more converter strategies |
| `min` | number | No | — | Range hint for numeric converters |
| `max` | number | No | — | Range hint for numeric converters |

### Defaults:

- Missing `source` defaults to `["name"]` (the index name is the JSON key)
- Missing `parser` means raw data is treated as JSON
- Missing `logging` means logging is off

---

## 3. Parser Plugin Interface

### What a parser does:

Transforms format-specific bytes into a JSON object. A PDF parser might return:

```json
{
  "text": "The full extracted body text...",
  "metadata": {
    "title": "Quarterly Report",
    "author": "Jane Smith",
    "created": "2026-01-15",
    "page_count": 42
  }
}
```

An image parser might return:

```json
{
  "metadata": {
    "width": 1920,
    "height": 1080,
    "format": "JPEG",
    "exif": {
      "camera": "Canon EOS R5",
      "gps": {"lat": 37.7749, "lng": -122.4194},
      "date_taken": "2026-03-15T14:30:00Z"
    }
  }
}
```

### WASM interface:

Same as existing plugin protocol:
- Host writes raw file bytes at offset 0 in guest linear memory
- Host calls exported `handle(request_ptr: i32, request_len: i32) -> i64`
- Return value: `(response_ptr << 32) | response_len` (packed i64)
- Response bytes must be valid UTF-8 JSON

### Memory limit:

- Default for parser plugins: 256MB (up from 16MB for regular plugins)
- Configurable per directory via `"parser_memory_limit"` in config
- Regular (non-parser) plugins retain the 16MB default

### Parser deployment:

Parsers are deployed as regular plugins via the existing plugin system:

```
PUT /{db}/{schema}/{table}/_deploy?name=pdf-extractor&plugin_type=wasm
Body: WASM binary
```

Referenced by name in the index config's `"parser"` field.

### Parser selection at store time:

1. Check `"parser"` in directory's `.config/indexes.json` — explicit config always wins, even for JSON files
2. If no explicit parser, check content-type against a global parser registry at `/.config/parsers.json`:
   ```json
   {
     "application/pdf": "pdf-extractor",
     "image/jpeg": "image-metadata",
     "image/png": "image-metadata",
     "audio/mpeg": "audio-metadata"
   }
   ```
3. If no matching parser and content-type is `application/json` (or unset) — skip parsing, use raw data as JSON
4. If no matching parser and content-type is not JSON — no parsing, no indexing (file is still stored)

### Parser failure modes:

| Failure | Behavior |
|---------|----------|
| Parser plugin not deployed | Skip parsing, skip indexing, store file, log if enabled |
| Parser returns non-JSON | Skip indexing, store file, log if enabled |
| Parser returns empty/null | Skip indexing, store file, log if enabled |
| Parser exceeds memory limit | WASM trap, skip indexing, store file, log if enabled |
| Parser exceeds fuel limit | WASM trap, skip indexing, store file, log if enabled |

In all cases: the file is stored. Data is never lost due to parser failures.

---

## 4. Source Path Resolution

The `"source"` field in each index definition is an array of JSON segments that walks the parser's output.

### Resolution algorithm:

```rust
fn resolve_source(json: &Value, source: &[Value]) -> Option<Value> {
    let mut current = json;
    for segment in source {
        match segment {
            Value::String(key) => {
                // Object key lookup
                current = current.get(key)?;
            }
            Value::Number(n) => {
                let idx = n.as_u64()? as usize;
                if current.is_array() {
                    // Array index
                    current = current.get(idx)?;
                } else {
                    // Object key as string
                    current = current.get(&idx.to_string())?;
                }
            }
            _ => return None, // booleans, null, etc. — invalid segment
        }
    }
    Some(current.clone())
}
```

### Rules:

- String segment → object key lookup
- Integer segment → array index if current value is array, else object key as string (e.g., `"0"`)
- Failure at any step → field is skipped (log if enabled)
- Resolved value is converted to bytes using existing `json_value_to_bytes` logic (strings → UTF-8, numbers → big-endian, etc.)
- `.length` is not special — it's a regular string key. If the value is a JSON array, it won't have a `.length` key. Use integer indexing or a plugin mapper for array length.

### Default source:

When `"source"` is omitted, it defaults to `["name"]` — the index's `name` field is used as a direct JSON key lookup. This preserves backward compatibility with existing configs.

### Plugin mapper source:

When `"source"` is a string starting with `"plugin:"`, the named plugin is invoked instead of path resolution:

- Input: the full parser JSON output (as bytes)
- Output: the resolved field value (as bytes)
- The plugin is a regular WASM/native plugin, invoked per field per file store

---

## 5. The `.logs/` Directory

Per-directory logging for parse, map, and index errors. Stored as regular files in the database filesystem.

### Directory structure:

```
{dir}/.logs/
  system/
    indexing.log      — source resolution failures, converter errors
    parsing.log       — parser invocation failures, invalid output
  plugins/
    pdf-extractor.log — whatever the parser plugin logs via host function
    my-mapper.log     — whatever the mapper plugin logs
```

### Behavior:

- **Opt-in**: only written when `"logging": true` in the directory's config
- **Append-only**: new entries appended to existing log files
- **Permissions**: same crudlify rules as everything else in the directory
- **Versioned**: included in snapshots/forks like any other file
- **No auto-rotation**: user's responsibility to manage size (delete logs, disable logging)
- **No compaction**: each error is a separate line entry

### Log entry format:

```
{ISO-8601 timestamp} {LEVEL} {message}
```

Example:
```
2026-04-04T13:22:01Z WARN  source ["metadata","author"] not found in parser output for /people/upload.pdf
2026-04-04T13:22:02Z ERROR parser "pdf-extractor" returned invalid JSON for /people/corrupt.pdf
2026-04-04T13:22:03Z WARN  parser "pdf-extractor" not deployed, skipping parse for /people/report.pdf
```

### Plugin `log` host function:

WASM/native plugins can write to their log via a host function:

```
log(level_ptr, level_len, msg_ptr, msg_len)
```

- Writes to `{calling_directory}/.logs/plugins/{plugin_name}.log`
- Level: "INFO", "WARN", "ERROR"
- Only writes if logging is enabled for that directory
- The WASM runtime receives the calling directory path and engine reference to enable this

### Recursive guard:

Writes to `.logs/*`, `.indexes/*`, and `.config/*` paths do NOT trigger the indexing pipeline. These are internal system directories. The guard checks the path before entering the pipeline.

---

## 6. `type` as String or Array

A single index definition can specify multiple converter types:

```json
{ "name": "title", "source": ["metadata", "title"], "type": ["string", "trigram"] }
```

This expands internally to two `IndexFieldConfig` entries during deserialization — one per converter type. Both share the same `name` and `source`, producing `title.string.idx` and `title.trigram.idx`.

Single string is also accepted (existing behavior):

```json
{ "name": "title", "type": "string" }
```

Deserialization handles both:

```rust
// type is either a JSON string or array of strings
match item.get("type") {
    Some(Value::String(s)) => vec![s.clone()],
    Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
    _ => return Err(...)
}
```

---

## 7. Indexing Pipeline Changes

### Current flow (`store_file_with_indexing`):

```
1. Read .config/indexes.json
2. Detect compression
3. Store file
4. parse_json_fields(raw_data, field_names)
5. For each index config: insert into index
```

### New flow:

```
1. Read .config/indexes.json (extended format)
2. Guard: skip pipeline for .logs/*, .indexes/*, .config/* paths
3. Detect compression
4. Store file
5. Run parser (if configured):
   a. Load parser plugin by name
   b. Invoke with raw file bytes
   c. Validate output is JSON
   d. On failure: log if enabled, skip indexing, return
6. For each index config:
   a. Resolve source path against parsed JSON (or raw data if no parser)
   b. If source is "plugin:name", invoke mapper plugin instead
   c. On resolution failure: log if enabled, skip this field
   d. Expand type array into individual converters
   e. For each converter: insert_expanded into index
   f. Save index
```

### Decomposition:

Extract an `IndexingPipeline` struct from the growing `store_file_with_indexing` method:

```rust
pub struct IndexingPipeline<'a> {
    engine: &'a StorageEngine,
    plugin_manager: Option<&'a PluginManager>,
}

impl<'a> IndexingPipeline<'a> {
    pub fn run(&self, path: &str, data: &[u8], content_type: Option<&str>) -> EngineResult<()>;
    fn load_config(&self, parent: &str) -> EngineResult<Option<ExtendedIndexConfig>>;
    fn run_parser(&self, data: &[u8], parser_name: &str, memory_limit: usize) -> EngineResult<serde_json::Value>;
    fn resolve_source(&self, json: &Value, source: &[Value]) -> Option<Vec<u8>>;
    fn invoke_mapper(&self, json: &Value, plugin_name: &str) -> EngineResult<Vec<u8>>;
    fn log_error(&self, dir: &str, log_name: &str, message: &str);
}
```

`store_file_with_indexing` delegates to `IndexingPipeline::run` after storing the file.

---

## 8. Content-Type Detection

When no parser is explicitly configured, the engine falls back to content-type matching.

### Content-type sources (priority order):

1. HTTP `Content-Type` header on PUT request (already captured by engine routes)
2. Magic byte sniffing via `file-format` crate (already a dependency, not yet integrated)
3. Default: `application/octet-stream` (no parser, no indexing)

### Global parser registry:

Stored at `/.config/parsers.json`:

```json
{
  "application/pdf": "pdf-extractor",
  "image/jpeg": "image-metadata",
  "image/png": "image-metadata",
  "audio/mpeg": "audio-metadata",
  "text/xml": "xml-parser",
  "text/csv": "csv-parser"
}
```

Lookup: `content_type → parser_name`. If no match and content-type is not `application/json`, no parsing occurs.

### `application/json` default behavior:

When no parser is explicitly configured and the content-type is `application/json`, the raw data is used directly as JSON — no parser invocation. This is the default, not a hard rule. Users can override by setting `"parser"` in the directory config (e.g., for JSONC, JSON5, or any custom JSON variant that needs pre-processing).

---

## 9. WASM Runtime Changes

### Memory limit per plugin type:

| Plugin type | Default memory | Configurable |
|-------------|---------------|-------------|
| Regular (invoke) | 16 MB | No (per existing design) |
| Parser | 256 MB | Yes, via `parser_memory_limit` in config |
| Mapper | 16 MB | No (mapper input is JSON, small) |

### New host function — `log`:

```
log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32)
```

- Reads level string and message string from guest memory
- Writes to `{calling_directory}/.logs/plugins/{plugin_name}.log`
- No-op if logging is not enabled for the directory
- The WASM runtime context must include: calling directory path, plugin name, engine reference, logging flag

### Runtime context extension:

The `WasmPluginRuntime` (or the invocation call) needs additional context:

```rust
pub struct ParserContext {
    pub directory: String,
    pub plugin_name: String,
    pub logging_enabled: bool,
    pub memory_limit_bytes: usize,
}
```

Passed when invoking a parser plugin, used by the `log` host function implementation.

---

## 10. Edge Cases

### Files that should not be parsed/indexed:

- Paths matching `{dir}/.logs/*` — log files
- Paths matching `{dir}/.indexes/*` — index files
- Paths matching `{dir}/.config/*` — configuration files
- Detection: check if any path segment after the parent starts with `.logs`, `.indexes`, or `.config`

### Parser output validation:

- Must be valid UTF-8
- Must parse as JSON
- Must be a JSON object (not array, string, number, etc.)
- Violation → log if enabled, skip indexing

### Large files:

- Parser receives the full file bytes (up to the WASM memory limit)
- If the file exceeds the parser's memory limit, the WASM instantiation fails (trap)
- This is logged, file is stored without indexing
- For very large files (video, etc.), metadata parsers typically only need the first few KB — the parser itself can ignore trailing bytes

### Concurrent writes:

- Log appends are serialized through the engine's write lock (same as all file operations)
- No special concurrency handling needed — the engine is single-writer

### Empty files:

- Empty file → parser receives zero bytes → parser returns empty/error → skip indexing, store file

---

## 11. Implementation Phases

### Phase 1 — Pipeline Decomposition + Config Extension

- Extract `IndexingPipeline` from `store_file_with_indexing`
- Extend `IndexFieldConfig` with `name` (alias for `field_name`), `source`, array `type`
- Extend `PathIndexConfig` with `parser`, `parser_memory_limit`, `logging`
- Recursive guard for `.logs/*`, `.indexes/*`, `.config/*` paths
- Backward compatibility for old config format
- Tests: config parsing (new + old format), pipeline extraction, recursive guard

### Phase 2 — Source Path Resolution

- Implement `resolve_source(json, segments)` — walk JSON with array-of-segments
- Integrate into pipeline: resolve source → extract value → feed to converter
- Handle missing paths (log if enabled, skip field)
- Tests: dot paths, array indexing, integer-as-string-key, nested access, missing paths, edge cases

### Phase 3 — `.logs/` Directory

- Log writer utility: append timestamped entries to `.logs/system/*.log`
- Wire into pipeline: parsing errors → `parsing.log`, resolution errors → `indexing.log`
- Respect `"logging": true/false` config
- Tests: log creation, append, opt-in/opt-out, permissions, content format

### Phase 4 — Parser Plugin Invocation

- Extend WASM runtime with configurable memory limit
- Add `ParserContext` to invocation path
- Wire parser invocation into pipeline: load parser → invoke → validate JSON output
- Content-type fallback: integrate `file-format` crate, global parser registry at `/.config/parsers.json`
- Tests: parser invocation, memory limit, invalid output, parser not found, content-type fallback

### Phase 5 — Plugin Mapper + `log` Host Function

- Implement `"source": "plugin:name"` mapper invocation in pipeline
- Implement `log` host function in WASM runtime (writes to `.logs/plugins/{name}.log`)
- Pass `ParserContext` to WASM runtime for directory/logging context
- Tests: mapper invocation, log host function, logging disabled no-op

### Phase 6 — E2E Testing with Real Data

- Build or use a simple test parser (e.g., plain text → JSON with word count, line count)
- Test with user's real data paths:
  - PDF files from `/home/wyatt/Documents/OpenAudible/books/`
  - Documents from `/home/wyatt/Documents/Writing/`
  - Source code from `~/Projects/`
- Full pipeline: store file → parser → source resolution → indexing → query
- Stress test the plugin system under load

---

## 12. Non-Goals (Deferred)

- Streaming/chunked parser input (parsers receive full file bytes)
- Parser byte-range host functions (read_bytes)
- Built-in parsers for common formats (all parsers are plugins)
- Parser result caching (re-parse on every store)
- Async/parallel parser invocation
- Parser chaining (parser A output → parser B input)
- Content inside images/video (computer vision, transcription — much later)
