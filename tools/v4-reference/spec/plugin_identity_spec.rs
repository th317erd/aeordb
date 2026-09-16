use super::*;

#[test]
fn independently_constructed_plugin_metadata_matches_its_declared_results() {
  for case in fixture_cases() {
    assert_eq!(observe(case.format, &case.bytes), (case.expected.to_string(), case.canonical_key), "{}", case.id);
  }
}

#[test]
fn independent_metadata_rejects_every_truncated_payload() {
  for case in fixture_cases().into_iter().filter(|case| !case.expected.starts_with("error:")) {
    for end in 0..case.bytes.len() {
      assert!(observe(case.format, &case.bytes[..end]).0.starts_with("error:"), "{}/{end}", case.id);
    }
  }
}
