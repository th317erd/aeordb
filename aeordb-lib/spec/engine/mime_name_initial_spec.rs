//! RFC 6838 section 4.2 name initials, as adopted by frozen Round 9.
use aeordb::engine::{HashAlgorithm, RequestContext, StorageEngine};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
use aeordb::engine::v4::index_native_parser::NativeIndexParserExecutorV1;
use aeordb::engine::v4::index_native_source::{NativeIndexFileRevisionSourceV1, NativeIndexSourceLimitsV1};
use aeordb::engine::v4::index_producer_collector::{IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1};
use aeordb::engine::v4::index_producer_source::IndexFileRevisionSourceV1;
use aeordb::engine::v4::parser_plan::{decode_parser_resolution_plan, encode_parser_resolution_plan};
use aeordb::engine::v4::value_store::decode_value_store_definition;

const PUNCTUATION: &[u8] = b"!#$&^_.+-";

#[path = "../helpers/native_semantic_dependencies.rs"]
mod native_semantic_dependencies;

fn fixture(family: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{family}/{name}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn corrected_parser_writer_requires_alphanumeric_type_and_subtype_initials() {
  let original = fixture("parser-resolution-plan-v1", "aprp-blake3-256-automatic-valid");
  let mut plan = decode_parser_resolution_plan(&original).unwrap();
  // Keep one registry candidate followed by the raw/native footer.
  plan.candidates.remove(1);
  for initial in PUNCTUATION {
    for media_type in [format!("{}abc/plain", *initial as char), format!("text/{}abc", *initial as char)] {
      let mut candidate_plan = plan.clone();
      candidate_plan.candidates[0].match_bytes = media_type.as_bytes();
      assert!(encode_parser_resolution_plan(&candidate_plan).is_err(), "invalid initial: {media_type}");
    }
  }
  for media_type in ["a/b", "0/1", "a!b/c+d", "a.b/c_d", "a-b/c^d", "a#b/c$d", "a&b/c"] {
    let mut candidate_plan = plan.clone();
    candidate_plan.candidates[0].match_bytes = media_type.as_bytes();
    assert!(encode_parser_resolution_plan(&candidate_plan).is_ok(), "valid restricted name: {media_type}");
  }
}

#[test]
fn corrected_parser_reader_checks_initials_in_independent_bytes_without_changing_legacy_matches() {
  for profile in ["blake3-256", "sha512"] {
    for legacy in [false, true] {
      let suffix = if legacy { "automatic-legacy" } else { "automatic" };
      let original = fixture("parser-resolution-plan-v1", &format!("aprp-{profile}-{suffix}-valid"));
      let length = u32::from_le_bytes(original[64..68].try_into().unwrap()) as usize;
      let slash = original[80..80 + length].iter().position(|byte| *byte == b'/').unwrap();
      for initial in PUNCTUATION {
        for offset in [80, 80 + slash + 1] {
          let mut bytes = original.clone();
          bytes[offset] = *initial;
          assert_eq!(decode_parser_resolution_plan(&bytes).is_ok(), legacy, "{profile}/{legacy}/{initial}/{offset}");
        }
      }
      for initial in b"ab09" {
        for offset in [80, 80 + slash + 1] {
          let mut bytes = original.clone();
          bytes[offset] = *initial;
          assert!(decode_parser_resolution_plan(&bytes).is_ok(), "valid initial: {profile}/{legacy}/{initial}/{offset}");
        }
      }
    }
  }
}

#[test]
fn corrected_native_mime_treats_bad_initials_as_generic_without_changing_legacy_routing() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("mime-name-initial.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let context = RequestContext::system();
  let operations = DirectoryOps::new(&engine);
  operations.ensure_root_directory(&context).unwrap();
  operations.store_file_buffered(&context, "/doc.txt", b"Hello", Some("text/plain")).unwrap();
  let root = engine.head_hash().unwrap();
  let limits = NativeIndexSourceLimitsV1::new(16 << 20, 16 << 20, 64).unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, limits);
  let loaded = source.load_file_revision(&root, "/doc.txt").unwrap().unwrap();
  let revision = loaded.revision();
  for legacy in [false, true] {
    let family = if legacy { "legacy" } else { "corrected" };
    let mut encoded = fixture("value-store-definition-v1", &format!("avst-blake3-256-json-{family}-valid"));
    native_semantic_dependencies::pin_native_semantics(&mut encoded, HashAlgorithm::Blake3_256);
    let definition = decode_value_store_definition(&encoded, HashAlgorithm::Blake3_256).unwrap();
    for initial in PUNCTUATION {
      for media_type in [format!("{}abc/plain", *initial as char), format!("text/{}abc", *initial as char)] {
        let mut record = revision.file_record.clone();
        record.content_type = Some(media_type.clone());
        let result = NativeIndexParserExecutorV1::new(&engine)
          .parse(IndexParserExecutionRequestV1::new(
            &root,
            &revision.revision_hash,
            &record,
            &definition.parser_plan,
            &definition.dependencies,
            64 << 20,
            &|| false,
          ))
          .unwrap();
        if legacy {
          assert_eq!(result, IndexParserOutcomeV1::NotApplicable, "{media_type}");
        } else {
          let IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Map(value)) = result else {
            panic!("invalid MIME must allow .txt fallback: {media_type}");
          };
          assert_eq!(value.get("text"), Some(&CanonicalConfigValueV1::String("Hello".to_string())));
          let Some(CanonicalConfigValueV1::Map(metadata)) = value.get("metadata") else {
            panic!("native metadata must be retained");
          };
          assert_eq!(metadata.get("content_type"), Some(&CanonicalConfigValueV1::String(media_type)));
        }
      }
    }
  }
}
