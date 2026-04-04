use std::collections::HashSet;

/// Extract PostgreSQL-style trigrams from a string.
///
/// - Lowercases the input (case-insensitive)
/// - Non-alphanumeric characters are word boundaries
/// - Each word is padded: 2 spaces prefix, 1 space suffix
/// - 3-character sliding window extracts trigrams from each padded word
/// - Returns deduplicated trigrams as Vec<Vec<u8>> (each trigram is UTF-8 bytes)
/// - Unicode-aware: operates on chars (codepoints), not bytes
pub fn extract_trigrams(s: &str) -> Vec<Vec<u8>> {
  let lower = s.to_lowercase();

  // Split into words on non-alphanumeric boundaries
  let words: Vec<&str> = lower
    .split(|c: char| !c.is_alphanumeric())
    .filter(|w| !w.is_empty())
    .collect();

  if words.is_empty() {
    return Vec::new();
  }

  let mut seen = HashSet::new();
  let mut result = Vec::new();

  for word in &words {
    // Pad: 2 spaces prefix, 1 space suffix
    let padded = format!("  {} ", word);
    let chars: Vec<char> = padded.chars().collect();

    if chars.len() < 3 {
      continue;
    }

    for window in chars.windows(3) {
      let trigram: String = window.iter().collect();
      let bytes = trigram.into_bytes();
      if seen.insert(bytes.clone()) {
        result.push(bytes);
      }
    }
  }

  result
}

/// Compute trigram similarity between two strings using the Dice coefficient.
///
/// `2 * |A intersection B| / (|A| + |B|)`
///
/// Returns 0.0 if both sets are empty.
pub fn trigram_similarity(a: &str, b: &str) -> f64 {
  let trigrams_a = extract_trigrams(a);
  let trigrams_b = extract_trigrams(b);

  if trigrams_a.is_empty() && trigrams_b.is_empty() {
    return 0.0;
  }

  let set_a: HashSet<&Vec<u8>> = trigrams_a.iter().collect();
  let set_b: HashSet<&Vec<u8>> = trigrams_b.iter().collect();

  let intersection_count = set_a.intersection(&set_b).count();
  let total = set_a.len() + set_b.len();

  if total == 0 {
    return 0.0;
  }

  (2.0 * intersection_count as f64) / total as f64
}

/// Compute automatic fuzziness (edit distance) based on term length.
///
/// - 0-2 chars: 0 (exact match)
/// - 3-5 chars: 1 edit
/// - 6+ chars: 2 edits
pub fn auto_fuzziness(len: usize) -> usize {
  match len {
    0..=2 => 0,
    3..=5 => 1,
    _ => 2,
  }
}
