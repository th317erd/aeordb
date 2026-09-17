//! Thread-local allocation measurements shared by independent test harnesses.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Allocations {
  pub(super) total: usize,
  pub(super) maximum: usize,
  pub(super) matching_requests: usize,
  pub(super) injected_failure: bool,
}

thread_local! {
  static ENABLED: Cell<bool> = const { Cell::new(false) };
  pub(super) static FAIL_SIZE: Cell<usize> = const { Cell::new(0) };
  pub(super) static FAIL_OCCURRENCE: Cell<usize> = const { Cell::new(1) };
  static ALLOCATIONS: Cell<Allocations> = const { Cell::new(Allocations { total: 0, maximum: 0, matching_requests: 0, injected_failure: false }) };
}

struct WriterAllocator;

#[global_allocator]
static ALLOCATOR: WriterAllocator = WriterAllocator;

fn should_fail(size: usize) -> bool {
  if !ENABLED.try_with(Cell::get).unwrap_or(false) {
    return false;
  }
  let matches_size = FAIL_SIZE.with(|target| target.get() != 0 && target.get() == size);
  let fail = matches_size
    && FAIL_OCCURRENCE.with(|remaining| {
      let occurrence = remaining.get();
      remaining.set(occurrence.saturating_sub(1));
      occurrence == 1
    });
  if fail {
    FAIL_SIZE.with(|target| target.set(0));
  }
  ALLOCATIONS.with(|value| {
    let mut measured = value.get();
    measured.total = measured.total.saturating_add(size);
    measured.maximum = measured.maximum.max(size);
    measured.matching_requests += usize::from(matches_size);
    measured.injected_failure |= fail;
    value.set(measured);
  });
  fail
}

unsafe impl GlobalAlloc for WriterAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if should_fail(layout.size()) {
      std::ptr::null_mut()
    } else {
      unsafe { System.alloc(layout) }
    }
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    if should_fail(layout.size()) {
      std::ptr::null_mut()
    } else {
      unsafe { System.alloc_zeroed(layout) }
    }
  }

  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    if should_fail(size) {
      std::ptr::null_mut()
    } else {
      unsafe { System.realloc(pointer, layout, size) }
    }
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    unsafe { System.dealloc(pointer, layout) };
  }
}

struct Measurement;

impl Drop for Measurement {
  fn drop(&mut self) {
    ENABLED.with(|enabled| enabled.set(false));
    FAIL_SIZE.with(|target| target.set(0));
  }
}

pub(super) fn measure<T>(fail_size: usize, action: impl FnOnce() -> T) -> (T, Allocations) {
  measure_nth(fail_size, 1, action)
}

pub(super) fn measure_nth<T>(fail_size: usize, occurrence: usize, action: impl FnOnce() -> T) -> (T, Allocations) {
  assert!(occurrence > 0);
  ALLOCATIONS.with(|measured| measured.set(Allocations::default()));
  FAIL_SIZE.with(|target| target.set(fail_size));
  FAIL_OCCURRENCE.with(|remaining| remaining.set(occurrence));
  ENABLED.with(|enabled| enabled.set(true));
  let guard = Measurement;
  let result = action();
  let allocations = ALLOCATIONS.with(Cell::get);
  drop(guard);
  (result, allocations)
}
