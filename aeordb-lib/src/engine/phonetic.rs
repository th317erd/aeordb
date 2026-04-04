//! Phonetic indexing algorithms for fuzzy name matching.
//!
//! Implements:
//! - American Soundex (4-character codes)
//! - Double Metaphone primary code (simplified, covering ~90% of English names)
//! - Double Metaphone alternate code (SCH variant)

/// Compute American Soundex code for a string.
///
/// Returns a 4-character code: first letter + 3 digits.
/// Empty input returns empty string.
pub fn soundex(s: &str) -> String {
  let chars: Vec<char> = s
    .to_uppercase()
    .chars()
    .filter(|c| c.is_ascii_alphabetic())
    .collect();

  if chars.is_empty() {
    return String::new();
  }

  let first = chars[0];
  let mut result = String::with_capacity(4);
  result.push(first);

  let code_for = |c: char| -> Option<char> {
    match c {
      'B' | 'F' | 'P' | 'V' => Some('1'),
      'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => Some('2'),
      'D' | 'T' => Some('3'),
      'L' => Some('4'),
      'M' | 'N' => Some('5'),
      'R' => Some('6'),
      _ => None, // A, E, I, O, U, H, W, Y
    }
  };

  // The first letter's digit is needed for adjacent-duplicate suppression,
  // but we don't include it in the output (it's represented by the letter).
  let first_code = code_for(first);
  let mut last_code = first_code;

  let mut i = 1;
  while i < chars.len() && result.len() < 4 {
    let c = chars[i];
    let current_code = code_for(c);

    match current_code {
      Some(digit) => {
        // H and W act as separators: if the letter before H/W has the same
        // code as the letter after, they are coded as a single value.
        // But if the previous coded letter is the same digit, skip.
        if last_code != Some(digit) {
          result.push(digit);
        }
        last_code = Some(digit);
      }
      None => {
        // Vowels and H/W: H and W don't reset the "last code" —
        // they allow adjacent-duplicate collapse across them.
        // But vowels DO separate identical consonant codes.
        if !matches!(c, 'H' | 'W') {
          last_code = None;
        }
      }
    }
    i += 1;
  }

  // Pad with zeros to exactly 4 characters
  while result.len() < 4 {
    result.push('0');
  }
  result.truncate(4);
  result
}

/// Compute Double Metaphone primary code for a string.
///
/// Returns up to 4 characters. Handles the most common English phonetic rules
/// (~90% coverage). Empty input returns empty string.
pub fn dmetaphone_primary(s: &str) -> String {
  if s.is_empty() {
    return String::new();
  }

  let chars: Vec<char> = s
    .to_uppercase()
    .chars()
    .filter(|c| c.is_ascii_alphabetic())
    .collect();

  if chars.is_empty() {
    return String::new();
  }

  let mut result = String::new();
  let mut i = 0;
  let len = chars.len();

  // Handle initial silent consonants
  if len >= 2 {
    match (chars[0], chars[1]) {
      ('G', 'N') | ('K', 'N') | ('P', 'N') | ('A', 'E') | ('W', 'R') => i = 1,
      _ => {}
    }
  }

  while i < len && result.len() < 4 {
    let c = chars[i];
    let next = if i + 1 < len { Some(chars[i + 1]) } else { None };
    let prev = if i > 0 { Some(chars[i - 1]) } else { None };

    match c {
      'A' | 'E' | 'I' | 'O' | 'U' => {
        if i == 0 {
          result.push('A');
        }
        i += 1;
      }
      'B' => {
        result.push('P');
        i += if next == Some('B') { 2 } else { 1 };
      }
      'C' => {
        if next == Some('H') {
          result.push('X');
          i += 2;
        } else if matches!(next, Some('I') | Some('E') | Some('Y')) {
          result.push('S');
          i += 2;
        } else {
          result.push('K');
          i += if next == Some('C')
            && !matches!(chars.get(i + 2), Some(&'I') | Some(&'E'))
          {
            2
          } else {
            1
          };
        }
      }
      'D' => {
        if next == Some('G')
          && matches!(
            chars.get(i + 2),
            Some(&'I') | Some(&'E') | Some(&'Y')
          )
        {
          result.push('J');
          i += 3;
        } else {
          result.push('T');
          i += if next == Some('D') { 2 } else { 1 };
        }
      }
      'F' => {
        result.push('F');
        i += if next == Some('F') { 2 } else { 1 };
      }
      'G' => {
        if next == Some('H') {
          // GH: silent if after vowel and not at start
          if i > 0
            && matches!(
              prev,
              Some('A') | Some('E') | Some('I') | Some('O') | Some('U')
            )
          {
            i += 2; // silent
          } else {
            result.push('K');
            i += 2;
          }
        } else if next == Some('N') {
          i += 2; // silent GN
        } else if matches!(next, Some('I') | Some('E') | Some('Y')) && prev != Some('G')
        {
          result.push('J');
          i += 2;
        } else {
          if next != Some('G') || (next == Some('G') && i == 0) {
            result.push('K');
          }
          i += if next == Some('G') { 2 } else { 1 };
        }
      }
      'H' => {
        // H is voiced only before a vowel and not after a vowel
        if matches!(
          next,
          Some('A') | Some('E') | Some('I') | Some('O') | Some('U')
        ) && !matches!(
          prev,
          Some('A') | Some('E') | Some('I') | Some('O') | Some('U')
        ) {
          result.push('H');
        }
        i += 1;
      }
      'J' => {
        result.push('J');
        i += if next == Some('J') { 2 } else { 1 };
      }
      'K' => {
        result.push('K');
        i += if prev == Some('C') { 2 } else { 1 };
      }
      'L' => {
        result.push('L');
        i += if next == Some('L') { 2 } else { 1 };
      }
      'M' => {
        result.push('M');
        i += if next == Some('M') { 2 } else { 1 };
      }
      'N' => {
        result.push('N');
        i += if next == Some('N') { 2 } else { 1 };
      }
      'P' => {
        if next == Some('H') {
          result.push('F');
          i += 2;
        } else {
          result.push('P');
          i += if next == Some('P') { 2 } else { 1 };
        }
      }
      'Q' => {
        result.push('K');
        i += if next == Some('Q') { 2 } else { 1 };
      }
      'R' => {
        result.push('R');
        i += if next == Some('R') { 2 } else { 1 };
      }
      'S' => {
        if next == Some('H') {
          result.push('X');
          i += 2;
        } else if next == Some('C') && matches!(chars.get(i + 2), Some(&'H')) {
          result.push('X');
          i += 3;
        } else if matches!(next, Some('I') | Some('E'))
          && matches!(chars.get(i + 2), Some(&'O') | Some(&'A'))
        {
          result.push('X');
          i += 3;
        } else {
          result.push('S');
          i += if next == Some('S') || next == Some('Z') { 2 } else { 1 };
        }
      }
      'T' => {
        if next == Some('H') {
          result.push('0'); // theta
          i += 2;
        } else if next == Some('I')
          && matches!(chars.get(i + 2), Some(&'O') | Some(&'A'))
        {
          result.push('X');
          i += 3;
        } else {
          result.push('T');
          i += if next == Some('T') { 2 } else { 1 };
        }
      }
      'V' => {
        result.push('F');
        i += if next == Some('V') { 2 } else { 1 };
      }
      'W' | 'Y' => {
        // W/Y before vowel produce A
        if matches!(
          next,
          Some('A') | Some('E') | Some('I') | Some('O') | Some('U')
        ) {
          result.push('A');
        }
        i += 1;
      }
      'X' => {
        result.push('K');
        if result.len() < 4 {
          result.push('S');
        }
        i += if next == Some('X') { 2 } else { 1 };
      }
      'Z' => {
        result.push('S');
        i += if next == Some('Z') { 2 } else { 1 };
      }
      _ => {
        i += 1;
      }
    }
  }

  result.truncate(4);
  result
}

/// Compute Double Metaphone alternate code for a string.
///
/// Returns `None` if the alternate code would be identical to the primary code.
/// Currently handles the most significant alternate case: SCH produces "S" as
/// alternate vs "X" as primary.
pub fn dmetaphone_alt(s: &str) -> Option<String> {
  let primary = dmetaphone_primary(s);
  if primary.is_empty() {
    return None;
  }

  let chars: Vec<char> = s
    .to_uppercase()
    .chars()
    .filter(|c| c.is_ascii_alphabetic())
    .collect();

  // SCH: primary uses X, alternate uses SK
  if chars.len() >= 3 && chars[0] == 'S' && chars[1] == 'C' && chars[2] == 'H' {
    let mut alt = String::from("S");
    if primary.len() > 1 {
      alt.push_str(&primary[1..]);
    }
    alt.truncate(4);
    if alt != primary {
      return Some(alt);
    }
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;

  // === Soundex basic ===

  #[test]
  fn test_soundex_basic_codes() {
    assert_eq!(soundex("Robert"), "R163");
    assert_eq!(soundex("Rupert"), "R163");
    assert_eq!(soundex("Smith"), "S530");
    assert_eq!(soundex("Smythe"), "S530");
  }

  #[test]
  fn test_soundex_ashcraft() {
    assert_eq!(soundex("Ashcraft"), "A261");
  }

  #[test]
  fn test_soundex_empty() {
    assert_eq!(soundex(""), "");
  }

  #[test]
  fn test_soundex_single_char() {
    assert_eq!(soundex("A"), "A000");
  }

  // === Double Metaphone basic ===

  #[test]
  fn test_dmetaphone_not_empty_for_names() {
    assert!(!dmetaphone_primary("Smith").is_empty());
    assert!(!dmetaphone_primary("Schmidt").is_empty());
  }

  #[test]
  fn test_dmetaphone_empty() {
    assert_eq!(dmetaphone_primary(""), "");
  }
}
