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

//! The mark-region collection phases of [`ImmixSpace`]: mark, classify, release,
//! evacuate, finalize — plus the [`EvacDest`] implementation that lets the shared
//! word-level forwarding protocol of `copy.rs` drive an evacuation *into* this space.
//!
//! This is a child module of `immix`, so it sees `ImmixSpace`'s private fields and
//! geometry helpers without widening the crate's public surface.
//!
//! # Why the forwarding-marker protocol is safe here
//!
//! The copying collector overwrites a from-space object's header (or a cons cell's car)
//! with a forward marker, so a second reference to the same object follows the marker
//! instead of copying it again. That requires the marker's memory never to be handed out
//! again while a reference to the original address can still be followed. Here the
//! invariant is structural rather than positional:
//!
//! * a marker is written **only** into a *marked* line, and
//! * a destination is taken **only** from an *unallocated* line,
//!
//! and after [`release_dead_lines`](ImmixSpace::release_dead_lines) the allocated lines
//! are exactly the marked ones. A marker can therefore never be overwritten, and no
//! ordering of the evacuation matters (unlike an address-ordered in-place compactor).
//! The lines an evacuated object leaves behind are released only in
//! [`finalize`](ImmixSpace::finalize), *after* every reference to them has been followed —
//! reusing a source line mid-collection would clobber its marker.

use beam_common::Result;
use beam_term::prelude::{
    boxed_val, is_header, is_non_value, list_val, make_boxed, make_list, primary_tag, ptr_val,
    Eterm, PrimaryTag,
};

use crate::copy::{for_each_ref, forward_slot, move_size, EvacDest, Target};

use super::{lines_for, BlockState, ImmixSpace, ImmixStats};

/// Appends `word` to the worklist when it is a reference (an immediate or a bare header
/// has nothing to mark).
fn push_candidate(work: &mut Vec<Eterm>, word: Eterm) {
    if matches!(primary_tag(word), PrimaryTag::Boxed | PrimaryTag::List) {
        work.push(word);
    }
}

/// Queues every reference word of the live object at `ptr` for forwarding.
///
/// A cons cell (`is_cons`) is `[CAR, CDR]` with no header, so both of its words are
/// references; any other object is walked with the shared per-object enumerator, which
/// skips a fun's inlined descriptor, steps transparencies word by word and follows an
/// `ErlSubBits`'s `orig`.
///
/// # Safety
///
/// `ptr` must address a live object of `total` words (2 for a cons cell), all readable.
unsafe fn queue_refs(
    work: &mut Vec<*mut Eterm>,
    ptr: *mut Eterm,
    is_cons: bool,
    total: usize,
) -> Result<()> {
    if is_cons {
        // SAFETY: a cons cell is exactly two reference words.
        unsafe {
            work.push(ptr);
            work.push(ptr.add(1));
        }
        return Ok(());
    }
    // SAFETY: forwarded to the caller's guarantee.
    unsafe { for_each_ref(ptr, total, &mut |slot| Ok(work.push(slot))) }
}

/// The [`EvacDest`] decision for a mark-region space: evacuate the marked objects of
/// `Partial` blocks into free lines, and leave everything else — a pinned `AllMarked`
/// block, an out-of-space reference, a literal, a fresh destination — exactly as it is.
///
/// This is what makes the space usable both as the destination of an evacuation (this
/// implementation is the whole of it) and as the source (by [`ImmixSpace::collect`]).
impl EvacDest for ImmixSpace {
    unsafe fn classify_boxed(&self, ptr: *const Eterm) -> Target {
        self.classify_ref(ptr, true)
    }

    unsafe fn classify_cons(&self, ptr: *const Eterm) -> Target {
        self.classify_ref(ptr, false)
    }

    fn can_reserve(&self, words: usize) -> bool {
        ImmixSpace::can_reserve(self, words)
    }

    unsafe fn reserve(&mut self, words: usize) -> Result<*mut Eterm> {
        self.reserve_for_copy(words)
    }
}

impl ImmixSpace {
    /// Classifies the object named by a reference word.
    ///
    /// `Target::Foreign` covers five different reasons to leave a reference alone, and in
    /// every one of them that is the correct answer: the pointer is outside the region (a
    /// literal, an off-heap MSO/binary, another space), it falls in an unallocated line
    /// (memory this collection already reclaimed), it falls in an unmarked line (a copy
    /// this collection just made — already in its final position), its block is
    /// `AllMarked` (pinned by the full-block rule), or its line is in the `kept` bitmap.
    ///
    /// The `kept` test is what makes the *hole* pinning rule of
    /// [`ImmixSpace::evacuate`] sound: an object that no hole could hold is retained in
    /// place by [`ImmixSpace::add_kept`], so a reference to it must be left verbatim —
    /// otherwise the fix-up pass would try to copy it a second time, find the same absence
    /// of a hole, and fail an otherwise-satisfiable collection. A `kept` line cannot also
    /// be a *to-be-evacuated* source: destinations are always unmarked holes, while every
    /// object still pending evacuation sits on a marked line, and a live line is never a
    /// destination. So a `kept` line is either a pinned source or an already-written
    /// destination — both final positions — and treating it as `Foreign` is exact for
    /// multi-line objects and cons cells alike (a reference always names the object's
    /// first line, which is the line `add_kept` set).
    fn classify_ref(&self, ptr: *const Eterm, boxed: bool) -> Target {
        let Some((b, l)) = self.locate(ptr) else {
            return Target::Foreign;
        };
        if !self.line_allocated(b, l)
            || !self.line_marked(b, l)
            || self.block_state(b) == BlockState::AllMarked
            || self.line_kept(b, l)
        {
            return Target::Foreign;
        }
        // SAFETY: the location test established that `ptr` is inside this space's own
        // region, and a marked line is an object's header word (line-granular
        // allocation), so the first word is this space's memory and is initialised.
        let first = unsafe { *ptr };
        if boxed {
            // `IS_MOVED_BOXED`: a moved object's header word was overwritten with the new
            // BOXED-tagged pointer, so it is no longer a header.
            if is_header(first) {
                Target::Live
            } else {
                Target::Forwarded
            }
        } else {
            // `IS_MOVED_CONS`: a moved cons cell's car was set to `THE_NON_VALUE`.
            if is_non_value(first) {
                Target::Forwarded
            } else {
                Target::Live
            }
        }
    }

    /// Marks every line covered by a live object reachable from the flat root slice
    /// `roots` plus the incoming live set `promoted`, then classifies every block.
    ///
    /// `roots` is the crate's root-set contract: `x_reg_array[0..Live)` plus the whole
    /// stack, with no frame liveness maps — word by word, a word is a candidate iff its
    /// primary tag is BOXED or LIST and it lands on an allocated line of this space (the C
    /// `erl_gc.c:1677-1717` rule). `promoted` is the incoming set of nursery objects the
    /// caller has promoted into this space: they are treated as additional roots. An
    /// entry that still points outside the region is left alone — copying a nursery object
    /// *into* the mature space is a promotion driver's job (this type is an
    /// [`EvacDest`], so such a driver can evacuate straight into it), not the mark
    /// phase's.
    ///
    /// The traversal uses an explicit worklist and never recurses into the term graph, so
    /// a deep list cannot grow the real stack.
    pub fn mark(&self, roots: &[Eterm], promoted: &[Eterm]) -> Result<()> {
        self.clear_cycle_bits();
        let mut work: Vec<Eterm> = Vec::new();
        for word in roots.iter().chain(promoted.iter()) {
            push_candidate(&mut work, *word);
        }
        while let Some(word) = work.pop() {
            self.mark_object(word, &mut work)?;
        }
        self.classify();
        Ok(())
    }

    /// Marks the object `word` names and pushes its references onto `work`.
    ///
    /// The object's **first line** is both the mark and the visited test: because
    /// allocation is line-granular, a line belongs to at most one object, so "the line
    /// that starts this object is already marked" means "this object is already done".
    fn mark_object(&self, word: Eterm, work: &mut Vec<Eterm>) -> Result<()> {
        match primary_tag(word) {
            PrimaryTag::Boxed => {
                let ptr = boxed_val(word);
                let Some(start_g) = self.object_start(ptr) else {
                    return Ok(());
                };
                // SAFETY: `object_start` established that `ptr` is the header word of an
                // allocated, not-yet-visited line start — a live object of this space.
                let total = move_size(unsafe { *ptr }, ptr);
                self.add_marked(start_g, lines_for(total));
                // SAFETY: `ptr` addresses a live boxed object of `total` words.
                unsafe {
                    for_each_ref(ptr, total, &mut |slot| {
                        // `for_each_ref` only yields addresses of reference words inside
                        // the live object, so the dereference is already covered by the
                        // enclosing `unsafe` block.
                        push_candidate(work, *slot);
                        Ok(())
                    })
                }
            }
            PrimaryTag::List => {
                let ptr = list_val(word);
                let Some(start_g) = self.object_start(ptr) else {
                    return Ok(());
                };
                self.add_marked(start_g, lines_for(2));
                // SAFETY: a live cons cell is two readable words, both references.
                unsafe {
                    push_candidate(work, *ptr);
                    push_candidate(work, *ptr.add(1));
                }
                Ok(())
            }
            // A bare header is not a reference; an immediate holds no object.
            PrimaryTag::Header | PrimaryTag::Immed1 => Ok(()),
        }
    }

    /// The global line the object `ptr` starts at, or `None` when `ptr` is not an
    /// allocated, not-yet-visited line start (outside the region, reclaimed memory, a
    /// continuation line, or an object already marked).
    fn object_start(&self, ptr: *const Eterm) -> Option<usize> {
        let (b, l) = self.locate(ptr)?;
        if !self.line_allocated(b, l) || self.line_marked(b, l) {
            return None;
        }
        Some(b * super::LINES_PER_BLOCK + l)
    }

    /// Drops the previous cycle's mark and `kept` bits.
    fn clear_cycle_bits(&self) {
        for b in &self.blocks {
            b.marked.set(0);
            b.kept.set(0);
        }
    }

    /// Classifies every block from the marks:
    /// [`Free`](BlockState::Free) / [`Partial`](BlockState::Partial) /
    /// [`AllMarked`](BlockState::AllMarked).
    fn classify(&self) {
        for b in &self.blocks {
            let marked = b.marked.get();
            b.state.set(if marked == 0 {
                BlockState::Free
            } else if marked == super::ALL_LINES {
                BlockState::AllMarked
            } else {
                BlockState::Partial
            });
            b.cursor.set(0);
        }
    }

    /// Releases every line that is **not** marked, so the holes a collection can fill are
    /// exactly the unmarked lines — which is what guarantees a destination never overlaps
    /// a live object (and therefore never a forward marker).
    fn release_dead_lines(&mut self) {
        for b in &self.blocks {
            b.allocated.set(b.marked.get());
            b.cursor.set(0);
        }
        self.current = super::NO_BLOCK;
    }

    /// The retreat of the space to "unclaimed": every line that the running collection
    /// decided to keep (`kept`) survives; every other line is free again. `occupied_before`
    /// is the pre-release occupancy of each block, so a block that held only garbage is
    /// counted as reclaimed and a block that was already free is not. Returns how many
    /// previously occupied blocks this reclaimed outright.
    ///
    /// After this the mutator can allocate exactly as before — including into every hole
    /// the collection left.
    fn finalize(&mut self, occupied_before: &[bool]) -> usize {
        let mut reclaimed = 0;
        for (b, &was_occupied) in self.blocks.iter().zip(occupied_before) {
            let kept = b.kept.get();
            b.allocated.set(kept);
            b.state.set(super::state_of(kept));
            b.marked.set(0);
            b.kept.set(0);
            b.cursor.set(0);
            if was_occupied && kept == 0 {
                reclaimed += 1;
            }
        }
        self.current = super::NO_BLOCK;
        reclaimed
    }

    /// Runs one stop-the-world mark-region collection.
    ///
    /// The phases are: **mark** (`roots` + `promoted`), **classify** the blocks,
    /// **release** the unmarked lines, **evacuate** the `Partial` blocks (pinning every
    /// object whose block is full or that no hole can hold) and **finalize**. `roots` and
    /// `promoted` are rewritten in place, exactly like the copying collector's roots.
    pub fn collect(&mut self, roots: &mut [Eterm], promoted: &mut [Eterm]) -> Result<ImmixStats> {
        self.mark(roots, promoted)?;

        let lines_before = self.used_lines();
        let mut occupied_before: Vec<bool> = Vec::with_capacity(self.blocks.len());
        for b in &self.blocks {
            occupied_before.push(b.allocated.get() != 0);
        }
        self.release_dead_lines();

        let mut stats = self.evacuate(roots, promoted)?;
        stats.blocks_reclaimed = self.finalize(&occupied_before);
        stats.words_reclaimed = lines_before
            .saturating_sub(self.used_lines())
            .saturating_mul(super::LINE_WORDS);
        self.collections += 1;
        Ok(stats)
    }

    /// Walks the whole region in global-line order and evacuates (or pins) every live
    /// object, then rewrites `roots`/`promoted`.
    ///
    /// The walk is driven by the mark bitmap and by the object sizes, so a large object
    /// that spans several blocks is processed once, from its start, and every
    /// line-granular object start is visited exactly once. Objects are copied through the
    /// shared marker protocol (`forward_slot`), whose `Live` decision comes from this
    /// type's [`EvacDest`] implementation. References *inside* a copy or a pinned object
    /// are queued on a worklist and forwarded after the walk.
    fn evacuate(&mut self, roots: &mut [Eterm], promoted: &mut [Eterm]) -> Result<ImmixStats> {
        let mut stats = ImmixStats::default();
        let mut work: Vec<*mut Eterm> = Vec::new();
        let n_lines = self.line_count();
        let mut g = 0usize;

        while g < n_lines {
            let (b, l) = (g / super::LINES_PER_BLOCK, g % super::LINES_PER_BLOCK);
            if !self.line_marked(b, l) {
                g += 1;
                continue;
            }
            let ptr = self.base().wrapping_add(g * super::LINE_WORDS);
            // SAFETY: a marked line is an object's line-aligned start (line-granular
            // allocation), so it holds a live word of this space.
            let first = unsafe { *ptr };
            // A cons cell has no header — its first word is the car, which is a *term*,
            // and no term carries the `Header` primary tag (it is the "not a value"
            // pattern of the frozen layout, `beam_term::tag::is_header`). So the tag of
            // the first word says which shape this line starts, with no extra metadata.
            let is_cons = !is_header(first);
            let (total, span) = if is_cons {
                (2, 1)
            } else {
                let total = move_size(first, ptr);
                (total, lines_for(total).min(n_lines - g).max(1))
            };
            debug_assert!(
                span <= super::LINES_PER_BLOCK - l || self.block_state(b) == BlockState::AllMarked,
                "an object that spans blocks must start in a block it fills completely"
            );

            if self.block_state(b) == BlockState::AllMarked || !self.can_reserve(total) {
                // Pinned: a full block, or no hole (and no block run) can hold it. Its
                // lines are retained and its references still have to be forwarded.
                self.add_kept(g, span);
                // SAFETY: `ptr` addresses a live object of `total` words.
                unsafe { queue_refs(&mut work, ptr, is_cons, total)? };
                stats.objects_pinned += 1;
                stats.lines_pinned += span;
                stats.words_pinned += total;
            } else {
                // Evacuate through the shared marker protocol. The slot is a temporary
                // word holding a reference to the object itself, tagged with the object's
                // own shape (a cons cell is a `List`, everything else is `Boxed`), so the
                // copy and the marker are written exactly as the copying collector writes
                // them; after the call the slot names the copy.
                let mut slot = if is_cons {
                    make_list(ptr as *const Eterm)
                } else {
                    make_boxed(ptr as *const Eterm)
                };
                // SAFETY: `slot` is a live word holding a reference to a live object of
                // this space, and `can_reserve` just confirmed room for its copy.
                unsafe { forward_slot(self, &mut slot)? };
                // SAFETY: `slot` now names the freshly written copy of `total` words (a
                // `Live` classification is the only one `can_reserve` allowed).
                let dst = ptr_val(slot);
                debug_assert_ne!(dst, ptr, "an evacuated object moves");
                // SAFETY: `dst` addresses the live copy of `total` words.
                unsafe { queue_refs(&mut work, dst, is_cons, total)? };
                stats.objects_evacuated += 1;
                stats.lines_evacuated += span;
            }

            g += span;
        }

        // Forward the references *inside* copies and pinned objects. Each queued slot is
        // processed once; a slot that copies an object queues the copy's own references,
        // so the worklist reaches every object the mark found.
        while let Some(slot) = work.pop() {
            // SAFETY: every queued address is a reference word inside a live object — a
            // copy made by this collection, or an object this collection pinned.
            let copied = unsafe { forward_slot(self, slot)? };
            if copied > 0 {
                // SAFETY: `forward_slot` just rewrote `slot` to name the copy it made.
                let name = unsafe { *slot };
                let dst = beam_term::prelude::ptr_val(name);
                // A copied cons cell keeps the `List` tag; anything else is boxed.
                let is_cons = matches!(primary_tag(name), PrimaryTag::List);
                // SAFETY: `dst` addresses the fresh copy of `copied` words.
                unsafe { queue_refs(&mut work, dst, is_cons, copied)? };
            }
        }

        // Roots last: by now every marked object is in its final position, so this only
        // follows markers into the new addresses.
        for root in roots.iter_mut().chain(promoted.iter_mut()) {
            // SAFETY: `root` is an exclusively borrowed, live term word of the caller's
            // root slice.
            unsafe { forward_slot(self, root as *mut Eterm)? };
        }

        Ok(stats)
    }
}
