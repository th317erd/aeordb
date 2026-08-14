pub fn normalize_path(path: &str) -> String {
  // Reject null bytes (H11)
  let path = path.replace('\0', "");
  let trimmed = path.trim();

  if trimmed.is_empty() {
    return "/".to_string();
  }

  // Split on '/', filter empties (handles multiple consecutive slashes),
  // and resolve "." (current dir) and ".." (parent dir) segments.
  let mut segments: Vec<&str> = Vec::new();
  for segment in trimmed.split('/').filter(|s| !s.is_empty()) {
    match segment {
      "." => {} // skip current-dir references
      ".." => {
        segments.pop();
      } // go up one level (silently ignored at root)
      s => segments.push(s),
    }
  }

  if segments.is_empty() {
    "/".to_string()
  } else {
    format!("/{}", segments.join("/"))
  }
}

pub fn parent_path(path: &str) -> Option<String> {
  let normalized = normalize_path(path);

  if normalized == "/" {
    return None;
  }

  match normalized.rfind('/') {
    Some(0) => Some("/".to_string()),
    Some(index) => Some(normalized[..index].to_string()),
    None => None,
  }
}

pub fn file_name(path: &str) -> Option<&str> {
  let trimmed = path.trim().trim_end_matches('/');

  if trimmed.is_empty() || trimmed == "/" {
    return None;
  }

  match trimmed.rfind('/') {
    Some(index) => {
      let name = &trimmed[index + 1..];
      if name.is_empty() {
        None
      } else {
        Some(name)
      }
    }
    None => Some(trimmed),
  }
}

pub fn path_segments(path: &str) -> Vec<String> {
  let mut segments = Vec::new();
  for segment in path.trim().split('/') {
    if segment.is_empty() || segment == "." {
      continue;
    }
    if segment == ".." {
      segments.pop(); // resolve parent reference
      continue;
    }
    segments.push(segment.to_string());
  }
  segments
}

/// Match canonical path segments using the index-scope wildcard contract.
///
/// `*` matches within one segment, `**` matches zero or more complete
/// segments, and `?` matches one byte within a segment.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
  let pattern = pattern.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
  let path = path.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
  glob_match_segments(&pattern, &path)
}

fn glob_match_segments(pattern: &[&str], path: &[&str]) -> bool {
  let Some((head, tail)) = pattern.split_first() else {
    return path.is_empty();
  };
  if *head == "**" {
    return (0..=path.len()).any(|skip| glob_match_segments(tail, &path[skip..]));
  }
  let Some((path_head, path_tail)) = path.split_first() else {
    return false;
  };
  segment_matches(head.as_bytes(), path_head.as_bytes()) && glob_match_segments(tail, path_tail)
}

fn segment_matches(pattern: &[u8], segment: &[u8]) -> bool {
  let mut pattern_index = 0usize;
  let mut segment_index = 0usize;
  let mut star_pattern_index = None;
  let mut star_segment_index = 0usize;

  while segment_index < segment.len() {
    if pattern_index < pattern.len() && (pattern[pattern_index] == b'?' || pattern[pattern_index] == segment[segment_index]) {
      pattern_index += 1;
      segment_index += 1;
    } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
      star_pattern_index = Some(pattern_index);
      star_segment_index = segment_index;
      pattern_index += 1;
    } else if let Some(star) = star_pattern_index {
      pattern_index = star + 1;
      star_segment_index += 1;
      segment_index = star_segment_index;
    } else {
      return false;
    }
  }
  while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
    pattern_index += 1;
  }
  pattern_index == pattern.len()
}
