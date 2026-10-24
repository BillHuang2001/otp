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

//! [`HeapFootprint`] — the one-call accounting of everything this crate charges to one
//! process: its heap (nursery chain + mature generation) and its off-heap store.
//!
//! # The three-way split, stated honestly
//!
//! A footprint is **not** an RSS estimate and it is not a complete memory account; it is
//! exactly the bytes this crate can see, in three parts that a reporter must not conflate:
//!
//! 1. **Reserved heap capacity** ([`HeapFootprint::capacity_bytes`]) — the nursery chain's
//!    and the mature generation's *capacity* in words times [`WORD_BYTES`].
//!    Every byte of it is a reserved `beam_alloc` heap allocation, live occupancy or not, so
//!    it is what a "processes" figure charges.
//!    The **used** half ([`HeapFootprint::used_bytes`]) is the same spaces' occupancy — the
//!    reporting split `processes` / `processes_used`.
//! 2. **Live off-heap payload bytes** ([`HeapFootprint::off_heap_payload_bytes`]) — the
//!    apparent size of every live MSO plus every live refcounted binary's payload length.
//!    These are **already counted** by `erlang:memory(binary)` (they are live
//!    [`AllocType::Binary`](beam_alloc::AllocType::Binary) bytes plus the MSO payloads), so a
//!    reporter that adds a footprint to `processes` **on top of** `binary` double counts them;
//!    the two figures answer different questions ("what does this process hold off-heap" vs
//!    "how many off-heap bytes exist") and must be reported side by side, not added.
//! 3. **The off-heap roster's slot array** ([`HeapFootprint::off_heap_slot_bytes`]) — the
//!    `Vec` that holds the roster's slots, whose positions are stable for the roster's whole
//!    lifetime (a released slot keeps its position).
//!    **No allocator type records these bytes**: the `Vec` goes through the process's global
//!    allocator, not `beam_alloc`, which is precisely why they need their own field.
//!
//! [`HeapFootprint::heap_bytes`] is therefore the **allocator-visible subset** (parts 1 and 2 —
//! every byte of it is a `beam_alloc` allocation) and [`HeapFootprint::total_bytes`] is
//! everything this crate accounts for (parts 1, 2 and 3).
//!
//! # What this crate cannot see
//!
//! * **No process mailbox or message-queue bytes.** The queue is `beam_message`'s; this crate
//!   is handed a collector and an off-heap roster and nothing else.
//! * **No carrier/mseg retention beyond live payload.** A `beam_alloc` carrier that still holds
//!   free blocks after a collection is a process-global allocator concern, not a per-process
//!   one; only the live figures above are attributed to the process.
//! * **No runtime text/data**, no stack, no Rust-side bookkeeping (`Vec` roots a host holds, the
//!   collector's own index structures).
//!
//! This module is safe Rust, result-free and allocation-free: it reads counts and multiplies.

use core::mem::size_of;

use beam_term::prelude::Eterm;

use crate::heap_info::HeapObservable;
use crate::mso::OffHeap;

/// The bytes one term word occupies — `size_of::<Eterm>()`, i.e. 8 on a 64-bit target.
///
/// This is the single conversion factor between the word counts the collector reports and the
/// byte counts an allocator reports; it is a constant, never a per-target guess.
pub const WORD_BYTES: usize = size_of::<Eterm>();

/// The bytes this crate accounts for one process's heap and off-heap store.
///
/// Built by [`HeapFootprint::of`] from any [`HeapObservable`] collector plus the process's
/// [`OffHeap`] roster; see the module docs for the three-way split and what it deliberately
/// leaves out.
/// Every field is a plain count, so the struct is `Copy`, comparable, and cheap to hand to a
/// reporter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeapFootprint {
    /// Nursery-chain capacity in words (summed over every space, retired ones included).
    pub young_capacity_words: usize,
    /// Nursery-chain occupancy in words — chain-summed, so a retired space's live words count.
    pub young_used_words: usize,
    /// Mature-generation capacity in words.
    pub mature_capacity_words: usize,
    /// Mature-generation occupancy in words.
    pub mature_used_words: usize,
    /// Live off-heap payload bytes (MSO apparent size + refc-binary payload length), live slots only.
    pub off_heap_payload_bytes: usize,
    /// The off-heap roster's reserved slot-array bytes (retained; released slots still occupy it).
    pub off_heap_slot_bytes: usize,
    /// Live off-heap entries (MSO + refc binaries).
    pub off_heap_entries: usize,
}

impl HeapFootprint {
    /// Derives the footprint of `collector` plus its off-heap `off_heap` roster in one call.
    ///
    /// The heap half comes from the collector's own observables: the **capacities** from
    /// [`heap_size_words`](HeapObservable::heap_size_words) /
    /// [`total_heap_size_words`](HeapObservable::total_heap_size_words) and the
    /// **occupancies** from [`heap_used_words`](HeapObservable::heap_used_words) /
    /// [`total_heap_used_words`](HeapObservable::total_heap_used_words).
    /// The young/mature split is the difference between the total and the young figure, taken
    /// with a saturating subtraction so a collector whose totals ever disagreed still yields a
    /// footprint rather than a panic (both are chain-summed, so `total >= young` holds).
    ///
    /// The off-heap half is the roster's live entries ([`OffHeap::len`]), their payload bytes
    /// ([`OffHeap::overhead_bytes`]) and its reserved slot array ([`OffHeap::slot_bytes`]).
    /// Result-free, allocation-free, no panics.
    pub fn of<C: HeapObservable + ?Sized>(collector: &C, off_heap: &OffHeap) -> HeapFootprint {
        let young_capacity_words = collector.heap_size_words();
        let young_used_words = collector.heap_used_words();
        let total_capacity_words = collector.total_heap_size_words();
        let total_used_words = collector.total_heap_used_words();
        HeapFootprint {
            young_capacity_words,
            young_used_words,
            mature_capacity_words: total_capacity_words.saturating_sub(young_capacity_words),
            mature_used_words: total_used_words.saturating_sub(young_used_words),
            off_heap_payload_bytes: off_heap.overhead_bytes(),
            off_heap_slot_bytes: off_heap.slot_bytes(),
            off_heap_entries: off_heap.len(),
        }
    }

    /// The reserved heap capacity in words — the nursery chain plus the mature generation.
    pub fn capacity_words(&self) -> usize {
        self.young_capacity_words + self.mature_capacity_words
    }

    /// The heap occupancy in words — the whole nursery chain plus the mature generation.
    pub fn used_words(&self) -> usize {
        self.young_used_words + self.mature_used_words
    }

    /// The reserved heap capacity in bytes ([`capacity_words`](Self::capacity_words) times
    /// [`WORD_BYTES`]).
    pub fn capacity_bytes(&self) -> usize {
        self.capacity_words() * WORD_BYTES
    }

    /// The heap occupancy in bytes ([`used_words`](Self::used_words) times [`WORD_BYTES`]).
    pub fn used_bytes(&self) -> usize {
        self.used_words() * WORD_BYTES
    }

    /// The **allocator-visible** subset: reserved heap capacity bytes plus live off-heap payload
    /// bytes.
    ///
    /// Every byte of it is a live `beam_alloc` allocation (a heap space's block, a
    /// [`RefcBinary`](beam_alloc::RefcBinary)'s payload, an MSO's boxed payload), so it can never
    /// exceed the sum of the allocators' own live bytes — the invariant `tests/heap_accounting.rs`
    /// pins.
    /// It deliberately **excludes** [`off_heap_slot_bytes`](Self::off_heap_slot_bytes), which no
    /// allocator type records.
    pub fn heap_bytes(&self) -> usize {
        self.capacity_bytes() + self.off_heap_payload_bytes
    }

    /// **Everything** this crate accounts for: [`heap_bytes`](Self::heap_bytes) plus the
    /// off-heap roster's reserved slot array.
    ///
    /// A *reserved-capacity plus payload* account, not an RSS estimate — see the module docs for
    /// the three parts and what is left out.
    pub fn total_bytes(&self) -> usize {
        self.heap_bytes() + self.off_heap_slot_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::Collector;
    use crate::policy::HeapPolicy;
    use beam_alloc::RefcBinary;

    #[test]
    fn a_fresh_collector_and_an_empty_roster_report_their_capacities() {
        let policy = HeapPolicy {
            initial_young_words: 233,
            initial_mature_words: 376,
            ..HeapPolicy::default()
        };
        let c = Collector::new(policy).expect("collector");
        let footprint = HeapFootprint::of(&c, &OffHeap::new());
        assert_eq!(footprint.young_capacity_words, 233);
        assert_eq!(footprint.mature_capacity_words, 376);
        assert_eq!(footprint.young_used_words, 0, "nothing was allocated");
        assert_eq!(footprint.mature_used_words, 0);
        assert_eq!(footprint.off_heap_payload_bytes, 0);
        assert_eq!(footprint.off_heap_slot_bytes, 0, "an empty Vec reserves none");
        assert_eq!(footprint.off_heap_entries, 0);
        assert_eq!(footprint.capacity_words(), 609);
        assert_eq!(footprint.capacity_bytes(), 609 * WORD_BYTES);
        assert_eq!(footprint.used_words(), 0);
        assert_eq!(footprint.total_bytes(), footprint.heap_bytes());
    }

    #[test]
    fn the_payload_is_the_allocator_visible_half_and_the_slot_array_is_not() {
        let c = Collector::new(HeapPolicy::default()).expect("collector");
        let mut roster = OffHeap::new();
        for _ in 0..3 {
            roster.push(RefcBinary::new(&[0u8; 64]).expect("off-heap binary"));
        }
        let footprint = HeapFootprint::of(&c, &roster);
        assert_eq!(footprint.off_heap_entries, 3);
        assert_eq!(footprint.off_heap_payload_bytes, 3 * 64);
        assert_eq!(footprint.off_heap_slot_bytes, roster.slot_bytes());
        assert_eq!(
            footprint.heap_bytes(),
            footprint.capacity_bytes() + footprint.off_heap_payload_bytes,
            "the payload is part of the allocator-visible subset"
        );
        assert_eq!(
            footprint.total_bytes(),
            footprint.heap_bytes() + footprint.off_heap_slot_bytes,
            "the roster array is counted by the total only"
        );
        assert!(
            footprint.off_heap_slot_bytes > 0 && footprint.off_heap_slot_bytes < 4096,
            "a three-slot array reserves a few entries' worth of bytes, got {}",
            footprint.off_heap_slot_bytes
        );
    }

    #[test]
    fn a_released_slot_keeps_reserving_bytes_while_its_payload_stops_counting() {
        let c = Collector::new(HeapPolicy::default()).expect("collector");
        let mut roster = OffHeap::new();
        for _ in 0..4 {
            roster.push(RefcBinary::new(&[0u8; 128]).expect("off-heap binary"));
        }
        let before = HeapFootprint::of(&c, &roster);
        assert_eq!(roster.sweep(|_| false), 4, "every entry is released");
        let after = HeapFootprint::of(&c, &roster);
        assert_eq!(after.off_heap_entries, 0, "no live entry is left");
        assert_eq!(after.off_heap_payload_bytes, 0, "the payloads are gone");
        assert_eq!(
            after.off_heap_slot_bytes, before.off_heap_slot_bytes,
            "the slot array is retained: slot stability means it never shrinks"
        );
        assert_eq!(
            after.total_bytes(),
            after.capacity_bytes() + after.off_heap_slot_bytes,
            "the retained array is the only off-heap footprint left"
        );
        assert_eq!(roster.slot_count(), 4, "the released slots are still positions");
    }
}
