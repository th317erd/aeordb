use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::v4::index_native_parser::NativeIndexParserExecutorV1;
use aeordb::engine::v4::index_native_source::{NativeIndexFileRevisionSourceV1, NativeIndexSourceLimitsV1};
use aeordb::engine::v4::index_producer_collector::{IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1};
use aeordb::engine::v4::index_producer_source::IndexFileRevisionSourceV1;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use aeordb::engine::{HashAlgorithm, RequestContext, StorageEngine};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Cursor, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const MIB: usize = 1_024 * 1_024;
const MAXIMUM_CORRECTED_PARSE_GROWTH: usize = 48 * MIB;
const NEAR_LIMIT_ARCHIVE_EXPANDED_BYTES: usize = 15 * MIB;
const OVERSIZED_ARCHIVE_EXPANDED_BYTES: usize = 24 * MIB;
const HIGH_NODE_CONTAINERS: usize = 40_000;
const HIGH_NODE_MEMBERS: usize = 32;
const HIGH_NODE_MAPS: usize = 60_000;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static CURRENT_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PEAK_ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc(layout) };
    if !pointer.is_null() {
      allocation_added(layout.size());
    }
    pointer
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc_zeroed(layout) };
    if !pointer.is_null() {
      allocation_added(layout.size());
    }
    pointer
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    unsafe { System.dealloc(pointer, layout) };
    CURRENT_ALLOCATED.fetch_sub(layout.size(), Ordering::SeqCst);
  }

  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let replacement = unsafe { System.realloc(pointer, layout, new_size) };
    if !replacement.is_null() {
      if new_size >= layout.size() {
        allocation_added(new_size - layout.size());
      } else {
        CURRENT_ALLOCATED.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
      }
    }
    replacement
  }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocation_added(bytes: usize) {
  let current = CURRENT_ALLOCATED.fetch_add(bytes, Ordering::SeqCst).saturating_add(bytes);
  let mut peak = PEAK_ALLOCATED.load(Ordering::SeqCst);
  while current > peak {
    match PEAK_ALLOCATED.compare_exchange_weak(peak, current, Ordering::SeqCst, Ordering::SeqCst) {
      Ok(_) => break,
      Err(observed) => peak = observed,
    }
  }
}

fn reset_peak() -> usize {
  let baseline = CURRENT_ALLOCATED.load(Ordering::SeqCst);
  PEAK_ALLOCATED.store(baseline, Ordering::SeqCst);
  baseline
}

fn peak_growth(baseline: usize) -> usize {
  PEAK_ALLOCATED.load(Ordering::SeqCst).saturating_sub(baseline)
}

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("native-index-parser-resource.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

fn corrected_definition_bytes() -> Vec<u8> {
  std::fs::read(format!(
    "{}/spec/fixtures/v4/value-store-definition-v1/avst-blake3-256-json-corrected-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap()
}

fn parse_file(engine: &StorageEngine, root: &[u8], path: &str) -> IndexParserOutcomeV1 {
  let definition_bytes = corrected_definition_bytes();
  let definition = decode_value_store_definition(&definition_bytes, ALGORITHM).unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(engine, NativeIndexSourceLimitsV1::new(64 * MIB as u32, 16 * MIB as u32, 64).unwrap());
  let revision = source.load_file_revision(root, path).unwrap().unwrap();
  let revision = revision.revision();
  NativeIndexParserExecutorV1::new(engine)
    .parse(IndexParserExecutionRequestV1::new(
      root,
      &revision.revision_hash,
      &revision.file_record,
      &definition.parser_plan,
      &definition.dependencies,
      64 * MIB as u64,
      &|| false,
    ))
    .unwrap()
}

fn build_compressed_archive(kind: ArchiveKind, expanded_bytes: usize) -> Vec<u8> {
  let cursor = Cursor::new(Vec::new());
  let mut writer = zip::ZipWriter::new(cursor);
  let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(9));
  let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
  match kind {
    ArchiveKind::Docx => {
      writer.start_file("word/document.xml", deflated).unwrap();
      writer.write_all(b"<w:document><w:body><w:p><w:r><w:t>").unwrap();
      write_repeated(&mut writer, expanded_bytes);
      writer.write_all(b"</w:t></w:r></w:p></w:body></w:document>").unwrap();
    }
    ArchiveKind::Odt => {
      writer.start_file("mimetype", stored).unwrap();
      writer.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
      writer.start_file("content.xml", deflated).unwrap();
      writer.write_all(b"<office:document-content><text:p>").unwrap();
      write_repeated(&mut writer, expanded_bytes);
      writer.write_all(b"</text:p></office:document-content>").unwrap();
    }
  }
  writer.finish().unwrap().into_inner()
}

fn write_repeated(writer: &mut zip::ZipWriter<Cursor<Vec<u8>>>, bytes: usize) {
  let block = [b'x'; 8 * 1_024];
  for _ in 0..bytes / block.len() {
    writer.write_all(&block).unwrap();
  }
  writer.write_all(&block[..bytes % block.len()]).unwrap();
}

#[derive(Clone, Copy)]
enum ArchiveKind {
  Docx,
  Odt,
}

fn build_cumulative_docx(entry_bytes: usize) -> Vec<u8> {
  let cursor = Cursor::new(Vec::new());
  let mut writer = zip::ZipWriter::new(cursor);
  let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(9));
  writer.start_file("docProps/core.xml", options).unwrap();
  writer.write_all(b"<root>").unwrap();
  write_repeated(&mut writer, entry_bytes);
  writer.write_all(b"</root>").unwrap();
  writer.start_file("word/document.xml", options).unwrap();
  writer.write_all(b"<w:document><w:body><w:p><w:r><w:t>").unwrap();
  write_repeated(&mut writer, entry_bytes);
  writer.write_all(b"</w:t></w:r></w:p></w:body></w:document>").unwrap();
  writer.finish().unwrap().into_inner()
}

fn build_metadata_amplification_docx(metadata_bytes: usize) -> Vec<u8> {
  let cursor = Cursor::new(Vec::new());
  let mut writer = zip::ZipWriter::new(cursor);
  let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(9));
  let tags =
    ["dc:title", "dc:creator", "dc:subject", "dc:description", "cp:keywords", "cp:lastModifiedBy", "dcterms:created", "dcterms:modified"];
  writer.start_file("docProps/core.xml", options).unwrap();
  for tag in tags {
    write!(writer, "<{tag}>").unwrap();
  }
  write_repeated(&mut writer, metadata_bytes);
  for tag in tags.into_iter().rev() {
    write!(writer, "</{tag}>").unwrap();
  }
  writer.start_file("word/document.xml", options).unwrap();
  writer.write_all(b"<w:document><w:body><w:p><w:r><w:t>small</w:t></w:r></w:p></w:body></w:document>").unwrap();
  writer.finish().unwrap().into_inner()
}

fn build_keyword_amplification_odt(keywords: usize) -> Vec<u8> {
  let cursor = Cursor::new(Vec::new());
  let mut writer = zip::ZipWriter::new(cursor);
  let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(9));
  let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
  writer.start_file("mimetype", stored).unwrap();
  writer.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
  writer.start_file("content.xml", deflated).unwrap();
  writer.write_all(b"<office:document-content><text:p>small</text:p></office:document-content>").unwrap();
  writer.start_file("meta.xml", deflated).unwrap();
  writer.write_all(b"<office:document-meta><office:meta>").unwrap();
  for _ in 0..keywords {
    writer.write_all(b"<meta:keyword>x</meta:keyword>").unwrap();
  }
  writer.write_all(b"</office:meta></office:document-meta>").unwrap();
  writer.finish().unwrap().into_inner()
}

#[test]
fn corrected_compressed_office_and_odf_expansion_has_a_measured_allocation_bound() {
  let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  for expanded_bytes in [NEAR_LIMIT_ARCHIVE_EXPANDED_BYTES, OVERSIZED_ARCHIVE_EXPANDED_BYTES] {
    for (kind, extension, content_type) in [
      (ArchiveKind::Docx, "docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
      (ArchiveKind::Odt, "odt", "application/vnd.oasis.opendocument.text"),
    ] {
      let directory = tempfile::tempdir().unwrap();
      let engine = create_engine(&directory);
      let path = format!("/docs/amplified-{expanded_bytes}.{extension}");
      let archive = build_compressed_archive(kind, expanded_bytes);
      assert!(archive.len() < 128 * 1_024, "fixture stopped exercising compressed expansion");
      DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), &path, &archive, Some(content_type)).unwrap();
      let root = engine.head_hash().unwrap();
      drop(archive);

      let baseline = reset_peak();
      assert!(matches!(parse_file(&engine, &root, &path), IndexParserOutcomeV1::DeterministicUnindexable(_)));
      let growth = peak_growth(baseline);
      eprintln!("corrected archive parse {path}: allocator peak growth {growth} bytes");
      assert!(growth <= MAXIMUM_CORRECTED_PARSE_GROWTH, "{path} allocator peak grew by {growth} bytes");
    }
  }
}

#[test]
fn corrected_high_node_json_stops_during_deserialization_with_a_measured_allocation_bound() {
  let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  let mut body = Vec::with_capacity(HIGH_NODE_CONTAINERS * (HIGH_NODE_MEMBERS * 5 + 2));
  body.push(b'[');
  for container in 0..HIGH_NODE_CONTAINERS {
    if container > 0 {
      body.push(b',');
    }
    body.push(b'[');
    for member in 0..HIGH_NODE_MEMBERS {
      if member > 0 {
        body.push(b',');
      }
      body.extend_from_slice(b"null");
    }
    body.push(b']');
  }
  body.push(b']');
  let mut maps = Vec::with_capacity(HIGH_NODE_MAPS * 11 + 2);
  maps.push(b'[');
  for container in 0..HIGH_NODE_MAPS {
    if container > 0 {
      maps.push(b',');
    }
    maps.extend_from_slice(br#"{"k":null}"#);
  }
  maps.push(b']');

  for (path, body) in [("/docs/high-node-arrays.json", body), ("/docs/high-node-maps.json", maps)] {
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), path, &body, Some("application/json")).unwrap();
    let root = engine.head_hash().unwrap();
    drop(body);

    let baseline = reset_peak();
    assert!(matches!(parse_file(&engine, &root, path), IndexParserOutcomeV1::DeterministicUnindexable(_)));
    let growth = peak_growth(baseline);
    eprintln!("corrected high-node JSON parse {path}: allocator peak growth {growth} bytes");
    assert!(growth <= MAXIMUM_CORRECTED_PARSE_GROWTH, "{path} allocator peak grew by {growth} bytes");
  }
}

#[test]
fn corrected_archive_cumulative_and_metadata_amplification_share_the_same_measured_bound() {
  let _guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
  for (path, archive) in [
    ("/docs/cumulative.docx", build_cumulative_docx(9 * MIB)),
    ("/docs/metadata.docx", build_metadata_amplification_docx(15 * MIB)),
    ("/docs/keywords.odt", build_keyword_amplification_odt(70_000)),
  ] {
    let directory = tempfile::tempdir().unwrap();
    let engine = create_engine(&directory);
    assert!(archive.len() < 128 * 1_024, "fixture stopped exercising compressed expansion");
    let content_type = if path.ends_with(".odt") {
      "application/vnd.oasis.opendocument.text"
    } else {
      "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    };
    DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), path, &archive, Some(content_type)).unwrap();
    let root = engine.head_hash().unwrap();
    drop(archive);

    let baseline = reset_peak();
    assert!(matches!(parse_file(&engine, &root, path), IndexParserOutcomeV1::DeterministicUnindexable(_)));
    let growth = peak_growth(baseline);
    eprintln!("corrected archive policy parse {path}: allocator peak growth {growth} bytes");
    assert!(growth <= MAXIMUM_CORRECTED_PARSE_GROWTH, "{path} allocator peak grew by {growth} bytes");
  }
}
