use super::parse_query_from_json;

#[test]
fn plugin_query_rejects_present_options_with_wrong_types() {
  for (field, value) in [
    ("limit", serde_json::json!("5")),
    ("offset", serde_json::json!(-1)),
    ("after", serde_json::json!(7)),
    ("before", serde_json::json!(7)),
    ("include_total", serde_json::json!("true")),
    ("order_by", serde_json::json!({})),
    ("aggregate", serde_json::json!([])),
  ] {
    let mut request = serde_json::json!({"path": "/docs"});
    request[field] = value;
    assert!(parse_query_from_json(&request).is_err(), "malformed {field} must not become an omitted option");
  }
}

#[test]
fn plugin_query_rejects_malformed_sort_fields() {
  for order_by in [
    serde_json::json!([{"direction": "asc"}]),
    serde_json::json!([{"field": 7, "direction": "asc"}]),
    serde_json::json!([{"field": "name", "direction": 7}]),
    serde_json::json!([{"field": "name", "direction": "sideways"}]),
  ] {
    let request = serde_json::json!({"path": "/docs", "order_by": order_by});
    assert!(parse_query_from_json(&request).is_err(), "malformed sort fields must not be dropped or changed to ascending");
  }
}

#[test]
fn plugin_query_rejects_malformed_aggregate_fields() {
  for aggregate in [
    serde_json::json!({"count": "true"}),
    serde_json::json!({"sum": "size"}),
    serde_json::json!({"avg": ["size", 7]}),
    serde_json::json!({"min": null}),
  ] {
    let request = serde_json::json!({"path": "/docs", "aggregate": aggregate});
    assert!(parse_query_from_json(&request).is_err(), "malformed aggregate fields must not become empty aggregate operations");
  }
}

#[test]
fn plugin_query_defaults_only_absent_optional_fields() {
  let query = parse_query_from_json(&serde_json::json!({"path": "/docs"})).unwrap();
  assert_eq!(query.path, "/docs");
  assert!(query.limit.is_none());
  assert!(query.offset.is_none());
  assert!(query.order_by.is_empty());
  assert!(query.aggregate.is_none());
}
