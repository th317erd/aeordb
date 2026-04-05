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

    // Detect if it looks like source code (has common patterns)
    let has_braces = text.contains('{') && text.contains('}');
    let has_imports = text.contains("import ") || text.contains("use ") || text.contains("#include");
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
