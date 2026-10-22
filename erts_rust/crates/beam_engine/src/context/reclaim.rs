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

//! The arena's two **reclaim tiers**: `HeapMark` rewind and copying collapse.
//!
//! [`MinimalContext`]'s heap ([`ContextHeap`]) is a `Vec` of `beam_alloc::ReservedHeap`
//! regions that, on its own, only ever grows: every word it hands out stays valid and
//! unmoved until the arena is dropped. Full ERTS is not the only thing that needs more than
//! that — a tree-walking `-eval` evaluator allocates a scratch term graph per expression and
//! then abandons it, and a long-lived arena would otherwise retain every byte it ever handed
//! out (see `beam_rt`'s `-eval` scratch window). This module adds the two **additive**,
//! **explicitly-invoked** tiers that give those words back, and nothing else:
//!
//! | Tier | Door | What it does | Cost |
//! |---|---|---|---|
//! | A — rewind | [`MinimalContext::heap_rewind`] | moves the frontier's bump pointer back to a [`HeapMark`] and drops every later region | **O(regions dropped)**; no word is read |
//! | B — reclaim | [`MinimalContext::reclaim_heap`] | copies the live graph into one fresh region and drops every old one | one Cheney copy of the live set |
//!
//! Neither one changes the allocation policy, is ever invoked automatically, or touches
//! [`ExecutionContext::alloc_words`](crate::ExecutionContext::alloc_words) /
//! `alloc_words_rooted` / [`ensure_heap`](crate::ExecutionContext::ensure_heap) /
//! [`heap_available`](crate::ExecutionContext::heap_available): an arena that never calls
//! them behaves exactly as it did before this module existed.
//!
//! # The caller's liveness assertion (both tiers)
//!
//! The arena **cannot tell a term word from raw payload** — a region's prefix holds a
//! mixture of tagged term words and raw bytes (a bitstring's bytes, a float's mantissa, a
//! tuple's element words) and only a structural walk knows which is which. So both tiers
//! are **purely mechanical**: they never decide what is live, they act on a claim the
//! caller makes.
//!
//! * **Tier A** requires: *no reachable term lies at or above `mark`* (and, therefore, in
//!   any of the regions it drops). Everything at or above the mark is then unreachable and
//!   may be overwritten; the caller's roots must be brought below the mark (or dropped)
//!   **before** the rewind, and must never be read afterwards.
//! * **Tier B** requires: *the root set the `forward` closure hands to the relocator is the
//!   whole live set*, and *every word the copier walks is a well-formed term structure* —
//!   any word whose primary tag is `LIST`/`BOXED` and whose address falls inside the arena
//!   must address a real object header there (raw payload is fine wherever the canonical
//!   child-slot walk does not visit it). A root the closure forgets makes its graph
//!   unreachable, and it is **silently dropped**: the arena is not a collector, it has no
//!   way to notice.
//!
//! This is the same discipline `beam_rt`'s `-eval` scratch store applies to its term
//! store, and the reason `beam_gc::FromSet::from_ranges` and
//! `beam_gc::LinearDest::new` are `unsafe`: their `# Safety` contracts are exactly these
//! assertions.
//!
//! # Memory: only `Drop` gives a region's bytes back
//!
//! `beam_alloc::ReservedHeap::reset` only moves the bump pointer back to the base — it
//! frees **nothing**. A region's whole reserved block (one anonymous, lazily-faulted
//! carrier `mmap`) is released in `ReservedHeap`'s `Drop` and nowhere else. Tier A's
//! `regions.truncate(..)`, Tier B's `regions.clear()`, and the arena itself being dropped
//! are therefore the only paths on which the resident set actually falls; Tier A's
//! *surviving* frontier keeps its carrier (only its bump pointer moved), which is why a
//! rewind inside one region releases address space but no RSS until the arena is dropped.
//!
//! # The new dependency edge (`beam_engine` → `beam_gc`)
//!
//! Tier B drives `beam_gc`'s published relocation API — `FromSet`, `LinearDest`,
//! `Relocator`, `Relocation` — and nothing else from that crate. The edge is
//! **directionally legal**: `beam_gc` is wave 1 and `beam_engine` is wave 4 in
//! `../CONTEXT.md`'s layering table, and `beam_gc` depends only on
//! `beam_common`/`beam_term`/`beam_alloc`/`libc` — it does not depend on `beam_engine`, so
//! the DAG stays acyclic (`cargo tree -p beam_engine` shows no cycle).
//!
//! # `unsafe`
//!
//! The crate root keeps `#![deny(unsafe_code)]`; this module carries its own
//! `#![allow(unsafe_code)]` because it is the arena's second raw-heap seam: it builds
//! `beam_gc::FromSet`/`LinearDest` over the arena's own reservations and advances a bump
//! pointer to a validated position (Tier B copies **straight into** the survivor — there
//! is no memcpy, see the destination note in `reclaim`). Every `unsafe` block states its
//! `// SAFETY:` rationale.

#![allow(unsafe_code)]

use beam_alloc::ReservedHeap;
use beam_common::{BeamError, Result};
use beam_gc::{FromSet, LinearDest, Relocator};

use super::{ContextHeap, HeapMark, MinimalContext};

/// What a [`MinimalContext::heap_rewind`] released, and what the arena holds afterwards.
///
/// Every field is a plain count; nothing here is sampled. See
/// [`ContextHeapStats`](super::ContextHeapStats) for the arena's whole census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapRewind {
    /// How many regions the rewind **dropped** — every region strictly newer than the one
    /// the mark names (each one's carrier is released by its `Drop`). Rewinding to
    /// [`HeapMark::EMPTY`] drops every region the arena held.
    pub regions_dropped: usize,
    /// How many words the rewind removed from the arena's live accounting: the dropped
    /// regions' `used` figures **plus** the words the surviving frontier had handed out
    /// above the mark. These are exactly the words added to
    /// [`released_words`](super::ContextHeapStats::released_words), which is why
    /// [`used_words`](super::ContextHeapStats::used_words) is unchanged by a rewind.
    pub words_released: usize,
    /// The surviving frontier's `used` figure after the rewind — the marked figure, i.e.
    /// the words the arena still holds ([`live_words`](super::ContextHeapStats::live_words)).
    pub live_words: usize,
    /// The arena's total reservation in `Eterm` words after the rewind (the surviving
    /// regions only; `0` after a rewind to [`HeapMark::EMPTY`]).
    pub reserved_words: usize,
    /// The arena's total committed slice in `Eterm` words after the rewind.
    pub committed_words: usize,
}

/// What a [`MinimalContext::reclaim_heap`] moved, and what the arena holds afterwards.
///
/// Every field is a plain count; nothing here is sampled. See
/// [`ContextHeapStats`](super::ContextHeapStats) for the arena's whole census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapReclaim {
    /// How many regions the previous arena held — **all** of them are dropped (each one's
    /// carrier is released by its `Drop`), and one fresh survivor replaces them (none when
    /// [`live_words`](HeapReclaim::live_words) is `0`).
    pub regions_dropped: usize,
    /// The words the dropped regions held (`Σ used` over the old arena) — the copy
    /// source's whole volume, of which only the live part was copied.
    pub source_words: usize,
    /// The words copied into the survivor: the live set the `forward` closure's roots
    /// reached, transitively.
    pub live_words: usize,
    /// `source_words - live_words` — the words the reclaim took back
    /// (`= relocator.stats.words_reclaimed`), added to
    /// [`released_words`](super::ContextHeapStats::released_words).
    pub words_reclaimed: usize,
    /// How many boxed objects and cons cells were copied.
    pub objects_copied: usize,
    /// The survivor's reservation in `Eterm` words (`0` when `live_words == 0`).
    pub reserved_words: usize,
    /// The survivor's committed slice in `Eterm` words (`0` when `live_words == 0`).
    pub committed_words: usize,
}

impl ContextHeap {
    /// The words the regions currently hold (`Σ used`) — the exact upper bound on what a
    /// reclaim has to copy, and the arena's
    /// [`live_words`](super::ContextHeapStats::live_words) figure.
    fn live_words(&self) -> usize {
        self.regions.iter().map(ReservedHeap::used).sum()
    }

    /// **Tier A**: move the frontier's bump pointer back to `mark` and drop every region
    /// newer than the marked one.
    ///
    /// See [`MinimalContext::heap_rewind`] for the full contract (this is its body).
    fn rewind(&mut self, mark: HeapMark) -> Result<HeapRewind> {
        // ---- validate first: a refusal must leave the arena byte-identical ----
        if mark.epoch != self.epoch {
            // A mark from before a rewind/reclaim (or from another arena, whose epoch
            // number carries no meaning here): the words above it are already gone, so
            // accepting it could discard words written afterwards.
            return Err(BeamError::BadArg);
        }
        // `(regions_dropped, words_above_the_mark)` for the two shapes of a valid mark.
        let (regions_dropped, frontier_words_above) = match mark.region {
            None => {
                // An epoch-matching EMPTY mark is the arena-reset door: it names "no
                // region", so it is only valid while the arena holds none... except that
                // the reset form deliberately accepts a populated arena and drops it all
                // (see `MinimalContext::heap_rewind`).
                (self.regions.len(), 0)
            }
            Some(index) => {
                let Some(region) = self.regions.get(index) else {
                    // A region index this arena never reached.
                    return Err(BeamError::BadArg);
                };
                if region.base() as usize != mark.base {
                    // The region at that index is not the one the mark was taken in (it
                    // was replaced), or the mark belongs to another arena.
                    return Err(BeamError::BadArg);
                }
                if mark.used > region.used() {
                    // A forward mark: it claims words this region never handed out.
                    return Err(BeamError::BadArg);
                }
                // `used <= committed` is a `ReservedHeap` invariant, so this cannot
                // actually refuse; it states the precondition the bump-pointer advance
                // below relies on, and turns a broken invariant into a typed refusal
                // *before* anything mutates rather than after.
                if mark.used > region.committed_words() {
                    return Err(BeamError::BadArg);
                }
                (
                    self.regions.len() - (index + 1),
                    region.used() - mark.used(),
                )
            }
        };

        // ---- mutate: nothing below can refuse ----
        let dropped_words: usize = match mark.region {
            None => {
                let words = self.live_words();
                self.regions.clear();
                words
            }
            Some(index) => {
                let dropped: usize = self.regions[index + 1..]
                    .iter()
                    .map(ReservedHeap::used)
                    .sum();
                self.regions.truncate(index + 1);
                // The frontier survives with its carrier, its reservation, its committed
                // limit and its peak; only the bump pointer moves back.
                let frontier = self
                    .regions
                    .last_mut()
                    .expect("the validated region index leaves at least one region");
                frontier.reset();
                // SAFETY: `reset` put the bump pointer back at the region's base and left
                // the committed limit untouched, and the validation above proved
                // `mark.used <= committed`, so `mark.used` words are available inside the
                // exclusively-owned committed slice. The words are not written here (they
                // still hold whatever the discarded objects left, which the next
                // allocation overwrites) — `alloc_words` only advances the bump pointer.
                unsafe {
                    frontier.alloc_words(mark.used)?;
                }
                dropped
            }
        };
        self.epoch += 1;
        let words_released = dropped_words + frontier_words_above;
        self.released_words += words_released;

        let stats = self.stats();
        Ok(HeapRewind {
            regions_dropped,
            words_released,
            live_words: stats.live_words,
            reserved_words: stats.reserved_words,
            committed_words: stats.committed_words,
        })
    }

    /// **Tier B**: copy the live term graph into one fresh region (through `beam_gc`'s
    /// relocator, over the roots the `forward` closure supplies) and drop every old
    /// region.
    ///
    /// See [`MinimalContext::reclaim_heap`] for the full contract (this is its body).
    fn reclaim(
        &mut self,
        dest_capacity_words: usize,
        mut forward: impl FnMut(&mut Relocator<'_>) -> Result<()>,
    ) -> Result<Option<HeapReclaim>> {
        let source_words = self.live_words();
        if source_words == 0 {
            // Nothing was ever handed out (or a previous reclaim left the arena empty):
            // there is no word to copy and no region to drop.
            return Ok(None);
        }
        if self.regions.len() == 1 && self.regions[0].used() == self.regions[0].reserved_words() {
            // One region, and its used figure already fills its whole reservation: no
            // reservation can shrink and no word can be reclaimed.
            return Ok(None);
        }

        // ---- the copy source: the arena's own written prefixes ----
        let ranges: Vec<(usize, usize)> = self
            .regions
            .iter()
            .map(|region| (region.base() as usize, region.used()))
            .collect();
        // SAFETY: every entry is one of this arena's own reservations, which the arena
        // owns exclusively (`&mut self` rules out any concurrent mutation), whose `base()`
        // is word-aligned by the allocator, whose `used()` is a real prefix of the block it
        // handed out (`used <= committed <= reserved`, advanced only by the bump pointer),
        // and which are pairwise disjoint because each is a distinct allocation. The
        // copier's completeness claim is the caller's: the roots passed to `forward` must be
        // the whole live set (documented on `MinimalContext::reclaim_heap`).
        let from = unsafe { FromSet::from_ranges(&ranges) };

        // ---- the destination is the SURVIVOR ITSELF ----
        //
        // The relocator rewrites every root **and every child slot of every copy** to the
        // address the copy was deposited at. Those addresses are `beam_gc`'s marker protocol
        // and it knows nothing about a later move, so a copy made in a transient scratch and
        // then installed with `copy_nonoverlapping` would leave every interior pointer aiming
        // at the scratch's (freed) words — the survivor would hold a self-describing graph
        // whose children dangle. The copies must therefore be deposited where they will live,
        // which is why the region below starts life as the destination and becomes the
        // survivor; the only bookkeeping left after the copy is advancing its bump pointer to
        // the words it holds.
        //
        // The price is that the survivor is reserved at the copy **bound**
        // (`dest_capacity_words`), not at the live volume: a right-sized survivor would need
        // either a second relocation of the live set (to re-forward the interior pointers the
        // first one baked in) or a destination that may refuse **mid-copy**, which is exactly
        // the torn state the exact-size preflight exists to rule out. One copy of the live set
        // wins over saving lazily-faulted address space.
        let mut survivor = ReservedHeap::new(dest_capacity_words, dest_capacity_words)?;
        // SAFETY: `survivor` owns exactly `dest_capacity_words` word-aligned, writable words
        // (its whole reservation, committed in full at construction), exclusively — it is a
        // local moved only into the arena after the relocator is done — and it is a distinct
        // `beam_alloc` carrier, so it cannot overlap any from-range (asserted in debug builds
        // by `LinearDest::new`). It is never moved while the relocator borrows it, which is
        // why the copies keep their addresses, and `dest_capacity_words` is its true capacity:
        // the figure `reserve` bounds every write by.
        let mut dest = unsafe { LinearDest::new(&from, survivor.base(), dest_capacity_words) };
        // The exact-size preflight. `source_words` is an upper bound on the copy volume (each
        // source word is copied at most once), so this refuses **before the first copy**
        // whenever the destination cannot possibly hold the live set; `can_reserve` and
        // `reserve` are the same test for `LinearDest`, so a mid-copy `heap_exhausted` is
        // unreachable once this passes.
        let mut reloc = match Relocator::new_checked(&from, &mut dest, source_words) {
            Ok(reloc) => reloc,
            Err(err) => {
                // Nothing was mutated: no forwarding marker was written, no root was
                // rewritten, and the untouched `survivor` is dropped with this frame. The
                // arena is byte-identical.
                return Err(err);
            }
        };

        // From here on the source is mutated (forwarding markers), so any failure is
        // unrecoverable: the arena's words are abandoned, never merely left behind.
        if let Err(err) = forward(&mut reloc) {
            self.abandon();
            return Err(err);
        }
        let relocation = match reloc.complete() {
            Ok(relocation) => relocation,
            Err(err) => {
                self.abandon();
                return Err(err);
            }
        };
        let live = relocation.dest_words_used;

        // Advance the survivor's bump pointer to exactly the words the relocator reserved —
        // nothing is written here (the copies are already in place), and the whole reservation
        // is committed, so this cannot refuse.
        if let Err(err) = unsafe { survivor.alloc_words(live) } {
            self.abandon();
            return Err(err);
        }
        let (reserved_words, committed_words) = if live == 0 {
            (0, 0)
        } else {
            (survivor.reserved_words(), survivor.committed_words())
        };

        // Dropping every old region is what makes the arena's resident set actually fall
        // (`reset` would free nothing; `Drop` is the only path that releases a carrier).
        let regions_dropped = self.regions.len();
        self.regions.clear();
        if live > 0 {
            self.regions.push(survivor);
        }
        self.epoch += 1;
        self.released_words += source_words - live;

        Ok(Some(HeapReclaim {
            regions_dropped,
            source_words,
            live_words: live,
            words_reclaimed: source_words - live,
            objects_copied: relocation.stats.objects_copied,
            reserved_words,
            committed_words,
        }))
    }

    /// Leave the arena in its documented **empty** state after an unrecoverable mid-copy
    /// failure, keeping the cumulative-demand figure continuous.
    ///
    /// The copy source has forwarding markers written into it and the copies live in a
    /// scratch reservation that is about to be dropped, so **every** word the arena held —
    /// and every root the caller forwarded — now names freed memory. The host must abandon
    /// both; the arena keeps only honest bookkeeping.
    fn abandon(&mut self) {
        let released = self.live_words();
        self.regions.clear();
        self.epoch += 1;
        self.released_words += released;
    }
}

impl MinimalContext {
    /// **Tier A — rewind the arena to a [`HeapMark`]**, dropping every region newer than
    /// the marked one and moving the surviving frontier's bump pointer back to the marked
    /// `used` figure.
    ///
    /// This is the cheap tier: it reads **no word** (`O(regions dropped)` bookkeeping plus
    /// one bump-pointer move), it never copies, and it never fails on a valid mark.
    ///
    /// # The caller's contract (this is what makes it sound)
    ///
    /// **The caller asserts that no reachable term lies at or above `mark`** — neither in
    /// the frontier above the marked `used` figure nor in any region the rewind drops.
    /// Every such word then either becomes unreachable or is garbage, and after the rewind
    /// it may be overwritten by a later allocation. Concretely, before calling:
    ///
    /// * every root the caller still holds (a `Vec<Eterm>`, a machine's `RootSet` via
    ///   [`Machine::for_each_root`](crate::machine::Machine::for_each_root), a term it is
    ///   about to hand back) must address words **below** the mark or outside the arena;
    /// * nothing may hold a raw `*mut Eterm` into the dropped words afterwards.
    ///
    /// The arena cannot check that claim (it cannot tell a term word from raw payload), so
    /// a violated contract is a **use-after-free**, exactly as in `beam_alloc::Heap::reset`.
    /// The supporting evidence is [`MinimalContext::for_each_heap_region`] (walk every word
    /// the arena holds) and [`MinimalContext::heap_mark_matches`] (the structural
    /// precondition).
    ///
    /// # Validation and refusal
    ///
    /// The rewind is refused with the typed [`BeamError::BadArg`] — and the arena left
    /// **byte-identical** — when the mark is
    ///
    /// * **pre-reclaim**: taken before an earlier rewind or reclaim (its epoch differs);
    /// * **stale**: it names a region index the arena no longer has, or a region whose base
    ///   address differs (the region it was taken in was replaced, or the mark belongs to a
    ///   different arena);
    /// * **forward**: it claims more words than the marked region handed out.
    ///
    /// # [`HeapMark::EMPTY`] is the arena-reset door
    ///
    /// An **epoch-matching** empty mark names "no region", so rewinding to it drops **every**
    /// region the arena holds (`regions_dropped == stats().regions`, `live_words == 0`),
    /// releasing all of their carriers. Because `EMPTY`'s epoch is `0` — a fresh arena's —
    /// it is accepted only until the arena's first rewind/reclaim; afterwards a caller that
    /// wants to empty the arena takes a fresh `heap_mark()` and rewinds to that (or reclaims).
    ///
    /// A rewind inside the frontier region keeps that region's carrier, reservation,
    /// committed limit and peak: only its bump pointer moved, so no RSS is released there
    /// (`ReservedHeap::reset` frees nothing). Dropping a **later** region does release its
    /// carrier, because `Drop` is the only path that frees one.
    pub fn heap_rewind(&mut self, mark: HeapMark) -> Result<HeapRewind> {
        self.heap.rewind(mark)
    }

    /// **Tier B — copy the live term graph into one fresh region** and drop every old
    /// region, collapsing the arena to a single survivor.
    ///
    /// Use this when Tier A's "nothing above the mark" claim does not hold (live terms are
    /// interleaved with garbage, or they live in several regions): the relocator visits only
    /// what the roots reach (a Cheney copy), so garbage is left behind and the arena's
    /// resident set really falls — every old region's carrier is released by its `Drop`.
    ///
    /// # The caller's contract (this is what makes it sound)
    ///
    /// `forward` must hand **every** live root to the relocator, typically as
    /// `|reloc| reloc.forward_roots(&mut roots)` — possibly several times, once per root
    /// slice, since `forward_roots` is idempotent — and the relocator then rewrites each of
    /// them in place to the survivor's address:
    ///
    /// * a root the closure forgets makes its whole graph unreachable, and it is **dropped
    ///   silently** (the arena is not a collector: it only follows what it is handed);
    /// * every word inside the arena must be part of a well-formed term structure wherever
    ///   the copier walks it — a word whose primary tag is `LIST`/`BOXED` and whose address
    ///   falls inside the arena must address a real object header there (raw payload is fine
    ///   wherever the canonical child-slot walk does not visit it);
    /// * the roots the closure is given must outlive the call (they are rewritten in place)
    ///   and must not be read through a stale pointer copied before it.
    ///
    /// The provenance of the copy: `beam_alloc`'s distinct reservations, so the destination
    /// never overlaps a source range. The copies are deposited **straight into the survivor
    /// region** (which starts life as the destination), because the relocator rewrites each
    /// copied child slot to the address it deposited the child at: a copy made in a scratch
    /// region and then moved would leave every interior pointer naming the scratch. One
    /// consequence a caller must know: the survivor is reserved at the copy **bound**, not at
    /// the live volume, so [`HeapReclaim::reserved_words`] reports the bound rather than a
    /// right-sized figure (the resident set still falls — a reservation is lazily faulted and
    /// every old region's carrier is released).    ///
    /// # Destination sizing and refusal
    ///
    /// This door sizes the destination itself: `Σ regions[i].used()` — an exact upper bound
    /// on the copy volume, since each source word is copied at most once. A reclaim therefore
    /// never refuses for want of capacity (a genuinely huge arena can still fail with the
    /// typed [`BeamError::Alloc`] from `beam_alloc` when the survivor is reserved). Use
    /// [`reclaim_heap_with_capacity`](MinimalContext::reclaim_heap_with_capacity) to set the
    /// bound yourself.
    ///
    /// # [`None`] — nothing worth doing
    ///
    /// `Ok(None)` (and **no** state change, **no** epoch bump) when
    ///
    /// * the arena holds no written word (`Σ used == 0`), or
    /// * it holds a single region whose `used` already fills its whole **reservation** —
    ///   there is no word to reclaim and no reservation that could shrink.
    ///
    /// # Failure semantics
    ///
    /// * A destination too small is the typed `heap_exhausted` ([`BeamError::Alloc`])
    ///   decided **before the first copy**, with the arena byte-identical.
    /// * An `Err` from `forward` itself, or an unexpected mid-copy refusal, is
    ///   **unrecoverable** (`beam_gc`'s documented semantics: the source is left partially
    ///   forwarded). It is propagated, and the arena is left in its defined **empty** state
    ///   (every region dropped, the epoch bumped) rather than holding forwarded marker words
    ///   — the host must then abandon the arena's words **and its own roots**, because they
    ///   name freed memory.
    pub fn reclaim_heap(
        &mut self,
        forward: impl FnMut(&mut Relocator<'_>) -> Result<()>,
    ) -> Result<Option<HeapReclaim>> {
        let bound = self.heap.live_words();
        self.heap.reclaim(bound, forward)
    }

    /// As [`reclaim_heap`](MinimalContext::reclaim_heap), with the destination's capacity
    /// chosen by the caller (`dest_capacity_words`).
    ///
    /// This is the raw door that makes the pre-copy refusal a real, testable path: a
    /// `dest_capacity_words` below the live volume is the typed `heap_exhausted`
    /// ([`BeamError::Alloc`]) from `Relocator::new_checked`, decided **before any copy**,
    /// with the arena byte-identical (no forwarding marker written, no root rewritten).
    ///
    /// The preflight compares against the exact **upper bound** `Σ regions[i].used()`, not
    /// against the (unknowable before the roots arrive) live volume, so a capacity between
    /// the live volume and the total source volume is refused too — conservatively, and for
    /// the reason that matters: refusing up front instead of failing mid-copy. Pass
    /// [`reclaim_heap`](MinimalContext::reclaim_heap) when you have no independent
    /// live-volume bound.
    ///
    /// On success `dest_capacity_words` is also the survivor region's **reservation** (the
    /// copies are deposited straight into the survivor, so it is the destination), which is
    /// why the preflight must bound the copy volume exactly: too small a figure would either
    /// refuse (below `Σ used`) or tear a copy in progress. The survivor's committed slice
    /// equals its reservation, so the next allocations never have to grow it.
    pub fn reclaim_heap_with_capacity(
        &mut self,
        dest_capacity_words: usize,
        forward: impl FnMut(&mut Relocator<'_>) -> Result<()>,
    ) -> Result<Option<HeapReclaim>> {
        self.heap.reclaim(dest_capacity_words, forward)
    }
}

#[cfg(test)]
mod tests;
