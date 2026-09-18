//! Current-thread allocation measurements for explicitly scoped unit benchmarks.
//! This module and its allocator wrapper are absent from shipped binaries.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Measurements {
    pub(crate) allocation_calls: usize,
    pub(crate) retained_bytes: usize,
    pub(crate) peak_bytes: usize,
}

thread_local! {
    static ACTIVE: Cell<Option<Measurements>> = const { Cell::new(None) };
}

fn record(allocated: usize, released: usize, allocation_call: bool) {
    let _ = ACTIVE.try_with(|state| {
        if let Some(mut value) = state.get() {
            value.allocation_calls += usize::from(allocation_call);
            value.retained_bytes = value.retained_bytes.saturating_sub(released) + allocated;
            value.peak_bytes = value.peak_bytes.max(value.retained_bytes);
            state.set(Some(value));
        }
    });
}

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// All operations forward the caller's original layout/pointer to System;
// accounting neither allocates memory nor changes ownership.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record(layout.size(), 0, true);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record(layout.size(), 0, true);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(0, layout.size(), false);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(ptr, layout, new_size) };
        if !next.is_null() {
            record(new_size, layout.size(), true);
        }
        next
    }
}

/// Keep the returned value alive until measurements are captured. Only memory
/// allocated and released inside this closure on this thread is accounted for.
pub(crate) fn measure<T>(run: impl FnOnce() -> T) -> (T, Measurements) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE.with(|state| state.set(None));
        }
    }
    ACTIVE.with(|state| {
        assert!(state.get().is_none(), "allocation measurements cannot nest");
        state.set(Some(Measurements::default()));
    });
    let reset = Reset;
    let value = run();
    let measurements = ACTIVE.with(|state| state.get().unwrap());
    drop(reset);
    (value, measurements)
}
