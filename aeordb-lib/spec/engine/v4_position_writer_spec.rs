use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::position::{
  CanonicalRouteOrderDefinitionV1, LogicalPositionWriteV1, PositionComparatorV1, PositionComponentStateV1, PositionComponentWriteV1,
  PositionRouteV1, PositionSortDefinitionV1, PositionSortDirectionV1, compile_route_order_definition, decode_logical_position,
  encode_logical_position,
};
use aeordb::engine::v4::reader::MalformedInputClass;

const HASH32: &str = "blake3-256";
const HASH64: &str = "sha512";

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/logical-position-v1")
}

fn fixture_name(algorithm: HashAlgorithm, route: PositionRouteV1) -> String {
  let profile = match algorithm {
    HashAlgorithm::Blake3_256 => HASH32,
    HashAlgorithm::Sha512 => HASH64,
    other => panic!("fixture profile is not defined for {other:?}"),
  };
  format!("apos-{profile}-{}-valid.bin", route.name())
}

fn sample_sort(route: PositionRouteV1) -> Vec<PositionSortDefinitionV1<'static>> {
  use PositionSortDirectionV1::{Ascending, Descending};
  match route {
    PositionRouteV1::DirectoryListing => vec![
      PositionSortDefinitionV1 { field: "category", direction: Ascending, comparator: "u64_order_v1" },
      PositionSortDefinitionV1 { field: "name_folded", direction: Ascending, comparator: "utf8_binary_order_v1" },
      PositionSortDefinitionV1 { field: "name_raw", direction: Ascending, comparator: "utf8_binary_order_v1" },
    ],
    PositionRouteV1::Query => vec![
      PositionSortDefinitionV1 { field: "@path", direction: Ascending, comparator: "utf8_binary_order_v1" },
      PositionSortDefinitionV1 { field: "optional", direction: Ascending, comparator: "null" },
      PositionSortDefinitionV1 { field: "missing", direction: Ascending, comparator: "missing" },
    ],
    PositionRouteV1::GlobalSearch => vec![
      PositionSortDefinitionV1 { field: "@score", direction: Descending, comparator: "f64_finite_order_v1" },
      PositionSortDefinitionV1 { field: "@path", direction: Ascending, comparator: "utf8_binary_order_v1" },
    ],
    PositionRouteV1::AggregateGroups => vec![
      PositionSortDefinitionV1 { field: "@count", direction: Descending, comparator: "u64_order_v1" },
      PositionSortDefinitionV1 { field: "group_tuple", direction: Ascending, comparator: "bytes_binary_order_v1" },
    ],
  }
}

fn sample_order<'a>(route: PositionRouteV1, sort: &'a [PositionSortDefinitionV1<'a>]) -> CanonicalRouteOrderDefinitionV1<'a> {
  let (directories, collation, nulls, multi, score) = match route {
    PositionRouteV1::DirectoryListing => {
      ("always", "aeor-listing-lowercase-then-raw-utf8-v1", "not-applicable", "not-applicable", "not-applicable")
    }
    PositionRouteV1::Query => (
      "not-applicable",
      "not-applicable",
      "present-null-missing-only-present-reverses",
      "minimum-ascending-maximum-descending",
      "not-applicable",
    ),
    PositionRouteV1::GlobalSearch => (
      "not-applicable",
      "not-applicable",
      "present-null-missing-only-present-reverses",
      "minimum-ascending-maximum-descending",
      "corrected-finite-score-v1",
    ),
    PositionRouteV1::AggregateGroups => (
      "not-applicable",
      "not-applicable",
      "present-null-missing-only-present-reverses",
      "minimum-ascending-maximum-descending",
      "not-applicable",
    ),
  };
  let sort_json = format!(
    "[{}]",
    sort
      .iter()
      .map(|row| { format!(r#"{{"field":"{}","direction":"{}","comparator":"{}"}}"#, row.field, row.direction.name(), row.comparator) })
      .collect::<Vec<_>>()
      .join(",")
  );
  let semantics = format!(
    "route={};sort={sort_json};directories={directories};collation={collation};nulls={nulls};multi={multi};score={score}",
    route as u16
  );
  let fingerprint = blake3::hash(semantics.as_bytes()).to_hex().to_string();
  CanonicalRouteOrderDefinitionV1 {
    route,
    sort,
    directories_first: directories,
    name_collation: collation,
    null_missing_policy: nulls,
    multi_value_selector: multi,
    score_semantics: score,
    semantic_fingerprints: vec![fingerprint],
  }
}

fn write_components<'a>(position: &'a aeordb::engine::v4::position::LogicalPositionV1) -> Vec<PositionComponentWriteV1<'a>> {
  position
    .components()
    .map(|component| {
      let component = component.unwrap();
      PositionComponentWriteV1 { comparator: component.comparator, state: component.state, payload: component.payload }
    })
    .collect()
}

#[test]
fn writer_matches_every_ordinary_independent_fixture_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for route in
      [PositionRouteV1::DirectoryListing, PositionRouteV1::Query, PositionRouteV1::GlobalSearch, PositionRouteV1::AggregateGroups]
    {
      let expected = fs::read(fixture_root().join(fixture_name(algorithm, route))).unwrap();
      let decoded = decode_logical_position(&expected, algorithm).unwrap();
      let sort = sample_sort(route);
      let definition = sample_order(route, &sort);
      let compiled = compile_route_order_definition(algorithm, &definition).unwrap();
      let components = write_components(&decoded);
      let encoded = encode_logical_position(&LogicalPositionWriteV1 {
        order: &compiled,
        namespace_root: decoded.namespace_root(),
        file_key_tie: decoded.file_key_tie(),
        record_revision_tie: decoded.record_revision_tie(),
        components: &components,
      })
      .unwrap();

      assert_eq!(encoded, expected, "{algorithm:?} {}", route.name());
      let round_trip = decode_logical_position(&encoded, algorithm).unwrap();
      assert_eq!(round_trip.route, route);
      assert_eq!(round_trip.order_fingerprint(), compiled.fingerprint());
      assert_eq!(round_trip.sort_tuple(), decoded.sort_tuple());
    }
  }
}

#[test]
fn writer_matches_the_exact_one_mib_independent_boundary_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = fs::read(
      fixture_root()
        .join(format!("apos-{}-maximum-decoded-length-valid.bin", if algorithm == HashAlgorithm::Blake3_256 { HASH32 } else { HASH64 })),
    )
    .unwrap();
    let decoded = decode_logical_position(&expected, algorithm).unwrap();
    let fields: Vec<String> = (0..32).map(|index| format!("field-{index:02}")).collect();
    let sort: Vec<_> = fields
      .iter()
      .map(|field| PositionSortDefinitionV1 { field, direction: PositionSortDirectionV1::Ascending, comparator: "bytes_binary_order_v1" })
      .collect();
    let sort_json = serde_json::to_string(
      &sort
        .iter()
        .map(|row| serde_json::json!({ "field": row.field, "direction": "asc", "comparator": row.comparator }))
        .collect::<Vec<_>>(),
    )
    .unwrap();
    let definition = CanonicalRouteOrderDefinitionV1 {
      route: PositionRouteV1::Query,
      sort: &sort,
      directories_first: "not-applicable",
      name_collation: "not-applicable",
      null_missing_policy: "present-null-missing-only-present-reverses",
      multi_value_selector: "minimum-ascending-maximum-descending",
      score_semantics: "not-applicable",
      semantic_fingerprints: vec![blake3::hash(sort_json.as_bytes()).to_hex().to_string()],
    };
    let compiled = compile_route_order_definition(algorithm, &definition).unwrap();
    let components = write_components(&decoded);
    let encoded = encode_logical_position(&LogicalPositionWriteV1 {
      order: &compiled,
      namespace_root: decoded.namespace_root(),
      file_key_tie: decoded.file_key_tie(),
      record_revision_tie: decoded.record_revision_tie(),
      components: &components,
    })
    .unwrap();

    assert_eq!(encoded, expected);
    assert_eq!(decode_logical_position(&encoded, algorithm).unwrap().decoded_len(), 1_048_576);
  }
}

#[test]
fn route_order_compilation_rejects_incomplete_or_noncanonical_semantics() {
  let sort = sample_sort(PositionRouteV1::Query);
  let valid = sample_order(PositionRouteV1::Query, &sort);
  assert!(compile_route_order_definition(HashAlgorithm::Blake3_256, &valid).is_ok());

  let empty_sort = CanonicalRouteOrderDefinitionV1 { sort: &[], ..valid.clone() };
  assert_eq!(
    compile_route_order_definition(HashAlgorithm::Blake3_256, &empty_sort).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  let empty_policy = CanonicalRouteOrderDefinitionV1 { directories_first: "", ..valid.clone() };
  assert_eq!(
    compile_route_order_definition(HashAlgorithm::Blake3_256, &empty_policy).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  let bad_fingerprint = CanonicalRouteOrderDefinitionV1 { semantic_fingerprints: vec!["A".repeat(64)], ..valid.clone() };
  assert_eq!(
    compile_route_order_definition(HashAlgorithm::Blake3_256, &bad_fingerprint).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  let bad_sort = [PositionSortDefinitionV1 {
    field: "@path",
    direction: PositionSortDirectionV1::Ascending,
    comparator: "locale-dependent-mystery-order",
  }];
  let bad_comparator = CanonicalRouteOrderDefinitionV1 { sort: &bad_sort, ..valid };
  assert_eq!(
    compile_route_order_definition(HashAlgorithm::Blake3_256, &bad_comparator).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let oversized_field = "x".repeat(64 * 1024 + 1);
  let oversized_sort = [PositionSortDefinitionV1 {
    field: &oversized_field,
    direction: PositionSortDirectionV1::Ascending,
    comparator: "utf8_binary_order_v1",
  }];
  let oversized = CanonicalRouteOrderDefinitionV1 { sort: &oversized_sort, ..sample_order(PositionRouteV1::Query, &sort) };
  assert_eq!(
    compile_route_order_definition(HashAlgorithm::Blake3_256, &oversized).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

#[test]
fn writer_round_trips_every_component_kind_and_database_hash_profile() {
  let comparators = [
    "bytes_binary_order_v1",
    "utf8_binary_order_v1",
    "u64_order_v1",
    "i64_order_v1",
    "f64_finite_order_v1",
    "timestamp_ms_order_v1",
    "bool_order_v1",
    "null",
    "missing",
  ];
  let fields: Vec<String> = (0..comparators.len()).map(|index| format!("field-{index}")).collect();
  let sort: Vec<_> = fields
    .iter()
    .zip(comparators)
    .map(|(field, comparator)| PositionSortDefinitionV1 { field, direction: PositionSortDirectionV1::Ascending, comparator })
    .collect();
  let definition = CanonicalRouteOrderDefinitionV1 {
    route: PositionRouteV1::Query,
    sort: &sort,
    directories_first: "not-applicable",
    name_collation: "not-applicable",
    null_missing_policy: "present-null-missing-only-present-reverses",
    multi_value_selector: "minimum-ascending-maximum-descending",
    score_semantics: "not-applicable",
    semantic_fingerprints: vec!["0123456789abcdef".repeat(4)],
  };
  let u64_bytes = u64::MAX.to_le_bytes();
  let i64_bytes = i64::MIN.to_le_bytes();
  let f64_bytes = 1.5f64.to_le_bytes();
  let timestamp_bytes = (-1_234_567i64).to_le_bytes();
  let components = [
    PositionComponentWriteV1::bytes(&[]),
    PositionComponentWriteV1::utf8("Straße".as_bytes()),
    PositionComponentWriteV1 { comparator: Some(PositionComparatorV1::U64), state: PositionComponentStateV1::Present, payload: &u64_bytes },
    PositionComponentWriteV1 { comparator: Some(PositionComparatorV1::I64), state: PositionComponentStateV1::Present, payload: &i64_bytes },
    PositionComponentWriteV1 {
      comparator: Some(PositionComparatorV1::FiniteF64),
      state: PositionComponentStateV1::Present,
      payload: &f64_bytes,
    },
    PositionComponentWriteV1 {
      comparator: Some(PositionComparatorV1::TimestampMs),
      state: PositionComponentStateV1::Present,
      payload: &timestamp_bytes,
    },
    PositionComponentWriteV1::boolean_payload(&[1]),
    PositionComponentWriteV1::typed_null(),
    PositionComponentWriteV1::missing(),
  ];

  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let compiled = compile_route_order_definition(algorithm, &definition).unwrap();
    let identity = vec![0x5a; algorithm.hash_length()];
    let encoded = encode_logical_position(&LogicalPositionWriteV1 {
      order: &compiled,
      namespace_root: &identity,
      file_key_tie: &identity,
      record_revision_tie: &identity,
      components: &components,
    })
    .unwrap();
    let decoded = decode_logical_position(&encoded, algorithm).unwrap();
    assert_eq!(decoded.components().count(), components.len());
    assert_eq!(decoded.order_fingerprint(), compiled.fingerprint());
  }
}

#[test]
fn writer_rejects_every_noncanonical_component_payload() {
  let nan = f64::NAN.to_le_bytes();
  let negative_zero = (-0.0f64).to_le_bytes();
  let cases: Vec<PositionComponentWriteV1<'_>> = vec![
    PositionComponentWriteV1::utf8(&[0xff]),
    PositionComponentWriteV1 { comparator: Some(PositionComparatorV1::U64), state: PositionComponentStateV1::Present, payload: &[0; 7] },
    PositionComponentWriteV1 { comparator: Some(PositionComparatorV1::I64), state: PositionComponentStateV1::Present, payload: &[0; 9] },
    PositionComponentWriteV1 { comparator: Some(PositionComparatorV1::FiniteF64), state: PositionComponentStateV1::Present, payload: &nan },
    PositionComponentWriteV1 {
      comparator: Some(PositionComparatorV1::FiniteF64),
      state: PositionComponentStateV1::Present,
      payload: &negative_zero,
    },
    PositionComponentWriteV1 {
      comparator: Some(PositionComparatorV1::TimestampMs),
      state: PositionComponentStateV1::Present,
      payload: &[0; 7],
    },
    PositionComponentWriteV1::boolean_payload(&[]),
    PositionComponentWriteV1::boolean_payload(&[2]),
    PositionComponentWriteV1 {
      comparator: Some(PositionComparatorV1::BytesBinary),
      state: PositionComponentStateV1::TypedNull,
      payload: &[],
    },
    PositionComponentWriteV1 { comparator: None, state: PositionComponentStateV1::Missing, payload: &[0] },
  ];
  let sort =
    [PositionSortDefinitionV1 { field: "value", direction: PositionSortDirectionV1::Ascending, comparator: "bytes_binary_order_v1" }];
  let definition = CanonicalRouteOrderDefinitionV1 {
    route: PositionRouteV1::Query,
    sort: &sort,
    directories_first: "not-applicable",
    name_collation: "not-applicable",
    null_missing_policy: "present-null-missing-only-present-reverses",
    multi_value_selector: "minimum-ascending-maximum-descending",
    score_semantics: "not-applicable",
    semantic_fingerprints: vec!["0123456789abcdef".repeat(4)],
  };
  let compiled = compile_route_order_definition(HashAlgorithm::Blake3_256, &definition).unwrap();
  let identity = vec![0x5a; 32];

  for component in cases {
    let components = [component];
    let result = encode_logical_position(&LogicalPositionWriteV1 {
      order: &compiled,
      namespace_root: &identity,
      file_key_tie: &identity,
      record_revision_tie: &identity,
      components: &components,
    });
    assert!(result.is_err(), "accepted {component:?}");
  }
}

#[test]
fn writer_rejects_identity_component_and_allocation_amplification_before_output() {
  let sort = sample_sort(PositionRouteV1::Query);
  let definition = sample_order(PositionRouteV1::Query, &sort);
  let compiled = compile_route_order_definition(HashAlgorithm::Blake3_256, &definition).unwrap();
  let identity = vec![1; 32];
  let components =
    [PositionComponentWriteV1::utf8(b"/docs/a.json"), PositionComponentWriteV1::typed_null(), PositionComponentWriteV1::missing()];
  let valid = LogicalPositionWriteV1 {
    order: &compiled,
    namespace_root: &identity,
    file_key_tie: &identity,
    record_revision_tie: &identity,
    components: &components,
  };
  assert!(encode_logical_position(&valid).is_ok());

  let zero = vec![0; 32];
  assert_eq!(
    encode_logical_position(&LogicalPositionWriteV1 { namespace_root: &zero, ..valid }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_logical_position(&LogicalPositionWriteV1 { file_key_tie: &identity[..31], ..valid }).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    encode_logical_position(&LogicalPositionWriteV1 { components: &components[..2], ..valid }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let invalid_bool =
    [PositionComponentWriteV1::boolean_payload(&[2]), PositionComponentWriteV1::typed_null(), PositionComponentWriteV1::missing()];
  assert_eq!(
    encode_logical_position(&LogicalPositionWriteV1 { components: &invalid_bool, ..valid }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  let invalid_state = [
    PositionComponentWriteV1 { comparator: None, state: PositionComponentStateV1::Present, payload: &[] },
    PositionComponentWriteV1::typed_null(),
    PositionComponentWriteV1::missing(),
  ];
  assert_eq!(
    encode_logical_position(&LogicalPositionWriteV1 { components: &invalid_state, ..valid }).unwrap_err().class(),
    MalformedInputClass::NoncanonicalBooleanOrOptionalPresence
  );

  let oversized_payload = vec![0xa5; 1_048_576];
  let oversized =
    [PositionComponentWriteV1::bytes(&oversized_payload), PositionComponentWriteV1::typed_null(), PositionComponentWriteV1::missing()];
  assert_eq!(
    encode_logical_position(&LogicalPositionWriteV1 { components: &oversized, ..valid }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}
