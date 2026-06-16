# Search Hit Locators for Agent Range Fetch

Date: 2026-06-15
Status: implemented MVP
Priority: high for agent workflows

## Problem

AeorDB's current `POST /files/search` response tells callers which files matched, their score, metadata, and which indexed fields matched. It does not tell callers where inside the matched file or indexed value the query hit occurred.

That is fine for human file-browser search, but it is not enough for agent workflows. Agents regularly search very large frames, transcripts, tool outputs, logs, rendered pages, and generated artifacts. When a search result only says "this file matched", the agent must either fetch the whole document or guess a range. That breaks down once the document is multi-megabyte or intentionally kept out-of-context.

Kikx currently works around this for some tool outputs with its own `output-grep` and `exec-grep` tools. Those return `lineNumber`, a line-start `byteOffset`, the matched text, and the whole matching line. This is useful, but it is local to Kikx, line-oriented only, and not a universal AeorDB search-location contract.

AeorDB should return optional hit locators from search. A locator tells the caller exactly what range to fetch next.

## Current AeorDB Behavior

Documented `POST /files/search` response shape:

```json
{
  "results": [
    {
      "path": "/users/alice.json",
      "score": 0.95,
      "matched_by": ["@filename"],
      "source": "/",
      "size": 256,
      "content_type": "application/json",
      "created_at": 1775968398000,
      "updated_at": 1775968398000
    }
  ],
  "has_more": false,
  "total_count": 1
}
```

The engine `SearchResult` currently contains:

- `path`
- `score`
- `matched_by`
- `source_dir`
- `size`
- `content_type`
- `created_at`
- `updated_at`

There is no `matches`, `locations`, `spans`, `snippets`, `byte_range`, `line_range`, `char_range`, `json_pointer`, or equivalent field.

## Required Capability

Search callers need a universal, typed location format that can describe:

- plain text hits
- JSON field hits
- virtual metadata hits such as `@filename` or `@path`
- parser-extracted text hits from PDFs, HTML, Markdown, DOCX, images/OCR, etc.
- tool-output hits stored as serialized text
- binary files where the hit is in a derived text layer rather than raw bytes

The location must give enough information for the next call to fetch only the relevant range.

## Bot-First Workflow

The primary workflow is not a human search results page. It is a bot/tool loop:

1. The bot searches indexed fields to find candidate files.
2. The bot asks AeorDB to generate bounded hit locators for the returned candidates.
3. AeorDB returns snippets, exact ranges when possible, and stable file identity anchors.
4. The bot batch-fetches only the selected ranges.

This means the locator response is part of a machine contract. It must be deterministic, self-describing, and safe to use in a follow-up request without fetching the whole file.

Bot-specific requirements:

- Search must remain cheap by default. Locator generation is opt-in.
- Locator generation must run after candidate selection/pagination, not during index lookup.
- Locators must include enough snippet text for quick triage; snippets are often the whole payload a bot needs.
- Locators must include stable file identity fields so a follow-up range fetch can detect stale results.
- Range fetch must support batching because a bot commonly follows several locators at once.
- Partial locator generation must be explicit. Bots can handle partial answers when the response clearly says what was skipped or truncated.
- Approximate positions are dangerous. If the engine cannot produce an exact range, it should either omit the range or mark it clearly as approximate.

## Current Range-Fetch Status

AeorDB currently has two related capabilities, but not the exact bot workflow:

- `POST /files/fetch` is batch, but whole-file only. It accepts many paths and returns full file bodies as JSON strings.
- The bundled `extract` plugin can fetch a single file line or character range through the streaming plugin host extraction function.

Gaps:

- no first-class HTTP batch range fetch endpoint
- no per-item range request shape for multiple ranges from the same file
- no byte-range extraction in the existing `extract` plugin surface
- no JSON-pointer extraction as a first-class range fetch mode
- no stale-file guard such as `if_content_hash`

The existing streaming extraction implementation should be reused/factored. The missing work is the bot-friendly API shape and byte/json-pointer modes.

## Proposed Request Extensions

Add optional fields to `/files/search` and `/files/query` where text/fuzzy operators are used:

```json
{
  "query": "panic: unwrap",
  "path": "/kikx/tool-outputs",
  "limit": 20,
  "include_matches": true,
  "max_matches_per_result": 5,
  "snippet_chars": 160,
  "match_context_lines": 2,
  "max_locator_scan_bytes": 268435456,
  "location_spaces": ["stored-file", "extracted-text", "field-value", "metadata"]
}
```

Field meanings:

- `include_matches`: opt in to hit locator generation. Default false for compatibility/performance.
- `max_matches_per_result`: cap per result so one huge file cannot dominate response size.
- `snippet_chars`: max characters of preview text around each hit.
- `match_context_lines`: optional line context for text coordinate spaces.
- `max_locator_scan_bytes`: optional caller-requested cap for request-time locator scans. The server clamps this to an admin/server maximum and reports truncation when hit.
- `location_spaces`: caller preference for which coordinate spaces should be returned.

The engine may return fewer locators than requested when an index can prove the file matched but cannot cheaply reconstruct exact hit positions.

## Proposed Response Shape

Each search result gains an optional `matches` array:

```json
{
  "path": "/kikx/tool-outputs/ABC123/result.txt",
  "score": 0.93,
  "matched_by": ["contentText"],
  "source": "/kikx/tool-outputs",
  "size": 8429911,
  "content_type": "text/plain; charset=utf-8",
  "created_at": 1775968398000,
  "updated_at": 1775968398000,
  "content_hash": "4a1f...",
  "matches": [
    {
      "id": "m_0001",
      "query": "panic: unwrap",
      "matched_text": "panic: unwrap",
      "score": 1.0,
      "field": "contentText",
      "source": {
        "type": "stored-file",
        "mime_type": "text/plain",
        "encoding": "utf-8"
      },
      "range": {
        "byte": { "start": 184923, "end": 184936, "unit": "utf8-byte", "basis": "stored-file" },
        "char": { "start": 181004, "end": 181017, "unit": "unicode-scalar", "basis": "stored-file-text" },
        "line": { "start": 3910, "end": 3910, "unit": "line", "basis": "stored-file-text" },
        "column": { "start": 18, "end": 31, "unit": "unicode-scalar", "basis": "line" }
      },
      "fetch": {
        "byte_range": { "start": 184000, "end": 186000 },
        "line_range": { "start": 3908, "end": 3912 }
      },
      "snippet": {
        "text": "... previous context panic: unwrap next context ...",
        "highlight": [{ "start": 21, "end": 34, "unit": "unicode-scalar" }]
      },
      "confidence": "exact"
    }
  ],
  "matches_truncated": false,
  "locator_status": "complete"
}
```

## Universal Locator Schema

A match locator should be self-describing.

```ts
type SearchHitLocator = {
  id: string;
  query?: string;
  matched_text?: string;
  score?: number;
  field?: string;
  operator?: string;
  source: LocatorSource;
  range?: LocatorRangeSet;
  fetch?: LocatorFetchHints;
  snippet?: LocatorSnippet;
  confidence?: "exact" | "derived" | "approximate";
  scan_status?: "complete" | "partial" | "unsupported";
  notes?: string[];
};
```

### File Identity Anchors

Each result that includes locators should expose enough identity information for a follow-up range fetch to detect stale locators:

```ts
type FileIdentity = {
  path: string;
  content_hash?: string;
  file_record_hash?: string;
  size: number;
  updated_at: number;
};
```

The follow-up range fetch should accept `if_content_hash` and/or `if_updated_at`. If the file no longer matches, return a per-item stale/conflict response instead of silently fetching the wrong range.

### Locator Source

```ts
type LocatorSource =
  | {
      type: "stored-file";
      mime_type?: string;
      encoding?: string;
    }
  | {
      type: "field-value";
      field: string;
      json_pointer?: string;
      source_path?: string[];
      value_type?: "string" | "number" | "boolean" | "object" | "array" | "null";
    }
  | {
      type: "metadata";
      field: string; // e.g. "@filename", "@path", "@hash", "@content_type"
    }
  | {
      type: "extracted-text";
      extractor: string; // e.g. "html", "pdf", "ocr", "markdown", "docx"
      layer_id?: string;
      mime_type?: string;
      encoding?: string;
    };
```

### Locator Ranges

All ranges are half-open: `start` inclusive, `end` exclusive, except line ranges where `end` is inclusive by convention because users and agents naturally request line ranges that way.

```ts
type LocatorRangeSet = {
  byte?: {
    start: number;
    end: number;
    unit: "byte" | "utf8-byte";
    basis: "stored-file" | "extracted-text" | "field-value";
  };
  char?: {
    start: number;
    end: number;
    unit: "unicode-scalar";
    basis: "stored-file-text" | "extracted-text" | "field-value";
  };
  line?: {
    start: number; // 1-based
    end: number;   // 1-based inclusive
    unit: "line";
    basis: "stored-file-text" | "extracted-text" | "field-value";
  };
  column?: {
    start: number; // 0-based within line
    end: number;   // 0-based exclusive within line
    unit: "unicode-scalar";
    basis: "line";
  };
  json_pointer?: string;
};
```

Rationale:

- `byte` ranges are ideal for HTTP `Range` reads and raw AeorDB file reads.
- `char` ranges are ideal for UTF-8 extraction APIs.
- `line` ranges are ideal for source code, logs, Markdown, transcripts, and agent-visible instructions.
- `json_pointer` is needed when the match is in parsed JSON and byte/line positions are unavailable.

### Fetch Hints

The engine should tell agents the next useful fetch range, not merely the exact hit span.

```ts
type LocatorFetchHints = {
  byte_range?: { start: number; end: number };
  char_range?: { start: number; end: number };
  line_range?: { start: number; end: number };
  json_pointer?: string;
  preferred?: "byte_range" | "char_range" | "line_range" | "json_pointer";
};
```

For example, if the exact match is on line 3910, `fetch.line_range` might be `{ "start": 3908, "end": 3912 }`.

### Snippet

```ts
type LocatorSnippet = {
  text: string;
  mime_type?: "text/plain" | "text/markdown" | "text/html";
  highlight?: Array<{
    start: number;
    end: number;
    unit: "unicode-scalar";
  }>;
  truncated_before?: boolean;
  truncated_after?: boolean;
};
```

The snippet is for display and quick agent triage. It is not the authoritative location; `range` and `fetch` are authoritative.

## Proposed Batch Range Fetch

Extend `POST /files/fetch` with a new `items` request form, or add `POST /files/fetch-ranges` if keeping the legacy endpoint simpler is preferred. The important part is that range fetch is first-class and batch-oriented.

Request:

```json
{
  "items": [
    {
      "id": "m_0001",
      "path": "/kikx/tool-outputs/ABC/result.txt",
      "if_content_hash": "4a1f...",
      "range": {
        "mode": "lines",
        "start": 3908,
        "end": 3912
      },
      "max_bytes": 65536
    },
    {
      "id": "m_0002",
      "path": "/kikx/tool-outputs/ABC/result.txt",
      "if_content_hash": "4a1f...",
      "range": {
        "mode": "bytes",
        "start": 184000,
        "end": 186000
      }
    },
    {
      "id": "m_0003",
      "path": "/kikx/session.json",
      "if_content_hash": "9b2c...",
      "range": {
        "mode": "json_pointer",
        "pointer": "/messages/12/content"
      }
    }
  ],
  "max_bytes": 1048576,
  "continue_on_error": true
}
```

Response:

```json
{
  "items": [
    {
      "id": "m_0001",
      "path": "/kikx/tool-outputs/ABC/result.txt",
      "content_hash": "4a1f...",
      "range": { "mode": "lines", "start": 3908, "end": 3912 },
      "content_type": "text/plain",
      "content": "...",
      "truncated": false
    },
    {
      "id": "m_0002",
      "path": "/kikx/tool-outputs/ABC/result.txt",
      "status": "stale",
      "message": "File content hash changed"
    }
  ],
  "has_errors": true
}
```

Range modes:

- `lines`: 1-based inclusive line range. Treat `\r\n` as one line break while preserving returned line endings.
- `chars`: 0-based Unicode scalar range, end-exclusive.
- `bytes`: 0-based byte range, end-exclusive.
- `json_pointer`: RFC 6901 JSON pointer. This may load the JSON document in full initially.

Response shape should be item-array based rather than path-keyed because a bot may request multiple ranges from the same file.

## Examples

### Metadata Hit

Search hit on filename:

```json
{
  "id": "m_0001",
  "matched_text": "crash",
  "field": "@filename",
  "source": {
    "type": "metadata",
    "field": "@filename"
  },
  "range": {
    "char": { "start": 0, "end": 5, "unit": "unicode-scalar", "basis": "field-value" }
  },
  "fetch": {
    "preferred": "json_pointer",
    "json_pointer": "/@filename"
  },
  "confidence": "exact"
}
```

There may be no stored-file byte range because the match is in metadata, not file content.

### JSON Field Hit

Search hit in a parsed JSON frame:

```json
{
  "id": "m_0002",
  "matched_text": "ToolExecutionService",
  "field": "contentText",
  "source": {
    "type": "field-value",
    "field": "contentText",
    "json_pointer": "/content/text",
    "value_type": "string"
  },
  "range": {
    "char": { "start": 812, "end": 832, "unit": "unicode-scalar", "basis": "field-value" },
    "json_pointer": "/content/text"
  },
  "fetch": {
    "preferred": "json_pointer",
    "json_pointer": "/content/text"
  },
  "confidence": "exact"
}
```

If the JSON parser has source-span tracking, add stored-file byte/line positions too. If not, the JSON pointer and field-local char range are still useful.

### Extracted PDF/OCR Hit

```json
{
  "id": "m_0003",
  "matched_text": "quarterly revenue",
  "field": "bodyText",
  "source": {
    "type": "extracted-text",
    "extractor": "pdf",
    "layer_id": "page-text",
    "mime_type": "application/pdf",
    "encoding": "utf-8"
  },
  "range": {
    "char": { "start": 11820, "end": 11837, "unit": "unicode-scalar", "basis": "extracted-text" },
    "line": { "start": 214, "end": 214, "unit": "line", "basis": "extracted-text" }
  },
  "fetch": {
    "preferred": "char_range",
    "char_range": { "start": 11600, "end": 12100 }
  },
  "confidence": "derived"
}
```

For document formats where original byte offsets are meaningless, the extracted-text coordinate space is the correct basis.

## Engine Implementation Notes

There are two separate tasks:

1. Search proves which documents/fields match.
2. Locator generation reopens the matching value/text and computes exact spans.

Do not force every index to store span postings immediately. A practical first version can compute locators after candidate selection:

1. Run existing search/query to get top candidate files.
2. For each result, inspect `matched_by`.
3. Load only the matched field value or text layer when possible.
4. Run exact regex/literal matching against that value/text.
5. Return bounded locators.

This is intentionally request-time work. AeorDB should not persist positional indexes as the default design. The indexes identify candidate files; locator generation scans only the bounded candidate page when the caller asks for positions.

If high-volume workloads eventually prove that persisted positions are required, that should be a separate opt-in index type, not the baseline search architecture.

## Storage/Indexing Considerations

For precise locators, index entries need enough reverse metadata to locate source values:

- index field name
- source path from index config
- JSON pointer or source path resolved for each indexed value
- text layer identity for parser-produced text
- optional source map from extracted text offsets to original file positions

For JSON, exact byte/line offsets require a parser that preserves source spans. Without source spans, return:

- `source.type = "field-value"`
- `json_pointer`
- field-local `char` range
- no stored-file `byte` range

For plain text files, locators can be exact without extra indexing by scanning selected candidate text.

## API Compatibility

This should be additive:

- Existing clients see no changes unless they request `include_matches`.
- Existing result fields remain unchanged.
- The engine may omit `matches` when no locator can be computed.
- The engine should set `matches_truncated: true` when caps cut off additional matches.
- Batch range fetch should preserve the legacy `/files/fetch` `paths` request shape if the existing endpoint is extended.
- Range fetch should use per-item errors for bot workflows when `continue_on_error` is true.

## Minimum Acceptable MVP

For Kikx/agent usefulness, MVP support should cover:

1. `include_matches: true`
2. text/plain and application/json
3. line number, line-start byte offset, match byte/char span within the line
4. snippet text with highlight span
5. `fetch.line_range` and `fetch.byte_range`
6. metadata hits represented as `source.type = "metadata"`
7. result-level `content_hash`, `updated_at`, and `size` anchors
8. batch range fetch for `lines`, `chars`, and `bytes`
9. stale-file detection through `if_content_hash`
10. explicit `matches_truncated`, `scan_status`, and `locator_status`

MVP result example:

```json
{
  "path": "/kikx/tool-outputs/ABC/result.txt",
  "score": 1.0,
  "matched_by": ["outputText"],
  "matches": [
    {
      "id": "m_0001",
      "matched_text": "ENOENT",
      "source": { "type": "stored-file", "mime_type": "text/plain", "encoding": "utf-8" },
      "range": {
        "byte": { "start": 92310, "end": 92316, "unit": "utf8-byte", "basis": "stored-file" },
        "line": { "start": 1204, "end": 1204, "unit": "line", "basis": "stored-file-text" },
        "column": { "start": 44, "end": 50, "unit": "unicode-scalar", "basis": "line" }
      },
      "fetch": {
        "preferred": "line_range",
        "line_range": { "start": 1200, "end": 1210 },
        "byte_range": { "start": 91800, "end": 92800 }
      },
      "snippet": {
        "text": "Error: ENOENT: no such file or directory",
        "highlight": [{ "start": 7, "end": 13, "unit": "unicode-scalar" }]
      },
      "confidence": "exact"
    }
  ],
  "matches_truncated": false
}
```

## Why This Matters for Agents

Agents need to search large stores and then fetch only the relevant range. Without hit locators:

- they waste context reading entire files
- they may miss relevant context hidden deep in a large result
- they cannot reliably cite or inspect the local area around a match
- they repeatedly call broad search instead of narrowing through range reads

With hit locators:

- search becomes an index-discovery step
- range fetch becomes deterministic
- tool output retrieval remains cheap
- large logs/transcripts become practical for agent workflows

## Implementation Phases

### Phase 1: Bot-Safe Range Fetch

- Factor the existing plugin host line/char extraction into an engine-native helper.
- Add byte-range extraction over `EngineFileStream`.
- Add first-class batch range fetch with per-item ids and per-item errors.
- Add `if_content_hash` stale checks.
- Keep existing `/files/fetch` path-list behavior unchanged.

### Phase 2: Locator Generation

- Preserve matched field and operator provenance through query/search execution.
- Add locator generation options to `/files/search` and `/files/query`.
- Generate exact locators for metadata, indexed field values, and UTF-8 stored files.
- Return snippets and fetch hints with explicit truncation/status fields.

### Phase 3: JSON and Extracted Text Polish

- Resolve simple index `source` paths to JSON pointers where possible.
- Add `json_pointer` batch range fetch.
- Mark parser/native extracted text as `derived` unless there is a reliable source map.

### Phase 4: Hardening and Docs

- Add server/admin caps for max range items, max returned bytes, max locator scan bytes, and max matches per result.
- Add unit tests for CRLF, UTF-8 boundary handling, stale file checks, duplicate paths with multiple ranges, and partial errors.
- Add real-world tests using a temp database with large text, JSON, and repeated same-file range requests.
- Update API docs, plugin docs, and agent examples.

## Implemented MVP Notes

Implemented in the AeorDB HTTP surface:

- `/files/fetch` keeps the legacy `paths` whole-file shape and adds an `items` range-fetch shape.
- Range fetch supports `lines`, `chars`, `bytes`, and `json_pointer`.
- Range fetch supports per-item `id`, `if_content_hash`, `if_updated_at`, `max_bytes`, and `continue_on_error`.
- `/files/search` and `/files/query` accept `include_matches`, `max_matches_per_result`, `snippet_chars`, `match_context_lines`, and `max_locator_scan_bytes`.
- Locator generation runs only after pagination and permission filtering.
- JSON indexed field locators use `source.type = "field-value"` and return a `fetch.json_pointer` hint when the field resolves to a JSON Pointer.
- Metadata locators use `source.type = "metadata"`.
- UTF-8 fallback locators use `source.type = "stored-file"` with byte, char, line, column, snippet, and line/byte fetch hints.
- Result-level locator responses include `content_hash`, `matches`, `matches_truncated`, and `locator_status`.

Known follow-up:

- Parser-derived text locators still need source-map support before they can safely report original-file byte ranges.
- JSON object/array locators currently provide field-local character ranges and JSON Pointer fetch hints, not original-file byte spans.
