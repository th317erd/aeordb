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
      ".." => { segments.pop(); } // go up one level (silently ignored at root)
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

pub fn path_segments(path: &str) -> Vec<&str> {
  path.trim()
    .split('/')
    .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
    .collect()
}
