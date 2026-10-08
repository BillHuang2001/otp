// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Atomic reference counts with `erts_refc_*` semantics: [`RefCount`].
//!
//! This is the shared refcount primitive of the workspace. It lives here, in the
//! *lowest common ancestor* allocator crate, because every refcounted off-heap
//! object (`beam_alloc::binary::RefcBinary` today, the `beam_gc` MSO payload
//! container, and the `beam_codec`/`beam_term` binary views) needs exactly the same
//! primitive — and the crate-layer contract forbids duplicating a type across crates.
//!
//! Mutation ordering mirrors the C `erts_refc_*` macros: `inc`/`dec` are `AcqRel`
//! (an `Acquire` of the object's state plus a `Release` of the previous owner's
//! work) and `count` reads with `Acquire`, so taking a reference happens-before the
//! payload is observed, and the final `dec_and_test(0)` observes every write any
//! earlier owner made before its own reference disappeared.
//!
//! This module is **safe Rust** (the only interior mutability is the `AtomicUsize`).

use core::sync::atomic::{AtomicUsize, Ordering};

/// An atomic reference count with `erts_refc_*` semantics.
///
/// `RefCount` is both [`Send`] and [`Sync`] (it wraps an `AtomicUsize`), so it may be
/// embedded in an object shared across threads without further synchronization: the
/// atomic itself is the synchronization point.
#[repr(transparent)]
pub struct RefCount {
    count: AtomicUsize,
}

impl RefCount {
    /// Creates a reference count with the initial value `initial`.
    pub const fn new(initial: usize) -> RefCount {
        RefCount {
            count: AtomicUsize::new(initial),
        }
    }

    /// The current count (`Acquire`).
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Atomically increments the count, returning the new value (`AcqRel`).
    pub fn inc(&self) -> usize {
        self.count.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Atomically decrements the count, returning the new value (`AcqRel`).
    pub fn dec(&self) -> usize {
        self.count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    /// Atomically decrements the count and reports whether the new value is at most
    /// `min`.
    ///
    /// This is `erts_refc_dectest`: the caller that sees `true` is the one that must
    /// destroy the object. With `min == 0` that is exactly the last reference.
    ///
    /// On the *underflow* path (a spuriously over-decremented count) `fetch_sub`
    /// wraps, so the returned `new` value is huge and `new <= min` is `false` — the
    /// object is simply not destroyed, which is the safe (non-freeing) outcome.
    pub fn dec_and_test(&self, min: usize) -> bool {
        let new = self.count.fetch_sub(1, Ordering::AcqRel) - 1;
        new <= min
    }
}

// A nicer default than nothing when a `RefCount` shows up in a `{:?}` (test failure
// messages, logs): print the count, not the opaque atomic.
impl core::fmt::Debug for RefCount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RefCount").field("count", &self.count()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn new_starts_at_the_requested_value() {
        assert_eq!(RefCount::new(0).count(), 0);
        assert_eq!(RefCount::new(1).count(), 1);
        assert_eq!(RefCount::new(42).count(), 42);
    }

    #[test]
    fn inc_and_dec_return_the_new_value() {
        let refc = RefCount::new(5);
        assert_eq!(refc.inc(), 6);
        assert_eq!(refc.count(), 6);
        assert_eq!(refc.dec(), 5);
        assert_eq!(refc.count(), 5);
    }

    #[test]
    fn dec_and_test_reports_the_last_reference() {
        let refc = RefCount::new(1);
        assert!(refc.dec_and_test(0), "1 -> 0 is the last reference");
        assert_eq!(refc.count(), 0);
    }

    #[test]
    fn dec_and_test_holds_until_the_count_reaches_zero() {
        let refc = RefCount::new(2);
        assert!(!refc.dec_and_test(0), "2 -> 1 still has an owner");
        assert_eq!(refc.count(), 1);
        assert!(refc.dec_and_test(0), "1 -> 0 is the last reference");
        assert_eq!(refc.count(), 0);
    }

    #[test]
    fn dec_and_test_honours_the_min_threshold() {
        // `erts_refc_dectest` with a non-zero `min` destroys when the count falls to
        // (or below) `min`, not only at zero.
        let refc = RefCount::new(4);
        assert!(!refc.dec_and_test(2), "4 -> 3 is above min 2");
        assert!(refc.dec_and_test(2), "3 -> 2 is at most min 2");
        assert_eq!(refc.count(), 2);
    }

    #[test]
    fn concurrent_inc_and_dec_pairs_leave_the_count_unchanged() {
        const THREADS: usize = 8;
        const ITERS: usize = 20_000;
        let refc = Arc::new(RefCount::new(1));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let refc = Arc::clone(&refc);
                thread::spawn(move || {
                    for _ in 0..ITERS {
                        refc.inc();
                        refc.dec();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread must not panic");
        }
        assert_eq!(refc.count(), 1, "balanced inc/dec must net to zero");
    }

    #[test]
    fn exactly_one_concurrent_dec_and_test_observes_the_last_reference() {
        const THREADS: usize = 8;
        // The count starts at THREADS: each worker owns exactly one reference, so
        // exactly one of the racing `dec_and_test(0)` calls may report `true`.
        let refc = Arc::new(RefCount::new(THREADS));
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let refc = Arc::clone(&refc);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    refc.dec_and_test(0)
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker thread must not panic"))
            .filter(|&destroy| destroy)
            .count();
        assert_eq!(winners, 1, "exactly one owner destroys the object");
        assert_eq!(refc.count(), 0);
    }
}
