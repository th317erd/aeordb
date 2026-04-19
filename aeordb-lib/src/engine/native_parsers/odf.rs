/// Native ODF (ODT/ODS) parser.
///
/// Ported from `aeordb-plugin-parser-odf`.

use std::io::{Cursor, Read};

/// MIME types for supported ODF formats.
const MIMETYPE_ODT: &str = "application/vnd.oasis.opendocument.text";
const MIMETYPE_ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";

pub fn parse(data: &[u8], filename: &str, _content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    let cursor = Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("not a valid ZIP archive: {}", e))?;

    // Read mimetype file to detect format
    let mimetype = read_zip_entry(&mut archive, "mimetype")
        .map_err(|e| format!("failed to read mimetype: {}", e))?;
    let mimetype = mimetype.trim().to_string();

    let format = match mimetype.as_str() {
        MIMETYPE_ODT => "odt",
        MIMETYPE_ODS => "ods",
        _ => return Err(format!("unsupported ODF mimetype: {}", mimetype)),
    };

    // Extract text from content.xml
    let content_xml = read_zip_entry(&mut archive, "content.xml")
        .map_err(|e| format!("failed to read content.xml: {}", e))?;
    let extracted_text = strip_xml_tags(&content_xml);

    // Parse metadata from meta.xml (optional -- some ODF files may lack it)
    let meta_xml = read_zip_entry(&mut archive, "meta.xml").ok();

    let title = meta_xml
        .as_deref()
        .and_then(|xml| extract_element_text(xml, "dc:title"));
    let author = meta_xml
        .as_deref()
        .and_then(|xml| extract_element_text(xml, "dc:creator"));
    let subject = meta_xml
        .as_deref()
        .and_then(|xml| extract_element_text(xml, "dc:subject"));
    let description = meta_xml
        .as_deref()
        .and_then(|xml| extract_element_text(xml, "dc:description"));
    let created = meta_xml
        .as_deref()
        .and_then(|xml| extract_element_text(xml, "meta:creation-date"));
    let modified = meta_xml
        .as_deref()
        .and_then(|xml| extract_element_text(xml, "dc:date"));

    let keywords = meta_xml
        .as_deref()
        .map(extract_keywords)
        .unwrap_or_default();

    let statistics = meta_xml.as_deref().map(extract_document_statistics);

    let mut metadata = serde_json::json!({
        "filename": filename,
        "content_type": mimetype,
        "size": size,
        "format": format,
    });

    if let Some(ref author_value) = author {
        metadata["author"] = serde_json::Value::String(author_value.clone());
    }
    if let Some(ref subject_value) = subject {
        metadata["subject"] = serde_json::Value::String(subject_value.clone());
    }
    if let Some(ref description_value) = description {
        metadata["description"] = serde_json::Value::String(description_value.clone());
    }
    if !keywords.is_empty() {
        metadata["keywords"] = serde_json::json!(keywords);
    }
    if let Some(ref created_value) = created {
        metadata["created"] = serde_json::Value::String(created_value.clone());
    }
    if let Some(ref modified_value) = modified {
        metadata["modified"] = serde_json::Value::String(modified_value.clone());
    }

    if let Some(ref statistics_map) = statistics {
        for (key, value) in statistics_map {
            metadata[key] = serde_json::json!(value);
        }
    }

    Ok(serde_json::json!({
        "text": extracted_text,
        "title": title.unwrap_or_default(),
        "metadata": metadata,
    }))
}

/// Read a named entry from a ZIP archive and return its contents as a String.
fn read_zip_entry(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<String, String> {
    let mut file = archive
        .by_name(name)
        .map_err(|e| format!("entry '{}' not found: {}", name, e))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|e| format!("failed to read '{}': {}", name, e))?;
    Ok(contents)
}

/// Strip all XML tags from content, producing plain text.
fn strip_xml_tags(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut inside_tag = false;

    for character in content.chars() {
        if character == '<' {
            inside_tag = true;
        } else if character == '>' {
            inside_tag = false;
            if !result.ends_with(' ') && !result.ends_with('\n') {
                result.push(' ');
            }
        } else if !inside_tag {
            result.push(character);
        }
    }

    collapse_whitespace(&result)
}

fn collapse_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut previous_was_whitespace = true;

    for character in text.chars() {
        if character.is_whitespace() {
            if !previous_was_whitespace {
                result.push(' ');
                previous_was_whitespace = true;
            }
        } else {
            result.push(character);
            previous_was_whitespace = false;
        }
    }

    if result.ends_with(' ') {
        result.pop();
    }

    result
}

fn extract_element_text(xml: &str, tag_name: &str) -> Option<String> {
    let open_tag = format!("<{}", tag_name);
    let close_tag = format!("</{}>", tag_name);

    let open_start = xml.find(&open_tag)?;
    let tag_end = xml[open_start..].find('>')? + open_start + 1;
    let close_start = xml[tag_end..].find(&close_tag)? + tag_end;

    let raw = &xml[tag_end..close_start];
    Some(raw.trim().to_string())
}

fn extract_keywords(xml: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let open_tag = "<meta:keyword>";
    let close_tag = "</meta:keyword>";
    let mut search_from = 0;

    while let Some(open_position) = xml[search_from..].find(open_tag) {
        let absolute_open = search_from + open_position;
        let content_start = absolute_open + open_tag.len();

        if let Some(close_position) = xml[content_start..].find(close_tag) {
            let absolute_close = content_start + close_position;
            let keyword = xml[content_start..absolute_close].trim().to_string();
            if !keyword.is_empty() {
                keywords.push(keyword);
            }
            search_from = absolute_close + close_tag.len();
        } else {
            break;
        }
    }

    keywords
}

fn extract_document_statistics(xml: &str) -> Vec<(String, u64)> {
    let mut statistics = Vec::new();

    let tag_start = match xml.find("<meta:document-statistic") {
        Some(position) => position,
        None => return statistics,
    };

    let tag_end = match xml[tag_start..].find('>') {
        Some(position) => tag_start + position + 1,
        None => return statistics,
    };

    let tag_content = &xml[tag_start..tag_end];

    let stat_attributes = [
        ("meta:page-count", "page_count"),
        ("meta:paragraph-count", "paragraph_count"),
        ("meta:word-count", "word_count"),
        ("meta:character-count", "character_count"),
        ("meta:table-count", "table_count"),
    ];

    for (attribute_name, output_key) in &stat_attributes {
        if let Some(value) = extract_xml_attribute(tag_content, attribute_name) {
            if let Ok(number) = value.parse::<u64>() {
                statistics.push((output_key.to_string(), number));
            }
        }
    }

    statistics
}

fn extract_xml_attribute(tag: &str, attribute_name: &str) -> Option<String> {
    let search = format!("{}=\"", attribute_name);
    let attr_start = tag.find(&search)?;
    let value_start = attr_start + search.len();
    let value_end = tag[value_start..].find('"')? + value_start;
    Some(tag[value_start..value_end].to_string())
}
