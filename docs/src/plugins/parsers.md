# Parser Plugins

Parser plugins let you transform non-JSON files (plaintext, CSV, PDF, images, etc.) into structured, queryable JSON when they are stored in AeorDB. Parsers are compiled to WebAssembly and deployed per-table, so each data collection can have its own parsing logic.

## How It Works

When a file is written to a table that has a parser configured, AeorDB automatically routes the raw bytes through the parser's WASM module. The parser receives the file data plus metadata and returns a JSON value. That JSON is then indexed by AeorDB's query engine, making the original non-JSON file fully searchable.

## Writing a Parser: Step by Step

### 1. Create a Rust Crate

```bash
cargo new my-parser --lib
cd my-parser
```

Edit `Cargo.toml`:

```toml
[package]
name = "my-parser"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
aeordb-plugin-sdk = { path = "../aeordb-plugin-sdk" }
serde_json = "1"
```

The `crate-type = ["cdylib"]` is required -- it tells the compiler to produce a dynamic library suitable for WASM.

### 2. Implement the Parse Function

Use the `aeordb_parser!` macro to generate the WASM export boilerplate. Your job is to write a function that takes a `ParserInput` and returns `Result<serde_json::Value, String>`.

```rust
use aeordb_plugin_sdk::aeordb_parser;
use aeordb_plugin_sdk::parser::*;

aeordb_parser!(parse);

fn parse(input: ParserInput) -> Result<serde_json::Value, String> {
    let text = std::str::from_utf8(&input.data)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "text": text,
        "metadata": {
            "line_count": text.lines().count(),
            "word_count": text.split_whitespace().count(),
        }
    }))
}
```

The `aeordb_parser!` macro generates:
- A global allocator for the WASM target
- A `handle(ptr, len) -> i64` export that deserializes the parser envelope, calls your function, and returns the serialized response as a packed pointer+length

You never interact with the raw WASM ABI directly.

### 3. Build for WASM

```bash
cargo build --target wasm32-unknown-unknown --release
```

The compiled module lands at:
```
target/wasm32-unknown-unknown/release/my_parser.wasm
```

### 4. Deploy the Parser

Upload the WASM binary to a table's plugin deployment endpoint:

```bash
curl -X PUT \
  http://localhost:3000/mydb/myschema/mytable/_deploy \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/wasm" \
  --data-binary @target/wasm32-unknown-unknown/release/my_parser.wasm
```

### 5. Configure Content-Type Routing

Create or update `/.config/parsers.json` to route specific content types to your parser:

```json
{
  "parsers": {
    "text/plain": "my-parser",
    "text/csv": "csv-parser",
    "application/pdf": "pdf-parser"
  }
}
```

When a file with a matching `Content-Type` is stored, AeorDB automatically invokes the corresponding parser.

### 6. Configure Indexing

Add the parser name to `indexes.json` so the parsed output is indexed:

```json
{
  "indexes": [
    {
      "field": "text",
      "type": "fulltext"
    },
    {
      "field": "metadata.word_count",
      "type": "numeric"
    }
  ]
}
```

## The `ParserInput` Struct

Your parse function receives a `ParserInput` with two fields:

| Field | Type | Description |
|-------|------|-------------|
| `data` | `Vec<u8>` | Raw file bytes (already base64-decoded from the wire envelope) |
| `meta` | `FileMeta` | Metadata about the file being parsed |

### `FileMeta` Fields

| Field | Type | Description |
|-------|------|-------------|
| `filename` | `String` | File name only (e.g., `"report.pdf"`) |
| `path` | `String` | Full storage path (e.g., `"/docs/reports/report.pdf"`) |
| `content_type` | `String` | MIME type (e.g., `"text/plain"`) |
| `size` | `u64` | Raw file size in bytes |
| `hash` | `String` | Hex-encoded content hash (may be empty) |
| `hash_algorithm` | `String` | Hash algorithm used (e.g., `"blake3_256"`) |
| `created_at` | `i64` | Creation timestamp (ms since epoch, default 0) |
| `updated_at` | `i64` | Last update timestamp (ms since epoch, default 0) |

## Real-World Example: Plaintext Parser

The built-in plaintext parser (`aeordb-parsers/plaintext`) demonstrates a production parser:

```rust
use aeordb_plugin_sdk::aeordb_parser;
use aeordb_plugin_sdk::parser::*;

aeordb_parser!(parse);

fn parse(input: ParserInput) -> Result<serde_json::Value, String> {
    let text = std::str::from_utf8(&input.data)
        .map_err(|e| format!("not valid UTF-8: {}", e))?;

    let line_count = text.lines().count();
    let word_count = text.split_whitespace().count();
    let char_count = text.chars().count();
    let byte_count = input.data.len();

    // Extract first line as a "title" (common convention for text files)
    let title = text.lines().next().unwrap_or("").trim().to_string();

    // Detect if it looks like source code
    let has_braces = text.contains('{') && text.contains('}');
    let has_imports = text.contains("import ")
        || text.contains("use ")
        || text.contains("#include");
    let looks_like_code = has_braces || has_imports;

    Ok(serde_json::json!({
        "text": text,
        "metadata": {
            "filename": input.meta.filename,
            "content_type": input.meta.content_type,
            "size": byte_count,
            "line_count": line_count,
            "word_count": word_count,
            "char_count": char_count,
        },
        "title": title,
        "looks_like_code": looks_like_code,
    }))
}
```

This parser:
- Validates UTF-8 encoding (returns an error for binary data)
- Extracts text statistics (lines, words, characters)
- Pulls the first line as a title
- Heuristically detects source code

## Error Handling

Return `Err(String)` from your parse function to signal a failure. AeorDB will store the error in the parser response and the file will not be indexed. The original file is still stored -- only parsing/indexing is skipped.

```rust
fn parse(input: ParserInput) -> Result<serde_json::Value, String> {
    if input.data.is_empty() {
        return Err("empty file".to_string());
    }
    // ...
}
```

## See Also

- [Query Plugins](query-plugins.md) -- plugins that query the database and return custom responses
- [SDK Reference](sdk-reference.md) -- complete type reference for the plugin SDK
