use crate::engine::compression::{should_compress, CompressionAlgorithm};
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::EngineResult;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::storage_engine::StorageEngine;

/// Simple glob matching for index config path patterns.
///
/// Supported wildcards:
///   - `*`  matches exactly one path segment (anything between slashes)
///   - `**` matches zero or more path segments (any depth)
///   - `?`  matches a single character within a segment
///
/// Both `pattern` and `path` are split by `/` and matched segment by segment.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
  let pat_segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
  let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
  glob_match_segments(&pat_segments, &path_segments)
}

fn glob_match_segments(pattern: &[&str], path: &[&str]) -> bool {
  if pattern.is_empty() {
    return path.is_empty();
  }

  if pattern[0] == "**" {
    for skip in 0..=path.len() {
      if glob_match_segments(&pattern[1..], &path[skip..]) {
        return true;
      }
    }
    return false;
  }

  if path.is_empty() {
    return false;
  }

  if segment_matches(pattern[0], path[0]) {
    glob_match_segments(&pattern[1..], &path[1..])
  } else {
    false
  }
}

fn segment_matches(pattern: &str, segment: &str) -> bool {
  if pattern == "*" {
    return true;
  }
  char_glob_match(pattern.as_bytes(), segment.as_bytes())
}

fn char_glob_match(pat: &[u8], seg: &[u8]) -> bool {
  let mut pi = 0;
  let mut si = 0;
  let mut star_pi: Option<usize> = None;
  let mut star_si: usize = 0;

  while si < seg.len() {
    if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == seg[si]) {
      pi += 1;
      si += 1;
    } else if pi < pat.len() && pat[pi] == b'*' {
      star_pi = Some(pi);
      star_si = si;
      pi += 1;
    } else if let Some(sp) = star_pi {
      pi = sp + 1;
      star_si += 1;
      si = star_si;
    } else {
      return false;
    }
  }

  while pi < pat.len() && pat[pi] == b'*' {
    pi += 1;
  }

  pi == pat.len()
}

/// Resolves `.aeordb-config/indexes.json` ownership and derived policies.
pub struct IndexConfigResolver<'a> {
  engine: &'a StorageEngine,
}

impl<'a> IndexConfigResolver<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexConfigResolver { engine }
  }

  pub fn config_path_for_directory(parent: &str) -> String {
    let normalized_parent = normalize_path(parent);
    if normalized_parent == "/" {
      "/.aeordb-config/indexes.json".to_string()
    } else {
      format!("{}/.aeordb-config/indexes.json", normalized_parent)
    }
  }

  pub fn load_config(&self, parent: &str) -> EngineResult<Option<PathIndexConfig>> {
    let normalized_parent = normalize_path(parent);
    self.engine.index_config_cache.get(&normalized_parent, self.engine)
  }

  /// Find the config that applies to a normalized file path.
  pub fn find_config_for_path(&self, normalized_path: &str) -> EngineResult<Option<(PathIndexConfig, String)>> {
    let immediate_parent = parent_path(normalized_path).unwrap_or_else(|| "/".to_string());

    if let Some(config) = self.load_config(&immediate_parent)? {
      if config.glob.is_none() {
        return Ok(Some((config, immediate_parent)));
      }

      let filename = file_name(normalized_path).unwrap_or_default();
      if glob_matches(config.glob.as_deref().unwrap_or(""), filename) {
        return Ok(Some((config, immediate_parent)));
      }
    }

    let mut ancestor = parent_path(&immediate_parent);
    while let Some(ref dir) = ancestor {
      if let Some(config) = self.load_config(dir)? {
        if let Some(ref glob_pattern) = config.glob {
          let prefix = if dir == "/" { "/".to_string() } else { format!("{}/", dir) };
          if let Some(relative) = normalized_path.strip_prefix(&prefix) {
            if glob_matches(glob_pattern, relative) {
              return Ok(Some((config, dir.clone())));
            }
          }
        }
      }

      if dir == "/" {
        break;
      }
      ancestor = parent_path(dir);
    }

    Ok(None)
  }

  pub fn compression_for_path(&self, path: &str, content_type: Option<&str>, data_length: usize) -> CompressionAlgorithm {
    let normalized = normalize_path(path);
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
    let config_path = Self::config_path_for_directory(&parent);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file_buffered(&config_path) {
      Ok(config_data) => match PathIndexConfig::deserialize_with_compression(&config_data) {
        Ok(Some(algo_str)) if algo_str == "zstd" && should_compress(content_type, data_length) => CompressionAlgorithm::Zstd,
        _ => CompressionAlgorithm::None,
      },
      Err(_) => CompressionAlgorithm::None,
    }
  }
}
