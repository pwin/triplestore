//! A global allocator that counts what it hands out.
//!
//! The whole reason this file exists is in `holos_engine::memory`: a query that asks for
//! more memory than the machine has does not fail the request, it aborts the process, and
//! on a server that takes down every other client's work along with it. Stopping that needs
//! the query cancelled *before* the allocation, which needs someone counting.
//!
//! # Why it is here and not in a library
//!
//! Every library crate in this workspace carries `#![forbid(unsafe_code)]`. A `GlobalAlloc`
//! implementation cannot be written under that lint, and relaxing it in a library would
//! trade a workspace-wide guarantee for a convenience. So the arithmetic, the ceiling and
//! the error live in `holos_engine::memory`, which stays safe, and the twenty lines that
//! must be `unsafe` live here — in the binary, in one file, where a reviewer can read all
//! of them at once.
//!
//! # What it costs
//!
//! One relaxed atomic read-modify-write per allocation and one per free. Nothing is
//! ordered by the counter — a watchdog samples it every 50 ms — so `Relaxed` is the correct
//! ordering rather than a shortcut, and the operation is an uncontended `lock xadd` on the
//! common path.

use holos_engine::memory;
use std::alloc::{GlobalAlloc, Layout};

/// Wraps another allocator and reports every block to `holos_engine::memory`.
///
/// Wrapping rather than implementing: the allocation strategy is not the interesting part,
/// and delegating to the platform's own keeps this file to bookkeeping.
pub struct Counting<A>(pub A);

// SAFETY: every method delegates to `self.0` and returns exactly what it returns, so the
// memory-safety contract is discharged by the inner allocator, which satisfies it by its
// own `unsafe impl`. The only added work is arithmetic on an atomic in `memory`, which
// allocates nothing — so it cannot re-enter this allocator — and cannot unwind. Sizes
// reported are `layout.size()`, the same size the caller will present on free, so the
// counter balances.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged to an allocator that accepts it.
        let pointer = unsafe { self.0.alloc(layout) };
        // A null return means the allocation failed and no bytes were handed out. Counting
        // it would leave the counter permanently high and eventually refuse queries that
        // were never given anything.
        if !pointer.is_null() {
            memory::record_alloc(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let pointer = unsafe { self.0.alloc_zeroed(layout) };
        if !pointer.is_null() {
            memory::record_alloc(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `pointer` came from this allocator with `layout`,
        // which is exactly what the inner allocator requires.
        unsafe { self.0.dealloc(pointer, layout) };
        memory::record_dealloc(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `dealloc`, plus `new_size` is the caller's, which the inner allocator
        // validates on its own terms.
        let moved = unsafe { self.0.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            // A failed `realloc` leaves the original block live and unchanged, so there is
            // nothing to record. A successful one changed the size by a difference; adding
            // `new_size` outright would count every `Vec` at every size it ever passed
            // through, and the counter would climb forever.
            memory::record_realloc(layout.size(), new_size);
        }
        moved
    }
}
