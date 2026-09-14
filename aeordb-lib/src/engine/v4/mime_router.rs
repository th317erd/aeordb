//! Corrected MIME syntax. Legacy exact matching does not use this owner.

/// Validate RFC 9110 media-type parameters without allocating their contents.
/// The returned RFC 6838 essence is at most 255 bytes; stored metadata is never
/// normalized in place or replaced by the routing value.
pub(crate) fn corrected_mime_essence(content_type: Option<&str>) -> Option<String> {
  let value = content_type?.trim_matches(|character| matches!(character, ' ' | '\t'));
  let essence_end = match value.bytes().position(|byte| matches!(byte, b';' | b' ' | b'\t')) {
    Some(position) => position,
    None => value.len(),
  };
  if essence_end == 0 || essence_end > 255 {
    return None;
  }
  let essence = value[..essence_end].to_ascii_lowercase();
  if !super::parser_plan::is_canonical_mime_essence(essence.as_bytes()) || !parameters_valid(&value.as_bytes()[essence_end..]) {
    return None;
  }
  Some(essence)
}

fn parameters_valid(bytes: &[u8]) -> bool {
  let mut cursor = 0;
  loop {
    skip_optional_whitespace(bytes, &mut cursor);
    if cursor == bytes.len() {
      return true;
    }
    if bytes[cursor] != b';' {
      return false;
    }
    cursor += 1;
    skip_optional_whitespace(bytes, &mut cursor);
    // RFC 9110 section 5.6.6 permits empty parameter slots.
    if cursor == bytes.len() || bytes[cursor] == b';' {
      continue;
    }
    let name_start = cursor;
    while bytes.get(cursor).is_some_and(|byte| token_character(*byte)) {
      cursor += 1;
    }
    // No whitespace is permitted around the equals sign.
    if cursor == name_start || bytes.get(cursor) != Some(&b'=') {
      return false;
    }
    cursor += 1;
    if bytes.get(cursor) == Some(&b'"') {
      let Some(end) = quoted_value_end(bytes, cursor + 1) else {
        return false;
      };
      cursor = end;
    } else {
      let value_start = cursor;
      while bytes.get(cursor).is_some_and(|byte| token_character(*byte)) {
        cursor += 1;
      }
      if cursor == value_start {
        return false;
      }
    }
  }
}

fn quoted_value_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
  loop {
    match *bytes.get(cursor)? {
      b'"' => return Some(cursor + 1),
      b'\\' => {
        cursor += 1;
        if !matches!(*bytes.get(cursor)?, b'\t' | b' '..=b'~' | 0x80..=0xff) {
          return None;
        }
      }
      // qdtext: HTAB, SP, visible bytes other than quote/backslash, obs-text.
      b'\t' | b' '..=b'!' | b'#'..=b'[' | b']'..=b'~' | 0x80..=0xff => {}
      _ => return None,
    }
    cursor += 1;
  }
}

fn skip_optional_whitespace(bytes: &[u8], cursor: &mut usize) {
  while bytes.get(*cursor).is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
    *cursor += 1;
  }
}

fn token_character(byte: u8) -> bool {
  byte.is_ascii_alphanumeric()
    || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
}
