//! Parser-plan writers preserve the independent APRP program, not mutable aliases.
use aeordb::engine::v4::dependency::{InvocationPolicyKind, InvocationPolicyV1};
use aeordb::engine::v4::parser_plan::{
  ParserCandidateKind, ParserCandidateV1, ParserPlanKind, ParserResolutionPlanV1, decode_parser_resolution_plan,
  encode_parser_resolution_plan,
};
use aeordb::engine::v4::reader::MalformedInputClass;

fn policy(kind: InvocationPolicyKind) -> InvocationPolicyV1 {
  let wasm = kind != InvocationPolicyKind::Native;
  InvocationPolicyV1 {
    kind,
    max_request_bytes: if wasm { 8 * 1_024 * 1_024 } else { 0 },
    max_response_bytes: 4 * 1_024 * 1_024,
    max_linear_memory_bytes: if wasm { 64 * 1_024 * 1_024 } else { 0 },
    max_fuel: if wasm { 50_000_000 } else { 0 },
    max_table_elements: if wasm { 100_000 } else { 0 },
    max_structure_nodes: 100_000,
    max_scalar_bytes: 65_536,
    max_structure_depth: 32,
    max_container_members: 65_535,
    max_wasm_instances: u32::from(wasm),
    max_wasm_memories: u32::from(wasm),
    max_wasm_tables: u32::from(wasm),
    max_value_stack_height: 4_096,
    max_recursion_depth: 256,
  }
}

fn candidate(kind: ParserCandidateKind, dependency_ordinal: u32, match_bytes: &[u8], legacy: bool) -> ParserCandidateV1<'_> {
  let invocation = match kind {
    ParserCandidateKind::RawJson | ParserCandidateKind::NativeSuite => InvocationPolicyKind::Native,
    _ if legacy => InvocationPolicyKind::LegacyWasm,
    _ => InvocationPolicyKind::PureWasm,
  };
  ParserCandidateV1 {
    kind,
    dependency_ordinal,
    match_bytes,
    policy: policy(invocation),
    match_semantics: if kind == ParserCandidateKind::Registry {
      if legacy {
        2
      } else {
        1
      }
    } else {
      0
    },
  }
}

fn automatic<'a>(registry: &[&'a [u8]], legacy: bool) -> ParserResolutionPlanV1<'a> {
  let semantics = if legacy { 2 } else { 1 };
  let mut candidates: Vec<_> =
    registry.iter().map(|bytes| candidate(ParserCandidateKind::Registry, if legacy { 2 } else { 1 }, bytes, legacy)).collect();
  candidates.push(candidate(ParserCandidateKind::RawJson, 3, b"", legacy));
  candidates.push(candidate(ParserCandidateKind::NativeSuite, 4, b"", legacy));
  ParserResolutionPlanV1 {
    kind: ParserPlanKind::Automatic,
    resolution_semantics: semantics,
    mime_semantics: semantics,
    no_match_semantics: semantics,
    mime_dependency_ordinal: 4,
    candidates,
  }
}

fn explicit() -> ParserResolutionPlanV1<'static> {
  ParserResolutionPlanV1 {
    kind: ParserPlanKind::ExplicitPlugin,
    resolution_semantics: 1,
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: vec![candidate(ParserCandidateKind::Explicit, 1, b"", false)],
  }
}

fn none() -> ParserResolutionPlanV1<'static> {
  ParserResolutionPlanV1 {
    kind: ParserPlanKind::None,
    resolution_semantics: 0,
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: vec![],
  }
}

#[test]
fn parser_writer_matches_all_eight_independent_program_fixtures() {
  for profile in ["blake3-256", "sha512"] {
    for (name, plan) in [
      ("none", none()),
      ("explicit-plugin", explicit()),
      ("automatic", automatic(&[b"application/pdf", b"text/plain"], false)),
      ("automatic-legacy", automatic(&[b"Text/Plain; charset=UTF-8"], true)),
    ] {
      let expected =
        std::fs::read(format!("{}/spec/fixtures/v4/parser-resolution-plan-v1/aprp-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR")))
          .unwrap();
      let encoded = encode_parser_resolution_plan(&plan).unwrap();
      assert_eq!(encoded, expected, "{profile}/{name}");
      assert_eq!(decode_parser_resolution_plan(&encoded).unwrap(), plan);
    }
  }
}

#[test]
fn parser_writer_rejects_each_inapplicable_none_and_explicit_field() {
  for mut plan in [none(), explicit()] {
    for field in 0..4 {
      let mut invalid = plan.clone();
      match field {
        0 => invalid.resolution_semantics = 3,
        1 => invalid.mime_semantics = 1,
        2 => invalid.no_match_semantics = 1,
        _ => invalid.mime_dependency_ordinal = 1,
      }
      assert!(encode_parser_resolution_plan(&invalid).is_err(), "{:?}/{field}", plan.kind);
    }
    if plan.kind == ParserPlanKind::None {
      plan.candidates.push(candidate(ParserCandidateKind::Explicit, 1, b"", false));
    } else {
      plan.candidates.clear();
    }
    assert!(encode_parser_resolution_plan(&plan).is_err());
  }
  let mut plan = explicit();
  plan.resolution_semantics = 0;
  assert!(encode_parser_resolution_plan(&plan).is_err());
  plan = explicit();
  plan.candidates.push(plan.candidates[0].clone());
  assert!(encode_parser_resolution_plan(&plan).is_err());
}

#[test]
fn parser_writer_checks_candidate_dependency_match_and_policy_context() {
  for mutation in 0..6 {
    let mut plan = explicit();
    match mutation {
      0 => plan.candidates[0].dependency_ordinal = 0,
      1 => plan.candidates[0].match_semantics = 1,
      2 => plan.candidates[0].match_bytes = b"text/plain",
      3 => plan.candidates[0].policy = policy(InvocationPolicyKind::Native),
      4 => plan.candidates[0].policy.max_fuel = 0,
      _ => plan.candidates[0].kind = ParserCandidateKind::Registry,
    }
    assert!(encode_parser_resolution_plan(&plan).is_err(), "{mutation}");
  }
  for ordinal in [1, u32::MAX] {
    let mut plan = explicit();
    plan.candidates[0].dependency_ordinal = ordinal;
    let encoded = encode_parser_resolution_plan(&plan).unwrap();
    assert_eq!(&encoded[56..60], &ordinal.to_le_bytes());
  }
}

#[test]
fn parser_writer_rejects_noncanonical_corrected_mime_and_registry_order() {
  for media_type in [&b"Text/Plain"[..], b"text/plain; charset=utf-8", b"application/json", b"text/*", b"", b"text/\xff"] {
    assert!(encode_parser_resolution_plan(&automatic(&[media_type], false)).is_err(), "{media_type:?}");
  }
  for registry in [[&b"text/z"[..], &b"text/a"[..]], [&b"text/a"[..], &b"text/a"[..]]] {
    assert_eq!(
      encode_parser_resolution_plan(&automatic(&registry, false)).unwrap_err().class(),
      MalformedInputClass::NoncanonicalOrderOrDuplicate
    );
  }
  let mut mixed = automatic(&[b"text/plain"], false);
  mixed.candidates[0].policy = policy(InvocationPolicyKind::LegacyWasm);
  assert!(encode_parser_resolution_plan(&mixed).is_err());
}

#[test]
fn parser_writer_preserves_registry_then_raw_then_native_and_family_consistency() {
  for legacy in [false, true] {
    let original = automatic(&[], legacy);
    assert!(encode_parser_resolution_plan(&original).is_ok());
    for mutation in 0..7 {
      let mut plan = original.clone();
      match mutation {
        0 => plan.mime_dependency_ordinal = 0,
        1 => plan.mime_semantics = if legacy { 1 } else { 2 },
        2 => plan.no_match_semantics = if legacy { 1 } else { 2 },
        3 => plan.candidates.swap(0, 1),
        4 => {
          plan.candidates.pop();
        }
        5 => plan.candidates[0].policy = policy(InvocationPolicyKind::PureWasm),
        _ => plan.candidates[1].match_bytes = b"application/json",
      }
      assert!(encode_parser_resolution_plan(&plan).is_err(), "{legacy}/{mutation}");
    }
  }
}

#[test]
fn parser_writer_accepts_512_registry_entries_but_preflights_the_513th() {
  let names: Vec<_> = (0..513).map(|number| format!("text/x-{number:04}")).collect();
  let matches: Vec<_> = names.iter().map(|name| name.as_bytes()).collect();
  let maximum = automatic(&matches[..512], false);
  let encoded = encode_parser_resolution_plan(&maximum).unwrap();
  assert_eq!(&encoded[24..28], &514u32.to_le_bytes());
  assert_eq!(decode_parser_resolution_plan(&encoded).unwrap().candidates.len(), 514);
  assert_eq!(encode_parser_resolution_plan(&automatic(&matches, false)).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn parser_writer_enforces_combined_128kib_budget_before_semantic_work() {
  let maximum_match = vec![b'x'; 131_072 - 48 - 3 * (32 + 128)];
  let plan = automatic(&[&maximum_match], true);
  assert_eq!(encode_parser_resolution_plan(&plan).unwrap().len(), 131_072);
  let oversized = vec![0xff; maximum_match.len() + 1];
  let mut invalid = automatic(&[&oversized], true);
  invalid.candidates[0].policy.max_fuel = 0;
  assert_eq!(encode_parser_resolution_plan(&invalid).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn parser_writer_legacy_lookup_is_exact_and_never_normalized() {
  for lookup in [&b"Text/Plain; charset=UTF-8"[..], b"text/plain", b" application/json ", b"x\0y"] {
    let plan = automatic(&[lookup], true);
    let encoded = encode_parser_resolution_plan(&plan).unwrap();
    assert_eq!(&encoded[80..80 + lookup.len()], lookup);
    assert_eq!(decode_parser_resolution_plan(&encoded).unwrap().candidates[0].match_bytes, lookup);
  }
  assert!(encode_parser_resolution_plan(&automatic(&[b"\xff"], true)).is_err());
}
