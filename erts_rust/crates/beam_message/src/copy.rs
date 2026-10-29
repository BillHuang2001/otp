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

//! The copy engine: `copy_struct`, the iterative copy of a term graph between two process heaps.
//!
//! The copier is *iterative* — it drives an explicit worklist of pending fix-ups, so a deep list
//! never recurses on the native stack — and mirrors the C `copy.c` `copy_struct_x` sharing policy:
//! **immediates** and **literal-area** compound terms are returned as-is (the literal bit means
//! the term is permanent and heap-independent), everything else (cons cells, tuples/records, funs,
//! flatmaps) is copied, and an **off-heap reference** (an MSO container or a refc binary) is
//! refcounted into the destination table rather than deep-copied. A `SUB_BITS` **match context**
//! (an `ErlSubBits` over an on-heap bitstring) is instead *flattened* into a fresh on-heap
//! bitstring, so the copy never aliases the source heap, while a **refc binary** shares its
//! off-heap payload ([`copy_refc_binary`]) — or, when no held handle owns that payload, is
//! flattened by the very same routine.
//!
//! Fidelity scope: this is a correct, *tree* copy — a subterm occurring twice is copied twice
//! (C's default behaviour; the sharing-preserving copier is [`crate::copy_struct_shared`]). It
//! allocates each object incrementally from the destination heap (parent before child, so the
//! outer-before-inner ordering invariant of the C `htop`/`hbot` layout still holds) rather than
//! pre-sizing a single block.
//!
//! `unsafe` is confined to the raw word read/write of the copy loop; every `unsafe` block carries
//! a `// SAFETY:` rationale.
//!
//! # One traversal whenever the destination's off-heap table is empty (the single-pass fast path)
//!
//! The copier's sized shape is **two traversals**: [`preflight_words`] prices the whole graph in one
//! walk, then [`shallow`] + the worklist loop write it. A destination whose **off-heap table** holds
//! nothing in any of its three lists (no MSO handle, no refc binary, no node reference) has no
//! partially-built state a failed attempt could not wholly roll back, and is the shape the common
//! case actually has (a fresh [`HeapFragment`](crate::HeapFragment) region, and — because the staging
//! table is per-send and always fresh — every receiver heap a real `send` copies into), so for it the
//! copier runs the writing traversal **alone**: [`try_copy_fast`] copies through a [`TrialTarget`] —
//! a bump pointer over the destination heap's own *unwritten* words — and commits with a single
//! `alloc_words`. A copy that turns out not to fit is abandoned and the preflight path below
//! re-derives the same typed outcome, so nothing about the error contract (`AllocKind::Message`
//! exhaustion, `BadArg` for a malformed/cyclic graph, send atomicity) changes. Because the trial
//! never advances the destination's `used`/`peak`, a refused copy has provably written nothing at all
//! — the strongest form of the crate's "no partial write on refusal" rule, and the one
//! `tests/sizing.rs` pins.
//!
//! A destination heap that is merely **dirty** (`used() > 0`) is *not* excluded: the trial only ever
//! writes into the heap's unused tail and commits once, so a failure needs no heap rewind at all.
//! Only a **non-empty off-heap table** keeps the sized path, because rolling *that* back would
//! discard entries that are not this copy's to release.
//!
//! # The sizing walk is `beam_term::size`'s policy-parameterised walk, on a warm per-thread scratch
//!
//! [`preflight_words`] counts what [`shallow`]/`copy_refc_binary` will **write into the
//! destination** — the C's `size_object_litopt(obj, area)`: the `litopt != NULL` half of
//! `size_object_x` (`erts/emulator/beam/copy.c:105,119`), the form the **message-heap charge** uses
//! (`global.h:1136`; `erl_message.c:771,839`, whose reduction charge follows at `erl_message.c:881`
//! on the `do_send` path of `bif.c`) and *not* the `size_object_x(t, NULL)` source-occupancy count
//! (`global.h:1135`) [`beam_term::size_object`] publishes.
//!
//! That destination-pricing model **is delegated**: the DFS, the per-object interior
//! (`beam_term::layout`), the bound and the cycle verdict are `beam_term::size`'s, driven by a
//! [`SizingPolicy`] this crate supplies; the walk, its two pricing axes, its cycle verdict and the
//! per-thread [`SizingScratch`](beam_term::size::SizingScratch) it runs on are [`crate::preflight`]'s.
//! The copier keeps only the *call* to it, so the same figure sizes the destination and — on the
//! fragment path — the message buffer.
//!
//! The scratch is what makes the walk affordable: `beam_term::size` gives the caller the walk's two
//! buffers as a reusable value which every walk **clears** rather than drops, and [`crate::preflight`]
//! keeps one per thread, so a graph priced twice (every refused send, and every retried one) pays for
//! the buffers once. `tests/traversals.rs` measures that: a warm preflight allocates nothing.
//!
//! # The worklist is retained per thread, and a cons chain is copied in one pass
//!
//! The writing traversal drives an explicit worklist of pending fix-ups ([`drain`]), which both copy
//! routes share: the optimistic [`try_copy_fast`] and the sized tail of [`copy_struct`] are the same
//! loop over a different [`CopyTarget`], so it exists once. The `Vec` behind it is a **retained
//! per-thread buffer** ([`WARM_WORKLIST`], mirrored on
//! [`preflight::WARM`](crate::preflight)) that every copy **clears** rather than drops, so a thread
//! that has copied one graph of a given depth copies the next one allocation-free instead of growing
//! `1, 2, 4, … 2ⁿ` entries on every send. Retaining it cannot keep a term alive: a [`Fixup`] records
//! a destination word (inside a heap that never reallocates) and a source word, its lifetime is one
//! copy, and the list is empty whenever the copy returns — it is not a GC root holder.
//! [`shrink_warm_worklist`] is the host's RSS lever after an outlier payload, exactly as
//! [`shrink_warm_scratch`](crate::preflight::shrink_warm_scratch) is for the pricing scratch.
//!
//! Inside the driver a **cons chain** is copied by [`copy_cons_chain`] rather than one worklist
//! round-trip per cell: a list is the dominant object count in a real payload, and the whole `cdr`
//! chain is written by one tight loop that records a fix-up only for a child that is not shared (the
//! same [`push_fixup`] test, in the same order), so the destination layout and word count are
//! unchanged. The chain walk's **cycle bound** is the target's remaining capacity
//! ([`CopyTarget::remaining_words`]): it extends the chain only while another 2-word cell can be
//! served, so a forged `cdr` back-edge cannot spin — the pending cell goes back through the ordinary
//! per-object path, whose `reserve` fails, and the typed `BadArg` is re-derived exactly as before
//! (on the sized path the preflight has already proven acyclicity, so the bound never fires there).
//!
//! # One sizing walk, the cycle guard on it, and one price per refusal
//!
//! A term the engine builds is always acyclic, but a *forged* term (a raw write into the heap) can
//! point back at one of its own ancestors, and a naive walk of such a graph would loop forever (or
//! exhaust memory). [`preflight_words`] therefore runs the shared walk **unbounded**
//! (`limit = usize::MAX`), the one mode in which the walk detects the back-edge itself through its
//! DFS path set and reports
//! [`TermError::TermGraphCyclic`](beam_term::error::TermError::TermGraphCyclic) — which the copier
//! maps to a typed
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg) instead of hanging. That single walk is also
//! **exact**: at `usize::MAX` the running total saturates rather than passing the limit, so the
//! walk's over-limit arm is unreachable and an acyclic graph always comes back with its true
//! destination price. One walk is the whole gate: the caller compares that figure against the
//! destination's free words, and — on the fragment path — reuses the very same figure to size the
//! message buffer, so a refusal is priced **once** and never re-walked (see
//! [`crate::send_fragmented`] and [`crate::send_fragmented_after_refusal`], the latter for a host
//! that already holds the refusal). This is the C's shape, two walks per send
//! (`size_object_litopt` then `copy_struct_litopt`, `erl_message.c:771,797,839,860`).
//!
//! The fast path inherits the same guarantee from the other direction: its trial target hands out
//! only the destination's finite free words, so a forged cycle exhausts them (in at most
//! `available / 2` objects), the attempt is abandoned and the preflight path re-derives the typed
//! `BadArg`.

use std::cell::RefCell;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;
use crate::layout;
use crate::off_heap::OffHeapTable;
use crate::preflight::preflight_words;
/// The **destination-word source** of a copy: the real destination heap, or — for
/// [`copy_struct`]'s optimistic single-pass fast path — a [`TrialTarget`], a bump pointer over
/// the destination heap's own unwritten words.
///
/// Abstracting the two is what lets the fast path reuse [`shallow`] *unchanged* (no copy logic is
/// duplicated): the [`Heap`] implementation is today's `alloc_words`, the trial hands out the very
/// same words but keeps the heap's `used`/`peak` at zero until the copy has succeeded.
pub(crate) trait CopyTarget {
    /// Reserves `n` contiguous words of the destination for one object and returns their base.
    ///
    /// The returned words are **uninitialised**: the caller must write them before they are read,
    /// and must not use them after the owning target is gone. The region never moves, so the
    /// pointer stays valid for the target's lifetime. (Returning uninitialised memory as a raw
    /// pointer is safe; reading it is not, which is why every write below is an `unsafe` block.)
    fn reserve(&mut self, n: usize) -> Result<*mut Eterm>;

    /// Writes the M1 off-heap reference container `[HEADER_BIN_REF, slot, 0]` and returns it.
    fn off_heap_container(&mut self, slot: usize) -> Result<Eterm>;

    /// The words this target can still hand out.
    ///
    /// The chain walk ([`copy_cons_chain`]) reads it as its **cycle bound**: it keeps extending a
    /// `cdr` chain only while the target can still serve another 2-word cell, so a forged
    /// back-edge terminates instead of spinning. It is an upper bound on `reserve`'s own patience,
    /// not a substitute for it — every reservation is still checked by [`CopyTarget::reserve`].
    fn remaining_words(&self) -> usize;
}

impl CopyTarget for Heap {
    fn reserve(&mut self, n: usize) -> Result<*mut Eterm> {
        // SAFETY: the words are written by the copy that reserves them before anything reads
        // them, and the heap never reallocates.
        unsafe { self.alloc_words(n) }
    }

    fn off_heap_container(&mut self, slot: usize) -> Result<Eterm> {
        layout::make_off_heap_container(self, slot)
    }

    fn remaining_words(&self) -> usize {
        self.available()
    }
}

/// The optimistic attempt's destination: a bump pointer over the **unwritten** words of the
/// destination heap (`[hp, hend)`).
///
/// It hands out exactly the words the heap itself would, without touching the heap's `used` or
/// `peak` — so an attempt that fails leaves the destination heap's accounting provably untouched
/// (whatever it already held stays live, and the words the trial wrote lie past `used`, invisible),
/// and one that succeeds is committed by [`try_copy_fast`] with a single `alloc_words(written)`.
struct TrialTarget {
    hp: *mut Eterm,
    free: usize,
    written: usize,
}

impl TrialTarget {
    /// A trial over the words `to` has not handed out yet.
    fn over(to: &Heap) -> TrialTarget {
        TrialTarget {
            hp: to.hp(),
            free: to.available(),
            written: 0,
        }
    }

    /// The number of words the trial handed out (the commit size).
    fn written(&self) -> usize {
        self.written
    }
}

impl CopyTarget for TrialTarget {
    fn reserve(&mut self, n: usize) -> Result<*mut Eterm> {
        if n > self.free {
            // The one failure the trial reports: the destination cannot hold this object. It is
            // never surfaced — `copy_struct` abandons the attempt and re-runs the preflight path,
            // which raises the crate's typed `AllocKind::Message` refusal.
            return Err(crate::error::heap_exhausted(n));
        }
        let p = self.hp;
        // SAFETY: `hp` is the bump pointer of the destination heap's own region with `n <= free`
        // words still ahead of it, so the advanced pointer stays inside (at worst one-past-the-end
        // of) that region; `add` reads and writes no memory.
        self.hp = unsafe { self.hp.add(n) };
        self.free -= n;
        self.written += n;
        Ok(p)
    }

    fn off_heap_container(&mut self, slot: usize) -> Result<Eterm> {
        // The same M1 encoding `layout::make_off_heap_container` writes for a real destination
        // heap; that writer is typed on `&mut Heap`, so the trial carries its own copy of the
        // three writes. `tests/single_pass.rs` pins that the two agree.
        let p = self.reserve(ERL_BIN_REF_SIZE)?;
        // SAFETY: `p` heads `ERL_BIN_REF_SIZE` exclusively-owned words of the trial region.
        unsafe {
            *p = HEADER_BIN_REF;
            *p.add(1) = Eterm::from_raw(slot);
            *p.add(2) = Eterm::from_raw(0);
        }
        Ok(make_boxed(p as *const Eterm))
    }

    fn remaining_words(&self) -> usize {
        self.free
    }
}

/// The copy boundary: the source process's off-heap table (to resolve off-heap references) and
/// the destination's (where the referenced off-heap objects are re-registered).
pub struct CopyContext<'a> {
    src: &'a OffHeapTable,
    dst: &'a mut OffHeapTable,
}

impl<'a> CopyContext<'a> {
    /// A copy boundary from `src`'s off-heap table into `dst`'s.
    pub fn new(src: &'a OffHeapTable, dst: &'a mut OffHeapTable) -> CopyContext<'a> {
        CopyContext { src, dst }
    }

    /// The source process's off-heap table.
    pub fn src_off_heap(&self) -> &OffHeapTable {
        self.src
    }

    /// The destination process's off-heap table.
    pub fn dst_off_heap(&self) -> &OffHeapTable {
        self.dst
    }

    /// The destination off-heap table, mutably — the copier registers referenced off-heap objects
    /// here and `send` drains it into the message's off-heap list.
    pub fn dst_off_heap_mut(&mut self) -> &mut OffHeapTable {
        self.dst
    }
}

/// A pending fix-up: an address of a reference word inside an already-materialised destination
/// object, and the source child term that must be copied into it.
pub(crate) type Fixup = (*mut Eterm, Eterm);

/// Whether `term` is shared as-is rather than copied: an immediate, or a compound (boxed/list)
/// term word carrying the literal-area bit (`TAG_LITERAL_PTR`, tested only on compound words).
///
/// Ground truth: `copy.c`'s `copy_struct_x` shares `is_immediate` terms and, for compound words,
/// `erts_is_literal` terms.
///
/// The compound test must be a **term word** as well as boxed: [`is_boxed`] mirrors the C bit trick
/// `(x & (_TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED)) == 0`, which is *also* true for every word whose
/// primary tag is `HEADER` (see [`is_boxed_term`]). A bare header word is never a shared literal —
/// testing only `is_boxed`/`is_list` would let a header word that happens to have bit 2 set (e.g.
/// `HEADER_SUB_BITS`) be "shared" verbatim instead of reaching [`shallow`]'s
/// [`PrimaryTag::Header`] arm, which is where a non-term is refused. Genuine boxed (`0b10`) and
/// list (`0b01`) words are always term words, so this changes nothing for valid terms.
pub(crate) fn is_shared(term: Eterm) -> bool {
    if is_immediate(term) {
        return true;
    }
    if is_boxed_term(term) || (is_list(term) && is_term_word(term)) {
        return is_literal(term);
    }
    false
}

/// The **source identity** of a non-shared compound term: the from-space address of the object
/// the word points at.
///
/// The sharing-preserving copier ([`crate::copy_struct_shared`]) keys its one-copy-per-identity
/// memo on this, exactly as the C `erts_shcopy_t` markers key sharing on the from-space term
/// address. `None` for anything that is not a list/boxed word (an immediate or a literal, which
/// are shared rather than memoised).
pub(crate) fn identity(term: Eterm) -> Option<usize> {
    if is_list(term) {
        Some(list_val(term) as usize)
    } else if is_boxed(term) {
        Some(boxed_val(term) as usize)
    } else {
        None
    }
}

/// Records a fix-up when the copied child `child` is a compound the copier must materialise its
/// own copy of; shared terms (immediates, literals) were already written verbatim and need no
/// entry.
fn push_fixup(work: &mut Vec<Fixup>, slot: *mut Eterm, child: Eterm) {
    if !is_shared(child) {
        work.push((slot, child));
    }
}

/// Whether `term` is a **non-shared cons cell** — the shape the chain walk
/// ([`copy_cons_chain`]) continues into.
///
/// `NIL` is an immediate, so [`is_list`] already excludes it; [`is_shared`] is reused rather than
/// re-deriving the literal-area test, which keeps the one shared/not-shared decision in one place.
fn is_cons_unshared(term: Eterm) -> bool {
    is_list(term) && !is_shared(term)
}

/// Copies the `cdr` chain starting at the non-shared cons cell `head` in one tight loop.
///
/// A list is the dominant object count in a real message payload (`[1..10]`, a char list, …), and
/// re-entering [`shallow_unshared`] once per cell pays a worklist push/pop, a dispatch and a
/// separate `reserve` for each one. This loop writes the whole chain directly — two words per cell —
/// and records a fix-up only for a child that is not shared ([`push_fixup`], the one shared-term
/// test), in exactly the order the per-cell path records it: each cell's `CAR` fix-up is queued
/// while the chain is walked and popped after it, so the destination layout and word count are
/// unchanged.
///
/// **The cycle bound is the target's remaining capacity.** A forged `cdr` back-edge
/// (`[1 | <self>]`, the shape `tests/guards.rs` forges) would otherwise spin forever, so the loop
/// extends the chain only while the target can still serve another 2-word cell. That is sound on
/// both copy routes:
///
/// * the trial path hands out exactly the destination heap's free words, so the chain stops after
///   `free / 2` cells and the pending cell goes back through the ordinary per-object path, whose
///   `reserve` fails — the attempt is abandoned and the sized path re-derives the typed `BadArg`
///   from the preflight's cycle verdict;
/// * the sized path has already had its graph proven acyclic by that preflight, so a genuine chain
///   always fits and the bound never fires (and if it somehow did, the ordinary path would answer
///   the same typed `AllocKind::Message` refusal rather than truncating the list).
fn copy_cons_chain<A: CopyTarget>(to: &mut A, work: &mut Vec<Fixup>, head: Eterm) -> Result<Eterm> {
    // The caller guarantees a non-shared cons cell, so the first cell is always materialised and
    // the root is well defined.
    let root = to.reserve(2)?;
    let mut dst = root;
    let mut src = head;
    loop {
        // SAFETY: `src` is a live, non-`NIL` cons cell — the caller's guard for the first iteration,
        // and for every later one `is_cons_unshared` re-tested the `cdr` just read.
        let (car, cdr) = unsafe {
            let cell = list_val(src);
            (*cell, *cell.add(1))
        };
        // SAFETY: `dst` heads the two destination words reserved for this cell (the reservation
        // above, or the one at the end of the previous iteration). Neither target ever reallocates,
        // so the pointer is still live.
        unsafe { *dst = car };
        push_fixup(work, dst, car);

        if is_cons_unshared(cdr) && to.remaining_words() >= 2 {
            // The chain continues, so the cell's `CDR` is the **next destination cell** — exactly
            // the word the per-cell path's tail fix-up would have written, `make_list(dst_next)`.
            // (Writing the source `cdr` verbatim here would leave the destination list pointing
            // into the source heap.)
            let next = to.reserve(2)?;
            // SAFETY: `dst` owns two words and `next` is the destination cell just reserved for
            // this cell's tail.
            unsafe { *dst.add(1) = make_list(next as *const Eterm) };
            dst = next;
            src = cdr;
            continue;
        }
        // The chain ends here: the tail is written verbatim and queued for the ordinary per-object
        // path, which copies it when it is a compound (`push_fixup` skips an immediate/literal) and
        // answers its typed refusal when this target cannot serve a cell the bound above refused.
        // SAFETY: `dst` owns two words, so `dst.add(1)` is in bounds.
        unsafe { *dst.add(1) = cdr };
        push_fixup(work, unsafe { dst.add(1) }, cdr);
        return Ok(make_list(root as *const Eterm));
    }
}

/// Copies the refc-binary term `src` (the 8-word `[BinRef(3)][ErlSubBits(5)]` pair whose root is
/// the `ErlSubBits`) onto `to`, **sharing** its off-heap payload.
///
/// Ground truth: `copy.c:812-848`. The source `BinRef.val` is the payload's first data byte; the
/// owning [`beam_alloc::RefcBinary`] handle is resolved from the source off-heap table by that
/// pointer and cloned (refcount++) into the destination table, so the payload is never
/// deep-copied. The new on-heap pair reproduces the source's `ErlSubBits` window verbatim and its
/// `orig` slot is fixed up to the **copied** `BinRef` (the bug the plain leaf treatment of a
/// `SUB_BITS` would leave dangling).
///
/// A source payload that no held handle owns has no pair to share, so the copier falls back to
/// **flattening** the window into a fresh on-heap bitstring — exactly as it does for a `SUB_BITS`
/// match context ([`copy_match_context`], the C's `ERTS_GET_BITSTRING_REF(from_sb) == NULL`
/// branch). That is sound with no change to the flattener: a refc binary's `base_flags` word *is*
/// its payload's data pointer ([`beam_term::refc::write_refc_binary`]), so the window `start..end`
/// reads the same bits and the result's data lives inside the destination, never aliasing the
/// source payload. Nothing is registered in the destination off-heap table, and the cost is the
/// flattened block — which [`refc_binary_words`](crate::preflight::refc_binary_words) sizes, so the
/// preflight and this copy agree.
///
/// The block's shape is validated up front by the checked reader ([`try_sub_bits_parts`]) and the
/// origin must be a `BinRef`: [`layout::classify_sub_bits`](crate::layout::classify_sub_bits) sends
/// this function only a `RefcBinary`, so a caller that reaches it with anything else gets a typed
/// `BadArg` rather than heap-bits words read as a `BinRef`.
fn copy_refc_binary<A: CopyTarget>(to: &mut A, ctx: &mut CopyContext<'_>, src: Eterm) -> Result<Eterm> {
    // SAFETY: `src` is a boxed term whose header word is readable; the checked reader validates the
    // block's shape (window order, non-null base, a recognised `orig`) and refuses a mis-shaped one
    // with a typed error instead of building a tuple from garbage words.
    let (base_flags, start, end, orig) = unsafe { try_sub_bits_parts(src) }?;
    // The classifier proved the origin is *some* recognised one; only a `BinRef` gives this
    // function a `val`/`next` pair to share. A caller that hands over a match context (or any other
    // block) gets a typed error rather than heap-bits words read as a `BinRef`.
    // SAFETY: `orig` is the validated block's word 4; `is_bin_ref` dereferences only a boxed header.
    if !unsafe { is_bin_ref(orig) } {
        return Err(crate::error::bad_term());
    }
    // SAFETY: `orig` is a `BinRef` (the test above read its header word), so its two payload words
    // are live and `val` is the off-heap payload's first data byte.
    let (val, next) = unsafe { bin_ref_parts(orig) };

    let Some(handle) = ctx.src_off_heap().binary_by_data_ptr(val as *const u8) else {
        return copy_match_context(to, src);
    };
    ctx.dst_off_heap_mut().push_binary(handle);

    let dst = to.reserve(ERL_REFC_BITS_SIZE)?;
    // SAFETY: `dst` owns all 8 words of the pair and `src`'s `ErlSubBits` words 1..3 are live; the
    // `orig` slot is rewritten to the copied `BinRef` so the root reaches the destination copy.
    unsafe {
        *dst = HEADER_BIN_REF;
        *dst.add(1) = Eterm::from_raw(val as usize);
        *dst.add(2) = Eterm::from_raw(next as usize);
        let dsb = dst.add(ERL_BIN_REF_SIZE);
        *dsb = HEADER_SUB_BITS;
        *dsb.add(1) = base_flags;
        *dsb.add(2) = start;
        *dsb.add(3) = end;
        *dsb.add(4) = make_boxed(dst);
        Ok(make_boxed(dst.add(ERL_BIN_REF_SIZE)))
    }
}

/// Flattens the `SUB_BITS` **match context** `src` into a fresh on-heap bitstring on `to`.
///
/// Ground truth: `copy.c:849-863`. When an `ErlSubBits` is *not* a refc binary
/// (`ERTS_GET_BITSTRING_REF(from_sb) == NULL`) the C takes the `else` branch and materialises a
/// brand-new `ErlHeapBits` block for the window —
/// `*argp = HEAP_BITSTRING(hbot, erl_sub_bits_get_base(from_sb), from_sb->start, size)` — so the
/// destination carries no `base_flags`/`orig` pointer into the source process's heap.
///
/// The window is `size = end - start` bits taken from the source base starting at bit offset
/// `start`, packed MSB-first (the bit order [`write_heap_bits`] expects). For a genuine match
/// context (its `orig` is an on-heap bitstring) `size` is at most [`ERL_ONHEAP_BITS_LIMIT`],
/// matching the C's `ASSERT(erl_sub_bits_is_match_context(from_sb))`; a refc binary whose payload
/// no source handle owns takes this path too ([`copy_refc_binary`]) and its window may be larger,
/// so the flattened block can exceed the on-heap limit (see the crate `CONTEXT.md` Known Issues).
///
/// The window is read **once** through the checked reader [`try_bitstring_parts`], which accepts
/// both representations (an `ErlSubBits` window or a heap bitstring) — so a mis-shaped block is the
/// typed `BadArg` instead of a flattening pass driven by a garbage base pointer.
fn copy_match_context<A: CopyTarget>(to: &mut A, src: Eterm) -> Result<Eterm> {
    // SAFETY: `src` is a boxed term whose header word is readable; the checked reader validates the
    // block's shape and reports a mis-shaped one as a typed error, so the returned base/offset/size
    // are a validated triple.
    let (base, offset, size) = unsafe { try_bitstring_parts(src) }?;

    // Pack the window bits MSB-first, bit by bit, exactly as the C `HEAP_BITSTRING` does: source
    // bit `offset + i` is bit `(offset + i) & 7` of byte `base[(offset + i) >> 3]`.
    let mut bytes = vec![0u8; bit_count_to_bytes(size)];
    // SAFETY: the validated triple covers bits `offset .. offset + size` of `base` (a `SUB_BITS`
    // has `start <= end` with `size = end - start`; a heap bitstring's header arity agrees with its
    // own bit count), so every byte read is inside the block's own window and `i < size` keeps the
    // write inside `bytes`.
    unsafe {
        for i in 0..size {
            let bit = offset + i;
            if (*base.add(bit >> 3) >> (7 - (bit & 7))) & 1 != 0 {
                bytes[i >> 3] |= 1 << (7 - (i & 7));
            }
        }
    }

    let dst = to.reserve(heap_bits_size_words(size))?;
    // SAFETY: `dst` owns `heap_bits_size_words(size)` words and `bytes` holds `ceil(size / 8)`;
    // `?` maps a `TermError` into the crate error via `From<TermError> for BeamError`.
    Ok(unsafe { write_heap_bits(dst, &bytes, size)? })
}

/// Materialises one destination object for `src` (the shallow half of the copy) and records a
/// fix-up for each of its non-shared children, which the caller's worklist loop resolves.
///
/// Shared terms (immediates, literals, the zero-arity tuple literal) are returned unchanged.
/// An off-heap **MSO** container and a **refc binary** are refcounted into `ctx`'s destination
/// table, never deep-copied — except a refc binary whose payload no source handle owns, which is
/// flattened into an on-heap bitstring ([`copy_refc_binary`]); an external pid/port/reference
/// registers its node there too.
pub(crate) fn shallow<A: CopyTarget>(
    to: &mut A,
    ctx: &mut CopyContext<'_>,
    src: Eterm,
    work: &mut Vec<Fixup>,
) -> Result<Eterm> {
    if is_shared(src) {
        return Ok(src);
    }
    shallow_unshared(to, ctx, src, work)
}
/// [`shallow`] for a source term the caller has **already** proven not shared — the form the
/// worklist loops use.
///
/// A fix-up is only ever recorded for a non-shared child ([`push_fixup`]), so the copy loop's pop
/// side knows the answer the head of [`shallow`] would re-derive. Splitting the two saves one
/// [`is_shared`] per copied object (four tag tests on the hottest path of the copy), at the cost of
/// one extra character in the API: the two entry points must stay behaviourally identical for every
/// term `is_shared` rejects, which holds by construction because [`shallow`] is exactly this
/// function behind that one guard.
pub(crate) fn shallow_unshared<A: CopyTarget>(
    to: &mut A,
    ctx: &mut CopyContext<'_>,
    src: Eterm,
    work: &mut Vec<Fixup>,
) -> Result<Eterm> {

    match primary_tag(src) {
        PrimaryTag::List => copy_cons_chain(to, work, src),
        PrimaryTag::Boxed => {
            // An off-heap MSO reference is refcounted, never deep-copied: clone the source handle
            // (bumping the MSO refcount) into the destination table and write the new slot.
            if layout::is_off_heap_term(src) {
                let src_slot = layout::off_heap_slot(src);
                let handle = ctx
                    .src_off_heap()
                    .clone_slot(src_slot)
                    .ok_or_else(crate::error::bad_term)?;
                let dst_slot = ctx.dst_off_heap_mut().push(handle);
                return to.off_heap_container(dst_slot);
            }

            // A `SUB_BITS` term is dispatched on its **validated** shape (one shape proof, before
            // anything reads its `orig` slot): a refc binary (the `[BinRef][ErlSubBits]` pair)
            // shares its off-heap payload, while a **match context** (an `ErlSubBits` over an
            // on-heap bitstring) is *flattened* into a fresh on-heap bitstring, so the copy never
            // leaves the destination term pointing into the source process's heap — the dangling
            // reference a verbatim 5-word leaf copy (the generic path below) would produce. A
            // mis-shaped block is the typed `BadArg` the classifier reports, never a read of its
            // garbage `base_flags`/`orig` words.
            if let Some(kind) = layout::classify_sub_bits(src)? {
                return match kind {
                    layout::SubBitsKind::RefcBinary => copy_refc_binary(to, ctx, src),
                    layout::SubBitsKind::MatchContext => copy_match_context(to, src),
                };
            }

            let src_obj = boxed_val(src);
            // SAFETY: `src` is a live boxed term, so its header word is readable.
            let header = unsafe { *src_obj };

            // An external pid/port/reference registers its node, refcounted per identity; its
            // words are then copied verbatim below (the node atom is itself shared).
            if layout::is_external_header(header) {
                if let Some((node, creation)) = layout::external_node_creation(header, src) {
                    // `erts_ref_node_entry` (`copy.c:882`): one reference per copied external
                    // thing. The destination table keeps one entry per distinct node and counts
                    // the references; a second copy of the same node adds a reference to it.
                    let entry = crate::node::NodeEntry::new(node, creation);
                    ctx.dst_off_heap_mut().push_node(entry);
                }
            }

            // The zero-arity tuple is the shared global literal, never a fresh heap object.
            if is_arity_value(header) && header_arity(header) == 0 {
                return Ok(tuple0());
            }

            // SAFETY: `src` is a live boxed term, so `src_obj` heads the live, initialized object
            // `header` names, making its per-object word count well-defined.
            let words = unsafe { layout::object_words(header, src_obj) };
            let dst = to.reserve(words)?;
            // SAFETY: `src_obj` addresses `words` live words and `dst` is a distinct,
            // non-overlapping `words`-word region (a separate, never-reallocated heap).
            unsafe { core::ptr::copy_nonoverlapping(src_obj, dst, words) };
            // SAFETY: `src_obj` heads a live object of `header`'s shape whose payload words are
            // live. A `SUB_BITS` never reaches this generic arm — the off-heap, refc-binary and
            // match-context cases all returned above — so the copy-child enumeration here equals
            // the sizing one for every object seen.
            unsafe {
                layout::for_each_child_slot(header, src_obj, |offset| {
                    // SAFETY: `dst` owns `words` words and every visited offset is `< words`, so
                    // `dst.add(offset)` is an in-bounds, live reference word inside the copy.
                    let child = *dst.add(offset);
                    push_fixup(work, dst.add(offset), child);
                });
            }
            Ok(make_boxed(dst as *const Eterm))
        }
        // A bare header word is never a standing term: a malformed graph.
        PrimaryTag::Header => Err(crate::error::bad_term()),
        // Immediates are shared by `is_shared` above; this arm is unreachable for a valid value.
        PrimaryTag::Immed1 => Ok(src),
    }
}

/// Whether a copy into `ctx`'s destination may take the **single-pass fast path**.
///
/// The one precondition is that the destination's off-heap table holds nothing in **any** of its
/// three lists (no MSO handle, no refc binary, no node reference). That — not the heap's `used` —
/// is what makes an abandoned attempt roll-back-able: [`OffHeapTable::rollback`](crate::off_heap::OffHeapTable::rollback)
/// empties a table *wholesale*, so it is sound only on a table the caller knows started empty, which
/// this gate guarantees.
///
/// The destination **heap's** `used` is deliberately *not* part of the gate. [`try_copy_fast`]
/// writes through [`TrialTarget`], which hands out only `[to.hp(), to.hp() + to.available())` — the
/// heap's *unwritten* tail — and leaves `used`/`peak` untouched until the single commit
/// (`to.alloc_words(written)`). A failed attempt therefore committed nothing, so there is nothing to
/// rewind: the leftover trial words lie beyond `used`, invisible to the heap and overwritten by the
/// next allocation. That is exactly the common case — a long-lived receiver heap a previous message
/// left non-empty (see `beam_process::Process::copy_message`, whose destination is the process's
/// young heap) — and it is the shape the single traversal now serves.
fn is_fast_path_eligible(ctx: &CopyContext<'_>) -> bool {
    ctx.dst_off_heap().is_fully_empty()
}

thread_local! {
    /// The copier's **fix-up worklist**, one per thread: cleared (never re-`malloc`ed) by every
    /// copy, so a thread that has copied one graph of a given depth copies the next one
    /// **allocation-free**.
    ///
    /// Retaining the buffer is sound because a [`Fixup`] has a one-copy lifetime: the recorded
    /// address points into the destination heap (or the trial overlay of it), which never
    /// reallocates and is kept alive by the caller's own root, and the source term is a word of a
    /// graph the caller still owns. The list is cleared before every use and is empty when the copy
    /// returns, so the retained buffer **holds no live term** — it is not a GC root and can never
    /// keep a term alive.
    ///
    /// A `RefCell` because the loop needs `&mut` and the borrow must be checked, not assumed: a
    /// nested copy (see [`drain_with_warm_worklist`]) must not alias it.
    static WARM_WORKLIST: RefCell<Vec<Fixup>> = RefCell::new(Vec::new());
}

/// The one **worklist driver** both copy routes share: materialise `term`, then every pending
/// fix-up, until the worklist is empty.
///
/// [`try_copy_fast`] and the sized tail of [`copy_struct`] are the same loop over a different
/// [`CopyTarget`], so it lives here once instead of once per route. A popped item is always a term
/// [`push_fixup`] proved **not** shared, which is why the pop side calls [`shallow_unshared`] (no
/// second `is_shared` per object) and why the root call is [`shallow`] (which does test it).
fn drain<A: CopyTarget>(
    target: &mut A,
    ctx: &mut CopyContext<'_>,
    term: Eterm,
    work: &mut Vec<Fixup>,
) -> Result<Eterm> {
    let root = shallow(target, ctx, term, work)?;
    while let Some((slot, src)) = work.pop() {
        let copied = shallow_unshared(target, ctx, src, work)?;
        // SAFETY: `slot` was recorded by `shallow`/`shallow_unshared` as a reference word inside an
        // object it just materialised on the destination (a heap region, or the trial overlay of
        // one, neither of which ever reallocates), so the word is still live, exclusively owned by
        // this copy and writable.
        unsafe { *slot = copied };
    }
    Ok(root)
}

/// [`drain`] over this thread's retained worklist ([`WARM_WORKLIST`]).
///
/// A copy does not re-enter itself — the worklist is only touched from here, and no step of a copy
/// starts another one — so the borrow succeeds in practice. It is still a `try_borrow_mut` with a
/// **locally built** `Vec` fallback, exactly the structure
/// [`preflight_words`](crate::preflight::preflight_words) uses for its scratch: a re-entrant call,
/// or a call from a thread whose worklist is already destroyed at thread teardown, runs the
/// identical loop on its own buffer instead of panicking. The answer never depends on which buffer
/// ran — only the allocation does.
fn drain_with_warm_worklist<A: CopyTarget>(
    target: &mut A,
    ctx: &mut CopyContext<'_>,
    term: Eterm,
) -> Result<Eterm> {
    let warmed = WARM_WORKLIST.try_with(|cell| match cell.try_borrow_mut() {
        Ok(mut warm) => {
            warm.clear();
            Some(drain(target, ctx, term, &mut warm))
        }
        Err(_) => None,
    });
    match warmed {
        Ok(Some(result)) => result,
        _ => drain(target, ctx, term, &mut Vec::new()),
    }
}

/// Releases this thread's retained copy worklist — the buffer [`drain_with_warm_worklist`] keeps —
/// back to the allocator.
///
/// Retaining it is the point (a warm worklist makes a copy allocation-free), so an outlier payload
/// leaves a high-water-mark `Vec` behind for the life of the thread. This is the same RSS lever
/// [`shrink_warm_scratch`](crate::preflight::shrink_warm_scratch) is for the pricing scratch:
/// **no result changes**, the next copy simply re-grows the buffer. A call that cannot take the
/// borrow (a copy in progress on this thread, or a worklist already destroyed) is a no-op.
pub fn shrink_warm_worklist() {
    let _ = WARM_WORKLIST.try_with(|cell| {
        if let Ok(mut warm) = cell.try_borrow_mut() {
            warm.shrink_to_fit();
        }
    });
}

/// The optimistic **single-pass** copy: [`shallow`] + the worklist loop with no preflight
/// traversal, writing into a [`TrialTarget`] over the destination heap's own unwritten words and
/// committing the heap with one `alloc_words` when the whole graph has been materialised.
///
/// Returns the copied root. A failure leaves the destination heap's `used`/`peak` **exactly where
/// they were** (the trial never advanced them, whether the destination was clean or dirty) and
/// registers exactly the off-heap references the caller must roll back (none on the success path) —
/// the caller re-runs the sized path, which re-derives the crate's typed outcome.
fn try_copy_fast(term: Eterm, to: &mut Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm> {
    let mut trial = TrialTarget::over(to);
    let root = drain_with_warm_worklist(&mut trial, ctx, term)?;

    // Commit: every word the trial handed out has already been written, so the destination's bump
    // pointer now advances over exactly those words — the one allocation the heap sees, in place
    // of one per object. (`reserve` bounded each request by `to.available()`, so this cannot
    // fail; propagating instead of asserting keeps the function panic-free.)
    let written = trial.written();
    // SAFETY: `written <= to.available()`, and the words were written by the copy above.
    let _ = unsafe { to.alloc_words(written) }?;
    Ok(root)
}

/// Copies `term`'s graph into `to`, returning the copied root.
///
/// Iterative (an explicit worklist); immediates and literal-area terms are shared (returned
/// unchanged); cons cells, tuples/records, funs, flatmaps and off-heap containers are copied, an
/// off-heap **MSO** reference or **refc binary** is refcounted into `ctx`'s destination table
/// (never deep-copied), a `SUB_BITS` **match context** is flattened into a fresh on-heap bitstring
/// (so the copy never aliases the source heap), and an external pid/port/reference registers its
/// node there. A refc binary whose payload no source handle owns is flattened the same way.
///
/// This is the **flat** copy: a subterm occurring twice is copied twice. See
/// [`copy_struct_shared`](crate::copy_struct_shared) for the sharing-preserving variant.
///
/// A **fast-path-eligible** destination (see [`is_fast_path_eligible`]) is served by a single
/// traversal ([`try_copy_fast`]); any other destination takes the preflight-then-write path. Either
/// way a copy that does not fit is the typed [`AllocKind::Message`](beam_common::AllocKind)
/// refusal, a malformed/cyclic graph is [`BeamError::BadArg`](beam_common::BeamError::BadArg), and a
/// refusal writes no destination word.
pub fn copy_struct(term: Eterm, to: &mut Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm> {
    // The optimistic single-pass attempt: a destination whose off-heap table is empty has no
    // partial state worth preserving, so its copy needs no preflight traversal — whether or not the
    // heap has already handed words out. `try_copy_fast` writes through a trial bump pointer that
    // leaves the heap's `used`/`peak` untouched, so a failed attempt is fully roll-back-able: the
    // heap needs **no** rewind (nothing was ever handed out of it) and the three off-heap lists are
    // drained (releasing the refcounts the attempt took).
    //
    // A destination whose off-heap table is *non-empty* keeps the sized path below: rolling that one
    // back would drain its pre-existing entries, which are not this copy's to discard.
    if is_fast_path_eligible(ctx) {
        match try_copy_fast(term, to, ctx) {
            Ok(root) => return Ok(root),
            Err(_) => {
                // Abandon the attempt completely, then let the preflight path below re-derive the
                // crate's typed outcome (`AllocKind::Message`, `BadArg`, …) — the optimistic path
                // must never surface a raw `AllocKind::Heap` from its trial target.
                //
                // Only the off-heap table is rolled back. The trial never advanced the destination
                // heap's `used`/`peak`, so the heap is already exactly where it was and a
                // `to.reset()` here would be wrong for a dirty destination: it would discard the
                // words that were live before the copy. The trial's leftover words sit past `used`,
                // invisible to the heap and overwritten by the next allocation.
                ctx.dst_off_heap_mut().rollback();
            }
        }
    }

    // Preflight the destination capacity against the whole graph's flat word count, so the common
    // exhaustion is attributed to the message heap (`AllocKind::Message`) up front rather than
    // surfacing mid-copy as a bare heap failure — and so a graph too large for `to` is refused
    // **before** any word is written (the destination is left untouched).
    //
    // `preflight_words` prices the graph in ONE walk with the copier's own `SizingPolicy` — not
    // `beam_term::size_object`, which sizes the source graph — and that walk is the one mode that
    // also detects a forged cyclic graph (a typed error instead of a hang). It runs over this
    // thread's warm `SizingScratch` (`crate::preflight`), so the walk itself allocates nothing once
    // the thread has priced a graph of comparable size. The comparison below is the gate: the figure
    // is exact, and the refusal carries it — which is the figure a fragmenting caller reuses, so an
    // oversized payload is never priced twice.
    let needed = preflight_words(term, to.available(), ctx)?;
    if to.available() < needed {
        return Err(crate::error::heap_exhausted(needed));
    }

    drain_with_warm_worklist(to, ctx, term)
}

/// The common case of [`copy_struct`] with empty off-heap tables (no off-heap references).
pub fn copy_struct_simple(term: Eterm, to: &mut Heap) -> Result<Eterm> {
    let src = OffHeapTable::new();
    let mut dst = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src, &mut dst);
    copy_struct(term, to, &mut ctx)
}

/// The flat **source** word count of `term`'s graph — `beam_term::size_object`'s answer via a thin
/// forwarder (the `size_object_x` port, i.e. the `litopt == NULL` source-occupancy count); exposed so
/// callers and tests can size a destination heap without re-deriving it.
///
/// This is what a destination needs when the copier's policy walk agrees with it, which holds for a
/// graph with no literal-area subterm and no refc binary whose payload no source handle owns
/// (`tests/sizing.rs`'s corpus). It is **not** the copier's own price for either of those two shapes
/// — a shared literal costs 0 destination words to the copy and a no-handle refc binary costs its
/// flattened window — so the copy engine prices through
/// [`preflight_words`](crate::preflight::preflight_words) instead, never through this function.
/// `tests/destination_pricing.rs` pins the divergence.
pub fn size_of_copy(term: Eterm) -> usize {
    beam_term::size_object(term)
}

#[cfg(test)]
mod tests;
