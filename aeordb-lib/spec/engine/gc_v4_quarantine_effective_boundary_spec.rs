use super::*;
use aeordb::engine::memory_coordinator::{AdmissionClass, HostMemorySample};
use aeordb::engine::v4::gc_quarantine::{PhysicalQuarantineCandidateV1, QuarantineEffectiveClosureSummaryV1};
use aeordb::engine::v4::gc_state::GcStatePageV1;

type CollectedEffectiveRows = (QuarantineEffectiveClosureSummaryV1, Vec<(Identity, Vec<u8>)>);

fn request_copy<'a>(request: &QuarantineEffectiveClosureRequestV1<'a>, work: u64) -> QuarantineEffectiveClosureRequestV1<'a> {
  QuarantineEffectiveClosureRequestV1 {
    manifest: request.manifest,
    directory: request.directory,
    lifecycle: request.lifecycle,
    delta_values: request.delta_values,
    hash_algorithm: request.hash_algorithm,
    limits: QuarantineEffectiveClosureLimitsV1 { maximum_support_artifacts: request.limits.maximum_support_artifacts, maximum_work: work },
  }
}

fn collect(
  request: QuarantineEffectiveClosureRequestV1<'_>,
  page: Option<&GcStatePageV1<'_>>,
  memory: &MemoryCoordinator,
) -> Result<CollectedEffectiveRows, QuarantineEffectiveClosureErrorV1> {
  let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory)?;
  let mut rows = Vec::new();
  let mut visitor = |row: PhysicalQuarantineCandidateV1<'_>| {
    rows.push((identity(row.incarnation), row.encoded.to_vec()));
    Ok::<(), QuarantineEffectiveClosureErrorV1>(())
  };
  if let Some(page) = page {
    closure.observe_base_page(page, &mut visitor)?;
  }
  let summary = closure.finish(&mut visitor)?;
  Ok((summary, rows))
}

#[test]
fn effective_quarantine_candidates_support_empty_base_only_and_delta_only() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for base in [false, true] {
      for deltas in [Vec::new(), vec![fixture(&format!("agca-{}-candidate-delta-valid.bin", algorithm_name(algorithm)))]] {
        with_effective_fixture(algorithm, &deltas, base, |request, page, expected, input_rows, memory| {
          let (summary, rows) = collect(request, page, memory).unwrap();
          assert_eq!(rows, expected.into_iter().collect::<Vec<_>>());
          assert_eq!(summary.input_rows, input_rows);
          assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
        });
      }
    }
  }
}

#[test]
fn effective_quarantine_candidates_enforce_exact_work_and_admit_declared_rows_first() {
  let delta = fixture("agca-blake3-256-candidate-delta-valid.bin");
  with_effective_fixture(HashAlgorithm::Blake3_256, &[delta], true, |request, page, _, _, memory| {
    let (baseline, expected) = collect(request_copy(&request, 10_000), page, memory).unwrap();
    assert!(baseline.work > baseline.input_rows);
    let (exact, actual) = collect(request_copy(&request, baseline.work), page, memory).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(exact.work, baseline.work);
    assert_eq!(collect(request_copy(&request, baseline.work - 1), page, memory).unwrap_err().code(), "quarantine_effective_work");
    let error = QuarantineEffectiveClosureV1::new(request_copy(&request, 1), CancellationToken::new(), memory).err().unwrap();
    assert_eq!(error.code(), "quarantine_effective_work");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(collect(request, page, memory).unwrap().1, expected);
  });
}

#[test]
fn effective_quarantine_candidates_refuse_incorrect_effective_totals() {
  for change_bytes in [false, true] {
    with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, _, _, memory| {
      let mut manifest = request.manifest.clone();
      if change_bytes {
        manifest.candidate_bytes += 1;
      } else {
        manifest.candidate_count += 1;
      }
      let request = QuarantineEffectiveClosureRequestV1 { manifest: &manifest, ..request };
      assert_eq!(collect(request, page, memory).unwrap_err().code(), "quarantine_effective_totals");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}

#[test]
fn effective_quarantine_candidates_reject_missing_delta_and_bad_chain_identity() {
  let delta = fixture("agca-blake3-256-candidate-delta-valid.bin");
  with_effective_fixture(HashAlgorithm::Blake3_256, &[delta], true, |request, _, _, _, memory| {
    let missing = QuarantineEffectiveClosureRequestV1 { delta_values: &[], ..request_copy(&request, 10_000) };
    assert_eq!(
      QuarantineEffectiveClosureV1::new(missing, CancellationToken::new(), memory).err().unwrap().code(),
      "quarantine_delta_count"
    );
    let mut manifest = request.manifest.clone();
    let incorrect = vec![0x55; 32];
    manifest.delta_hashes = &incorrect;
    let changed = QuarantineEffectiveClosureRequestV1 { manifest: &manifest, ..request_copy(&request, 10_000) };
    assert_eq!(
      QuarantineEffectiveClosureV1::new(changed, CancellationToken::new(), memory).err().unwrap().code(),
      "quarantine_delta_identity"
    );
    let oversized: Vec<_> = std::iter::repeat_n(request.delta_values[0], 257).collect();
    let mut manifest = request.manifest.clone();
    manifest.delta_count = 257;
    let changed = QuarantineEffectiveClosureRequestV1 { manifest: &manifest, delta_values: &oversized, ..request_copy(&request, 10_000) };
    assert_eq!(
      QuarantineEffectiveClosureV1::new(changed, CancellationToken::new(), memory).err().unwrap().code(),
      "quarantine_delta_count"
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_reject_reversed_base_rows_and_latch_failure() {
  with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, _, _, memory| {
    let page = page.unwrap();
    let width = 52 + 2 * request.hash_algorithm.hash_length();
    let rows: Vec<_> = page.records.chunks_exact(width).rev().flatten().copied().collect();
    let mut invalid_page = page.clone();
    invalid_page.records = &rows;
    let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).unwrap();
    let mut seen = 0;
    let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| {
      seen += 1;
      Ok::<(), QuarantineEffectiveClosureErrorV1>(())
    };
    assert_eq!(closure.observe_base_page(&invalid_page, &mut visitor).unwrap_err().code(), "quarantine_effective_base_order");
    assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_effective_failed");
    assert_eq!(seen, 1, "emitted rows before a later error remain provisional");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[derive(Debug)]
enum CallbackFailure {
  Original,
  Core(QuarantineEffectiveClosureErrorV1),
}
impl From<QuarantineEffectiveClosureErrorV1> for CallbackFailure {
  fn from(error: QuarantineEffectiveClosureErrorV1) -> Self {
    Self::Core(error)
  }
}

#[test]
fn effective_quarantine_candidates_preserve_callback_error_over_interruption() {
  for pressure in [false, true] {
    with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, _, _, memory| {
      let cancellation = CancellationToken::new();
      let mut closure = QuarantineEffectiveClosureV1::new(request, cancellation.clone(), memory).unwrap();
      let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| {
        if pressure {
          memory.update_host_sample(HostMemorySample { rss_bytes: 64 << 20, ..Default::default() }).unwrap();
        } else {
          cancellation.cancel();
        }
        Err::<(), CallbackFailure>(CallbackFailure::Original)
      };
      assert!(matches!(closure.observe_base_page(page.unwrap(), &mut visitor), Err(CallbackFailure::Original)));
      let Err(CallbackFailure::Core(error)) = closure.finish(&mut visitor) else {
        panic!("partial traversal must stay failed")
      };
      assert_eq!(error.code(), "quarantine_effective_failed");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}

#[test]
fn effective_quarantine_candidates_refuse_memory_pressure_release_and_retry() {
  with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, expected, _, memory| {
    let limit = memory.snapshot().unwrap().policy.unwrap().ordinary_limit_bytes();
    let pressure = memory.reserve(MemoryOwner::Task, limit, AdmissionClass::Workload).unwrap();
    let error = QuarantineEffectiveClosureV1::new(request_copy(&request, 10_000), CancellationToken::new(), memory).err().unwrap();
    assert_eq!(error.code(), "quarantine_effective_memory");
    drop(pressure);
    let closure = QuarantineEffectiveClosureV1::new(request_copy(&request, 10_000), CancellationToken::new(), memory).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    assert!(retained > 0 && retained < 4096);
    drop(closure);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(collect(request, page, memory).unwrap().1, expected.into_iter().collect::<Vec<_>>());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_observe_cancellation_at_entry_page_and_finish() {
  for stage in 0..3 {
    with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, _, _, memory| {
      let cancellation = CancellationToken::new();
      if stage == 0 {
        cancellation.cancel();
        let error = QuarantineEffectiveClosureV1::new(request, cancellation, memory).err().unwrap();
        assert_eq!(error.code(), "quarantine_closure_canceled");
      } else {
        let mut closure = QuarantineEffectiveClosureV1::new(request, cancellation.clone(), memory).unwrap();
        let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| Ok::<(), QuarantineEffectiveClosureErrorV1>(());
        if stage == 1 {
          cancellation.cancel();
          assert_eq!(closure.observe_base_page(page.unwrap(), &mut visitor).unwrap_err().code(), "quarantine_closure_canceled");
          assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_effective_failed");
        } else {
          closure.observe_base_page(page.unwrap(), &mut visitor).unwrap();
          cancellation.cancel();
          assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_closure_canceled");
        }
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}

#[test]
fn effective_quarantine_candidates_retain_memory_checks_between_callbacks() {
  with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, _, _, memory| {
    let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).unwrap();
    let mut count = 0;
    let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| {
      count += 1;
      memory.update_host_sample(HostMemorySample { rss_bytes: 64 << 20, ..Default::default() }).unwrap();
      Ok::<(), QuarantineEffectiveClosureErrorV1>(())
    };
    assert_eq!(closure.observe_base_page(page.unwrap(), &mut visitor).unwrap_err().code(), "quarantine_effective_memory");
    assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_effective_failed");
    assert_eq!(count, 1);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_refuse_zero_limits_before_traversal() {
  with_effective_fixture(HashAlgorithm::Blake3_256, &[], false, |request, _, _, _, memory| {
    let error = QuarantineEffectiveClosureV1::new(request_copy(&request, 0), CancellationToken::new(), memory).err().unwrap();
    assert_eq!(error.code(), "quarantine_closure_configuration");
    let mut request = request;
    request.limits.maximum_support_artifacts = 0;
    let error = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).err().unwrap();
    assert_eq!(error.code(), "quarantine_closure_configuration");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_merge_256_streams_with_measured_logarithmic_work() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
    unreachable!()
  };
  let base = quarantine_candidate_records_v1(&page, algorithm).unwrap().next().unwrap().unwrap();
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let mut artifacts: Vec<aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1> = Vec::new();
  for index in 0..256 {
    let mut candidate = PhysicalQuarantineCandidateWriteV1::from(&base);
    candidate.grace_at_pending_ms += index;
    artifacts.push(
      encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
        hash_algorithm: algorithm,
        database_id: manifest.database_id.try_into().unwrap(),
        mark_generation: manifest.mark_generation,
        delta_ordinal: index as u32 + 1,
        previous_delta_hash: artifacts.last().map(|artifact| artifact.key.as_slice()),
        records: &[CandidateDeltaRecordWriteV1 { operation: CandidateDeltaOperationV1::Set, candidate }],
      })
      .unwrap(),
    );
  }
  let values: Vec<_> = artifacts.into_iter().map(|artifact| artifact.value).collect();
  with_effective_fixture(algorithm, &values, true, |request, page, expected, input_rows, memory| {
    let (summary, actual) = collect(request, page, memory).unwrap();
    assert_eq!(input_rows, 258);
    assert_eq!(summary.input_rows, input_rows);
    assert!(summary.comparisons <= input_rows * 20, "bounded binary heap, not a scan of every stream for every row");
    assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_bound_overlapping_multirecord_streams() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
    unreachable!()
  };
  let base = quarantine_candidate_records_v1(&page, algorithm).unwrap().next().unwrap().unwrap();
  let manifest_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let keys: Vec<_> = (1u16..=128)
    .map(|index| {
      let mut key = vec![0; 32];
      key[..2].copy_from_slice(&index.to_be_bytes());
      key
    })
    .collect();
  let mut artifacts: Vec<aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1> = Vec::new();
  for stream in 0..64 {
    let rows: Vec<_> = keys
      .iter()
      .enumerate()
      .map(|(index, key)| {
        let mut candidate = PhysicalQuarantineCandidateWriteV1::from(&base);
        candidate.incarnation.logical_key = key;
        candidate.grace_at_pending_ms += stream;
        let operation = if (index as u64 + stream).is_multiple_of(3) {
          candidate.pending_since_ms = 0;
          candidate.first_unreachable_generation = 0;
          candidate.grace_at_pending_ms = 0;
          CandidateDeltaOperationV1::Clear
        } else {
          CandidateDeltaOperationV1::Set
        };
        CandidateDeltaRecordWriteV1 { operation, candidate }
      })
      .collect();
    artifacts.push(
      encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
        hash_algorithm: algorithm,
        database_id: manifest.database_id.try_into().unwrap(),
        mark_generation: manifest.mark_generation,
        delta_ordinal: stream as u32 + 1,
        previous_delta_hash: artifacts.last().map(|artifact| artifact.key.as_slice()),
        records: &rows,
      })
      .unwrap(),
    );
  }
  let values: Vec<_> = artifacts.into_iter().map(|artifact| artifact.value).collect();
  with_effective_fixture(algorithm, &values, true, |request, page, expected, input_rows, memory| {
    let mut request = request;
    request.limits.maximum_work = 250_000;
    let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    assert!(retained < 65_536, "retain stream heads, not all candidate records: {retained}");
    let mut actual = Vec::new();
    let mut visitor = |row: PhysicalQuarantineCandidateV1<'_>| {
      actual.push((identity(row.incarnation), row.encoded.to_vec()));
      Ok::<(), QuarantineEffectiveClosureErrorV1>(())
    };
    closure.observe_base_page(page.unwrap(), &mut visitor).unwrap();
    let summary = closure.finish(&mut visitor).unwrap();
    assert_eq!(summary.input_rows, 8194);
    assert_eq!(summary.input_rows, input_rows);
    assert!(summary.comparisons <= input_rows * 24, "heap work must not grow linearly with stream count");
    assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_refuse_oversized_and_malformed_delta_bodies() {
  with_effective_fixture(HashAlgorithm::Blake3_256, &[], false, |request, _, _, _, memory| {
    // Repeated borrowed buffer crosses the aggregate 64MiB cap without a large test allocation.
    let bytes = vec![0; 262_145];
    let values = vec![bytes.as_slice(); 256];
    let mut manifest = request.manifest.clone();
    manifest.delta_count = 256;
    let oversized = QuarantineEffectiveClosureRequestV1 { manifest: &manifest, delta_values: &values, ..request_copy(&request, 10_000) };
    assert_eq!(
      QuarantineEffectiveClosureV1::new(oversized, CancellationToken::new(), memory).err().unwrap().code(),
      "quarantine_delta_bytes"
    );
    let valid = fixture("agca-blake3-256-candidate-delta-valid.bin");
    for mutation in 0..3 {
      let mut bytes = valid.clone();
      if mutation == 0 {
        bytes.truncate(3);
      } else if mutation == 1 {
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
      } else {
        bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
      }
      let values = [bytes.as_slice()];
      manifest.delta_count = 1;
      let malformed = QuarantineEffectiveClosureRequestV1 { manifest: &manifest, delta_values: &values, ..request_copy(&request, 10_000) };
      assert!(matches!(
        QuarantineEffectiveClosureV1::new(malformed, CancellationToken::new(), memory),
        Err(QuarantineEffectiveClosureErrorV1::Format(_))
      ));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  });
}

#[test]
fn effective_quarantine_candidates_admit_page_rows_before_iteration() {
  with_effective_fixture(HashAlgorithm::Blake3_256, &[], true, |request, page, _, _, memory| {
    let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).unwrap();
    let mut page = page.unwrap().clone();
    page.record_count = u32::MAX;
    let mut count = 0;
    let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| {
      count += 1;
      Ok::<(), QuarantineEffectiveClosureErrorV1>(())
    };
    assert_eq!(closure.observe_base_page(&page, &mut visitor).unwrap_err().code(), "quarantine_effective_work");
    assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_effective_failed");
    assert_eq!(count, 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_preserve_finish_callback_failure_and_final_cancellation() {
  let delta = fixture("agca-blake3-256-candidate-delta-valid.bin");
  for failure in [false, true] {
    with_effective_fixture(HashAlgorithm::Blake3_256, std::slice::from_ref(&delta), false, |request, _, _, _, memory| {
      let cancellation = CancellationToken::new();
      let closure = QuarantineEffectiveClosureV1::new(request, cancellation.clone(), memory).unwrap();
      let mut seen = 0;
      let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| {
        seen += 1;
        cancellation.cancel();
        if failure {
          Err(CallbackFailure::Original)
        } else {
          Ok(())
        }
      };
      let error = closure.finish(&mut visitor).unwrap_err();
      if failure {
        assert!(matches!(error, CallbackFailure::Original));
      } else {
        let CallbackFailure::Core(error) = error else {
          panic!("must retain cancellation")
        };
        assert_eq!(error.code(), "quarantine_closure_canceled");
      }
      assert_eq!(seen, 1);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}
