/// Native HTML/XML parser.
///
/// Ported from `aeordb-plugin-parser-html`.

pub fn parse(data: &[u8], filename: &str, content_type: &str, size: u64) -> Result<serde_json::Value, String> {
    let text = std::str::from_utf8(data).map_err(|e| format!("invalid UTF-8: {}", e))?;

    let is_html = detect_html(text);
    let format = if is_html { "html" } else { "xml" };

    let title_tag = if is_html {
        extract_tag_content(text, "title")
    } else {
        None
    };

    let description = if is_html {
        extract_meta_content(text, "description")
    } else {
        None
    };

    let keywords = if is_html {
        extract_meta_content(text, "keywords")
    } else {
        None
    };

    let headings = if is_html {
        extract_headings(text)
    } else {
        Vec::new()
    };

    let link_count = if is_html {
        count_links(text)
    } else {
        0
    };

    let root_element = if !is_html {
        extract_xml_root_element(text)
    } else {
        None
    };

    let namespaces = if !is_html {
        extract_xml_namespaces(text)
    } else {
        Vec::new()
    };

    let stripped = strip_tags(text);
    let line_count = stripped.lines().count();
    let word_count = count_words(&stripped);

    let title = title_tag
        .or_else(|| headings.first().cloned())
        .unwrap_or_default();

    let mut metadata = serde_json::json!({
        "filename": filename,
        "content_type": content_type,
        "size": size,
        "format": format,
        "line_count": line_count,
        "word_count": word_count,
    });

    if is_html {
        if let Some(ref description_value) = description {
            metadata["description"] = serde_json::Value::String(description_value.clone());
        }
        if let Some(ref keywords_value) = keywords {
            metadata["keywords"] = serde_json::Value::String(keywords_value.clone());
        }
        metadata["headings"] = serde_json::json!(headings);
        metadata["link_count"] = serde_json::json!(link_count);
    } else {
        if let Some(ref root) = root_element {
            metadata["root_element"] = serde_json::Value::String(root.clone());
        }
        if !namespaces.is_empty() {
            metadata["namespaces"] = serde_json::json!(namespaces);
        }
    }

    Ok(serde_json::json!({
        "text": stripped,
        "title": title,
        "metadata": metadata,
    }))
}

fn detect_html(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("<!doctype html") || lower.contains("<html")
}

fn extract_tag_content(content: &str, tag_name: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let open_pattern = format!("<{}", tag_name);
    let close_pattern = format!("</{}>", tag_name);

    let open_start = lower.find(&open_pattern)?;
    let tag_end = lower[open_start..].find('>')? + open_start + 1;
    let close_start = lower[tag_end..].find(&close_pattern)? + tag_end;

    let raw = &content[tag_end..close_start];
    Some(decode_entities(raw.trim()))
}

fn extract_meta_content(content: &str, meta_name: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let target = meta_name.to_ascii_lowercase();

    let mut search_from = 0;
    while let Some(meta_start) = lower[search_from..].find("<meta") {
        let absolute_start = search_from + meta_start;
        let tag_end = match lower[absolute_start..].find('>') {
            Some(position) => absolute_start + position,
            None => break,
        };

        let tag_slice = &content[absolute_start..=tag_end];
        let tag_lower = tag_slice.to_ascii_lowercase();

        if meta_tag_has_name(&tag_lower, &target) {
            if let Some(value) = extract_attribute(tag_slice, "content") {
                return Some(decode_entities(&value));
            }
        }

        search_from = tag_end + 1;
    }
    None
}

fn meta_tag_has_name(tag_lower: &str, target: &str) -> bool {
    if let Some(name_position) = tag_lower.find("name") {
        let after_name = &tag_lower[name_position + 4..];
        let after_equals = after_name.trim_start().strip_prefix('=');
        if let Some(rest) = after_equals {
            let rest = rest.trim_start();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    return &stripped[..end] == target;
                }
            } else if let Some(stripped) = rest.strip_prefix('\'') {
                if let Some(end) = stripped.find('\'') {
                    return &stripped[..end] == target;
                }
            }
        }
    }
    false
}

fn extract_attribute(tag: &str, attribute_name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let attr_lower = attribute_name.to_ascii_lowercase();

    let attr_position = lower.find(&attr_lower)?;
    let after_attr = &tag[attr_position + attr_lower.len()..];
    let after_equals = after_attr.trim_start().strip_prefix('=')?;
    let trimmed = after_equals.trim_start();

    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else if let Some(rest) = trimmed.strip_prefix('\'') {
        let end = rest.find('\'')?;
        Some(rest[..end].to_string())
    } else {
        let end = trimmed
            .find(|character: char| character.is_whitespace() || character == '>')
            .unwrap_or(trimmed.len());
        Some(trimmed[..end].to_string())
    }
}

fn extract_headings(content: &str) -> Vec<String> {
    let mut headings = Vec::new();
    let lower = content.to_ascii_lowercase();
    let mut search_from = 0;

    while search_from < lower.len() {
        let remaining = &lower[search_from..];
        let next_heading = (1..=6)
            .filter_map(|level| {
                let pattern = format!("<h{}", level);
                remaining.find(&pattern).map(|position| (position, level))
            })
            .min_by_key(|(position, _)| *position);

        let (position, level) = match next_heading {
            Some(found) => found,
            None => break,
        };

        let absolute_position = search_from + position;
        let close_tag = format!("</h{}>", level);

        let tag_end = match lower[absolute_position..].find('>') {
            Some(offset) => absolute_position + offset + 1,
            None => break,
        };

        let tag_content = &lower[absolute_position..tag_end];
        let after_h = &tag_content[2 + level.to_string().len()..];
        if !after_h.is_empty() && !after_h.starts_with('>') && !after_h.starts_with(' ') {
            search_from = tag_end;
            continue;
        }

        let close_position = match lower[tag_end..].find(&close_tag) {
            Some(offset) => tag_end + offset,
            None => {
                search_from = tag_end;
                continue;
            }
        };

        let raw_heading = &content[tag_end..close_position];
        let stripped = strip_tags_simple(raw_heading);
        let decoded = decode_entities(stripped.trim());
        if !decoded.is_empty() {
            headings.push(decoded);
        }

        search_from = close_position + close_tag.len();
    }

    headings
}

fn count_links(content: &str) -> usize {
    let lower = content.to_ascii_lowercase();
    let mut count = 0;
    let mut search_from = 0;

    while let Some(position) = lower[search_from..].find("<a") {
        let absolute_position = search_from + position;
        let after_tag = absolute_position + 2;

        if after_tag < lower.len() {
            let next_character = lower.as_bytes()[after_tag];
            if next_character == b'>' || next_character == b' ' || next_character == b'\t'
                || next_character == b'\n' || next_character == b'\r'
            {
                count += 1;
            }
        } else if after_tag == lower.len() {
            count += 1;
        }

        search_from = after_tag;
    }

    count
}

fn extract_xml_root_element(content: &str) -> Option<String> {
    let trimmed = content.trim();

    let mut search_from = 0;
    loop {
        let tag_start = trimmed[search_from..].find('<')? + search_from;
        let after_open = &trimmed[tag_start + 1..];

        if after_open.starts_with('?')
            || after_open.starts_with('!')
            || after_open.starts_with('/')
        {
            if after_open.starts_with("!--") {
                match trimmed[tag_start..].find("-->") {
                    Some(end) => search_from = tag_start + end + 3,
                    None => return None,
                }
            } else {
                match trimmed[tag_start + 1..].find('>') {
                    Some(end) => search_from = tag_start + 1 + end + 1,
                    None => return None,
                }
            }
            continue;
        }

        let end = after_open
            .find(|character: char| {
                character.is_whitespace() || character == '>' || character == '/'
            })
            .unwrap_or(after_open.len());

        let name = &after_open[..end];
        if name.is_empty() {
            return None;
        }
        return Some(name.to_string());
    }
}

fn extract_xml_namespaces(content: &str) -> Vec<String> {
    let mut namespaces = Vec::new();
    let trimmed = content.trim();

    let mut search_from = 0;
    loop {
        let tag_start = match trimmed[search_from..].find('<') {
            Some(position) => search_from + position,
            None => return namespaces,
        };
        let after_open = &trimmed[tag_start + 1..];

        if after_open.starts_with('?') || after_open.starts_with('!') || after_open.starts_with('/')
        {
            if after_open.starts_with("!--") {
                match trimmed[tag_start..].find("-->") {
                    Some(end) => search_from = tag_start + end + 3,
                    None => return namespaces,
                }
            } else {
                match trimmed[tag_start + 1..].find('>') {
                    Some(end) => search_from = tag_start + 1 + end + 1,
                    None => return namespaces,
                }
            }
            continue;
        }

        let tag_end = match trimmed[tag_start..].find('>') {
            Some(end) => tag_start + end,
            None => return namespaces,
        };

        let root_tag = &trimmed[tag_start..=tag_end];
        let lower = root_tag.to_ascii_lowercase();
        let mut namespace_search = 0;

        while let Some(xmlns_position) = lower[namespace_search..].find("xmlns") {
            let absolute_position = namespace_search + xmlns_position;
            let after_xmlns = &root_tag[absolute_position + 5..];

            let value = if let Some(after_colon) = after_xmlns.strip_prefix(':') {
                if let Some(equals_position) = after_colon.find('=') {
                    let after_equals = after_colon[equals_position + 1..].trim_start();
                    extract_quoted_value(after_equals)
                } else {
                    None
                }
            } else if after_xmlns.starts_with('=') || after_xmlns.starts_with(' ') {
                let trimmed_after = after_xmlns.trim_start();
                if let Some(rest) = trimmed_after.strip_prefix('=') {
                    extract_quoted_value(rest.trim_start())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(uri) = value {
                namespaces.push(uri);
            }

            namespace_search = absolute_position + 5;
        }

        break;
    }

    namespaces
}

fn extract_quoted_value(source: &str) -> Option<String> {
    if let Some(rest) = source.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    } else if let Some(rest) = source.strip_prefix('\'') {
        let end = rest.find('\'')?;
        Some(rest[..end].to_string())
    } else {
        None
    }
}

fn strip_tags(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut characters = content.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if character != '<' {
            result.push(character);
            continue;
        }

        let remaining = &content[index..];

        if remaining.starts_with("<!--") {
            if let Some(end) = content[index + 4..].find("-->") {
                let skip_to = index + 4 + end + 3;
                while characters.peek().is_some_and(|(i, _)| *i < skip_to) {
                    characters.next();
                }
            } else {
                while characters.next().is_some() {}
            }
            continue;
        }

        let remaining_lower_bytes: Vec<u8> = remaining
            .bytes()
            .take(8)
            .map(|byte| byte.to_ascii_lowercase())
            .collect();
        if remaining_lower_bytes.starts_with(b"<script") {
            let seventh = remaining_lower_bytes.get(7);
            if seventh == Some(&b'>') || seventh == Some(&b' ') || seventh == Some(&b'\t')
                || seventh == Some(&b'\n') || seventh == Some(&b'\r')
            {
                let lower_content = content[index..].to_ascii_lowercase();
                if let Some(end) = lower_content.find("</script>") {
                    let skip_to = index + end + 9;
                    while characters.peek().is_some_and(|(i, _)| *i < skip_to) {
                        characters.next();
                    }
                } else {
                    while characters.next().is_some() {}
                }
                continue;
            }
        }

        if remaining_lower_bytes.starts_with(b"<style") {
            let sixth = remaining_lower_bytes.get(6);
            if sixth == Some(&b'>') || sixth == Some(&b' ') || sixth == Some(&b'\t')
                || sixth == Some(&b'\n') || sixth == Some(&b'\r')
            {
                let lower_content = content[index..].to_ascii_lowercase();
                if let Some(end) = lower_content.find("</style>") {
                    let skip_to = index + end + 8;
                    while characters.peek().is_some_and(|(i, _)| *i < skip_to) {
                        characters.next();
                    }
                } else {
                    while characters.next().is_some() {}
                }
                continue;
            }
        }

        for (_, tag_character) in characters.by_ref() {
            if tag_character == '>' {
                result.push(' ');
                break;
            }
        }
    }

    let decoded = decode_entities(&result);
    normalize_whitespace(&decoded)
}

fn strip_tags_simple(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut inside_tag = false;

    for character in content.chars() {
        if character == '<' {
            inside_tag = true;
        } else if character == '>' {
            inside_tag = false;
        } else if !inside_tag {
            result.push(character);
        }
    }

    result
}

fn decode_entities(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '&' {
            result.push(character);
            continue;
        }

        let mut entity = String::new();
        let mut found_semicolon = false;
        let mut consumed: Vec<char> = Vec::new();

        for _ in 0..12 {
            match characters.peek() {
                Some(&';') => {
                    characters.next();
                    found_semicolon = true;
                    break;
                }
                Some(&next_character) => {
                    consumed.push(next_character);
                    entity.push(next_character);
                    characters.next();
                }
                None => break,
            }
        }

        if !found_semicolon {
            result.push('&');
            for consumed_character in consumed {
                result.push(consumed_character);
            }
            continue;
        }

        match entity.as_str() {
            "amp" => result.push('&'),
            "lt" => result.push('<'),
            "gt" => result.push('>'),
            "quot" => result.push('"'),
            "apos" => result.push('\''),
            "nbsp" => result.push(' '),
            other => {
                if let Some(hex_digits) = other.strip_prefix("#x").or_else(|| other.strip_prefix("#X")) {
                    if let Ok(code_point) = u32::from_str_radix(hex_digits, 16) {
                        if let Some(decoded_character) = char::from_u32(code_point) {
                            result.push(decoded_character);
                        } else {
                            result.push('&');
                            result.push_str(&entity);
                            result.push(';');
                        }
                    } else {
                        result.push('&');
                        result.push_str(&entity);
                        result.push(';');
                    }
                } else if let Some(decimal_digits) = other.strip_prefix('#') {
                    if let Ok(code_point) = decimal_digits.parse::<u32>() {
                        if let Some(decoded_character) = char::from_u32(code_point) {
                            result.push(decoded_character);
                        } else {
                            result.push('&');
                            result.push_str(&entity);
                            result.push(';');
                        }
                    } else {
                        result.push('&');
                        result.push_str(&entity);
                        result.push(';');
                    }
                } else {
                    result.push('&');
                    result.push_str(&entity);
                    result.push(';');
                }
            }
        }
    }

    result
}

fn normalize_whitespace(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut previous_was_whitespace = true;

    for character in text.chars() {
        if character.is_whitespace() {
            if !previous_was_whitespace {
                result.push(' ');
            }
            previous_was_whitespace = true;
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

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}
