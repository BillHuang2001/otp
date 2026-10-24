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

//! [`ScannedBounds`] — the `O(1)` aggregate extent over every region a collector scans.
//!
//! A host's **unscanned-root valve** asks, per reference word *and* per heap admission, "does
//! this address lie in a region the collector scans?" The exact answer is a test per region:
//! [`young_contains`](crate::Collector::young_contains) (the nursery chain, `O(log #spaces)`
//! through the chain's ordered index — see `src/nursery.rs`) and
//! [`mature_contains`](crate::AnyCollector::mature_contains) (the mature generation), on top of
//! whatever the host itself owns (a heap block, a pooled arena, …).
//!
//! [`ScannedBounds`] answers the **rejection** half of that question for the whole collector in
//! a single interval comparison: it is the covering interval `[base, limit)` of **every** live
//! scanned region — the whole nursery chain (the current space *and* every space a growth
//! retired) **plus** the mature generation.
//!
//! # A fast-reject filter, never a containment claim
//!
//! The regions behind the aggregate are *not* one contiguous allocation, and an individual
//! space's live region is narrower than the extent the aggregate is built from (see
//! [`ScannedBounds::may_contain`]). So:
//!
//! * [`may_contain`](ScannedBounds::may_contain) `== false` ⇒ the address is in **no** scanned
//!   region, because every scanned region lies inside `[base, limit)`. That is the only
//!   direction the aggregate may ever be used for, and the safety property it relies on is the
//!   safe one: the aggregate **over**-approximates, so a `false` is exact and a `true` is not.
//! * [`may_contain`](ScannedBounds::may_contain) `== true` ⇒ **nothing is known**: the address
//!   may be inside a region, in a hole between two spaces, in the not-yet-allocated tail of a
//!   space, or on a free line of the mark-region mature generation. The caller must fall
//!   through to the exact per-region tests.
//!
//! A fast reject is allowed; a fast *accept* is not. Using a `true` answer as "contained" would
//! let a host treat a foreign pointer as a live reference and, in the copying collector, follow
//! or rewrite a word outside every space — a memory-safety bug, not a wrong answer. That is why
//! the predicate is named `may_contain` and why [`definitely_unscanned`](ScannedBounds::definitely_unscanned)
//! is the form a caller should write.
//!
//! # What the extent is built from
//!
//! Every space contributes its **reserved extent**, not its live fill: a [`HeapSpace`]'s
//! `[base, limit)` ([`limit`](crate::HeapSpace::limit) is `base + capacity`) and the
//! [`ImmixSpace`]'s whole block-aligned region (`base .. base + capacity`). The live fill
//! (`top()`) moves on **every allocation**, so a cached bound built from it would be stale
//! immediately; the reserved extent is stable for the space's whole lifetime and changes only
//! when a space is installed, retired, collapsed or replaced — exactly the events that advance
//! [`generation`](ScannedBounds::generation). The aggregate is therefore a **superset** of every
//! space's live region, which is precisely the safe direction for a reject.
//!
//! This module is plain value-type arithmetic — no `unsafe`, no allocation.
//!
//! [`HeapSpace`]: crate::space::HeapSpace
//! [`ImmixSpace`]: crate::immix::ImmixSpace

use core::mem::size_of;

use beam_term::prelude::Eterm;

/// The covering interval `[base, limit)` of every live scanned region of one collector, tagged
/// with the geometry epoch it describes.
///
/// Built by [`Collector::scanned_bounds`](crate::Collector::scanned_bounds),
/// [`GenImmixCollector::scanned_bounds`](crate::GenImmixCollector::scanned_bounds) and
/// (forwarding) [`AnyCollector::scanned_bounds`](crate::AnyCollector::scanned_bounds) in
/// `O(1)` — the nursery chain's half is cached inside the chain itself and the mature
/// generation's half is two reads of a space that never moves.
///
/// `ScannedBounds` is a [`Copy`] value: read it once at a safepoint and test as many addresses
/// against it as the caller likes. It is only valid for the heap state it was read at — compare
/// [`generation`](Self::generation) to notice, or simply re-read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScannedBounds {
    /// The lowest scanned base address, or `usize::MAX` for the empty aggregate.
    base: usize,
    /// One past the highest scanned extent end (exclusive), or `0` for the empty aggregate.
    limit: usize,
    /// The geometry epoch: a monotone counter of the geometry events that produced this value.
    generation: u64,
}

impl ScannedBounds {
    /// The empty aggregate: it covers nothing, so
    /// [`may_contain`](ScannedBounds::may_contain) answers `false` for **every** address and
    /// [`definitely_unscanned`](ScannedBounds::definitely_unscanned) `true`.
    ///
    /// It is the identity of the crate-internal `covering` operation and the value a collector
    /// with no scanned region at all would report.
    pub const EMPTY: ScannedBounds = ScannedBounds {
        base: usize::MAX,
        limit: 0,
        generation: 0,
    };

    /// The half-open byte-address interval `[base, limit)`, at geometry epoch `0`.
    pub fn new(base: usize, limit: usize) -> ScannedBounds {
        ScannedBounds {
            base,
            limit,
            generation: 0,
        }
    }

    /// The half-open byte-address interval `[base, limit)`, tagged with `generation` — the
    /// epoch of the geometry it was read from.
    pub fn at(base: usize, limit: usize, generation: u64) -> ScannedBounds {
        ScannedBounds {
            base,
            limit,
            generation,
        }
    }

    /// The lowest scanned base address (inclusive). For [`EMPTY`](ScannedBounds::EMPTY) this is
    /// `usize::MAX`, which no address is `>=`.
    pub fn base(&self) -> usize {
        self.base
    }

    /// One past the highest scanned extent end (exclusive). For
    /// [`EMPTY`](ScannedBounds::EMPTY) this is `0`, which no address is `<`.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The geometry epoch of the heap state this value was read at: a monotone counter that
    /// advances on **every** event that can change a scanned region — a nursery space being
    /// installed, retired or collapsed (the chain's edits), and the mature generation being
    /// replaced/relocated. It never advances for an event that leaves the geometry alone (a
    /// [`grow_nursery`](crate::Collector::grow_nursery) that finds room, a collection that
    /// changes neither the spaces nor their occupancy).
    ///
    /// Two bounds with the same generation describe the same geometry, so a caller that caches
    /// derived facts (a match against the aggregate, a precomputed predicate) can key them on
    /// it. The counter saturates rather than wrapping, so it is monotone (non-decreasing) over
    /// the collector's whole life.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether this aggregate covers no address at all.
    pub fn is_empty(&self) -> bool {
        self.base >= self.limit
    }

    /// Whether `addr` can possibly lie in a scanned region — the **fast-reject** filter.
    ///
    /// `false` is exact and load-bearing: the address is in **no** region the collector scans,
    /// because every scanned region lies inside `[base, limit)`. `true` is **not** a
    /// containment claim — the aggregate spans every hole between two spaces, the unallocated
    /// tail of each space, and the mark-region mature generation's free lines — so it means
    /// only "the exact per-region tests must decide". See the module docs.
    pub fn may_contain(&self, addr: usize) -> bool {
        self.base <= addr && addr < self.limit
    }

    /// Whether `ptr` definitely addresses **nothing** the collector scans — the form a host's
    /// unscanned-root valve should call.
    ///
    /// Exactly `!may_contain(ptr as usize)`: `true` means "in no scanned region, so the
    /// collector's exact range tests would both answer `false`"; `false` means "unknown, ask
    /// them".
    pub fn definitely_unscanned(&self, ptr: *const Eterm) -> bool {
        !self.may_contain(ptr as usize)
    }

    /// The covered span in term words — a reporting/measurement aid (how wide the
    /// over-approximation a collector's geometry forces is), never part of the filter.
    pub fn span_words(&self) -> usize {
        (self.limit.saturating_sub(self.base)) / size_of::<Eterm>()
    }

    /// The smallest interval covering both `self` and `other`, as an interval — the operation
    /// that turns a collector's parts into its whole.
    ///
    /// The returned value carries `self`'s [`generation`](ScannedBounds::generation):
    /// generations belong to the owner of the geometry epoch, which is the only party that
    /// knows how to combine two independent counters (`Collector::scanned_bounds` sums the
    /// nursery chain's epoch and the mature generation's replacement count). Interval
    /// arithmetic and the epoch tag are therefore kept separate — a caller cannot silently
    /// acquire an epoch it did not compute.
    pub(crate) fn covering(self, other: ScannedBounds) -> ScannedBounds {
        ScannedBounds {
            base: self.base.min(other.base),
            limit: self.limit.max(other.limit),
            generation: self.generation,
        }
    }

    /// The same interval tagged with `generation`.
    pub(crate) fn with_generation(self, generation: u64) -> ScannedBounds {
        ScannedBounds {
            generation,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the value type: the half-open boundary rule, the empty aggregate as the
    //! identity of `covering`, and the `span_words` accounting. The aggregate's agreement with
    //! the exact per-region tests is pinned where the geometry lives (`src/nursery.rs`'s
    //! in-module tests and `tests/scanned_bounds.rs`).

    use super::*;

    /// One term word, in bytes.
    const WORD: usize = size_of::<Eterm>();

    #[test]
    fn may_contain_is_half_open_on_both_ends() {
        let bounds = ScannedBounds::new(0x1000, 0x1000 + 8 * WORD);
        // The whole covered run, word by word.
        let mut addr = 0x1000;
        while addr < 0x1000 + 8 * WORD {
            assert!(bounds.may_contain(addr), "{addr:#x} is inside");
            addr += WORD;
        }
        // Both boundaries: `base` is inclusive, `limit` is exclusive.
        assert!(bounds.may_contain(0x1000), "base is covered");
        assert!(
            bounds.may_contain(0x1000 + 8 * WORD - 1),
            "the last covered byte"
        );
        assert!(!bounds.may_contain(0x1000 - 1), "one byte below base");
        assert!(
            !bounds.may_contain(0x1000 + 8 * WORD),
            "limit itself is outside"
        );
        assert!(!bounds.may_contain(usize::MAX), "nothing is covered at MAX");
        assert!(!bounds.may_contain(0));
    }

    #[test]
    fn the_empty_aggregate_rejects_every_address() {
        for bounds in [ScannedBounds::EMPTY, ScannedBounds::new(0, 0), ScannedBounds::new(9, 3)] {
            assert!(bounds.is_empty(), "{bounds:?} covers nothing");
            for addr in [0, 1, 3, 8, WORD, 0x1000, usize::MAX] {
                assert!(!bounds.may_contain(addr), "{bounds:?} must reject {addr:#x}");
                assert!(
                    bounds.definitely_unscanned(addr as *const Eterm),
                    "a rejected address is definitely unscanned"
                );
            }
            assert_eq!(bounds.span_words(), 0, "an empty span has no words");
        }
        // A zero-capacity space is `base == limit`, i.e. empty as well.
        let zero = ScannedBounds::new(0x2000, 0x2000);
        assert!(zero.is_empty());
        assert!(!zero.may_contain(0x2000));
    }

    #[test]
    fn covering_is_the_smallest_interval_holding_both() {
        let a = ScannedBounds::new(0x1000, 0x2000);
        let b = ScannedBounds::new(0x1800, 0x2400);
        let c = a.covering(b);
        assert_eq!((c.base(), c.limit()), (0x1000, 0x2400));
        // Commutative on the interval, on either order.
        assert_eq!(b.covering(a).base(), c.base());
        assert_eq!(b.covering(a).limit(), c.limit());
        // Every address either part covers is covered by the union.
        for addr in [0x1000, 0x1FFF, 0x1800, 0x23FF] {
            assert!(c.may_contain(addr), "{addr:#x} is covered by the union");
        }
        // A disjoint neighbourhood stays rejected.
        assert!(!c.may_contain(0x0FFF));
        assert!(!c.may_contain(0x2400));

        // The empty aggregate is the identity.
        assert_eq!(ScannedBounds::EMPTY.covering(a), ScannedBounds::new(a.base(), a.limit()));
        assert_eq!(a.covering(ScannedBounds::EMPTY), a);
        assert!(ScannedBounds::EMPTY
            .covering(ScannedBounds::EMPTY)
            .is_empty());

        // Interval and epoch are separate: `covering` keeps the receiver's generation.
        let tagged = ScannedBounds::at(0x1000, 0x2000, 7);
        assert_eq!(tagged.covering(b).generation(), 7);
        assert_eq!(tagged.covering(b).with_generation(11).generation(), 11);
    }

    #[test]
    fn span_words_counts_term_words_not_bytes() {
        assert_eq!(ScannedBounds::new(0x1000, 0x1000 + 12 * WORD).span_words(), 12);
        // A limit below the base cannot underflow.
        assert_eq!(ScannedBounds::new(0x1000, 0x10).span_words(), 0);
    }

    #[test]
    fn definitely_unscanned_is_the_negation_of_may_contain() {
        let bounds = ScannedBounds::at(0x1000, 0x1000 + 4 * WORD, 3);
        for addr in [0, 0xFFF, 0x1000, 0x1000 + 3 * WORD, 0x1000 + 4 * WORD, usize::MAX] {
            assert_eq!(
                bounds.definitely_unscanned(addr as *const Eterm),
                !bounds.may_contain(addr),
                "{addr:#x}"
            );
        }
    }
}
