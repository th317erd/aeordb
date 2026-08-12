use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::PhysicalIncarnationV1;
use aeordb::engine::v4::gc_quarantine::{
  CandidateDeltaOperationV1, PhysicalQuarantineCandidateClassV1, PhysicalQuarantineCandidateV1, PhysicalQuarantineCandidateWriteV1,
  QuarantineManifestWriteV1, decode_physical_quarantine_candidate_v1, decode_quarantine_manifest_v1,
  encode_physical_quarantine_candidate_v1, encode_quarantine_manifest_v1,
};
use aeordb::engine::v4::gc_quarantine_transition::{
  PhysicalQuarantineObservationV1, PhysicalQuarantineReachabilityV1, PhysicalQuarantineTransitionContextV1,
  PhysicalQuarantineTransitionModelV1, PhysicalQuarantineTransitionV1,
};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x41; 16];

fn sequence(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn capabilities() -> [u8; 32] {
  let mut value = [0u8; 32];
  for capability in [12usize, 13, 15, 17] {
    value[capability / 8] |= 1 << (capability % 8);
  }
  value
}

macro_rules! candidate_bytes {
  ($algorithm:expr, $logical_seed:expr, $integrity_seed:expr, $wal_offset:expr, $class:expr, $pending_since_ms:expr, $first_generation:expr, $grace_ms:expr) => {{
    let width = $algorithm.hash_length();
    let logical_key = sequence(width, $logical_seed);
    let integrity = sequence(width, $integrity_seed);
    encode_physical_quarantine_candidate_v1(&PhysicalQuarantineCandidateWriteV1 {
      hash_algorithm: $algorithm,
      incarnation: PhysicalIncarnationV1 {
        logical_key: &logical_key,
        integrity_or_legacy_digest: &integrity,
        wal_offset: $wal_offset,
        write_sequence: $wal_offset + 10,
        entity_length: 512,
        entry_type: 1,
        entity_version: 1,
      },
      class: $class,
      pending_since_ms: $pending_since_ms,
      first_unreachable_generation: $first_generation,
      grace_at_pending_ms: $grace_ms,
    })
    .unwrap()
  }};
}

fn manifest_bytes(algorithm: HashAlgorithm, mark_generation: u64, completed_at_ms: u64, candidate_count: u64) -> Vec<u8> {
  let width = algorithm.hash_length();
  let required_capabilities = capabilities();
  let authority = sequence(width, 0x51);
  let semantic = sequence(width, 0x71);
  let layout = sequence(width, 0x91);
  let result = sequence(width, 0xb1);
  let candidate_root = sequence(width, 0xd1);
  let lifecycle = sequence(width, 0xe1);
  let candidate_bytes = candidate_count * u64::try_from(52 + 2 * width).unwrap();
  encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: DATABASE_ID,
    mark_generation,
    completed_at_ms,
    required_capabilities: &required_capabilities,
    authority_root_set_digest: &authority,
    semantic_state_digest: &semantic,
    kv_layout_fingerprint: &layout,
    mark_result_digest: &result,
    candidate_directory_root: (candidate_count != 0).then_some(candidate_root.as_slice()),
    captured_root_lifecycle_manifest: &lifecycle,
    candidate_count,
    candidate_bytes,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: 2,
    delta_hashes: &[],
  })
  .unwrap()
  .value
}

struct TransitionInputs {
  authority: Vec<u8>,
  semantic: Vec<u8>,
  layout: Vec<u8>,
  result: Vec<u8>,
  lifecycle: Vec<u8>,
}

impl TransitionInputs {
  fn new(algorithm: HashAlgorithm) -> Self {
    let width = algorithm.hash_length();
    Self {
      authority: sequence(width, 0x52),
      semantic: sequence(width, 0x72),
      layout: sequence(width, 0x92),
      result: sequence(width, 0xb2),
      lifecycle: sequence(width, 0xe2),
    }
  }
}

macro_rules! transition_context {
  ($inputs:expr, $algorithm:expr, $prior_manifest:expr, $mark_generation:expr, $completed_at_ms:expr, $grace_ms:expr, $maximum_incarnations:expr, $maximum_candidates:expr) => {{
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: $algorithm,
      prior_manifest: $prior_manifest,
      mark_generation: $mark_generation,
      completed_at_ms: $completed_at_ms,
      current_configured_grace_ms: $grace_ms,
      authority_root_set_digest: &$inputs.authority,
      semantic_state_digest: &$inputs.semantic,
      kv_layout_fingerprint: &$inputs.layout,
      mark_result_digest: &$inputs.result,
      captured_root_lifecycle_manifest: &$inputs.lifecycle,
      maximum_incarnations: $maximum_incarnations,
      maximum_candidates: $maximum_candidates,
      mark_complete: true,
      destructive_gc_enabled: true,
      mark_authority_healthy: true,
      physical_inventory_healthy: true,
      root_lifecycle_healthy: true,
    }
  }};
}

fn assert_started_candidate(
  candidate: &aeordb::engine::v4::gc_quarantine_transition::PhysicalQuarantineCandidateStateV1,
  pending_since_ms: u64,
  first_generation: u64,
  grace_ms: u64,
  class: PhysicalQuarantineCandidateClassV1,
) {
  assert_eq!(candidate.pending_since_ms, pending_since_ms);
  assert_eq!(candidate.first_unreachable_generation, first_generation);
  assert_eq!(candidate.grace_at_pending_ms, grace_ms);
  assert_eq!(candidate.class, class);
}

#[test]
fn zero_grace_still_requires_two_complete_marks_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let empty_bytes = manifest_bytes(algorithm, 100, 1_000, 0);
    let empty = decode_quarantine_manifest_v1(&empty_bytes, algorithm).unwrap();
    let inputs = TransitionInputs::new(algorithm);
    let cancellation = CancellationToken::new();
    let mut first =
      PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &empty, 101, 2_000, 0, 1, 1), &cancellation).unwrap();
    let incarnation_bytes =
      candidate_bytes!(algorithm, 0x11, 0x31, 4_096, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
    let incarnation = decode_physical_quarantine_candidate_v1(&incarnation_bytes, algorithm, false).unwrap().incarnation;
    let transition = first
      .observe(PhysicalQuarantineObservationV1 {
        incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
        },
      })
      .unwrap();
    let PhysicalQuarantineTransitionV1::CandidateStarted(candidate) = transition else {
      panic!("a first complete mark must start, not sweep, a candidate")
    };
    assert_started_candidate(&candidate, 2_000, 101, 0, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation);
    assert_eq!(candidate.hash_algorithm, algorithm);
    let set_write = candidate.as_delta_write_request();
    assert_eq!(set_write.operation, CandidateDeltaOperationV1::Set);
    let encoded_candidate = encode_physical_quarantine_candidate_v1(&set_write.candidate).unwrap();
    first.finish().unwrap();

    let prior_bytes = manifest_bytes(algorithm, 101, 2_000, 1);
    let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
    let prior_candidate = decode_physical_quarantine_candidate_v1(&encoded_candidate, algorithm, false).unwrap();
    let mut second =
      PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 102, 2_000, 0, 1, 1), &cancellation).unwrap();
    let transition = second
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: prior_candidate.incarnation,
        prior_candidate: Some(&prior_candidate),
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
        },
      })
      .unwrap();
    let PhysicalQuarantineTransitionV1::SweepEligible(intent) = transition else {
      panic!("the later complete mark should emit a non-authoritative sweep intent")
    };
    assert_eq!(intent.confirming_mark_generation, 102);
    assert_eq!(intent.eligible_at_ms, 2_000);
    assert_eq!(intent.prior_quarantine_manifest_hash, prior.key);
    let summary = second.finish().unwrap();
    assert_eq!((summary.started_count, summary.eligible_count, summary.resulting_candidate_count), (0, 1, 1));
  }
}

#[test]
fn a_delta_only_candidate_manifest_can_advance_to_its_second_complete_mark() {
  let algorithm = HashAlgorithm::Blake3_256;
  let width = algorithm.hash_length();
  let required_capabilities = capabilities();
  let authority = sequence(width, 0x51);
  let semantic = sequence(width, 0x71);
  let layout = sequence(width, 0x91);
  let result = sequence(width, 0xb1);
  let lifecycle = sequence(width, 0xe1);
  let delta_hash = sequence(width, 0xd1);
  let record_bytes = u64::try_from(52 + 2 * width).unwrap();
  let manifest_bytes = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: DATABASE_ID,
    mark_generation: 100,
    completed_at_ms: 1_000,
    required_capabilities: &required_capabilities,
    authority_root_set_digest: &authority,
    semantic_state_digest: &semantic,
    kv_layout_fingerprint: &layout,
    mark_result_digest: &result,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: &lifecycle,
    candidate_count: 1,
    candidate_bytes: record_bytes,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: 1,
    delta_hashes: &delta_hash,
  })
  .unwrap()
  .value;
  let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let candidate_bytes =
    candidate_bytes!(algorithm, 0x11, 0x31, 4_096, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1_000, 100, 0);
  let candidate = decode_physical_quarantine_candidate_v1(&candidate_bytes, algorithm, false).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();
  let mut model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &manifest, 101, 1_000, 0, 1, 1), &cancellation)
      .unwrap();

  let transition = model
    .observe(PhysicalQuarantineObservationV1 {
      incarnation: candidate.incarnation,
      prior_candidate: Some(&candidate),
      reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable { class: candidate.class },
    })
    .unwrap();

  assert!(matches!(transition, PhysicalQuarantineTransitionV1::SweepEligible(_)));
  assert_eq!(model.finish().unwrap().eligible_count, 1);
}

#[test]
fn effective_grace_uses_the_larger_frozen_or_current_value_and_checks_overflow() {
  let algorithm = HashAlgorithm::Blake3_256;
  let encoded_candidate =
    candidate_bytes!(algorithm, 0x12, 0x32, 8_192, PhysicalQuarantineCandidateClassV1::OrphanUncommittedIncarnation, 1_000, 20, 2_000);
  let candidate = decode_physical_quarantine_candidate_v1(&encoded_candidate, algorithm, false).unwrap();
  let prior_bytes = manifest_bytes(algorithm, 20, 1_000, 1);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();

  for (current_grace, completed_at, eligible) in [(500, 2_999, false), (3_000, 3_999, false), (3_000, 4_000, true)] {
    let mut model = PhysicalQuarantineTransitionModelV1::new(
      transition_context!(inputs, algorithm, &prior, 21, completed_at, current_grace, 1, 1),
      &cancellation,
    )
    .unwrap();
    let transition = model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: candidate.incarnation,
        prior_candidate: Some(&candidate),
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::OrphanUncommittedIncarnation,
        },
      })
      .unwrap();
    assert_eq!(matches!(transition, PhysicalQuarantineTransitionV1::SweepEligible(_)), eligible);
    if !eligible {
      let PhysicalQuarantineTransitionV1::CandidateConfirmed(candidate) = transition else {
        panic!("a not-yet-eligible later mark must retain the exact candidate")
      };
      encode_physical_quarantine_candidate_v1(&candidate.as_write_request()).unwrap();
    }
    model.finish().unwrap();
  }

  let overflow_bytes =
    candidate_bytes!(algorithm, 0x12, 0x32, 8_192, PhysicalQuarantineCandidateClassV1::OrphanUncommittedIncarnation, u64::MAX, 20, 1);
  let overflow = decode_physical_quarantine_candidate_v1(&overflow_bytes, algorithm, false).unwrap();
  let mut model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 21, u64::MAX, 1, 1, 1), &cancellation).unwrap();
  assert_eq!(
    model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: overflow.incarnation,
        prior_candidate: Some(&overflow),
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::OrphanUncommittedIncarnation,
        },
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_time",
  );
}

#[test]
fn class_changes_restart_frozen_evidence_instead_of_inheriting_eligibility() {
  let algorithm = HashAlgorithm::Blake3_256;
  let prior_candidate_bytes =
    candidate_bytes!(algorithm, 0x13, 0x33, 12_288, PhysicalQuarantineCandidateClassV1::UnreachableActiveLocator, 1_000, 30, 0);
  let prior_candidate = decode_physical_quarantine_candidate_v1(&prior_candidate_bytes, algorithm, false).unwrap();
  let prior_bytes = manifest_bytes(algorithm, 30, 1_000, 1);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();
  let mut model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 31, 10_000, 5_000, 1, 1), &cancellation)
      .unwrap();
  let transition = model
    .observe(PhysicalQuarantineObservationV1 {
      incarnation: prior_candidate.incarnation,
      prior_candidate: Some(&prior_candidate),
      reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
        class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
      },
    })
    .unwrap();
  let PhysicalQuarantineTransitionV1::CandidateRestarted(candidate) = transition else {
    panic!("changed classification must reset the candidate")
  };
  assert_started_candidate(&candidate, 10_000, 31, 5_000, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation);
  let summary = model.finish().unwrap();
  assert_eq!((summary.restarted_count, summary.eligible_count, summary.resulting_candidate_count), (1, 0, 1));
}

#[test]
fn reachable_candidates_clear_while_indeterminate_evidence_retains_exact_state() {
  let algorithm = HashAlgorithm::Blake3_256;
  let first_bytes =
    candidate_bytes!(algorithm, 0x14, 0x34, 16_384, PhysicalQuarantineCandidateClassV1::ExpiredDerivedArtifact, 1_000, 40, 10_000);
  let second_bytes =
    candidate_bytes!(algorithm, 0x54, 0x74, 20_480, PhysicalQuarantineCandidateClassV1::ExpiredGcAuditArtifact, 1_001, 40, 10_000);
  let first = decode_physical_quarantine_candidate_v1(&first_bytes, algorithm, false).unwrap();
  let second = decode_physical_quarantine_candidate_v1(&second_bytes, algorithm, false).unwrap();
  let third_bytes = candidate_bytes!(algorithm, 0x94, 0x84, 21_504, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
  let fourth_bytes = candidate_bytes!(algorithm, 0xd4, 0xc4, 22_528, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
  let third = decode_physical_quarantine_candidate_v1(&third_bytes, algorithm, false).unwrap();
  let fourth = decode_physical_quarantine_candidate_v1(&fourth_bytes, algorithm, false).unwrap();
  let prior_bytes = manifest_bytes(algorithm, 40, 1_001, 2);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();
  let mut model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 41, 2_000, 10_000, 4, 2), &cancellation)
      .unwrap();
  let PhysicalQuarantineTransitionV1::CandidateCleared(clear) = model
    .observe(PhysicalQuarantineObservationV1 {
      incarnation: first.incarnation,
      prior_candidate: Some(&first),
      reachability: PhysicalQuarantineReachabilityV1::Reachable,
    })
    .unwrap()
  else {
    panic!("reachable prior candidate must emit an exact clear")
  };
  let clear_write = clear.as_delta_write_request();
  assert_eq!(clear_write.operation, CandidateDeltaOperationV1::Clear);
  assert_eq!((clear_write.candidate.pending_since_ms, clear_write.candidate.first_unreachable_generation), (0, 0));
  assert_eq!(
    model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: second.incarnation,
        prior_candidate: Some(&second),
        reachability: PhysicalQuarantineReachabilityV1::Indeterminate,
      })
      .unwrap(),
    PhysicalQuarantineTransitionV1::IndeterminateRetained { had_candidate: true },
  );
  assert_eq!(
    model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: third.incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::Reachable,
      })
      .unwrap(),
    PhysicalQuarantineTransitionV1::Retained,
  );
  assert_eq!(
    model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: fourth.incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::Indeterminate,
      })
      .unwrap(),
    PhysicalQuarantineTransitionV1::IndeterminateRetained { had_candidate: false },
  );
  let summary = model.finish().unwrap();
  assert_eq!((summary.cleared_count, summary.indeterminate_count, summary.resulting_candidate_count), (1, 2, 1));
}

#[test]
fn deterministic_capacity_refusal_does_not_reopen_after_a_later_clear() {
  let algorithm = HashAlgorithm::Blake3_256;
  let existing_bytes =
    candidate_bytes!(algorithm, 0x55, 0x75, 24_576, PhysicalQuarantineCandidateClassV1::ExpiredNamespaceRootClosure, 1_000, 50, 10_000);
  let existing = decode_physical_quarantine_candidate_v1(&existing_bytes, algorithm, false).unwrap();
  let prior_bytes = manifest_bytes(algorithm, 50, 1_000, 1);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();
  let first_new_bytes =
    candidate_bytes!(algorithm, 0x15, 0x35, 22_528, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
  let last_new_bytes =
    candidate_bytes!(algorithm, 0x95, 0xb5, 28_672, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
  let first_new = decode_physical_quarantine_candidate_v1(&first_new_bytes, algorithm, false).unwrap();
  let last_new = decode_physical_quarantine_candidate_v1(&last_new_bytes, algorithm, false).unwrap();
  let mut model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 51, 2_000, 10_000, 3, 1), &cancellation)
      .unwrap();
  for (incarnation, prior_candidate, reachability) in [
    (
      first_new.incarnation,
      None,
      PhysicalQuarantineReachabilityV1::ConfirmedUnreachable { class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation },
    ),
    (existing.incarnation, Some(&existing), PhysicalQuarantineReachabilityV1::Reachable),
    (
      last_new.incarnation,
      None,
      PhysicalQuarantineReachabilityV1::ConfirmedUnreachable { class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation },
    ),
  ] {
    let transition = model.observe(PhysicalQuarantineObservationV1 { incarnation, prior_candidate, reachability }).unwrap();
    if prior_candidate.is_none() {
      assert_eq!(transition, PhysicalQuarantineTransitionV1::CapacityDeferred);
    }
  }
  let summary = model.finish().unwrap();
  assert!(summary.capacity_blocked);
  assert_eq!((summary.capacity_deferred_count, summary.cleared_count, summary.resulting_candidate_count), (2, 1, 0));
}

#[test]
fn stale_incomplete_unhealthy_malformed_and_canceled_transitions_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let empty_bytes = manifest_bytes(algorithm, 60, 1_000, 0);
  let empty = decode_quarantine_manifest_v1(&empty_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);

  for unavailable_field in 0..5 {
    let cancellation = CancellationToken::new();
    let mut context = transition_context!(inputs, algorithm, &empty, 61, 2_000, 0, 1, 1);
    match unavailable_field {
      0 => context.mark_complete = false,
      1 => context.destructive_gc_enabled = false,
      2 => context.mark_authority_healthy = false,
      3 => context.physical_inventory_healthy = false,
      _ => context.root_lifecycle_healthy = false,
    }
    assert_eq!(
      PhysicalQuarantineTransitionModelV1::new(context, &cancellation).unwrap_err().code(),
      "physical_quarantine_transition_unavailable",
    );
  }

  let cancellation = CancellationToken::new();
  assert_eq!(
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &empty, 60, 2_000, 0, 1, 1), &cancellation)
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_generation",
  );
  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &empty, 61, 2_000, 0, 1, 1), &canceled)
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_canceled",
  );

  let incarnation_bytes =
    candidate_bytes!(algorithm, 0x16, 0x36, 32_768, PhysicalQuarantineCandidateClassV1::UnexplainedGapInventoryCandidate, 1, 1, 0);
  let incarnation = decode_physical_quarantine_candidate_v1(&incarnation_bytes, algorithm, false).unwrap().incarnation;
  let cancellation = CancellationToken::new();
  let mut model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &empty, 61, 2_000, 0, 1, 1), &cancellation).unwrap();
  cancellation.cancel();
  assert_eq!(
    model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::UnexplainedGapInventoryCandidate,
        },
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_canceled",
  );
  assert_eq!(
    model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_failed",
  );
}

#[test]
fn configuration_manifest_incarnation_candidate_and_stream_bounds_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let empty_bytes = manifest_bytes(algorithm, 80, 1_000, 0);
  let empty = decode_quarantine_manifest_v1(&empty_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();

  for (maximum_incarnations, maximum_candidates, completed_at_ms) in [(0, 1, 2_000), (1, 0, 2_000), (1, 1, 0)] {
    assert_eq!(
      PhysicalQuarantineTransitionModelV1::new(
        transition_context!(inputs, algorithm, &empty, 81, completed_at_ms, 0, maximum_incarnations, maximum_candidates),
        &cancellation,
      )
      .unwrap_err()
      .code(),
      "physical_quarantine_transition_configuration",
    );
  }

  let zeros = vec![0; algorithm.hash_length()];
  let mut invalid_basis = transition_context!(inputs, algorithm, &empty, 81, 2_000, 0, 1, 1);
  invalid_basis.authority_root_set_digest = &zeros;
  assert_eq!(
    PhysicalQuarantineTransitionModelV1::new(invalid_basis, &cancellation).unwrap_err().code(),
    "physical_quarantine_transition_manifest",
  );
  let mut mismatched_algorithm = transition_context!(inputs, algorithm, &empty, 81, 2_000, 0, 1, 1);
  mismatched_algorithm.hash_algorithm = HashAlgorithm::Sha512;
  assert_eq!(
    PhysicalQuarantineTransitionModelV1::new(mismatched_algorithm, &cancellation).unwrap_err().code(),
    "physical_quarantine_transition_manifest",
  );
  let mut malformed_manifest = empty.clone();
  malformed_manifest.candidate_count = 1;
  assert_eq!(
    PhysicalQuarantineTransitionModelV1::new(
      transition_context!(inputs, algorithm, &malformed_manifest, 81, 2_000, 0, 1, 1),
      &cancellation,
    )
    .unwrap_err()
    .code(),
    "physical_quarantine_transition_manifest",
  );

  let valid_bytes = candidate_bytes!(algorithm, 0x19, 0x39, 45_056, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
  let valid = decode_physical_quarantine_candidate_v1(&valid_bytes, algorithm, false).unwrap();
  let zero_key = vec![0; algorithm.hash_length()];
  let invalid_incarnation = PhysicalIncarnationV1 { logical_key: &zero_key, ..valid.incarnation };
  let mut invalid_model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &empty, 81, 2_000, 0, 1, 1), &cancellation).unwrap();
  assert_eq!(
    invalid_model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: invalid_incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_incarnation",
  );

  let prior_bytes = manifest_bytes(algorithm, 81, 1_000, 1);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let prior_candidate_bytes =
    candidate_bytes!(algorithm, 0x19, 0x39, 45_056, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1_000, 81, 0);
  let prior_candidate = decode_physical_quarantine_candidate_v1(&prior_candidate_bytes, algorithm, false).unwrap();
  let invalid_candidate = PhysicalQuarantineCandidateV1 { pending_since_ms: 0, ..prior_candidate };
  let mut invalid_candidate_model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 82, 2_000, 0, 1, 1), &cancellation).unwrap();
  assert_eq!(
    invalid_candidate_model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: invalid_candidate.incarnation,
        prior_candidate: Some(&invalid_candidate),
        reachability: PhysicalQuarantineReachabilityV1::Indeterminate,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_candidate_state",
  );

  let later_bytes = candidate_bytes!(algorithm, 0x59, 0x79, 49_152, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1, 1, 0);
  let later = decode_physical_quarantine_candidate_v1(&later_bytes, algorithm, false).unwrap();
  let mut bounded =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &empty, 81, 2_000, 0, 1, 1), &cancellation).unwrap();
  bounded
    .observe(PhysicalQuarantineObservationV1 {
      incarnation: valid.incarnation,
      prior_candidate: None,
      reachability: PhysicalQuarantineReachabilityV1::Reachable,
    })
    .unwrap();
  assert_eq!(
    bounded
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: later.incarnation,
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_limit",
  );
}

#[test]
fn order_exact_identity_prior_coverage_and_candidate_generation_are_mandatory() {
  let algorithm = HashAlgorithm::Blake3_256;
  let encoded_candidate =
    candidate_bytes!(algorithm, 0x17, 0x37, 36_864, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1_000, 70, 0);
  let candidate = decode_physical_quarantine_candidate_v1(&encoded_candidate, algorithm, false).unwrap();
  let prior_bytes = manifest_bytes(algorithm, 70, 1_000, 1);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let inputs = TransitionInputs::new(algorithm);
  let cancellation = CancellationToken::new();
  let missing =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 71, 2_000, 0, 1, 1), &cancellation).unwrap();
  assert_eq!(missing.finish().unwrap_err().code(), "physical_quarantine_transition_manifest");

  let wrong_bytes =
    candidate_bytes!(algorithm, 0x18, 0x38, 40_960, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1_000, 70, 0);
  let wrong = decode_physical_quarantine_candidate_v1(&wrong_bytes, algorithm, false).unwrap();
  let mut mismatch =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 71, 2_000, 0, 1, 1), &cancellation).unwrap();
  assert_eq!(
    mismatch
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: wrong.incarnation,
        prior_candidate: Some(&candidate),
        reachability: PhysicalQuarantineReachabilityV1::Indeterminate,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_candidate_identity",
  );

  let mut ordered =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 71, 2_000, 0, 2, 1), &cancellation).unwrap();
  ordered
    .observe(PhysicalQuarantineObservationV1 {
      incarnation: candidate.incarnation,
      prior_candidate: Some(&candidate),
      reachability: PhysicalQuarantineReachabilityV1::Indeterminate,
    })
    .unwrap();
  assert_eq!(
    ordered
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: candidate.incarnation,
        prior_candidate: Some(&candidate),
        reachability: PhysicalQuarantineReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_order",
  );

  let future_bytes =
    candidate_bytes!(algorithm, 0x17, 0x37, 36_864, PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation, 1_000, 71, 0);
  let future = decode_physical_quarantine_candidate_v1(&future_bytes, algorithm, false).unwrap();
  let mut future_model =
    PhysicalQuarantineTransitionModelV1::new(transition_context!(inputs, algorithm, &prior, 71, 2_000, 0, 1, 1), &cancellation).unwrap();
  assert_eq!(
    future_model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: future.incarnation,
        prior_candidate: Some(&future),
        reachability: PhysicalQuarantineReachabilityV1::Indeterminate,
      })
      .unwrap_err()
      .code(),
    "physical_quarantine_transition_generation",
  );
}

#[test]
fn quarantine_transition_remains_disconnected_from_live_service_and_destructive_gc() {
  let source = fs::read_to_string(source_path("src/engine/v4/gc_quarantine_transition.rs")).unwrap();
  let publication_source = fs::read_to_string(source_path("src/engine/v4/gc_quarantine_publication.rs")).unwrap();
  for forbidden in ["StorageEngine", "DirectoryOps", "AppState", "server::", "VoidManager", "run_gc", "remove_entry"] {
    assert!(!source.contains(forbidden), "transition model unexpectedly references {forbidden}");
    assert!(!publication_source.contains(forbidden), "publication qualifier unexpectedly references {forbidden}");
  }
  let source_root = source_path("src");
  let mut production_uses = Vec::new();
  collect_production_uses(&source_root, &mut production_uses);
  assert_eq!(
    production_uses,
    vec![
      source_path("src/engine/v4/gc_quarantine_publication.rs"),
      source_path("src/engine/v4/gc_sweep.rs"),
      source_path("src/engine/v4/mod.rs"),
    ]
  );
}

fn source_path(relative: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn collect_production_uses(directory: &Path, matches: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(directory).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      collect_production_uses(&path, matches);
      continue;
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some("rs") || path.ends_with("gc_quarantine_transition.rs") {
      continue;
    }
    if fs::read_to_string(&path).unwrap().contains("gc_quarantine_transition") {
      matches.push(path);
    }
  }
  matches.sort();
}
