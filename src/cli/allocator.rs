//! The allocator the decompiler works in, one per worker thread.
//!
//! Decompiling a chunk builds a lot of small nodes that are all released in one
//! go, which is what a bump allocator is for. The allocator is a thread local
//! rather than a value passed around, so that a worker thread can keep one for
//! its whole life and hand it back empty after every dump.
//!
//! Two properties of `#[thread_local]` are what make this work:
//!
//! * A thread local static does not have to be `Sync`, so the cell can be a
//!   plain `RefCell` instead of a lock.
//! * The initialiser only has to be a constant expression. `LazyCell::new` is a
//!   `const fn` and a closure that captures nothing is a function pointer, so
//!   the cell can be built in a static without any `unsafe` code.

use std::cell::{LazyCell, RefCell};

use oxc_allocator::Allocator;

/// The arena-based allocator of the current thread, filled in on first use.
#[thread_local]
static ALLOCATOR: LazyCell<RefCell<Allocator>> = LazyCell::new(|| RefCell::new(Allocator::new()));

/// Runs `f` with the allocator of the current thread, then empties it.
///
/// The `'static` bound on the return type is what keeps the allocator safe to
/// reuse: a value that borrows from it cannot be returned, so everything `f`
/// allocated is either gone or copied out by the time the allocator is reset.
pub fn with_allocator<T: 'static>(f: impl FnOnce(&Allocator) -> T) -> T {
    let mut alloc = ALLOCATOR.borrow_mut();
    let result = f(&alloc);
    alloc.reset();
    result
}
