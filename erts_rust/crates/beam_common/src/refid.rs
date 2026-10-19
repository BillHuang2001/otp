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

//! The **one local reference-id space** of the Rust runtime.
//!
//! [`next_ref_id`] is a process-wide monotonic id source. Every reference *term* minted locally
//! draws its `id` word from it:
//!
//! * `erlang:make_ref/0`;
//! * the NIF `make_ref`;
//! * `erlang:monitor/2,3` monitor references and `erlang:alias/1` aliases;
//! * timer references;
//! * `seq_trace` references;
//! * **NIF resource terms** (`enif_make_resource`).
//!
//! # Why a single, collision-free space
//!
//! A reference term in this tree is the on-heap block
//!
//! ```text
//! [make_header(3, REF_SUBTAG), node = small(0), serial = small(0), id = small(N)]
//! ```
//!
//! so a reference's **identity is the `id` word alone** — `node` and `serial` are constant in
//! Milestone 1 (one node, one local space). Two independent id spaces that overlap therefore make
//! a **dead** NIF resource term indistinguishable, at word level, from a *live* plain reference:
//! a reachability predicate that matches reference *identity* (the runtime prunes unreferenced
//! resource terms at the runner's park arm) then answers "reached" for the dead resource and
//! retains it — a leak; worse, a monitor/liveness check can treat a dead resource's monitor
//! reference as live. One shared monotonic source removes the overlap by construction: two
//! producers can never hand out the same `id`.
//!
//! # Why it lives here
//!
//! `beam_common` is the workspace's lowest common ancestor, and that is exactly what this id
//! space needs: `beam_abi` (wave 0, which mints resource terms) cannot host a type that
//! `beam_process` (wave 3, which mints monitor/alias references) draws from, and `beam_process`
//! must not depend on the NIF ABI crate. No crate may keep a private reference-id counter;
//! every local reference term's `id` comes from [`next_ref_id`].
//!
//! # Range: the small-int encoding is the caller's check
//!
//! This module is term-independent (`beam_common` names no term type and must not depend on
//! `beam_term`), so it deliberately does **not** bound the counter to the small-integer range. A
//! caller that writes an id into a term via `make_small` must range-check the small-int encoding
//! itself: `beam_process::MonitorRef::to_eterm` and `beam_abi`'s checked resource mint refuse an
//! id past `MAX_SMALL` (`2^59 - 1`) with `BeamError::SystemLimit` instead of truncating,
//! substituting or wrapping it into a *different* reference. Reaching that bound is unreachable in
//! practice — it would take about `5.8e17` (2^59) draws in one process — and it lies far below the
//! `2^64` wrap point of the counter.
//!
//! # Caveat: ids from another node are not from this space
//!
//! An id decoded from another node (ETF via `beam_codec`) is not drawn from this space. Real ERTS
//! separates remote ids from local ones with the reference's `node`/`serial` words, which
//! Milestone 1 keeps at `0`; the uniqueness guarantee stated here is therefore over **locally
//! minted** ids only.

use std::sync::atomic::{AtomicU64, Ordering};

/// The next id of the process-wide local reference-id space (see the [module docs](self)).
///
/// Backed by one private `static NEXT: AtomicU64` (relaxed `fetch_add`), so the **first** value is
/// `0` and every subsequent draw is strictly greater than all earlier ones — no reset, no
/// per-caller space. Concurrent draws never repeat an id: the union of all ids handed out in a
/// process is distinct.
///
/// The counter is **total**: it never panics. The underlying `AtomicU64::fetch_add` wraps at the
/// extreme, which keeps the counter total rather than aborting; that wrap is not reachable in
/// practice (2^64 draws) and, as the [module docs](self) record, it lies far beyond the `2^59`
/// small-integer bound a term-writing caller must enforce anyway.
///
/// # Example
///
/// ```
/// let a = beam_common::next_ref_id();
/// let b = beam_common::next_ref_id();
/// assert!(b > a, "successive draws strictly increase");
/// ```
#[must_use]
pub fn next_ref_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the shared reference-id space: monotonicity, uniqueness under concurrency
    //! and the absence of any per-caller space. The tests serialize on [`TEST_LOCK`] because they
    //! all draw from the one process-wide counter and assert exact draw counts.
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes the tests of this module, so the shared counter's draw count is deterministic
    /// even though `cargo test` runs the crate's tests on parallel threads.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Takes [`TEST_LOCK`], recovering from a poisoned mutex (a failed sibling test must not turn
    /// the remaining tests into panics of their own).
    fn serialize() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn successive_draws_strictly_increase() {
        let _guard = serialize();
        let mut previous = next_ref_id();
        for _ in 0..64 {
            let current = next_ref_id();
            assert!(
                current > previous,
                "draws must strictly increase: {current} came after {previous}"
            );
            previous = current;
        }
    }

    #[test]
    fn concurrent_draws_are_unique_and_advance_the_shared_counter_exactly() {
        const THREADS: usize = 4;
        const DRAWS: usize = 512;

        let _guard = serialize();
        let base = next_ref_id();
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            handles.push(std::thread::spawn(|| {
                (0..DRAWS).map(|_| next_ref_id()).collect::<Vec<u64>>()
            }));
        }
        let mut ids = Vec::with_capacity(THREADS * DRAWS);
        for handle in handles {
            ids.extend(handle.join().expect("worker thread must not panic"));
        }
        let after = next_ref_id();

        let total = THREADS * DRAWS;
        assert_eq!(ids.len(), total);
        // Every draw landed inside the window, and no id was handed out twice — not within one
        // thread, not across threads.
        for id in &ids {
            assert!(
                *id > base && *id < after,
                "id {id} outside ({base}, {after})"
            );
        }
        let distinct: HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            total,
            "no two concurrent draws may repeat an id"
        );
        // A single shared counter advancing by exactly `total` between the two bracketing draws:
        // the ids are consecutive, so the window is one id per draw.
        assert_eq!(
            after - base - 1,
            total as u64,
            "the shared counter advanced by exactly the draws"
        );
        let min = ids.iter().copied().min().expect("non-empty");
        let max = ids.iter().copied().max().expect("non-empty");
        assert_eq!(
            max - min + 1,
            total as u64,
            "one contiguous run of ids, one space"
        );
    }

    #[test]
    fn a_later_draw_exceeds_every_earlier_draw() {
        let _guard = serialize();
        let mut earlier = Vec::new();
        for _ in 0..16 {
            earlier.push(next_ref_id());
        }
        // No reset and no per-caller space: an interleaved caller's draws are still followed by
        // larger ones, because the counter only moves forward.
        let later = next_ref_id();
        for id in &earlier {
            assert!(
                later > *id,
                "later draw {later} must exceed earlier draw {id}"
            );
        }
        let mut from_another_caller = Vec::new();
        for _ in 0..16 {
            from_another_caller.push(next_ref_id());
        }
        let last = next_ref_id();
        for id in earlier.iter().chain(from_another_caller.iter()) {
            assert!(
                last > *id,
                "the space is shared, not per caller: {last} vs {id}"
            );
        }
    }
}
