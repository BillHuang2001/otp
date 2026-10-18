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

//! The **off-heap reclaim half** of a collection: which registered refc-binary payloads a
//! live heap still names, and the roster sweep that drops the rest.
//!
//! This is the Rust counterpart of the C `sweep_off_heap`
//! (`erts/emulator/beam/erl_gc.c:2996`): a collection walks the process's on-heap term graph
//! once, decides for every on-heap off-heap handle whether it survived, and then drops the
//! `MSO(p)` entries no survivor names. The copier itself never touches the payload — it copies
//! the small on-heap `[BinRef][ErlSubBits]` pair verbatim and leaves the `val` pointer
//! byte-identical (the `erl_gc.c` "sweep off heap" rule, and the reason
//! [`crate::mso::OffHeap`] exists at all). The *liveness test* therefore belongs to whoever
//! holds both the root set and the roster, i.e. here, next to the reachability walk it needs.
//!
//! Two pieces, split along that seam:
//!
//! - [`for_each_reachable_binary_payload`] — the **read-only** walk: every reachable refc
//!   binary's payload pointer, collected through the collector's safe whole-heap enumeration
//!   ([`AnyCollector::for_each_reachable_object_view`] over
//!   [`ReachableObject`](crate::ReachableObject)) and the **frozen** handle layout
//!   (`[HEADER_BIN_REF, val, next]`, `beam_term::refc`): a reachable `BinRef` is reported by
//!   the walk as a 3-word boxed object whose word 1 is the payload's **first data byte**, which
//!   is exactly the pointer value `beam_alloc::RefcBinary::data_ptr` hands out.
//! - [`sweep_unreachable_binaries`] — the **write** side: it feeds that set to
//!   [`OffHeap::sweep`](crate::mso::OffHeap::sweep), which releases the dead entries *in place*
//!   (slot-stable) and keeps every [`Mso`](crate::mso::OffHeapEntry::Mso) — an MSO is not
//!   referenced by a `BinRef`, so this seam never reclaims one.
//!
//! # Why this lives in `beam_gc` and not in its callers
//!
//! Every host that owns a process (an owning `beam_process::Process`, the runtime) needs this
//! exact test, and each host otherwise hand-holds its own copy — `beam_process/src/process.rs:176-199`
//! (`reachable_bin_ref_payload`) is the surviving private copy this module is the lowest
//! common ancestor of. Duplicating the *subtag test plus payload word index* in every host is
//! precisely the "no duplication across crates" hazard the crate-layer contracts forbid: a
//! wrong word index silently leaks every off-heap binary of the process, which is the very
//! retention bug these functions exist to remove.
//!
//! # Contracts honoured here
//!
//! - **Safe Rust.** The walk is driven entirely through the safe `ReachableObject` view; this
//!   module contains no `unsafe` and no raw-pointer arithmetic of its own.
//! - **Frozen layout only.** The `BinRef` header and its `val` word index are read from
//!   `beam_term` (`HEADER_BIN_REF`, word 1); no interior layout is re-derived here.
//! - **Errors propagate.** A walk that cannot complete (a malformed heap, an allocation
//!   failure) returns the typed [`beam_common::BeamError`] to the caller; it is never folded
//!   into "nothing is live", which would release payloads a live term still names.

use beam_common::Result;
use beam_term::prelude::{Eterm, HEADER_BIN_REF};

use crate::any_collector::AnyCollector;
use crate::mso::{OffHeap, OffHeapEntry};

/// Visits the payload first-data-byte pointer of every **reachable** off-heap refc binary.
///
/// The walk is the collector's own whole-heap reachability enumeration
/// ([`AnyCollector::for_each_reachable_object_view`]): read-only, iterative and cycle-safe,
/// every reachable boxed object exactly once, cons cells traversed but not reported. A refc
/// binary's on-heap handle is the frozen `[BinRef(3 words)][ErlSubBits(5 words)]` pair
/// (`beam_term::refc`); the collector's child walk descends the `ErlSubBits`'s `orig` slot
/// unconditionally, so the `BinRef` behind **every** reachable `ErlSubBits` is reported as an
/// object of its own and the header test alone covers the whole pair — whichever of the two
/// objects is reached first.
///
/// A reported payload pointer is the value `BinRef.val` holds, i.e.
/// [`RefcBinary::data_ptr`](beam_alloc::RefcBinary::data_ptr) of the payload the owning roster
/// registered: the comparison key between a live handle and an owned off-heap object. The same
/// payload reached through two different handles (a shared binary, two sub-binaries over one
/// buffer) is visited once per `BinRef` — dedup is the visitor's business — and a match-context
/// `ErlSubBits` (whose `orig` is an on-heap bitstring) is correctly not reported.
///
/// # Errors
///
/// Propagates the reachability walk's typed error; a partial walk is never reported as a
/// complete one.
///
/// # Examples
///
/// ```ignore
/// let mut live = Vec::new();
/// for_each_reachable_binary_payload(&collector, &roots, |payload| live.push(payload))?;
/// live.sort_unstable();  // a sorted set keeps the roster sweep below a binary search
/// ```
pub fn for_each_reachable_binary_payload(
    collector: &AnyCollector,
    roots: &[Eterm],
    mut visit: impl FnMut(usize),
) -> Result<()> {
    collector.for_each_reachable_object_view(roots, |object| {
        // The subtag test: an off-heap refc binary's `BinRef` is the only on-heap object whose
        // header is `HEADER_BIN_REF` (the frozen `_make_header(2, BIN_REF_SUBTAG)` word).
        if object.header() != HEADER_BIN_REF {
            return;
        }
        // Word 1 is `BinRef.val` — the payload's first data byte. A narrower object (a shape
        // whose header lied) simply names nothing.
        if let Some(val) = object.word(1) {
            visit(val.raw());
        }
    })
}

/// Drops every [`OffHeapEntry::Binary`] that no reachable `BinRef` names; keeps every
/// [`OffHeapEntry::Mso`].
///
/// Returns how many entries were released. The release itself is
/// [`OffHeap::sweep`](crate::mso::OffHeap::sweep)'s, so it is **slot-stable**: a released slot
/// stays in the roster as a released slot (a later `insert` never reuses it), every surviving
/// entry keeps its original slot index, and each released entry's refcount is decremented
/// **exactly once** — the payload is freed when the roster held the last reference, which is
/// the C `erts_bin_release` in `sweep_off_heap`'s garbage arm (`erl_gc.c:3089-3094`).
///
/// The liveness predicate is the reachable-payload set of
/// [`for_each_reachable_binary_payload`], matched against
/// [`RefcBinary::data_ptr`](beam_alloc::RefcBinary::data_ptr) — the identical pointer value
/// `BinRef.val` carries. An [`Mso`](crate::mso::OffHeapEntry::Mso) is unconditionally kept:
/// an MSO has no on-heap reference *term* (its safe reference is the slot index the caller
/// stores in an ordinary term), so deciding an MSO's liveness needs the caller's own slot
/// interpretation, which this seam deliberately does not guess. A roster holding MSOs must
/// therefore sweep them through [`OffHeap::sweep`](crate::mso::OffHeap::sweep) directly, or
/// expect them retained.
///
/// # Errors
///
/// Propagates the reachability walk's typed error and releases **nothing** in that case: the
/// roster is only touched once the live set is complete.
///
/// # Examples
///
/// ```ignore
/// let released = sweep_unreachable_binaries(&mut roster, &collector, &roots)?;
/// let surviving_words = roster.overhead_bytes() / size_of::<Eterm>();
/// vheap.record_collection(&policy, surviving_words);
/// ```
pub fn sweep_unreachable_binaries(
    off_heap: &mut OffHeap,
    collector: &AnyCollector,
    roots: &[Eterm],
) -> Result<usize> {
    // The live set, sorted, so the roster's per-entry test is a binary search rather than a
    // linear scan: a process holding `n` off-heap binaries pays `O(n log n)` here instead of
    // `O(n^2)`, which matters precisely for the off-heap-heavy process this seam exists for.
    let mut live: Vec<usize> = Vec::new();
    for_each_reachable_binary_payload(collector, roots, |payload| live.push(payload))?;
    live.sort_unstable();

    let released = off_heap.sweep(|entry| match entry {
        // A binary is live iff a reachable `BinRef` names its payload.
        OffHeapEntry::Binary(binary) => live.binary_search(&(binary.data_ptr() as usize)).is_ok(),
        // An MSO has no `BinRef`-shaped reference; this seam never reclaims one.
        OffHeapEntry::Mso(_) => true,
    });
    Ok(released)
}
