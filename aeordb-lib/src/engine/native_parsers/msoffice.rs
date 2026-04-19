/// Native MS Office (DOCX/XLSX) parser.
///
/// Ported from `aeordb-plugin-parser-msoffice`.

use std::io::{Cursor, Read};

/// The detected Office format of a ZIP archive.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OfficeFormat {
    Docx,
    Xlsx,
}

/// Metadata extracted from the `docProps/core.xml` file found in Office ZIP archives.
#[derive(Debug, Default)]
struct CoreProperties {
    title: Option<String>,
    creator: Option<String>,
    subject: Option<String>,
    description: Option<String>,
    keywords: Option<String>,
    last_modified_by: Option<String>,
    created: Option<String>,
    modified: Option<String>,
}

/// Parse a Microsoft Office file (DOCX or XLSX) into a queryable JSON document.
pub fn parse(data: &[u8], filename: &str, _content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    let cursor = Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("not a valid ZIP archive: {}", e))?;

    let format = detect_format(&archive)?;
    let core_properties = extract_core_properties(&mut archive);

    match format {
        OfficeFormat::Docx => parse_docx(&mut archive, filename, size, &core_properties),
        OfficeFormat::Xlsx => parse_xlsx(&mut archive, filename, size, &core_properties),
    }
}

/// Detect whether the ZIP archive is a DOCX or XLSX file by checking for known entry paths.
fn detect_format(archive: &zip::ZipArchive<Cursor<&[u8]>>) -> Result<OfficeFormat, String> {
    for index in 0..archive.len() {
        if let Some(entry) = archive.name_for_index(index) {
            if entry == "word/document.xml" {
                return Ok(OfficeFormat::Docx);
            }
        }
    }

    for index in 0..archive.len() {
        if let Some(entry) = archive.name_for_index(index) {
            if entry == "xl/workbook.xml" {
                return Ok(OfficeFormat::Xlsx);
            }
        }
    }

    Err("ZIP archive is not a recognized Office format (no word/document.xml or xl/workbook.xml found)".to_string())
}

/// Read a file entry from the ZIP archive as a UTF-8 string.
fn read_zip_entry(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<String> {
    let mut entry = archive.by_name(name).ok()?;
    let mut contents = String::new();
    entry.read_to_string(&mut contents).ok()?;
    Some(contents)
}

// ---------------------------------------------------------------------------
// DOCX parsing
// ---------------------------------------------------------------------------

fn parse_docx(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    filename: &str,
    size: u64,
    core_properties: &CoreProperties,
) -> Result<serde_json::Value, String> {
    let document_xml = read_zip_entry(archive, "word/document.xml")
        .ok_or_else(|| "DOCX file missing word/document.xml".to_string())?;

    let paragraph_count = count_tag_occurrences(&document_xml, "w:p");
    let text = strip_xml_tags(&document_xml);
    let title = core_properties
        .title
        .clone()
        .unwrap_or_default();

    Ok(serde_json::json!({
        "text": text,
        "title": title,
        "metadata": {
            "filename": filename,
            "content_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "size": size,
            "format": "docx",
            "author": core_properties.creator,
            "subject": core_properties.subject,
            "description": core_properties.description,
            "keywords": core_properties.keywords,
            "created": core_properties.created,
            "modified": core_properties.modified,
            "last_modified_by": core_properties.last_modified_by,
            "paragraph_count": paragraph_count,
            "sheet_count": serde_json::Value::Null,
        }
    }))
}

// ---------------------------------------------------------------------------
// XLSX parsing
// ---------------------------------------------------------------------------

fn parse_xlsx(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    filename: &str,
    size: u64,
    core_properties: &CoreProperties,
) -> Result<serde_json::Value, String> {
    let text = match read_zip_entry(archive, "xl/sharedStrings.xml") {
        Some(shared_strings_xml) => extract_shared_strings(&shared_strings_xml),
        None => String::new(),
    };

    let sheet_count = match read_zip_entry(archive, "xl/workbook.xml") {
        Some(workbook_xml) => count_occurrences(&workbook_xml, "<sheet "),
        None => 0,
    };

    let title = core_properties
        .title
        .clone()
        .unwrap_or_default();

    Ok(serde_json::json!({
        "text": text,
        "title": title,
        "metadata": {
            "filename": filename,
            "content_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "size": size,
            "format": "xlsx",
            "author": core_properties.creator,
            "subject": core_properties.subject,
            "description": core_properties.description,
            "keywords": core_properties.keywords,
            "created": core_properties.created,
            "modified": core_properties.modified,
            "last_modified_by": core_properties.last_modified_by,
            "paragraph_count": serde_json::Value::Null,
            "sheet_count": sheet_count,
        }
    }))
}

fn extract_shared_strings(xml: &str) -> String {
    let mut strings = Vec::new();
    let mut search_from = 0;

    while search_from < xml.len() {
        let remaining = &xml[search_from..];
        let tag_position = match remaining.find("<t") {
            Some(position) => position,
            None => break,
        };
        let absolute_tag_start = search_from + tag_position;
        let after_tag_name = absolute_tag_start + 2;

        let next_character = match xml.as_bytes().get(after_tag_name) {
            Some(&character) => character,
            None => break,
        };

        let content_start = if next_character == b'>' {
            after_tag_name + 1
        } else if next_character == b' ' {
            match xml[after_tag_name..].find('>') {
                Some(close_offset) => after_tag_name + close_offset + 1,
                None => break,
            }
        } else {
            search_from = after_tag_name;
            continue;
        };

        match xml[content_start..].find("</t>") {
            Some(end_offset) => {
                let content = &xml[content_start..content_start + end_offset];
                if !content.is_empty() {
                    strings.push(content.to_string());
                }
                search_from = content_start + end_offset + 4;
            }
            None => break,
        }
    }

    strings.join(" ")
}

// ---------------------------------------------------------------------------
// Core properties parsing
// ---------------------------------------------------------------------------

fn extract_core_properties(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> CoreProperties {
    let xml = match read_zip_entry(archive, "docProps/core.xml") {
        Some(contents) => contents,
        None => return CoreProperties::default(),
    };

    parse_core_properties_xml(&xml)
}

fn parse_core_properties_xml(xml: &str) -> CoreProperties {
    CoreProperties {
        title: extract_xml_tag_content(xml, "dc:title"),
        creator: extract_xml_tag_content(xml, "dc:creator"),
        subject: extract_xml_tag_content(xml, "dc:subject"),
        description: extract_xml_tag_content(xml, "dc:description"),
        keywords: extract_xml_tag_content(xml, "cp:keywords"),
        last_modified_by: extract_xml_tag_content(xml, "cp:lastModifiedBy"),
        created: extract_xml_tag_content(xml, "dcterms:created"),
        modified: extract_xml_tag_content(xml, "dcterms:modified"),
    }
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn extract_xml_tag_content(xml: &str, tag_name: &str) -> Option<String> {
    let open_tag = format!("<{}>", tag_name);
    let open_tag_with_attributes = format!("<{} ", tag_name);
    let close_tag = format!("</{}>", tag_name);

    let content_start = if let Some(position) = xml.find(&open_tag) {
        position + open_tag.len()
    } else if let Some(position) = xml.find(&open_tag_with_attributes) {
        let tag_start = position;
        match xml[tag_start..].find('>') {
            Some(close_bracket) => tag_start + close_bracket + 1,
            None => return None,
        }
    } else {
        return None;
    };

    let close_position = xml[content_start..].find(&close_tag)?;
    let content = &xml[content_start..content_start + close_position];

    if content.is_empty() {
        return None;
    }

    Some(content.to_string())
}

fn strip_xml_tags(xml: &str) -> String {
    let mut result = String::with_capacity(xml.len() / 2);
    let mut inside_tag = false;

    for character in xml.chars() {
        if character == '<' {
            inside_tag = true;
        } else if character == '>' {
            inside_tag = false;
        } else if !inside_tag {
            result.push(character);
        }
    }

    result
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn count_tag_occurrences(xml: &str, tag_name: &str) -> usize {
    let exact_open = format!("<{}>", tag_name);
    let attributed_open = format!("<{} ", tag_name);
    count_occurrences(xml, &exact_open) + count_occurrences(xml, &attributed_open)
}
