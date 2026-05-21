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

/// Extract trigrams without word-boundary padding.
///
/// Used for substring (Contains) queries where we need raw character-level
/// trigrams without the space-padding that would introduce false boundary
/// constraints.
pub fn extract_trigrams_no_pad(s: &str) -> Vec<Vec<u8>> {
  let lower = s.to_lowercase();
  let chars: Vec<char> = lower.chars().collect();

  if chars.len() < 3 {
    // For very short strings, return the whole string as a single "trigram"
    if !lower.is_empty() {
      return vec![lower.into_bytes()];
    }
    return Vec::new();
  }

  let mut seen = HashSet::new();
  let mut result = Vec::new();
  for window in chars.windows(3) {
    let trigram: String = window.iter().collect();
    let bytes = trigram.into_bytes();
    if seen.insert(bytes.clone()) {
      result.push(bytes);
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

/// Compute the Damerau-Levenshtein distance (Optimal String Alignment variant)
/// between two strings.
///
/// Supports insertions, deletions, substitutions, and transpositions of adjacent
/// characters. Returns the minimum number of edit operations to transform `a` into `b`.
pub fn damerau_levenshtein(a: &str, b: &str) -> usize {
  let a_chars: Vec<char> = a.chars().collect();
  let b_chars: Vec<char> = b.chars().collect();
  let m = a_chars.len();
  let n = b_chars.len();

  if m == 0 {
    return n;
  }
  if n == 0 {
    return m;
  }

  // dp[i][j] = edit distance between a[0..i] and b[0..j]
  let mut dp = vec![vec![0usize; n + 1]; m + 1];

  for i in 0..=m {
    dp[i][0] = i;
  }
  for j in 0..=n {
    dp[0][j] = j;
  }

  for i in 1..=m {
    for j in 1..=n {
      let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };

      dp[i][j] = (dp[i - 1][j] + 1)           // deletion
        .min(dp[i][j - 1] + 1)                 // insertion
        .min(dp[i - 1][j - 1] + cost);         // substitution

      // Transposition
      if i > 1
        && j > 1
        && a_chars[i - 1] == b_chars[j - 2]
        && a_chars[i - 2] == b_chars[j - 1]
      {
        dp[i][j] = dp[i][j].min(dp[i - 2][j - 2] + 1);
      }
    }
  }

  dp[m][n]
}

/// Compute Jaro-Winkler similarity between two strings.
///
/// Returns a value in [0.0, 1.0] where 1.0 means identical strings.
/// Applies the Winkler prefix bonus (up to 4 characters, scaling factor 0.1).
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
  let a_chars: Vec<char> = a.chars().collect();
  let b_chars: Vec<char> = b.chars().collect();
  let a_len = a_chars.len();
  let b_len = b_chars.len();

  // Both empty
  if a_len == 0 && b_len == 0 {
    return 1.0;
  }
  // One empty
  if a_len == 0 || b_len == 0 {
    return 0.0;
  }

  let match_window = (a_len.max(b_len) / 2).saturating_sub(1);

  let mut a_matched = vec![false; a_len];
  let mut b_matched = vec![false; b_len];

  let mut matches = 0usize;

  // Find matching characters within window
  for i in 0..a_len {
    let start = i.saturating_sub(match_window);
    let end = (i + match_window + 1).min(b_len);

    for j in start..end {
      if !b_matched[j] && a_chars[i] == b_chars[j] {
        a_matched[i] = true;
        b_matched[j] = true;
        matches += 1;
        break;
      }
    }
  }

  if matches == 0 {
    return 0.0;
  }

  // Count transpositions among matched characters
  let mut transpositions = 0usize;
  let mut k = 0usize;
  for i in 0..a_len {
    if !a_matched[i] {
      continue;
    }
    while !b_matched[k] {
      k += 1;
    }
    if a_chars[i] != b_chars[k] {
      transpositions += 1;
    }
    k += 1;
  }

  let m = matches as f64;
  let jaro = (m / a_len as f64 + m / b_len as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0;

  // Winkler prefix bonus
  let prefix_len = a_chars
    .iter()
    .zip(b_chars.iter())
    .take(4)
    .take_while(|(a, b)| a == b)
    .count();

  let p = 0.1;
  jaro + prefix_len as f64 * p * (1.0 - jaro)
}
