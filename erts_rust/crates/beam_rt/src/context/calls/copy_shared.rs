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

//! Sharing-aware copy-out for path-copying container BIFs.
//!
//! `beam_bifs`'s map BIFs (`maps:put/3`, `maps:remove/2`, …) build their result with
//! **path-copying** writers (`map_put_cow`/`map_remove_cow`): they allocate only the O(log N)
//! words of the new spine on the pooled BIF heap (the `[mark, used)` window) while **sharing**
//! the untouched subtrees, which already live on the caller's scanned process heap. The full
//! copy-out ([`super::copy_result_fresh`]) walks and re-copies the **whole** result graph, so each
//! Θ(log N) map write pays Θ(N) — Θ(N²) for a fan-out delete over a large map.
//!
//! This module is the Θ(pool delta) alternative: it shallow-copies only the words in the pool
//! window `[mark, used)`, reuses every child already on a scanned region **verbatim** (without
//! descending, so the untouched subtrees are shared, not walked), and declines to the full copier
//! for any word it cannot classify — a pointer-bearing word that is neither pool-local nor scanned.

use std::mem::size_of;

use beam_alloc::Heap;
use beam_process::Process;
use beam_term::Eterm;

use super::super::gc::{dereferenceable_pointer, Geometry};

/// The cheap admission test a word must pass to be a **pool-copy candidate**: a container (a cons
/// cell or a boxed **term** word) that is not a frozen literal. Mirrors
/// [`super::RuntimeContext::walk_may_lead_further`]'s three guards: a literal names nothing on any
/// heap, a `HEADER`-tagged word is not a term at all (yet the `is_boxed`/`is_list` bit tricks can
/// be true for it), and an immediate (atom, pid, port, `NIL`, small int) is neither.
#[inline]
fn may_lead_further(src: Eterm) -> bool {
    !beam_term::literal::is_literal(src)
        && beam_term::tag::is_term_word(src)
        && (beam_term::boxed::is_boxed(src) || beam_term::prelude::is_list(src))
}

/// Shallow-copies only the words `term` holds in the pool window `[mark, used)`, reusing every
/// child already on a scanned region verbatim (never descended into), and returns the re-rooted
/// term.
///
/// Returns `None` to **decline** — a word that is neither pool-local, nor scanned, nor an
/// immediate/literal, above all a reference (whose identity is its address) or a forged cycle
/// whose copied words exceed `demand`. On decline the caller leaves the result on the pool and
/// does not rewind (a leak, never a dangle), falling back to the full copier.
#[allow(unsafe_code)]
pub(super) fn sharing_copy(
    process: &mut Process,
    term: Eterm,
    pool: &Heap,
    mark: usize,
    traced: bool,
) -> Option<Eterm> {
    // A non-container root (an immediate, a literal, or a `HEADER`-tagged word like the C ABI's
    // `THE_NON_VALUE`) has nothing to shallow-copy. Declining here — rather than handing such a
    // word through verbatim — keeps the caller's "a refused copy leaves the pool un-rewound"
    // contract intact: `result_is_foreign_container` admits a bare header (its `is_boxed` bit
    // trick is true), but the full copier refuses it, and that refusal must still observe its old
    // behavior, not license a rewind.
    if !may_lead_further(term) {
        return None;
    }
    let geometry = Geometry::of(process);
    // The pool window in bytes: the words this call allocated, `[mark, used)`, ending at the
    // bump pointer.
    let demand = pool.used().saturating_sub(mark);
    let hi = pool.hp() as usize;
    let lo = hi.wrapping_sub(demand * size_of::<Eterm>());
    // The copied-word budget: a well-formed result holds at most `demand` pool-local words, so a
    // forged cycle (or a mis-sized header) that makes us copy more declines here.
    let mut budget = demand;
    let mut work: Vec<(*mut Eterm, Eterm)> = Vec::new();
    let root = copy_word(process, &geometry, lo, hi, traced, &mut budget, &mut work, term)?;
    while let Some((slot, src)) = work.pop() {
        let fixed = copy_word(process, &geometry, lo, hi, traced, &mut budget, &mut work, src)?;
        // SAFETY: `slot` addresses a child word inside an object this function freshly allocated
        // on the process's bump heap, which only ever advances during this copy (no collection or
        // growth runs here), so the slot stays valid until it is written once, here.
        unsafe { *slot = fixed; }
    }
    Some(root)
}

/// Classifies one word `src` into its final value: the word itself (reuse verbatim) for an
/// immediate/literal, an unreadable pointer, or a scanned container; a fresh shallow copy for a
/// pool-local container; or `None` to decline.
#[allow(unsafe_code)]
#[inline]
fn copy_word(
    process: &mut Process,
    geometry: &Geometry,
    lo: usize,
    hi: usize,
    traced: bool,
    budget: &mut usize,
    work: &mut Vec<(*mut Eterm, Eterm)>,
    src: Eterm,
) -> Option<Eterm> {
    if !may_lead_further(src) {
        return Some(src);
    }
    let Some(addr) = dereferenceable_pointer(src) else {
        // No live term can be at this pointer (null/mis-aligned): keep the word as it is.
        return Some(src);
    };
    if addr >= lo && addr < hi {
        return shallow_copy(process, budget, work, src, addr);
    }
    if geometry.word_is_scanned(process, src, traced) {
        // Already on a region the collector scans: reuse verbatim and do **not** descend. A
        // scanned word predates this call, so it cannot name a word in this call's pool window,
        // and the collector forwards our reference when it relocates the object — identity
        // preserved, nothing left on the pool.
        return Some(src);
    }
    None
}

/// Shallow-copies one pool-local container `src` (at address `addr`) onto the process heap and
/// returns the re-tagged term, queueing its child slots for fix-up. Declines on a reference or on
/// budget exhaustion.
#[allow(unsafe_code)]
fn shallow_copy(
    process: &mut Process,
    budget: &mut usize,
    work: &mut Vec<(*mut Eterm, Eterm)>,
    src: Eterm,
    addr: usize,
) -> Option<Eterm> {
    if beam_term::prelude::is_list(src) {
        *budget = budget.checked_sub(2)?;
        let dst = unsafe { process.heap_mut().alloc_words(2).ok()? };
        // SAFETY: `addr` is the live, aligned cell pointer of `src`; `dst` is two fresh words on
        // the process heap. The copy is word-for-word and both halves are fixed up below.
        unsafe { std::ptr::copy_nonoverlapping(addr as *const Eterm, dst, 2) };
        let (car, cdr) = unsafe { (*dst, *dst.add(1)) };
        work.push((dst, car));
        work.push((unsafe { dst.add(1) }, cdr));
        return Some(beam_term::list::make_list(dst));
    }
    // SAFETY: `src` is a boxed **term** word (`may_lead_further` checked `is_term_word`), so its
    // object pointer is non-null, aligned, and the word at `addr` is its live header.
    let header = unsafe { *(addr as *const Eterm) };
    if beam_term::boxed::is_ref_thing_header(header) || beam_term::boxed::is_external_ref_header(header) {
        // A reference's identity is its address; a copy would stop being the timer/monitor it
        // names. Decline rather than fabricate a different term.
        return None;
    }
    // SAFETY: `header` heads the live object at `addr`; `object_words` reads its shape.
    let words = unsafe { beam_term::layout::object_words(header, addr as *const Eterm) };
    *budget = budget.checked_sub(words)?;
    let dst = unsafe { process.heap_mut().alloc_words(words).ok()? };
    // SAFETY: `words` live words from `addr` to the `words` fresh words at `dst`, word-for-word.
    unsafe { std::ptr::copy_nonoverlapping(addr as *const Eterm, dst, words) };
    // SAFETY: `dst` now heads a live object of `header`'s shape; `for_each_child_slot` visits
    // exactly its term-reference slots, each offset in bounds, so the child words read here are
    // the verbatim-copied originals that must be fixed up.
    unsafe {
        beam_term::layout::for_each_child_slot(header, dst, |offset| {
            let child = *dst.add(offset);
            work.push((dst.add(offset), child));
        });
    }
    Some(beam_term::boxed::make_boxed(dst))
}
