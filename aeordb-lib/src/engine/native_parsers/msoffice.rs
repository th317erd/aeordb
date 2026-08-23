//! Native MS Office (DOCX/XLSX) parser.
//!
//! Ported from `aeordb-plugin-parser-msoffice`.

use std::io::{Cursor, Read};

use super::{CorrectedNativeParserErrorV1, CorrectedNativeParserLimitsV1, ExpandedArchiveBudgetV1, read_zip_entry_bounded};

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
  let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("not a valid ZIP archive: {}", e))?;

  let format = detect_format(&archive)?;
  let core_properties = extract_core_properties(&mut archive);

  match format {
    OfficeFormat::Docx => parse_docx(&mut archive, filename, size, &core_properties),
    OfficeFormat::Xlsx => parse_xlsx(&mut archive, filename, size, &core_properties),
  }
}

pub(super) fn parse_corrected(
  data: &[u8],
  filename: &str,
  size: u64,
  limits: CorrectedNativeParserLimitsV1,
) -> Result<serde_json::Value, CorrectedNativeParserErrorV1> {
  let cursor = Cursor::new(data);
  let mut archive =
    zip::ZipArchive::new(cursor).map_err(|error| CorrectedNativeParserErrorV1::Malformed(format!("not a valid ZIP archive: {error}")))?;
  let format = detect_format(&archive).map_err(CorrectedNativeParserErrorV1::Malformed)?;
  let mut budget = ExpandedArchiveBudgetV1::new(limits.maximum_expanded_bytes());
  let core_properties = match read_zip_entry_bounded(&mut archive, "docProps/core.xml", false, &mut budget)? {
    Some(xml) => parse_core_properties_xml_bounded(&xml, limits.maximum_scalar_bytes())?,
    None => CoreProperties::default(),
  };

  match format {
    OfficeFormat::Docx => {
      let document_xml = read_zip_entry_bounded(&mut archive, "word/document.xml", true, &mut budget)?
        .ok_or_else(|| CorrectedNativeParserErrorV1::Malformed("DOCX file missing word/document.xml".to_string()))?;
      let paragraph_count = count_tag_occurrences(&document_xml, "w:p");
      let text = strip_xml_tags_bounded(&document_xml, limits.maximum_scalar_bytes())?;
      drop(document_xml);
      Ok(build_docx_output(text, paragraph_count, filename, size, &core_properties))
    }
    OfficeFormat::Xlsx => {
      let shared_strings_xml = read_zip_entry_bounded(&mut archive, "xl/sharedStrings.xml", false, &mut budget)?;
      let workbook_xml = read_zip_entry_bounded(&mut archive, "xl/workbook.xml", false, &mut budget)?;
      let text = match shared_strings_xml.as_deref() {
        Some(xml) => extract_shared_strings_bounded(xml, limits.maximum_scalar_bytes())?,
        None => String::new(),
      };
      drop(shared_strings_xml);
      let sheet_count = match workbook_xml.as_deref() {
        Some(xml) => count_occurrences(xml, "<sheet "),
        None => 0,
      };
      drop(workbook_xml);
      Ok(build_xlsx_output(text, sheet_count, filename, size, &core_properties))
    }
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
  let document_xml = read_zip_entry(archive, "word/document.xml").ok_or_else(|| "DOCX file missing word/document.xml".to_string())?;

  Ok(build_docx_value(document_xml, filename, size, core_properties))
}

fn build_docx_value(document_xml: String, filename: &str, size: u64, core_properties: &CoreProperties) -> serde_json::Value {
  let paragraph_count = count_tag_occurrences(&document_xml, "w:p");
  let text = strip_xml_tags(&document_xml);
  drop(document_xml);
  build_docx_output(text, paragraph_count, filename, size, core_properties)
}

fn build_docx_output(
  text: String,
  paragraph_count: usize,
  filename: &str,
  size: u64,
  core_properties: &CoreProperties,
) -> serde_json::Value {
  // Keep valid missing metadata explicit for the suppression audit.
  #[allow(clippy::manual_unwrap_or_default)]
  let title = match core_properties.title.clone() {
    Some(title) => title,
    None => String::new(),
  };

  serde_json::json!({
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
  })
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
  let shared_strings_xml = read_zip_entry(archive, "xl/sharedStrings.xml");
  let workbook_xml = read_zip_entry(archive, "xl/workbook.xml");
  Ok(build_xlsx_value(shared_strings_xml, workbook_xml, filename, size, core_properties))
}

fn build_xlsx_value(
  shared_strings_xml: Option<String>,
  workbook_xml: Option<String>,
  filename: &str,
  size: u64,
  core_properties: &CoreProperties,
) -> serde_json::Value {
  let text = match shared_strings_xml.as_deref() {
    Some(xml) => extract_shared_strings(xml),
    None => String::new(),
  };
  drop(shared_strings_xml);
  let sheet_count = match workbook_xml.as_deref() {
    Some(xml) => count_occurrences(xml, "<sheet "),
    None => 0,
  };
  drop(workbook_xml);

  build_xlsx_output(text, sheet_count, filename, size, core_properties)
}

fn build_xlsx_output(text: String, sheet_count: usize, filename: &str, size: u64, core_properties: &CoreProperties) -> serde_json::Value {
  // Keep valid missing metadata explicit for the suppression audit.
  #[allow(clippy::manual_unwrap_or_default)]
  let title = match core_properties.title.clone() {
    Some(title) => title,
    None => String::new(),
  };

  serde_json::json!({
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
  })
}

fn extract_shared_strings(xml: &str) -> String {
  let mut output = String::new();
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
          if !output.is_empty() {
            output.push(' ');
          }
          output.push_str(content);
        }
        search_from = content_start + end_offset + 4;
      }
      None => break,
    }
  }

  output
}

fn extract_shared_strings_bounded(xml: &str, maximum_bytes: u64) -> Result<String, CorrectedNativeParserErrorV1> {
  let mut output = String::new();
  let mut search_from = 0;
  while search_from < xml.len() {
    let remaining = &xml[search_from..];
    let Some(tag_position) = remaining.find("<t") else {
      break;
    };
    let absolute_tag_start = search_from + tag_position;
    let after_tag_name = absolute_tag_start + 2;
    let Some(&next_character) = xml.as_bytes().get(after_tag_name) else {
      break;
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
    let Some(end_offset) = xml[content_start..].find("</t>") else {
      break;
    };
    let content = &xml[content_start..content_start + end_offset];
    if !content.is_empty() {
      let separator = !output.is_empty();
      append_bounded_text(&mut output, content, separator, maximum_bytes)?;
    }
    search_from = content_start + end_offset + 4;
  }
  Ok(output)
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

fn parse_core_properties_xml_bounded(xml: &str, maximum_scalar_bytes: u64) -> Result<CoreProperties, CorrectedNativeParserErrorV1> {
  Ok(CoreProperties {
    title: extract_xml_tag_content_bounded(xml, "dc:title", maximum_scalar_bytes)?,
    creator: extract_xml_tag_content_bounded(xml, "dc:creator", maximum_scalar_bytes)?,
    subject: extract_xml_tag_content_bounded(xml, "dc:subject", maximum_scalar_bytes)?,
    description: extract_xml_tag_content_bounded(xml, "dc:description", maximum_scalar_bytes)?,
    keywords: extract_xml_tag_content_bounded(xml, "cp:keywords", maximum_scalar_bytes)?,
    last_modified_by: extract_xml_tag_content_bounded(xml, "cp:lastModifiedBy", maximum_scalar_bytes)?,
    created: extract_xml_tag_content_bounded(xml, "dcterms:created", maximum_scalar_bytes)?,
    modified: extract_xml_tag_content_bounded(xml, "dcterms:modified", maximum_scalar_bytes)?,
  })
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn extract_xml_tag_content(xml: &str, tag_name: &str) -> Option<String> {
  extract_xml_tag_content_slice(xml, tag_name).map(str::to_string)
}

fn extract_xml_tag_content_bounded(
  xml: &str,
  tag_name: &str,
  maximum_scalar_bytes: u64,
) -> Result<Option<String>, CorrectedNativeParserErrorV1> {
  let Some(content) = extract_xml_tag_content_slice(xml, tag_name) else {
    return Ok(None);
  };
  if content.len() as u64 > maximum_scalar_bytes {
    return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: content.len() as u64 });
  }
  let mut value = String::new();
  value
    .try_reserve_exact(content.len())
    .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot reserve bounded Office metadata: {error}")))?;
  value.push_str(content);
  Ok(Some(value))
}

fn extract_xml_tag_content_slice<'a>(xml: &'a str, tag_name: &str) -> Option<&'a str> {
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

  Some(content)
}

fn strip_xml_tags(xml: &str) -> String {
  let mut result = String::with_capacity(xml.len() / 2);
  let mut inside_tag = false;
  let mut pending_space = false;

  for character in xml.chars() {
    if character == '<' {
      inside_tag = true;
    } else if character == '>' {
      inside_tag = false;
    } else if !inside_tag {
      if character.is_whitespace() {
        pending_space = !result.is_empty();
      } else {
        if pending_space {
          result.push(' ');
          pending_space = false;
        }
        result.push(character);
      }
    }
  }

  result
}

fn strip_xml_tags_bounded(xml: &str, maximum_bytes: u64) -> Result<String, CorrectedNativeParserErrorV1> {
  let maximum = maximum_bytes as usize;
  let mut result = String::new();
  result
    .try_reserve_exact(maximum.min(xml.len()))
    .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot reserve bounded Office text: {error}")))?;
  let mut inside_tag = false;
  let mut pending_space = false;
  for character in xml.chars() {
    if character == '<' {
      inside_tag = true;
    } else if character == '>' {
      inside_tag = false;
    } else if !inside_tag {
      if character.is_whitespace() {
        pending_space = !result.is_empty();
      } else {
        let added = character.len_utf8() + usize::from(pending_space);
        let observed = result.len().saturating_add(added);
        if observed > maximum {
          return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: observed as u64 });
        }
        if pending_space {
          result.push(' ');
          pending_space = false;
        }
        result.push(character);
      }
    }
  }
  Ok(result)
}

fn append_bounded_text(output: &mut String, value: &str, separator: bool, maximum_bytes: u64) -> Result<(), CorrectedNativeParserErrorV1> {
  let added = value.len().saturating_add(usize::from(separator));
  let observed = output.len().saturating_add(added);
  if observed as u64 > maximum_bytes {
    return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: observed as u64 });
  }
  output.try_reserve(added).map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot grow bounded Office text: {error}")))?;
  if separator {
    output.push(' ');
  }
  output.push_str(value);
  Ok(())
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
  haystack.matches(needle).count()
}

fn count_tag_occurrences(xml: &str, tag_name: &str) -> usize {
  let exact_open = format!("<{}>", tag_name);
  let attributed_open = format!("<{} ", tag_name);
  count_occurrences(xml, &exact_open) + count_occurrences(xml, &attributed_open)
}
