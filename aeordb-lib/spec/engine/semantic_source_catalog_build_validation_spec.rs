//! Count/order/capacity refusal and final-callback admission boundaries.
use super::*;
use std::cell::Cell;
use aeordb::engine::memory_coordinator::HostMemorySample;

#[test]
fn catalog_build_refuses_invalid_input_and_count_without_a_completed_result() {
  let algorithm = HashAlgorithm::Blake3_256;
  for case in 0..14 {
    let memory = memory();
    let mut input = request(algorithm, 2);
    let mut rows = vec![row(0, algorithm), row(1, algorithm)];
    match case {
      0 => input.database_id = [0; 16],
      1 => input.expected_path_count = 0,
      2 => input.expected_path_count = u64::MAX,
      3 => input.expected_path_count = 1,
      4 => input.expected_path_count = 3,
      5 => rows.swap(0, 1),
      6 => rows[1].path = rows[0].path.clone(),
      7 => rows[0].path = "relative".into(),
      8 => rows[0].path = "/p/../q".into(),
      9 => rows[0].base_file_record_id = Some(vec![0; algorithm.hash_length()]),
      10 => rows[0].base_file_record_id = Some(vec![1; algorithm.hash_length() - 1]),
      11 => rows[0].requested_file_record_id = Some(vec![2; algorithm.hash_length() + 1]),
      12 => {
        let mut path = String::with_capacity(65_536);
        path.push_str(&rows[0].path);
        rows[0].path = path;
      }
      13 => {
        let mut identity = Vec::with_capacity(algorithm.hash_length() + 1);
        identity.resize(algorithm.hash_length(), 1);
        rows[0].base_file_record_id = Some(identity);
      }
      _ => unreachable!(),
    }
    let result = build_semantic_source_catalog_pair_v1(input, rows.into_iter().map(Ok), &mut |_, _| Ok(()), &memory, &|| false);
    assert!(result.is_err(), "case {case} was admitted");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn catalog_build_invalid_bounds_refuse_before_requesting_input_or_emitting() {
  for case in 0..5 {
    let algorithm = HashAlgorithm::Blake3_256;
    let memory = memory();
    let mut input = request(algorithm, 2);
    match case {
      0 => input.maximum_path_bytes = 0,
      1 => input.maximum_path_bytes = 65_536,
      2 => input.maximum_workspace_bytes = 0,
      3 => input.maximum_node_pairs = 0,
      4 => input.maximum_output_bytes = 0,
      _ => unreachable!(),
    }
    let result = build_semantic_source_catalog_pair_v1(
      input,
      std::iter::from_fn(|| -> Option<Result<SemanticSourceCatalogPairRowV1, SemanticCompilationErrorV1>> {
        panic!("invalid bounds requested input")
      }),
      &mut |_, _| panic!("invalid bounds emitted nodes"),
      &memory,
      &|| false,
    );
    assert!(result.is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn catalog_build_preserves_final_input_error_instead_of_reporting_completion() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let rows = (0..257).map(|index| Ok(row(index, algorithm))).chain(std::iter::once(Err(SemanticCompilationErrorV1::Operational {
    path: "test-input",
    message: "EOF could not be established".into(),
  })));
  let mut emissions = 0;
  let result = build_semantic_source_catalog_pair_v1(
    request(algorithm, 257),
    rows,
    &mut |_, _| {
      emissions += 1;
      Ok(())
    },
    &memory,
    &|| false,
  );
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Operational { path: "test-input", .. })));
  assert_eq!(emissions, 1);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn catalog_build_checks_cancellation_at_final_input_and_final_sink_callbacks() {
  for at_sink in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let memory = memory();
    let cancelled = Cell::new(false);
    let mut position = 0;
    let rows = std::iter::from_fn(|| {
      if position == 2 {
        if !at_sink {
          cancelled.set(true);
        }
        return None;
      }
      let value = row(position, algorithm);
      position += 1;
      Some(Ok(value))
    });
    let mut emissions = 0;
    let result = build_semantic_source_catalog_pair_v1(
      request(algorithm, 2),
      rows,
      &mut |_, _| {
        emissions += 1;
        if at_sink {
          cancelled.set(true);
        }
        Ok(())
      },
      &memory,
      &|| cancelled.get(),
    );
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Cancelled)));
    assert_eq!(emissions, usize::from(at_sink));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn catalog_build_final_sink_memory_pressure_refuses_and_next_build_retries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let result = build_semantic_source_catalog_pair_v1(
    request(algorithm, 2),
    (0..2).map(|index| Ok(row(index, algorithm))),
    &mut |_, _| {
      memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
      Ok(())
    },
    &memory,
    &|| false,
  );
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  memory.update_host_sample(HostMemorySample::default()).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let retry = build_semantic_source_catalog_pair_v1(
    request(algorithm, 2),
    (0..2).map(|index| Ok(row(index, algorithm))),
    &mut |_, _| Ok(()),
    &memory,
    &|| false,
  )
  .unwrap();
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn catalog_build_enforces_exact_node_and_output_limits_before_sink_calls() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory();
  let mut total = 0u64;
  let warm = build_semantic_source_catalog_pair_v1(
    request(algorithm, 513),
    (0..513).map(|index| Ok(row(index, algorithm))),
    &mut |base, requested| {
      total += (base.len() + requested.len()) as u64;
      Ok(())
    },
    &memory,
    &|| false,
  )
  .unwrap();
  let count = warm.node_count();
  assert_eq!(count, 5);
  drop(warm);
  for case in 0..3 {
    let mut input = request(algorithm, 513);
    input.maximum_output_bytes = total - u64::from(case == 1);
    input.maximum_node_pairs = count - u64::from(case == 2);
    let mut emitted_bytes = 0u64;
    let mut emitted_nodes = 0u64;
    let result = build_semantic_source_catalog_pair_v1(
      input,
      (0..513).map(|index| Ok(row(index, algorithm))),
      &mut |base, requested| {
        emitted_bytes += (base.len() + requested.len()) as u64;
        emitted_nodes += 1;
        Ok(())
      },
      &memory,
      &|| false,
    );
    assert_eq!(result.is_ok(), case == 0);
    assert!(emitted_bytes <= input.maximum_output_bytes);
    assert!(emitted_nodes <= input.maximum_node_pairs);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
