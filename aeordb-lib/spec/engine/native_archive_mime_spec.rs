//! Next U1 conformance slice: archive parsers must preserve corrected MIME metadata.
use aeordb::engine::{HashAlgorithm, RequestContext, StorageEngine};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
use aeordb::engine::v4::index_native_parser::NativeIndexParserExecutorV1;
use aeordb::engine::v4::index_native_source::{NativeIndexFileRevisionSourceV1, NativeIndexSourceLimitsV1};
use aeordb::engine::v4::index_producer_collector::{IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1};
use aeordb::engine::v4::index_producer_source::IndexFileRevisionSourceV1;
use aeordb::engine::v4::parser_plan::ParserCandidateKind;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use std::io::{Cursor, Write};

#[path = "../helpers/native_semantic_dependencies.rs"]
mod native_semantic_dependencies;

fn archive(format: &str) -> Vec<u8> {
  let entries: Vec<(&str, &[u8])> = match format {
    "docx" => vec![("word/document.xml", b"<w:document><w:p>Hello</w:p></w:document>")],
    "xlsx" => vec![("xl/workbook.xml", b"<workbook><sheet name=\"Hello\"/></workbook>")],
    "odt" => vec![("mimetype", b"application/vnd.oasis.opendocument.text"), ("content.xml", b"<text:p>Hello</text:p>")],
    "ods" => vec![("mimetype", b"application/vnd.oasis.opendocument.spreadsheet"), ("content.xml", b"<text:p>Hello</text:p>")],
    _ => panic!("unknown test archive"),
  };
  let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
  for (name, bytes) in entries {
    writer.start_file(name, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    writer.write_all(bytes).unwrap();
  }
  writer.finish().unwrap().into_inner()
}

fn exercise(legacy: bool, maximum_scalar_bytes: Option<u64>) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("archive-mime.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.ensure_root_directory(&context).unwrap();
  let family = if legacy { "legacy" } else { "corrected" };
  let mut encoded = std::fs::read(format!(
    "{}/spec/fixtures/v4/value-store-definition-v1/avst-blake3-256-json-{family}-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  native_semantic_dependencies::pin_native_semantics(&mut encoded, HashAlgorithm::Blake3_256);
  let mut definition = decode_value_store_definition(&encoded, HashAlgorithm::Blake3_256).unwrap();
  if let Some(maximum) = maximum_scalar_bytes {
    let native_candidate =
      definition.parser_plan.candidates.iter_mut().find(|candidate| candidate.kind == ParserCandidateKind::NativeSuite).unwrap();
    native_candidate.policy.max_scalar_bytes = maximum;
  }
  for (format, detected) in [
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
  ] {
    let path = format!("/document.{format}");
    operations.store_file_buffered(&context, &path, &archive(format), Some("application/octet-stream")).unwrap();
    let root = engine.head_hash().unwrap();
    let source = NativeIndexFileRevisionSourceV1::new(&engine, NativeIndexSourceLimitsV1::new(16 << 20, 16 << 20, 64).unwrap());
    let loaded = source.load_file_revision(&root, &path).unwrap().unwrap();
    let revision = loaded.revision();
    let stored_types = if legacy || maximum_scalar_bytes.is_some() {
      vec!["application/octet-stream".to_string()]
    } else {
      vec![
        "application/octet-stream".to_string(),
        format!("{}; charset=\"utf-8\"", detected.to_ascii_uppercase()),
        "!abc/plain".to_string(),
      ]
    };
    for stored in stored_types {
      let mut record = revision.file_record.clone();
      record.content_type = Some(stored.clone());
      let outcome = NativeIndexParserExecutorV1::new(&engine)
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
      if maximum_scalar_bytes.is_some_and(|maximum| maximum < stored.len() as u64) {
        assert!(matches!(outcome, IndexParserOutcomeV1::DeterministicUnindexable(_)), "over-limit stored MIME: {format}/{stored}");
        continue;
      }
      let IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Map(value)) = outcome else {
        panic!("archive not parsed: {format}/{stored}");
      };
      let Some(CanonicalConfigValueV1::Map(metadata)) = value.get("metadata") else {
        panic!("missing metadata");
      };
      let expected = if legacy { detected } else { &stored };
      assert_eq!(metadata.get("content_type"), Some(&CanonicalConfigValueV1::String(expected.to_string())), "{format}/{stored}");
    }
  }
}

#[test]
fn corrected_archive_outputs_preserve_original_stored_mime_in_all_four_formats() {
  exercise(false, None);
}

#[test]
fn legacy_archive_outputs_retain_their_original_detected_mime_contract() {
  exercise(true, None);
}

#[test]
fn corrected_archive_scalar_limit_applies_to_stored_mime_not_detected_format() {
  // The stored value is 24 bytes; each detected archive MIME exceeds 32.
  exercise(false, Some(32));
}

#[test]
fn corrected_archive_scalar_limit_rejects_oversized_stored_mime() {
  exercise(false, Some(23));
}
