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
//! off-heap payload ([`copy_refc_binary`]).
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
//! # Cycle guard on the preflight
//!
//! A term the engine builds is always acyclic, but a *forged* term (a raw write into the heap)
//! can point back at one of its own ancestors. `beam_term::size_object` — the obvious way to size
//! the destination — walks a graph with an unbounded worklist, so such a cycle would loop forever
//! (or exhaust memory). The copy path therefore never calls `size_object` on an unproven graph:
//! [`preflight_words`] first runs a **bounded** walk of the copy engine's own sizing, and only if
//! that walk shows the graph does not fit does it (a) reject a cyclic graph with a typed
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg) via [`is_acyclic`], or (b) fall back to
//! `size_object` on a graph now known to be acyclic. A cyclic graph is therefore always a typed
//! error, never a hang, while a normal (acyclic) graph is sized exactly as before.

use std::collections::HashSet;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use crate::layout;
use crate::off_heap::OffHeapTable;

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

/// Whether a word contributes a destination object the copier must descend into: a compound term
/// that is **not** shared as-is. Immediates, literal-area terms (shared verbatim) and non-term
/// (bare header) words are all skipped — they write no destination word and expose no child.
#[inline]
fn needs_visit(term: Eterm) -> bool {
    !is_shared(term) && is_term_word(term)
}

/// The number of destination words the copier writes for `term` — the copy engine's **own**
/// sizing, so it is exact and a sound preflight — or `None` as soon as the running total exceeds
/// `limit`.
///
/// It mirrors [`shallow`]/[`copy_refc_binary`]'s allocation exactly: `2` words per cons cell,
/// [`ERL_BIN_REF_SIZE`] for an off-heap MSO container, [`ERL_REFC_BITS_SIZE`] for a refc binary,
/// nothing for the shared zero-arity-tuple literal, and [`layout::object_words`] for every other
/// boxed object (walking its child slots with [`layout::for_each_child_slot`]). A `SUB_BITS`
/// **match context** is one of those "other" objects: [`layout::object_words`] returns the
/// `heap_bits_size_words(size)` block [`copy_match_context`] writes and [`layout::for_each_child_slot`]
/// yields no child, so this walk sizes it exactly as the copy writes it.
///
/// The `limit` is what makes it cycle-safe: a forged cyclic graph revisits its own objects, so its
/// count grows without bound and the walk stops at `limit` instead of looping forever. Every
/// compound object contributes at least 2 words, so a graph that terminates this way always
/// exceeds `limit` — i.e. `Some(n)` is returned only for a graph that is both acyclic and `<=
/// limit` words.
fn bounded_words(term: Eterm, limit: usize) -> Option<usize> {
    let mut total = 0usize;
    let mut work: Vec<Eterm> = Vec::new();
    let mut current = term;
    loop {
        if needs_visit(current) {
            match primary_tag(current) {
                PrimaryTag::List => {
                    total = total.saturating_add(2);
                    if total > limit {
                        return None;
                    }
                    // SAFETY: `current` is a live, non-`NIL` cons cell, so both words are readable.
                    let (head, tail) = unsafe { (car(current), cdr(current)) };
                    if needs_visit(head) {
                        work.push(head);
                    }
                    if needs_visit(tail) {
                        work.push(tail);
                    }
                }
                PrimaryTag::Boxed => {
                    if layout::is_off_heap_term(current) {
                        total = total.saturating_add(ERL_BIN_REF_SIZE);
                    } else if layout::is_refc_binary_term(current) {
                        total = total.saturating_add(ERL_REFC_BITS_SIZE);
                    } else {
                        // Everything else — including a `SUB_BITS` match context, whose
                        // `object_words` is the flattened heap-bits block's size — is sized by
                        // `object_words` and walked by `for_each_child_slot`.
                        let p = boxed_val(current);
                        // SAFETY: `current` is a live boxed term, so `p` addresses its header word.
                        let header = unsafe { *p };
                        // The zero-arity tuple is the shared global literal: no destination word.
                        if !(is_arity_value(header) && header_arity(header) == 0) {
                            total = total.saturating_add(layout::object_words(header, p));
                            if total > limit {
                                return None;
                            }
                            layout::for_each_child_slot(header, p, |offset| {
                                // SAFETY: `offset` is an in-bounds slot of the object at `p`.
                                let child = unsafe { *p.add(offset) };
                                if needs_visit(child) {
                                    work.push(child);
                                }
                            });
                        }
                    }
                    if total > limit {
                        return None;
                    }
                }
                // Unreachable: `needs_visit` already excluded non-terms and immediates. Kept for
                // exhaustiveness — such a word contributes no destination word.
                PrimaryTag::Header | PrimaryTag::Immed1 => {}
            }
        }
        match work.pop() {
            Some(next) => current = next,
            None => return Some(total),
        }
    }
}

/// Whether the term graph rooted at `term` is **acyclic**.
///
/// A depth-first walk (`car`/`cdr` for cons cells, [`layout::for_each_child_slot`] for boxed
/// objects — the same children the copier descends into) that tracks, in `on_path`, exactly the
/// compound objects on the current DFS path. A revisit of a word still on the path is a back-edge,
/// i.e. a cycle. Shared objects (`immediates`/literals), off-heap references and an `ErlSubBits`
/// (a refc binary, or a match context that `copy_struct` flattens) are leaves here, matching the
/// copier: they have no navigable child.
///
/// This is only ever called on the *error* path ([`preflight_words`] falls back to it after the
/// bounded walk has already shown the graph does not fit), so the `HashSet` it needs is never paid
/// on a normal copy.
fn is_acyclic(term: Eterm) -> bool {
    enum Step {
        Enter(Eterm),
        Exit(usize),
    }

    let mut on_path: HashSet<usize> = HashSet::new();
    let mut stack: Vec<Step> = Vec::new();
    stack.push(Step::Enter(term));

    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(addr) => {
                on_path.remove(&addr);
            }
            Step::Enter(t) => {
                if !needs_visit(t) {
                    continue;
                }
                match primary_tag(t) {
                    PrimaryTag::List => {
                        let addr = list_val(t) as usize;
                        if !on_path.insert(addr) {
                            return false;
                        }
                        // SAFETY: `t` is a live, non-`NIL` cons cell.
                        let (head, tail) = unsafe { (car(t), cdr(t)) };
                        stack.push(Step::Exit(addr));
                        if needs_visit(head) {
                            stack.push(Step::Enter(head));
                        }
                        if needs_visit(tail) {
                            stack.push(Step::Enter(tail));
                        }
                    }
                    PrimaryTag::Boxed => {
                        if layout::is_off_heap_term(t) || layout::is_refc_binary_term(t) {
                            continue;
                        }
                        let p = boxed_val(t);
                        // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
                        let header = unsafe { *p };
                        if is_arity_value(header) && header_arity(header) == 0 {
                            continue;
                        }
                        let addr = p as usize;
                        if !on_path.insert(addr) {
                            return false;
                        }
                        stack.push(Step::Exit(addr));
                        layout::for_each_child_slot(header, p, |offset| {
                            // SAFETY: `offset` is an in-bounds slot of the object at `p`.
                            let child = unsafe { *p.add(offset) };
                            if needs_visit(child) {
                                stack.push(Step::Enter(child));
                            }
                        });
                    }
                    // Unreachable (see `bounded_words`); a non-term has no children.
                    PrimaryTag::Header | PrimaryTag::Immed1 => {}
                }
            }
        }
    }
    true
}

/// Sizes the destination for a copy of `term` against a destination holding `limit` free words,
/// returning the flat word count to reserve.
///
/// The cycle-safe replacement for a bare `beam_term::size_object` preflight:
///
/// * a graph whose copy fits `limit` is sized by [`bounded_words`] — an exact, bounded,
///   allocation-free walk that cannot hang;
/// * a graph that does not fit is either **cyclic** (a typed
///   [`BeamError::BadArg`](beam_common::BeamError::BadArg) — never a hang) or simply too big, in
///   which case the canonical `beam_term::size_object` now gives the exact count (it is safe to
///   call because [`is_acyclic`] has just proven the graph terminates).
///
/// Either way the caller compares the result against `limit`; a graph that `bounded_words` accepted
/// is guaranteed `<= limit`.
pub(crate) fn preflight_words(term: Eterm, limit: usize) -> Result<usize> {
    match bounded_words(term, limit) {
        Some(n) => Ok(n),
        None => {
            if !is_acyclic(term) {
                return Err(crate::error::bad_term());
            }
            Ok(beam_term::size_object(term))
        }
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
/// A source payload that no held handle owns is a malformed graph (the M1 encoding has no other
/// way to recover the handle) and yields [`BeamError::BadArg`](beam_common::BeamError::BadArg).
fn copy_refc_binary(to: &mut Heap, ctx: &mut CopyContext<'_>, src: Eterm) -> Result<Eterm> {
    // SAFETY: `src` is a live refc binary, so its `ErlSubBits` words 1..4 are initialised and its
    // `orig` slot is a live `BinRef` term.
    let (base_flags, start, end, orig) = unsafe { sub_bits_parts(src) };
    // SAFETY: `orig` is the `BinRef` of a refc binary, so its two payload words are live.
    let (val, next) = unsafe { bin_ref_parts(orig) };

    let handle = ctx
        .src_off_heap()
        .binary_by_data_ptr(val as *const u8)
        .ok_or_else(crate::error::bad_term)?;
    ctx.dst_off_heap_mut().push_binary(handle);

    // SAFETY: `alloc_words` returns `ERL_REFC_BITS_SIZE` exclusively-owned, writable words.
    let dst = unsafe { to.alloc_words(ERL_REFC_BITS_SIZE)? };
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
/// `start`, packed MSB-first (the bit order [`write_heap_bits`] expects). `size` is at most
/// [`ERL_ONHEAP_BITS_LIMIT`] for a genuine match context (its `orig` is an on-heap bitstring),
/// matching the C's `ASSERT(erl_sub_bits_is_match_context(from_sb))`.
fn copy_match_context(to: &mut Heap, src: Eterm) -> Result<Eterm> {
    // SAFETY: `src` is a live match-context `ErlSubBits`, so its words 1..4 are initialised and
    // the window `start..end` is covered by its base storage.
    let (_base_flags, start, end, _orig) = unsafe { sub_bits_parts(src) };
    let size = end.raw().saturating_sub(start.raw());

    // Pack the window bits MSB-first, bit by bit, exactly as the C `HEAP_BITSTRING` does.
    let mut bytes = vec![0u8; bit_count_to_bytes(size)];
    for i in 0..size {
        // SAFETY: `src` is a live bitstring and `i < size` is within its bit range.
        if unsafe { bit_at(src, i) } {
            bytes[i >> 3] |= 1 << (7 - (i & 7));
        }
    }

    // SAFETY: `alloc_words` returns `heap_bits_size_words(size)` exclusively-owned, writable words.
    let dst = unsafe { to.alloc_words(heap_bits_size_words(size))? };
    // SAFETY: `dst` owns `heap_bits_size_words(size)` words and `bytes` holds `ceil(size / 8)`;
    // `?` maps a `TermError` into the crate error via `From<TermError> for BeamError`.
    Ok(unsafe { write_heap_bits(dst, &bytes, size)? })
}

/// Materialises one destination object for `src` (the shallow half of the copy) and records a
/// fix-up for each of its non-shared children, which the caller's worklist loop resolves.
///
/// Shared terms (immediates, literals, the zero-arity tuple literal) are returned unchanged.
/// An off-heap **MSO** container and a **refc binary** are refcounted into `ctx`'s destination
/// table, never deep-copied; an external pid/port/reference registers its node there too.
pub(crate) fn shallow(
    to: &mut Heap,
    ctx: &mut CopyContext<'_>,
    src: Eterm,
    work: &mut Vec<Fixup>,
) -> Result<Eterm> {
    if is_shared(src) {
        return Ok(src);
    }

    match primary_tag(src) {
        PrimaryTag::List => {
            // SAFETY: `src` is a live, non-`NIL` cons cell, so its two words are readable.
            let (head, tail) = unsafe {
                let cell = list_val(src);
                (*cell, *cell.add(1))
            };
            // SAFETY: `alloc_words` returns two exclusively-owned, writable words.
            let dst = unsafe { to.alloc_words(2)? };
            // SAFETY: `dst` is the base of those two words.
            unsafe {
                *dst = head;
                *dst.add(1) = tail;
            }
            push_fixup(work, dst, head);
            // SAFETY: `dst` owns two words, so `dst.add(1)` is in bounds.
            push_fixup(work, unsafe { dst.add(1) }, tail);
            Ok(make_list(dst as *const Eterm))
        }
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
                return layout::make_off_heap_container(to, dst_slot);
            }

            // A refc binary (the `[BinRef][ErlSubBits]` pair) shares its off-heap payload.
            if layout::is_refc_binary_term(src) {
                return copy_refc_binary(to, ctx, src);
            }

            // A `SUB_BITS` **match context** (an `ErlSubBits` over an on-heap bitstring) is
            // *flattened* into a fresh on-heap bitstring, so the copy never leaves the destination
            // term pointing into the source process's heap — the dangling reference a verbatim
            // 5-word leaf copy (the generic path below) would produce.
            if layout::is_match_context_term(src) {
                return copy_match_context(to, src);
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

            let words = layout::object_words(header, src_obj);
            // SAFETY: `alloc_words` returns `words` exclusively-owned, writable words.
            let dst = unsafe { to.alloc_words(words)? };
            // SAFETY: `src_obj` addresses `words` live words and `dst` is a distinct,
            // non-overlapping `words`-word region (a separate, never-reallocated heap).
            unsafe { core::ptr::copy_nonoverlapping(src_obj, dst, words) };
            layout::for_each_child_slot(header, src_obj, |offset| {
                // SAFETY: `dst` owns `words` words and every visited offset is `< words`.
                let child = unsafe { *dst.add(offset) };
                // SAFETY: as above — the same in-bounds offset into `dst`.
                push_fixup(work, unsafe { dst.add(offset) }, child);
            });
            Ok(make_boxed(dst as *const Eterm))
        }
        // A bare header word is never a standing term: a malformed graph.
        PrimaryTag::Header => Err(crate::error::bad_term()),
        // Immediates are shared by `is_shared` above; this arm is unreachable for a valid value.
        PrimaryTag::Immed1 => Ok(src),
    }
}

/// Copies `term`'s graph into `to`, returning the copied root.
///
/// Iterative (an explicit worklist); immediates and literal-area terms are shared (returned
/// unchanged); cons cells, tuples/records, funs, flatmaps and off-heap containers are copied, an
/// off-heap **MSO** reference or **refc binary** is refcounted into `ctx`'s destination table
/// (never deep-copied), a `SUB_BITS` **match context** is flattened into a fresh on-heap bitstring
/// (so the copy never aliases the source heap), and an external pid/port/reference registers its
/// node there.
///
/// This is the **flat** copy: a subterm occurring twice is copied twice. See
/// [`copy_struct_shared`](crate::copy_struct_shared) for the sharing-preserving variant.
pub fn copy_struct(term: Eterm, to: &mut Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm> {
    // Preflight the destination capacity against the whole graph's flat word count, so the common
    // exhaustion is attributed to the message heap (`AllocKind::Message`) up front rather than
    // surfacing mid-copy as a bare heap failure — and so a graph too large for `to` is refused
    // **before** any word is written (the destination is left untouched).
    //
    // `preflight_words` is the cycle-safe form of `beam_term::size_object`: it bounds its own walk
    // by `to`'s capacity, so a forged cyclic graph is a typed error instead of an unbounded walk.
    let needed = preflight_words(term, to.available())?;
    if to.available() < needed {
        return Err(crate::error::heap_exhausted(needed));
    }

    let mut work: Vec<Fixup> = Vec::new();
    let root = shallow(to, ctx, term, &mut work)?;
    while let Some((slot, src)) = work.pop() {
        let copied = shallow(to, ctx, src, &mut work)?;
        // SAFETY: `slot` was recorded by `shallow` as a reference word inside an object it just
        // materialised on the destination heap. That heap never reallocates, so the word is still
        // live, exclusively owned by this copy and writable.
        unsafe { *slot = copied };
    }
    Ok(root)
}

/// The common case of [`copy_struct`] with empty off-heap tables (no off-heap references).
pub fn copy_struct_simple(term: Eterm, to: &mut Heap) -> Result<Eterm> {
    let src = OffHeapTable::new();
    let mut dst = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src, &mut dst);
    copy_struct(term, to, &mut ctx)
}

/// The flat word count [`copy_struct`] preflights against [`Heap::available`].
///
/// A thin forwarder to `beam_term::size_object` (the `size_object_x` port); exposed so callers
/// and tests can size a destination heap without re-deriving it.
pub fn size_of_copy(term: Eterm) -> usize {
    beam_term::size_object(term)
}

#[cfg(test)]
mod tests {
    use beam_alloc::Heap;
    use beam_term::prelude::*;

    use super::{copy_struct_simple, is_shared};

    #[test]
    fn immediates_and_literals_are_shared() {
        let mut heap = Heap::new(8).expect("test heap");

        assert!(is_shared(make_small(1)));
        assert!(is_shared(NIL));

        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        assert!(!is_shared(tuple), "a heap tuple is copied");
        assert!(is_shared(make_literal(tuple)), "a literal-tagged tuple is shared");

        let list = heap.cons(make_small(1), NIL).expect("the cons fits");
        assert!(!is_shared(list), "a heap cons is copied");
        assert!(is_shared(make_literal(list)), "a literal-tagged cons is shared");
    }

    #[test]
    fn zero_arity_tuple_normalises_to_the_shared_literal() {
        let mut heap = Heap::new(8).expect("test heap");
        // SAFETY: the heap owns this one word.
        let p = unsafe { heap.alloc_words(1).expect("the header fits") };
        // SAFETY: `p` is that word; a zero-arity tuple header is a single word.
        unsafe { *p = make_arityval_zero() };
        let t0 = make_boxed(p as *const Eterm);

        let mut dst = Heap::new(8).expect("destination heap");
        let copy = copy_struct_simple(t0, &mut dst).expect("the copy succeeds");
        assert_eq!(copy.raw(), tuple0().raw());
        assert_eq!(dst.used(), 0, "the shared literal uses no destination word");
    }

    #[test]
    fn over_capacity_simple_copy_is_rejected_before_allocating() {
        let mut src = Heap::new(16).expect("source heap");
        let tuple = src
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("the tuple fits");
        let mut dst = Heap::new(1).expect("too-small heap");
        assert!(copy_struct_simple(tuple, &mut dst).is_err());
        assert_eq!(dst.used(), 0);
    }
}
