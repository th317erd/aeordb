use std::cell::Cell;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::semantic_mutation_control::{
  SemanticMutationSourceFingerprintRequestV1, SemanticMutationSourceFingerprintV1, SemanticMutationSourceIdentityV1,
  fingerprint_semantic_mutation_sources_v1,
};
use sha2::Digest;

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap())
}

fn request(algorithm: HashAlgorithm, count: u64) -> SemanticMutationSourceFingerprintRequestV1 {
  SemanticMutationSourceFingerprintRequestV1 {
    hash_algorithm: algorithm,
    expected_record_count: count,
    maximum_path_bytes: 65535,
    maximum_workspace_bytes: 1 << 20,
  }
}

fn row(path: &str, algorithm: HashAlgorithm, byte: Option<u8>) -> SemanticMutationSourceIdentityV1 {
  SemanticMutationSourceIdentityV1 { path: path.into(), file_record_id: byte.map(|byte| vec![byte; algorithm.hash_length()]) }
}

fn independent_digest(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(bytes).to_vec(),
  }
}

fn failure(result: Result<SemanticMutationSourceFingerprintV1, SemanticCompilationErrorV1>) -> SemanticCompilationErrorV1 {
  match result {
    Err(error) => error,
    Ok(_) => panic!("invalid capture stream returned a fingerprint"),
  }
}

#[test]
fn source_fingerprint_matches_independently_framed_bytes_at_every_hash_width() {
  for algorithm in super::ALGORITHMS {
    let memory = memory();
    let width = algorithm.hash_length();
    // Literal UTF-8 byte lengths, explicitly framed independently of the
    // production stream. Neither a serializer nor its helpers create this.
    let mut bytes = b"aeordb.semantic-mutation-sources.v1\0\x1c\0\0\0/.aeordb-config/parsers.json".to_vec();
    bytes.extend_from_slice(&vec![0; width]);
    bytes.extend_from_slice(b"\x02\0\0\0/a");
    bytes.extend_from_slice(&vec![0x11; width]);
    bytes.extend_from_slice(b"\x03\0\0\0/\xc3\xa9");
    bytes.extend_from_slice(&vec![0x22; width]);
    assert_eq!(bytes.len(), 81 + 3 * width);
    let rows = [row("/.aeordb-config/parsers.json", algorithm, None), row("/a", algorithm, Some(0x11)), row("/é", algorithm, Some(0x22))];
    let fingerprint =
      fingerprint_semantic_mutation_sources_v1(request(algorithm, 3), rows.into_iter().map(Ok), &memory, &|| false).unwrap();
    assert_eq!(fingerprint.digest(), independent_digest(algorithm, &bytes));
    assert_eq!(fingerprint.record_count(), 3);
    assert_eq!(fingerprint.digest().len(), width);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, width as u64);
    drop(fingerprint);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn empty_absent_present_and_same_record_at_another_path_are_distinct() {
  for algorithm in super::ALGORITHMS {
    let memory = memory();
    let empty = fingerprint_semantic_mutation_sources_v1(request(algorithm, 0), std::iter::empty(), &memory, &|| false).unwrap();
    assert_eq!(empty.digest(), independent_digest(algorithm, b"aeordb.semantic-mutation-sources.v1\0"));
    let mut digests = vec![empty.digest().to_vec()];
    for input in [row("/a", algorithm, None), row("/a", algorithm, Some(1)), row("/b", algorithm, Some(1))] {
      let fingerprint = fingerprint_semantic_mutation_sources_v1(request(algorithm, 1), [Ok(input)], &memory, &|| false).unwrap();
      assert!(digests.iter().all(|previous| previous != fingerprint.digest()));
      digests.push(fingerprint.digest().to_vec());
    }
  }
}

#[test]
fn malformed_duplicate_unordered_and_incomplete_source_streams_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let mut invalid = Vec::new();
  for path in ["", "relative", "/a/", "/a//b", "/a/../b", " /a", "/a\0b"] {
    invalid.push(vec![row(path, algorithm, Some(1))]);
  }
  for bytes in [vec![], vec![0; 32], vec![1; 31], vec![1; 64]] {
    invalid.push(vec![SemanticMutationSourceIdentityV1 { path: "/a".into(), file_record_id: Some(bytes) }]);
  }
  invalid.push(vec![row("/a", algorithm, None), row("/a", algorithm, None)]);
  invalid.push(vec![row("/b", algorithm, None), row("/a", algorithm, None)]);
  for rows in invalid {
    let count = rows.len() as u64;
    let error = failure(fingerprint_semantic_mutation_sources_v1(request(algorithm, count), rows.into_iter().map(Ok), &memory, &|| false));
    assert!(matches!(error, SemanticCompilationErrorV1::InvalidSource { .. }), "{error}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
  for expected in [0, 2] {
    let error =
      failure(fingerprint_semantic_mutation_sources_v1(request(algorithm, expected), [Ok(row("/a", algorithm, None))], &memory, &|| false));
    assert!(matches!(error, SemanticCompilationErrorV1::InvalidSource { .. }), "{error}");
  }
}

#[test]
fn cancellation_and_workspace_refusal_precede_source_iteration() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let observed = Cell::new(0);
  for cancelled in [false, true] {
    let mut request = request(algorithm, 1);
    if !cancelled {
      request.maximum_workspace_bytes = 1;
    }
    let source = std::iter::once_with(|| {
      observed.set(observed.get() + 1);
      Ok(row("/a", algorithm, None))
    });
    let error = failure(fingerprint_semantic_mutation_sources_v1(request, source, &memory, &|| cancelled));
    if cancelled {
      assert!(matches!(error, SemanticCompilationErrorV1::Cancelled));
    } else {
      assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }));
    }
    assert_eq!(observed.get(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn source_failures_keep_their_identity_at_first_middle_and_final_boundaries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  for at in 0..=3 {
    for mode in 0..5 {
      let source = (0..=3).map(|index| {
        if index != at {
          return Ok(row(&format!("/a{index}"), algorithm, Some(1)));
        }
        Err(match mode {
          0 => SemanticCompilationErrorV1::Operational { path: "test-source", message: "read failed".into() },
          1 => SemanticCompilationErrorV1::Resource { path: "test-source", message: "read refused".into() },
          2 => SemanticCompilationErrorV1::DependencyUnavailable { path: "test-source", message: "dependency missing".into() },
          3 => SemanticCompilationErrorV1::InvalidSource { path: "test-source", message: "invalid captured source".into() },
          _ => SemanticCompilationErrorV1::Cancelled,
        })
      });
      let error = failure(fingerprint_semantic_mutation_sources_v1(request(algorithm, 3), source, &memory, &|| false));
      let matched = match (mode, error) {
        (0, SemanticCompilationErrorV1::Operational { path: "test-source", message }) => message == "read failed",
        (1, SemanticCompilationErrorV1::Resource { path: "test-source", message }) => message == "read refused",
        (2, SemanticCompilationErrorV1::DependencyUnavailable { path: "test-source", message }) => message == "dependency missing",
        (3, SemanticCompilationErrorV1::InvalidSource { path: "test-source", message }) => message == "invalid captured source",
        (4, SemanticCompilationErrorV1::Cancelled) => true,
        _ => false,
      };
      assert!(matched, "mode {mode} / boundary {at}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn cancellation_and_host_pressure_at_every_check_produce_no_partial_fingerprint() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let rows = || (0..3).map(|index| Ok(row(&format!("/a{index}"), algorithm, Some(1))));
  let checks = Cell::new(0);
  drop(
    fingerprint_semantic_mutation_sources_v1(request(algorithm, 3), rows(), &memory, &|| {
      checks.set(checks.get() + 1);
      false
    })
    .unwrap(),
  );
  let total = checks.get();
  assert!(total >= 5);
  for pressure in [false, true] {
    for at in 1..=total {
      checks.set(0);
      let error = failure(fingerprint_semantic_mutation_sources_v1(request(algorithm, 3), rows(), &memory, &|| {
        checks.set(checks.get() + 1);
        if checks.get() < at {
          return false;
        }
        if pressure {
          memory.update_host_sample(HostMemorySample { rss_bytes: 32 << 20, ..Default::default() }).unwrap();
          false
        } else {
          true
        }
      }));
      if pressure {
        assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }), "{error}");
      } else {
        assert!(matches!(error, SemanticCompilationErrorV1::Cancelled));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      memory.update_host_sample(HostMemorySample::default()).unwrap();
    }
  }
}

#[test]
fn path_budget_and_workspace_overflow_fail_without_unbounded_reads() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  for maximum_path_bytes in [0, usize::MAX] {
    let observed = Cell::new(false);
    let source = std::iter::once_with(|| {
      observed.set(true);
      Ok(row("/", algorithm, None))
    });
    let request =
      SemanticMutationSourceFingerprintRequestV1 { maximum_path_bytes, maximum_workspace_bytes: usize::MAX, ..request(algorithm, 1) };
    assert!(matches!(
      failure(fingerprint_semantic_mutation_sources_v1(request, source, &memory, &|| false)),
      SemanticCompilationErrorV1::Resource { .. }
    ));
    assert!(!observed.get());
  }
  let bounded = SemanticMutationSourceFingerprintRequestV1 { maximum_path_bytes: 2, ..request(algorithm, 1) };
  drop(fingerprint_semantic_mutation_sources_v1(bounded, [Ok(row("/a", algorithm, None))], &memory, &|| false).unwrap());
  let error = failure(fingerprint_semantic_mutation_sources_v1(bounded, [Ok(row("/ab", algorithm, None))], &memory, &|| false));
  assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }), "{error}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn overallocated_source_buffers_cannot_escape_the_declared_retained_budget() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let request = SemanticMutationSourceFingerprintRequestV1 { maximum_path_bytes: 16, ..request(algorithm, 1) };
  for path_buffer in [true, false] {
    let mut row = row("/a", algorithm, Some(1));
    if path_buffer {
      let mut path = String::with_capacity(8192);
      path.push_str("/a");
      row.path = path;
    } else {
      let mut identity = Vec::with_capacity(8192);
      identity.resize(32, 1);
      row.file_record_id = Some(identity);
    }
    let error = failure(fingerprint_semantic_mutation_sources_v1(request, [Ok(row)], &memory, &|| false));
    assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }), "{error}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn admission_refusal_does_not_even_construct_the_source_iterator() {
  struct Unopened<'a>(&'a Cell<bool>);
  impl IntoIterator for Unopened<'_> {
    type Item = Result<SemanticMutationSourceIdentityV1, SemanticCompilationErrorV1>;
    type IntoIter = std::iter::Empty<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
      self.0.set(true);
      std::iter::empty()
    }
  }
  let algorithm = HashAlgorithm::Blake3_256;
  for mode in 0..3 {
    let memory = memory();
    let opened = Cell::new(false);
    let mut request = request(algorithm, 0);
    if mode == 0 {
      request.maximum_workspace_bytes = 1;
    } else if mode == 1 {
      memory.update_host_sample(HostMemorySample { rss_bytes: 32 << 20, ..Default::default() }).unwrap();
    }
    let error = failure(fingerprint_semantic_mutation_sources_v1(request, Unopened(&opened), &memory, &|| mode == 2));
    assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. } | SemanticCompilationErrorV1::Cancelled));
    assert!(!opened.get());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn untrusted_record_count_cannot_request_bulk_allocation_or_unbounded_extra_reads() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  for expected in [0, 1, u64::MAX] {
    let reads = Cell::new(0);
    let source = (0..3).map(|index| {
      reads.set(reads.get() + 1);
      Ok(row(&format!("/a{index}"), algorithm, None))
    });
    let bounded =
      SemanticMutationSourceFingerprintRequestV1 { maximum_path_bytes: 16, maximum_workspace_bytes: 8192, ..request(algorithm, expected) };
    let error = failure(fingerprint_semantic_mutation_sources_v1(bounded, source, &memory, &|| false));
    assert!(matches!(error, SemanticCompilationErrorV1::InvalidSource { .. }), "{error}");
    assert_eq!(reads.get(), if expected == u64::MAX { 3 } else { expected + 1 });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
