use super::{fixture, measure, measure_nth};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::semantic_mutation_control::{decode_semantic_mutation_checkpoint, decode_semantic_mutation_selection};
use aeordb::engine::v4::system_control::decode_system_control;
use aeordb::engine::HashAlgorithm;
use sha2::Digest;

fn reseal(bytes: &mut [u8]) {
  let end = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..end]);
  bytes[end..].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn semantic_task_envelope_identity_allocations_fail_with_typed_errors() {
  for (slug, length) in [("task", 16), ("checkpoint", 24)] {
    let bytes = fixture("system-control-v1", &format!("control-blake3-256-semantic-mutation-{slug}-valid"));
    let (result, allocations) = measure(length, || decode_system_control(&bytes, HashAlgorithm::Blake3_256));
    assert!(allocations.injected_failure);
    let error = result.unwrap_err();
    assert_eq!(error.code(), "semantic_task_identity_allocation");
    assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
    assert!(error.is_allocation_failure());
  }
}

#[test]
fn semantic_checkpoint_maximum_cursor_is_borrowed_with_only_fixed_identity_allocation() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let width = algorithm.hash_length();
    let mut bytes = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-checkpoint-valid"));
    bytes.truncate(bytes.len() - 4);
    bytes[120..122].copy_from_slice(&2u16.to_le_bytes());
    bytes[122..124].copy_from_slice(&1u16.to_le_bytes());
    bytes[124..128].copy_from_slice(&65_535u32.to_le_bytes());
    bytes[200 + 4 * width..200 + 6 * width].fill(0);
    bytes.push(b'/');
    bytes.resize(bytes.len() + 65_534, b'x');
    let body_length = (bytes.len() - 32) as u32;
    bytes[24..28].copy_from_slice(&body_length.to_le_bytes());
    bytes[8..12].copy_from_slice(&(body_length + 36).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    reseal(&mut bytes);
    let (result, allocations) = measure(0, || decode_semantic_mutation_checkpoint(&bytes, algorithm));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(allocations.total, 24);
    assert_eq!(allocations.maximum, 24);
    let checkpoint = result.unwrap();
    let (edges, allocations) = measure(0, || checkpoint.references().count());
    assert_eq!(edges, 3);
    assert_eq!(allocations.total, 0);
  }
}

#[test]
fn semantic_task_selected_digest_allocation_failure_is_not_corruption_or_abort() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let checkpoint = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-checkpoint-valid"));
    let mut task = fixture("system-control-v1", &format!("control-{profile}-semantic-mutation-task-valid"));
    task[128..130].copy_from_slice(&4u16.to_le_bytes());
    let digest = if algorithm == HashAlgorithm::Blake3_256 {
      blake3::hash(&checkpoint).as_bytes().to_vec()
    } else {
      sha2::Sha512::digest(&checkpoint).to_vec()
    };
    task[144..144 + algorithm.hash_length()].copy_from_slice(&digest);
    reseal(&mut task);
    let (result, allocations) = measure(0, || decode_semantic_mutation_selection(&task, &checkpoint, algorithm));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(allocations.total, 16 + 24 + algorithm.hash_length());
    let (result, allocations) = measure(algorithm.hash_length(), || decode_semantic_mutation_selection(&task, &checkpoint, algorithm));
    assert!(allocations.injected_failure);
    let error = result.unwrap_err();
    assert_eq!(error.code(), "semantic_task_digest_allocation");
    assert!(error.is_allocation_failure());
  }
}

#[test]
fn source_fingerprint_output_allocation_refusal_releases_memory_and_retries() {
  use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
  use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
  use aeordb::engine::v4::semantic_mutation_control::{SemanticMutationSourceFingerprintRequestV1, fingerprint_semantic_mutation_sources_v1};
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    // The shared coordinator is an established engine owner, not the digest
    // output. Some platforms allocate its mutex lazily at the same byte size
    // as a hash. Measure that initialization without denying it, then inject
    // refusal into the fingerprint's actual fallible output allocation.
    let (initialization, allocations) =
      measure_nth(algorithm.hash_length(), usize::MAX, || memory.reserve(MemoryOwner::Task, 0, AdmissionClass::Workload).unwrap());
    assert!(!allocations.injected_failure);
    eprintln!("{algorithm:?} shared coordinator initialization: {allocations:?}");
    drop(initialization);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    let request = SemanticMutationSourceFingerprintRequestV1 {
      hash_algorithm: algorithm,
      expected_record_count: 0,
      maximum_path_bytes: 4096,
      maximum_workspace_bytes: 1 << 20,
    };
    let (result, allocations) =
      measure(algorithm.hash_length(), || fingerprint_semantic_mutation_sources_v1(request, std::iter::empty(), &memory, &|| false));
    assert!(allocations.injected_failure);
    let error = match result {
      Err(error) => error,
      Ok(_) => panic!("refused digest returned a result"),
    };
    assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }), "{error}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    let fingerprint = fingerprint_semantic_mutation_sources_v1(request, std::iter::empty(), &memory, &|| false).unwrap();
    assert_eq!(fingerprint.digest().len(), algorithm.hash_length());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, algorithm.hash_length() as u64);
    drop(fingerprint);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn source_fingerprint_stream_does_not_allocate_a_whole_input_container() {
  use std::cell::Cell;
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::semantic_mutation_control::{
    SemanticMutationSourceFingerprintRequestV1, SemanticMutationSourceIdentityV1, fingerprint_semantic_mutation_sources_v1,
  };
  for count in [32, 8192] {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let request = SemanticMutationSourceFingerprintRequestV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      expected_record_count: count,
      maximum_path_bytes: 16,
      maximum_workspace_bytes: 8192,
    };
    let observed_reservation = Cell::new(None);
    let source = |observe| {
      let memory = &memory;
      let observed_reservation = &observed_reservation;
      (0..count).map(move |index| {
        if observe {
          let retained = memory.snapshot().unwrap().reserved_bytes;
          assert!(retained <= 8192);
          match observed_reservation.get() {
            Some(previous) => assert_eq!(retained, previous),
            None => observed_reservation.set(Some(retained)),
          }
        }
        Ok(SemanticMutationSourceIdentityV1 { path: format!("/s/{index:08}"), file_record_id: None })
      })
    };
    // Snapshot diagnostics themselves allocate their owner vector. Check
    // reservation stability separately so the allocator measurement below
    // covers production plus the small streaming input buffers, not diagnostics.
    let baseline = fingerprint_semantic_mutation_sources_v1(request, source(true), &memory, &|| false).unwrap();
    let expected = baseline.digest().to_vec();
    drop(baseline);
    let (result, allocations) = measure(0, || fingerprint_semantic_mutation_sources_v1(request, source(false), &memory, &|| false));
    let fingerprint = result.unwrap();
    assert_eq!(fingerprint.digest(), expected);
    assert_eq!(fingerprint.record_count(), count);
    assert!(allocations.maximum <= 4096, "whole-input allocation: {allocations:?}");
    assert!(allocations.total <= count as usize * 64 + 4096, "unexpected input copying: {allocations:?}");
    drop(fingerprint);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
