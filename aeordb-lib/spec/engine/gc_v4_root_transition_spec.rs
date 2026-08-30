use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_lifecycle::{
  RootCandidateRecordWriteV1, RootLifecycleManifestV1, decode_root_expiry_manifest_v1, decode_root_lifecycle_manifest_v1,
  encode_root_candidate_record_v1,
};
use aeordb::engine::v4::gc_root_transition::{
  REQUIRED_ROOT_LIFECYCLE_COMPLETE_MARKS_V1, RootCandidateStateV1, RootLifecycleReachabilityV1, RootLifecycleRootObservationV1,
  RootLifecycleTransitionContextV1, RootLifecycleTransitionModelV1, RootLifecycleTransitionV1,
};
use aeordb::engine::v4::gc_state::{GcStateArtifactV1, decode_gc_state_artifact, decode_root_candidate_record_v1};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x17; 16];

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  let algorithm_name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("root transition fixtures cover both frozen hash widths"),
  };
  fs::read(fixture_root().join(format!("agca-{algorithm_name}-{name}.bin"))).unwrap()
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
  let mut sources = Vec::new();
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      sources.extend(rust_sources(&path));
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      sources.push(path);
    }
  }
  sources
}

fn sequence(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn lifecycle_manifest<'a>(
  database_id: &'a [u8],
  authority_digest: &'a [u8],
  candidate_directory_hash: Option<&'a [u8]>,
  generation: u64,
  source_complete_mark_generation: u64,
  candidates: (u64, u64),
  key: Vec<u8>,
) -> RootLifecycleManifestV1<'a> {
  let (candidate_count, candidate_bytes) = candidates;
  RootLifecycleManifestV1 {
    database_id,
    generation,
    published_at_ms: 1_700_000_000_000,
    source_complete_mark_generation,
    authority_root_set_digest: authority_digest,
    candidate_directory_hash,
    root_expiry_manifest_hash: None,
    next_page_id: if candidate_count == 0 { 0 } else { 2 },
    candidate_count,
    pending_count: candidate_count,
    retired_evidence_count: 0,
    candidate_bytes,
    expiry_bytes: 0,
    key,
  }
}

fn candidate_bytes(
  algorithm: HashAlgorithm,
  root_hash: &[u8],
  pending_since_ms: i64,
  first_generation: u64,
  last_generation: u64,
  grace_at_pending_ms: u64,
  evidence: (&[u8], &[u8]),
) -> Vec<u8> {
  let (authority_digest, admission_hash) = evidence;
  encode_root_candidate_record_v1(&RootCandidateRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: root_hash,
    reason: 1,
    pending_since_ms,
    first_unreachable_generation: first_generation,
    last_confirmed_unreachable_generation: last_generation,
    grace_at_pending_ms,
    authority_root_set_digest: authority_digest,
    admission_commit_payload_hash: admission_hash,
  })
  .unwrap()
}

fn assert_candidate(
  candidate: &RootCandidateStateV1,
  root_hash: &[u8],
  pending_since_ms: i64,
  first_generation: u64,
  last_generation: u64,
  grace_at_pending_ms: u64,
  evidence: (&[u8], &[u8]),
) {
  let (authority_digest, admission_hash) = evidence;
  assert_eq!(candidate.namespace_root_hash, root_hash);
  assert_eq!(candidate.reason, 1);
  assert_eq!(candidate.pending_since_ms, pending_since_ms);
  assert_eq!(candidate.first_unreachable_generation, first_generation);
  assert_eq!(candidate.last_confirmed_unreachable_generation, last_generation);
  assert_eq!(candidate.grace_at_pending_ms, grace_at_pending_ms);
  assert_eq!(candidate.authority_root_set_digest, authority_digest);
  assert_eq!(candidate.admission_commit_payload_hash, admission_hash);
}

#[test]
fn zero_grace_starts_a_candidate_and_still_requires_a_later_complete_mark_at_both_hash_widths() {
  assert_eq!(REQUIRED_ROOT_LIFECYCLE_COMPLETE_MARKS_V1, 2);
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let width = algorithm.hash_length();
    let authority_digest = sequence(width, 0x20);
    let next_authority_digest = sequence(width, 0x40);
    let root_hash = sequence(width, 0x60);
    let admission_hash = sequence(width, 0x80);
    let candidate_directory_hash = sequence(width, 0xa0);
    let prior = lifecycle_manifest(&DATABASE_ID, &authority_digest, None, 10, 100, (0, 0), sequence(width, 0xc0));
    let cancellation = CancellationToken::new();
    let mut first_mark = RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &prior,
        prior_expiry: None,
        lifecycle_generation: 11,
        complete_mark_generation: 101,
        completed_at_ms: 1_000,
        current_configured_grace_ms: 0,
        authority_root_set_digest: &next_authority_digest,
        lifecycle_hard_max_bytes: 1_048_576,
        maximum_roots: 4,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let transition = first_mark
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: None,
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &admission_hash },
      })
      .unwrap();
    let RootLifecycleTransitionV1::CandidateStarted(candidate) = transition else {
      panic!("the discovery mark must start a candidate, never retire it")
    };
    assert_candidate(&candidate, &root_hash, 1_000, 101, 101, 0, (&next_authority_digest, &admission_hash));
    let encoded_from_transition = encode_root_candidate_record_v1(&candidate.as_write_request(algorithm)).unwrap();
    assert_eq!(decode_root_candidate_record_v1(&encoded_from_transition, algorithm).unwrap().namespace_root_hash, root_hash);
    let first_summary = first_mark.finish().unwrap();
    assert_eq!((first_summary.started_count, first_summary.retirement_count), (1, 0));

    let encoded_candidate = candidate_bytes(algorithm, &root_hash, 1_000, 101, 101, 0, (&next_authority_digest, &admission_hash));
    let prior_candidate = decode_root_candidate_record_v1(&encoded_candidate, algorithm).unwrap();
    let second_prior = lifecycle_manifest(
      &DATABASE_ID,
      &next_authority_digest,
      Some(&candidate_directory_hash),
      11,
      101,
      (1, encoded_candidate.len() as u64),
      sequence(width, 0xd0),
    );
    let mut second_mark = RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &second_prior,
        prior_expiry: None,
        lifecycle_generation: 12,
        complete_mark_generation: 102,
        completed_at_ms: 1_000,
        current_configured_grace_ms: 0,
        authority_root_set_digest: &authority_digest,
        lifecycle_hard_max_bytes: 1_048_576,
        maximum_roots: 4,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let transition = second_mark
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &admission_hash },
      })
      .unwrap();
    let RootLifecycleTransitionV1::RetirementEligible(retirement) = transition else {
      panic!("zero grace may retire only on the later complete mark")
    };
    assert_eq!(retirement.namespace_root_hash, root_hash);
    assert_eq!((retirement.pending_since_ms, retirement.grace_at_pending_ms), (1_000, 0));
    assert_eq!(retirement.final_mark_generation, 102);
    assert_eq!(retirement.prior_lifecycle_manifest_hash, second_prior.key);
    assert_eq!(retirement.authority_root_set_digest, next_authority_digest);
    assert_eq!(retirement.admission_commit_payload_hash, admission_hash);
    let second_summary = second_mark.finish().unwrap();
    assert_eq!((second_summary.resulting_candidate_count, second_summary.resulting_mandatory_count), (0, 1));
  }
}

#[test]
fn mandatory_retirement_evidence_counts_toward_the_hard_cap_and_is_never_evicted() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let lifecycle_bytes = fixture(algorithm, "root-lifecycle-manifest-populated");
    let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_bytes, algorithm).unwrap();
    let expiry_bytes = fixture(algorithm, "root-expiry-catalog-manifest-populated");
    let expiry = decode_root_expiry_manifest_v1(&expiry_bytes, algorithm).unwrap();
    let candidate_page_bytes = fixture(algorithm, "root-candidate-page-valid");
    let GcStateArtifactV1::Page(candidate_page) = decode_gc_state_artifact(&candidate_page_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    let candidate = decode_root_candidate_record_v1(candidate_page.records, algorithm).unwrap();
    let current_lifecycle_bytes = lifecycle.candidate_bytes + expiry.mandatory_bytes;
    let grace_at_pending_ms = i64::try_from(candidate.grace_at_pending_ms).unwrap();
    let completed_at_ms = candidate.pending_since_ms.checked_add(grace_at_pending_ms).unwrap();
    let cancellation = CancellationToken::new();

    let mut blocked_retirement = RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &lifecycle,
        prior_expiry: Some(&expiry),
        lifecycle_generation: lifecycle.generation + 1,
        complete_mark_generation: lifecycle.source_complete_mark_generation + 1,
        completed_at_ms,
        current_configured_grace_ms: 0,
        authority_root_set_digest: lifecycle.authority_root_set_digest,
        lifecycle_hard_max_bytes: current_lifecycle_bytes,
        maximum_roots: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let transition = blocked_retirement
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: candidate.namespace_root_hash,
        prior_candidate: Some(&candidate),
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable {
          reason: candidate.reason,
          admission_commit_payload_hash: candidate.admission_commit_payload_hash,
        },
      })
      .unwrap();
    assert!(matches!(transition, RootLifecycleTransitionV1::CapacityDeferred { candidate: Some(_) }));
    let summary = blocked_retirement.finish().unwrap();
    assert_eq!((summary.resulting_mandatory_count, summary.resulting_mandatory_bytes), (expiry.mandatory_count, expiry.mandatory_bytes));
    assert_eq!(summary.resulting_lifecycle_bytes, current_lifecycle_bytes);
    assert!(summary.capacity_blocked);

    let mut over_cap_clear = RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &lifecycle,
        prior_expiry: Some(&expiry),
        lifecycle_generation: lifecycle.generation + 1,
        complete_mark_generation: lifecycle.source_complete_mark_generation + 1,
        completed_at_ms,
        current_configured_grace_ms: 0,
        authority_root_set_digest: lifecycle.authority_root_set_digest,
        lifecycle_hard_max_bytes: expiry.mandatory_bytes - 1,
        maximum_roots: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    assert_eq!(
      over_cap_clear
        .observe(RootLifecycleRootObservationV1 {
          namespace_root_hash: candidate.namespace_root_hash,
          prior_candidate: Some(&candidate),
          reachability: RootLifecycleReachabilityV1::Reachable,
        })
        .unwrap(),
      RootLifecycleTransitionV1::CandidateCleared,
    );
    let summary = over_cap_clear.finish().unwrap();
    assert_eq!((summary.resulting_candidate_count, summary.resulting_mandatory_count), (0, expiry.mandatory_count));
    assert_eq!(summary.resulting_lifecycle_bytes, expiry.mandatory_bytes);
    assert!(summary.capacity_blocked);
  }
}

#[test]
fn effective_grace_uses_the_larger_frozen_or_current_value_and_checked_time() {
  let algorithm = HashAlgorithm::Blake3_256;
  let width = algorithm.hash_length();
  let authority_digest = sequence(width, 0x10);
  let root_hash = sequence(width, 0x30);
  let admission_hash = sequence(width, 0x50);
  let candidate_directory_hash = sequence(width, 0x70);
  let encoded_candidate = candidate_bytes(algorithm, &root_hash, 1_000, 40, 40, 2_000, (&authority_digest, &admission_hash));
  let prior_candidate = decode_root_candidate_record_v1(&encoded_candidate, algorithm).unwrap();
  let prior = lifecycle_manifest(
    &DATABASE_ID,
    &authority_digest,
    Some(&candidate_directory_hash),
    20,
    40,
    (1, encoded_candidate.len() as u64),
    sequence(width, 0x90),
  );
  let cancellation = CancellationToken::new();

  for (configured_grace, completed_at, should_retire) in [(500, 2_999, false), (3_000, 3_999, false), (3_000, 4_000, true)] {
    let mut model = RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &prior,
        prior_expiry: None,
        lifecycle_generation: 21,
        complete_mark_generation: 41,
        completed_at_ms: completed_at,
        current_configured_grace_ms: configured_grace,
        authority_root_set_digest: &authority_digest,
        lifecycle_hard_max_bytes: 1_048_576,
        maximum_roots: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let transition = model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &admission_hash },
      })
      .unwrap();
    if should_retire {
      assert!(matches!(transition, RootLifecycleTransitionV1::RetirementEligible(_)));
    } else {
      let RootLifecycleTransitionV1::CandidateConfirmed(candidate) = transition else {
        panic!("grace that has not elapsed must only confirm the pending candidate")
      };
      assert_candidate(&candidate, &root_hash, 1_000, 40, 41, 2_000, (&authority_digest, &admission_hash));
    }
    model.finish().unwrap();
  }

  let mut overflow_candidate = prior_candidate.clone();
  overflow_candidate.pending_since_ms = i64::MAX;
  overflow_candidate.grace_at_pending_ms = 1;
  let mut overflow_model = RootLifecycleTransitionModelV1::new(
    RootLifecycleTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_lifecycle: &prior,
      prior_expiry: None,
      lifecycle_generation: 21,
      complete_mark_generation: 41,
      completed_at_ms: i64::MAX,
      current_configured_grace_ms: 1,
      authority_root_set_digest: &authority_digest,
      lifecycle_hard_max_bytes: 1_048_576,
      maximum_roots: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      lifecycle_authority_healthy: true,
      physical_authority_healthy: true,
    },
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    overflow_model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&overflow_candidate),
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &admission_hash },
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_time",
  );
}

#[test]
fn reachable_roots_clear_candidates_while_indeterminate_roots_preserve_them() {
  let algorithm = HashAlgorithm::Blake3_256;
  let width = algorithm.hash_length();
  let authority_digest = sequence(width, 0x11);
  let first_root = sequence(width, 0x31);
  let second_root = sequence(width, 0x71);
  let first_admission = sequence(width, 0x91);
  let second_admission = sequence(width, 0xb1);
  let candidate_directory_hash = sequence(width, 0xd1);
  let first_bytes = candidate_bytes(algorithm, &first_root, 1_000, 50, 50, 10_000, (&authority_digest, &first_admission));
  let second_bytes = candidate_bytes(algorithm, &second_root, 1_001, 50, 50, 10_000, (&authority_digest, &second_admission));
  let first_candidate = decode_root_candidate_record_v1(&first_bytes, algorithm).unwrap();
  let second_candidate = decode_root_candidate_record_v1(&second_bytes, algorithm).unwrap();
  let prior = lifecycle_manifest(
    &DATABASE_ID,
    &authority_digest,
    Some(&candidate_directory_hash),
    30,
    50,
    (2, (first_bytes.len() + second_bytes.len()) as u64),
    sequence(width, 0xe1),
  );
  let cancellation = CancellationToken::new();
  let mut model = RootLifecycleTransitionModelV1::new(
    RootLifecycleTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_lifecycle: &prior,
      prior_expiry: None,
      lifecycle_generation: 31,
      complete_mark_generation: 51,
      completed_at_ms: 20_000,
      current_configured_grace_ms: 10_000,
      authority_root_set_digest: &authority_digest,
      lifecycle_hard_max_bytes: 1_048_576,
      maximum_roots: 2,
      mark_complete: true,
      destructive_gc_enabled: true,
      lifecycle_authority_healthy: true,
      physical_authority_healthy: true,
    },
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &first_root,
        prior_candidate: Some(&first_candidate),
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap(),
    RootLifecycleTransitionV1::CandidateCleared,
  );
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &second_root,
        prior_candidate: Some(&second_candidate),
        reachability: RootLifecycleReachabilityV1::Indeterminate,
      })
      .unwrap(),
    RootLifecycleTransitionV1::IndeterminateRetained { had_candidate: true },
  );
  let summary = model.finish().unwrap();
  assert_eq!((summary.cleared_count, summary.indeterminate_count, summary.resulting_candidate_count), (1, 1, 1));
}

#[test]
fn deterministic_capacity_refusal_never_evicts_state_or_reopens_growth_later_in_the_pass() {
  let algorithm = HashAlgorithm::Blake3_256;
  let width = algorithm.hash_length();
  let candidate_length = (36 + 3 * width) as u64;
  let authority_digest = sequence(width, 0x12);
  let first_root = sequence(width, 0x22);
  let clearing_root = sequence(width, 0x62);
  let final_root = sequence(width, 0xa2);
  let admission_hash = sequence(width, 0xd2);
  let candidate_directory_hash = sequence(width, 0xe2);
  let clearing_bytes = candidate_bytes(algorithm, &clearing_root, 1_000, 60, 60, 10_000, (&authority_digest, &admission_hash));
  let clearing_candidate = decode_root_candidate_record_v1(&clearing_bytes, algorithm).unwrap();
  let prior = lifecycle_manifest(
    &DATABASE_ID,
    &authority_digest,
    Some(&candidate_directory_hash),
    40,
    60,
    (1, candidate_length),
    sequence(width, 0xf2),
  );
  let cancellation = CancellationToken::new();
  let mut model = RootLifecycleTransitionModelV1::new(
    RootLifecycleTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_lifecycle: &prior,
      prior_expiry: None,
      lifecycle_generation: 41,
      complete_mark_generation: 61,
      completed_at_ms: 20_000,
      current_configured_grace_ms: 10_000,
      authority_root_set_digest: &authority_digest,
      lifecycle_hard_max_bytes: candidate_length,
      maximum_roots: 3,
      mark_complete: true,
      destructive_gc_enabled: true,
      lifecycle_authority_healthy: true,
      physical_authority_healthy: true,
    },
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &first_root,
        prior_candidate: None,
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &admission_hash },
      })
      .unwrap(),
    RootLifecycleTransitionV1::CapacityDeferred { candidate: None },
  );
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &clearing_root,
        prior_candidate: Some(&clearing_candidate),
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap(),
    RootLifecycleTransitionV1::CandidateCleared,
  );
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &final_root,
        prior_candidate: None,
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &admission_hash },
      })
      .unwrap(),
    RootLifecycleTransitionV1::CapacityDeferred { candidate: None },
  );
  let summary = model.finish().unwrap();
  assert!(summary.capacity_blocked);
  assert_eq!((summary.capacity_deferred_count, summary.cleared_count, summary.resulting_candidate_count), (2, 1, 0));
  assert_eq!(summary.resulting_lifecycle_bytes, 0);
}

#[test]
fn malformed_or_stale_evidence_cancellation_and_incomplete_authority_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let width = algorithm.hash_length();
  let authority_digest = sequence(width, 0x13);
  let root_hash = sequence(width, 0x33);
  let admission_hash = sequence(width, 0x53);
  let wrong_admission_hash = sequence(width, 0x73);
  let candidate_directory_hash = sequence(width, 0x93);
  let encoded_candidate = candidate_bytes(algorithm, &root_hash, 1_000, 70, 70, 0, (&authority_digest, &admission_hash));
  let prior_candidate = decode_root_candidate_record_v1(&encoded_candidate, algorithm).unwrap();
  let prior = lifecycle_manifest(
    &DATABASE_ID,
    &authority_digest,
    Some(&candidate_directory_hash),
    50,
    70,
    (1, encoded_candidate.len() as u64),
    sequence(width, 0xb3),
  );

  for (mark_complete, destructive, lifecycle_healthy, physical_healthy) in
    [(false, true, true, true), (true, false, true, true), (true, true, false, true), (true, true, true, false)]
  {
    let cancellation = CancellationToken::new();
    assert_eq!(
      RootLifecycleTransitionModelV1::new(
        RootLifecycleTransitionContextV1 {
          hash_algorithm: algorithm,
          prior_lifecycle: &prior,
          prior_expiry: None,
          lifecycle_generation: 51,
          complete_mark_generation: 71,
          completed_at_ms: 2_000,
          current_configured_grace_ms: 0,
          authority_root_set_digest: &authority_digest,
          lifecycle_hard_max_bytes: 1_048_576,
          maximum_roots: 1,
          mark_complete,
          destructive_gc_enabled: destructive,
          lifecycle_authority_healthy: lifecycle_healthy,
          physical_authority_healthy: physical_healthy,
        },
        &cancellation,
      )
      .unwrap_err()
      .code(),
      "root_lifecycle_transition_unavailable",
    );
  }

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &prior,
        prior_expiry: None,
        lifecycle_generation: 51,
        complete_mark_generation: 71,
        completed_at_ms: 2_000,
        current_configured_grace_ms: 0,
        authority_root_set_digest: &authority_digest,
        lifecycle_hard_max_bytes: 1_048_576,
        maximum_roots: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &canceled,
    )
    .unwrap_err()
    .code(),
    "root_lifecycle_transition_canceled",
  );

  let stale_cancellation = CancellationToken::new();
  assert_eq!(
    RootLifecycleTransitionModelV1::new(
      RootLifecycleTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &prior,
        prior_expiry: None,
        lifecycle_generation: 51,
        complete_mark_generation: prior.source_complete_mark_generation,
        completed_at_ms: 2_000,
        current_configured_grace_ms: 0,
        authority_root_set_digest: &authority_digest,
        lifecycle_hard_max_bytes: 1_048_576,
        maximum_roots: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        lifecycle_authority_healthy: true,
        physical_authority_healthy: true,
      },
      &stale_cancellation,
    )
    .unwrap_err()
    .code(),
    "root_lifecycle_transition_generation",
  );

  let mid_traversal_cancellation = CancellationToken::new();
  let mut canceled_model = RootLifecycleTransitionModelV1::new(
    RootLifecycleTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_lifecycle: &prior,
      prior_expiry: None,
      lifecycle_generation: 51,
      complete_mark_generation: 71,
      completed_at_ms: 2_000,
      current_configured_grace_ms: 0,
      authority_root_set_digest: &authority_digest,
      lifecycle_hard_max_bytes: 1_048_576,
      maximum_roots: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      lifecycle_authority_healthy: true,
      physical_authority_healthy: true,
    },
    &mid_traversal_cancellation,
  )
  .unwrap();
  mid_traversal_cancellation.cancel();
  assert_eq!(
    canceled_model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_canceled",
  );
  assert_eq!(
    canceled_model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_failed",
  );

  let cancellation = CancellationToken::new();
  let mut model = RootLifecycleTransitionModelV1::new(
    RootLifecycleTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_lifecycle: &prior,
      prior_expiry: None,
      lifecycle_generation: 51,
      complete_mark_generation: 71,
      completed_at_ms: 2_000,
      current_configured_grace_ms: 0,
      authority_root_set_digest: &authority_digest,
      lifecycle_hard_max_bytes: 1_048_576,
      maximum_roots: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      lifecycle_authority_healthy: true,
      physical_authority_healthy: true,
    },
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::ConfirmedUnreachable { reason: 1, admission_commit_payload_hash: &wrong_admission_hash },
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_evidence",
  );
  assert_eq!(
    model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &root_hash,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_failed",
  );
}

#[test]
fn ordering_limits_and_manifest_candidate_coverage_are_exact() {
  let algorithm = HashAlgorithm::Blake3_256;
  let width = algorithm.hash_length();
  let authority_digest = sequence(width, 0x14);
  let first_root = sequence(width, 0x24);
  let second_root = sequence(width, 0x64);
  let admission_hash = sequence(width, 0xa4);
  let candidate_directory_hash = sequence(width, 0xc4);
  let encoded_candidate = candidate_bytes(algorithm, &first_root, 1_000, 80, 80, 5_000, (&authority_digest, &admission_hash));
  let prior_candidate = decode_root_candidate_record_v1(&encoded_candidate, algorithm).unwrap();
  let prior = lifecycle_manifest(
    &DATABASE_ID,
    &authority_digest,
    Some(&candidate_directory_hash),
    60,
    80,
    (1, encoded_candidate.len() as u64),
    sequence(width, 0xe4),
  );
  let cancellation = CancellationToken::new();
  let context = RootLifecycleTransitionContextV1 {
    hash_algorithm: algorithm,
    prior_lifecycle: &prior,
    prior_expiry: None,
    lifecycle_generation: 61,
    complete_mark_generation: 81,
    completed_at_ms: 2_000,
    current_configured_grace_ms: 5_000,
    authority_root_set_digest: &authority_digest,
    lifecycle_hard_max_bytes: 1_048_576,
    maximum_roots: 2,
    mark_complete: true,
    destructive_gc_enabled: true,
    lifecycle_authority_healthy: true,
    physical_authority_healthy: true,
  };

  let mut missing_candidate_model = RootLifecycleTransitionModelV1::new(context, &cancellation).unwrap();
  missing_candidate_model
    .observe(RootLifecycleRootObservationV1 {
      namespace_root_hash: &second_root,
      prior_candidate: None,
      reachability: RootLifecycleReachabilityV1::Reachable,
    })
    .unwrap();
  assert_eq!(missing_candidate_model.finish().unwrap_err().code(), "root_lifecycle_transition_manifest");

  let mut ordering_model = RootLifecycleTransitionModelV1::new(context, &cancellation).unwrap();
  ordering_model
    .observe(RootLifecycleRootObservationV1 {
      namespace_root_hash: &second_root,
      prior_candidate: None,
      reachability: RootLifecycleReachabilityV1::Reachable,
    })
    .unwrap();
  assert_eq!(
    ordering_model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &first_root,
        prior_candidate: Some(&prior_candidate),
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_order",
  );

  let limited_context = RootLifecycleTransitionContextV1 { maximum_roots: 1, ..context };
  let mut limited_model = RootLifecycleTransitionModelV1::new(limited_context, &cancellation).unwrap();
  limited_model
    .observe(RootLifecycleRootObservationV1 {
      namespace_root_hash: &first_root,
      prior_candidate: Some(&prior_candidate),
      reachability: RootLifecycleReachabilityV1::Reachable,
    })
    .unwrap();
  assert_eq!(
    limited_model
      .observe(RootLifecycleRootObservationV1 {
        namespace_root_hash: &second_root,
        prior_candidate: None,
        reachability: RootLifecycleReachabilityV1::Reachable,
      })
      .unwrap_err()
      .code(),
    "root_lifecycle_transition_limit",
  );
}

#[test]
fn transition_model_remains_disconnected_from_service_storage_and_destructive_gc() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let violations = rust_sources(&source_root)
    .into_iter()
    .filter(|path| path.file_name().is_none_or(|name| name != "gc_root_transition.rs"))
    .filter(|path| fs::read_to_string(path).unwrap().contains("RootLifecycleTransitionModelV1"))
    .collect::<Vec<_>>();
  assert!(violations.is_empty(), "root transition model escaped its disconnected owner: {violations:?}");
}
