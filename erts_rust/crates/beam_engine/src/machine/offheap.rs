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

//! The off-heap refc-binary **retention seam** on [`Machine`].
//!
//! The machine's off-heap payload reservoir ([`Machine::refc_binaries`]) is what keeps
//! every refc binary the machine built alive and address-stable while the machine lives.
//! [`Machine::reset`] deliberately never clears it (a returned term may still name a
//! payload after the run that built it), so the only reclamation point is a **host**
//! decision — a reachability sweep over the live process roots, which is exactly what
//! this module's two [`Machine`] methods expose:
//!
//! * [`Machine::off_heap_overhead_bytes`] — how many backing-store bytes the reservoir
//!   holds, the figure a host prices against its off-heap-binary trigger
//!   (`beam_gc::BinVheap::triggered`, the C `ERTS_IS_GC_DESIRED_INTERNAL` disjunct
//!   `off_heap.overhead > bin_vheap_sz`).
//! * [`Machine::retain_refc_binaries`] — drop the handles the host's reachability
//!   predicate does not admit, together with the writable-append registry entries that
//!   named them.
//!
//! Both are plain safe Rust: no `unsafe`, no allocation, no panic. Reclamation **moves
//! nothing** — a retained payload keeps its exact `data_ptr`, which is the value a live
//! on-heap `BinRef.val` word holds ([`beam_term::refc`]'s frozen contract), so compaction
//! can never dangle a live term.
//!
//! ## Why a child module
//!
//! `src/machine/mod.rs` already carries the whole `Machine` state; this seam is a
//! self-contained pair of methods plus their pins, so it lives in its own file to keep
//! both under the crate's ~1000-line budget. A child module of `machine` sees the
//! parent's private and `pub(crate)` fields, so the two passes here touch the reservoir
//! and the writable registry directly instead of going through accessors.

use super::Machine;

impl Machine {
    /// The **backing-store bytes** this machine's off-heap refc-binary reservoir holds:
    /// the sum of [`beam_alloc::RefcBinary::reserved_bytes`] over
    /// [`Machine::refc_binaries`].
    ///
    /// # Why the reservation, not the length
    ///
    /// The figure a host charges for is the memory actually **held**, not the bytes a
    /// term happens to name — the same accounting `beam_gc::sweep_unreachable_binaries`
    /// and `beam_gc::BinVheap` reproduce. For an exactly-sized payload
    /// (`Machine::alloc_refc_binary`) `len() == capacity() == reserved_bytes()`, so the
    /// two readings coincide; for a `bs_init_writable` accumulator
    /// (`Machine::alloc_refc_binary_with_capacity`) the payload carries a 4x
    /// reservation whose spare room is real allocated memory, and `len()` would
    /// under-report it.
    ///
    /// # The words conversion belongs to the host
    ///
    /// The consumer is words-based (`beam_gc::BinVheap::triggered(off_heap_overhead_words)`,
    /// whose floor is the heap policy's `min_bin_vheap_words`), so a host converts with
    /// `off_heap_overhead_bytes() / size_of::<beam_term::Eterm>()` (8 words on a 64-bit
    /// target) — a truncating integer division, which is the same per-binary rounding the C
    /// applies when it sums each binary's size in words.
    ///
    /// Infallible, allocation-free and `O(n)` in the number of payload handles.
    pub fn off_heap_overhead_bytes(&self) -> usize {
        self.refc_binaries
            .iter()
            .map(beam_alloc::RefcBinary::reserved_bytes)
            .sum()
    }

    /// Release every off-heap refc-binary handle whose payload `keep` does **not** admit,
    /// returning how many handles were released.
    ///
    /// `keep` is called with a payload's first data byte (`RefcBinary::data_ptr` — exactly
    /// the value an on-heap `BinRef.val` word holds) and returns `true` to retain it.
    ///
    /// # Caller contract — the predicate IS the soundness assertion
    ///
    /// **This method cannot verify that its predicate is right; the caller asserts it.**
    /// `keep` must admit every payload a live term still reaches.
    /// A **false reject** (a payload a live `BinRef.val` still names) frees memory the term
    /// points at — a dangling pointer, read later through the frozen term layout — while a
    /// **false accept** is merely a leak. The engine has no way to tell the two apart: the
    /// reservoir holds handles, not roots, and the roots live in the host's process state.
    ///
    /// Intended use: a host that has just run a reachability sweep over its process roots
    /// (`beam_gc::for_each_reachable_binary_payload`, the C `sweep_off_heap`) passes "is
    /// this data pointer in the reachable set", after first **cloning** the handles it
    /// wants to keep beyond the machine into its own off-heap table — a cloned handle bumps
    /// the payload's refcount, so the payload survives the drop here even when the host's
    /// own copy of the handle is what keeps it alive.
    ///
    /// # What it does
    ///
    /// * The reservoir is compacted in place: every retained handle keeps its payload and
    ///   that payload's `data_ptr`, so no on-heap `BinRef.val` is invalidated and no term
    ///   needs republishing.
    /// * A released handle drops its [`beam_alloc::RefcBinary`]; that frees the payload
    ///   only when the handle was the **last** one (`refcount() == 1`). A payload a host or
    ///   another process still holds is kept alive by that other handle, so the released
    ///   count is a count of *handles*, not of freed blocks.
    /// * The writable-append registry (`Machine::writable_payloads`) is pruned **in the
    ///   same call**: a `(base, capacity)` entry whose base no longer names a live retained
    ///   payload is dropped, so a later `writable_capacity`/`writable_append_target`/
    ///   `grow_writable_payload` can never answer for a freed block, and a retained
    ///   accumulator keeps its exact capacity (those doors resolve payloads by `data_ptr`
    ///   search, so compaction is invisible to them).
    ///
    /// An empty reservoir is a cheap no-op returning `0`. Infallible, allocation-free and
    /// `O(n·m)` for `n` payload handles and `m` writable registrations (both a handful in
    /// practice).
    pub fn retain_refc_binaries<F>(&mut self, keep: F) -> usize
    where
        F: FnMut(*const u8) -> bool,
    {
        let mut keep = keep;
        let before = self.refc_binaries.len();
        self.refc_binaries
            .retain(|binary| keep(binary.data_ptr() as *const u8));
        let released = before - self.refc_binaries.len();
        // The writable-append registry is keyed by payload base address, so it may only
        // name a payload the pass above retained. Pruning AFTER the reservoir pass (never
        // before) is what makes "no live retained payload" the exact drop condition.
        self.writable_payloads.retain(|(base, _)| {
            self.refc_binaries
                .iter()
                .any(|binary| binary.data_ptr() as usize == *base)
        });
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine holding one payload per `payloads` entry, plus each payload's data
    /// pointer in the same order (the value an on-heap `BinRef.val` word would hold).
    fn machine_with(payloads: &[&[u8]]) -> (Machine, Vec<*mut u8>) {
        let mut machine = Machine::new();
        let bases = payloads
            .iter()
            .map(|bytes| {
                machine
                    .alloc_refc_binary(bytes)
                    .expect("an immutable payload allocates")
            })
            .collect();
        (machine, bases)
    }

    fn reserved(machine: &Machine) -> Vec<usize> {
        machine
            .refc_binaries()
            .iter()
            .map(beam_alloc::RefcBinary::reserved_bytes)
            .collect()
    }

    #[test]
    fn off_heap_overhead_sums_every_payloads_reservation() {
        let (machine, _bases) = machine_with(&[b"a", &[7u8; 200], &[9u8; 5_000]]);
        let expected: usize = reserved(&machine).iter().sum();
        assert_eq!(machine.off_heap_overhead_bytes(), expected);
        // A reservation covers the payload it holds, so the total can never be smaller
        // than the bytes the three payloads actually carry.
        assert!(machine.off_heap_overhead_bytes() >= 1 + 200 + 5_000);
        // The words figure a host feeds `BinVheap::triggered` is a truncating division by
        // the term size; the machine reports bytes and never converts for the host.
        let word = std::mem::size_of::<beam_term::Eterm>();
        assert_eq!(word, 8, "one Eterm is one machine word");
        assert_eq!(
            machine.off_heap_overhead_bytes() / word,
            expected / word,
            "the host's conversion is a plain truncating division"
        );
    }

    #[test]
    fn retaining_everything_releases_nothing_and_moves_no_payload() {
        let (mut machine, bases) = machine_with(&[b"one", b"two", b"three"]);
        let before = machine.off_heap_overhead_bytes();
        assert_eq!(machine.retain_refc_binaries(|_| true), 0);
        assert_eq!(machine.refc_binaries().len(), 3);
        assert_eq!(machine.off_heap_overhead_bytes(), before);
        let after: Vec<*mut u8> = machine
            .refc_binaries()
            .iter()
            .map(beam_alloc::RefcBinary::data_ptr)
            .collect();
        // Compaction never moves a payload: every live `BinRef.val` stays valid.
        assert_eq!(after, bases);
        assert_eq!(machine.refc_binaries()[1].as_bytes(), b"two");
    }

    #[test]
    fn retaining_one_payload_keeps_its_data_pointer_and_frees_the_others() {
        let (mut machine, bases) = machine_with(&[b"alpha", &[3u8; 300], b"omega"]);
        let freed: usize = reserved(&machine)
            .into_iter()
            .zip(&bases)
            .filter(|(_, base)| **base != bases[1])
            .map(|(bytes, _)| bytes)
            .sum();
        let before = machine.off_heap_overhead_bytes();
        assert_eq!(
            machine.retain_refc_binaries(|base| std::ptr::eq(base, bases[1])),
            2
        );
        // The middle payload survived at its exact address, with its bytes intact.
        assert_eq!(machine.refc_binaries().len(), 1);
        assert_eq!(machine.refc_binaries()[0].data_ptr(), bases[1]);
        assert_eq!(machine.refc_binaries()[0].as_bytes(), &[3u8; 300]);
        // Exactly the released payloads' reservations left the overhead figure.
        assert_eq!(
            machine.off_heap_overhead_bytes(),
            before - freed,
            "the overhead figure drops by exactly the freed reservations"
        );
    }

    #[test]
    fn a_released_handle_does_not_free_a_payload_another_handle_still_holds() {
        let (mut machine, bases) = machine_with(&[b"shared"]);
        // A host that clones a handle into its own off-heap table keeps the payload alive.
        let held = machine.refc_binaries()[0].clone();
        assert_eq!(held.refcount(), 2);
        assert_eq!(machine.retain_refc_binaries(|_| false), 1);
        assert_eq!(machine.refc_binaries().len(), 0);
        assert_eq!(machine.off_heap_overhead_bytes(), 0);
        // The released handle is a handle, not necessarily a freed block: the payload is
        // still readable through the cloned handle, at the same address.
        assert_eq!(held.refcount(), 1);
        assert_eq!(held.data_ptr(), bases[0]);
        assert_eq!(held.as_bytes(), b"shared");
    }

    #[test]
    fn pruning_a_registered_writable_base_drops_its_registration() {
        let mut machine = Machine::new();
        let (base, capacity) = machine
            .alloc_refc_binary_with_capacity(64)
            .expect("a capacity-carrying payload allocates");
        machine.mark_writable(base, capacity);
        assert_eq!(machine.writable_capacity(base), Some(capacity));
        assert_eq!(machine.writable_payload_count(), 1);

        assert_eq!(machine.retain_refc_binaries(|_| false), 1);
        // A registry entry must never outlive the payload it names.
        assert_eq!(machine.writable_capacity(base), None);
        assert_eq!(machine.writable_payload_count(), 0);
        assert_eq!(machine.refc_binaries().len(), 0);
        assert_eq!(machine.off_heap_overhead_bytes(), 0);
    }

    #[test]
    fn a_retained_writable_base_keeps_its_capacity_and_stays_growable() {
        let mut machine = Machine::new();
        let (first, first_capacity) = machine
            .alloc_refc_binary_with_capacity(16)
            .expect("a capacity-carrying payload allocates");
        machine.mark_writable(first, first_capacity);
        let (second, second_capacity) = machine
            .alloc_refc_binary_with_capacity(32)
            .expect("a capacity-carrying payload allocates");
        machine.mark_writable(second, second_capacity);
        assert_eq!(machine.writable_payload_count(), 2);

        assert_eq!(
            machine.retain_refc_binaries(|base| std::ptr::eq(base, second)),
            1
        );
        assert_eq!(machine.refc_binaries().len(), 1);
        assert_eq!(machine.writable_payload_count(), 1);
        assert_eq!(machine.writable_capacity(first), None);
        // The retained accumulator keeps its exact capacity, and the growth door still
        // resolves it after the reservoir compaction.
        assert_eq!(machine.writable_capacity(second), Some(second_capacity));
        let (grown_base, grown_capacity) = machine
            .grow_writable_payload(second, 1_000)
            .expect("the growth door reports rather than raises")
            .expect("a retained, unshared accumulator still grows");
        assert!(grown_capacity >= 1_000);
        assert_eq!(machine.writable_capacity(grown_base), Some(grown_capacity));
        // Whichever outcome the allocator chose, the one retained handle names the one
        // registered base.
        assert_eq!(machine.refc_binaries().len(), 1);
        assert_eq!(machine.refc_binaries()[0].data_ptr(), grown_base);
        assert_eq!(machine.writable_payload_count(), 1);
    }

    #[test]
    fn an_empty_reservoir_retain_is_a_no_op() {
        let mut machine = Machine::new();
        assert_eq!(machine.off_heap_overhead_bytes(), 0);
        assert_eq!(machine.writable_payload_count(), 0);
        assert_eq!(machine.retain_refc_binaries(|_| false), 0);
        assert_eq!(machine.retain_refc_binaries(|_| true), 0);
        assert_eq!(machine.refc_binaries().len(), 0);
        assert_eq!(machine.writable_payload_count(), 0);
        assert_eq!(machine.off_heap_overhead_bytes(), 0);
    }
}
