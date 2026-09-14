//! Native suite dispatch and complete outputs, not production-generated goldens.
use super::*;
use std::io::Write;

fn fixture() -> serde_json::Value {
  serde_json::from_str(include_str!("../fixtures/v4/native-semantic-conformance-v1/native-suite-v1/fixtures.json")).unwrap()
}

fn limits() -> CorrectedNativeParserLimitsV1 {
  CorrectedNativeParserLimitsV1::new(16 << 20, 16 << 20, 1 << 20, 1 << 20, 65_536)
}

fn body(case: &serde_json::Value) -> Vec<u8> {
  if let Some(value) = case["body_utf8"].as_str() {
    return value.as_bytes().to_vec();
  }
  if let Some(value) = case["body_hex"].as_str() {
    return hex::decode(value).unwrap();
  }
  let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
  for entry in case["zip_entries"].as_array().unwrap() {
    writer
      .start_file(entry[0].as_str().unwrap(), zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored))
      .unwrap();
    writer.write_all(entry[1].as_str().unwrap().as_bytes()).unwrap();
  }
  writer.finish().unwrap().into_inner()
}

fn require_family(parser: Option<CorrectedParserV1>, family: &str) {
  let parser = parser.unwrap_or_else(|| panic!("missing {family} parser"));
  match family {
    "docx" => assert!(matches!(parser, CorrectedParserV1::MsOffice)),
    "odt" => assert!(matches!(parser, CorrectedParserV1::Odf)),
    _ => {
      let expected: ParserFn = match family {
        "text" => text::parse,
        "html" => html::parse,
        "image" => image::parse,
        "audio" => audio::parse,
        "video" => video::parse,
        "pdf" => pdf::parse,
        other => panic!("unexercised family: {other}"),
      };
      let CorrectedParserV1::Generic(actual) = parser else {
        panic!("wrong native family for {family}");
      };
      assert!(std::ptr::fn_addr_eq(actual, expected), "wrong dispatch for {family}");
    }
  }
}

#[test]
fn conformance_native_suite_complete_eight_family_outputs() {
  let vectors = fixture();
  assert_eq!(vectors["cases"].as_array().unwrap().len(), 11);
  for case in vectors["cases"].as_array().unwrap() {
    let bytes = body(case);
    let mime = case["mime"].as_str().unwrap();
    let parsed = parse_native_corrected(
      &bytes,
      Some(mime),
      None,
      case["filename"].as_str().unwrap(),
      mime,
      vectors["metadata_size"].as_u64().unwrap(),
      limits(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(parsed, case["expected"], "{}", case["id"]);
  }
}

#[test]
fn conformance_native_suite_all_dispatch_aliases_and_generic_fallback() {
  let vectors = fixture();
  assert_eq!(vectors["dispatch"].as_array().unwrap().len(), 8);
  for group in vectors["dispatch"].as_array().unwrap() {
    let family = group["seed"].as_str().unwrap();
    for mime in group["mime"].as_array().unwrap() {
      require_family(corrected_parser(Some(mime.as_str().unwrap()), None), family);
    }
    for extension in group["extensions"].as_array().unwrap() {
      let extension = extension.as_str().unwrap();
      require_family(corrected_parser(None, Some(extension)), family);
      require_family(corrected_parser(Some("application/octet-stream"), Some(extension)), family);
      assert!(corrected_parser(Some("application/x-unregistered"), Some(extension)).is_none());
    }
    if let Some(prefixes) = group["prefix"].as_array() {
      for prefix in prefixes {
        require_family(corrected_parser(Some(&format!("{}fixture-language", prefix.as_str().unwrap())), None), family);
      }
    }
  }
  assert!(corrected_parser(None, None).is_none());
  assert!(corrected_parser(None, Some("unknown-native-extension")).is_none());
}

#[test]
fn conformance_native_suite_all_seed_prefixes_and_full_width_arithmetic() {
  let vectors = fixture();
  let mut failures = Vec::new();
  for case in vectors["cases"].as_array().unwrap().iter().chain(vectors["prefix_inputs"].as_array().unwrap()) {
    let bytes = body(case);
    let mime = case["mime"].as_str().unwrap();
    let filename = case["filename"].as_str().unwrap();
    for length in 0..=bytes.len() {
      let result =
        std::panic::catch_unwind(|| parse_native_corrected(&bytes[..length], Some(mime), None, filename, mime, length as u64, limits()));
      if result.is_err() {
        failures.push(format!("{filename}/{length}"));
      }
    }
  }
  assert!(failures.is_empty(), "panicking prefixes: {failures:?}");
  for case in vectors["arithmetic_inputs"].as_array().unwrap() {
    let bytes = body(case);
    let mime = case["mime"].as_str().unwrap();
    let parsed = parse_native_corrected(&bytes, Some(mime), None, case["filename"].as_str().unwrap(), mime, bytes.len() as u64, limits())
      .unwrap()
      .unwrap();
    assert_eq!(parsed["metadata"]["bitrate"], case["expected_bitrate"]);
  }
}

#[test]
fn conformance_native_metadata_limits_precede_copy_and_claimed_parser_work() {
  let vectors = fixture();
  let mut policy = limits();
  policy.maximum_scalar_bytes = 64;
  for case in vectors["cases"].as_array().unwrap() {
    let mime = case["mime"].as_str().unwrap();
    for (filename, stored) in [("n".repeat(65), "text/plain".to_string()), ("small".to_string(), "m".repeat(65))] {
      assert!(
        matches!(
          parse_native_corrected(b"not an archive or PDF", Some(mime), None, &filename, &stored, 0, policy),
          Some(Err(CorrectedNativeParserErrorV1::PolicyLimit { observed: 65 }))
        ),
        "{}",
        case["id"]
      );
      // No claim still means no value, independently of metadata size.
      assert!(parse_native_corrected(b"", None, None, &filename, &stored, 0, policy).is_none());
    }
  }
}

#[test]
fn conformance_native_wav_generated_rates_match_independent_wide_arithmetic() {
  let vectors = fixture();
  let case = &vectors["arithmetic_inputs"].as_array().unwrap()[0];
  let original = body(case);
  assert_eq!(&original[12..16], b"fmt ");
  let mut seed = 0xf281_34b9u32;
  for _ in 0..512 {
    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    let mut bytes = original.clone();
    // RIFF/WAVE fmt header's LE-u32 byte rate, independent of parser code.
    bytes[28..32].copy_from_slice(&seed.to_le_bytes());
    let parsed = parse_native_corrected(&bytes, Some("audio/wav"), None, "model.wav", "audio/wav", 1234, limits()).unwrap().unwrap();
    let expected = u128::from(seed) * 8;
    assert_eq!(u128::from(parsed["metadata"]["bitrate"].as_u64().unwrap()), expected);
  }
}
