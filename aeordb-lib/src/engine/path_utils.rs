pub fn normalize_path(path: &str) -> String {
  let trimmed = path.trim();

  if trimmed.is_empty() {
    return "/".to_string();
  }

  // Ensure leading slash
  let with_leading = if trimmed.starts_with('/') {
    trimmed.to_string()
  } else {
    format!("/{}", trimmed)
  };

  // Collapse multiple consecutive slashes
  let mut collapsed = String::with_capacity(with_leading.len());
  let mut previous_was_slash = false;
  for character in with_leading.chars() {
    if character == '/' {
      if !previous_was_slash {
        collapsed.push('/');
      }
      previous_was_slash = true;
    } else {
      collapsed.push(character);
      previous_was_slash = false;
    }
  }

  // Remove trailing slash (except for root "/")
  if collapsed.len() > 1 && collapsed.ends_with('/') {
    collapsed.pop();
  }

  collapsed
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
    .filter(|segment| !segment.is_empty())
    .collect()
}
