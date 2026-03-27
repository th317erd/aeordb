use aeordb::plugins::wasm_runtime::{WasmPluginRuntime, WasmRuntimeError};

/// A minimal valid WASM module (WAT format) that exports a `handle` function.
///
/// This function takes (request_ptr: i32, request_len: i32) -> i64 and returns
/// a packed pointer+length. For the simplest test, it just echoes the input
/// back: the response pointer = request_ptr, response length = request_len.
fn minimal_echo_wat() -> &'static str {
  r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      ;; Pack request_ptr into high 32 bits, request_len into low 32 bits
      ;; result = (i64(request_ptr) << 32) | i64(request_len)
      (i64.or
        (i64.shl
          (i64.extend_i32_u (local.get $request_ptr))
          (i64.const 32)
        )
        (i64.extend_i32_u (local.get $request_len))
      )
    )
  )
  "#
}

/// A WASM module that returns a response at a fixed offset.
/// It copies the input to offset 1024 and returns pointer=1024, length=request_len.
fn copy_response_wat() -> &'static str {
  r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      ;; Copy request bytes from request_ptr to offset 1024
      (memory.copy
        (i32.const 1024)
        (local.get $request_ptr)
        (local.get $request_len)
      )
      ;; Return pointer=1024, length=request_len
      (i64.or
        (i64.shl
          (i64.const 1024)
          (i64.const 32)
        )
        (i64.extend_i32_u (local.get $request_len))
      )
    )
  )
  "#
}

/// A WASM module that returns an empty response (length 0).
fn empty_response_wat() -> &'static str {
  r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      (i64.const 0)
    )
  )
  "#
}

/// A WASM module that traps unconditionally.
fn trapping_wat() -> &'static str {
  r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      (unreachable)
    )
  )
  "#
}

/// A WASM module that loops forever (burns fuel without producing a result).
fn infinite_loop_wat() -> &'static str {
  r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      (loop $infinite
        (br $infinite)
      )
      (i64.const 0)
    )
  )
  "#
}

/// Compile WAT text to WASM binary bytes using wasmi.
fn wat_to_wasm(wat: &str) -> Vec<u8> {
  wat::parse_str(wat).expect("WAT should be valid")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_load_valid_wasm_module() {
  let wasm_bytes = wat_to_wasm(minimal_echo_wat());
  let result = WasmPluginRuntime::new(&wasm_bytes);
  assert!(result.is_ok(), "should load a valid WASM module");
}

#[test]
fn test_load_invalid_wasm_rejected() {
  let garbage = vec![0x00, 0x61, 0x73, 0x6d, 0xFF, 0xFF, 0xFF, 0xFF];
  let result = WasmPluginRuntime::new(&garbage);
  assert!(result.is_err(), "should reject invalid WASM bytes");

  let error = result.unwrap_err();
  assert!(
    matches!(error, WasmRuntimeError::CompilationFailed(_)),
    "error should be CompilationFailed, got: {:?}",
    error
  );
}

#[test]
fn test_load_completely_empty_bytes_rejected() {
  let result = WasmPluginRuntime::new(&[]);
  assert!(result.is_err(), "should reject empty bytes");
}

#[test]
fn test_load_random_bytes_rejected() {
  let random_bytes = vec![42u8; 256];
  let result = WasmPluginRuntime::new(&random_bytes);
  assert!(result.is_err(), "should reject random bytes");
}

#[test]
fn test_call_exported_function() {
  let wasm_bytes = wat_to_wasm(minimal_echo_wat());
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let request = b"hello world";
  let response = runtime.call_handle(request).expect("handle should succeed");

  assert_eq!(
    response, request,
    "echo module should return the exact input bytes"
  );
}

#[test]
fn test_call_with_empty_request() {
  let wasm_bytes = wat_to_wasm(minimal_echo_wat());
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let response = runtime.call_handle(b"").expect("handle should succeed");
  assert!(
    response.is_empty(),
    "empty request should produce empty response"
  );
}

#[test]
fn test_call_copy_response_module() {
  let wasm_bytes = wat_to_wasm(copy_response_wat());
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let request = b"copied data";
  let response = runtime.call_handle(request).expect("handle should succeed");

  assert_eq!(
    response.as_slice(),
    request.as_slice(),
    "copy module should return a copy of the input"
  );
}

#[test]
fn test_call_empty_response_module() {
  let wasm_bytes = wat_to_wasm(empty_response_wat());
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let response = runtime
    .call_handle(b"something")
    .expect("handle should succeed");
  assert!(
    response.is_empty(),
    "empty-response module should return empty bytes"
  );
}

#[test]
fn test_memory_limit_enforced() {
  let wasm_bytes = wat_to_wasm(minimal_echo_wat());
  // Set a very tiny memory limit (1 page = 64KB).
  // Then try to write data larger than the memory.
  let runtime =
    WasmPluginRuntime::with_limits(&wasm_bytes, 64 * 1024, 1_000_000).expect("valid module");

  // 64KB of data should be at the boundary.
  let large_request = vec![0xABu8; 65 * 1024];
  let result = runtime.call_handle(&large_request);
  assert!(
    result.is_err(),
    "should fail when request exceeds memory limit"
  );
}

#[test]
fn test_fuel_limit_enforced() {
  let wasm_bytes = wat_to_wasm(infinite_loop_wat());
  // Very small fuel budget to ensure the loop gets cut off quickly.
  let runtime =
    WasmPluginRuntime::with_limits(&wasm_bytes, 16 * 1024 * 1024, 100).expect("valid module");

  let result = runtime.call_handle(b"go");
  assert!(
    result.is_err(),
    "should fail when fuel runs out during infinite loop"
  );

  match result.unwrap_err() {
    WasmRuntimeError::FuelLimitExceeded => {}
    WasmRuntimeError::Trap(message) => {
      // Some versions of wasmi report fuel exhaustion as a trap.
      assert!(
        message.to_lowercase().contains("fuel"),
        "trap message should mention fuel, got: {}",
        message
      );
    }
    other => panic!(
      "expected FuelLimitExceeded or fuel-related Trap, got: {:?}",
      other
    ),
  }
}

#[test]
fn test_wasm_trap_returns_clean_error() {
  let wasm_bytes = wat_to_wasm(trapping_wat());
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let result = runtime.call_handle(b"trigger trap");
  assert!(result.is_err(), "trapping module should return an error");

  let error = result.unwrap_err();
  match error {
    WasmRuntimeError::Trap(_) => {}
    other => panic!("expected Trap error, got: {:?}", other),
  }
}

#[test]
fn test_module_without_handle_export() {
  // A valid module that exports nothing useful.
  let wat = r#"
  (module
    (memory (export "memory") 1)
    (func (export "not_handle") (result i32)
      (i32.const 42)
    )
  )
  "#;
  let wasm_bytes = wat_to_wasm(wat);
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let result = runtime.call_handle(b"test");
  assert!(result.is_err(), "should fail if 'handle' export is missing");

  match result.unwrap_err() {
    WasmRuntimeError::ExportNotFound(_) => {}
    other => panic!("expected ExportNotFound, got: {:?}", other),
  }
}

#[test]
fn test_multiple_invocations_are_isolated() {
  let wasm_bytes = wat_to_wasm(minimal_echo_wat());
  let runtime = WasmPluginRuntime::new(&wasm_bytes).expect("valid module");

  let response_1 = runtime.call_handle(b"first").expect("first call");
  let response_2 = runtime.call_handle(b"second").expect("second call");

  assert_eq!(response_1, b"first");
  assert_eq!(response_2, b"second");
}
