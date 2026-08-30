use std::cmp::Ordering;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::position::{PositionComparatorV1, PositionRouteV1, PositionSortDirectionV1};
use aeordb::engine::v4::position_order::{
  AggregateOrderFieldV1, DirectoryOrderFieldV1, LogicalOrderComponentOwnedV1, LogicalOrderRowOwnedV1, LogicalOrderRowV1,
  PositionOrderErrorClassV1, PositionOrderFieldV1, PositionPaginationInputsV1, PositionWindowDirectionV1, PositionWindowLimitsV1,
  PositionWindowOriginV1, PositionWindowScanReceiptV1, PositionWindowScanRequestV1, PositionWindowSeekV1, PositionWindowSourceV1,
  PositionWindowVisitorV1, compare_logical_order_rows_v1, compile_aggregate_group_order_v1, compile_directory_listing_order_v1,
  compile_global_search_order_v1, compile_query_order_v1, execute_position_window_v1, plan_position_window_v1,
};

fn identity(byte: u8) -> Vec<u8> {
  vec![byte; 32]
}

fn present(comparator: PositionComparatorV1, payload: impl Into<Vec<u8>>) -> LogicalOrderComponentOwnedV1 {
  LogicalOrderComponentOwnedV1::present(comparator, payload.into())
}

fn row(route: PositionRouteV1, components: Vec<LogicalOrderComponentOwnedV1>, key: u8) -> LogicalOrderRowOwnedV1 {
  LogicalOrderRowOwnedV1 { route, components, file_key_tie: identity(key), record_revision_tie: identity(key.wrapping_add(100) | 1) }
}

fn query_path_row(path: &str, key: u8) -> LogicalOrderRowOwnedV1 {
  row(PositionRouteV1::Query, vec![present(PositionComparatorV1::Utf8Binary, path.as_bytes())], key)
}

fn compare(
  order: &aeordb::engine::v4::position::CompiledRouteOrderV1,
  left: &LogicalOrderRowOwnedV1,
  right: &LogicalOrderRowOwnedV1,
) -> Ordering {
  compare_logical_order_rows_v1(order, left.as_borrowed(), right.as_borrowed()).unwrap()
}

#[test]
fn route_builders_freeze_complete_directory_query_search_and_aggregate_orders() {
  let directory_desc =
    compile_directory_listing_order_v1(HashAlgorithm::Blake3_256, DirectoryOrderFieldV1::Name, PositionSortDirectionV1::Descending)
      .unwrap();
  let mut directory_rows = [
    row(
      PositionRouteV1::DirectoryListing,
      vec![
        present(PositionComparatorV1::U64, 1u64.to_le_bytes()),
        present(PositionComparatorV1::Utf8Binary, b"a"),
        present(PositionComparatorV1::Utf8Binary, b"A"),
        present(PositionComparatorV1::Utf8Binary, b"/A"),
      ],
      4,
    ),
    row(
      PositionRouteV1::DirectoryListing,
      vec![
        present(PositionComparatorV1::U64, 0u64.to_le_bytes()),
        present(PositionComparatorV1::Utf8Binary, b"z"),
        present(PositionComparatorV1::Utf8Binary, b"z"),
        present(PositionComparatorV1::Utf8Binary, b"/z"),
      ],
      1,
    ),
    row(
      PositionRouteV1::DirectoryListing,
      vec![
        present(PositionComparatorV1::U64, 1u64.to_le_bytes()),
        present(PositionComparatorV1::Utf8Binary, b"z"),
        present(PositionComparatorV1::Utf8Binary, b"z"),
        present(PositionComparatorV1::Utf8Binary, b"/z"),
      ],
      3,
    ),
    row(
      PositionRouteV1::DirectoryListing,
      vec![
        present(PositionComparatorV1::U64, 0u64.to_le_bytes()),
        present(PositionComparatorV1::Utf8Binary, b"a"),
        present(PositionComparatorV1::Utf8Binary, b"A"),
        present(PositionComparatorV1::Utf8Binary, b"/A"),
      ],
      2,
    ),
  ];
  directory_rows.sort_by(|left, right| compare(&directory_desc, left, right));
  assert_eq!(directory_rows.iter().map(|value| value.file_key_tie[0]).collect::<Vec<_>>(), [1, 2, 3, 4]);

  let query_fields =
    [PositionOrderFieldV1 { field: "age", direction: PositionSortDirectionV1::Descending, comparator: PositionComparatorV1::U64 }];
  let query = compile_query_order_v1(HashAlgorithm::Blake3_256, &query_fields).unwrap();
  let mut query_rows = [
    row(PositionRouteV1::Query, vec![LogicalOrderComponentOwnedV1::missing(), present(PositionComparatorV1::Utf8Binary, b"/missing")], 4),
    row(
      PositionRouteV1::Query,
      vec![present(PositionComparatorV1::U64, 5u64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/five")],
      2,
    ),
    row(PositionRouteV1::Query, vec![LogicalOrderComponentOwnedV1::typed_null(), present(PositionComparatorV1::Utf8Binary, b"/null")], 3),
    row(
      PositionRouteV1::Query,
      vec![present(PositionComparatorV1::U64, 9u64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/nine")],
      1,
    ),
  ];
  query_rows.sort_by(|left, right| compare(&query, left, right));
  assert_eq!(query_rows.iter().map(|value| value.file_key_tie[0]).collect::<Vec<_>>(), [1, 2, 3, 4]);

  let search = compile_global_search_order_v1(HashAlgorithm::Blake3_256).unwrap();
  let mut search_rows = [
    row(
      PositionRouteV1::GlobalSearch,
      vec![present(PositionComparatorV1::FiniteF64, 0.5f64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/b")],
      8,
    ),
    row(
      PositionRouteV1::GlobalSearch,
      vec![present(PositionComparatorV1::FiniteF64, 0.9f64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/z")],
      7,
    ),
    row(
      PositionRouteV1::GlobalSearch,
      vec![present(PositionComparatorV1::FiniteF64, 0.5f64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/a")],
      9,
    ),
  ];
  search_rows.sort_by(|left, right| compare(&search, left, right));
  assert_eq!(search_rows.iter().map(|value| value.file_key_tie[0]).collect::<Vec<_>>(), [7, 9, 8]);

  let aggregate = compile_aggregate_group_order_v1(HashAlgorithm::Blake3_256, &[]).unwrap();
  let mut aggregate_rows = [
    row(
      PositionRouteV1::AggregateGroups,
      vec![present(PositionComparatorV1::U64, 2u64.to_le_bytes()), present(PositionComparatorV1::BytesBinary, b"b")],
      12,
    ),
    row(
      PositionRouteV1::AggregateGroups,
      vec![present(PositionComparatorV1::U64, 5u64.to_le_bytes()), present(PositionComparatorV1::BytesBinary, b"z")],
      10,
    ),
    row(
      PositionRouteV1::AggregateGroups,
      vec![present(PositionComparatorV1::U64, 2u64.to_le_bytes()), present(PositionComparatorV1::BytesBinary, b"a")],
      11,
    ),
  ];
  aggregate_rows.sort_by(|left, right| compare(&aggregate, left, right));
  assert_eq!(aggregate_rows.iter().map(|value| value.file_key_tie[0]).collect::<Vec<_>>(), [10, 11, 12]);

  let custom =
    [AggregateOrderFieldV1 { field: "group.total", direction: PositionSortDirectionV1::Ascending, comparator: PositionComparatorV1::I64 }];
  assert!(compile_aggregate_group_order_v1(HashAlgorithm::Blake3_256, &custom).is_ok());
}

#[test]
fn comparator_covers_every_permanent_type_presence_state_and_identity_tie() {
  let cases = [
    (PositionComparatorV1::BytesBinary, vec![0], vec![1]),
    (PositionComparatorV1::Utf8Binary, b"a".to_vec(), b"b".to_vec()),
    (PositionComparatorV1::U64, 1u64.to_le_bytes().to_vec(), 2u64.to_le_bytes().to_vec()),
    (PositionComparatorV1::I64, (-2i64).to_le_bytes().to_vec(), 1i64.to_le_bytes().to_vec()),
    (PositionComparatorV1::FiniteF64, (-1.5f64).to_le_bytes().to_vec(), 0.5f64.to_le_bytes().to_vec()),
    (PositionComparatorV1::TimestampMs, (-10i64).to_le_bytes().to_vec(), 5i64.to_le_bytes().to_vec()),
    (PositionComparatorV1::Boolean, vec![0], vec![1]),
  ];

  for (comparator, low, high) in cases {
    assert_eq!(PositionComparatorV1::from_name(comparator.name()), Some(comparator));
    for direction in [PositionSortDirectionV1::Ascending, PositionSortDirectionV1::Descending] {
      let fields = [PositionOrderFieldV1 { field: "value", direction, comparator }];
      let order = compile_query_order_v1(HashAlgorithm::Blake3_256, &fields).unwrap();
      let low = row(PositionRouteV1::Query, vec![present(comparator, low.clone()), present(PositionComparatorV1::Utf8Binary, b"/low")], 1);
      let high =
        row(PositionRouteV1::Query, vec![present(comparator, high.clone()), present(PositionComparatorV1::Utf8Binary, b"/high")], 2);
      let expected = if direction == PositionSortDirectionV1::Ascending { Ordering::Less } else { Ordering::Greater };
      assert_eq!(compare(&order, &low, &high), expected, "{comparator:?} {direction:?}");

      let null = row(
        PositionRouteV1::Query,
        vec![LogicalOrderComponentOwnedV1::typed_null(), present(PositionComparatorV1::Utf8Binary, b"/null")],
        3,
      );
      let missing = row(
        PositionRouteV1::Query,
        vec![LogicalOrderComponentOwnedV1::missing(), present(PositionComparatorV1::Utf8Binary, b"/missing")],
        4,
      );
      assert_eq!(compare(&order, &low, &null), Ordering::Less, "presence must not reverse");
      assert_eq!(compare(&order, &null, &missing), Ordering::Less, "null must precede missing");
    }
  }

  let order = compile_query_order_v1(HashAlgorithm::Blake3_256, &[]).unwrap();
  let first = query_path_row("/same", 1);
  let second = query_path_row("/same", 2);
  assert_eq!(compare(&order, &first, &second), Ordering::Less);
  let mut later_revision = first.clone();
  later_revision.record_revision_tie = identity(250);
  assert_eq!(compare(&order, &first, &later_revision), Ordering::Less);
}

#[test]
fn deterministic_differential_order_and_windows_match_an_independent_model() {
  #[derive(Clone)]
  struct ModelRow {
    row: LogicalOrderRowOwnedV1,
    state_rank: u8,
    value: u64,
    path: String,
    key: u8,
  }

  let fields =
    [PositionOrderFieldV1 { field: "rank", direction: PositionSortDirectionV1::Descending, comparator: PositionComparatorV1::U64 }];
  let order = compile_query_order_v1(HashAlgorithm::Blake3_256, &fields).unwrap();
  let mut seed = 0x6a09_e667_f3bc_c909u64;
  let mut model = Vec::new();
  for index in 0..200u16 {
    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
    let value = seed >> 17;
    let state_rank = match seed % 7 {
      0 => 1,
      1 => 2,
      _ => 0,
    };
    let component = match state_rank {
      0 => present(PositionComparatorV1::U64, value.to_le_bytes()),
      1 => LogicalOrderComponentOwnedV1::typed_null(),
      _ => LogicalOrderComponentOwnedV1::missing(),
    };
    let path = format!("/model/{:03}-{:016x}", 199 - index, seed.rotate_left(11));
    let key = u8::try_from(index + 1).unwrap();
    model.push(ModelRow {
      row: row(PositionRouteV1::Query, vec![component, present(PositionComparatorV1::Utf8Binary, path.as_bytes())], key),
      state_rank,
      value,
      path,
      key,
    });
  }
  model.sort_by(|left, right| {
    left
      .state_rank
      .cmp(&right.state_rank)
      .then_with(|| if left.state_rank == 0 { right.value.cmp(&left.value) } else { Ordering::Equal })
      .then_with(|| left.path.cmp(&right.path))
      .then_with(|| left.key.cmp(&right.key))
  });
  let expected: Vec<u8> = model.iter().map(|value| value.key).collect();
  let mut production: Vec<_> = model.iter().map(|value| value.row.clone()).collect();
  production.reverse();
  production.sort_by(|left, right| compare(&order, left, right));
  assert_eq!(production.iter().map(|value| value.file_key_tie[0]).collect::<Vec<_>>(), expected);

  let limits = PositionWindowLimitsV1::new(11, 50, 256 * 1024).unwrap();
  let never_cancelled = || false;
  for offset in [0, 1, 37, 188, 200, 250] {
    let plan =
      plan_position_window_v1(PositionPaginationInputsV1 { offset: Some(offset), limit: Some(11), ..Default::default() }, limits).unwrap();
    let mut source = ModelSource::new(production.clone());
    let page = execute_position_window_v1(&order, plan, None, &mut source, &never_cancelled).unwrap();
    let start = usize::try_from(offset).unwrap().min(expected.len());
    let end = (start + 11).min(expected.len());
    assert_eq!(row_keys(&page), expected[start..end]);
    assert_eq!(page.has_more, end < expected.len());
  }
  for bound_index in [0usize, 1, 19, 100, 199] {
    let plan = plan_position_window_v1(PositionPaginationInputsV1 { before: true, limit: Some(11), ..Default::default() }, limits).unwrap();
    let mut source = ModelSource::new(production.clone());
    let page =
      execute_position_window_v1(&order, plan, Some(production[bound_index].as_borrowed()), &mut source, &never_cancelled).unwrap();
    let start = bound_index.saturating_sub(11);
    assert_eq!(row_keys(&page), expected[start..bound_index]);
    assert_eq!(page.has_more, start > 0);
  }
}

#[test]
fn logical_order_accepts_both_frozen_identity_widths() {
  let order = compile_query_order_v1(HashAlgorithm::Sha512, &[]).unwrap();
  let first = LogicalOrderRowOwnedV1 {
    route: PositionRouteV1::Query,
    components: vec![present(PositionComparatorV1::Utf8Binary, b"/a")],
    file_key_tie: vec![1; 64],
    record_revision_tie: vec![2; 64],
  };
  let second = LogicalOrderRowOwnedV1 {
    route: PositionRouteV1::Query,
    components: vec![present(PositionComparatorV1::Utf8Binary, b"/b")],
    file_key_tie: vec![3; 64],
    record_revision_tie: vec![4; 64],
  };
  assert_eq!(compare_logical_order_rows_v1(&order, first.as_borrowed(), second.as_borrowed()).unwrap(), Ordering::Less);
}

#[test]
fn window_planner_accepts_only_one_origin_and_checks_every_bound() {
  let limits = PositionWindowLimitsV1::new(20, 100, 8 * 1024 * 1024).unwrap();
  let start = plan_position_window_v1(PositionPaginationInputsV1::default(), limits).unwrap();
  assert_eq!(start.origin, PositionWindowOriginV1::Start);
  assert_eq!(start.limit, 20);

  let page = plan_position_window_v1(PositionPaginationInputsV1 { page: Some(3), limit: Some(10), ..Default::default() }, limits).unwrap();
  assert_eq!(page.origin, PositionWindowOriginV1::AbsoluteRank(20));
  let offset =
    plan_position_window_v1(PositionPaginationInputsV1 { offset: Some(7), limit: Some(10), ..Default::default() }, limits).unwrap();
  assert_eq!(offset.origin, PositionWindowOriginV1::AbsoluteRank(7));
  assert_eq!(
    plan_position_window_v1(PositionPaginationInputsV1 { after: true, ..Default::default() }, limits).unwrap().origin,
    PositionWindowOriginV1::After
  );
  assert_eq!(
    plan_position_window_v1(PositionPaginationInputsV1 { before: true, ..Default::default() }, limits).unwrap().origin,
    PositionWindowOriginV1::Before
  );

  for invalid in [
    PositionPaginationInputsV1 { page: Some(0), ..Default::default() },
    PositionPaginationInputsV1 { limit: Some(0), ..Default::default() },
    PositionPaginationInputsV1 { limit: Some(101), ..Default::default() },
    PositionPaginationInputsV1 { page: Some(2), offset: Some(1), ..Default::default() },
    PositionPaginationInputsV1 { after: true, before: true, ..Default::default() },
    PositionPaginationInputsV1 { offset: Some(1), after: true, ..Default::default() },
    PositionPaginationInputsV1 { page: Some(u64::MAX), limit: Some(100), ..Default::default() },
  ] {
    assert_eq!(plan_position_window_v1(invalid, limits).unwrap_err().class(), PositionOrderErrorClassV1::InvalidRequest);
  }
  assert!(PositionWindowLimitsV1::new(0, 10, 1024).is_err());
  assert!(PositionWindowLimitsV1::new(20, 10, 1024).is_err());
  assert!(PositionWindowLimitsV1::new(1, 10, 0).is_err());
}

struct ModelSource {
  rows: Vec<LogicalOrderRowOwnedV1>,
  force_receipt_count: Option<u64>,
  force_exhausted: Option<bool>,
  duplicate_first: bool,
}

impl ModelSource {
  fn new(rows: Vec<LogicalOrderRowOwnedV1>) -> Self {
    Self { rows, force_receipt_count: None, force_exhausted: None, duplicate_first: false }
  }

  fn find(&self, bound: LogicalOrderRowV1<'_>) -> usize {
    self
      .rows
      .iter()
      .position(|candidate| candidate.file_key_tie == bound.file_key_tie && candidate.record_revision_tie == bound.record_revision_tie)
      .expect("model bound")
  }
}

impl PositionWindowSourceV1 for ModelSource {
  fn scan(
    &mut self,
    request: PositionWindowScanRequestV1<'_>,
    visitor: &mut dyn PositionWindowVisitorV1,
  ) -> Result<PositionWindowScanReceiptV1, aeordb::engine::v4::position_order::PositionOrderErrorV1> {
    let (start, direction) = match request.seek {
      PositionWindowSeekV1::First => (0usize, PositionWindowDirectionV1::Forward),
      PositionWindowSeekV1::AbsoluteRank(rank) => (usize::try_from(rank).unwrap_or(usize::MAX), PositionWindowDirectionV1::Forward),
      PositionWindowSeekV1::After(bound) => (self.find(bound) + 1, PositionWindowDirectionV1::Forward),
      PositionWindowSeekV1::Before(bound) => (self.find(bound), PositionWindowDirectionV1::Reverse),
    };
    assert_eq!(request.direction, direction);
    let mut emitted = 0u64;
    let maximum = usize::try_from(request.maximum_rows).unwrap();
    match direction {
      PositionWindowDirectionV1::Forward => {
        for candidate in self.rows.iter().skip(start).take(maximum) {
          visitor.visit(candidate.as_borrowed())?;
          emitted += 1;
          if self.duplicate_first && emitted == 1 {
            visitor.visit(candidate.as_borrowed())?;
            emitted += 1;
          }
        }
      }
      PositionWindowDirectionV1::Reverse => {
        for candidate in self.rows[..start].iter().rev().take(maximum) {
          visitor.visit(candidate.as_borrowed())?;
          emitted += 1;
        }
      }
    }
    let exhausted = match direction {
      PositionWindowDirectionV1::Forward => start.saturating_add(usize::try_from(emitted).unwrap()) >= self.rows.len(),
      PositionWindowDirectionV1::Reverse => usize::try_from(emitted).unwrap() >= start,
    };
    Ok(PositionWindowScanReceiptV1 {
      emitted_rows: self.force_receipt_count.unwrap_or(emitted),
      exhausted: self.force_exhausted.unwrap_or(exhausted),
    })
  }
}

fn row_keys(page: &aeordb::engine::v4::position_order::PositionWindowPageV1) -> Vec<u8> {
  page.rows.iter().map(|value| value.file_key_tie[0]).collect()
}

#[test]
fn bounded_windows_seek_by_rank_or_bound_and_normalize_before_pages() {
  let order = compile_query_order_v1(HashAlgorithm::Blake3_256, &[]).unwrap();
  let rows: Vec<_> = (0..8).map(|index| query_path_row(&format!("/{index:02}"), index + 1)).collect();
  let limits = PositionWindowLimitsV1::new(2, 10, 256 * 1024).unwrap();
  let never_cancelled = || false;

  let offset_plan =
    plan_position_window_v1(PositionPaginationInputsV1 { offset: Some(3), limit: Some(2), ..Default::default() }, limits).unwrap();
  let mut source = ModelSource::new(rows.clone());
  let page = execute_position_window_v1(&order, offset_plan, None, &mut source, &never_cancelled).unwrap();
  assert_eq!(row_keys(&page), [4, 5]);
  assert!(page.has_more);
  assert_eq!(page.source_emitted_rows, 3);

  let after_plan =
    plan_position_window_v1(PositionPaginationInputsV1 { after: true, limit: Some(2), ..Default::default() }, limits).unwrap();
  let mut source = ModelSource::new(rows.clone());
  let page = execute_position_window_v1(&order, after_plan, Some(rows[1].as_borrowed()), &mut source, &never_cancelled).unwrap();
  assert_eq!(row_keys(&page), [3, 4]);
  assert!(page.has_more);

  let before_plan =
    plan_position_window_v1(PositionPaginationInputsV1 { before: true, limit: Some(2), ..Default::default() }, limits).unwrap();
  let mut source = ModelSource::new(rows.clone());
  let page = execute_position_window_v1(&order, before_plan, Some(rows[5].as_borrowed()), &mut source, &never_cancelled).unwrap();
  assert_eq!(row_keys(&page), [4, 5]);
  assert!(page.has_more);

  let mut source = ModelSource::new(rows.clone());
  let first_before = execute_position_window_v1(&order, before_plan, Some(rows[1].as_borrowed()), &mut source, &never_cancelled).unwrap();
  assert_eq!(row_keys(&first_before), [1]);
  assert!(!first_before.has_more);

  let end_plan =
    plan_position_window_v1(PositionPaginationInputsV1 { offset: Some(100), limit: Some(2), ..Default::default() }, limits).unwrap();
  let mut source = ModelSource::new(rows);
  let empty = execute_position_window_v1(&order, end_plan, None, &mut source, &never_cancelled).unwrap();
  assert!(empty.rows.is_empty());
  assert!(!empty.has_more);
}

#[test]
fn window_execution_fails_closed_on_bound_drift_source_lies_pressure_and_cancellation() {
  let order = compile_query_order_v1(HashAlgorithm::Blake3_256, &[]).unwrap();
  let rows: Vec<_> = (0..4).map(|index| query_path_row(&format!("/{index:02}"), index + 1)).collect();
  let limits = PositionWindowLimitsV1::new(2, 10, 256 * 1024).unwrap();
  let after_plan = plan_position_window_v1(PositionPaginationInputsV1 { after: true, ..Default::default() }, limits).unwrap();
  let start_plan = plan_position_window_v1(PositionPaginationInputsV1::default(), limits).unwrap();
  let never_cancelled = || false;

  let mut source = ModelSource::new(rows.clone());
  assert_eq!(
    execute_position_window_v1(&order, after_plan, None, &mut source, &never_cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::InvalidRequest
  );
  let mut source = ModelSource::new(rows.clone());
  assert_eq!(
    execute_position_window_v1(&order, start_plan, Some(rows[0].as_borrowed()), &mut source, &never_cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::InvalidRequest
  );

  let mut duplicate = ModelSource::new(rows.clone());
  duplicate.duplicate_first = true;
  assert_eq!(
    execute_position_window_v1(&order, start_plan, None, &mut duplicate, &never_cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );
  let mut count_lie = ModelSource::new(rows.clone());
  count_lie.force_receipt_count = Some(1);
  assert_eq!(
    execute_position_window_v1(&order, start_plan, None, &mut count_lie, &never_cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );
  let mut incomplete = ModelSource::new(rows[..1].to_vec());
  incomplete.force_exhausted = Some(false);
  assert_eq!(
    execute_position_window_v1(&order, start_plan, None, &mut incomplete, &never_cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );

  let tiny = PositionWindowLimitsV1::new(1, 2, 32).unwrap();
  let tiny_plan = plan_position_window_v1(PositionPaginationInputsV1::default(), tiny).unwrap();
  let mut source = ModelSource::new(vec![query_path_row(&"x".repeat(128), 1)]);
  assert_eq!(
    execute_position_window_v1(&order, tiny_plan, None, &mut source, &never_cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::ResourceLimit
  );

  let cancelled = || true;
  let mut source = ModelSource::new(rows.clone());
  assert_eq!(
    execute_position_window_v1(&order, start_plan, None, &mut source, &cancelled).unwrap_err().class(),
    PositionOrderErrorClassV1::Cancelled
  );

  let cancellation_checks = AtomicUsize::new(0);
  let cancelled_during_scan = || cancellation_checks.fetch_add(1, AtomicOrdering::Relaxed) >= 2;
  let mut source = ModelSource::new(rows);
  assert_eq!(
    execute_position_window_v1(&order, start_plan, None, &mut source, &cancelled_during_scan).unwrap_err().class(),
    PositionOrderErrorClassV1::Cancelled
  );
}

#[test]
fn malformed_rows_and_nonfinite_scores_are_never_comparable() {
  let order = compile_query_order_v1(HashAlgorithm::Blake3_256, &[]).unwrap();
  let valid = query_path_row("/a", 1);
  let mut wrong_route = valid.clone();
  wrong_route.route = PositionRouteV1::GlobalSearch;
  assert_eq!(
    compare_logical_order_rows_v1(&order, valid.as_borrowed(), wrong_route.as_borrowed()).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );
  let mut wrong_width = valid.clone();
  wrong_width.file_key_tie.pop();
  assert_eq!(
    compare_logical_order_rows_v1(&order, valid.as_borrowed(), wrong_width.as_borrowed()).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );
  let malformed = row(PositionRouteV1::Query, vec![present(PositionComparatorV1::U64, 1u64.to_le_bytes())], 2);
  assert_eq!(
    compare_logical_order_rows_v1(&order, valid.as_borrowed(), malformed.as_borrowed()).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );

  let search = compile_global_search_order_v1(HashAlgorithm::Blake3_256).unwrap();
  let nan = row(
    PositionRouteV1::GlobalSearch,
    vec![present(PositionComparatorV1::FiniteF64, f64::NAN.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/nan")],
    3,
  );
  let finite = row(
    PositionRouteV1::GlobalSearch,
    vec![present(PositionComparatorV1::FiniteF64, 1.0f64.to_le_bytes()), present(PositionComparatorV1::Utf8Binary, b"/finite")],
    4,
  );
  assert_eq!(
    compare_logical_order_rows_v1(&search, nan.as_borrowed(), finite.as_borrowed()).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );

  let directory =
    compile_directory_listing_order_v1(HashAlgorithm::Blake3_256, DirectoryOrderFieldV1::Name, PositionSortDirectionV1::Ascending).unwrap();
  let bad_category = row(
    PositionRouteV1::DirectoryListing,
    vec![
      present(PositionComparatorV1::U64, 2u64.to_le_bytes()),
      present(PositionComparatorV1::Utf8Binary, b"a"),
      present(PositionComparatorV1::Utf8Binary, b"a"),
      present(PositionComparatorV1::Utf8Binary, b"/a"),
    ],
    5,
  );
  assert_eq!(
    compare_logical_order_rows_v1(&directory, bad_category.as_borrowed(), bad_category.as_borrowed()).unwrap_err().class(),
    PositionOrderErrorClassV1::Corrupt
  );

  for malformed_component in [present(PositionComparatorV1::Utf8Binary, vec![0xff]), present(PositionComparatorV1::U64, 1u64.to_le_bytes())]
  {
    let malformed = row(PositionRouteV1::Query, vec![malformed_component], 6);
    assert_eq!(
      compare_logical_order_rows_v1(&order, malformed.as_borrowed(), valid.as_borrowed()).unwrap_err().class(),
      PositionOrderErrorClassV1::Corrupt
    );
  }
}

#[test]
fn position_order_has_one_storage_neutral_bounded_execution_path() {
  let package = Path::new(env!("CARGO_MANIFEST_DIR"));
  let production = fs::read_to_string(package.join("src/engine/v4/position_order.rs")).unwrap();
  let module_registry = fs::read_to_string(package.join("src/engine/v4/mod.rs")).unwrap();

  assert_eq!(production.matches("pub fn execute_position_window_v1(").count(), 1);
  assert_eq!(production.matches("pub fn compare_logical_order_rows_v1(").count(), 1);
  assert_eq!(module_registry.matches("pub mod position_order;").count(), 1);
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "crate::server",
    "server::",
    "tokio::spawn",
    "thread::spawn",
    ".sort_by(",
    ".sort_unstable",
    "read_to_end",
  ] {
    assert!(!production.contains(forbidden), "storage-neutral position-order module contains forbidden {forbidden:?}");
  }
}
