//! Native ODF (ODT/ODS) parser.
//!
//! Ported from `aeordb-plugin-parser-odf`.

use std::io::{Cursor, Read};

use super::{CorrectedNativeParserErrorV1, CorrectedNativeParserLimitsV1, ExpandedArchiveBudgetV1, read_zip_entry_bounded};

/// MIME types for supported ODF formats.
const MIMETYPE_ODT: &str = "application/vnd.oasis.opendocument.text";
const MIMETYPE_ODS: &str = "application/vnd.oasis.opendocument.spreadsheet";

pub fn parse(data: &[u8], filename: &str, _content_type: &str, size: u64) -> Result<serde_json::Value, String> {
  let cursor = Cursor::new(data);
  let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("not a valid ZIP archive: {}", e))?;

  // Read mimetype file to detect format
  let mimetype = read_zip_entry(&mut archive, "mimetype").map_err(|e| format!("failed to read mimetype: {}", e))?;
  let mimetype = mimetype.trim().to_string();

  let format = match mimetype.as_str() {
    MIMETYPE_ODT => "odt",
    MIMETYPE_ODS => "ods",
    _ => return Err(format!("unsupported ODF mimetype: {}", mimetype)),
  };

  // Extract text from content.xml
  let content_xml = read_zip_entry(&mut archive, "content.xml").map_err(|e| format!("failed to read content.xml: {}", e))?;

  // Parse metadata from meta.xml (optional -- some ODF files may lack it)
  let meta_xml = read_zip_entry(&mut archive, "meta.xml").ok();

  build_value(filename, size, mimetype, format, content_xml, meta_xml)
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
  let mut budget = ExpandedArchiveBudgetV1::new(limits.maximum_expanded_bytes());
  let mimetype = read_zip_entry_bounded(&mut archive, "mimetype", true, &mut budget)?
    .ok_or_else(|| CorrectedNativeParserErrorV1::Malformed("ODF file is missing mimetype".to_string()))?;
  let mimetype = mimetype.trim().to_string();
  let format = match mimetype.as_str() {
    MIMETYPE_ODT => "odt",
    MIMETYPE_ODS => "ods",
    _ => return Err(CorrectedNativeParserErrorV1::Malformed(format!("unsupported ODF mimetype: {mimetype}"))),
  };
  let content_xml = read_zip_entry_bounded(&mut archive, "content.xml", true, &mut budget)?
    .ok_or_else(|| CorrectedNativeParserErrorV1::Malformed("ODF file is missing content.xml".to_string()))?;
  let meta_xml = read_zip_entry_bounded(&mut archive, "meta.xml", false, &mut budget)?;
  let extracted_text = strip_xml_tags_bounded(&content_xml, limits.maximum_scalar_bytes())?;
  drop(content_xml);
  let metadata = extract_metadata_bounded(meta_xml.as_deref(), limits, extracted_text.len() as u64)?;
  Ok(build_value_from_parts(filename, size, mimetype, format, extracted_text, metadata))
}

fn build_value(
  filename: &str,
  size: u64,
  mimetype: String,
  format: &str,
  content_xml: String,
  meta_xml: Option<String>,
) -> Result<serde_json::Value, String> {
  let extracted_text = strip_xml_tags(&content_xml);
  drop(content_xml);
  Ok(build_value_from_text(filename, size, mimetype, format, extracted_text, meta_xml))
}

fn build_value_from_text(
  filename: &str,
  size: u64,
  mimetype: String,
  format: &str,
  extracted_text: String,
  meta_xml: Option<String>,
) -> serde_json::Value {
  let metadata = extract_metadata(meta_xml.as_deref());
  build_value_from_parts(filename, size, mimetype, format, extracted_text, metadata)
}

struct OdfExtractedMetadata {
  title: Option<String>,
  author: Option<String>,
  subject: Option<String>,
  description: Option<String>,
  created: Option<String>,
  modified: Option<String>,
  keywords: Vec<String>,
  statistics: Option<Vec<(String, u64)>>,
}

fn extract_metadata(meta_xml: Option<&str>) -> OdfExtractedMetadata {
  OdfExtractedMetadata {
    title: meta_xml.and_then(|xml| extract_element_text(xml, "dc:title")),
    author: meta_xml.and_then(|xml| extract_element_text(xml, "dc:creator")),
    subject: meta_xml.and_then(|xml| extract_element_text(xml, "dc:subject")),
    description: meta_xml.and_then(|xml| extract_element_text(xml, "dc:description")),
    created: meta_xml.and_then(|xml| extract_element_text(xml, "meta:creation-date")),
    modified: meta_xml.and_then(|xml| extract_element_text(xml, "dc:date")),
    keywords: match meta_xml {
      Some(xml) => extract_keywords(xml),
      None => Vec::new(),
    },
    statistics: meta_xml.map(extract_document_statistics),
  }
}

fn extract_metadata_bounded(
  meta_xml: Option<&str>,
  limits: CorrectedNativeParserLimitsV1,
  initial_output_bytes: u64,
) -> Result<OdfExtractedMetadata, CorrectedNativeParserErrorV1> {
  let mut output_bytes = initial_output_bytes;
  let Some(xml) = meta_xml else {
    return Ok(OdfExtractedMetadata {
      title: None,
      author: None,
      subject: None,
      description: None,
      created: None,
      modified: None,
      keywords: Vec::new(),
      statistics: None,
    });
  };
  Ok(OdfExtractedMetadata {
    title: extract_element_text_bounded(xml, "dc:title", limits, &mut output_bytes)?,
    author: extract_element_text_bounded(xml, "dc:creator", limits, &mut output_bytes)?,
    subject: extract_element_text_bounded(xml, "dc:subject", limits, &mut output_bytes)?,
    description: extract_element_text_bounded(xml, "dc:description", limits, &mut output_bytes)?,
    created: extract_element_text_bounded(xml, "meta:creation-date", limits, &mut output_bytes)?,
    modified: extract_element_text_bounded(xml, "dc:date", limits, &mut output_bytes)?,
    keywords: extract_keywords_bounded(xml, limits, &mut output_bytes)?,
    statistics: Some(extract_document_statistics(xml)),
  })
}

fn build_value_from_parts(
  filename: &str,
  size: u64,
  mimetype: String,
  format: &str,
  extracted_text: String,
  extracted: OdfExtractedMetadata,
) -> serde_json::Value {
  let OdfExtractedMetadata { title, author, subject, description, created, modified, keywords, statistics } = extracted;
  // Keep valid missing metadata explicit for the suppression audit.
  #[allow(clippy::manual_unwrap_or_default)]
  let title = match title {
    Some(title) => title,
    None => String::new(),
  };

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

  serde_json::json!({
      "text": extracted_text,
      "title": title,
      "metadata": metadata,
  })
}

/// Read a named entry from a ZIP archive and return its contents as a String.
fn read_zip_entry(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<String, String> {
  let mut file = archive.by_name(name).map_err(|e| format!("entry '{}' not found: {}", name, e))?;
  let mut contents = String::new();
  file.read_to_string(&mut contents).map_err(|e| format!("failed to read '{}': {}", name, e))?;
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

fn strip_xml_tags_bounded(content: &str, maximum_bytes: u64) -> Result<String, CorrectedNativeParserErrorV1> {
  let maximum = maximum_bytes as usize;
  let mut result = String::new();
  result
    .try_reserve_exact(maximum.min(content.len()))
    .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot reserve bounded ODF text: {error}")))?;
  let mut inside_tag = false;
  let mut pending_space = false;
  for character in content.chars() {
    if character == '<' {
      inside_tag = true;
    } else if character == '>' {
      inside_tag = false;
      pending_space = !result.is_empty();
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
  extract_element_text_slice(xml, tag_name).map(str::to_string)
}

fn extract_element_text_bounded(
  xml: &str,
  tag_name: &str,
  limits: CorrectedNativeParserLimitsV1,
  output_bytes: &mut u64,
) -> Result<Option<String>, CorrectedNativeParserErrorV1> {
  let Some(raw) = extract_element_text_slice(xml, tag_name) else {
    return Ok(None);
  };
  account_output_bytes(output_bytes, raw.len(), limits)?;
  let mut value = String::new();
  value
    .try_reserve_exact(raw.len())
    .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot reserve bounded ODF metadata: {error}")))?;
  value.push_str(raw);
  Ok(Some(value))
}

fn extract_element_text_slice<'a>(xml: &'a str, tag_name: &str) -> Option<&'a str> {
  let open_tag = format!("<{}", tag_name);
  let close_tag = format!("</{}>", tag_name);

  let open_start = xml.find(&open_tag)?;
  let tag_end = xml[open_start..].find('>')? + open_start + 1;
  let close_start = xml[tag_end..].find(&close_tag)? + tag_end;

  Some(xml[tag_end..close_start].trim())
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

fn extract_keywords_bounded(
  xml: &str,
  limits: CorrectedNativeParserLimitsV1,
  output_bytes: &mut u64,
) -> Result<Vec<String>, CorrectedNativeParserErrorV1> {
  let mut keywords = Vec::new();
  let open_tag = "<meta:keyword>";
  let close_tag = "</meta:keyword>";
  let mut search_from = 0;
  while let Some(open_position) = xml[search_from..].find(open_tag) {
    let content_start = search_from + open_position + open_tag.len();
    let Some(close_position) = xml[content_start..].find(close_tag) else {
      break;
    };
    let absolute_close = content_start + close_position;
    let keyword = xml[content_start..absolute_close].trim();
    if !keyword.is_empty() {
      let observed_members = keywords.len().saturating_add(1) as u64;
      if observed_members > u64::from(limits.maximum_container_members()) || observed_members > limits.maximum_structure_nodes() {
        return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: observed_members });
      }
      account_output_bytes(output_bytes, keyword.len(), limits)?;
      keywords.try_reserve(1).map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot grow bounded ODF keywords: {error}")))?;
      let mut value = String::new();
      value
        .try_reserve_exact(keyword.len())
        .map_err(|error| CorrectedNativeParserErrorV1::Host(format!("cannot reserve bounded ODF keyword: {error}")))?;
      value.push_str(keyword);
      keywords.push(value);
    }
    search_from = absolute_close + close_tag.len();
  }
  Ok(keywords)
}

fn account_output_bytes(
  output_bytes: &mut u64,
  scalar_bytes: usize,
  limits: CorrectedNativeParserLimitsV1,
) -> Result<(), CorrectedNativeParserErrorV1> {
  if scalar_bytes as u64 > limits.maximum_scalar_bytes() {
    return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: scalar_bytes as u64 });
  }
  let scalar_bytes = scalar_bytes as u64;
  if *output_bytes > limits.maximum_response_bytes().saturating_sub(scalar_bytes) {
    return Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: output_bytes.saturating_add(scalar_bytes) });
  }
  *output_bytes += scalar_bytes;
  Ok(())
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
