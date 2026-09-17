//! Shared-schema failures and provisional callback/resource boundaries.
use super::*;
use std::cell::Cell;
use aeordb::engine::memory_coordinator::HostMemorySample;
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;

#[test]
fn discovery_empty_completion_still_rechecks_cancellation_and_memory_admission() {
  for kind in [SemanticSourceAliasKindV1::ParserRegistry, SemanticSourceAliasKindV1::IndexConfiguration] {
    for pressure in [false, true] {
      let memory = memory();
      let checks = Cell::new(0);
      assert_eq!(
        visit_semantic_source_aliases_v1(request(kind, None), &mut |_, _| panic!("absent source emitted an alias"), &memory, &|| {
          checks.set(checks.get() + 1);
          false
        })
        .unwrap(),
        0
      );
      let final_check = checks.get();
      assert!(final_check > 1);
      checks.set(0);
      let result =
        visit_semantic_source_aliases_v1(request(kind, None), &mut |_, _| panic!("absent source emitted an alias"), &memory, &|| {
          let next = checks.get() + 1;
          checks.set(next);
          if next == final_check && pressure {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
          }
          next == final_check && !pressure
        });
      if pressure {
        assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
      } else {
        assert!(matches!(result, Err(SemanticCompilationErrorV1::Cancelled)));
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn registry_discovery_preserves_the_exact_existing_entry_ceiling() {
  for count in [512, 513] {
    let entries: Vec<_> = (0..count).map(|index| format!("\"application/x-{index:04}\":\"p\"")).collect();
    let source = format!("{{\"$v\":1,\"parsers\":{{{}}}}}", entries.join(","));
    let memory = memory();
    let mut calls = 0;
    let result = visit_semantic_source_aliases_v1(
      request(SemanticSourceAliasKindV1::ParserRegistry, Some(source.as_bytes())),
      &mut |_, _| {
        calls += 1;
        Ok(())
      },
      &memory,
      &|| false,
    );
    if count == 512 {
      assert_eq!(result.unwrap(), 512);
      assert_eq!(calls, 512);
    } else {
      assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })));
      assert_eq!(calls, 0);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn malformed_alias_sources_fail_before_any_callback() {
  for (kind, sources) in [
    (
      SemanticSourceAliasKindV1::ParserRegistry,
      vec![
        r#"{}"#,
        r#"{"$v":0,"parsers":{}}"#,
        r#"{"$v":1,"parsers":{},"extra":1}"#,
        r#"{"$v":1,"parsers":{"text/a":"p","TEXT/A":"q"}}"#,
        r#"{"$v":1,"parsers":{"application/json":"p"}}"#,
        r#"{"$v":1,"parsers":{"text/a;v=1":"p"}}"#,
        r#"{"$v":1,"parsers":{"text/a":""}}"#,
        r#"{"$v":1,"parsers":{"text/a":"bad\u0000"}}"#,
        r#"{"$v":1,"parsers":{"text/a":null}}"#,
      ],
    ),
    (
      SemanticSourceAliasKindV1::IndexConfiguration,
      vec![
        r#"{}"#,
        r#"{"$v":0,"indexes":[]}"#,
        r#"{"$v":1,"indexes":[],"extra":1}"#,
        r#"{"$v":1,"parser":"p","parser":"q","indexes":[]}"#,
        r#"{"$v":1,"parser":"","indexes":[]}"#,
        r#"{"$v":1,"parser":"bad\u0000","indexes":[]}"#,
        r#"{"$v":1,"indexes":[],"parser_policies":{"wasm":{"max_fuel":10000001}}}"#,
        r#"{"$v":1,"indexes":[{"name":"x","type":"unknown"}]}"#,
        r#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m","extra":1}}]}"#,
      ],
    ),
  ] {
    for source in sources {
      let memory = memory();
      let result = visit_semantic_source_aliases_v1(
        request(kind, Some(source.as_bytes())),
        &mut |_, _| panic!("malformed source emitted an alias"),
        &memory,
        &|| false,
      );
      assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })), "{source}: {result:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn discovery_source_workspace_and_occurrence_limits_never_overrun_callbacks() {
  let source = br#"{"$v":1,"parsers":{"text/a":"p","text/b":"q"}}"#;
  for case in 0..4 {
    let memory = memory();
    let mut request = request(SemanticSourceAliasKindV1::ParserRegistry, Some(source));
    match case {
      0 => request.maximum_source_bytes = source.len() - 1,
      1 => request.maximum_workspace_bytes = 0,
      2 => request.maximum_alias_occurrences = 1,
      3 => request.maximum_alias_occurrences = 2,
      _ => unreachable!(),
    }
    let mut calls = 0;
    let result = visit_semantic_source_aliases_v1(
      request,
      &mut |_, _| {
        calls += 1;
        Ok(())
      },
      &memory,
      &|| false,
    );
    assert_eq!(
      calls,
      match case {
        2 => 1,
        3 => 2,
        _ => 0,
      }
    );
    if case == 3 {
      assert_eq!(result.unwrap(), 2);
    } else {
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn discovery_preserves_final_callback_error_and_releases_accounting() {
  let source = br#"{"$v":1,"parsers":{"text/a":"p"}}"#;
  let memory = memory();
  let result = visit_semantic_source_aliases_v1(
    request(SemanticSourceAliasKindV1::ParserRegistry, Some(source)),
    &mut |_, _| Err(SemanticCompilationErrorV1::Operational { path: "test-discovery", message: "sink refused".into() }),
    &memory,
    &|| false,
  );
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Operational { path: "test-discovery", .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn discovery_initial_and_final_cancellation_and_pressure_refuse_without_leaks_then_retry() {
  let source = br#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#;
  for case in 0..3 {
    let memory = memory();
    let cancelled = Cell::new(case == 0);
    let mut calls = 0;
    let result = visit_semantic_source_aliases_v1(
      request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source)),
      &mut |_, _| {
        calls += 1;
        if case == 1 {
          cancelled.set(true);
        }
        if case == 2 {
          memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        }
        Ok(())
      },
      &memory,
      &|| cancelled.get(),
    );
    assert_eq!(calls, usize::from(case != 0));
    if case == 2 {
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    } else {
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Cancelled)));
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    assert_eq!(
      visit_semantic_source_aliases_v1(
        request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source)),
        &mut |_, _| Ok(()),
        &memory,
        &|| false
      )
      .unwrap(),
      1
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
