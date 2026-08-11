use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_mark::{
  GcMarkArtifactV1, MarkMutationJournalSegmentWriteV1, MarkMutationOperationV1, MarkMutationRecordWriteV1, decode_gc_mark_artifact,
  encode_mark_mutation_journal_segment, mark_mutation_journal_records_v1, validate_mark_mutation_journal_chain,
};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture_label(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("mark convergence fixtures use only frozen hash profiles"),
  }
}

fn sequence<const N: usize>(start: u8) -> [u8; N] {
  let mut bytes = [0u8; N];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(u8::try_from(index).unwrap());
  }
  bytes
}

fn repeated_hash(algorithm: HashAlgorithm, value: u8) -> Vec<u8> {
  vec![value; algorithm.hash_length()]
}

fn physical_incarnation(algorithm: HashAlgorithm, value: u8, write_sequence: u64) -> Vec<u8> {
  let width = algorithm.hash_length();
  let mut bytes = Vec::with_capacity(24 + 2 * width);
  bytes.extend_from_slice(&repeated_hash(algorithm, value));
  bytes.extend_from_slice(&repeated_hash(algorithm, value.wrapping_add(1)));
  bytes.extend_from_slice(&(4096 + write_sequence * 512).to_le_bytes());
  bytes.extend_from_slice(&write_sequence.to_le_bytes());
  bytes.extend_from_slice(&256u32.to_le_bytes());
  bytes.push(2);
  bytes.push(1);
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes
}

struct MutationValues {
  mutation_id: Vec<u8>,
  root_before: Vec<u8>,
  root_after: Vec<u8>,
  logical_key: Vec<u8>,
  incarnation: Vec<u8>,
}

fn mutation_values(algorithm: HashAlgorithm, value: u8, write_sequence: u64) -> MutationValues {
  MutationValues {
    mutation_id: repeated_hash(algorithm, value),
    root_before: repeated_hash(algorithm, value.wrapping_add(1)),
    root_after: repeated_hash(algorithm, value.wrapping_add(2)),
    logical_key: repeated_hash(algorithm, value.wrapping_add(3)),
    incarnation: physical_incarnation(algorithm, value.wrapping_add(4), write_sequence),
  }
}

fn mutation_record<'a>(
  publication_sequence: u64,
  values: &'a MutationValues,
  operation: MarkMutationOperationV1,
) -> MarkMutationRecordWriteV1<'a> {
  MarkMutationRecordWriteV1 {
    publication_sequence,
    mutation_id: &values.mutation_id,
    root_before: &values.root_before,
    root_after: &values.root_after,
    published_logical_key: &values.logical_key,
    new_incarnation: &values.incarnation,
    operation,
  }
}

#[test]
fn mutation_writer_matches_independent_both_width_fixtures_and_iterator_fields() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fs::read(fixture_root().join(format!("agca-{}-mark-mutation-journal-reset.bin", fixture_label(algorithm)))).unwrap();
    let GcMarkArtifactV1::MutationJournal(journal) = decode_gc_mark_artifact(&fixture, algorithm).unwrap() else {
      panic!("expected mutation journal fixture");
    };
    let records = mark_mutation_journal_records_v1(&journal, algorithm).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].publication_sequence, 800);
    assert_eq!(records[1].publication_sequence, 801);
    assert_eq!(records[0].operation, MarkMutationOperationV1::Create);
    assert_eq!(records[1].operation, MarkMutationOperationV1::Replace);
    assert_eq!(records[0].new_incarnation.write_sequence, 701);
    assert_eq!(records[1].new_incarnation.write_sequence, 702);

    let writes = records
      .iter()
      .map(|record| MarkMutationRecordWriteV1 {
        publication_sequence: record.publication_sequence,
        mutation_id: record.mutation_id,
        root_before: record.root_before,
        root_after: record.root_after,
        published_logical_key: record.published_logical_key,
        new_incarnation: record.new_incarnation_bytes,
        operation: record.operation,
      })
      .collect::<Vec<_>>();
    let database_id: [u8; 16] = journal.database_id.try_into().unwrap();
    let run_id: [u8; 16] = journal.run_id.try_into().unwrap();
    let encoded = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      run_id: &run_id,
      generation: journal.generation,
      segment_ordinal: journal.segment_sequence,
      previous_segment_hash: None,
      records: &writes,
    })
    .unwrap();
    assert_eq!(encoded.value, fixture);
    assert_eq!(encoded.key, journal.key);
  }
}

#[test]
fn segment_chain_allows_a_publication_to_span_bounded_segments_but_not_regress() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = sequence(0x31);
  let run_id = sequence(0x51);
  let first_values = mutation_values(algorithm, 0x11, 10);
  let second_values = mutation_values(algorithm, 0x21, 11);
  let first_record = mutation_record(20, &first_values, MarkMutationOperationV1::Create);
  let second_record = mutation_record(20, &second_values, MarkMutationOperationV1::Replace);
  let first = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 40,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[first_record],
  })
  .unwrap();
  let second = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 40,
    segment_ordinal: 2,
    previous_segment_hash: Some(&first.key),
    records: &[second_record],
  })
  .unwrap();
  let GcMarkArtifactV1::MutationJournal(first) = decode_gc_mark_artifact(&first.value, algorithm).unwrap() else {
    panic!("expected first mutation segment");
  };
  let GcMarkArtifactV1::MutationJournal(second) = decode_gc_mark_artifact(&second.value, algorithm).unwrap() else {
    panic!("expected second mutation segment");
  };
  validate_mark_mutation_journal_chain(&first, &second).unwrap();

  let regressing_values = mutation_values(algorithm, 0x10, 12);
  let regressing_record = mutation_record(20, &regressing_values, MarkMutationOperationV1::Repair);
  let regressing = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 40,
    segment_ordinal: 2,
    previous_segment_hash: Some(&first.key),
    records: &[regressing_record],
  })
  .unwrap();
  let GcMarkArtifactV1::MutationJournal(regressing) = decode_gc_mark_artifact(&regressing.value, algorithm).unwrap() else {
    panic!("expected regressing mutation segment");
  };
  assert!(validate_mark_mutation_journal_chain(&first, &regressing).is_err());
}

#[test]
fn mutation_writer_rejects_empty_malformed_or_noncanonical_input() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = sequence(0x31);
  let run_id = sequence(0x51);
  assert!(encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 1,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[],
  })
  .is_err());

  let values = mutation_values(algorithm, 0x11, 10);
  let mut wrong_width = mutation_record(1, &values, MarkMutationOperationV1::Create);
  wrong_width.mutation_id = &values.mutation_id[..31];
  assert!(encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 1,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[wrong_width],
  })
  .is_err());

  let first = mutation_record(2, &values, MarkMutationOperationV1::Create);
  let second = mutation_record(1, &values, MarkMutationOperationV1::Create);
  assert!(encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 1,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[first, second],
  })
  .is_err());
}

#[test]
fn mark_mutation_runtime_remains_disconnected_from_live_v3_and_service_paths() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  for relative in [
    "engine/gc.rs",
    "engine/namespace_mutation.rs",
    "engine/storage_engine.rs",
    "engine/task_worker.rs",
    "server/mod.rs",
    "server/routes.rs",
  ] {
    let source = fs::read_to_string(root.join(relative)).unwrap();
    assert!(!source.contains("encode_mark_mutation_journal_segment"), "mark writer escaped into {relative}");
    assert!(!source.contains("mark_mutation_journal_records_v1"), "mark iterator escaped into {relative}");
  }
}
