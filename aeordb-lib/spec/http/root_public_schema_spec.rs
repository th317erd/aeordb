use std::fs;
use std::path::Path;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::position::{
  LogicalPositionWriteV1, PositionComponentWriteV1, PositionRouteV1, PositionSortDirectionV1, encode_logical_position,
};
use aeordb::engine::v4::position_order::{PositionOrderFieldV1, PositionWindowOriginV1, compile_query_order_v1};
use aeordb::engine::v4::query_planner::{QueryExpressionV1, QueryPredicateOperationV1};
use aeordb::server::root_api::{RequestedRootSelectorV1, RootResponseV1};
use aeordb::server::root_public_schema::{
  PUBLIC_QUERY_MAXIMUM_REQUEST_BYTES_V1, PublicAffectedRelationshipChangeV1, PublicAffectedRelationshipV1, PublicCollectionMetadataV1,
  PublicEntryTypeV1, PublicHalfOpenRangeV1, PublicItemsResponseV1, PublicLineColumnPointV1, PublicLineColumnRangeV1,
  PublicLocatorContinuationV1, PublicLocatorMatchSemanticsV1, PublicLocatorMatchV1, PublicMutationEventMetadataV1, PublicMutationKindV1,
  PublicPositionContextV1, PublicRangeContinuationV1, PublicRangeSelectionV1, PublicResultsResponseV1, PublicSchemaErrorV1,
  parse_public_query_request_v1,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn query_order(hash_algorithm: HashAlgorithm) -> aeordb::engine::v4::position::CompiledRouteOrderV1 {
  compile_query_order_v1(
    hash_algorithm,
    &[PositionOrderFieldV1 {
      field: "priority",
      direction: PositionSortDirectionV1::Descending,
      comparator: aeordb::engine::v4::position::PositionComparatorV1::U64,
    }],
  )
  .unwrap()
}

fn position_token(order: &aeordb::engine::v4::position::CompiledRouteOrderV1, root: &[u8]) -> String {
  let components = [
    PositionComponentWriteV1 {
      comparator: Some(aeordb::engine::v4::position::PositionComparatorV1::U64),
      state: aeordb::engine::v4::position::PositionComponentStateV1::Present,
      payload: &7_u64.to_be_bytes(),
    },
    PositionComponentWriteV1::utf8(b"/docs/a.txt"),
  ];
  String::from_utf8(
    encode_logical_position(&LogicalPositionWriteV1 {
      order,
      namespace_root: root,
      file_key_tie: &[0x41; 32],
      record_revision_tie: &[0x51; 32],
      components: &components,
    })
    .unwrap(),
  )
  .unwrap()
}

fn context(order: &aeordb::engine::v4::position::CompiledRouteOrderV1) -> PublicPositionContextV1<'_> {
  PublicPositionContextV1 { route: PositionRouteV1::Query, order_fingerprint: order.fingerprint() }
}

fn parse(
  body: Value,
  order: &aeordb::engine::v4::position::CompiledRouteOrderV1,
) -> Result<aeordb::server::root_public_schema::PublicQueryRequestV1, PublicSchemaErrorV1> {
  parse_public_query_request_v1(&serde_json::to_vec(&body).unwrap(), HashAlgorithm::Blake3_256, context(order))
}

#[test]
fn query_schema_admits_one_selector_one_origin_and_a_bounded_typed_expression() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let order = query_order(hash_algorithm);
  let root = vec![0x31; hash_algorithm.hash_length()];
  let request = parse(
    json!({
      "path": "/docs",
      "root_hash": hex::encode(&root),
      "where": {
        "and": [
          { "field": "status", "op": "eq", "value": "ready" },
          { "field": "priority", "op": "in", "value": [1, 2, 3] }
        ]
      },
      "page": 2,
      "limit": 25,
      "order_by": [{ "field": "priority", "direction": "desc" }],
      "include_total": true,
      "select": ["@path", "priority"],
      "explain": "plan",
      "include_matches": true,
      "max_matches_per_result": 5,
      "max_locator_scan_bytes": 1048576,
      "snippet_chars": 200,
      "match_context_lines": 3
    }),
    &order,
  )
  .unwrap();

  assert_eq!(request.path, "/docs");
  assert_eq!(request.selector, RequestedRootSelectorV1::ExplicitRoot(root));
  assert_eq!(request.pagination.origin, PositionWindowOriginV1::AbsoluteRank(25));
  assert_eq!(request.pagination.limit, 25);
  assert!(request.position.is_none());
  assert_eq!(request.order_by.len(), 1);
  assert_eq!(request.select, vec!["@path".to_string(), "priority".to_string()]);
  assert!(request.include_total);
  assert!(request.locators.include_matches);
  assert_eq!(request.locators.snippet_characters, 200);
  assert_eq!(request.locators.match_context_lines, 3);

  let QueryExpressionV1::And(children) = request.expression else {
    panic!("query expression must retain boolean structure");
  };
  assert_eq!(children.len(), 2);
  let QueryExpressionV1::Field(predicate) = &children[1] else {
    panic!("second expression must be a predicate");
  };
  assert!(matches!(predicate.operation, QueryPredicateOperationV1::In(ref values) if values.len() == 3));
}

fn operation_names(expression: &QueryExpressionV1, names: &mut Vec<&'static str>) {
  match expression {
    QueryExpressionV1::Field(predicate) => names.push(predicate.operation.name()),
    QueryExpressionV1::And(children) | QueryExpressionV1::Or(children) => {
      for child in children {
        operation_names(child, names);
      }
    }
    QueryExpressionV1::Not(child) => operation_names(child, names),
  }
}

#[test]
fn every_public_predicate_and_boolean_form_reaches_the_typed_planner_ast() {
  let order = query_order(HashAlgorithm::Blake3_256);
  let request = parse(
    json!({
      "path": "/docs",
      "where": {
        "and": [
          { "field": "a", "op": "eq", "value": null },
          { "field": "b", "op": "in", "value": [true, -2, 3, 4.5, "five"] },
          { "field": "c", "op": "gt", "value": 1 },
          { "field": "d", "op": "lt", "value": 9 },
          { "field": "e", "op": "between", "value": 2, "value2": 8 },
          { "or": [
            { "field": "f", "op": "contains", "value": "needle" },
            { "field": "g", "op": "similar", "value": "near", "threshold": 0.75 }
          ] },
          { "field": "h", "op": "phonetic", "value": "sound" },
          { "not": { "field": "i", "op": "fuzzy", "value": "fuzz", "algorithm": "jaro_winkler", "fuzziness": 8 } },
          { "field": "j", "op": "match", "value": "tokens" }
        ]
      }
    }),
    &order,
  )
  .unwrap();
  let mut names = Vec::new();
  operation_names(&request.expression, &mut names);
  assert_eq!(names, vec!["eq", "in", "gt", "lt", "between", "contains", "similar", "phonetic", "fuzzy", "match"]);

  let QueryExpressionV1::And(children) = request.expression else {
    panic!("root must remain an and expression");
  };
  let QueryExpressionV1::Not(fuzzy) = &children[7] else {
    panic!("not must remain explicit");
  };
  let QueryExpressionV1::Field(fuzzy) = fuzzy.as_ref() else {
    panic!("not child must remain a predicate");
  };
  assert!(matches!(
    fuzzy.operation,
    QueryPredicateOperationV1::Fuzzy {
      algorithm: aeordb::engine::v4::query_planner::QueryFuzzyAlgorithmV1::JaroWinkler,
      edits: Some(8),
      ..
    }
  ));

  let legacy_empty = parse(json!({ "path": "/docs", "where": [] }), &order).unwrap();
  assert!(matches!(legacy_empty.expression, QueryExpressionV1::And(ref children) if children.is_empty()));
}

#[test]
fn selector_and_pagination_fail_closed_without_implicit_precedence_or_legacy_cursor() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let order = query_order(hash_algorithm);
  let root = vec![0x31; hash_algorithm.hash_length()];
  let base = json!({ "path": "/docs", "where": { "field": "status", "op": "eq", "value": "ready" } });

  let mut conflicting_selectors = base.clone();
  conflicting_selectors["root_hash"] = json!(hex::encode(&root));
  conflicting_selectors["snapshot"] = json!("before-import");
  assert_eq!(parse(conflicting_selectors, &order).unwrap_err().code(), "INVALID_ROOT_SELECTOR");

  let mut conflicting_origins = base.clone();
  conflicting_origins["page"] = json!(1);
  conflicting_origins["offset"] = json!(0);
  assert_eq!(parse(conflicting_origins, &order).unwrap_err().code(), "INVALID_PAGINATION");

  let mut position_without_root = base.clone();
  position_without_root["after"] = json!(position_token(&order, &root));
  assert_eq!(parse(position_without_root, &order).unwrap_err().code(), "INVALID_PAGINATION");

  let mut position_with_alias = base.clone();
  position_with_alias["snapshot"] = json!("before-import");
  position_with_alias["after"] = json!(position_token(&order, &root));
  assert_eq!(parse(position_with_alias, &order).unwrap_err().code(), "INVALID_PAGINATION");

  let mut conflicting_positions = base.clone();
  conflicting_positions["root_hash"] = json!(hex::encode(&root));
  conflicting_positions["after"] = json!(position_token(&order, &root));
  conflicting_positions["before"] = json!(position_token(&order, &root));
  assert_eq!(parse(conflicting_positions, &order).unwrap_err().code(), "INVALID_PAGINATION");

  let mut legacy_cursor = base.clone();
  legacy_cursor["root_hash"] = json!(hex::encode(&root));
  legacy_cursor["after"] = json!(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, br#"{"offset":10}"#));
  assert_eq!(parse(legacy_cursor, &order).unwrap_err().code(), "INVALID_POSITION_CURSOR");

  let mut zero_page = base.clone();
  zero_page["page"] = json!(0);
  assert_eq!(parse(zero_page, &order).unwrap_err().code(), "INVALID_PAGINATION");

  let mut zero_limit = base;
  zero_limit["limit"] = json!(0);
  assert_eq!(parse(zero_limit, &order).unwrap_err().code(), "INVALID_PAGINATION");

  let overflow_page = json!({
    "path": "/docs",
    "where": { "field": "status", "op": "eq", "value": "ready" },
    "page": u64::MAX,
    "limit": 1000
  });
  assert_eq!(parse(overflow_page, &order).unwrap_err().code(), "INVALID_PAGINATION");
}

#[test]
fn canonical_apos_is_checked_against_explicit_root_route_and_order_before_execution() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let order = query_order(hash_algorithm);
  let other_order = compile_query_order_v1(hash_algorithm, &[]).unwrap();
  let root = vec![0x31; hash_algorithm.hash_length()];
  let token = position_token(&order, &root);
  let request = json!({
    "path": "/docs",
    "root_hash": hex::encode(&root),
    "where": { "field": "status", "op": "eq", "value": "ready" },
    "after": token,
    "limit": 10
  });

  let parsed = parse(request.clone(), &order).unwrap();
  assert_eq!(parsed.pagination.origin, PositionWindowOriginV1::After);
  assert_eq!(parsed.position.as_ref().unwrap().namespace_root(), root);

  assert_eq!(parse(request.clone(), &other_order).unwrap_err().code(), "POSITION_ORDER_MISMATCH");

  let mut wrong_root = request.clone();
  wrong_root["root_hash"] = json!("44".repeat(32));
  assert_eq!(parse(wrong_root, &order).unwrap_err().code(), "POSITION_ROOT_MISMATCH");

  let route_context = PublicPositionContextV1 { route: PositionRouteV1::GlobalSearch, order_fingerprint: order.fingerprint() };
  assert_eq!(
    parse_public_query_request_v1(&serde_json::to_vec(&request).unwrap(), hash_algorithm, route_context).unwrap_err().code(),
    "INVALID_POSITION_CURSOR"
  );
}

#[test]
fn raw_request_ast_limits_reject_amplification_and_malformed_queries_before_planning() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let order = query_order(hash_algorithm);

  let oversized = vec![b' '; PUBLIC_QUERY_MAXIMUM_REQUEST_BYTES_V1 + 1];
  assert_eq!(parse_public_query_request_v1(&oversized, hash_algorithm, context(&order)).unwrap_err().code(), "QUERY_REQUEST_TOO_LARGE");

  assert_eq!(
    parse_public_query_request_v1(br#"{"path":"/docs","where":]"#, hash_algorithm, context(&order)).unwrap_err().code(),
    "INVALID_QUERY_REQUEST"
  );

  for invalid_where in [
    json!({ "field": "", "op": "eq", "value": "x" }),
    json!({ "field": "x", "op": "wat", "value": "x" }),
    json!({ "field": "x", "op": "between", "value": 1 }),
    json!({ "field": "x", "op": "in", "value": "not-an-array" }),
    json!({ "and": "not-an-array" }),
    json!({ "field": "x", "op": "eq", "value": "x", "threshold": 0.5 }),
    json!({ "field": "x", "op": "fuzzy", "value": "x", "algorithm": 7 }),
    json!({ "field": "x", "op": "similar", "value": "x", "threshold": 1.1 }),
  ] {
    let body = json!({ "path": "/docs", "where": invalid_where });
    assert_eq!(parse(body, &order).unwrap_err().code(), "INVALID_QUERY_EXPRESSION");
  }

  let too_many_literals = (0..=4096).map(|value| json!(value)).collect::<Vec<_>>();
  let body = json!({ "path": "/docs", "where": { "field": "x", "op": "in", "value": too_many_literals } });
  assert_eq!(parse(body, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let excessive_fuzziness = json!({
    "path": "/docs",
    "where": { "field": "x", "op": "fuzzy", "value": "x", "fuzziness": 9 }
  });
  assert_eq!(parse(excessive_fuzziness, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let large_literal = "x".repeat(1_048_577);
  let body = json!({ "path": "/docs", "where": { "field": "x", "op": "eq", "value": large_literal } });
  assert_eq!(parse(body, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let total_literals = (0..9).map(|_| "x".repeat(1_000_000)).collect::<Vec<_>>();
  let body = json!({ "path": "/docs", "where": { "field": "x", "op": "in", "value": total_literals } });
  assert_eq!(parse(body, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let mut nested = json!({ "field": "x", "op": "eq", "value": "y" });
  for _ in 0..=32 {
    nested = json!({ "not": nested });
  }
  assert_eq!(parse(json!({ "path": "/docs", "where": nested }), &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let nodes = (0..=1024).map(|index| json!({ "field": format!("field-{index}"), "op": "eq", "value": index })).collect::<Vec<_>>();
  assert_eq!(parse(json!({ "path": "/docs", "where": { "and": nodes } }), &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");
}

#[test]
fn query_auxiliary_lists_locator_limits_and_unknown_fields_are_strictly_bounded() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let order = query_order(hash_algorithm);
  let base = json!({ "path": "/docs", "where": { "field": "x", "op": "eq", "value": "y" } });

  let defaults = parse(base.clone(), &order).unwrap();
  assert!(!defaults.locators.include_matches);
  assert_eq!(defaults.locators.maximum_matches_per_result, 5);
  assert_eq!(defaults.locators.maximum_scan_bytes, 256 * 1_048_576);
  assert_eq!(defaults.locators.snippet_characters, 160);
  assert_eq!(defaults.locators.match_context_lines, 2);

  let mut too_many_sort_fields = base.clone();
  too_many_sort_fields["order_by"] = json!((0..33).map(|index| json!({ "field": format!("field-{index}") })).collect::<Vec<_>>());
  assert_eq!(parse(too_many_sort_fields, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let mut too_many_aggregate_fields = base.clone();
  too_many_aggregate_fields["aggregate"] = json!({ "sum": (0..33).map(|index| format!("field-{index}")).collect::<Vec<_>>() });
  assert_eq!(parse(too_many_aggregate_fields, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let mut too_many_group_fields = base.clone();
  too_many_group_fields["aggregate"] = json!({ "group_by": (0..33).map(|index| format!("field-{index}")).collect::<Vec<_>>() });
  assert_eq!(parse(too_many_group_fields, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  let mut too_many_selected_fields = base.clone();
  too_many_selected_fields["select"] = json!((0..257).map(|index| format!("field-{index}")).collect::<Vec<_>>());
  assert_eq!(parse(too_many_selected_fields, &order).unwrap_err().code(), "QUERY_EXPRESSION_LIMIT_EXCEEDED");

  for (field, value) in [
    ("max_matches_per_result", json!(0)),
    ("max_matches_per_result", json!(1025)),
    ("max_locator_scan_bytes", json!(0)),
    ("max_locator_scan_bytes", json!(256 * 1_048_576_u64 + 1)),
    ("snippet_chars", json!(0)),
    ("snippet_chars", json!(4097)),
    ("match_context_lines", json!(1025)),
  ] {
    let mut request = base.clone();
    request[field] = value;
    assert_eq!(parse(request, &order).unwrap_err().code(), "INVALID_QUERY_REQUEST");
  }

  let mut unknown = base.clone();
  unknown["mystery"] = json!(true);
  assert_eq!(parse(unknown, &order).unwrap_err().code(), "INVALID_QUERY_REQUEST");

  let duplicate = br#"{"path":"/docs","path":"/other","where":{"field":"x","op":"eq","value":"y"}}"#;
  assert_eq!(parse_public_query_request_v1(duplicate, hash_algorithm, context(&order)).unwrap_err().code(), "INVALID_QUERY_REQUEST");
}

#[test]
fn rooted_envelopes_preserve_items_and_results_collection_names() {
  let root = RootResponseV1 { hash: "31".repeat(32), state: "retained", expires_at: None };
  let metadata =
    PublicCollectionMetadataV1 { has_more: Some(true), next_cursor: Some("apos".to_string()), ..PublicCollectionMetadataV1::default() };
  let items = PublicItemsResponseV1::new(root.clone(), vec![json!({ "path": "/docs/a.txt" })], metadata);
  assert_eq!(
    serde_json::to_value(items).unwrap(),
    json!({
      "root": { "hash": "31".repeat(32), "state": "retained", "expires_at": null },
      "items": [{ "path": "/docs/a.txt" }],
      "has_more": true,
      "next_cursor": "apos"
    })
  );

  let results = PublicResultsResponseV1::new(
    root,
    vec![json!({ "path": "/docs/a.txt", "score": 1.0 })],
    PublicCollectionMetadataV1 { has_more: Some(false), ..PublicCollectionMetadataV1::default() },
  );
  let value = serde_json::to_value(results).unwrap();
  assert!(value.get("items").is_none());
  assert_eq!(value["results"].as_array().unwrap().len(), 1);
  assert_eq!(value["root"]["state"], "retained");
}

#[test]
fn locator_range_and_sse_wire_types_expose_logical_identity_without_physical_leakage() {
  let locator = PublicLocatorMatchV1 {
    path: "/docs/a.txt".to_string(),
    file_key: "41".repeat(32),
    record_revision: "51".repeat(32),
    content_hash: "61".repeat(32),
    updated_at: 1_700_000_000_000,
    matching_semantics: PublicLocatorMatchSemanticsV1::ExactBytes,
    byte_range: PublicHalfOpenRangeV1 { start: 8, end: 13 },
    unicode_scalar_range: Some(PublicHalfOpenRangeV1 { start: 7, end: 12 }),
    line_column_range: Some(PublicLineColumnRangeV1 {
      start: PublicLineColumnPointV1 { line: 2, column: 1 },
      end: PublicLineColumnPointV1 { line: 2, column: 6 },
    }),
    continuation: Some(PublicLocatorContinuationV1 { next_candidate_byte: 13 }),
  };
  let locator_value = serde_json::to_value(locator).unwrap();
  assert_eq!(locator_value["matching_semantics"], "exact_bytes");
  assert_eq!(locator_value["byte_range"], json!({ "start": 8, "end": 13 }));
  assert!(locator_value.get("offset").is_none());
  assert!(locator_value.get("physical_incarnation").is_none());

  let byte_range = PublicRangeSelectionV1::Bytes { start: 8, end: 13 }.validate().unwrap();
  assert_eq!(serde_json::to_value(byte_range).unwrap(), json!({ "unit": "bytes", "start": 8, "end": 13 }));
  assert_eq!(PublicRangeSelectionV1::Bytes { start: 13, end: 8 }.validate().unwrap_err().code(), "INVALID_RANGE");
  let continuation = PublicRangeContinuationV1 { remaining: PublicHalfOpenRangeV1 { start: 13, end: 21 } };
  assert_eq!(serde_json::to_value(continuation).unwrap(), json!({ "remaining": { "start": 13, "end": 21 } }));

  let event = PublicMutationEventMetadataV1 {
    operation_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
    mutation_kind: PublicMutationKindV1::Rename,
    publication_sequence: 42,
    previous_root_hash: "71".repeat(32),
    root_hash: "81".repeat(32),
    affected_relationships: vec![
      PublicAffectedRelationshipV1 {
        path: "/docs/old.txt".to_string(),
        entry_type: Some(PublicEntryTypeV1::File),
        change: PublicAffectedRelationshipChangeV1::Deleted,
      },
      PublicAffectedRelationshipV1 {
        path: "/docs/new.txt".to_string(),
        entry_type: Some(PublicEntryTypeV1::File),
        change: PublicAffectedRelationshipChangeV1::Created,
      },
    ],
  };
  let event_value = serde_json::to_value(event).unwrap();
  assert_eq!(event_value["affected_relationships"][0]["change"], "deleted");
  let encoded = serde_json::to_string(&event_value).unwrap();
  for forbidden in ["locator_replacements", "stable_key", "physical_incarnation", "previous_identity", "new_identity"] {
    assert!(!encoded.contains(forbidden), "public event leaked internal relationship authority: {forbidden}");
  }

  let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/root_public_schema.rs")).unwrap();
  for forbidden in [
    "LocatorPhysicalIncarnation",
    "LocatorReplacement",
    "locator_replacements",
    "stable_key",
    "StorageEngine",
    "DirectoryOps",
    "AppState",
    "QueryEngine",
    "parse_where_clause",
    "range_extract",
    "search_locators",
    "wasm_runtime",
  ] {
    assert!(!source.contains(forbidden), "public schema depends on physical mutation state: {forbidden}");
  }
}
